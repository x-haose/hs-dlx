//! 常驻解析进程，对着本地起的网页：Python 与 yt-dlp 用仓库开发环境（.venv，`make check` 先 `uv sync` 建好）里的，
//! 驱动脚本用仓库里的这份，只走通用解析器，不连外网。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::response::{Html, IntoResponse as _, Response};
use axum::routing::get;
use hs_dlx_ytdlp::{Cookie, ExtractError, Extractor, Installed, Protocol, Request, Track};
use hs_m3u8_core::{CancellationToken, Proxy, Url};
use tokio::sync::Notify;

fn venv() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.venv")
}

/// 开发环境的 Python 与 site-packages（yt-dlp 装在里面）。
fn installed(name: &str) -> (Installed, PathBuf) {
    let venv = venv();
    let (python, site) = if cfg!(windows) {
        (
            venv.join("Scripts/python.exe"),
            venv.join("Lib/site-packages"),
        )
    } else {
        let lib = venv.join("lib");
        let version = std::fs::read_dir(&lib)
            .unwrap_or_else(|error| panic!("缺 {}（先运行 uv sync）：{error}", lib.display()))
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("python3")
            })
            .expect("venv 里有 python3.x 目录");
        (venv.join("bin/python"), version.join("site-packages"))
    };
    assert!(python.exists(), "缺 {}：先运行 uv sync", python.display());
    let cache = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("ytdlp-extractor")
        .join(name);
    if cache.exists() {
        std::fs::remove_dir_all(&cache).unwrap();
    }
    std::fs::create_dir_all(&cache).unwrap();
    (
        Installed {
            python,
            // 通用解析器不用 JavaScript 运行时
            deno: PathBuf::from("deno"),
            site,
            version: "dev".to_owned(),
            previous: None,
            driver: Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver.py"),
        },
        cache,
    )
}

/// 本地网页：`/video` 嵌了 `/clip.mp4`；`/hang` 收到请求后一直不回（`hit` 通知收到了）。
async fn start(hit: Arc<Notify>, hangs: Arc<AtomicUsize>) -> Url {
    let app = axum::Router::new()
        .route(
            "/video",
            get(|| async {
                Html("<html><head><title>本地视频</title></head><body><video src=\"/clip.mp4\"></video></body></html>")
            }),
        )
        .route(
            "/hang",
            get(move || {
                let (hit, hangs) = (hit.clone(), hangs.clone());
                async move {
                    hangs.fetch_add(1, Ordering::SeqCst);
                    hit.notify_one();
                    std::future::pending::<Response>().await
                }
            }),
        )
        .route("/clip.mp4", get(|| async { vec![0u8; 64].into_response() }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn request(base: &Url, path: &str) -> Request {
    Request {
        url: base.join(path).unwrap(),
        cookies: Vec::new(),
        generic: true,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_is_extracted_with_its_formats_and_cookies() {
    let (installed, cache) = installed("formats");
    let base = start(Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0))).await;
    let extractor = Extractor::new(installed, cache, Duration::from_secs(60));
    let mut request = request(&base, "video");
    request.cookies.push(Cookie {
        domain: "127.0.0.1".to_owned(),
        include_subdomains: false,
        path: "/".to_owned(),
        secure: false,
        expires: None,
        name: "session".to_owned(),
        value: "abc".to_owned(),
    });
    let info = extractor
        .extract(&request, &Proxy::Direct, &CancellationToken::new())
        .await
        .unwrap();
    let [format] = &info.formats[..] else {
        panic!("一个格式：{:?}", info.formats);
    };
    assert_eq!(format.url, base.join("clip.mp4").unwrap().as_str());
    assert_eq!(format.protocol, Protocol::Http);
    assert!(format.video.maybe());
    assert_ne!(format.audio, Track::Absent);
    assert_eq!(format.cookie.as_deref(), Some("session=abc"));
    assert_eq!(info.entries, None);
    // 进程用完放回：再解析一次照常
    let again = extractor
        .extract(&request, &Proxy::Direct, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(again.formats, info.formats);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_generic_extractor_knowing_a_url_is_unsupported_unless_asked() {
    let (installed, cache) = installed("unsupported");
    let extractor = Extractor::new(installed, cache, Duration::from_secs(60));
    // 没有服务器：不发请求即回
    let url = Url::parse("http://127.0.0.1:9/page").unwrap();
    let error = extractor
        .extract(
            &Request {
                url,
                cookies: Vec::new(),
                generic: false,
            },
            &Proxy::Direct,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ExtractError::Unsupported(_)), "{error}");
}

/// 超时测试的时限（秒）：要比起进程、导入 yt-dlp 再发出请求（测试并行、机器忙时几秒）长，超时才落在挂着的请求上。
const TIMEOUT: u64 = 20;

/// 超过时限：杀掉进程报超时，下一次另起进程照常解析。
#[tokio::test(flavor = "multi_thread")]
async fn a_hung_extraction_times_out_and_the_next_one_still_runs() {
    let (installed, cache) = installed("timeout");
    let (hit, hangs) = (Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0)));
    let base = start(hit.clone(), hangs.clone()).await;
    let extractor = Extractor::new(installed, cache, Duration::from_secs(TIMEOUT));
    let error = extractor
        .extract(
            &request(&base, "hang"),
            &Proxy::Direct,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, ExtractError::TimedOut(TIMEOUT));
    assert_eq!(hangs.load(Ordering::SeqCst), 1);
    extractor
        .extract(
            &request(&base, "video"),
            &Proxy::Direct,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
}

/// 取消：等到请求发到网页上再取消，立即返回。
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_returns_at_once() {
    let (installed, cache) = installed("cancel");
    let (hit, hangs) = (Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0)));
    let base = start(hit.clone(), hangs).await;
    let extractor = Extractor::new(installed, cache, Duration::from_secs(60));
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        hit.notified().await;
        canceller.cancel();
    });
    let error = extractor
        .extract(&request(&base, "hang"), &Proxy::Direct, &cancel)
        .await
        .unwrap_err();
    assert_eq!(error, ExtractError::Cancelled);
}

/// 多个同时来的解析共用至多两个进程，各自拿到自己的结果。
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_extractions_share_the_workers() {
    let (installed, cache) = installed("concurrent");
    let base = start(Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0))).await;
    let extractor = Arc::new(Extractor::new(installed, cache, Duration::from_secs(60)));
    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let (extractor, request) = (extractor.clone(), request(&base, "video"));
            tokio::spawn(async move {
                extractor
                    .extract(&request, &Proxy::Direct, &CancellationToken::new())
                    .await
            })
        })
        .collect();
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap().formats.len(), 1);
    }
}

/// 交回 yt-dlp 自己下：先解析出格式，再让 yt-dlp 把认出的那个下到给的路径（目录里的 `$HOME` 原样），报进度。
#[tokio::test(flavor = "multi_thread")]
async fn yt_dlp_downloads_a_format_itself() {
    let (installed, cache) = installed("download");
    let base = start(Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0))).await;
    let extractor = Extractor::new(installed, cache.clone(), Duration::from_secs(60));
    let request = request(&base, "video");
    let info = extractor
        .extract(&request, &Proxy::Direct, &CancellationToken::new())
        .await
        .unwrap();
    let output = cache.join("下载 $HOME 100%").join("clip.mp4");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    let seen = std::sync::Mutex::new(Vec::new());
    extractor
        .download(
            &request,
            &info.formats[0].key(),
            &output,
            &Proxy::Direct,
            &|bytes, total| seen.lock().unwrap().push((bytes, total)),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), vec![0u8; 64]);
    assert!(!seen.into_inner().unwrap().is_empty(), "应有进度");
    // 认不出的格式（编号相同、扩展名不同）：报格式变了，不留文件
    let missing = cache.join("missing.mp4");
    let mut other = info.formats[0].key();
    other.ext = Some("webm".to_owned());
    let error = extractor
        .download(
            &request,
            &other,
            &missing,
            &Proxy::Direct,
            &|_, _| {},
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ExtractError::Changed(_)), "{error}");
    assert!(!missing.exists());
}
