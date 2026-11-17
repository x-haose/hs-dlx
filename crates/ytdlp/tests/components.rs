//! 组件的安装与更新，对着本地起的服务器：Python 与 deno 的包、PyPI 的 simple 索引与 JSON 接口都由它给。「Python」是
//! 转调仓库开发环境（.venv）里真 Python 的脚本，pip 与驱动脚本照真的跑；deno 是报版本的脚本。脚本靠 shebang 运行，
//! 只在类 Unix 系统上测（Windows 上的安装与这里同一套代码，只有可执行文件的路径不同）。
#![cfg(unix)]

use std::collections::HashMap;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use hs_dlx_ytdlp::{Archive, Channel, Components, Error, Pinned, Pins, Sources, Stage};
use hs_m3u8_core::{CancellationToken, Engine, Proxy, Url};
use sha2::{Digest as _, Sha256};

/// 服务器给出的文件：路径（不带开头的 `/`）到内容。
type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

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

/// 「Python」的包：`python/bin/python3` 转调开发环境里的 Python。
fn python_package() -> Vec<u8> {
    let script = format!("#!/bin/sh\nexec '{}' \"$@\"\n", dev_python().display());
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o755);
    tar.append_data(&mut header, "python/bin/python3", script.as_bytes())
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

/// 「deno」的包：报版本的脚本。
fn deno_package() -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o755);
    zip.start_file("deno", options).unwrap();
    zip.write_all(b"#!/bin/sh\necho 'deno 2.9.8 (stable, release, aarch64-apple-darwin)'\n")
        .unwrap();
    zip.finish().unwrap().into_inner()
}

/// 测试用的 yt-dlp `version` 的 wheel：只有版本模块与 yt-dlp-ejs 的空包，够自检用。
fn wheel(version: &str) -> (String, Vec<u8>) {
    let name = format!("yt_dlp-{version}-py3-none-any.whl");
    let info = format!("yt_dlp-{version}.dist-info");
    let files = [
        ("yt_dlp/__init__.py".to_owned(), String::new()),
        (
            "yt_dlp/version.py".to_owned(),
            format!("__version__ = {version:?}\n"),
        ),
        ("yt_dlp_ejs/__init__.py".to_owned(), String::new()),
        (
            format!("{info}/METADATA"),
            format!("Metadata-Version: 2.1\nName: yt-dlp\nVersion: {version}\n"),
        ),
        (
            format!("{info}/WHEEL"),
            "Wheel-Version: 1.0\nGenerator: hs-dlx-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n"
                .to_owned(),
        ),
    ];
    let mut record = String::new();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (path, content) in &files {
        zip.start_file(path.as_str(), options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
        record.push_str(&format!("{path},,\n"));
    }
    record.push_str(&format!("{info}/RECORD,,\n"));
    zip.start_file(format!("{info}/RECORD"), options).unwrap();
    zip.write_all(record.as_bytes()).unwrap();
    (name, zip.finish().unwrap().into_inner())
}

async fn serve_file(State(files): State<Files>, UrlPath(path): UrlPath<String>) -> Response {
    match files.lock().unwrap().get(&path) {
        Some(bytes) => bytes.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// PEP 503 的项目页：列出 `files/` 下的各个 wheel，带哈希。
async fn simple(State(files): State<Files>) -> Response {
    let files = files.lock().unwrap();
    let mut links: Vec<String> = files
        .iter()
        .filter(|(path, _)| path.ends_with(".whl"))
        .map(|(path, bytes)| {
            let name = path.trim_start_matches("files/");
            format!(
                "<a href=\"/{path}#sha256={}\">{name}</a><br>",
                sha256(bytes)
            )
        })
        .collect();
    links.sort();
    (
        [(header::CONTENT_TYPE, "text/html")],
        format!(
            "<!DOCTYPE html><html><body>{}</body></html>",
            links.join("")
        ),
    )
        .into_response()
}

async fn start(files: Files) -> Url {
    let app = axum::Router::new()
        .route("/simple/yt-dlp/", get(simple))
        .route("/{*path}", get(serve_file))
        .with_state(files);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn pins(base: &Url, python: &[u8], deno: &[u8]) -> Pins {
    Pins {
        python: Pinned {
            version: "3.13.18-20261110".to_owned(),
            url: base.join("python.tar.gz").unwrap().to_string(),
            sha256: sha256(python),
            archive: Archive::TarGz,
            executable: "python/bin/python3".to_owned(),
        },
        deno: Pinned {
            version: "2.9.8".to_owned(),
            url: base.join("deno.zip").unwrap().to_string(),
            sha256: sha256(deno),
            archive: Archive::Zip,
            executable: "deno".to_owned(),
        },
    }
}

fn sources(base: &Url) -> Sources {
    Sources {
        index: base.join("simple/").unwrap(),
        releases: base.join("pypi/yt-dlp/json").unwrap(),
    }
}

/// 服务器上的组件包与 yt-dlp 的 `versions`。
fn files(versions: &[&str]) -> (Files, Vec<u8>, Vec<u8>) {
    let (python, deno) = (python_package(), deno_package());
    let mut map = HashMap::new();
    map.insert("python.tar.gz".to_owned(), python.clone());
    map.insert("deno.zip".to_owned(), deno.clone());
    for version in versions {
        let (name, bytes) = wheel(version);
        map.insert(format!("files/{name}"), bytes);
    }
    (Arc::new(Mutex::new(map)), python, deno)
}

fn components(root: &Path, pins: Pins, sources: Sources) -> Components {
    Components::new(
        root.to_path_buf(),
        pins,
        sources,
        Engine::new(NonZeroUsize::new(4).unwrap()),
    )
}

/// 从无到有装全：下载、核对、解开 Python 与 deno，pip 从索引装上最新的稳定版并自检；再装一次什么也不做。
#[tokio::test(flavor = "multi_thread")]
async fn components_install_from_nothing() {
    let root = test_dir("fresh");
    let (files, python, deno) = files(&["2026.10.21", "2026.10.26"]);
    let base = start(files.clone()).await;
    let components = components(&root, pins(&base, &python, &deno), sources(&base));
    assert_eq!(components.installed().unwrap(), None);
    let stages = Mutex::new(Vec::new());
    let installed = components
        .install(
            &Proxy::Direct,
            Channel::Stable,
            &|progress| stages.lock().unwrap().push(progress.stage),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(installed.version, "2026.10.26");
    assert!(
        installed
            .python
            .ends_with("python-3.13.18-20261110/python/bin/python3")
    );
    assert!(installed.deno.ends_with("deno-2.9.8/deno"));
    assert!(installed.site.join("yt_dlp/version.py").exists());
    assert!(installed.driver.exists());
    assert_eq!(components.installed().unwrap(), Some(installed.clone()));
    let stages = stages.into_inner().unwrap();
    for stage in [Stage::Python, Stage::Deno, Stage::YtDlp, Stage::Checking] {
        assert!(stages.contains(&stage), "没报 {stage:?}：{stages:?}");
    }
    // 下载的包解开后删掉
    assert!(!root.join("downloads/python-3.13.18-20261110").exists());
    // 驱动脚本不在了（应用升级后换了内容）时读已装的组件即写出
    std::fs::remove_file(&installed.driver).unwrap();
    assert_eq!(components.installed().unwrap(), Some(installed.clone()));
    assert!(installed.driver.exists());
    // 装全了再装不再请求：服务器上的包没了也照样返回
    files.lock().unwrap().clear();
    let again = components
        .install(
            &Proxy::Direct,
            Channel::Stable,
            &|_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(again, installed);
}

/// 下到的包与锁定的哈希不符：报出、删掉，不解开。
#[tokio::test(flavor = "multi_thread")]
async fn a_package_that_does_not_match_its_pin_is_refused() {
    let root = test_dir("mismatch");
    let (files, python, deno) = files(&["2026.10.26"]);
    let base = start(files).await;
    let mut pins = pins(&base, &python, &deno);
    pins.deno.sha256 = "0".repeat(64);
    let components = components(&root, pins, sources(&base));
    let error = components
        .install(
            &Proxy::Direct,
            Channel::Stable,
            &|_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Mismatch { .. }), "{error}");
    assert!(!root.join("deno-2.9.8").exists());
    assert!(!root.join("downloads/deno-2.9.8").exists());
    assert_eq!(components.installed().unwrap(), None);
}

/// 更新到新版：装进另一个目录、自检过了才换成当前的，上一个留着，更早的删掉；要装的版本索引里没有时报出、在用的不变。
#[tokio::test(flavor = "multi_thread")]
async fn updates_switch_only_after_the_new_version_passes() {
    let root = test_dir("update");
    let (files, python, deno) = files(&["2026.10.21"]);
    let base = start(files.clone()).await;
    let components = components(&root, pins(&base, &python, &deno), sources(&base));
    let first = components
        .install(
            &Proxy::Direct,
            Channel::Stable,
            &|_| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap();
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
    assert!(matches!(missing, Error::Pip(_)), "{missing}");
    assert_eq!(components.installed().unwrap(), Some(first.clone()));

    for version in ["2026.10.26", "2026.11.16.232655.dev0"] {
        let (name, bytes) = wheel(version);
        files.lock().unwrap().insert(format!("files/{name}"), bytes);
        let updated = components
            .update(&Proxy::Direct, version, &|_| {}, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(updated.version, version);
        assert_eq!(components.installed().unwrap(), Some(updated));
    }
    assert!(root.join("yt-dlp-2026.10.26").exists(), "上一个留着");
    assert!(!root.join("yt-dlp-2026.10.21").exists(), "更早的删掉");
}

/// 查最新版本：稳定版与每日构建各按 PyPI 的 JSON 接口。
#[tokio::test(flavor = "multi_thread")]
async fn the_latest_version_comes_from_the_releases_api() {
    let root = test_dir("latest");
    let (files, python, deno) = files(&[]);
    let body = serde_json::json!({
        "info": { "version": "2026.10.26" },
        "releases": {
            "2026.10.26": [{ "upload_time_iso_8601": "2026-10-26T22:25:00.000000Z", "yanked": false }],
            "2026.11.16.232655.dev0": [{ "upload_time_iso_8601": "2026-11-16T23:30:00.000000Z", "yanked": false }]
        }
    });
    files
        .lock()
        .unwrap()
        .insert("pypi/yt-dlp/json".to_owned(), body.to_string().into_bytes());
    let base = start(files).await;
    let components = components(&root, pins(&base, &python, &deno), sources(&base));
    assert_eq!(
        components
            .latest(&Proxy::Direct, Channel::Stable)
            .await
            .unwrap(),
        "2026.10.26"
    );
    assert_eq!(
        components
            .latest(&Proxy::Direct, Channel::Nightly)
            .await
            .unwrap(),
        "2026.11.16.232655.dev0"
    );
}
