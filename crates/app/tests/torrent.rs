//! BT 任务的端到端测试：真实的 SQLite 文件，服务的 BT 会话只在回环地址上；做种的是测试自己建的另一个会话，磁力链接用
//! `x.pe` 给出它的地址（之后的任务从续传数据里记的节点连它）。服务自带 tokio 运行时，在测试自己的运行时外面建、外面
//! 丢弃；等事件在测试的运行时里。

use std::net::{Ipv4Addr, SocketAddrV4};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hs_dlx_app::{
    CheckItem, CheckLevel, Draft, DraftKind, Error, Event, SeedEnd, Seeding, Service, Status,
    Store, TaskId, TorrentInfo, TorrentNetwork, TorrentPhase, TorrentProgress,
};
use hs_dlx_torrent::{
    AddOptions, CreateOptions, Session, SessionConfig, State, TorrentId, create_torrent,
};
use hs_m3u8_core::{Proxy, Url};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

/// 每个测试独立的空目录。
fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("app-torrent")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `len` 字节的内容，按 `seed` 变化。
fn content(len: u32, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

const A: u32 = 100_000;
const B: u32 = 70_001;
/// 分块大小。libtorrent 建的是 v1 与 v2 兼有的种子，每个文件从新的一块开始（中间是对齐用的填充文件）：a.bin 占 7 块、
/// b.bin 占 5 块
const PIECE: i32 = 16 << 10;

/// 在 `dir` 下写 album/a.bin 与 album/b.bin，建种子（`v1_only` 时只有 v1：文件首尾相接，第 7 块跨 a.bin 与 b.bin）；
/// `without_b` 时建完再删掉 b.bin，做种的一方只有 a.bin 的块。在只监听回环地址的新会话里校验已有的文件后上传，返回
/// 会话与种子的编号。
fn seed(dir: &Path, without_b: bool, v1_only: bool) -> (Session, TorrentId) {
    let options = CreateOptions {
        piece_size: PIECE,
        v1_only,
        private_torrent: false,
        tracker: String::new(),
    };
    let (seeder, id, _) = seed_with(dir, without_b, &options);
    (seeder, id)
}

/// 同 [`seed`]，种子照 `options` 建；另返回种子文件。
fn seed_with(
    dir: &Path,
    without_b: bool,
    options: &CreateOptions,
) -> (Session, TorrentId, Vec<u8>) {
    let album = dir.join("album");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::write(album.join("a.bin"), content(A, 1)).unwrap();
    std::fs::write(album.join("b.bin"), content(B, 2)).unwrap();
    let torrent = create_torrent(album.to_str().unwrap(), options).unwrap();
    if without_b {
        std::fs::remove_file(album.join("b.bin")).unwrap();
    }
    let resume = hs_dlx_torrent::resume_from_torrent_file(&torrent).unwrap();
    let id = hs_dlx_torrent::describe(&resume).unwrap().id;
    let seeder = Session::new(&SessionConfig {
        listen_interfaces: "127.0.0.1:0".into(),
        dht: false,
        dht_bootstrap_nodes: String::new(),
        upnp: false,
        natpmp: false,
        lsd: false,
        user_agent: "hs-dlx-test-seeder".into(),
        peer_fingerprint: "-HD0000-".into(),
        upload_limit: 0,
        proxy: Default::default(),
        state: Vec::new(),
    })
    .unwrap();
    let options = AddOptions {
        trust_pieces: false,
        paused: false,
        upload_only: false,
    };
    seeder
        .add_resume(&resume, &id, dir.to_str().unwrap(), options)
        .unwrap();
    // 等它校验完、开始监听；取事件即是等待（不取时事件队列会满）
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let state = seeder.status(&id).unwrap().state();
        if matches!(state, Some(State::Seeding | State::Downloading)) && seeder.listen_port() != 0 {
            return (seeder, id, torrent);
        }
        assert!(Instant::now() < deadline, "做种的一方 20 秒内没准备好");
        seeder.pop_alerts(100).unwrap();
    }
}

fn magnet(id: &TorrentId, seeder: &Session) -> String {
    format!(
        "magnet:?xt=urn:btih:{id}&x.pe=127.0.0.1:{}",
        seeder.listen_port()
    )
}

fn service(store: &Path) -> (Service, mpsc::UnboundedReceiver<Event>) {
    service_with(store, Vec::new())
}

/// 公共 tracker 列表依次从 `public_trackers` 取的服务。
fn service_with(
    store: &Path,
    public_trackers: Vec<Url>,
) -> (Service, mpsc::UnboundedReceiver<Event>) {
    Service::new(
        Store::open(store).unwrap(),
        NonZeroUsize::new(8).unwrap(),
        TorrentNetwork::Loopback { public_trackers },
        hs_dlx_app::pikpak::Endpoints::official(),
        // 网页视频不在这里测：不给组件
        hs_dlx_app::WebConfig {
            root: std::env::temp_dir().join("hs-dlx-no-web"),
            bundle: None,
            sources: hs_dlx_app::ytdlp::Sources::official(),
        },
    )
    .unwrap()
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// 磁力链接取到的续传数据。
fn fetch(rt: &Runtime, service: &Service, uri: &str) -> Vec<u8> {
    let fetch = service.fetch_metadata(uri.to_owned()).unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), fetch.resume)
            .await
            .expect("30 秒内没取到种子信息")
    })
    .unwrap()
    .unwrap()
}

fn draft(uri: &str, folder: &Path, resume: Vec<u8>, wanted: Vec<i32>) -> Draft {
    Draft {
        url: uri.parse().unwrap(),
        headers: Vec::new(),
        folder: folder.to_path_buf(),
        kind: DraftKind::Torrent { resume, wanted },
    }
}

/// 等到任务 `id` 停下、状态写进存储，返回它的状态；其间的进度交给 `on_progress`，它返回 true 时不再等、返回 None。
async fn until(
    events: &mut mpsc::UnboundedReceiver<Event>,
    id: TaskId,
    mut on_progress: impl FnMut(&TorrentProgress) -> bool,
) -> Option<Status> {
    let waiting = async {
        loop {
            match events.recv().await.expect("服务还在，事件通道不会关闭") {
                Event::TorrentProgress { id: at, progress } if at == id => {
                    if on_progress(&progress) {
                        return None;
                    }
                }
                Event::Ended {
                    id: at,
                    status,
                    unsaved,
                    ..
                } if at == id => {
                    assert!(unsaved.is_none(), "没写进存储：{unsaved:?}");
                    return Some(status);
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(60), waiting)
        .await
        .expect("60 秒内没等到")
}

fn index_of(info: &TorrentInfo, name: &str) -> i32 {
    info.files
        .iter()
        .find(|file| file.path.ends_with(name))
        .unwrap()
        .index
}

/// 磁力链接取种子信息、只选一个文件下完（设置为下完不做种）：根名避开保存目录里已有的，完成的结果为选了的字节数，不
/// 做种；同一个种子不能再建，完成的可以从列表移除；关闭时记下 BT 会话的端口与 DHT 状态。
#[test]
fn a_magnet_link_downloads_the_chosen_file() {
    let dir = test_dir("download");
    let (seeder, id) = seed(&dir.join("seed"), false, false);
    let folder = dir.join("downloads");
    std::fs::create_dir_all(folder.join("album")).unwrap();
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let (service, mut events) = service(&store);
    service.set_seeding(Seeding::Off).unwrap();

    let uri = magnet(&id, &seeder);
    let resume = fetch(&rt, &service, &uri);
    let info = TorrentInfo::of(&resume).unwrap();
    assert_eq!(
        (info.name.as_str(), info.files.len(), info.done),
        ("album", 2, 0)
    );
    let wanted = vec![index_of(&info, "a.bin")];
    let task = rt
        .block_on(service.create(draft(&uri, &folder, resume.clone(), wanted.clone()), 1))
        .unwrap()
        .unwrap();
    assert_eq!(task.spec.title, "album 2");
    assert_eq!(task.spec.torrent().unwrap().root, folder.join("album 2"));
    assert_eq!(task.spec.torrent().unwrap().info_hash, id.as_str());

    let status = rt.block_on(until(&mut events, task.id, |_| false));
    assert_eq!(status, Some(Status::Done));
    let done = service.task(task.id).unwrap();
    assert!(!done.seeding);
    assert_eq!(done.result.unwrap().bytes, u64::from(A));
    // 不在下也不在做种：不能诊断
    assert!(matches!(
        rt.block_on(service.diagnose(task.id)).unwrap(),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        std::fs::read(folder.join("album 2").join("a.bin")).unwrap(),
        content(A, 1)
    );
    assert!(!folder.join("album 2").join("b.bin").exists());

    let again = rt
        .block_on(service.create(draft(&uri, &folder, resume, wanted), 2))
        .unwrap();
    assert!(
        matches!(again, Err(Error::TorrentTaken(at)) if at == task.id),
        "{again:?}"
    );
    rt.block_on(service.forget(task.id)).unwrap().unwrap();

    service.close().unwrap();
    // 关闭后不再建 BT 会话
    assert!(matches!(
        service.fetch_metadata(uri.clone()),
        Err(Error::Closed)
    ));
    drop(service);
    let conn = rusqlite::Connection::open(&store).unwrap();
    let (port, dht): (Option<i64>, Option<Vec<u8>>) = conn
        .query_row(
            "SELECT torrent_port, torrent_dht FROM settings WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(port.is_some() && dht.is_some_and(|dht| !dht.is_empty()));
    drop(seeder);
}

/// 做种的一方只有一部分：下到凑不齐为止（能凑齐的块数少于要下的）；暂停时存下已有的块，重开应用后看得到、继续时不
/// 重下；放弃时删掉下了的文件与记录。
#[test]
fn a_task_that_cannot_finish_keeps_what_it_has_until_discarded() {
    let dir = test_dir("partial");
    let (seeder, id) = seed(&dir.join("seed"), true, false);
    let folder = dir.join("downloads");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let (service, mut events) = service(&store);

    let uri = magnet(&id, &seeder);
    let resume = fetch(&rt, &service, &uri);
    let info = TorrentInfo::of(&resume).unwrap();
    let all = info.files.iter().map(|file| file.index).collect();
    let task = rt
        .block_on(service.create(draft(&uri, &folder, resume, all), 1))
        .unwrap()
        .unwrap();
    // a.bin 的 7 块，共 12 块
    let have = u64::from(A);
    let mut last = None;
    let stopped = rt.block_on(until(&mut events, task.id, |progress| {
        last = Some(progress.clone());
        progress.done == have && progress.reachable == 7
    }));
    assert_eq!(stopped, None, "凑不齐的任务停下了");
    let last = last.unwrap();
    assert_eq!(
        (last.pieces, last.wanted, last.seeds),
        (12, u64::from(A + B), 0)
    );

    service.pause(task.id);
    let status = rt.block_on(until(&mut events, task.id, |_| false));
    assert_eq!(status, Some(Status::Paused));
    let saved = rt
        .block_on(service.inspect_torrent(task.id))
        .unwrap()
        .unwrap();
    assert!(saved.trusted);
    assert_eq!(
        (saved.info.done, saved.info.wanted()),
        (have, u64::from(A + B))
    );

    service.close().unwrap();
    drop(service);
    let (service, mut events) = self::service(&store);
    assert_eq!(service.task(task.id).unwrap().status, Status::Paused);
    service.resume(task.id).unwrap();
    // 信续传数据：一开始就是已有的块，不重新校验
    let mut first = None;
    rt.block_on(until(&mut events, task.id, |progress| {
        first.get_or_insert_with(|| progress.clone());
        true
    }));
    assert_eq!(first.unwrap().done, have);
    // 从续传数据里记的节点连回做种的一方
    let reconnected = rt.block_on(until(&mut events, task.id, |progress| {
        progress.peers > 0 && progress.reachable == 7
    }));
    assert_eq!(reconnected, None, "继续后的任务停下了");

    // 连着节点而凑不齐，持续一阵才算资源不足
    let scarce = rt.block_on(until(&mut events, task.id, |progress| progress.scarce));
    assert_eq!(scarce, None, "继续后的任务停下了");

    // 诊断：在回环地址上监听着；测试的会话不开 DHT，种子里没有 tracker（不算问题：非私有种子靠 DHT 与公共 tracker）
    let checks = rt.block_on(service.diagnose(task.id)).unwrap().unwrap();
    let level = |item| checks.iter().find(|c| c.item == item).unwrap().level;
    assert_eq!(checks.len(), 8);
    assert_eq!(level(CheckItem::Listen), CheckLevel::Ok);
    assert_eq!(level(CheckItem::Dht), CheckLevel::Warn);
    assert_eq!(level(CheckItem::Trackers), CheckLevel::Warn);

    // 根里有用户自己放的文件：删不干净，写明留下了什么，任务留着
    let root = folder.join("album");
    assert!(root.join("a.bin").exists());
    std::fs::write(root.join("笔记.txt"), b"mine").unwrap();
    std::fs::write(root.join(".DS_Store"), b"finder").unwrap();
    let left = rt.block_on(service.discard(task.id)).unwrap();
    match left {
        Err(Error::ContentLeft { id, cause }) => {
            assert_eq!(id, task.id);
            assert!(cause.to_string().contains("笔记.txt"), "{cause}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        service.task(task.id).unwrap().status,
        Status::DiscardUnfinished
    );
    assert!(!root.join("a.bin").exists());
    // 拿走自己的文件后再放弃：操作系统建的文件（.DS_Store）一起删掉
    std::fs::remove_file(root.join("笔记.txt")).unwrap();
    let leftovers = rt.block_on(service.discard(task.id)).unwrap().unwrap();
    assert!(leftovers.is_empty());
    assert!(!root.exists(), "下了的文件与目录都删掉了");
    assert!(matches!(service.task(task.id), Err(Error::NotFound(_))));
    drop(service);
    drop(seeder);
}

/// 新建 BT 任务时核对选的文件与请求头；种子文件读不出时报错。
#[test]
fn torrent_drafts_are_checked() {
    let dir = test_dir("drafts");
    let (seeder, id) = seed(&dir.join("seed"), false, false);
    let rt = runtime();
    let (service, _events) = service(&dir.join("tasks.sqlite"));
    let uri = magnet(&id, &seeder);
    let resume = fetch(&rt, &service, &uri);
    let folder = dir.join("downloads");
    for (wanted, headers) in [
        (vec![], vec![]),
        (vec![99], vec![]),
        (vec![0], vec![("Cookie".to_owned(), "a=1".to_owned())]),
    ] {
        let mut draft = draft(&uri, &folder, resume.clone(), wanted);
        draft.headers = headers;
        let created = rt.block_on(service.create(draft, 1)).unwrap();
        assert!(
            matches!(created, Err(Error::InvalidInput(_))),
            "{created:?}"
        );
    }
    assert!(matches!(
        hs_dlx_app::torrent_from_file(b"not bencode"),
        Err(Error::InvalidInput(_))
    ));
    assert!(service.tasks().unwrap().is_empty());
    drop(service);
    drop(seeder);
}

/// 取种子信息时取消（释放句柄）后马上再取同一个磁力链接：加得进，不报「正在取」或「还在移除中」。
#[test]
fn a_cancelled_fetch_can_be_retried_at_once() {
    let dir = test_dir("refetch");
    let rt = runtime();
    let (service, _events) = service(&dir.join("tasks.sqlite"));
    // 没有节点、不补 tracker：一直取不到
    let uri = format!("magnet:?xt=urn:btih:{}", "ab".repeat(20));
    for round in 0..10 {
        let mut fetch = service.fetch_metadata(uri.clone()).unwrap();
        // 第一次报进度时已加进会话
        let added = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), fetch.progress.changed()).await
        });
        assert!(matches!(added, Ok(Ok(()))), "第 {round} 次没加进会话");
        // 下一轮随即再取
        drop(fetch);
    }
    drop(service);
}

/// 等到 `pick` 认的一个事件（期间的别的丢掉），返回它认出的；60 秒内没等到即失败。
async fn event<T>(
    events: &mut mpsc::UnboundedReceiver<Event>,
    mut pick: impl FnMut(Event) -> Option<T>,
) -> T {
    let waiting = async {
        loop {
            let event = events.recv().await.expect("服务还在，事件通道不会关闭");
            if let Some(found) = pick(event) {
                return found;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(60), waiting)
        .await
        .expect("60 秒内没等到")
}

/// 任务 `id` 的做种停了，返回原因（停下时的进度与「不再做种」都写进了存储）。
async fn seeding_ended(events: &mut mpsc::UnboundedReceiver<Event>, id: TaskId) -> SeedEnd {
    event(events, |event| match event {
        Event::SeedingEnded {
            id: at,
            end,
            unsaved,
        } if at == id => {
            assert!(unsaved.is_none(), "没写进存储：{unsaved:?}");
            Some(end)
        }
        _ => None,
    })
    .await
}

/// 任务 `id` 做着种（校验完、在上传）的一次进度。
async fn seeding(events: &mut mpsc::UnboundedReceiver<Event>, id: TaskId) -> TorrentProgress {
    event(events, |event| match event {
        Event::TorrentProgress { id: at, progress }
            if at == id && progress.phase == TorrentPhase::Seeding =>
        {
            Some(*progress)
        }
        _ => None,
    })
    .await
}

/// 照默认的设置下完接着做种；重开应用接着做；设置改成下完不做种即停；手动停止与再做种；文件不全时停下、不补下；做种
/// 时从列表移除，先停止做种、停下后删掉记录。
#[test]
fn a_finished_task_seeds_until_stopped_or_the_setting_says_so() {
    let dir = test_dir("seeding");
    let (seeder, id) = seed(&dir.join("seed"), false, false);
    let folder = dir.join("downloads");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let (service, mut events) = service(&store);

    let uri = magnet(&id, &seeder);
    let resume = fetch(&rt, &service, &uri);
    let info = TorrentInfo::of(&resume).unwrap();
    let all = info.files.iter().map(|file| file.index).collect();
    let task = rt
        .block_on(service.create(draft(&uri, &folder, resume, all), 1))
        .unwrap()
        .unwrap();
    let status = rt.block_on(until(&mut events, task.id, |_| false));
    assert_eq!(status, Some(Status::Done));
    let progress = rt.block_on(seeding(&mut events, task.id));
    assert_eq!(progress.done, u64::from(A + B));
    assert!(service.task(task.id).unwrap().seeding);
    // 别的 BT 任务没在做种，不能停
    service.stop_seeding(TaskId(task.id.0 + 100)).unwrap();

    // 退出时停下、记着在做种，重开接着做
    service.close().unwrap();
    drop(service);
    let (service, mut events) = self::service(&store);
    assert!(service.task(task.id).unwrap().seeding);
    rt.block_on(event(&mut events, |event| {
        matches!(event, Event::SeedingStarted { id } if id == task.id).then_some(())
    }));
    rt.block_on(seeding(&mut events, task.id));

    // 设置改成下完不做种：到了停的条件
    service.set_seeding(Seeding::Off).unwrap();
    assert_eq!(
        rt.block_on(seeding_ended(&mut events, task.id)),
        SeedEnd::Reached
    );
    assert!(!service.task(task.id).unwrap().seeding);
    assert!(matches!(
        service.seed(TaskId(task.id.0 + 100)),
        Err(Error::NotFound(_))
    ));

    // 手动再做种、再停
    service.set_seeding(Seeding::default()).unwrap();
    service.seed(task.id).unwrap();
    assert!(matches!(service.seed(task.id), Err(Error::Seeding(_))));
    rt.block_on(seeding(&mut events, task.id));
    service.stop_seeding(task.id).unwrap();
    assert_eq!(
        rt.block_on(seeding_ended(&mut events, task.id)),
        SeedEnd::Stopped
    );
    assert!(!service.task(task.id).unwrap().seeding);

    // 删了一个文件再做种：校验出不全即停，不补下
    let b = folder.join("album").join("b.bin");
    std::fs::remove_file(&b).unwrap();
    service.seed(task.id).unwrap();
    assert_eq!(
        rt.block_on(seeding_ended(&mut events, task.id)),
        SeedEnd::Incomplete
    );
    assert!(!b.exists(), "做种时补下了");
    assert!(!service.task(task.id).unwrap().seeding);
    // 文件放回去再做种：存着的进度没被不全的那次盖掉，照常做种
    std::fs::copy(dir.join("seed").join("album").join("b.bin"), &b).unwrap();
    service.seed(task.id).unwrap();
    rt.block_on(seeding(&mut events, task.id));
    let forgetting = service.forget(task.id);
    assert_eq!(
        rt.block_on(seeding_ended(&mut events, task.id)),
        SeedEnd::Stopped
    );
    rt.block_on(forgetting).unwrap().unwrap();
    assert!(matches!(service.task(task.id), Err(Error::NotFound(_))));
    assert!(b.exists(), "移除删了下好的文件");
    drop(service);
    drop(seeder);
}

/// 只有 v1 的种子只选 a.bin：跨 a.bin 与 b.bin 的那块里 b.bin 的部分存在保存目录的 .parts 里。下完做种、退出再打开
/// 接着做种、停止做种都留着它（再做种要读），从列表移除时一起删。
#[test]
fn the_part_file_stays_for_seeding_until_the_task_is_forgotten() {
    let dir = test_dir("part-file");
    let (seeder, id) = seed(&dir.join("seed"), false, true);
    let folder = dir.join("downloads");
    let store = dir.join("tasks.sqlite");
    let rt = runtime();
    let (service, mut events) = service(&store);

    let uri = magnet(&id, &seeder);
    let resume = fetch(&rt, &service, &uri);
    let info = TorrentInfo::of(&resume).unwrap();
    let task = rt
        .block_on(service.create(
            draft(&uri, &folder, resume, vec![index_of(&info, "a.bin")]),
            1,
        ))
        .unwrap()
        .unwrap();
    let status = rt.block_on(until(&mut events, task.id, |_| false));
    assert_eq!(status, Some(Status::Done));
    rt.block_on(seeding(&mut events, task.id));
    let part = folder.join(&info.part_file);
    assert!(part.exists(), "{} 不在", part.display());

    service.close().unwrap();
    drop(service);
    assert!(part.exists());
    let (service, mut events) = self::service(&store);
    rt.block_on(seeding(&mut events, task.id));
    service.stop_seeding(task.id).unwrap();
    assert_eq!(
        rt.block_on(seeding_ended(&mut events, task.id)),
        SeedEnd::Stopped
    );
    assert!(part.exists());
    rt.block_on(service.forget(task.id)).unwrap().unwrap();
    // .parts 在后台删：关闭时等后台的收尾做完
    service.close().unwrap();
    assert!(!part.exists(), "从列表移除时删掉 .parts");
    assert!(
        folder.join("album").join("a.bin").exists(),
        "下的文件归用户"
    );
    drop(service);
    drop(seeder);
}

/// 还没连上过节点时 tracker 报错照常报出，为「tracker 的地址：原因」：磁力链接只带一个连不上的 tracker、不给节点，取
/// 种子信息时报它连不上。
#[test]
fn a_tracker_error_is_shown_while_no_peer_has_connected() {
    // 本机这个端口没人监听：通告随即被拒
    let refused = "http://127.0.0.1:9/announce";
    let rt = runtime();
    let (service, _events) = service(&test_dir("tracker-error").join("tasks.sqlite"));
    // 系统代理可能不放过回环地址
    service.set_proxy(&Proxy::Direct).unwrap();
    let uri = format!("magnet:?xt=urn:btih:5123456789abcdef0123456789abcdef01234567&tr={refused}");
    let mut fetch = service.fetch_metadata(uri).unwrap();
    let shown = rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(error) = fetch.progress.borrow_and_update().tracker_error.clone() {
                    return error;
                }
                fetch.progress.changed().await.unwrap();
            }
        })
        .await
        .expect("20 秒内没报 tracker 出错")
    });
    assert!(shown.starts_with(&format!("{refused}：")), "{shown}");
    assert_eq!(fetch.progress.borrow().peers, 0);
}

/// 测试的 tracker：在回环地址上的 HTTP 服务，`/list` 为只有它自己的公共 tracker 列表，`/announce` 回一个节点（建时
/// 给出，[`Tracker::point_to`] 可改）；记下通告来的信息哈希（十六进制）。在自己的线程与运行时上跑，随测试进程结束。
struct Tracker {
    list: Url,
    announce: String,
    peer: Arc<Mutex<SocketAddrV4>>,
    announced: Arc<Mutex<Vec<String>>>,
}

impl Tracker {
    fn start(peer: SocketAddrV4) -> Tracker {
        use axum::extract::{RawQuery, State as Shared};
        type Answer = (Arc<Mutex<SocketAddrV4>>, Arc<Mutex<Vec<String>>>);
        async fn announce(
            Shared((peer, announced)): Shared<Answer>,
            RawQuery(query): RawQuery,
        ) -> Vec<u8> {
            let query = query.unwrap_or_default();
            let hash = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("info_hash="))
                .map(|hash| {
                    percent_encoding::percent_decode_str(hash)
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                })
                .unwrap_or_default();
            announced.lock().unwrap().push(hash);
            let port = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("port="))
                .and_then(|port| port.parse::<u16>().ok());
            let peer = *peer.lock().unwrap();
            // 同真实的 tracker：不把请求者自己回给它
            if port == Some(peer.port()) {
                return b"d8:intervali60e5:peers0:e".to_vec();
            }
            let mut body = b"d8:intervali60e5:peers6:".to_vec();
            body.extend_from_slice(&peer.ip().octets());
            body.extend_from_slice(&peer.port().to_be_bytes());
            body.push(b'e');
            body
        }
        let peer = Arc::new(Mutex::new(peer));
        let announced = Arc::new(Mutex::new(Vec::new()));
        let (sender, receiver) = std::sync::mpsc::channel();
        let shared: Answer = (peer.clone(), announced.clone());
        std::thread::spawn(move || {
            let rt = runtime();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let list = format!("{base}/announce\n");
                let app = axum::Router::new()
                    .route("/list", axum::routing::get(move || async move { list }))
                    .route("/announce", axum::routing::get(announce))
                    .with_state(shared);
                sender.send(base).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        });
        let base = receiver.recv().unwrap();
        Tracker {
            list: Url::parse(&format!("{base}/list")).unwrap(),
            announce: format!("{base}/announce"),
            peer,
            announced,
        }
    }

    /// 之后的通告回 `seeder` 的地址。
    fn point_to(&self, seeder: &Session) {
        *self.peer.lock().unwrap() = SocketAddrV4::new(Ipv4Addr::LOCALHOST, seeder.listen_port());
    }

    fn announced(&self, id: &TorrentId) -> bool {
        self.announced.lock().unwrap().contains(&id.to_string())
    }
}

/// 磁力链接既没带 tracker 也没给节点（测试的会话不开 DHT）：只能经补的公共 tracker（测试的 tracker，回做种一方的地址）
/// 找到节点、取到种子信息；取到的续传数据里没有补的 tracker。下完接着做种时诊断报补上了它。带了 tracker 的磁力链接取
/// 种子信息时不补；私有种子（由种子文件建）下载、做种时都不补：公共 tracker 收不到它们的通告。
#[test]
fn public_trackers_go_to_bare_magnet_links_and_never_to_private_torrents() {
    let dir = test_dir("public-trackers");
    let (seeder, id) = seed(&dir.join("seed"), false, false);
    let public = Tracker::start(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    public.point_to(&seeder);
    let rt = runtime();
    let (service, mut events) = service_with(&dir.join("tasks.sqlite"), vec![public.list.clone()]);
    // 系统代理可能不放过回环地址
    service.set_proxy(&Proxy::Direct).unwrap();

    let uri = format!("magnet:?xt=urn:btih:{id}");
    let resume = fetch(&rt, &service, &uri);
    assert!(public.announced(&id));
    let contains = |data: &[u8], text: &str| data.windows(text.len()).any(|w| w == text.as_bytes());
    assert!(
        !contains(&resume, &public.announce),
        "补的 tracker 存进了续传数据"
    );
    let info = TorrentInfo::of(&resume).unwrap();
    let all = info.files.iter().map(|file| file.index).collect();
    let task = rt
        .block_on(service.create(draft(&uri, &dir.join("downloads"), resume, all), 1))
        .unwrap()
        .unwrap();
    assert_eq!(
        rt.block_on(until(&mut events, task.id, |_| false)),
        Some(Status::Done)
    );
    rt.block_on(seeding(&mut events, task.id));
    let public_check = |id| {
        rt.block_on(service.diagnose(id))
            .unwrap()
            .unwrap()
            .into_iter()
            .find(|check| check.item == CheckItem::PublicTrackers)
            .unwrap()
    };
    let check = public_check(task.id);
    assert_eq!(
        (check.level, check.text.as_str()),
        (CheckLevel::Ok, "补了 1 个公共 tracker，1 个回应了")
    );

    // 带了 tracker 的磁力链接取种子信息时不补（经它自己的 tracker 找到做种一方）
    let own = Tracker::start(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
    let (tracked_seeder, tracked_id) = seed(&dir.join("tracked-seed"), false, true);
    own.point_to(&tracked_seeder);
    let tracked = fetch(
        &rt,
        &service,
        &format!("magnet:?xt=urn:btih:{tracked_id}&tr={}", own.announce),
    );
    assert!(
        contains(&tracked, &own.announce),
        "种子自己的 tracker 没存下"
    );
    assert!(own.announced(&tracked_id));
    assert!(
        !public.announced(&tracked_id),
        "带了 tracker 的磁力链接补了公共 tracker"
    );
    drop(tracked_seeder);

    // 私有种子（libtorrent 不向节点要私有种子的种子信息，只能用种子文件），自带 tracker：下载、做种都不补
    let (private_seeder, private_id, torrent) = seed_with(
        &dir.join("private-seed"),
        false,
        &CreateOptions {
            piece_size: PIECE,
            v1_only: true,
            private_torrent: true,
            tracker: own.announce.clone(),
        },
    );
    own.point_to(&private_seeder);
    let resume = hs_dlx_app::torrent_from_file(&torrent).unwrap();
    let info = TorrentInfo::of(&resume).unwrap();
    assert!(info.private);
    let all = info.files.iter().map(|file| file.index).collect();
    let file = dir.join("private.torrent");
    let url = Url::from_file_path(&file).unwrap();
    let task = rt
        .block_on(service.create(draft(url.as_str(), &dir.join("downloads"), resume, all), 2))
        .unwrap()
        .unwrap();
    assert_eq!(
        rt.block_on(until(&mut events, task.id, |_| false)),
        Some(Status::Done)
    );
    rt.block_on(seeding(&mut events, task.id));
    assert_eq!(public_check(task.id).text, "私有种子不补公共 tracker");
    assert!(own.announced(&private_id));
    assert!(
        !public.announced(&private_id),
        "私有种子的信息哈希通告给了公共 tracker"
    );
    drop(service);
    drop(seeder);
    drop(private_seeder);
}

/// 测试的 SOCKS5 代理：在回环地址上，不认证；CONNECT 照转并记下目的地址，UDP ASSOCIATE 照 `udp` 转发（记下发往的
/// 地址）或回「不支持」。释放时停下：不再接受连接，各转发随连接断开结束。
struct Socks {
    port: u16,
    connected: Arc<Mutex<Vec<std::net::SocketAddr>>>,
    relayed: Arc<Mutex<Vec<std::net::SocketAddr>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Socks {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // 叫醒在等连接的那一头，它看到停下的标记即退出
        let _woken = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
    }
}

impl Socks {
    fn start(udp: bool) -> Socks {
        use std::io::{Read, Write};
        use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
        type Seen = Arc<Mutex<Vec<SocketAddr>>>;

        fn read_address(client: &mut TcpStream, kind: u8) -> SocketAddr {
            let ip = match kind {
                1 => {
                    let mut v4 = [0; 4];
                    client.read_exact(&mut v4).unwrap();
                    IpAddr::from(v4)
                }
                4 => {
                    let mut v6 = [0; 16];
                    client.read_exact(&mut v6).unwrap();
                    IpAddr::from(v6)
                }
                other => panic!("测试的代理不解析主机名（地址类型 {other}）"),
            };
            let mut port = [0; 2];
            client.read_exact(&mut port).unwrap();
            SocketAddr::new(ip, u16::from_be_bytes(port))
        }

        /// 转发 UDP：客户端发来的去掉 SOCKS 头转给目的地址，目的地址回的加上头转回客户端；`waker` 发来任何包即停。
        fn relay(socket: UdpSocket, waker: SocketAddr, relayed: Seen) {
            let mut client = None;
            let mut buffer = [0; 65536];
            loop {
                let (len, from) = socket.recv_from(&mut buffer).unwrap();
                if from == waker {
                    return;
                }
                let datagram = &buffer[..len];
                if client.is_none_or(|client| client == from)
                    && datagram.len() > 10
                    && datagram[3] == 1
                {
                    client = Some(from);
                    let ip = IpAddr::from([datagram[4], datagram[5], datagram[6], datagram[7]]);
                    let target =
                        SocketAddr::new(ip, u16::from_be_bytes([datagram[8], datagram[9]]));
                    relayed.lock().unwrap().push(target);
                    let _sent = socket.send_to(&datagram[10..], target);
                } else if let (Some(client), SocketAddr::V4(back)) = (client, from) {
                    let mut wrapped = vec![0, 0, 0, 1];
                    wrapped.extend_from_slice(&back.ip().octets());
                    wrapped.extend_from_slice(&back.port().to_be_bytes());
                    wrapped.extend_from_slice(datagram);
                    let _sent = socket.send_to(&wrapped, client);
                }
            }
        }

        fn serve(mut client: TcpStream, udp: bool, connected: Seen, relayed: Seen) {
            let mut head = [0; 2];
            client.read_exact(&mut head).unwrap();
            let mut methods = vec![0; usize::from(head[1])];
            client.read_exact(&mut methods).unwrap();
            client.write_all(&[5, 0]).unwrap();
            let mut request = [0; 4];
            client.read_exact(&mut request).unwrap();
            let target = read_address(&mut client, request[3]);
            match request[1] {
                1 => {
                    let Ok(upstream) = TcpStream::connect(target) else {
                        let _closed = client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]);
                        return;
                    };
                    connected.lock().unwrap().push(target);
                    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
                    let (mut up_read, mut client_write) =
                        (upstream.try_clone().unwrap(), client.try_clone().unwrap());
                    let (mut up_write, mut client_read) = (upstream, client);
                    // 一头断了就关上另一头，免得对方以为连接还在
                    std::thread::spawn(move || {
                        let _done = std::io::copy(&mut up_read, &mut client_write);
                        let _closed = client_write.shutdown(std::net::Shutdown::Both);
                    });
                    let _done = std::io::copy(&mut client_read, &mut up_write);
                    let _closed = up_write.shutdown(std::net::Shutdown::Both);
                }
                3 if udp => {
                    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                    let waker = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                    let at = socket.local_addr().unwrap();
                    let mut reply = vec![5, 0, 0, 1, 127, 0, 0, 1];
                    reply.extend_from_slice(&at.port().to_be_bytes());
                    client.write_all(&reply).unwrap();
                    let wake_from = waker.local_addr().unwrap();
                    let relaying = std::thread::spawn(move || relay(socket, wake_from, relayed));
                    // 控制连接断开即结束这次转发
                    let _closed = std::io::copy(&mut client, &mut std::io::sink());
                    waker.send_to(b"stop", at).unwrap();
                    relaying.join().unwrap();
                }
                _ => {
                    let _closed = client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]);
                }
            }
        }

        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (connected, relayed): (Seen, Seen) = Default::default();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (seen, sent, stopped) = (connected.clone(), relayed.clone(), stop.clone());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                if stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let Ok(client) = client else { continue };
                let (seen, sent) = (seen.clone(), sent.clone());
                std::thread::spawn(move || serve(client, udp, seen, sent));
            }
        });
        Socks {
            port,
            connected,
            relayed,
            stop,
        }
    }

    fn url(&self) -> Url {
        Url::parse(&format!("socks5://127.0.0.1:{}", self.port)).unwrap()
    }
}

/// 取 `uri` 的种子信息，等到报出含 `wanted` 的 BT 代理的问题（最多 20 秒；之前报的别的问题，如 libtorrent 自己连代理
/// 出的错，跳过）；取到种子信息或超时还没报时为最后报的问题（没报过为 None）。
fn proxy_problem(
    rt: &Runtime,
    service: &Service,
    uri: &str,
    wanted: &str,
) -> Result<String, Option<String>> {
    let fetch = service.fetch_metadata(uri.to_owned()).unwrap();
    let mut progress = fetch.progress;
    let _resume = fetch.resume;
    let mut last = None;
    rt.block_on(async {
        let found = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(problem) = progress.borrow_and_update().proxy_problem.clone() {
                    if problem.contains(wanted) {
                        return Some(problem);
                    }
                    last = Some(problem);
                }
                if progress.changed().await.is_err() {
                    return None;
                }
            }
        })
        .await;
        found.ok().flatten().ok_or(last)
    })
}

/// BT 经 SOCKS5 代理：磁力链接连做种的一方经代理转发（TCP 与 uTP 都经它），取到种子信息；HTTP 代理不收。代理连不上、
/// 不转发 UDP 时，取种子信息的进度里报出原因。
#[test]
fn torrents_connect_through_the_socks_proxy() {
    let dir = test_dir("socks-proxy");
    let (seeder, id) = seed(&dir.join("seed"), false, false);
    let seeder_at = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, seeder.listen_port()));
    let rt = runtime();
    let (service, _events) = service(&dir.join("tasks.sqlite"));
    assert!(matches!(
        service.set_torrent_proxy(Some(Url::parse("http://127.0.0.1:1").unwrap())),
        Err(Error::InvalidInput(_))
    ));

    let socks = Socks::start(true);
    service.set_torrent_proxy(Some(socks.url())).unwrap();
    assert_eq!(service.settings().unwrap().torrent_proxy, Some(socks.url()));
    let resume = fetch(&rt, &service, &magnet(&id, &seeder));
    assert_eq!(TorrentInfo::of(&resume).unwrap().info_hash, id.as_str());
    let through =
        |seen: &Mutex<Vec<std::net::SocketAddr>>| seen.lock().unwrap().contains(&seeder_at);
    assert!(
        through(&socks.connected) || through(&socks.relayed),
        "没经代理连做种的一方"
    );
    // 会话刚建起时 UDP 转发还没谈好，第一次连节点的 uTP 发不出去、改走 TCP；再取一次时 uTP 经代理的 UDP 转发
    fetch(&rt, &service, &magnet(&id, &seeder));
    assert!(
        through(&socks.relayed),
        "uTP 没经代理转发：{:?}",
        socks.relayed.lock().unwrap()
    );

    // 不转发 UDP 的代理
    let tcp_only = Socks::start(false);
    service.set_torrent_proxy(Some(tcp_only.url())).unwrap();
    let problem = proxy_problem(&rt, &service, &magnet(&id, &seeder), "不转发 UDP");
    assert!(problem.is_ok(), "没报出代理不转发 UDP：{problem:?}");

    // 连不上的代理：端口上没人
    let closed = tcp_only.url();
    drop(tcp_only);
    service.set_torrent_proxy(Some(closed)).unwrap();
    let problem = proxy_problem(&rt, &service, &magnet(&id, &seeder), "连不上代理");
    assert!(problem.is_ok(), "没报出代理连不上：{problem:?}");
    drop(service);
    drop(seeder);
}
