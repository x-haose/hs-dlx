//! 任务服务的端到端测试：真实的 SQLite 文件、测试内的本地 HLS 与文件服务（分片取自 tests/fixtures/media/ts_a）。
//! 任务都经粘贴建（[`Handle::paste`]），要选的经 [`Handle::choose`] 选。服务自带 tokio 运行时，在测试自己的运行时
//! 外面建、外面丢弃；等事件与本地服务在测试的运行时里。

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use hs_dlx_app::{
    Answer, Ask, Error, Event, FailureKind, Kind, Offer, OutputKind, OutputSpec, Recognized,
    Service, Stage, Status, Step, Store, Task, TaskId,
};
use hs_m3u8_core::hls::{AudioChoice, Preference};
use hs_m3u8_core::{Saved, Url};
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc};

const PLAYLIST: &str =
    "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n";

/// 本地 HLS 与文件服务：index.m3u8 与 ts_a 的两个分片，两档都指向它的主播放列表 master.m3u8，一个文件 report.pdf
/// （[`FILE`] 字节，不支持范围请求），一个网页 page；`gate` 打开前 seg1.ts 与 gated.m3u8（内容同 index.m3u8）的请求挂着
/// 不回。changing.bin 第一次请求回 [`FILE`] 字节，之后少回一个字节。
struct Server {
    base: Url,
    gate: Arc<Gate>,
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Notify,
}

impl Gate {
    async fn pass(&self) {
        loop {
            let opened = self.opened.notified();
            if *self.open.lock().unwrap() {
                return;
            }
            opened.await;
        }
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_waiters();
    }
}

#[derive(Clone)]
struct Files {
    bodies: Arc<HashMap<String, Vec<u8>>>,
    gate: Arc<Gate>,
    /// changing.bin 被请求了几次
    changing: Arc<AtomicUsize>,
}

async fn serve(State(files): State<Files>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if matches!(path, "seg1.ts" | "gated.m3u8") {
        files.gate.pass().await;
    }
    match path {
        "page" => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<html><body>没有视频</body></html>",
        )
            .into_response(),
        "changing.bin" => {
            let mut body = file_body();
            if files.changing.fetch_add(1, Ordering::SeqCst) > 0 {
                body.pop();
            }
            body.into_response()
        }
        _ => match files.bodies.get(path) {
            Some(body) => body.clone().into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
    }
}

const MASTER: &str = "#EXTM3U\n\
    #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nindex.m3u8?low\n\
    #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\nindex.m3u8\n";

impl Server {
    async fn start(gate_open: bool) -> Server {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media/ts_a");
        let bodies = HashMap::from([
            ("index.m3u8".to_owned(), PLAYLIST.as_bytes().to_vec()),
            ("gated.m3u8".to_owned(), PLAYLIST.as_bytes().to_vec()),
            ("master.m3u8".to_owned(), MASTER.as_bytes().to_vec()),
            (
                "seg0.ts".to_owned(),
                std::fs::read(fixtures.join("seg0.ts")).unwrap(),
            ),
            (
                "seg1.ts".to_owned(),
                std::fs::read(fixtures.join("seg1.ts")).unwrap(),
            ),
            ("report.pdf".to_owned(), file_body()),
        ]);
        let gate = Arc::new(Gate::default());
        if gate_open {
            gate.open();
        }
        let files = Files {
            bodies: Arc::new(bodies),
            gate: gate.clone(),
            changing: Arc::default(),
        };
        let app = axum::Router::new().fallback(serve).with_state(files);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { base, gate }
    }
}

/// report.pdf 的字节数。
const FILE: usize = 300_007;

/// report.pdf 的内容。
fn file_body() -> Vec<u8> {
    (0..FILE as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

/// 每个测试独立的空目录。
fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("app")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// HLS 任务 `task` 的输出。
fn output(task: &Task) -> OutputSpec {
    task.spec.hls().expect("测试只建 HLS 任务").output.clone()
}

/// 粘贴 `text`、存到 `folder`，返回建出的那一个任务（准备中）。
fn paste(rt: &Runtime, service: &Service, text: &str, folder: &Path) -> Task {
    let mut pasted = rt.block_on(service.paste(text, folder, 1)).unwrap();
    match pasted.pop() {
        Some(Ok(task)) if pasted.is_empty() => task,
        other => panic!("应建出一个任务：{other:?}（另有 {} 行）", pasted.len()),
    }
}

/// 等到任务 `id` 定下、开始下载，返回定下后的任务。
async fn settled(events: &mut mpsc::UnboundedReceiver<Event>, id: TaskId) -> Task {
    loop {
        match events.recv().await.expect("服务还在，事件通道不会关闭") {
            Event::Changed { task } if task.id == id && task.spec.pending().is_none() => {
                return *task;
            }
            Event::Ended { id: at, status, .. } if at == id => panic!("没定下就停了：{status:?}"),
            _ => {}
        }
    }
}

/// 等到任务 `id` 停下等用户，返回给出的可选项。
async fn asked(events: &mut mpsc::UnboundedReceiver<Event>, id: TaskId) -> Offer {
    loop {
        match events.recv().await.expect("服务还在，事件通道不会关闭") {
            Event::Stage { id: at, stage } if at == id => {
                if let Some(Stage::Asking(offer)) = stage.map(|stage| *stage) {
                    return offer;
                }
            }
            Event::Ended {
                id: at,
                status: status @ (Status::Failed(_) | Status::Paused),
                ..
            } if at == id => panic!("没等到可选项就停了：{status:?}"),
            _ => {}
        }
    }
}

/// 粘贴 `url`、存到 `folder`，等它定下、开始下载（没有要选的），返回定下后的任务。
fn start(rt: &Runtime, service: &Service, url: &Url, folder: &Path) -> Task {
    let mut events = service.subscribe();
    let id = paste(rt, service, url.as_str(), folder).id;
    rt.block_on(settled(&mut events, id))
}

fn service(store: &Path) -> (Service, mpsc::UnboundedReceiver<Event>) {
    Service::new(
        Store::open(store).unwrap(),
        NonZeroUsize::new(8).unwrap(),
        hs_dlx_app::TorrentNetwork::Loopback {
            public_trackers: Vec::new(),
        },
        hs_dlx_app::pikpak::Endpoints::official(),
        // 网页视频不在这里测：不给组件
        hs_dlx_app::WebConfig {
            root: std::env::temp_dir().join("hs-dlx-no-web"),
            bundle: None,
            sources: hs_dlx_app::ytdlp::Sources::official(),
            update_every: None,
        },
    )
    .unwrap()
}

/// 等到任务 `id` 停下（停下等用户的不算），返回它的状态与没写进存储的原因；中途的进度事件交给 `on_progress`。
async fn stopped(
    events: &mut mpsc::UnboundedReceiver<Event>,
    id: TaskId,
    mut on_progress: impl FnMut(&hs_m3u8_core::Progress),
) -> (Status, Option<String>) {
    loop {
        match events.recv().await.expect("服务还在，事件通道不会关闭") {
            Event::Progress { id: at, progress } if at == id => on_progress(&progress),
            Event::Ended {
                id: at,
                status,
                unsaved,
                ..
            } if at == id && !matches!(status, Status::Waiting(_)) => return (status, unsaved),
            _ => {}
        }
    }
}

/// 等到任务 `id` 停下等用户、状态写进存储，返回它的状态。
async fn stopped_waiting(events: &mut mpsc::UnboundedReceiver<Event>, id: TaskId) -> Status {
    loop {
        if let Event::Ended {
            id: at,
            status,
            unsaved,
            ..
        } = events.recv().await.expect("服务还在，事件通道不会关闭")
            && at == id
        {
            assert!(unsaved.is_none(), "没写进存储：{unsaved:?}");
            return status;
        }
    }
}

/// 等到任务 `id` 停下、状态写进存储，返回它的状态；中途的进度事件交给 `on_progress`。
async fn ended(
    events: &mut mpsc::UnboundedReceiver<Event>,
    id: TaskId,
    on_progress: impl FnMut(&hs_m3u8_core::Progress),
) -> Status {
    match stopped(events, id, on_progress).await {
        (status, None) => status,
        (_, Some(error)) => panic!("没写进存储：{error}"),
    }
}

/// 等到任务 `id` 下完第一个分片。
async fn first_segment(events: &mut mpsc::UnboundedReceiver<Event>, id: TaskId) {
    loop {
        if let Some(Event::Progress { id: at, progress }) = events.recv().await
            && at == id
            && progress.segments.done == 1
        {
            return;
        }
    }
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// 新建即开始，下完写出 MP4（标题与主名取自地址）、删掉任务目录，存储里记为已完成；从列表移除只删记录，输出留着。
#[test]
fn a_task_downloads_and_is_recorded_as_done() {
    let dir = test_dir("done");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));

    let task = start(
        &rt,
        &service,
        &server.base.join("index.m3u8").unwrap(),
        &folder,
    );
    assert_eq!(task.spec.title, "index");
    assert_eq!(output(&task), OutputSpec::Mp4(folder.join("index.mp4")));
    let status = rt.block_on(ended(&mut events, task.id, |_| {}));

    assert_eq!(status, Status::Done);
    assert!(folder.join("index.mp4").is_file());
    assert!(!folder.join("index.hsdl").exists());
    let done = service.tasks().unwrap().remove(0);
    assert_eq!(done.status, Status::Done);
    let result = done.result.expect("完成时记下写出的结果");
    assert!(result.bytes > 0 && result.duration.is_some(), "{result:?}");
    assert_eq!(done.total, Some(2));
    assert!(done.finished_at.is_some() && !done.active.is_zero());
    assert!(matches!(
        service.resume(task.id),
        Err(Error::AlreadyDone(_))
    ));
    assert!(matches!(
        service.set_headers(task.id, Vec::new()),
        Err(Error::AlreadyDone(_))
    ));
    rt.block_on(service.forget(task.id)).unwrap().unwrap();
    assert!(service.tasks().unwrap().is_empty());
    assert!(folder.join("index.mp4").is_file());
}

/// 第一个分片下完后暂停：停下为暂停、任务目录保留、总片数记下，查看任务目录得到已下的 1 片；关掉应用重开后改请求头、
/// 继续，只补下没完成的分片。
#[test]
fn a_paused_task_resumes_after_reopening() {
    let dir = test_dir("pause");
    let folder = dir.join("out");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let id = {
        let (service, mut events) = service(&store);
        let id = start(
            &rt,
            &service,
            &server.base.join("index.m3u8").unwrap(),
            &folder,
        )
        .id;
        let mut paused = false;
        let status = rt.block_on(ended(&mut events, id, |progress| {
            if progress.segments.done == 1 && !paused {
                service.pause(id);
                paused = true;
            }
        }));
        assert_eq!(status, Status::Paused);
        assert!(folder.join("index.hsdl").is_dir());
        id
    };

    server.gate.open();
    let (service, mut events) = service(&store);
    let task = service.tasks().unwrap().remove(0);
    assert_eq!((task.status, task.total), (Status::Paused, Some(2)));
    // 没完成的任务不能只从列表移除，任务目录要一起放弃
    assert!(matches!(
        rt.block_on(service.forget(id)).unwrap(),
        Err(Error::NotFinished(_))
    ));
    let Saved::Hls(saved) = rt.block_on(service.inspect(id)).unwrap().unwrap().unwrap() else {
        panic!("HLS 任务的任务目录");
    };
    assert_eq!((saved.segments, saved.live), (1, false));
    // 停着时可以改请求头，不合要求的拒收
    let bad = vec![("Bad Name".to_owned(), "x".to_owned())];
    assert!(matches!(
        service.set_headers(id, bad),
        Err(Error::InvalidInput(_))
    ));
    let referer = vec![("Referer".to_owned(), "https://example.com/".to_owned())];
    service.set_headers(id, referer.clone()).unwrap();
    assert_eq!(service.tasks().unwrap()[0].spec.headers, referer);
    service.resume(id).unwrap();
    assert!(matches!(
        service.set_headers(id, Vec::new()),
        Err(Error::Running(_))
    ));
    let status = rt.block_on(ended(&mut events, id, |_| {}));

    assert_eq!(status, Status::Done);
    assert!(folder.join("index.mp4").is_file());
}

/// 同一地址新建两次：第二个任务的主名避开第一个任务占用的（第一个还没在磁盘上留下输出）。
#[test]
fn the_same_link_twice_gets_separate_outputs() {
    let dir = test_dir("twice");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, _events) = service(&dir.join("tasks.sqlite"));
    let url = server.base.join("index.m3u8").unwrap();

    let first = start(&rt, &service, &url, &folder);
    let second = start(&rt, &service, &url, &folder);

    assert_eq!(output(&first), OutputSpec::Mp4(folder.join("index.mp4")));
    assert_eq!(output(&second), OutputSpec::Mp4(folder.join("index 2.mp4")));
    assert_eq!(second.spec.title, "index 2");
    for id in [first.id, second.id] {
        rt.block_on(service.discard(id)).unwrap().unwrap();
    }
}

/// 运行中放弃：先停下，再删掉任务目录与存储里的记录；放弃做完之前不能继续、再放弃或移除。
#[test]
fn discarding_a_running_task_removes_it() {
    let dir = test_dir("discard");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let id = start(
        &rt,
        &service,
        &server.base.join("index.m3u8").unwrap(),
        &folder,
    )
    .id;
    rt.block_on(first_segment(&mut events, id));

    let discarding = service.discard(id);
    // 放弃很快：机器忙时下面这几步之前它可能已经做完、任务已不在（不在也是拒绝）
    let refused = |result: Result<(), Error>| {
        matches!(
            result,
            Err(Error::Discarding(_) | Error::Running(_) | Error::NotFound(_))
        )
    };
    assert!(refused(service.resume(id).map(|_| ())));
    assert!(refused(rt.block_on(service.forget(id)).unwrap()));
    assert!(matches!(
        rt.block_on(service.discard(id)).unwrap(),
        Err(Error::Discarding(_) | Error::NotFound(_))
    ));
    let leftovers = rt.block_on(discarding).unwrap().unwrap();

    assert_eq!(leftovers, []);
    assert!(!folder.join("index.hsdl").exists());
    assert!(service.tasks().unwrap().is_empty());
}

/// 保存目录不是绝对路径的拒收，什么也不存；不是链接的一行不收，别的行照建。建了任务的事件在它的其他事件之前。
#[test]
fn lines_that_cannot_become_tasks_are_refused_one_by_one() {
    let dir = test_dir("relative");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let (service, _events) = service(&dir.join("tasks.sqlite"));
    let url = server.base.join("index.m3u8").unwrap();
    let refused = rt
        .block_on(service.paste(url.as_str(), Path::new("out"), 1))
        .unwrap();
    assert!(
        matches!(refused.as_slice(), [Err(rejected)] if matches!(*rejected.reason, Error::InvalidInput(_))),
        "{refused:?}"
    );
    assert!(service.tasks().unwrap().is_empty());

    let mut events = service.subscribe();
    let pasted = rt
        .block_on(service.paste(&format!("这不是链接\n{url}\n"), &dir.join("out"), 1))
        .unwrap();
    let [Err(rejected), Ok(task)] = pasted.as_slice() else {
        panic!("{pasted:?}");
    };
    assert_eq!(rejected.text, "这不是链接");
    assert_eq!(
        (task.status.clone(), task.spec.title.as_str()),
        (Status::Preparing, "index")
    );
    assert_eq!(
        task.spec.pending().map(|pending| pending.folder.clone()),
        Some(dir.join("out"))
    );
    let first = rt.block_on(events.recv()).unwrap();
    assert!(
        matches!(&first, Event::Added { task: added } if added.id == task.id),
        "{first:?}"
    );
    rt.block_on(settled(&mut events, task.id));
    rt.block_on(service.discard(task.id)).unwrap().unwrap();
}

/// 任务停下时存储被别的连接占着：结束状态没写进去，事件里带着原因，服务记着它、列任务时按它返回；存储放开后关闭服务
/// 时补写进去，重开后是已完成。
#[test]
fn an_ending_that_cannot_be_saved_is_saved_later() {
    let dir = test_dir("unsaved");
    let folder = dir.join("out");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let id = {
        let (service, mut events) = service(&store);
        let id = start(
            &rt,
            &service,
            &server.base.join("index.m3u8").unwrap(),
            &folder,
        )
        .id;
        rt.block_on(first_segment(&mut events, id));
        let mut other = rusqlite::Connection::open(&store).unwrap();
        let holding = other
            .transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)
            .unwrap();
        server.gate.open();
        let (status, unsaved) = rt.block_on(stopped(&mut events, id, |_| {}));
        assert_eq!(status, Status::Done);
        assert!(unsaved.is_some());
        drop(holding);
        let task = service.tasks().unwrap().remove(0);
        assert_eq!(task.status, Status::Done);
        assert!(task.result.is_some());
        id
    };
    let (service, _events) = service(&store);
    let task = service.tasks().unwrap().remove(0);
    assert_eq!((task.id, task.status), (id, Status::Done));
    assert!(task.result.is_some());
}

/// 关闭服务：运行中的任务取消、停下、按暂停写进存储；之后不再开始任务。
#[test]
fn closing_pauses_running_tasks() {
    let dir = test_dir("close");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let url = server.base.join("index.m3u8").unwrap();
    let id = start(&rt, &service, &url, &folder).id;
    rt.block_on(first_segment(&mut events, id));

    service.close().unwrap();

    let task = service.tasks().unwrap().remove(0);
    assert_eq!(task.status, Status::Paused);
    assert!(folder.join("index.hsdl").is_dir());
    assert!(matches!(service.resume(id), Err(Error::Closed)));
    let refused = rt
        .block_on(service.paste(url.as_str(), &folder, 1))
        .unwrap();
    assert!(
        matches!(refused.as_slice(), [Err(rejected)] if matches!(*rejected.reason, Error::Closed)),
        "{refused:?}"
    );
}

/// 定下时提交被别的连接挡住（它持着读事务）：任务以失败停下、照旧没定下，已开始的下载取消、它建出的任务目录删掉；
/// 存储放开后继续，从头准备、下完。
#[test]
fn a_settling_that_cannot_commit_leaves_the_task_unsettled() {
    let dir = test_dir("settle-uncommitted");
    let folder = dir.join("out");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, mut events) = service(&store);
    // 探测挂在服务器上的时候占住存储
    let id = paste(
        &rt,
        &service,
        server.base.join("gated.m3u8").unwrap().as_str(),
        &folder,
    )
    .id;
    let mut other = rusqlite::Connection::open(&store).unwrap();
    let reading = other.transaction().unwrap();
    let count: i64 = reading
        .query_row("SELECT count(*) FROM task", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    server.gate.open();

    let (status, unsaved) = rt.block_on(stopped(&mut events, id, |_| {}));
    assert!(
        matches!(&status, Status::Failed(failure) if failure.kind == FailureKind::Other),
        "{status:?}"
    );
    // 失败的状态这时也写不进，记着待补写
    assert!(unsaved.is_some());
    drop(reading);
    let task = service.task(id).unwrap();
    assert_eq!(task.status, status);
    assert!(task.spec.pending().is_some() && task.work_dir.is_none());

    service.resume(id).unwrap();
    assert_eq!(rt.block_on(ended(&mut events, id, |_| {})), Status::Done);
    let done = service.task(id).unwrap();
    assert_eq!(output(&done), OutputSpec::Mp4(folder.join("gated.mp4")));
    // 没定下的那一次没留下任务目录
    let left: Vec<_> = std::fs::read_dir(&folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, ["gated.mp4"]);
}

/// 任务目录里有不是本库写的文件时放弃删不干净：任务留在列表里、记为放弃没做完（不能继续）、报出留下的东西；拿走那个
/// 文件后再放弃即删掉。
#[test]
fn a_discard_that_leaves_the_work_dir_keeps_the_task() {
    let dir = test_dir("discard-left");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let id = start(
        &rt,
        &service,
        &server.base.join("index.m3u8").unwrap(),
        &folder,
    )
    .id;
    rt.block_on(first_segment(&mut events, id));
    service.pause(id);
    assert_eq!(rt.block_on(ended(&mut events, id, |_| {})), Status::Paused);
    let foreign = folder.join("index.hsdl").join("notes.txt");
    std::fs::write(&foreign, b"mine").unwrap();

    let left = rt.block_on(service.discard(id)).unwrap();
    assert!(matches!(left, Err(Error::WorkDirLeft { .. })), "{left:?}");
    // 放弃没做完：任务留着、记为放弃没做完，不能继续
    assert_eq!(
        service.tasks().unwrap()[0].status,
        Status::DiscardUnfinished
    );
    assert!(matches!(
        service.resume(id),
        Err(Error::DiscardUnfinished(_))
    ));

    std::fs::remove_file(&foreign).unwrap();
    assert_eq!(rt.block_on(service.discard(id)).unwrap().unwrap(), []);
    assert!(service.tasks().unwrap().is_empty());
    assert!(!folder.join("index.hsdl").exists());
}

/// 同一地址同时粘贴两次（取名都在定下之前）：定下时后一个撞上前一个占用的路径，重新取名，两个各用各的输出。保存目录的
/// 写法不规整（带 `.`）也照样认出已有任务占用的路径。
#[test]
fn concurrent_pastes_get_separate_outputs() {
    let dir = test_dir("concurrent");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, _events) = service(&dir.join("tasks.sqlite"));
    let url = server.base.join("index.m3u8").unwrap();

    let mut events = service.subscribe();
    let first = service.paste(url.as_str(), &folder, 1);
    let second = service.paste(url.as_str(), &folder.join("."), 1);
    let (first, second) = rt.block_on(async {
        let (mut first, mut second) = (first.await.unwrap(), second.await.unwrap());
        let ids = [first.remove(0).unwrap().id, second.remove(0).unwrap().id];
        // 两个任务定下的先后不定：各等到自己的那一次
        let mut settled: Vec<Task> = Vec::new();
        while settled.len() < 2 {
            if let Event::Changed { task } = events.recv().await.unwrap()
                && ids.contains(&task.id)
                && task.spec.pending().is_none()
                && settled.iter().all(|seen| seen.id != task.id)
            {
                settled.push(*task);
            }
        }
        (settled.remove(0), settled.remove(0))
    });
    let mut outputs = vec![output(&first), output(&second)];
    outputs.sort_by_key(|output| format!("{output:?}"));
    assert_eq!(
        outputs,
        [
            OutputSpec::Mp4(folder.join("index 2.mp4")),
            OutputSpec::Mp4(folder.join("index.mp4")),
        ]
    );
    for id in [first.id, second.id] {
        rt.block_on(service.discard(id)).unwrap().unwrap();
    }
}

/// 只写出已录的用于直播录制：点播下载的任务目录照样开始一次运行，核心报目录里不是直播录制，任务以「没有可写出的」
/// 失败停下、任务目录留着；这个选择存进存储，续传时改回。
#[test]
fn writing_recorded_content_of_a_download_fails_visibly() {
    let dir = test_dir("write-recorded");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let id = start(
        &rt,
        &service,
        &server.base.join("index.m3u8").unwrap(),
        &folder,
    )
    .id;
    rt.block_on(first_segment(&mut events, id));
    service.pause(id);
    assert_eq!(rt.block_on(ended(&mut events, id, |_| {})), Status::Paused);

    service.write_recorded(id).unwrap();
    let status = rt.block_on(ended(&mut events, id, |_| {}));

    assert!(
        matches!(&status, Status::Failed(failure) if failure.kind == FailureKind::NothingToWrite),
        "{status:?}"
    );
    let task = service.task(id).unwrap();
    assert_eq!(task.status, status);
    assert!(task.write_only);
    assert!(folder.join("index.hsdl").is_dir());

    server.gate.open();
    service.resume(id).unwrap();
    assert_eq!(rt.block_on(ended(&mut events, id, |_| {})), Status::Done);
    assert!(!service.task(id).unwrap().write_only);
}

/// 下到一半改存本地 HLS：任务目录不变，继续后写出同名的本地 HLS 目录，不再写 MP4。
#[test]
fn a_paused_task_can_switch_to_local_hls() {
    let dir = test_dir("switch-hls");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let id = start(
        &rt,
        &service,
        &server.base.join("index.m3u8").unwrap(),
        &folder,
    )
    .id;
    rt.block_on(first_segment(&mut events, id));
    service.pause(id);
    assert_eq!(rt.block_on(ended(&mut events, id, |_| {})), Status::Paused);

    let switched = service.switch_to_hls(id).unwrap();
    assert_eq!(output(&switched), OutputSpec::Hls(folder.join("index")));
    assert_eq!(service.task(id).unwrap(), switched);
    server.gate.open();
    service.resume(id).unwrap();

    assert_eq!(rt.block_on(ended(&mut events, id, |_| {})), Status::Done);
    assert!(folder.join("index").join("index.m3u8").is_file());
    assert!(!folder.join("index.mp4").exists());
}

/// 只回 403 的 HTTP 代理：记下经它的请求行。
async fn refusing_proxy() -> (Url, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let log = log.clone();
            tokio::spawn(async move {
                let mut stream = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                log.lock().unwrap().push(line.trim_end().to_owned());
                // 读完请求头再回
                loop {
                    let mut header = String::new();
                    if stream.read_line(&mut header).await.unwrap() <= 2 {
                        break;
                    }
                }
                stream
                    .get_mut()
                    .write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
            });
        }
    });
    (url, seen)
}

/// 设置里的代理用于探测与之后开始的任务，重开后还在；用不了的代理地址存不进。
#[test]
fn the_proxy_setting_applies_to_probes_and_new_tasks() {
    let dir = test_dir("proxy");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let (proxy, seen) = rt.block_on(refusing_proxy());
    let playlist = server.base.join("index.m3u8").unwrap();
    let (service, mut events) = service(&dir.join("tasks.sqlite"));

    assert!(matches!(
        service.set_proxy(&hs_m3u8_core::Proxy::Url(
            Url::parse("ftp://127.0.0.1:21").unwrap()
        )),
        Err(Error::InvalidInput(_))
    ));
    service
        .set_proxy(&hs_m3u8_core::Proxy::Url(proxy.clone()))
        .unwrap();
    // 探测经代理：代理拒绝
    let probed = rt.block_on(
        service
            .probe(hs_m3u8_core::Source::new(playlist.clone()))
            .unwrap(),
    );
    assert!(probed.unwrap().is_err());
    assert_eq!(seen.lock().unwrap().len(), 1, "{:?}", seen.lock().unwrap());
    assert!(seen.lock().unwrap()[0].contains(playlist.as_str()));
    // 粘贴的任务认地址也经代理：被拒，以失败停下，还没定下
    let task = paste(&rt, &service, playlist.as_str(), &dir.join("out"));
    let status = rt.block_on(ended(&mut events, task.id, |_| {}));
    assert!(
        matches!(&status, Status::Failed(f) if f.kind == FailureKind::Status(403)),
        "{status:?}"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(service.task(task.id).unwrap().spec.pending().is_some());
    drop(service);

    // 重开后照旧；改为直连后不再经代理
    let (service, _events) = self::service(&dir.join("tasks.sqlite"));
    assert!(service.settings().unwrap().proxy == hs_m3u8_core::Proxy::Url(proxy));
    service.set_proxy(&hs_m3u8_core::Proxy::Direct).unwrap();
    let probed = rt.block_on(service.probe(hs_m3u8_core::Source::new(playlist)).unwrap());
    assert!(probed.unwrap().is_ok());
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// 文件任务：粘贴后直接开始，文件名由地址得出，下完装到保存目录、删掉任务目录，记下字节数；同名的第二个加编号。
#[test]
fn a_file_task_downloads_into_the_folder() {
    let dir = test_dir("file");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let url = server.base.join("report.pdf").unwrap();

    let task = start(&rt, &service, &url, &folder);
    assert_eq!(task.spec.title, "report.pdf");
    let Kind::File(file) = &task.spec.kind else {
        panic!("定下的是文件任务");
    };
    assert_eq!(file.path, folder.join("report.pdf"));
    assert_eq!(file.expected.length, Some(FILE as u64));
    let mut progressed = false;
    let status = rt.block_on(async {
        loop {
            match events.recv().await.expect("服务还在，事件通道不会关闭") {
                Event::FileProgress { id, .. } if id == task.id => progressed = true,
                Event::Ended {
                    id,
                    status,
                    unsaved,
                    ..
                } if id == task.id => {
                    assert!(unsaved.is_none());
                    return status;
                }
                _ => {}
            }
        }
    });
    assert_eq!(status, Status::Done);
    assert!(progressed, "下载中发了进度");
    assert_eq!(
        std::fs::read(folder.join("report.pdf")).unwrap(),
        file_body()
    );
    assert!(!folder.join("report.pdf.hsdl").exists());
    let done = service.tasks().unwrap().remove(0);
    assert_eq!(done.result.map(|r| r.bytes), Some(FILE as u64));

    let second = start(&rt, &service, &url, &folder);
    assert_eq!(second.spec.title, "report 2.pdf");
    assert_eq!(
        rt.block_on(ended(&mut events, second.id, |_| {})),
        Status::Done
    );
}

/// 第一次下载时地址回的与认地址时看到的不是同一个文件（大小不同）：以「不是之前的文件」失败；放弃后不留东西。
#[test]
fn a_file_that_changed_since_it_was_seen_fails_visibly() {
    let dir = test_dir("file-changed");
    let folder = dir.join("out");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));

    let task = start(
        &rt,
        &service,
        &server.base.join("changing.bin").unwrap(),
        &folder,
    );
    let status = rt.block_on(ended(&mut events, task.id, |_| {}));
    assert!(
        matches!(&status, Status::Failed(f) if f.kind == FailureKind::NotExpected),
        "{status:?}"
    );
    assert!(!folder.join("changing.bin").exists());
    // 停着的文件任务不能只写出已录的，也不能改存本地 HLS
    assert!(matches!(
        service.write_recorded(task.id),
        Err(Error::NotHls(_))
    ));
    assert!(matches!(
        service.switch_to_hls(task.id),
        Err(Error::NotHls(_))
    ));
    let leftovers = rt.block_on(service.discard(task.id)).unwrap().unwrap();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(!folder.join("changing.bin.hsdl").exists());
}

/// 主播放列表有两档：停下等选画质，可选项在内存里、状态在存储里；对不上的选择不收。重开应用后还在等、可选项不在，
/// 继续即重新准备、再停在同一处；选了一档即定下、下完。
#[test]
fn a_master_playlist_waits_for_a_quality_and_asks_again_after_reopening() {
    let dir = test_dir("quality");
    let folder = dir.join("out");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let url = server.base.join("master.m3u8").unwrap();
    let id = {
        let (service, mut events) = service(&store);
        let id = paste(&rt, &service, url.as_str(), &folder).id;
        let offer = rt.block_on(asked(&mut events, id));
        let Offer::Quality(probe) = &offer else {
            panic!("两档的 HLS 等选画质：{offer:?}");
        };
        assert_eq!(probe.master.as_ref().unwrap().variants.len(), 2);
        // 停下的状态随后写进存储
        assert_eq!(
            rt.block_on(stopped_waiting(&mut events, id)),
            Status::Waiting(Ask::Quality)
        );
        let task = service.task(id).unwrap();
        assert_eq!(task.status, Status::Waiting(Ask::Quality));
        assert_eq!(service.stage(id).unwrap(), Some(Stage::Asking(offer)));
        // 停下等用户不是在运行：暂停不起作用，也不能只从列表移除
        service.pause(id);
        assert_eq!(service.task(id).unwrap().status, task.status);
        assert!(matches!(
            rt.block_on(service.forget(id)).unwrap(),
            Err(Error::NotFinished(_))
        ));
        for wrong in [
            Answer::Install,
            Answer::File {
                folder: folder.clone(),
            },
            Answer::Hls {
                preference: Preference::default(),
                output: OutputKind::Mp4,
                folder: PathBuf::from("out"),
            },
        ] {
            assert!(
                matches!(
                    service.choose(id, wrong.clone()),
                    Err(Error::InvalidInput(_))
                ),
                "{wrong:?}"
            );
        }
        id
    };

    let (service, mut events) = service(&store);
    assert_eq!(
        service.task(id).unwrap().status,
        Status::Waiting(Ask::Quality)
    );
    assert_eq!(service.stage(id).unwrap(), None);
    let stale = Answer::Hls {
        preference: Preference::default(),
        output: OutputKind::Mp4,
        folder: folder.clone(),
    };
    assert!(matches!(
        service.choose(id, stale),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(service.resume(id).unwrap().status, Status::Preparing);
    let Offer::Quality(probe) = rt.block_on(asked(&mut events, id)) else {
        panic!("重新准备后还是等选画质");
    };
    assert_eq!(
        rt.block_on(stopped_waiting(&mut events, id)),
        Status::Waiting(Ask::Quality)
    );
    let low = probe
        .master
        .as_ref()
        .unwrap()
        .variants
        .iter()
        .min_by_key(|variant| variant.bandwidth)
        .unwrap();
    let chosen = service
        .choose(
            id,
            Answer::Hls {
                preference: Preference {
                    variant: low.choice(),
                    audio: AudioChoice::Default,
                },
                output: OutputKind::Hls,
                folder: folder.clone(),
            },
        )
        .unwrap();
    assert_eq!(chosen.status, Status::Preparing);
    assert_eq!(rt.block_on(ended(&mut events, id, |_| {})), Status::Done);
    let done = service.task(id).unwrap();
    assert_eq!(output(&done), OutputSpec::Hls(folder.join("master")));
    assert!(folder.join("master").join("index.m3u8").is_file());
    assert_eq!(service.stage(id).unwrap(), None);
}

/// 准备中暂停：停掉在做的那一步，状态为暂停、还没定下；继续即从头准备、下完。准备中关掉应用：重开后是暂停。准备中
/// 放弃：直接删记录，磁盘上什么也没有。
#[test]
fn a_preparing_task_pauses_resumes_and_discards() {
    let dir = test_dir("preparing");
    let folder = dir.join("out");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let server = rt.block_on(Server::start(false));
    let url = server.base.join("gated.m3u8").unwrap();
    let (kept, interrupted) = {
        let (service, mut events) = service(&store);
        let task = paste(&rt, &service, url.as_str(), &folder);
        assert_eq!(
            service.stage(task.id).unwrap(),
            Some(Stage::Working(Step::Identifying))
        );
        // 准备中算在运行：不能再继续、改请求头
        assert!(matches!(service.resume(task.id), Err(Error::Running(_))));
        assert!(matches!(
            service.set_headers(task.id, Vec::new()),
            Err(Error::Running(_))
        ));
        service.pause(task.id);
        assert_eq!(
            rt.block_on(ended(&mut events, task.id, |_| {})),
            Status::Paused
        );
        let paused = service.task(task.id).unwrap();
        assert!(paused.spec.pending().is_some() && paused.active.is_zero());
        assert_eq!(service.stage(task.id).unwrap(), None);
        // 停着时可以改请求头
        let referer = vec![("Referer".to_owned(), "https://example.com/".to_owned())];
        service.set_headers(task.id, referer.clone()).unwrap();
        assert_eq!(service.task(task.id).unwrap().spec.headers, referer);
        // 另一个任务准备中时关掉应用
        let interrupted = paste(&rt, &service, url.as_str(), &folder).id;
        (task.id, interrupted)
    };

    let (service, mut events) = service(&store);
    assert_eq!(service.task(interrupted).unwrap().status, Status::Paused);
    service.resume(interrupted).unwrap();
    let mut watching = service.subscribe();
    assert_eq!(
        rt.block_on(service.discard(interrupted)).unwrap().unwrap(),
        []
    );
    assert!(matches!(service.task(interrupted), Err(Error::NotFound(_))));
    rt.block_on(async {
        loop {
            if let Event::Removed { id } = watching.recv().await.unwrap() {
                assert_eq!(id, interrupted);
                break;
            }
        }
    });
    assert!(!folder.exists());

    server.gate.open();
    service.resume(kept).unwrap();
    assert_eq!(rt.block_on(ended(&mut events, kept, |_| {})), Status::Done);
    let done = service.task(kept).unwrap();
    assert_eq!(done.spec.headers.len(), 1);
    assert_eq!(output(&done), OutputSpec::Mp4(folder.join("gated.mp4")));
}

/// 设置里开了「开始前先停下确认」：没有要选的任务也停下，确认时可改保存目录与输出；换请求头即从头准备、再停在同一处。
#[test]
fn tasks_without_choices_wait_for_confirmation_when_asked_to() {
    let dir = test_dir("confirm");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    service.set_confirm_before_start(true).unwrap();
    assert!(service.settings().unwrap().confirm_before_start);

    let file = paste(
        &rt,
        &service,
        server.base.join("report.pdf").unwrap().as_str(),
        &dir.join("out"),
    )
    .id;
    let offer = rt.block_on(asked(&mut events, file));
    assert!(
        matches!(&offer, Offer::Confirm(Recognized::File(info)) if info.length == Some(FILE as u64)),
        "{offer:?}"
    );
    rt.block_on(stopped_waiting(&mut events, file));
    assert_eq!(service.task(file).unwrap().spec.title, "report.pdf");
    let cookie = vec![("Cookie".to_owned(), "a=1".to_owned())];
    assert_eq!(
        service
            .choose(file, Answer::Headers(cookie.clone()))
            .unwrap()
            .spec
            .headers,
        cookie
    );
    assert!(matches!(
        rt.block_on(asked(&mut events, file)),
        Offer::Confirm(Recognized::File(_))
    ));
    rt.block_on(stopped_waiting(&mut events, file));
    let elsewhere = dir.join("elsewhere");
    service
        .choose(
            file,
            Answer::File {
                folder: elsewhere.clone(),
            },
        )
        .unwrap();
    assert_eq!(rt.block_on(ended(&mut events, file, |_| {})), Status::Done);
    assert_eq!(
        std::fs::read(elsewhere.join("report.pdf")).unwrap(),
        file_body()
    );
    assert_eq!(service.task(file).unwrap().spec.headers, cookie);

    let hls = paste(
        &rt,
        &service,
        server.base.join("index.m3u8").unwrap().as_str(),
        &dir.join("out"),
    )
    .id;
    let offer = rt.block_on(asked(&mut events, hls));
    assert!(
        matches!(&offer, Offer::Confirm(Recognized::Hls(probe)) if probe.master.is_none() && !probe.live),
        "{offer:?}"
    );
    rt.block_on(stopped_waiting(&mut events, hls));
    service
        .choose(
            hls,
            Answer::Hls {
                preference: Preference::default(),
                output: OutputKind::Both,
                folder: elsewhere.clone(),
            },
        )
        .unwrap();
    assert_eq!(rt.block_on(ended(&mut events, hls, |_| {})), Status::Done);
    assert!(elsewhere.join("index.mp4").is_file());
    assert!(elsewhere.join("index").join("index.m3u8").is_file());
}

/// 地址回的是网页而这个平台没有解析组件：以「回的是网页」失败，还没定下；每个订阅方都收到同样的事件。
#[test]
fn a_page_without_a_resolver_fails_visibly() {
    let dir = test_dir("page");
    let rt = runtime();
    let server = rt.block_on(Server::start(true));
    let (service, mut events) = service(&dir.join("tasks.sqlite"));
    let mut second = service.subscribe();
    let id = paste(
        &rt,
        &service,
        server.base.join("page").unwrap().as_str(),
        &dir.join("out"),
    )
    .id;
    for events in [&mut events, &mut second] {
        let status = rt.block_on(ended(events, id, |_| {}));
        assert!(
            matches!(&status, Status::Failed(f) if f.kind == FailureKind::Malformed),
            "{status:?}"
        );
    }
    assert!(service.task(id).unwrap().spec.pending().is_some());
    // 没定下的任务没有只写出已录的
    assert!(matches!(service.write_recorded(id), Err(Error::NotHls(_))));
}
