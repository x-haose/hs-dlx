//! 本机两个会话互传：一个做种，另一个用磁力链接（只有信息哈希）连上它取种子信息，或直接用种子文件，再照选择加入、
//! 下完内容。不用 DHT、tracker 与端口映射，不出本机。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hs_dlx_torrent::{
    AddOptions, AlertKind, CreateOptions, Removal, Session, SessionConfig, State, TorrentId,
    create_torrent, describe, place, resume_from_torrent_file,
};

/// 照续传数据里记的已有块，加入后就运行。
const TRUSTED: AddOptions = AddOptions {
    trust_pieces: true,
    paused: false,
    upload_only: false,
};
/// 不信续传数据里的已有块（磁盘上有文件就整体校验），加入后就运行。
const CHECKED: AddOptions = AddOptions {
    trust_pieces: false,
    paused: false,
    upload_only: false,
};

/// 测试用的空目录，名字带进程号。
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hs-dlx-torrent-{name}-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 只在回环地址上监听（端口由系统挑）、不找外面的节点的会话。
fn local_session() -> Session {
    session_on("127.0.0.1:0")
}

/// 只在 `listen` 上监听、不找外面节点的会话。
fn session_on(listen: &str) -> Session {
    Session::new(&SessionConfig {
        listen_interfaces: listen.into(),
        dht: false,
        dht_bootstrap_nodes: String::new(),
        upnp: false,
        natpmp: false,
        lsd: false,
        user_agent: "hs-dlx-test".into(),
        peer_fingerprint: "-HD0000-".into(),
        upload_limit: 0,
        proxy: Default::default(),
        state: Vec::new(),
    })
    .unwrap()
}

/// `len` 字节的内容，按 `seed` 变化：不同的文件内容不同。
fn content(len: u32, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_add(seed).wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

/// 做种：`path`（`dir` 里的文件或目录，分块 `piece_size` 字节）建种子、在新会话里校验已有的文件后做种，等它可以连；
/// 返回会话、种子与种子文件转成的续传数据。
fn seed(dir: &Path, path: &Path, piece_size: i32) -> (Session, TorrentId, Vec<u8>) {
    let options = CreateOptions {
        piece_size,
        v1_only: false,
        private_torrent: false,
        tracker: String::new(),
    };
    let torrent = create_torrent(path_str(path), &options).unwrap();
    let resume = resume_from_torrent_file(&torrent).unwrap();
    let id = describe(&resume).unwrap().id;
    let seeder = local_session();
    seeder
        .add_resume(&resume, &id, path_str(dir), CHECKED)
        .unwrap();
    wait_until(&seeder, Duration::from_secs(20), |s| {
        s.status(&id).unwrap().state() == Some(State::Seeding) && s.listen_port() != 0
    });
    (seeder, id, resume)
}

/// 下完选了的文件（要下的字节都下完并校验过）。
fn done(session: &Session, id: &TorrentId) -> bool {
    let status = session.status(id).unwrap();
    status.has_metadata && status.pieces > 0 && status.total_wanted_done == status.total_wanted
}

/// 多文件的种子改了根名、只下选了的文件；从续传数据恢复的会话不重下、也不连节点就是完成的，不信续传数据时校验磁盘上
/// 的文件得出同样的结果；加入时可以暂停；移除完之前不能再加。
#[test]
fn placed_files_download_and_resume_without_peers() {
    let seed_dir = scratch("multi-seed");
    let fetch_dir = scratch("multi-fetch");
    let album = seed_dir.join("album");
    std::fs::create_dir_all(album.join("extra")).unwrap();
    let files = [
        ("album/a.bin", content(100_000, 1)),
        ("album/b.bin", content(70_001, 2)),
        ("album/extra/c.bin", content(250_000, 3)),
    ];
    for (path, data) in &files {
        std::fs::write(seed_dir.join(path), data).unwrap();
    }
    let (seeder, id, resume) = seed(&seed_dir, &album, 16 << 10);

    let metadata = describe(&resume).unwrap();
    assert_eq!((metadata.name.as_str(), metadata.private), ("album", false));
    assert!(metadata.files.iter().all(|f| f.wanted));
    let mut paths: Vec<(String, i64)> = metadata
        .files
        .iter()
        .map(|f| (f.path.replace('\\', "/"), f.size))
        .collect();
    paths.sort();
    let expected: Vec<(String, i64)> = files
        .iter()
        .map(|(path, data)| ((*path).to_owned(), i64::try_from(data.len()).unwrap()))
        .collect();
    assert_eq!(paths, expected);
    let wanted: Vec<i32> = metadata
        .files
        .iter()
        .filter(|f| !f.path.ends_with("b.bin"))
        .map(|f| f.index)
        .collect();
    assert!(place(&resume, "a/b", &wanted).is_err());
    assert!(place(&resume, "..", &wanted).is_err());
    assert!(place(&resume, "专辑", &[99]).is_err());
    let placed = place(&resume, "专辑", &wanted).unwrap();
    let renamed: Vec<String> = describe(&placed)
        .unwrap()
        .files
        .iter()
        .map(|f| f.path.replace('\\', "/"))
        .collect();
    assert!(
        renamed.iter().all(|path| path.starts_with("专辑/")),
        "{renamed:?}"
    );

    let fetcher = local_session();
    fetcher
        .add_resume(&placed, &id, path_str(&fetch_dir), TRUSTED)
        .unwrap();
    // 已在会话里的不能再加：报错，另有一条加入失败的事件
    assert!(
        fetcher
            .add_resume(&placed, &id, path_str(&fetch_dir), TRUSTED)
            .is_err()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fetcher
        .pop_alerts(100)
        .unwrap()
        .iter()
        .any(|a| a.kind == AlertKind::AddFailed)
    {
        assert!(Instant::now() < deadline, "没等到加入失败的事件");
    }
    fetcher
        .connect_peer(&id, "127.0.0.1", seeder.listen_port())
        .unwrap();
    wait_until(&fetcher, Duration::from_secs(60), |s| done(s, &id));
    assert_eq!(fetcher.status(&id).unwrap().total_wanted, 100_000 + 250_000);
    // 写到磁盘后看文件；再写出续传数据，换一个不连任何节点的会话恢复
    flushed(&fetcher, &id);
    let saved = resume_data(&fetcher, &id);
    // 续传数据里：没选的文件不要，已有的块落在要下的文件里的字节即两个文件的全部
    let described = describe(&saved).unwrap();
    let unwanted: Vec<&str> = described
        .files
        .iter()
        .filter(|f| !f.wanted)
        .map(|f| f.path.as_str())
        .collect();
    assert!(
        unwanted.len() == 1 && unwanted[0].ends_with("b.bin"),
        "{unwanted:?}"
    );
    assert_eq!(described.done, 100_000 + 250_000);
    assert_eq!(describe(&placed).unwrap().done, 0);
    assert_file(&fetch_dir, "专辑/a.bin", &files[0].1);
    assert_file(&fetch_dir, "专辑/extra/c.bin", &files[2].1);
    assert!(!fetch_dir.join("专辑/b.bin").exists());
    assert!(!fetch_dir.join("album").exists());
    drop(fetcher);
    drop(seeder);
    let restored = local_session();
    let other = TorrentId::parse(&"0".repeat(40)).unwrap();
    assert!(
        restored
            .add_resume(&saved, &other, path_str(&fetch_dir), TRUSTED)
            .is_err()
    );
    restored
        .add_resume(&saved, &id, path_str(&fetch_dir), TRUSTED)
        .unwrap();
    wait_until(&restored, Duration::from_secs(20), |s| done(s, &id));
    assert_eq!(restored.status(&id).unwrap().peers, 0);
    // 移除完（移除的事件）之前不许再加同一个种子，之后可以
    restored.remove(&id, Removal::Keep).unwrap();
    assert!(restored.status(&id).is_err());
    assert!(restored.removing(&id));
    assert!(
        restored
            .add_resume(&saved, &id, path_str(&fetch_dir), TRUSTED)
            .is_err()
    );
    removed(&restored, &id, false);
    assert!(!restored.removing(&id));
    // 不信续传数据：校验磁盘上的文件，改坏了的那块不算已有
    let a = fetch_dir.join("专辑").join("a.bin");
    let mut corrupted = std::fs::read(&a).unwrap();
    corrupted[0] ^= 0xff;
    std::fs::write(&a, &corrupted).unwrap();
    restored
        .add_resume(&saved, &id, path_str(&fetch_dir), CHECKED)
        .unwrap();
    wait_until(&restored, Duration::from_secs(20), |s| {
        s.status(&id).unwrap().state() == Some(State::Downloading)
    });
    assert_eq!(
        restored.status(&id).unwrap().total_wanted_done,
        100_000 + 250_000 - (16 << 10)
    );
    restored.remove(&id, Removal::Keep).unwrap();
    removed(&restored, &id, false);
    // 做种时只上传
    restored
        .add_resume(
            &saved,
            &id,
            path_str(&fetch_dir),
            AddOptions {
                upload_only: true,
                ..TRUSTED
            },
        )
        .unwrap();
    assert!(restored.status(&id).unwrap().held);
    restored.remove(&id, Removal::Keep).unwrap();
    removed(&restored, &id, false);
    // 加入时暂停
    restored
        .add_resume(
            &saved,
            &id,
            path_str(&fetch_dir),
            AddOptions {
                trust_pieces: true,
                paused: true,
                upload_only: false,
            },
        )
        .unwrap();
    assert!(restored.status(&id).unwrap().paused);
    assert!(!restored.status(&id).unwrap().held);
    // 连文件一起删：libtorrent 先发删的结果、后发移除的事件，两个都到了才算移除完
    restored.remove(&id, Removal::Files).unwrap();
    removed(&restored, &id, true);
    assert!(!fetch_dir.join("专辑").exists(), "文件与目录都删掉了");
    restored
        .add_resume(&saved, &id, path_str(&fetch_dir), TRUSTED)
        .unwrap();
    assert!(
        local_session()
            .add_resume(b"not bencode", &id, path_str(&fetch_dir), TRUSTED)
            .is_err()
    );
    assert!(describe(b"not bencode").is_err());
    assert!(resume_from_torrent_file(b"not bencode").is_err());

    drop(restored);
    std::fs::remove_dir_all(&seed_dir).unwrap();
    std::fs::remove_dir_all(&fetch_dir).unwrap();
}

/// 等种子 `id` 移除完（`deleted` 时还要删文件的结果），其间加入它都报错；10 秒内没等到即失败。
fn removed(session: &Session, id: &TorrentId, deleted: bool) {
    let (mut removed, mut settled) = (false, !deleted);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(removed && settled) {
        assert!(Instant::now() < deadline, "没等到移除完");
        assert!(session.removing(id), "移除完之前就不算移除中了");
        for alert in session.pop_alerts(100).unwrap() {
            if alert.torrent != id.as_str() {
                continue;
            }
            match alert.kind {
                AlertKind::Removed => removed = true,
                AlertKind::Deleted | AlertKind::DeleteFailed => settled = true,
                _ => {}
            }
        }
    }
    assert!(!session.removing(id));
}

/// DHT 的状态写得出、下次建会话时载得进。
#[test]
fn session_state_round_trips() {
    let state = local_session().session_state().unwrap();
    assert!(!state.is_empty());
    let config = |state: Vec<u8>| SessionConfig {
        listen_interfaces: "127.0.0.1:0".into(),
        dht: false,
        dht_bootstrap_nodes: String::new(),
        upnp: false,
        natpmp: false,
        lsd: false,
        user_agent: "hs-dlx-test".into(),
        peer_fingerprint: "-HD0000-".into(),
        upload_limit: 0,
        proxy: Default::default(),
        state,
    };
    Session::new(&config(state)).unwrap();
    assert!(Session::new(&config(b"not bencode".to_vec())).is_err());
}

/// 等到 `done` 成立，期间不断取走事件（不取时 libtorrent 的事件队列会满）；超过 `limit` 即失败。
fn wait_until(session: &Session, limit: Duration, mut done: impl FnMut(&Session) -> bool) {
    let deadline = Instant::now() + limit;
    while !done(session) {
        assert!(Instant::now() < deadline, "超过 {limit:?} 没等到");
        for alert in session.pop_alerts(100).unwrap() {
            assert!(
                !matches!(
                    alert.kind,
                    AlertKind::AddFailed | AlertKind::TorrentError | AlertKind::FileError
                ),
                "{alert:?}"
            );
        }
    }
}

/// 把 `id` 已下的数据写到磁盘并等它报来（libtorrent 先校验、再异步写盘，下完并校验过时数据可能还在内存里）。
fn flushed(session: &Session, id: &TorrentId) {
    session.flush(id).unwrap();
    wait_for(session, id, AlertKind::CacheFlushed, "写到磁盘");
}

/// 写出 `id` 的续传数据并等它报来，返回续传数据。
fn resume_data(session: &Session, id: &TorrentId) -> Vec<u8> {
    session.save_resume_data(id).unwrap();
    wait_for(session, id, AlertKind::ResumeData, "续传数据").data
}

/// 等到种子 `id` 的一条 `kind` 的事件（期间的别的事件丢掉）；20 秒内没等到即失败。
fn wait_for(
    session: &Session,
    id: &TorrentId,
    kind: AlertKind,
    what: &str,
) -> hs_dlx_torrent::Alert {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "没等到{what}");
        if let Some(alert) = session
            .pop_alerts(100)
            .unwrap()
            .into_iter()
            .find(|a| a.kind == kind && a.torrent == id.as_str())
        {
            return alert;
        }
    }
}

/// `dir` 下的 `relative`（`/` 分隔）是 `expected`：不在时列出 `dir` 下实际有的，不同时报两边的长度与第一个不同的位置
/// （整个内容打出来太长，CI 的日志里看不到）。
fn assert_file(dir: &Path, relative: &str, expected: &[u8]) {
    let path = relative
        .split('/')
        .fold(dir.to_path_buf(), |path, part| path.join(part));
    let actual = std::fs::read(&path).unwrap_or_else(|error| {
        panic!(
            "读不出 {}（{error}）；{} 下有：{:?}",
            path.display(),
            dir.display(),
            tree(dir)
        )
    });
    if actual != expected {
        let first = actual.iter().zip(expected).position(|(a, b)| a != b);
        panic!(
            "{relative} 的内容不对：{} 字节，应为 {} 字节；第一个不同在 {first:?}",
            actual.len(),
            expected.len()
        );
    }
}

/// `dir` 下各文件的相对路径与字节数。
fn tree(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(at) = pending.pop() {
        for entry in std::fs::read_dir(&at).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::metadata(&path).unwrap();
            if meta.is_dir() {
                pending.push(path);
            } else {
                found.push((path.strip_prefix(dir).unwrap().to_path_buf(), meta.len()));
            }
        }
    }
    found.sort();
    found
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("临时目录的路径是 UTF-8")
}

/// 磁力链接先只取种子信息（不下内容）；要到续传数据、移除之后，照选择加入再下完。
#[test]
fn a_magnet_link_downloads_from_a_local_seed() {
    let seed_dir = scratch("seed");
    let fetch_dir = scratch("fetch");
    // 3 MiB 加零头的内容，分成 64 KiB 的块：最后一块不满
    let content: Vec<u8> = (0..(3 << 20) + 12_345)
        .map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let file = seed_dir.join("sample.bin");
    std::fs::write(&file, &content).unwrap();
    let (seeder, seeding, _) = seed(&seed_dir, &file, 64 << 10);

    // 名字不是 UTF-8（%FF）：照样加得进，名字里换成替换字符
    let fetcher = local_session();
    let magnet = format!("magnet:?xt=urn:btih:{seeding}&dn=a%FFb");
    let fetching: TorrentId = fetcher.add_magnet(&magnet, path_str(&fetch_dir)).unwrap();
    assert_eq!(fetching, seeding);
    assert_eq!(fetcher.status(&fetching).unwrap().name, "a\u{fffd}b");
    fetcher
        .connect_peer(&fetching, "127.0.0.1", seeder.listen_port())
        .unwrap();
    // 取到种子信息、校验完：只上传，一块也不下
    wait_for(&fetcher, &fetching, AlertKind::Metadata, "种子信息");
    wait_until(&fetcher, Duration::from_secs(60), |s| {
        s.status(&fetching).unwrap().state() == Some(State::Downloading)
    });
    let held = fetcher.status(&fetching).unwrap();
    assert!(held.held);
    assert_eq!(held.total_wanted_done, 0);
    // 没下完的种子 libtorrent 不会自己发写完的事件：等得到即说明 flush 真的写了、事件转了出来
    flushed(&fetcher, &fetching);
    let probed = resume_data(&fetcher, &fetching);
    fetcher.remove(&fetching, Removal::Keep).unwrap();
    removed(&fetcher, &fetching, false);
    assert!(
        !fetch_dir.join("sample.bin").exists(),
        "取种子信息时不写文件"
    );

    let metadata = describe(&probed).unwrap();
    assert_eq!(metadata.name, "sample.bin");
    let placed = place(&probed, "样本 (2).bin", &[metadata.files[0].index]).unwrap();
    fetcher
        .add_resume(&placed, &fetching, path_str(&fetch_dir), TRUSTED)
        .unwrap();
    assert!(!fetcher.status(&fetching).unwrap().held);
    fetcher
        .connect_peer(&fetching, "127.0.0.1", seeder.listen_port())
        .unwrap();
    wait_until(&fetcher, Duration::from_secs(60), |s| done(s, &fetching));

    flushed(&fetcher, &fetching);
    let status = fetcher.status(&fetching).unwrap();
    assert!(status.finished);
    assert_eq!(status.total_wanted, i64::try_from(content.len()).unwrap());
    // 网络状况：在回环地址上监听着，没开 DHT（统计过后为 0 个节点），没映射端口；种子里没有 tracker
    let network = fetcher.network();
    assert_ne!(network.listen_port, 0);
    assert_eq!(network.dht_nodes, -1, "还没统计过");
    fetcher.request_stats();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fetcher
        .pop_alerts(100)
        .unwrap()
        .iter()
        .any(|a| a.kind == AlertKind::Stats)
    {
        assert!(Instant::now() < deadline, "没等到统计");
    }
    assert_eq!(fetcher.network().dht_nodes, 0);
    assert_eq!((network.upnp_port, network.natpmp_port), (0, 0));
    assert!(fetcher.trackers(&fetching).unwrap().is_empty());
    assert_eq!(status.swarm_seeds, -1);
    // 做种的一方累计上传了全部内容（libtorrent 每秒统计一次）
    let sent = i64::try_from(content.len()).unwrap();
    wait_until(&seeder, Duration::from_secs(10), |s| {
        s.status(&seeding).unwrap().all_time_upload >= sent
    });
    assert_eq!(status.reachable_pieces, status.pieces);
    assert_file(&fetch_dir, "样本 (2).bin", &content);

    drop(fetcher);
    drop(seeder);
    std::fs::remove_dir_all(&seed_dir).unwrap();
    std::fs::remove_dir_all(&fetch_dir).unwrap();
}

/// 补的 tracker：已有的（种子自己的、补过的）不重复补，查 tracker 时标为补的，报的错另成一类；写出的续传数据里只去掉
/// 补的，种子自己的留着；再加入时清空，要再补。
#[test]
fn added_trackers_stay_out_of_resume_data() {
    let dir = scratch("added-trackers");
    let session = local_session();
    // 本机这两个端口没人监听：通告随即被拒
    let own = "http://127.0.0.1:9/announce";
    let public = "http://127.0.0.1:7/announce";
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let uri = format!("magnet:?xt=urn:btih:{hash}&tr={own}");
    let id = session.add_magnet(&uri, path_str(&dir)).unwrap();
    let urls = [own.to_owned(), public.to_owned()];
    assert_eq!(session.add_trackers(&id, &urls).unwrap(), 1);
    assert_eq!(session.add_trackers(&id, &urls).unwrap(), 0);
    let listed: Vec<(String, bool)> = session
        .trackers(&id)
        .unwrap()
        .into_iter()
        .map(|tracker| (tracker.url, tracker.added))
        .collect();
    assert_eq!(listed, [(own.to_owned(), false), (public.to_owned(), true)]);
    // 两个都报错：种子自己的与补的各是一类，说明里是各自的地址
    let mut errors = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while errors.len() < 2 {
        assert!(
            Instant::now() < deadline,
            "没等到两个 tracker 都报错：{errors:?}"
        );
        for alert in session.pop_alerts(100).unwrap() {
            let url = if alert.message.contains(own) {
                own
            } else {
                public
            };
            let pair = match alert.kind {
                AlertKind::TrackerError => (false, url),
                AlertKind::AddedTrackerError => (true, url),
                _ => continue,
            };
            if !errors.contains(&pair) {
                errors.push(pair);
            }
        }
    }
    errors.sort();
    assert_eq!(errors, [(false, own), (true, public)]);
    let contains = |data: &[u8], text: &str| data.windows(text.len()).any(|w| w == text.as_bytes());
    let resume = resume_data(&session, &id);
    assert!(contains(&resume, own) && !contains(&resume, public));

    session.remove(&id, Removal::Keep).unwrap();
    wait_until(&session, Duration::from_secs(10), |s| !s.removing(&id));
    let again = session.add_magnet(&uri, path_str(&dir)).unwrap();
    assert_eq!(session.add_trackers(&again, &urls).unwrap(), 1);
}

/// 每个监听地址都路由不到的 tracker（只在 IPv4 回环上监听时的 IPv6 地址）报出错，说明是中文的；被拒的照常报
/// libtorrent 的原因。
#[test]
fn a_tracker_no_listen_address_can_route_to_is_reported_as_unroutable() {
    let dir = scratch("unroutable-tracker");
    let session = local_session();
    let unroutable = "http://[2001:db8::1]/announce";
    // 本机这个端口没人监听：通告随即被拒
    let refused = "http://127.0.0.1:9/announce";
    let hash = "1123456789abcdef0123456789abcdef01234567";
    let uri = format!(
        "magnet:?xt=urn:btih:{hash}&tr=http%3A%2F%2F%5B2001%3Adb8%3A%3A1%5D%2Fannounce&tr={refused}"
    );
    let id = session.add_magnet(&uri, path_str(&dir)).unwrap();
    let errors = tracker_errors_until_failed(&session, &id);
    let about = |url: &str| -> Vec<&String> { errors.iter().filter(|m| m.contains(url)).collect() };
    assert!(!about(unroutable).is_empty(), "{errors:?}");
    assert!(
        about(unroutable)
            .iter()
            .all(|m| m.contains("本机的网络地址都连不到它")),
        "{errors:?}"
    );
    assert!(
        about(refused)
            .iter()
            .all(|m| !m.contains("本机的网络地址都连不到它") && !m.contains("skipping")),
        "{errors:?}"
    );
    let message = tracker_message(&session, &id, unroutable);
    assert!(message.contains("本机的网络地址都连不到它"), "{message}");

    drop(session);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 跳过的事件还在队列里时种子移除完了：照样取得出事件，不因查不到这个种子的 tracker 出错。
#[test]
fn a_skipped_announce_of_a_removed_torrent_still_pops() {
    let dir = scratch("skipped-then-removed");
    let session = local_session();
    let hash = "3123456789abcdef0123456789abcdef01234567";
    let uri =
        format!("magnet:?xt=urn:btih:{hash}&tr=http%3A%2F%2F%5B2001%3Adb8%3A%3A1%5D%2Fannounce");
    let id = session.add_magnet(&uri, path_str(&dir)).unwrap();
    // 先不取事件：跳过的事件留在队列里
    let deadline = Instant::now() + Duration::from_secs(20);
    while !session.trackers(&id).unwrap().iter().all(|t| t.failed) {
        assert!(Instant::now() < deadline, "没等到 tracker 算连不上");
    }
    session.remove(&id, Removal::Keep).unwrap();
    // 没有种子信息的种子没有存储：移除在会话的线程上做完时随即发出移除的事件。之后的同步调用（取监听端口）在会话的线程
    // 上排在移除之后，返回时移除已做完、事件已在队列里
    session.listen_port();
    let kinds: Vec<AlertKind> = session
        .pop_alerts(100)
        .unwrap()
        .into_iter()
        .map(|a| a.kind)
        .collect();
    assert!(kinds.contains(&AlertKind::Removed), "{kinds:?}");

    drop(session);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 只有部分监听地址路由不到的 tracker（在 IPv6 与 IPv4 回环上监听时的 IPv4 地址，被跳过的 IPv6
/// 地址排在前面）：跳过的那个地址不报错，查 tracker 时说明是能通告的那个地址的真正原因。
#[test]
fn a_tracker_skipped_on_some_listen_addresses_reports_the_real_failure() {
    let dir = scratch("partly-skipped-tracker");
    let session = session_on("[::1]:0,127.0.0.1:0");
    let refused = "http://127.0.0.1:9/announce";
    let hash = "2123456789abcdef0123456789abcdef01234567";
    let uri = format!("magnet:?xt=urn:btih:{hash}&tr={refused}");
    let id = session.add_magnet(&uri, path_str(&dir)).unwrap();
    let errors = tracker_errors_until_failed(&session, &id);
    assert!(!errors.is_empty());
    assert!(
        errors
            .iter()
            .all(|m| !m.contains("skipping") && !m.contains("本机的网络地址都连不到它")),
        "{errors:?}"
    );
    let message = tracker_message(&session, &id, refused);
    assert!(
        !message.is_empty() && !message.contains("skipping"),
        "{message}"
    );

    drop(session);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// tracker 出错的说明为「tracker 的地址：原因」：回了 failure reason 的取它，连不上的取错误说明；不带种子、监听地址与
/// 连续失败的次数。
#[test]
fn tracker_errors_read_as_the_url_and_the_reason() {
    let dir = scratch("tracker-error-text");
    let session = local_session();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let failing = format!("http://{address}/announce");
    let refused = "http://127.0.0.1:9/announce";
    let stop = AtomicBool::new(false);
    let errors = std::thread::scope(|scope| {
        scope.spawn(|| refuse_announces(&listener, &stop));
        let hash = "4123456789abcdef0123456789abcdef01234567";
        let uri = format!("magnet:?xt=urn:btih:{hash}&tr={failing}&tr={refused}");
        let id = session.add_magnet(&uri, path_str(&dir)).unwrap();
        let errors = tracker_errors_until_failed(&session, &id);
        stop.store(true, Ordering::Relaxed);
        TcpStream::connect(address).unwrap();
        errors
    });
    assert!(
        errors.contains(&format!("{failing}：not allowed")),
        "{errors:?}"
    );
    let refusals: Vec<&str> = errors
        .iter()
        .filter_map(|m| m.strip_prefix(&format!("{refused}：")))
        .collect();
    assert!(!refusals.is_empty(), "{errors:?}");
    assert!(
        refusals
            .iter()
            .all(|reason| !reason.is_empty() && !reason.contains(['"', '['])),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .all(|m| m.starts_with(&format!("{failing}："))
                || m.starts_with(&format!("{refused}："))),
        "{errors:?}"
    );

    drop(session);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 在 `listener` 上当 HTTP tracker：每次通告都回 failure reason「not allowed」；`stop` 置位后收到下一个连接即返回。
fn refuse_announces(listener: &TcpListener, stop: &AtomicBool) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let mut stream = stream.unwrap();
        let mut head = Vec::new();
        let mut buf = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let read = stream.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            head.extend_from_slice(&buf[..read]);
        }
        let body = b"d14:failure reason11:not allowede";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    }
}

/// 等到 `id` 的每个 tracker 都算连不上，返回期间报的 tracker 出错的说明。状态先于事件更新：都算连不上之后再取一次，
/// 那时报的事件必已在队列里。
fn tracker_errors_until_failed(session: &Session, id: &TorrentId) -> Vec<String> {
    let mut errors = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let failed = session.trackers(id).unwrap().iter().all(|t| t.failed);
        errors.extend(
            session
                .pop_alerts(100)
                .unwrap()
                .into_iter()
                .filter(|alert| alert.kind == AlertKind::TrackerError)
                .map(|alert| alert.message),
        );
        if failed {
            return errors;
        }
        assert!(Instant::now() < deadline, "没等到每个 tracker 都算连不上");
    }
}

/// 查 tracker 时 `url` 的说明。
fn tracker_message(session: &Session, id: &TorrentId, url: &str) -> String {
    session
        .trackers(id)
        .unwrap()
        .into_iter()
        .find(|t| t.url == url)
        .unwrap()
        .message
}
