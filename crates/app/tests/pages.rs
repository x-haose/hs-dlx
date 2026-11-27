//! 在内置网页里找流的端到端测试：真实的 SQLite 文件、测试内的本地网页与 HLS 服务、进程内的假宿主（网页由测试
//! 发事件）。任务都经粘贴建；等事件时把每一件都记进 [`Model`]，随手核对「以为自己开着网页的任务」不超过名额。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use hs_dlx_app::page::{Cookie, Stream};
use hs_dlx_app::{
    Action, Answer, Ask, Error, Event, FailureKind, Kind, Offer, OutputKind, Page, PageAsk,
    PageEvent, PageHost, PagePace, PageSession, Service, Stage, Status, Step, Store, TaskId,
    Unopened, Verdict,
};
use hs_m3u8_core::hls::Preference;
use hs_m3u8_core::{Proxy, Url};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

const PLAYLIST: &str =
    "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n";
const MASTER: &str = "#EXTM3U\n\
    #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nindex.m3u8?low\n\
    #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\nindex.m3u8\n";
/// `members/` 下的地址要带的 Cookie：网页引擎里有，直接请求没有。
const MEMBER: &str = "sid=1";
/// members/report.bin 的字节数。
const FILE: usize = 70_000;

/// 本地服务：`watch/…` 是网页；index.m3u8（两个分片）、master.m3u8（两档）是谁都能取的流，missing.m3u8 没有；
/// `members/` 下的 index.m3u8、它的分片与 report.bin 要带 [`MEMBER`] 这条 Cookie，不然回 403。
async fn serve(headers: HeaderMap, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media/ts_a");
    if path.starts_with("watch/") {
        return (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<html><body>播放器</body></html>",
        )
            .into_response();
    }
    let (member, name) = match path.strip_prefix("members/") {
        Some(name) => (true, name),
        None => (false, path),
    };
    let allowed = headers
        .get(header::COOKIE)
        .is_some_and(|cookie| cookie.as_bytes() == MEMBER.as_bytes());
    if member && !allowed {
        return StatusCode::FORBIDDEN.into_response();
    }
    match name {
        "index.m3u8" => PLAYLIST.into_response(),
        "master.m3u8" => MASTER.into_response(),
        "seg0.ts" | "seg1.ts" => std::fs::read(fixtures.join(name)).unwrap().into_response(),
        "report.bin" if member => vec![7u8; FILE].into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 假宿主：开网页只是记下来，网页里发生什么由测试发事件。
#[derive(Default)]
struct Host {
    state: Mutex<HostState>,
}

#[derive(Default)]
struct HostState {
    /// 开着的网页：向服务发事件的一端，与它正显示给用户做什么（收起着为 None）
    open: HashMap<TaskId, (mpsc::UnboundedSender<PageEvent>, Option<PageAsk>)>,
    /// 同时开着的网页最多到过几个
    peak: usize,
    /// 各任务开过几次网页
    opened: HashMap<TaskId, usize>,
    /// 主窗口关着：开不了网页
    away: bool,
    /// 下一次开网页回建不出
    broken: bool,
}

/// 假宿主里的一个网页；释放即从宿主里去掉。
struct FakePage {
    host: Arc<Host>,
    task: TaskId,
}

/// 假宿主作为 [`PageHost`] 注册的一端。
struct Hosting(Arc<Host>);

impl PageHost for Hosting {
    fn open(&self, task: TaskId, _: &Url, _: &Proxy) -> Result<PageSession, Unopened> {
        let mut state = self.0.state.lock().unwrap();
        if state.away {
            return Err(Unopened::Away);
        }
        if std::mem::take(&mut state.broken) {
            return Err(Unopened::Failed("没有显示器".to_owned()));
        }
        let (sender, events) = mpsc::unbounded_channel();
        assert!(
            state.open.insert(task, (sender, None)).is_none(),
            "{task}已开着一个网页"
        );
        state.peak = state.peak.max(state.open.len());
        *state.opened.entry(task).or_default() += 1;
        Ok(PageSession {
            events,
            page: Box::new(FakePage {
                host: self.0.clone(),
                task,
            }),
        })
    }
}

impl FakePage {
    fn set_shown(&self, shown: Option<PageAsk>) -> Result<(), String> {
        let mut state = self.host.state.lock().unwrap();
        let page = state.open.get_mut(&self.task).ok_or("网页已不在")?;
        page.1 = shown;
        Ok(())
    }
}

impl Page for FakePage {
    fn show(&self, reason: PageAsk) -> Result<(), String> {
        self.set_shown(Some(reason))
    }

    fn hide(&self) -> Result<(), String> {
        self.set_shown(None)
    }

    fn cookies(
        &self,
        _: &Url,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Cookie>, String>> + Send>> {
        // 网页引擎里登录过：一条只在同站带的 Cookie
        Box::pin(async {
            Ok(vec![Cookie {
                name: "sid".to_owned(),
                value: "1".to_owned(),
                cross_site: false,
            }])
        })
    }
}

impl Drop for FakePage {
    fn drop(&mut self) {
        self.host.state.lock().unwrap().open.remove(&self.task);
    }
}

impl Host {
    /// 开着网页的任务。
    fn open(&self) -> HashSet<TaskId> {
        self.state.lock().unwrap().open.keys().copied().collect()
    }

    /// 任务 `task` 的网页正显示给用户做什么；收起着或没开着时为 None。
    fn shown(&self, task: TaskId) -> Option<PageAsk> {
        self.state.lock().unwrap().open.get(&task)?.1
    }

    /// 任务 `task` 的网页里发生了 `event`。
    fn send(&self, task: TaskId, event: PageEvent) {
        let state = self.state.lock().unwrap();
        let (sender, _) = state.open.get(&task).expect("这个任务的网页开着");
        sender.send(event).expect("服务还在看着这个网页");
    }

    /// 主窗口关了：开着的网页都关掉，之后开不了。
    fn close_window(&self) {
        let mut state = self.state.lock().unwrap();
        state.away = true;
        for (sender, _) in state.open.values() {
            sender
                .send(PageEvent::Closed)
                .expect("服务还在看着这个网页");
        }
    }
}

/// 照事件记下的各任务的样子。
#[derive(Default)]
struct Model {
    tasks: HashMap<TaskId, (Status, Option<Stage>)>,
    /// 以为自己开着网页的任务最多到过几个
    peak: usize,
}

impl Model {
    fn on(&mut self, event: &Event) {
        match event {
            Event::Added { task } => {
                self.tasks.insert(task.id, (task.status.clone(), None));
            }
            Event::Changed { task } => {
                let entry = self
                    .tasks
                    .get_mut(&task.id)
                    .expect("任务的事件都在建了它之后");
                entry.0 = task.status.clone();
                if !matches!(task.status, Status::Preparing | Status::Waiting(_)) {
                    entry.1 = None;
                }
            }
            Event::Stage { id, stage } => {
                let entry = self.tasks.get_mut(id).expect("任务的事件都在建了它之后");
                entry.1 = stage.as_deref().cloned();
            }
            Event::Removed { id } => {
                self.tasks.remove(id);
            }
            _ => {}
        }
        self.peak = self.peak.max(self.paged().len());
    }

    /// 以为自己开着网页的任务：步骤是在网页里找流，或停在网页上等用户。
    fn paged(&self) -> HashSet<TaskId> {
        self.tasks
            .iter()
            .filter(|(_, (_, stage))| {
                matches!(
                    stage,
                    Some(Stage::Working(Step::Page { .. }) | Stage::Asking(Offer::Page(_)))
                )
            })
            .map(|(id, _)| *id)
            .collect()
    }

    fn status(&self, id: TaskId) -> Option<&Status> {
        self.tasks.get(&id).map(|(status, _)| status)
    }

    fn step(&self, id: TaskId) -> Option<&Step> {
        match self.tasks.get(&id) {
            Some((_, Some(Stage::Working(step)))) => Some(step),
            _ => None,
        }
    }

    fn offer(&self, id: TaskId) -> Option<&Offer> {
        match self.tasks.get(&id) {
            Some((_, Some(Stage::Asking(offer)))) => Some(offer),
            _ => None,
        }
    }
}

struct Rig {
    rt: Runtime,
    base: Url,
    folder: PathBuf,
    service: Service,
    events: mpsc::UnboundedReceiver<Event>,
    host: Arc<Host>,
    model: Model,
}

/// 找流的时长：采用与等播放都很短，定论给足。
fn quick() -> PagePace {
    PagePace {
        decide_within: Duration::from_secs(20),
        adopt_after: Duration::from_millis(150),
        play_after: Duration::from_millis(400),
    }
}

/// 找流的时长：到时没有流也不显示网页（由测试决定任务何时离开网页）。
fn patient() -> PagePace {
    PagePace {
        play_after: Duration::from_secs(600),
        ..quick()
    }
}

fn open_service(store: &Path) -> (Service, mpsc::UnboundedReceiver<Event>) {
    Service::new(
        Store::open(store).unwrap(),
        NonZeroUsize::new(8).unwrap(),
        hs_dlx_app::TorrentNetwork::Loopback {
            public_trackers: Vec::new(),
        },
        hs_dlx_app::pikpak::Endpoints::official(),
        // 没有解析组件：网页直接到内置网页里找流
        hs_dlx_app::WebConfig {
            root: std::env::temp_dir().join("hs-dlx-no-web"),
            bundle: None,
            sources: hs_dlx_app::ytdlp::Sources::official(),
            update_every: None,
        },
    )
    .unwrap()
}

impl Rig {
    /// 每个测试独立的空目录、本地服务、服务与注册好的假宿主。
    fn new(name: &str, pace: PagePace) -> Rig {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("pages")
            .join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let base = rt.block_on(async {
            let app = axum::Router::new().fallback(serve);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            base
        });
        let (service, events) = open_service(&dir.join("tasks.sqlite"));
        let host = Arc::new(Host::default());
        service.set_page_host(Arc::new(Hosting(host.clone())), pace);
        Rig {
            rt,
            base,
            folder: dir.join("out"),
            service,
            events,
            host,
            model: Model::default(),
        }
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).unwrap()
    }

    /// 粘贴本地服务上的 `path`，返回建出的任务。
    fn paste(&self, path: &str) -> TaskId {
        let url = self.url(path);
        let mut pasted = self
            .rt
            .block_on(self.service.paste(url.as_str(), &self.folder, 1))
            .unwrap();
        pasted.remove(0).expect("建出任务").id
    }

    /// 一边收事件（记进 [`Rig::model`]，核对以为开着网页的任务不超过 `limit` 个）一边等，直到 `done`；20 秒没等到即
    /// 失败，`what` 为在等什么。
    fn wait_within(&mut self, limit: usize, what: &str, done: impl Fn(&Rig) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            assert!(Instant::now() < deadline, "没等到：{what}");
            let received = self.rt.block_on(async {
                tokio::time::timeout(Duration::from_millis(50), self.events.recv()).await
            });
            if let Ok(event) = received {
                let event = event.expect("服务还在，事件通道不会关闭");
                self.model.on(&event);
                let paged = self.model.paged();
                assert!(
                    paged.len() <= limit,
                    "{} 个任务以为自己开着网页，名额只有 {limit} 个：{paged:?}（刚收到 {event:?}）",
                    paged.len()
                );
            }
        }
    }

    /// 同 [`Rig::wait_within`]，名额为默认的 2 个。
    fn wait(&mut self, what: &str, done: impl Fn(&Rig) -> bool) {
        self.wait_within(2, what, done);
    }

    /// 等到任务 `id` 的网页开着，并让它报出「是网页」。
    fn page_loaded(&mut self, id: TaskId) {
        self.wait("网页打开", |rig| rig.host.open().contains(&id));
        let top = self.url("watch/1");
        self.host.send(id, PageEvent::Navigated(top));
        self.host.send(id, PageEvent::Verdict(Verdict::Page));
    }

    /// 任务 `id` 的网页里找到本地服务上 `path` 的流，由顶层页面报出。
    fn stream(&self, id: TaskId, path: &str) {
        let top = self.url("watch/1");
        self.host.send(
            id,
            PageEvent::Stream(Stream {
                url: self.url(path),
                frame: Some(top.clone()),
                top: Some(top),
                agent: "FakeBrowser/1".to_owned(),
            }),
        );
    }

    /// 等到任务 `id` 的状态为 `status`。
    fn status(&mut self, id: TaskId, status: &Status) {
        self.wait(&format!("{id}变成 {status:?}"), |rig| {
            rig.model.status(id) == Some(status)
        });
    }

    /// 等到任务 `id` 失败，返回失败的种类与说明。
    fn failed(&mut self, id: TaskId) -> (FailureKind, String) {
        self.wait("失败", |rig| {
            matches!(rig.model.status(id), Some(Status::Failed(_)))
        });
        match self.model.status(id) {
            Some(Status::Failed(failure)) => (failure.kind, failure.message.clone()),
            other => unreachable!("刚等到失败：{other:?}"),
        }
    }
}

/// 网页里只找到一条流：等一阵没有第二条即采用，任务定下为 HLS 任务（地址是流的，请求头是照网页配的：网站作
/// Referer、网页的 User-Agent、同站的 Cookie；标题是网页的），网页随即关掉，照常下完。
#[test]
fn the_only_stream_found_in_a_page_is_adopted() {
    let mut rig = Rig::new("adopt", quick());
    let id = rig.paste("watch/1");
    rig.page_loaded(id);
    rig.host
        .send(id, PageEvent::Title("公开课 / 第 1 讲".to_owned()));
    rig.wait("找流", |rig| {
        rig.model.step(id) == Some(&Step::Page { found: 0 })
    });
    // 网页开着、没要用户做什么：收起着
    assert_eq!(rig.host.shown(id), None);
    assert_eq!(
        rig.service.actions(id).unwrap().actions,
        [Action::Pause, Action::Discard]
    );
    rig.stream(id, "members/index.m3u8");
    rig.status(id, &Status::Done);
    assert!(rig.host.open().is_empty(), "采用后网页关掉");
    let task = rig.service.task(id).unwrap();
    assert!(matches!(task.spec.kind, Kind::Hls(_)));
    assert_eq!(task.spec.url, rig.url("members/index.m3u8"));
    assert_eq!(task.spec.title, "公开课 _ 第 1 讲");
    let site = format!("{}/", rig.base.origin().ascii_serialization());
    let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
    assert_eq!(
        task.spec.headers,
        [
            pair("Referer", &site),
            pair("User-Agent", "FakeBrowser/1"),
            pair("Cookie", MEMBER),
        ]
    );
    assert!(rig.folder.join("公开课 _ 第 1 讲.mp4").is_file());
}

/// 找到几条探测成功的流：停下让用户选，没探测成的列着原因、不能选；选的那条有两档时接着停下选画质，定下后地址与
/// 请求头是那条流的。
#[test]
fn several_streams_stop_for_a_choice() {
    let mut rig = Rig::new("choose", quick());
    let id = rig.paste("watch/1");
    rig.page_loaded(id);
    rig.stream(id, "index.m3u8");
    rig.stream(id, "missing.m3u8");
    rig.stream(id, "master.m3u8");
    rig.status(id, &Status::Waiting(Ask::Stream));
    rig.wait("可选项", |rig| rig.model.offer(id).is_some());
    assert!(rig.host.open().is_empty(), "停下选流时网页关掉");
    let Some(Offer::Streams(options)) = rig.model.offer(id).cloned() else {
        panic!("应是选流：{:?}", rig.model.offer(id));
    };
    assert_eq!(
        options
            .iter()
            .map(|option| (option.url.path(), option.probed.is_ok()))
            .collect::<Vec<_>>(),
        [
            ("/index.m3u8", true),
            ("/missing.m3u8", false),
            ("/master.m3u8", true)
        ]
    );
    // 没探测成的、没有的不能选，任务照旧等着
    for wrong in [1, 3] {
        assert!(matches!(
            rig.service.choose(id, Answer::Stream(wrong)),
            Err(Error::InvalidInput(_))
        ));
    }
    assert_eq!(
        rig.service.actions(id).unwrap().actions,
        [Action::EditHeaders, Action::Discard]
    );
    rig.service.choose(id, Answer::Stream(2)).unwrap();
    rig.status(id, &Status::Waiting(Ask::Quality));
    rig.wait("画质的可选项", |rig| {
        matches!(rig.model.offer(id), Some(Offer::Quality(_)))
    });
    let answer = Answer::Hls {
        preference: Preference::default(),
        output: OutputKind::Mp4,
        folder: rig.folder.clone(),
    };
    rig.service.choose(id, answer).unwrap();
    rig.status(id, &Status::Done);
    let task = rig.service.task(id).unwrap();
    assert_eq!(task.spec.url, rig.url("master.m3u8"));
    assert!(
        task.spec
            .headers
            .iter()
            .any(|(name, _)| name == "User-Agent")
    );
}

/// 找到的流都没探测成：到时停下，列出原因；换请求头即从头准备，重新开网页。
#[test]
fn streams_that_all_failed_are_listed_with_their_reasons() {
    let mut rig = Rig::new("all-failed", quick());
    let id = rig.paste("watch/1");
    rig.page_loaded(id);
    rig.stream(id, "missing.m3u8");
    rig.status(id, &Status::Waiting(Ask::Stream));
    rig.wait("可选项", |rig| rig.model.offer(id).is_some());
    let Some(Offer::Streams(options)) = rig.model.offer(id).cloned() else {
        panic!("应是选流：{:?}", rig.model.offer(id));
    };
    let reason = options[0].probed.as_ref().unwrap_err();
    assert!(reason.contains("404"), "{reason}");
    let headers = vec![("X-Token".to_owned(), "1".to_owned())];
    rig.service
        .choose(id, Answer::Headers(headers.clone()))
        .unwrap();
    rig.wait("重新开网页", |rig| rig.host.open().contains(&id));
    assert_eq!(rig.service.task(id).unwrap().spec.headers, headers);
    assert_eq!(rig.host.state.lock().unwrap().opened[&id], 2);
}

/// 整页的人机验证：网页显示出来，任务停下等用户（照旧在跑、占着名额）；过了自动收起、接着找。到时还没有流：网页
/// 再显示出来等用户点播放，找到流后自己接着走到下完。
#[test]
fn a_challenge_then_a_page_that_needs_a_click() {
    let mut rig = Rig::new("challenge", quick());
    let id = rig.paste("watch/1");
    rig.wait("网页打开", |rig| rig.host.open().contains(&id));
    rig.host.send(id, PageEvent::Navigated(rig.url("watch/1")));
    rig.host
        .send(id, PageEvent::Title("Just a moment...".to_owned()));
    rig.host.send(id, PageEvent::Challenge(true));
    rig.host.send(id, PageEvent::Verdict(Verdict::Page));
    let challenge = Status::Waiting(Ask::Page(PageAsk::Challenge));
    rig.status(id, &challenge);
    rig.wait("验证的可选项", |rig| {
        rig.model.offer(id) == Some(&Offer::Page(PageAsk::Challenge))
    });
    rig.wait("网页显示出来", |rig| {
        rig.host.shown(id) == Some(PageAsk::Challenge)
    });
    assert_eq!(
        rig.service.stage(id).unwrap(),
        Some(Stage::Asking(Offer::Page(PageAsk::Challenge)))
    );
    // 验证页的标题不留在任务上；这一轮在跑，不能继续，也不到时显示成「点播放」
    assert_eq!(rig.service.task(id).unwrap().spec.title, "1");
    assert!(matches!(rig.service.resume(id), Err(Error::Running(_))));
    std::thread::sleep(quick().play_after * 2);
    assert_eq!(rig.service.task(id).unwrap().status, challenge);
    // 过了验证
    rig.host.send(id, PageEvent::Navigated(rig.url("watch/1")));
    rig.host.send(id, PageEvent::Challenge(false));
    rig.status(id, &Status::Preparing);
    rig.wait("网页收起", |rig| {
        rig.host.open().contains(&id) && rig.host.shown(id).is_none()
    });
    // 到时没有流
    let play = Status::Waiting(Ask::Page(PageAsk::Play));
    rig.status(id, &play);
    rig.wait("网页显示出来", |rig| {
        rig.host.shown(id) == Some(PageAsk::Play)
    });
    rig.host.send(id, PageEvent::Title("公开课".to_owned()));
    rig.stream(id, "index.m3u8");
    rig.status(id, &Status::Done);
    assert!(rig.host.open().is_empty());
    assert_eq!(rig.service.task(id).unwrap().spec.title, "公开课");
}

/// 停在网页上等用户的任务：暂停即关掉网页；换请求头顶掉这一轮、从头准备；关闭服务时照旧等着，重开后没有可选项，
/// 继续即重新准备。
#[test]
fn a_task_waiting_on_its_page_can_be_paused_reheaded_and_survives_a_restart() {
    let mut rig = Rig::new("waiting", quick());
    let id = rig.paste("watch/1");
    let play = Status::Waiting(Ask::Page(PageAsk::Play));
    rig.page_loaded(id);
    rig.status(id, &play);
    rig.service.pause(id);
    rig.status(id, &Status::Paused);
    assert!(rig.host.open().is_empty(), "暂停后网页关掉");
    assert_eq!(rig.service.stage(id).unwrap(), None);

    rig.service.resume(id).unwrap();
    rig.page_loaded(id);
    rig.status(id, &play);
    let headers = vec![("X-Token".to_owned(), "1".to_owned())];
    rig.service
        .choose(id, Answer::Headers(headers.clone()))
        .unwrap();
    rig.status(id, &Status::Preparing);
    rig.wait("重新开网页", |rig| {
        rig.host.state.lock().unwrap().opened[&id] == 3 && rig.host.open().contains(&id)
    });
    rig.host.send(id, PageEvent::Verdict(Verdict::Page));
    rig.status(id, &play);

    rig.service.close().unwrap();
    assert!(rig.host.open().is_empty(), "关闭后网页关掉");
    let Rig {
        service, folder, ..
    } = rig;
    let store = folder.with_file_name("tasks.sqlite");
    drop(service);
    let (service, _events) = open_service(&store);
    let task = service.task(id).unwrap();
    assert_eq!(task.status, play);
    assert_eq!(task.spec.headers, headers);
    assert_eq!(service.stage(id).unwrap(), None);
    assert_eq!(
        service.actions(id).unwrap().actions,
        [Action::Resume, Action::EditHeaders, Action::Discard]
    );
}

/// 网页引擎打开的不是网页：出错页以状态码失败；是播放列表、是文件的带着网页引擎里的 Cookie 重新取，照它定下。
#[test]
fn what_is_not_a_page_is_refetched_with_the_page_s_cookies() {
    let mut rig = Rig::new("not-a-page", quick());
    // 直接取回 403（缺 Cookie），到网页里：网页引擎也回 403
    let refused = rig.paste("members/index.m3u8");
    rig.wait("网页打开", |rig| rig.host.open().contains(&refused));
    rig.host
        .send(refused, PageEvent::Verdict(Verdict::Refused(403)));
    assert_eq!(rig.failed(refused).0, FailureKind::Status(403));
    assert!(rig.host.open().is_empty());
    assert_eq!(
        rig.service.actions(refused).unwrap().actions,
        [Action::PasteCurl, Action::EditHeaders, Action::Discard]
    );

    // 网页引擎打开它是播放列表
    rig.service.resume(refused).unwrap();
    rig.wait("网页打开", |rig| rig.host.open().contains(&refused));
    rig.host
        .send(refused, PageEvent::Verdict(Verdict::Playlist));
    rig.status(refused, &Status::Done);
    let task = rig.service.task(refused).unwrap();
    assert!(matches!(task.spec.kind, Kind::Hls(_)));
    assert_eq!(
        task.spec.headers,
        [("Cookie".to_owned(), MEMBER.to_owned())]
    );

    // 网页引擎要下载它
    let file = rig.paste("members/report.bin");
    rig.wait("网页打开", |rig| rig.host.open().contains(&file));
    rig.host.send(file, PageEvent::Verdict(Verdict::Download));
    rig.status(file, &Status::Done);
    let task = rig.service.task(file).unwrap();
    assert!(matches!(task.spec.kind, Kind::File(_)));
    assert_eq!(
        std::fs::metadata(rig.folder.join("report.bin"))
            .unwrap()
            .len(),
        FILE as u64
    );
}

/// 网页开不了、坏了、迟迟没有定论、宿主没交代就关了它：任务以失败结束，名额让出来。
#[test]
fn a_page_that_cannot_be_used_fails_the_task() {
    let pace = PagePace {
        decide_within: Duration::from_millis(300),
        ..quick()
    };
    let mut rig = Rig::new("broken", pace);
    rig.host.state.lock().unwrap().broken = true;
    let unbuilt = rig.paste("watch/1");
    let (kind, message) = rig.failed(unbuilt);
    assert_eq!(kind, FailureKind::Network);
    assert!(message.contains("没有显示器"), "{message}");

    let crashed = rig.paste("watch/2");
    rig.wait("网页打开", |rig| rig.host.open().contains(&crashed));
    rig.host
        .send(crashed, PageEvent::Failed("网页进程意外退出了".to_owned()));
    let (kind, message) = rig.failed(crashed);
    assert_eq!(kind, FailureKind::Network);
    assert!(message.contains("网页进程意外退出了"), "{message}");

    // 一直没有定论
    let silent = rig.paste("watch/3");
    assert_eq!(rig.failed(silent).0, FailureKind::Network);

    let dropped = rig.paste("watch/4");
    rig.wait("网页打开", |rig| rig.host.open().contains(&dropped));
    // 发送端没发 Closed 就释放（网页还登记着，由服务丢弃会话时去掉）
    let sender = {
        let mut state = rig.host.state.lock().unwrap();
        let (_, shown) = state.open[&dropped];
        let (replacement, _) = mpsc::unbounded_channel();
        state.open.get_mut(&dropped).unwrap().0 = replacement;
        shown
    };
    assert_eq!(sender, None);
    assert_eq!(rig.failed(dropped).0, FailureKind::Other);
    rig.wait("网页都关掉", |rig| rig.host.open().is_empty());
}

/// 没注册宿主：要到网页里找流的任务以原来的失败结束，说明里写明这里没有内置网页。
#[test]
fn without_a_host_the_task_fails_with_the_original_reason() {
    let rig = Rig::new("no-host", quick());
    let Rig {
        rt,
        base,
        folder,
        service,
        ..
    } = rig;
    let store = folder.with_file_name("tasks.sqlite");
    drop(service);
    let (service, mut events) = open_service(&store);
    let url = base.join("watch/1").unwrap();
    let mut pasted = rt
        .block_on(service.paste(url.as_str(), &folder, 1))
        .unwrap();
    let id = pasted.remove(0).unwrap().id;
    let failure = rt.block_on(async {
        loop {
            if let Event::Ended {
                id: at,
                status: Status::Failed(failure),
                ..
            } = events.recv().await.unwrap()
                && at == id
            {
                return failure;
            }
        }
    });
    assert_eq!(failure.kind, FailureKind::Malformed);
    assert!(failure.message.contains("没有内置网页"), "{failure:?}");
}

/// 六个任务抢两个名额：先到的两个开网页，其余按到的先后排队；暂停、放弃（占着的与排着的）、调名额、主窗口关掉又
/// 打开、采用了流、关闭服务，每一步之后开着网页的任务都正是该开的那几个。全程照事件看，以为自己开着网页的任务从不
/// 超过名额。
#[test]
fn tasks_take_turns_at_the_page_seats() {
    let mut rig = Rig::new("seats", patient());
    let queued = |ahead| Step::PageQueued { ahead, away: false };
    let away = |ahead| Step::PageQueued { ahead, away: true };
    // 先到先得按走到「要开网页」这一步的先后：一个到了再粘贴下一个
    let mut ids = Vec::new();
    for index in 0..6 {
        let id = rig.paste(&format!("watch/{index}"));
        rig.wait("开了网页或排上了", |rig| match index {
            0 | 1 => rig.host.open().contains(&id),
            _ => rig.model.step(id) == Some(&queued(index - 2)),
        });
        ids.push(id);
    }
    let [a, b, c, d, e, f] = ids[..] else {
        unreachable!("建了六个");
    };
    /// 等到开着网页的正是 `open` 这几个任务，事件里以为自己开着的也是它们；`steps` 里的任务排在所写的位置。
    fn settle(rig: &mut Rig, limit: usize, open: &[TaskId], steps: &[(TaskId, Step)]) {
        let open: HashSet<TaskId> = open.iter().copied().collect();
        rig.wait_within(
            limit,
            &format!("开着网页的是 {open:?}，排着的是 {steps:?}"),
            |rig| {
                rig.host.open() == open
                    && rig.model.paged() == open
                    && steps
                        .iter()
                        .all(|(id, step)| rig.model.step(*id) == Some(step))
            },
        );
    }
    let all_queued = [
        (c, queued(0)),
        (d, queued(1)),
        (e, queued(2)),
        (f, queued(3)),
    ];
    settle(&mut rig, 2, &[a, b], &all_queued);
    assert_eq!(
        rig.service.stage(f).unwrap(),
        Some(Stage::Working(queued(3)))
    );

    // 暂停占着名额的：下一个排着的补上
    rig.service.pause(a);
    rig.status(a, &Status::Paused);
    settle(
        &mut rig,
        2,
        &[b, c],
        &[(d, queued(0)), (e, queued(1)), (f, queued(2))],
    );
    // 放弃占着名额的
    rig.rt.block_on(rig.service.discard(b)).unwrap().unwrap();
    settle(&mut rig, 2, &[c, d], &[(e, queued(0)), (f, queued(1))]);
    // 暂停、放弃排着的：后面的往前挪，开着的不动
    rig.service.pause(e);
    rig.status(e, &Status::Paused);
    settle(&mut rig, 2, &[c, d], &[(f, queued(0))]);
    rig.rt.block_on(rig.service.discard(f)).unwrap().unwrap();
    settle(&mut rig, 2, &[c, d], &[]);
    // 继续的排到最后（先后按走到「要开网页」这一步：一个排上了再继续下一个）
    rig.service.resume(a).unwrap();
    settle(&mut rig, 2, &[c, d], &[(a, queued(0))]);
    rig.service.resume(e).unwrap();
    settle(&mut rig, 2, &[c, d], &[(a, queued(0)), (e, queued(1))]);

    // 名额调大：排着的随即放行；调小：开着的不关
    rig.service.set_web_pages(3).unwrap();
    settle(&mut rig, 3, &[c, d, a], &[(e, queued(0))]);
    rig.service.set_web_pages(1).unwrap();
    assert!(matches!(
        rig.service.set_web_pages(4),
        Err(Error::InvalidInput(_))
    ));
    settle(&mut rig, 3, &[c, d, a], &[(e, queued(0))]);

    // 主窗口关了：开着的都排回去，照原来的先后
    rig.host.close_window();
    let all_away = [(c, away(0)), (d, away(1)), (a, away(2)), (e, away(3))];
    settle(&mut rig, 3, &[], &all_away);
    for id in [c, d, a, e] {
        assert_eq!(rig.model.status(id), Some(&Status::Preparing));
    }
    // 窗口回来：名额此刻是 1 个
    rig.host.state.lock().unwrap().away = false;
    rig.service.set_page_host_ready(true);
    settle(
        &mut rig,
        1,
        &[c],
        &[(d, queued(0)), (a, queued(1)), (e, queued(2))],
    );
    rig.service.set_web_pages(2).unwrap();
    settle(&mut rig, 2, &[c, d], &[(a, queued(0)), (e, queued(1))]);

    // 采用了流：离开网页，下一个补上
    rig.host.send(c, PageEvent::Verdict(Verdict::Page));
    rig.stream(c, "index.m3u8");
    rig.status(c, &Status::Done);
    settle(&mut rig, 2, &[d, a], &[(e, queued(0))]);

    // 窗口关着时宿主开不了（没有开着的网页可关，服务从开不成得知）
    rig.service.pause(d);
    rig.service.pause(a);
    rig.status(a, &Status::Paused);
    settle(&mut rig, 2, &[e], &[]);
    rig.host.state.lock().unwrap().away = true;
    rig.service.resume(d).unwrap();
    settle(&mut rig, 2, &[e], &[(d, away(0))]);
    rig.host.state.lock().unwrap().away = false;
    rig.service.set_page_host_ready(true);
    settle(&mut rig, 2, &[e, d], &[]);

    // 关闭服务：网页都关掉，任务记为暂停
    rig.service.close().unwrap();
    assert!(rig.host.open().is_empty());
    for id in [d, e] {
        assert_eq!(rig.service.task(id).unwrap().status, Status::Paused);
    }
    let state = rig.host.state.lock().unwrap();
    assert!(state.peak <= 3, "同时开着 {} 个网页", state.peak);
    assert!(rig.model.peak <= 3);
}

/// 主窗口关掉时停在网页上等用户的任务：照旧等用户，可选项作废（事件里报出），名额让出来；继续即重新准备、排队。
#[test]
fn closing_the_window_voids_the_offer_of_a_task_waiting_on_its_page() {
    let mut rig = Rig::new("window", quick());
    let id = rig.paste("watch/1");
    let play = Status::Waiting(Ask::Page(PageAsk::Play));
    rig.page_loaded(id);
    rig.status(id, &play);
    rig.wait("可选项", |rig| rig.model.offer(id).is_some());
    rig.host.close_window();
    rig.wait("可选项作废", |rig| {
        rig.model.tasks.get(&id) == Some(&(play.clone(), None)) && rig.host.open().is_empty()
    });
    assert_eq!(rig.service.task(id).unwrap().status, play);
    assert_eq!(rig.service.stage(id).unwrap(), None);
    // 宿主还开不了：继续后排着
    rig.service.resume(id).unwrap();
    rig.wait("排着", |rig| {
        rig.model.step(id)
            == Some(&Step::PageQueued {
                ahead: 0,
                away: true,
            })
    });
    rig.host.state.lock().unwrap().away = false;
    rig.service.set_page_host_ready(true);
    rig.wait("重新开网页", |rig| rig.host.open().contains(&id));
}

/// 宿主关掉网页的消息晚到、它已经说了又开得了：不把它记成开不了，任务随即重新开网页，不用等它下一次回来。
#[test]
fn a_close_that_arrives_after_the_host_returned_does_not_hold_the_queue() {
    let mut rig = Rig::new("late-close", patient());
    let id = rig.paste("watch/1");
    let reopened = |times: usize| {
        move |rig: &Rig| {
            rig.host.state.lock().unwrap().opened[&id] == times && rig.host.open().contains(&id)
        }
    };
    rig.wait("网页打开", |rig| rig.host.open().contains(&id));
    // 窗口关了随即又打开：服务多半还没处理到 Closed
    rig.host.close_window();
    rig.host.state.lock().unwrap().away = false;
    rig.service.set_page_host_ready(true);
    rig.wait("重新开网页", reopened(2));
    // 宿主先说开得了，这之前占上名额的网页的 Closed 才到
    rig.service.set_page_host_ready(true);
    rig.host.send(id, PageEvent::Closed);
    rig.wait("再重新开网页", reopened(3));
    // 开不成（Away）的消息同样：宿主回来之后才处理到的不算
    rig.service.pause(id);
    rig.status(id, &Status::Paused);
    rig.host.state.lock().unwrap().away = true;
    rig.service.resume(id).unwrap();
    rig.wait("排着", |rig| {
        rig.model.step(id)
            == Some(&Step::PageQueued {
                ahead: 0,
                away: true,
            })
    });
    rig.host.state.lock().unwrap().away = false;
    rig.service.set_page_host_ready(true);
    rig.wait("又开了网页", reopened(4));
}

/// 暂停了停在网页上等用户的任务、随即退出：照暂停记，不当成退出时还等着用户的。
#[test]
fn pausing_a_task_waiting_on_its_page_right_before_quitting_leaves_it_paused() {
    let mut rig = Rig::new("pause-then-quit", quick());
    let id = rig.paste("watch/1");
    rig.page_loaded(id);
    rig.status(id, &Status::Waiting(Ask::Page(PageAsk::Play)));
    rig.service.pause(id);
    rig.service.close().unwrap();
    assert!(rig.host.open().is_empty());
    assert_eq!(rig.service.task(id).unwrap().status, Status::Paused);
}
