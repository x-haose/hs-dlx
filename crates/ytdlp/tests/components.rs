//! 组件的安装与更新：随应用带的「Python」与「QuickJS-NG」直接给内容，PyPI 的 JSON 接口与 wheel 由本地起的服务器给。
//! 「Python」是转调仓库开发环境（.venv）里真 Python 的脚本，驱动脚本列依赖、挑 wheel、pip 离线安装与自检都照真的跑；
//! 「QuickJS-NG」是打出 42 的//! 脚本。脚本靠 shebang 运行，只在类 Unix 系统上测（Windows 上的安装与这里同一套代码，只有可执行文件的路径不同）。
#![cfg(unix)]

use std::collections::HashMap;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hs_dlx_ytdlp::{Bundle, Bundled, Channel, Components, Error, Installed, Sources, Stage};
use hs_m3u8_core::{CancellationToken, Engine, Proxy, Url};
use sha2::{Digest as _, Sha256};

/// 服务器：路径（不带开头的 `/`）到内容，与各路径被请求的次数。
#[derive(Clone, Default)]
struct Server {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl Server {
    fn put(&self, path: &str, bytes: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_owned(), bytes);
    }

    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("ytdlp")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 仓库开发环境里的 Python（`make check` 先 `uv sync` 建好；带 pip）。
fn dev_python() -> PathBuf {
    let python = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.venv/bin/python");
    assert!(
        python.exists(),
        "缺 {}：先在仓库根目录运行 uv sync",
        python.display()
    );
    python.canonicalize().unwrap()
}

/// 随应用带的「Python」：tar 经 zstd 压缩，`python/bin/python3` 转调开发环境里的 Python。
fn python_package() -> Vec<u8> {
    let script = format!("#!/bin/sh\nexec '{}' \"$@\"\n", dev_python().display());
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o755);
    tar.append_data(&mut header, "python/bin/python3", script.as_bytes())
        .unwrap();
    let tar = tar.into_inner().unwrap();
    ruzstd::encoding::compress_to_vec(tar.as_slice(), ruzstd::encoding::CompressionLevel::Fastest)
}

/// 随应用带的「QuickJS-NG」：`--help` 报版本，别的（自检让它算 6 * 7）打出 42。
const QUICKJS: &[u8] =
    b"#!/bin/sh\n[ \"$1\" = --help ] && echo 'QuickJS-ng version 0.18.0' || echo 42\n";

/// 随应用带的 Python 与「QuickJS-NG」（自检让它算 6 * 7）。
fn bundle() -> Bundle {
    Bundle {
        python: Bundled {
            id: "3.13.18-test".to_owned(),
            bytes: python_package().into(),
            executable: "python/bin/python3".to_owned(),
        },
        python_abi: "cp313-test".to_owned(),
        quickjs: Bundled {
            id: "0.18.0-test".to_owned(),
            bytes: QUICKJS.to_vec().into(),
            executable: "qjs".to_owned(),
        },
    }
}

/// 名为 `dist`（wheel 里的写法）、版本 `version`、内含 `files` 的纯 Python wheel 的文件名与内容。
fn wheel(dist: &str, version: &str, files: &[(String, String)]) -> (String, Vec<u8>) {
    let name = format!("{dist}-{version}-py3-none-any.whl");
    let info = format!("{dist}-{version}.dist-info");
    let mut all: Vec<(String, String)> = files.to_vec();
    all.push((
        format!("{info}/METADATA"),
        format!(
            "Metadata-Version: 2.1\nName: {}\nVersion: {version}\n",
            dist.replace('_', "-")
        ),
    ));
    all.push((
        format!("{info}/WHEEL"),
        "Wheel-Version: 1.0\nGenerator: hs-dlx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
            .to_owned(),
    ));
    let mut record = String::new();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (path, content) in &all {
        zip.start_file(path.as_str(), options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
        record.push_str(&format!("{path},,\n"));
    }
    record.push_str(&format!("{info}/RECORD,,\n"));
    zip.start_file(format!("{info}/RECORD"), options).unwrap();
    zip.write_all(record.as_bytes()).unwrap();
    (name, zip.finish().unwrap().into_inner())
}

/// PyPI 上一个版本的 JSON：依赖声明与各个文件（`files` 下的）。
fn release_json(base: &Url, requires: &[&str], files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let urls: Vec<serde_json::Value> = files
        .iter()
        .map(|(name, bytes)| {
            serde_json::json!({
                "filename": name,
                "url": base.join(&format!("files/{name}")).unwrap().as_str(),
                "size": bytes.len(),
                "digests": { "sha256": sha256(bytes).to_uppercase() },
            })
        })
        .collect();
    serde_json::json!({ "info": { "requires_dist": requires }, "urls": urls })
        .to_string()
        .into_bytes()
}

/// 假 yt-dlp 的 `yt_dlp/__init__.py`：驱动脚本的自检要的 YoutubeDL 认得给它的 QuickJS-NG（像真的那样跑它的
/// `--help`，第一行带 `QuickJS-ng` 即认为支持）。
const FAKE_YT_DLP: &str = "\
import subprocess, types

class YoutubeDL:
    def __init__(self, params):
        path = params['js_runtimes']['quickjs']['path']
        out = subprocess.run([path, '--help'], capture_output=True, text=True).stdout
        info = types.SimpleNamespace(supported=out.startswith('QuickJS-ng')) if out else None
        self._js_runtimes = {'quickjs': types.SimpleNamespace(info=info)}
";

/// 在服务器上发布 yt-dlp `version`（wheel 的元数据也写这个版本，像 PyPI 上的一样；`yt_dlp.version` 的写法不同，自检
/// 不该看它）与它锁定的依赖：certifi、yt-dlp-ejs；mutagen 锁定了但不该装（服务器上没有它，请求即失败）；brotlicffi
/// 只给别的解释器。
fn publish(server: &Server, base: &Url, version: &str) {
    publish_with(server, base, version, true);
}

/// 同 [`publish`]；`allowlist` 为假时包里没有驱动脚本自检要找的扩展名白名单（像改掉了内部名字的新版）。
fn publish_with(server: &Server, base: &Url, version: &str, allowlist: bool) {
    let mut files = vec![
        ("yt_dlp/__init__.py".to_owned(), FAKE_YT_DLP.to_owned()),
        (
            "yt_dlp/version.py".to_owned(),
            "__version__ = 'not-the-pypi-spelling'\n".to_owned(),
        ),
    ];
    if allowlist {
        files.push(("yt_dlp/utils/__init__.py".to_owned(), String::new()));
        files.push((
            "yt_dlp/utils/_utils.py".to_owned(),
            "class _UnsafeExtensionError(Exception):\n    ALLOWED_EXTENSIONS = frozenset({'mp4', 'm4a'})\n"
                .to_owned(),
        ));
    }
    let (name, bytes) = wheel("yt_dlp", version, &files);
    // 别的平台的包不该被挑中
    let foreign = format!("yt_dlp-{version}-cp27-cp27m-win32.whl");
    let requires = [
        "certifi==2026.10.23; extra == 'pin'",
        "mutagen==1.48.1; extra == 'pin'",
        "yt-dlp-ejs==0.8.0; extra == 'pin'",
        "brotlicffi==1.2.1.0; (implementation_name != 'cpython') and extra == 'pin'",
        "certifi>=2026.10.19; extra == 'default'",
    ];
    let files = [
        (name.clone(), bytes.clone()),
        (foreign.clone(), b"x".to_vec()),
    ];
    server.put(
        &format!("pypi/yt-dlp/{version}/json"),
        release_json(base, &requires, &files),
    );
    server.put(&format!("files/{name}"), bytes);
    server.put(&format!("files/{foreign}"), b"x".to_vec());
    for (dist, dep_version) in [("certifi", "2026.10.23"), ("yt_dlp_ejs", "0.8.0")] {
        let (name, bytes) = wheel(
            dist,
            dep_version,
            &[(format!("{dist}/__init__.py"), String::new())],
        );
        let files = [(name.clone(), bytes.clone())];
        server.put(
            &format!("pypi/{}/{dep_version}/json", dist.replace('_', "-")),
            release_json(base, &[], &files),
        );
        server.put(&format!("files/{name}"), bytes);
    }
}

/// 项目的 JSON：稳定版为 `stable`，另有每日构建 `nightly`（上传得更晚）。
fn project_json(stable: &str, nightly: &str) -> Vec<u8> {
    serde_json::json!({
        "info": { "version": stable },
        "releases": {
            stable: [{ "upload_time_iso_8601": "2026-10-26T22:25:00.000000Z", "yanked": false }],
            nightly: [{ "upload_time_iso_8601": "2026-11-16T23:30:00.000000Z", "yanked": false }]
        }
    })
    .to_string()
    .into_bytes()
}

async fn serve_file(State(server): State<Server>, UrlPath(path): UrlPath<String>) -> Response {
    *server.hits.lock().unwrap().entry(path.clone()).or_default() += 1;
    match server.files.lock().unwrap().get(&path) {
        Some(bytes) => bytes.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 起服务器，放上组件包与 yt-dlp 的 `versions`（第一个为稳定版、第二个为每日构建，没有时同第一个）。
async fn start(versions: &[&str]) -> (Server, Url) {
    let server = Server::default();
    let app = axum::Router::new()
        .route("/{*path}", get(serve_file))
        .with_state(server.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for version in versions {
        publish(&server, &base, version);
    }
    if let Some(stable) = versions.first() {
        let nightly = versions.get(1).unwrap_or(stable);
        server.put("pypi/yt-dlp/json", project_json(stable, nightly));
    }
    (server, base)
}

fn components(root: &Path, bundle: Bundle, base: &Url) -> Components {
    Components::new(
        root.to_path_buf(),
        bundle,
        Sources {
            api: base.join("pypi/").unwrap(),
        },
        Engine::new(NonZeroUsize::new(4).unwrap()),
    )
}

async fn install(components: &Components, channel: Channel) -> Result<Installed, Error> {
    components
        .install(&Proxy::Direct, channel, &|_| {}, &CancellationToken::new())
        .await
}

/// 从无到有装全：解出随应用带的 Python 与 QuickJS-NG，下载 yt-dlp 与锁定的依赖（不要 mutagen、不要别的平台的包）、
/// pip 离线装上并自检；再装一次什么也不做；驱动脚本不在了（应用升级后换了内容）时读已装的即写出。
#[tokio::test(flavor = "multi_thread")]
async fn components_install_from_nothing() {
    let root = test_dir("fresh");
    let (server, base) = start(&["2026.10.26"]).await;
    let components = components(&root, bundle(), &base);
    assert_eq!(components.installed().unwrap(), None);
    let seen = Mutex::new(Vec::new());
    let installed = components
        .install(
            &Proxy::Direct,
            Channel::Stable,
            &|progress| seen.lock().unwrap().push(progress),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(installed.version, "2026.10.26");
    assert!(
        installed
            .python
            .ends_with("python-3.13.18-test/python/bin/python3")
    );
    assert!(installed.quickjs.ends_with("quickjs-0.18.0-test/qjs"));
    for module in ["yt_dlp", "certifi", "yt_dlp_ejs"] {
        assert!(installed.site.join(module).is_dir(), "缺 {module}");
    }
    assert!(!installed.site.join("mutagen").exists());
    assert_eq!(server.hits("pypi/mutagen/1.48.1/json"), 0);
    assert_eq!(
        server.hits("files/yt_dlp-2026.10.26-cp27-cp27m-win32.whl"),
        0
    );
    let seen = seen.into_inner().unwrap();
    for stage in [
        Stage::Python,
        Stage::QuickJs,
        Stage::Resolving,
        Stage::YtDlp,
        Stage::Setup,
        Stage::Checking,
    ] {
        assert!(seen.iter().any(|p| p.stage == stage), "没报 {stage:?}");
    }
    // 下 yt-dlp 时按字节报进度，最后到总数
    let last = seen.iter().rev().find(|p| p.stage == Stage::YtDlp).unwrap();
    assert!(
        last.total.is_some() && Some(last.bytes) == last.total,
        "{last:?}"
    );
    // 下载的 wheel 用过即删
    assert_eq!(
        std::fs::read_dir(root.join("downloads/wheels"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(components.installed().unwrap(), Some(installed.clone()));
    // 装全了再装不再请求
    server.files.lock().unwrap().clear();
    assert_eq!(
        install(&components, Channel::Stable).await.unwrap(),
        installed
    );
    std::fs::remove_file(&installed.driver).unwrap();
    assert_eq!(components.installed().unwrap(), Some(installed.clone()));
    assert!(installed.driver.exists());
}

/// 下到的 wheel 与 PyPI 给的哈希不符：报出（可重试）、删掉，不装。
#[tokio::test(flavor = "multi_thread")]
async fn a_wheel_that_does_not_match_its_digest_is_refused() {
    let root = test_dir("mismatch");
    let (server, base) = start(&["2026.10.26"]).await;
    server.put(
        "files/certifi-2026.10.23-py3-none-any.whl",
        b"tampered".to_vec(),
    );
    let components = components(&root, bundle(), &base);
    let error = install(&components, Channel::Stable).await.unwrap_err();
    assert!(matches!(error, Error::Mismatch { .. }), "{error}");
    assert!(error.retryable());
    assert!(
        !root
            .join("downloads/wheels/certifi-2026.10.23-py3-none-any.whl")
            .exists()
    );
    assert_eq!(components.installed().unwrap(), None);
}

/// 解了一半、缺了可执行文件的目录重新解出来；以前版本下载的组件包与续传目录删掉。
#[tokio::test(flavor = "multi_thread")]
async fn leftovers_from_an_interrupted_install_are_checked_not_trusted() {
    let root = test_dir("leftovers");
    let (_server, base) = start(&["2026.10.26"]).await;
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    std::fs::create_dir_all(root.join("quickjs-0.18.0-test")).unwrap();
    std::fs::write(root.join("downloads/deno-2.9.8"), b"old").unwrap();
    std::fs::create_dir_all(root.join("downloads/python-3.13.hsdl")).unwrap();
    let components = components(&root, bundle(), &base);
    install(&components, Channel::Stable).await.unwrap();
    assert!(root.join("quickjs-0.18.0-test/qjs").is_file());
    assert!(!root.join("downloads/deno-2.9.8").exists());
    assert!(!root.join("downloads/python-3.13.hsdl").exists());
}

/// 重装：随应用带的重新解出来、yt-dlp 重新下载，换下在用的（内容坏了而路径还在，安装会当作装好了），yt-dlp 照记着
/// 的版本；yt-dlp 下载失败时在用的 yt-dlp 不动。
#[tokio::test(flavor = "multi_thread")]
async fn repairing_puts_everything_in_place_again() {
    let root = test_dir("repair");
    let (server, base) = start(&["2026.10.26"]).await;
    let components = components(&root, bundle(), &base);
    let first = install(&components, Channel::Stable).await.unwrap();
    std::fs::write(&first.quickjs, b"broken").unwrap();
    let wheel = "files/yt_dlp-2026.10.26-py3-none-any.whl";
    let fetched = server.hits(wheel);
    assert_eq!(install(&components, Channel::Stable).await.unwrap(), first);
    let repaired = components
        .repair(
            &Proxy::Direct,
            Channel::Stable,
            &|_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(repaired, first);
    assert_eq!(std::fs::read(&repaired.quickjs).unwrap(), QUICKJS);
    assert_eq!(server.hits(wheel), fetched + 1);
    // yt-dlp 下不到（服务器上没有了）：报错，在用的照旧
    server.files.lock().unwrap().remove(wheel);
    std::fs::write(repaired.site.join("kept"), b"").unwrap();
    assert!(
        components
            .repair(
                &Proxy::Direct,
                Channel::Stable,
                &|_| {},
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(repaired.site.join("kept").exists());
    assert_eq!(components.installed().unwrap(), Some(repaired));
}

/// 换组件目录时在两次改名之间中断：原名没有新的放上，启动时把最近挪开的那个挪回去；清理删掉别的挪开的，要挪回而没
/// 挪成的留着。
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_replacement_is_moved_back_when_sweeping() {
    let root = test_dir("trash");
    let (_server, base) = start(&["2026.10.26"]).await;
    let components = components(&root, bundle(), &base);
    let installed = install(&components, Channel::Stable).await.unwrap();
    std::fs::rename(
        root.join("quickjs-0.18.0-test"),
        root.join("trash-quickjs-0.18.0-test-20"),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("trash-quickjs-0.18.0-test-10")).unwrap();
    std::fs::create_dir_all(root.join("trash-yt-dlp-2026.10.26-5")).unwrap();
    assert_eq!(components.installed().unwrap(), None);
    // 没挪回之前清理：要挪回的那个留着
    components.sweep().await;
    assert!(root.join("trash-quickjs-0.18.0-test-20").is_dir());
    assert!(!root.join("trash-quickjs-0.18.0-test-10").exists());
    components.recover();
    assert_eq!(components.installed().unwrap(), Some(installed));
    components.sweep().await;
    let left: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("trash-"))
        .collect();
    assert_eq!(left, Vec::<String>::new());
}

/// 更新：装进另一个目录、自检过了才换成当前的，上一个留着，更早的到清理时删掉（更新时不删）；每日构建的版本号带 `.dev0`（元数据也这样
/// 写）；要装的版本 PyPI 上没有时报出、在用的不变。
#[tokio::test(flavor = "multi_thread")]
async fn updates_switch_only_after_the_new_version_passes() {
    let root = test_dir("update");
    let (server, base) = start(&["2026.10.21"]).await;
    let components = components(&root, bundle(), &base);
    let first = install(&components, Channel::Stable).await.unwrap();
    assert_eq!(first.version, "2026.10.21");

    let missing = components
        .update(
            &Proxy::Direct,
            "2026.10.26",
            &|_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(missing, Error::Index(_)), "{missing}");
    assert_eq!(components.installed().unwrap(), Some(first.clone()));
    // 自检不过（扩展名白名单没了）：不换上
    publish_with(&server, &base, "2026.10.24", false);
    let refused = components
        .update(
            &Proxy::Direct,
            "2026.10.24",
            &|_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(refused, Error::SelfTest(_)), "{refused}");
    assert_eq!(components.installed().unwrap(), Some(first.clone()));

    for version in ["2026.10.26", "2026.11.16.232655.dev0"] {
        publish(&server, &base, version);
        let updated = components
            .update(&Proxy::Direct, version, &|_| {}, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(updated.version, version);
        assert_eq!(components.installed().unwrap(), Some(updated));
    }
    assert!(root.join("yt-dlp-2026.10.21").exists(), "更新时不删");
    // 换回上一个：不联网，换后上一个是刚才在用的；再换一次又回来
    let hits = server.hits("pypi/yt-dlp/2026.10.26/json");
    let back = components
        .rollback(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(back.version, "2026.10.26");
    assert_eq!(back.previous.as_deref(), Some("2026.11.16.232655.dev0"));
    assert_eq!(components.installed().unwrap(), Some(back));
    assert_eq!(server.hits("pypi/yt-dlp/2026.10.26/json"), hits);
    let again = components
        .rollback(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(again.version, "2026.11.16.232655.dev0");
    // 更新到上一个（目录还在）：照换回，不重装
    let listed = server.hits("pypi/yt-dlp/2026.10.26/json");
    let updated = components
        .update(
            &Proxy::Direct,
            "2026.10.26",
            &|_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(updated.version, "2026.10.26");
    assert_eq!(server.hits("pypi/yt-dlp/2026.10.26/json"), listed);
    components.sweep().await;
    assert!(root.join("yt-dlp-2026.10.26").exists(), "上一个留着");
    assert!(!root.join("yt-dlp-2026.10.21").exists(), "更早的删掉");
}

/// 换了锁定的 Python（应用升级）：已装的 yt-dlp 不算装好，安装时用新的 Python 重装同一版本，旧的 Python 到清理时
/// 删掉。
#[tokio::test(flavor = "multi_thread")]
async fn a_new_python_reinstalls_yt_dlp() {
    let root = test_dir("python");
    let (_server, base) = start(&["2026.10.21"]).await;
    install(&components(&root, bundle(), &base), Channel::Stable)
        .await
        .unwrap();
    let mut newer = bundle();
    newer.python.id = "3.14.10-test".to_owned();
    newer.python_abi = "cp314-test".to_owned();
    let components = components(&root, newer, &base);
    assert_eq!(components.installed().unwrap(), None);
    let installed = install(&components, Channel::Stable).await.unwrap();
    assert_eq!(installed.version, "2026.10.21");
    assert!(
        installed
            .python
            .starts_with(root.join("python-3.14.10-test"))
    );
    assert!(root.join("python-3.13.18-test").exists(), "安装时不删");
    components.sweep().await;
    assert!(!root.join("python-3.13.18-test").exists());
}

/// 应用升级只换了随带的运行时、Python 的 ABI 没变：装着的 yt-dlp 照用，解出随带的即可（启动时在后台解，可取消；没解成
/// 时再装），都不联网。
#[tokio::test(flavor = "multi_thread")]
async fn a_new_build_of_the_same_python_is_unpacked_without_reinstalling() {
    let mut newer = bundle();
    newer.python.id = "3.13.19-test".to_owned();
    newer.quickjs.id = "0.18.1-test".to_owned();
    for (name, after_failure) in [("unpack", false), ("unpack-install", true)] {
        let root = test_dir(name);
        let (server, base) = start(&["2026.10.21"]).await;
        let old = install(&components(&root, bundle(), &base), Channel::Stable)
            .await
            .unwrap();
        // 服务器上什么都没有了：联网的一步都会失败
        server.files.lock().unwrap().clear();
        let components = components(&root, newer.clone(), &base);
        assert_eq!(components.installed().unwrap(), None);
        assert!(components.bundled_missing());
        let installed = if after_failure {
            install(&components, Channel::Stable).await.unwrap()
        } else {
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            assert!(matches!(
                components.unpack_bundled(&cancelled).await,
                Err(Error::Cancelled)
            ));
            components
                .unpack_bundled(&CancellationToken::new())
                .await
                .unwrap()
        };
        assert_eq!(installed.version, old.version);
        assert_eq!(installed.site, old.site);
        assert!(
            installed
                .python
                .starts_with(root.join("python-3.13.19-test"))
        );
        assert!(
            installed
                .quickjs
                .starts_with(root.join("quickjs-0.18.1-test"))
        );
        assert!(!components.bundled_missing());
        assert_eq!(components.installed().unwrap(), Some(installed));
    }
}

/// 状态文件坏了：读已装的报出，安装修好它（重装最新的）。
#[tokio::test(flavor = "multi_thread")]
async fn a_broken_state_file_is_repaired_by_installing() {
    let root = test_dir("broken-state");
    let (_server, base) = start(&["2026.10.26"]).await;
    let components = components(&root, bundle(), &base);
    install(&components, Channel::Stable).await.unwrap();
    std::fs::write(root.join("state.json"), b"").unwrap();
    assert!(matches!(components.installed(), Err(Error::Disk(_))));
    let installed = install(&components, Channel::Stable).await.unwrap();
    assert_eq!(components.installed().unwrap(), Some(installed));
}

/// 查最新版本按 PyPI 的 JSON 接口；每日构建渠道装上的就是最新的那个每日构建。
#[tokio::test(flavor = "multi_thread")]
async fn the_nightly_channel_installs_the_latest_dev_release() {
    let root = test_dir("nightly");
    let (_server, base) = start(&["2026.10.26", "2026.11.16.232655.dev0"]).await;
    let components = components(&root, bundle(), &base);
    assert_eq!(
        components
            .latest(&Proxy::Direct, Channel::Stable, &CancellationToken::new())
            .await
            .unwrap(),
        "2026.10.26"
    );
    let installed = install(&components, Channel::Nightly).await.unwrap();
    assert_eq!(installed.version, "2026.11.16.232655.dev0");
}

/// make runtime 备好的、随应用带的真的 Python 与 QuickJS-NG（third_party/runtime/dist，见 scripts/runtime.py）。
fn real_bundle() -> Bundle {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/runtime/dist");
    let env = std::fs::read_to_string(dist.join("bundle.env"))
        .unwrap_or_else(|error| panic!("先运行 make runtime：{error}"));
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

/// 随应用带的真的 Python 与 QuickJS-NG：解得出来（含符号链接与可执行权限），解出的 Python 跑得了 pip 离线安装、驱动
/// 脚本与自检，QuickJS-NG 算得对。
#[tokio::test(flavor = "multi_thread")]
async fn the_real_bundled_runtime_unpacks_and_runs() {
    let root = test_dir("real");
    let (_server, base) = start(&["2026.10.26"]).await;
    let components = components(&root, real_bundle(), &base);
    let installed = install(&components, Channel::Stable).await.unwrap();
    assert_eq!(installed.version, "2026.10.26");
    assert!(installed.python.is_file());
    assert!(installed.quickjs.is_file());
}
