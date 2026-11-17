//! 应用更新的端到端测试：本机的 HTTP 服务当 GitHub 的 API 与下载地址，经服务查最新版本、下载本平台的安装包并验签名。
//! 签名用的是测试的密钥对（tests/fixtures/update，与发布用的无关），版本为 99.0.0。安装要换掉正在运行的应用，不在
//! 这里测。

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as Segment, State};
use axum::http::StatusCode;
use hs_dlx_app::{Error, Service, Store, TorrentNetwork, UpdateFeed};
use hs_m3u8_core::{Proxy, Url};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/update");
const NAMES: [&str; 2] = ["hs-dlx_99.0.0_aarch64.dmg", "hs-dlx_99.0.0_x64-setup.exe"];

type Files = Arc<HashMap<String, Vec<u8>>>;

async fn file(
    State(files): State<Files>,
    Segment(name): Segment<String>,
) -> Result<Vec<u8>, StatusCode> {
    files.get(&name).cloned().ok_or(StatusCode::NOT_FOUND)
}

/// 起服务：`/latest` 为 v99.0.0 的 Release，两个平台的安装包（`/files/<名>`；`tampered` 时内容改了一个字节）与签名。
fn serve(rt: &tokio::runtime::Runtime, tampered: bool) -> Url {
    let package = std::fs::read(Path::new(FIXTURES).join("package")).unwrap();
    let mut files = HashMap::new();
    for name in NAMES {
        let mut body = package.clone();
        if tampered {
            body[0] ^= 1;
        }
        files.insert(name.to_owned(), body);
        let signature = format!("{name}.minisig");
        files.insert(
            signature.clone(),
            std::fs::read(Path::new(FIXTURES).join(&signature)).unwrap(),
        );
    }
    let files: Files = Arc::new(files);
    rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let assets: Vec<serde_json::Value> = files
            .iter()
            .map(|(name, body)| {
                serde_json::json!({
                    "name": name,
                    "size": body.len(),
                    "browser_download_url": format!("{base}/files/{name}"),
                })
            })
            .collect();
        let latest = serde_json::json!({
            "tag_name": "v99.0.0",
            "html_url": format!("{base}/release"),
            "draft": false,
            "prerelease": false,
            "assets": assets,
        })
        .to_string();
        let app = axum::Router::new()
            .route("/latest", axum::routing::get(move || async move { latest }))
            .route("/files/{name}", axum::routing::get(file))
            .with_state(files);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Url::parse(&format!("{base}/latest")).unwrap()
    })
}

/// 等 `handle` 的结果，最多 30 秒。
fn wait<T: Send + 'static>(
    rt: &tokio::runtime::Runtime,
    handle: tokio_util::task::AbortOnDropHandle<T>,
) -> T {
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(30), handle).await })
        .expect("30 秒内没做完")
        .unwrap()
}

/// `latest` 上的更新，下载到 `dir`。
fn feed(latest: Url, dir: &Path) -> UpdateFeed {
    let key = std::fs::read_to_string(Path::new(FIXTURES).join("test.pub")).unwrap();
    UpdateFeed {
        latest,
        public_key: key.lines().nth(1).unwrap().to_owned(),
        dir: dir.to_path_buf(),
    }
}

/// 查到新版本、下载并验过签名；内容被改过的不认，文件删掉。不能自动装的平台（Linux）只给下载页。
#[test]
fn updates_are_found_downloaded_and_verified() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("app-update");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    let (service, _events) = Service::new(
        Store::open(&dir.join("tasks.sqlite")).unwrap(),
        NonZeroUsize::new(8).unwrap(),
        TorrentNetwork::Loopback {
            public_trackers: Vec::new(),
        },
        hs_dlx_app::pikpak::Endpoints::official(),
        // 网页视频不在这里测：不给组件
        hs_dlx_app::WebConfig {
            root: std::env::temp_dir().join("hs-dlx-no-web"),
            bundle: None,
            sources: hs_dlx_app::ytdlp::Sources::official(),
        },
    )
    .unwrap();
    // 系统代理可能不放过回环地址
    service.set_proxy(&Proxy::Direct).unwrap();

    let downloads = dir.join("downloads");
    let good = feed(serve(&rt, false), &downloads);
    let release = wait(&rt, service.check_update(&good).unwrap())
        .unwrap()
        .expect("99.0.0 比现在的新");
    assert_eq!(release.version.to_string(), "99.0.0");
    let Some(package) = &release.package else {
        assert!(matches!(
            service.download_update(&good, &release),
            Err(Error::Update(_))
        ));
        return;
    };
    assert!(NAMES.contains(&package.name.as_str()));
    let download = service.download_update(&good, &release).unwrap();
    let downloaded = wait(&rt, download.done).unwrap();
    assert_eq!(downloaded.version, release.version);
    assert_eq!(*download.received.borrow(), package.size);

    let bad = feed(serve(&rt, true), &downloads);
    let release = wait(&rt, service.check_update(&bad).unwrap())
        .unwrap()
        .unwrap();
    let download = service.download_update(&bad, &release).unwrap();
    let error = wait(&rt, download.done).unwrap_err();
    assert!(error.to_string().contains("与签名不符"), "{error}");
    let left = downloads.join(release.package.unwrap().name);
    assert!(!left.exists(), "签名不符的文件没删");
    drop(service);
}
