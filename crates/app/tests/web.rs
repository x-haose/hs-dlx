//! 网页视频接进服务：组件目录里摆好「装好的」组件（Python 与 yt-dlp 链到仓库开发环境 .venv 里的），服务一开就认得、
//! 能解析；没装时解析报还没装。网页视频任务对着本地网页（`<video>` 里两个 `<source>`，yt-dlp 的通用解析器给出编号为
//! `0`、`1` 的两个格式）跑完整流程。组件的下载与安装见 crates/ytdlp 的测试。符号链接只在类 Unix 系统上建。
#![cfg(unix)]

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use hs_dlx_app::web::Container;
use hs_dlx_app::web::Preference;
use hs_dlx_app::ytdlp::{Archive, ExtractError, Pinned, Pins, Sources};
use hs_dlx_app::{
    Draft, DraftKind, Error, Event, FailureKind, Service, Status, Store, TaskId, TorrentNetwork,
    WebConfig, WebState,
};
use hs_m3u8_core::Url;
use tokio::sync::mpsc;

fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("web")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pins() -> Pins {
    let pinned = |executable: &str| Pinned {
        version: "test".to_owned(),
        url: "https://example.invalid/package".to_owned(),
        sha256: "0".repeat(64),
        archive: Archive::TarGz,
        executable: executable.to_owned(),
    };
    Pins {
        python: pinned("python/bin/python3"),
        deno: pinned("deno"),
    }
}

/// 在 `root` 下摆好装好的组件：Python 与 yt-dlp 链到开发环境的。
fn lay_out(root: &Path) {
    let venv = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.venv");
    let python = venv
        .join("bin/python")
        .canonicalize()
        .expect("先运行 uv sync");
    let site = std::fs::read_dir(venv.join("lib"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("python3")
        })
        .unwrap()
        .join("site-packages");
    std::fs::create_dir_all(root.join("python-test/python/bin")).unwrap();
    std::os::unix::fs::symlink(python, root.join("python-test/python/bin/python3")).unwrap();
    std::fs::create_dir_all(root.join("deno-test")).unwrap();
    std::fs::write(root.join("deno-test/deno"), "").unwrap();
    std::os::unix::fs::symlink(site, root.join("yt-dlp-dev")).unwrap();
    std::fs::write(
        root.join("state.json"),
        r#"{"yt_dlp":"dev","previous":null,"python":"test"}"#,
    )
    .unwrap();
}

fn service(dir: &Path, root: PathBuf) -> Service {
    with_events(dir, root).0
}

fn with_events(dir: &Path, root: PathBuf) -> (Service, mpsc::UnboundedReceiver<Event>) {
    let (service, events) = Service::new(
        Store::open(&dir.join("tasks.sqlite")).unwrap(),
        NonZeroUsize::new(4).unwrap(),
        TorrentNetwork::Loopback {
            public_trackers: Vec::new(),
        },
        hs_dlx_app::pikpak::Endpoints::official(),
        WebConfig {
            root,
            pins: Some(pins()),
            sources: Sources::official(),
        },
    )
    .unwrap();
    service.set_proxy(&hs_m3u8_core::Proxy::Direct).unwrap();
    (service, events)
}

#[test]
fn installed_components_are_ready_and_resolve_through_the_driver() {
    let dir = test_dir("ready");
    let root = dir.join("components");
    lay_out(&root);
    let service = service(&dir, root);
    assert_eq!(
        *service.web_state().borrow(),
        WebState::Ready("dev".to_owned())
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // 只有通用解析器认得的地址：驱动脚本不发请求即回不支持（转嗅探页）
    let handle = service
        .resolve(
            Url::parse("http://127.0.0.1:9/page").unwrap(),
            Vec::new(),
            Preference::Compatible,
        )
        .unwrap();
    let result = runtime.block_on(handle).unwrap();
    assert!(
        matches!(result, Err(ExtractError::Unsupported(_))),
        "{result:?}"
    );
}

#[test]
fn resolving_before_the_components_are_installed_is_refused() {
    let dir = test_dir("missing");
    let service = service(&dir, dir.join("components"));
    assert_eq!(*service.web_state().borrow(), WebState::Missing);
    let refused = service.resolve(
        Url::parse("https://www.youtube.com/watch?v=x").unwrap(),
        Vec::new(),
        Preference::Compatible,
    );
    assert!(matches!(refused, Err(Error::WebNotReady)));
}

/// 两路的样本：媒体测试样本里的两个 TS 分片（都有视频与音频）。
fn samples() -> (Vec<u8>, Vec<u8>) {
    let media = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/media/ts_a");
    (
        std::fs::read(media.join("seg0.ts")).unwrap(),
        std::fs::read(media.join("seg1.ts")).unwrap(),
    )
}

/// 本地网页 `/page`：`<video>` 里两个 `<source>`（`/a.ts`、`/b.ts`）。
async fn start_page() -> Url {
    use axum::response::{Html, IntoResponse as _};
    use axum::routing::get;
    let (a, b) = samples();
    let app = axum::Router::new()
        .route(
            "/page",
            get(|| async {
                Html(
                    "<html><head><title>两路</title></head><body><video>\
                     <source src=\"/a.ts\" type=\"video/mp2t\"><source src=\"/b.ts\" type=\"video/mp2t\">\
                     </video></body></html>",
                )
            }),
        )
        .route("/a.ts", get(move || async move { a.clone().into_response() }))
        .route("/b.ts", get(move || async move { b.clone().into_response() }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn web_draft(
    base: &Url,
    folder: PathBuf,
    video: Option<&str>,
    audio: Option<&str>,
    container: Container,
    ext: &str,
) -> Draft {
    Draft {
        url: base.join("page").unwrap(),
        headers: Vec::new(),
        folder,
        kind: DraftKind::Web {
            title: "两路".to_owned(),
            extractor: "HTML5MediaEmbed".to_owned(),
            video_id: "page-1".to_owned(),
            video: video.map(str::to_owned),
            audio: audio.map(str::to_owned),
            container,
            ext: ext.to_owned(),
        },
    }
}

async fn ended(events: &mut mpsc::UnboundedReceiver<Event>, id: TaskId) -> Status {
    loop {
        match events.recv().await.expect("服务还在，事件通道不会关闭") {
            Event::Ended {
                id: at,
                status,
                unsaved,
                ..
            } if at == id => {
                assert!(unsaved.is_none(), "没写进存储：{unsaved:?}");
                return status;
            }
            _ => {}
        }
    }
}

/// 只要一路：下完直接改名成输出，内容不动；任务目录随之删掉。
#[test]
fn a_single_format_becomes_the_output() {
    let dir = test_dir("single");
    let root = dir.join("components");
    lay_out(&root);
    let (service, mut events) = with_events(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let base = runtime.block_on(start_page());
    let folder = dir.join("out");
    let task = runtime
        .block_on(service.create(
            web_draft(
                &base,
                folder.clone(),
                Some("0"),
                None,
                Container::Original,
                "ts",
            ),
            1,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(task.spec.title, "两路.ts");
    assert_eq!(runtime.block_on(ended(&mut events, task.id)), Status::Done);
    assert_eq!(std::fs::read(folder.join("两路.ts")).unwrap(), samples().0);
    assert!(!folder.join("两路.ts.hsdl").exists());
}

/// 视频与另配的音频：各下一路再合并成 MP4（视频取第一路的、音频取第二路的）。
#[test]
fn video_and_audio_are_fetched_apart_and_merged() {
    let dir = test_dir("merged");
    let root = dir.join("components");
    lay_out(&root);
    let (service, mut events) = with_events(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let base = runtime.block_on(start_page());
    let folder = dir.join("out");
    let task = runtime
        .block_on(service.create(
            web_draft(
                &base,
                folder.clone(),
                Some("0"),
                Some("1"),
                Container::Mp4,
                "mp4",
            ),
            1,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(runtime.block_on(ended(&mut events, task.id)), Status::Done);
    let output = std::fs::read(folder.join("两路.mp4")).unwrap();
    assert_eq!(&output[4..8], b"ftyp");
    let done = service.task(task.id).unwrap();
    assert!(done.result.unwrap().resolution.is_some());
    assert!(!folder.join("两路.mp4.hsdl").exists());
}

/// 选中的格式网站已经不给了：以视频没了停下（再继续也一样）。
#[test]
fn a_format_that_is_gone_fails_the_task() {
    let dir = test_dir("gone");
    let root = dir.join("components");
    lay_out(&root);
    let (service, mut events) = with_events(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let base = runtime.block_on(start_page());
    let task = runtime
        .block_on(service.create(
            web_draft(
                &base,
                dir.join("out"),
                Some("9"),
                None,
                Container::Original,
                "ts",
            ),
            1,
        ))
        .unwrap()
        .unwrap();
    match runtime.block_on(ended(&mut events, task.id)) {
        Status::Failed(failure) => {
            assert_eq!(failure.kind, FailureKind::WebGone);
            assert!(!failure.kind.retry_helps());
        }
        other => panic!("应以视频没了停下：{other:?}"),
    }
}

/// 网页视频只带 Cookie。
#[test]
fn web_tasks_take_only_cookies() {
    let dir = test_dir("headers");
    let root = dir.join("components");
    lay_out(&root);
    let service = service(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut draft = web_draft(
        &Url::parse("http://127.0.0.1:9/").unwrap(),
        dir.join("out"),
        Some("0"),
        None,
        Container::Original,
        "ts",
    );
    draft.headers.push(("Referer".to_owned(), "x".to_owned()));
    let refused = runtime.block_on(service.create(draft, 1)).unwrap();
    assert!(matches!(refused, Err(Error::InvalidInput(_))));
}
