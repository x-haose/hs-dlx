//! 发版前的验收：对真实网站、PyPI、BT 网络与 PikPak 跑主要流程。要联网、结果随网站与网络变，默认不跑（`#[ignore]`），
//! 由 `make acceptance` 逐个串行跑。
//!
//! - 网页视频的组件照真实安装：随应用带的 Python 与 QuickJS-NG 取 `make runtime` 备好的（third_party/runtime/dist），
//!   yt-dlp 从 PyPI 装。
//! - 代理照系统设置（[`Proxy::System`]），直连的几项另设；`HS_DLX_ACCEPT_PROXY_PORT` 为本机代理的端口，测只填端口的
//!   代理设置。
//! - 下载的东西留在 target 的临时目录 `acceptance/<测试名>/out` 下供核对，下一次跑同一项时清掉。下完的视频用本机的
//!   ffprobe 读时长与流。
//! - PikPak 要真实账号：测试打出确认页的地址与登录码，在浏览器里确认后接着跑（最多等 10 分钟）。

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hs_dlx_app::web::{Choice, Preference, VideoCodec};
use hs_dlx_app::ytdlp::{Bundle, Bundled, Channel, Sources};
use hs_dlx_app::{
    CloudProgress, CloudProvider, Draft, DraftKind, Event, Events, OutputKind, ProxyProtocol,
    ProxyWanted, Resolved, Service, Status, Store, TaskId, TorrentInfo, TorrentNetwork, WebConfig,
    WebProgress, WebState,
};
use hs_m3u8_core::hls::{AudioChoice, Preference as TrackPreference};
use hs_m3u8_core::{Proxy, Source, Url};
use tokio::runtime::Runtime;

/// Blender 基金会的 Big Buck Bunny（CC BY 3.0），约 10 分钟。
const YOUTUBE: &str = "https://www.youtube.com/watch?v=aqz-KE-bpKQ";
/// B 站上存在多年的公开视频。
const BILIBILI: &str = "https://www.bilibili.com/video/BV1GJ411x7h7";
/// 在中国大陆直连被掐断的网站。
const BLOCKED: &str = "https://www.xvideos.com/video.otdpbiv8d1c/_";
/// Mux 的公开 HLS 测试流（Big Buck Bunny，多档清晰度）。
const HLS: &str = "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8";
/// Debian 13.7.0 的网络安装镜像（约 790 MB）：普通 BT 网络里常年有大量做种，带 Debian 的 tracker。换了版本后旧的
/// 做种会变少，取不到种子信息时换成 cdimage.debian.org 上当前的。
const DEBIAN: &str = "magnet:?xt=urn:btih:7acf8fb590b2060dd9c3146ef770169d593433b0&dn=debian-13.7.0-amd64-netinst.iso&tr=http%3A%2F%2Fbttracker.debian.org%3A6969%2Fannounce";
/// WebTorrent 的 Sintel 示例种子（Blender 基金会，CC BY 3.0）：网盘多已存有，离线即完成。
const SINTEL: &str = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337&tr=wss%3A%2F%2Ftracker.openwebtorrent.com&ws=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2F&xs=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2Fsintel.torrent";
/// WebTorrent 的 Big Buck Bunny 示例种子。
const BIG_BUCK_BUNNY: &str = "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337&tr=wss%3A%2F%2Ftracker.openwebtorrent.com&ws=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2F&xs=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2Fbig-buck-bunny.torrent";

/// 装组件、解析、下载各自最多等多久。
const INSTALL: Duration = Duration::from_secs(10 * 60);
const RESOLVE: Duration = Duration::from_secs(3 * 60);
const DOWNLOAD: Duration = Duration::from_secs(30 * 60);
const LOGIN: Duration = Duration::from_secs(10 * 60);

/// 一项验收：服务、它的事件与等待用的运行时。服务自带运行时，在 `runtime` 外面建、外面丢弃。
struct Rig {
    dir: PathBuf,
    service: Service,
    events: Events,
    runtime: Runtime,
}

impl Rig {
    /// 名为 `name` 的一项：清掉上次留下的，建服务，代理照系统设置。
    fn new(name: &str) -> Rig {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("acceptance")
            .join(name);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let (service, events) = Service::new(
            Store::open(&dir.join("tasks.sqlite")).unwrap(),
            NonZeroUsize::new(32).unwrap(),
            TorrentNetwork::Internet,
            hs_dlx_app::pikpak::Endpoints::official(),
            WebConfig {
                root: dir.join("components"),
                bundle: Some(bundle()),
                sources: Sources::official(),
            },
        )
        .unwrap();
        service.set_proxy(&Proxy::System).unwrap();
        Rig {
            dir,
            service,
            events,
            runtime: Runtime::new().unwrap(),
        }
    }

    /// 在运行时里等 `future`，超过 `limit` 时以 `what` 失败。
    fn within<F: Future>(&self, limit: Duration, what: &str, future: F) -> F::Output {
        self.runtime.block_on(async {
            tokio::time::timeout(limit, future)
                .await
                .unwrap_or_else(|_| panic!("{what}超时"))
        })
    }

    fn out(&self) -> PathBuf {
        self.dir.join("out")
    }

    /// 装网页视频的组件，返回装上的 yt-dlp 版本。
    fn install(&self) -> String {
        self.web_changed(|service| service.install_web().unwrap())
    }

    /// 做 `change`（装、更新、换回组件）后等它有了结果：组件装好、换好（[`WebState::Ready`]）时返回版本，装不成或换
    /// 不成时失败。
    fn web_changed(&self, change: impl FnOnce(&Service)) -> String {
        let mut state = self.service.web_state();
        state.borrow_and_update();
        change(&self.service);
        self.within(INSTALL, "装组件", async {
            loop {
                state.changed().await.unwrap();
                match state.borrow_and_update().clone() {
                    WebState::Ready(version) => return version,
                    WebState::Installing(_) => {}
                    other => panic!("组件没装好：{other:?}"),
                }
            }
        })
    }

    fn resolve(&self, url: &str) -> Resolved {
        let handle = self.service.resolve(Url::parse(url).unwrap(), &[]).unwrap();
        self.within(RESOLVE, "解析", handle)
            .unwrap()
            .unwrap_or_else(|error| panic!("解析 {url} 失败：{error}"))
    }

    fn create(&self, draft: Draft) -> TaskId {
        let handle = self.service.create(draft, 1);
        self.runtime.block_on(handle).unwrap().unwrap().id
    }

    /// 等任务 `id` 停下，返回状态；`watch` 看着途中的每个事件，返回 true 时暂停任务。
    fn ended(&mut self, id: TaskId, mut watch: impl FnMut(&Event) -> bool) -> Status {
        let (service, events) = (&self.service, &mut self.events);
        self.runtime.block_on(async {
            tokio::time::timeout(DOWNLOAD, async {
                loop {
                    let event = events.recv().await.expect("服务还在，事件通道不会关闭");
                    if let Event::Ended {
                        id: at,
                        status,
                        unsaved,
                        ..
                    } = &event
                        && *at == id
                    {
                        assert!(unsaved.is_none(), "没写进存储：{unsaved:?}");
                        return status.clone();
                    }
                    if watch(&event) {
                        service.pause(id);
                    }
                }
            })
            .await
            .expect("任务超时没停下")
        })
    }
}

/// `make runtime` 备好的随应用带的运行时。
fn bundle() -> Bundle {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/runtime/dist");
    let env = std::fs::read_to_string(dist.join("bundle.env"))
        .expect("读不出 bundle.env：先运行 make runtime");
    let get = |key: &str| {
        env.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("bundle.env 里没有 {key}"))
            .to_owned()
    };
    let quickjs = get("QUICKJS_FILE");
    Bundle {
        python: Bundled {
            id: get("PYTHON_ID"),
            bytes: std::fs::read(dist.join("python.tar.zst")).unwrap().into(),
            executable: get("PYTHON_EXECUTABLE"),
        },
        python_abi: get("PYTHON_ABI"),
        quickjs: Bundled {
            id: get("QUICKJS_ID"),
            bytes: std::fs::read(dist.join(&quickjs)).unwrap().into(),
            executable: quickjs,
        },
    }
}

/// 解析出的选项里高度不超过 `height`、有视频的最高的一档（选项高的在前）。
fn at_most(resolved: &Resolved, height: u32) -> &Choice {
    resolved
        .choices
        .iter()
        .find(|choice| choice.video.is_some() && choice.height.is_some_and(|h| h <= height))
        .unwrap_or_else(|| panic!("没有不超过 {height} 的画质：{:?}", resolved.choices))
}

fn web_draft(url: &str, resolved: &Resolved, choice: &Choice, folder: PathBuf) -> Draft {
    Draft {
        url: Url::parse(url).unwrap(),
        headers: Vec::new(),
        folder,
        kind: DraftKind::Web {
            title: resolved.info.title.clone(),
            extractor: resolved.info.extractor.clone(),
            video_id: resolved.info.id.clone(),
            video: choice.video.clone(),
            audio: choice.audio.clone(),
            container: choice.container,
            ext: choice.ext.clone(),
        },
    }
}

/// `folder` 里唯一的文件。
fn only_file(folder: &Path) -> PathBuf {
    let files: Vec<PathBuf> = std::fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    files.into_iter().next().unwrap()
}

/// 本机的 ffprobe 读出的时长（秒）与各流的种类（video、audio）。
fn ffprobe(path: &Path) -> (f64, Vec<String>) {
    let run = |args: &[&str]| {
        let output = std::process::Command::new("ffprobe")
            .args(["-v", "error"])
            .args(args)
            .arg(path)
            .output()
            .expect("本机要有 ffprobe");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let duration = run(&["-show_entries", "format=duration", "-of", "csv=p=0"])
        .trim()
        .parse()
        .unwrap();
    let kinds = run(&["-show_entries", "stream=codec_type", "-of", "csv=p=0"])
        .lines()
        .map(str::to_owned)
        .collect();
    (duration, kinds)
}

/// 下完的视频有视频与音频、时长与 `expected`（秒）相差不到 2 秒。
fn plays(path: &Path, expected: f64) {
    let (duration, kinds) = ffprobe(path);
    println!("{}：{duration:.1} 秒，{kinds:?}", path.display());
    assert!(kinds.iter().any(|k| k == "video") && kinds.iter().any(|k| k == "audio"));
    assert!(
        (duration - expected).abs() < 2.0,
        "时长 {duration}，应为 {expected}"
    );
}

/// 装上稳定版；查新版；换到每日构建、更新到它；换回上一个版本；重装；换回稳定版的渠道。
#[test]
#[ignore = "要联网，由 make acceptance 跑"]
fn components_install_update_roll_back_and_reinstall() {
    let rig = Rig::new("components");
    let stable = rig.install();
    println!("装上 {stable}");
    let latest = rig
        .runtime
        .block_on(rig.service.check_web_update().unwrap());
    assert_eq!(latest.unwrap().unwrap(), None, "刚装的就是稳定版最新的");

    rig.service.set_web_channel(Channel::Nightly).unwrap();
    let nightly = rig
        .runtime
        .block_on(rig.service.check_web_update().unwrap())
        .unwrap()
        .unwrap()
        .expect("每日构建与稳定版不同");
    println!("每日构建 {nightly}");
    let updated = rig.web_changed(|service| service.update_web(nightly.clone()).unwrap());
    assert_eq!(updated, nightly);
    assert_eq!(rig.service.web_previous(), Some(stable.clone()));

    let rolled_back = rig.web_changed(|service| service.rollback_web().unwrap());
    assert_eq!(rolled_back, stable);

    rig.service.set_web_channel(Channel::Stable).unwrap();
    let reinstalled = rig.web_changed(|service| service.install_web().unwrap());
    assert_eq!(reinstalled, stable);
    // 重装后照样解析得了
    let resolved = rig.resolve(YOUTUBE);
    assert!(!resolved.choices.is_empty());
}

/// YouTube：选 360p 下载，下到四分之一时暂停、再继续，下完的能播放、时长对得上；画质优先时最高一档是 AV1 或 VP9。
#[test]
#[ignore = "要联网，由 make acceptance 跑"]
fn youtube_downloads_pauses_resumes_and_plays() {
    let mut rig = Rig::new("youtube");
    rig.install();
    let resolved = rig.resolve(YOUTUBE);
    let choice = at_most(&resolved, 360).clone();
    println!("{}：选 {:?}", resolved.info.title, choice.height);
    let id = rig.create(web_draft(YOUTUBE, &resolved, &choice, rig.out()));
    let paused = rig.ended(id, |event| {
        matches!(event, Event::WebProgress { progress, .. }
            if matches!(**progress, WebProgress::Fetching { bytes, total: Some(total), .. } if bytes * 4 >= total))
    });
    assert_eq!(paused, Status::Paused);
    rig.service.resume(id).unwrap();
    assert_eq!(rig.ended(id, |_| false), Status::Done);
    let duration = resolved.info.duration.expect("YouTube 给时长");
    plays(&only_file(&rig.out()), duration);

    rig.service.set_web_preference(Preference::Quality).unwrap();
    let best = rig.resolve(YOUTUBE).choices[0].clone();
    println!("画质优先：{:?} {:?}", best.height, best.video_codec);
    assert!(matches!(
        best.video_codec,
        Some(VideoCodec::Av1 | VideoCodec::Vp9)
    ));
}

/// B 站：直连解析、下载最低的一档，下完能播放。
#[test]
#[ignore = "要联网，由 make acceptance 跑"]
fn bilibili_downloads_over_a_direct_connection() {
    let mut rig = Rig::new("bilibili");
    rig.install();
    rig.service.set_proxy(&Proxy::Direct).unwrap();
    let resolved = rig.resolve(BILIBILI);
    let choice = resolved
        .choices
        .iter()
        .rev()
        .find(|choice| choice.video.is_some())
        .expect("有视频的画质")
        .clone();
    println!("{}：选 {:?}", resolved.info.title, choice.height);
    let id = rig.create(web_draft(BILIBILI, &resolved, &choice, rig.out()));
    assert_eq!(rig.ended(id, |_| false), Status::Done);
    plays(
        &only_file(&rig.out()),
        resolved.info.duration.expect("B 站给时长"),
    );
}

/// 直连解析在本地网络被拦的网站：说改走代理。
#[test]
#[ignore = "要联网，由 make acceptance 跑"]
fn a_blocked_site_over_a_direct_connection_says_use_a_proxy() {
    let rig = Rig::new("blocked");
    rig.install();
    rig.service.set_proxy(&Proxy::Direct).unwrap();
    let handle = rig
        .service
        .resolve(Url::parse(BLOCKED).unwrap(), &[])
        .unwrap();
    let error = rig
        .within(RESOLVE, "解析", handle)
        .unwrap()
        .expect_err("直连应被拦");
    println!("{error}");
    assert!(error.to_string().contains("改走代理"), "{error}");
}

/// 只填端口：下载代理认出协议，BT 代理认出 SOCKS5 并记下。
#[test]
#[ignore = "要联网，由 make acceptance 跑"]
fn a_proxy_given_only_by_its_port_is_identified() {
    let port: u16 = std::env::var("HS_DLX_ACCEPT_PROXY_PORT")
        .expect("设 HS_DLX_ACCEPT_PROXY_PORT 为本机代理的端口")
        .parse()
        .unwrap();
    let rig = Rig::new("proxy");
    let identify = |wanted| {
        let handle = rig
            .service
            .identify_proxy("127.0.0.1".to_owned(), port, None, wanted);
        rig.runtime.block_on(handle).unwrap().unwrap()
    };
    let download = identify(ProxyWanted::Any);
    println!("下载代理：{download}");
    rig.service.set_proxy(&Proxy::Url(download)).unwrap();
    let bt = identify(ProxyWanted::Only(ProxyProtocol::Socks5));
    println!("BT 代理：{bt}");
    assert!(bt.scheme().starts_with("socks5"));
    rig.service.set_torrent_proxy(Some(bt.clone())).unwrap();
    assert_eq!(rig.service.settings().unwrap().torrent_proxy, Some(bt));
}

/// 普通 m3u8：选带宽最低的一档写成 MP4，能播放。
#[test]
#[ignore = "要联网，由 make acceptance 跑"]
fn an_hls_stream_downloads_and_plays() {
    let mut rig = Rig::new("hls");
    let mut source = Source::new(Url::parse(HLS).unwrap());
    source.http.proxy = Proxy::System;
    let probe = rig
        .runtime
        .block_on(rig.service.probe(source).unwrap())
        .unwrap()
        .unwrap();
    let master = probe.master.as_ref().expect("是主播放列表");
    let lowest = master
        .variants
        .iter()
        .min_by_key(|variant| variant.bandwidth.unwrap_or(u64::MAX))
        .unwrap();
    let id = rig.create(Draft {
        url: Url::parse(HLS).unwrap(),
        headers: Vec::new(),
        folder: rig.out(),
        kind: DraftKind::Hls {
            title: None,
            preference: TrackPreference {
                variant: lowest.choice(),
                audio: AudioChoice::Default,
            },
            output: OutputKind::Mp4,
            live: false,
        },
    });
    assert_eq!(rig.ended(id, |_| false), Status::Done);
    let (duration, kinds) = ffprobe(&only_file(&rig.out()));
    println!("{duration:.1} 秒，{kinds:?}");
    assert!(duration > 590.0 && kinds.iter().any(|k| k == "video"));
}

/// 磁力链接：取到种子信息，下完全部文件，大小对得上。设了 `HS_DLX_ACCEPT_PROXY_PORT` 时 BT 经本机的 SOCKS5 代理（中国
/// 大陆直连多半找不到节点）；取不到种子信息时打出连着的节点数与 tracker 的错误。
#[test]
#[ignore = "要联网，由 make acceptance 跑"]
fn a_magnet_link_downloads() {
    let mut rig = Rig::new("magnet");
    if let Ok(port) = std::env::var("HS_DLX_ACCEPT_PROXY_PORT") {
        let proxy = Url::parse(&format!("socks5h://127.0.0.1:{port}")).unwrap();
        rig.service.set_torrent_proxy(Some(proxy)).unwrap();
    }
    let fetch = rig.service.fetch_metadata(DEBIAN.to_owned()).unwrap();
    let progress = fetch.progress.clone();
    let resume = rig.runtime.block_on(async {
        tokio::time::timeout(RESOLVE, fetch.resume)
            .await
            .unwrap_or_else(|_| panic!("取种子信息超时：{:?}", *progress.borrow()))
    });
    let resume = resume.unwrap().unwrap();
    let info = TorrentInfo::of(&resume).unwrap();
    println!("{}：{} 个文件", info.name, info.files.len());
    let id = rig.create(Draft {
        url: Url::parse(DEBIAN).unwrap(),
        headers: Vec::new(),
        folder: rig.out(),
        kind: DraftKind::Torrent {
            resume,
            wanted: info.files.iter().map(|file| file.index).collect(),
        },
    });
    assert_eq!(rig.ended(id, |_| false), Status::Done);
    for file in &info.files {
        let size = std::fs::metadata(rig.out().join(&file.path)).unwrap().len();
        assert_eq!(size, file.size, "{}", file.path);
    }
}

/// PikPak 云离线：登录；一个任务网盘下完、取回本机；另一个取回途中放弃，本机的删掉。
#[test]
#[ignore = "要联网与真实账号，由 make acceptance 跑"]
fn pikpak_offline_fetches_and_discards() {
    let mut rig = Rig::new("pikpak");
    let login = rig
        .runtime
        .block_on(rig.service.login_pikpak())
        .unwrap()
        .unwrap();
    println!(
        "在浏览器里打开 {} 确认登录，登录码 {}",
        login.url, login.user_code
    );
    rig.within(LOGIN, "等登录", login.done).unwrap().unwrap();
    let cloud = |magnet: &str, folder: PathBuf| Draft {
        url: Url::parse(magnet).unwrap(),
        headers: Vec::new(),
        folder,
        kind: DraftKind::Cloud {
            provider: CloudProvider::PikPak,
        },
    };

    let id = rig.create(cloud(SINTEL, rig.out()));
    let mut remote = false;
    assert_eq!(
        rig.ended(id, |event| {
            if let Event::CloudProgress { progress, .. } = event
                && matches!(**progress, CloudProgress::Remote { .. })
            {
                remote = true;
            }
            false
        }),
        Status::Done
    );
    println!("网盘进度报过：{remote}");
    let files = rig.service.cloud_files(id).unwrap();
    assert!(!files.is_empty());
    println!("取回 {} 个文件", files.len());

    let dropped = rig.dir.join("dropped");
    std::fs::create_dir_all(&dropped).unwrap();
    let id = rig.create(cloud(BIG_BUCK_BUNNY, dropped.clone()));
    let paused = rig.ended(id, |event| {
        matches!(event, Event::CloudProgress { progress, .. }
            if matches!(**progress, CloudProgress::Fetching { done, ref file, .. } if done + file.bytes > 0))
    });
    assert_eq!(paused, Status::Paused);
    let leftovers = rig
        .runtime
        .block_on(rig.service.discard(id))
        .unwrap()
        .unwrap();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let left: Vec<PathBuf> = walk(&dropped);
    assert!(left.is_empty(), "本机没删干净：{left:?}");
}

/// `dir` 下的全部文件。
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}
