//! 网页视频接进服务：组件目录里摆好「装好的」组件（Python 与 yt-dlp 链到仓库开发环境 .venv 里的），服务一开就认得、
//! 能解析；没装时解析报还没装。网页视频任务对着本地网页（`<video>` 里两个 `<source>`，yt-dlp 的通用解析器给出编号为
//! `0`、`1` 的两个格式）跑完整流程。组件的下载与安装见 crates/ytdlp 的测试。符号链接只在类 Unix 系统上建。
#![cfg(unix)]

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hs_dlx_app::web::Container;
use hs_dlx_app::ytdlp::{Bundle, Bundled, ExtractError, FormatKey, Sources, Track};
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

/// 随应用带的运行时（内容用不到：组件已摆好，见 [`lay_out`]）。
fn bundle() -> Bundle {
    let bundled = |executable: &str| Bundled {
        id: "test".to_owned(),
        bytes: Vec::new().into(),
        executable: executable.to_owned(),
    };
    Bundle {
        python: bundled("python/bin/python3"),
        python_abi: "test".to_owned(),
        quickjs: bundled("qjs"),
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
    std::fs::create_dir_all(root.join("quickjs-test")).unwrap();
    std::fs::write(root.join("quickjs-test/qjs"), "").unwrap();
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
            bundle: Some(bundle()),
            sources: Sources::official(),
            update_every: None,
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
        .resolve(Url::parse("http://127.0.0.1:9/page").unwrap(), &[])
        .unwrap();
    let result = runtime.block_on(handle).unwrap();
    assert!(
        matches!(result, Err(ExtractError::Unsupported(_))),
        "{result:?}"
    );
}

/// 解析进程起不来（Python 还在而运行不了：被隔离、没了执行权限）：报组件坏了，状态随之记为坏了、不再拿它解析
/// （要重装）。
#[test]
fn components_that_cannot_start_are_marked_broken() {
    let dir = test_dir("broken");
    let root = dir.join("components");
    lay_out(&root);
    let python = root.join("python-test/python/bin/python3");
    std::fs::remove_file(&python).unwrap();
    std::fs::write(&python, b"").unwrap();
    let service = service(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let handle = service
        .resolve(
            Url::parse("https://www.youtube.com/watch?v=x").unwrap(),
            &[],
        )
        .unwrap();
    let result = runtime.block_on(handle).unwrap();
    assert!(matches!(result, Err(ExtractError::Broken(_))), "{result:?}");
    assert!(matches!(*service.web_state().borrow(), WebState::Broken(_)));
    let refused = service.resolve(
        Url::parse("https://www.youtube.com/watch?v=x").unwrap(),
        &[],
    );
    assert!(matches!(refused, Err(Error::WebNotReady)));
}

#[test]
fn resolving_before_the_components_are_installed_is_refused() {
    let dir = test_dir("missing");
    let service = service(&dir, dir.join("components"));
    assert_eq!(*service.web_state().borrow(), WebState::Missing);
    let refused = service.resolve(
        Url::parse("https://www.youtube.com/watch?v=x").unwrap(),
        &[],
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
    start_page_refusing(0).await.0
}

/// 同 [`start_page`]，`/a.ts` 的前 `refusals` 次请求回 403（直链过期的样子）；另给网页被请求的次数。
async fn start_page_refusing(refusals: usize) -> (Url, Arc<AtomicUsize>) {
    use axum::http::StatusCode;
    use axum::response::{Html, IntoResponse as _};
    use axum::routing::get;
    let (a, b) = samples();
    let (pages, refused) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let seen = pages.clone();
    let app = axum::Router::new()
        .route(
            "/page",
            get(move || async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Html(
                    "<html><head><title>两路</title></head><body><video>\
                     <source src=\"/a.ts\" type=\"video/mp2t\"><source src=\"/b.ts\" type=\"video/mp2t\">\
                     </video></body></html>",
                )
            }),
        )
        .route(
            "/a.ts",
            get(move || async move {
                if refused.fetch_add(1, Ordering::SeqCst) < refusals {
                    StatusCode::FORBIDDEN.into_response()
                } else {
                    a.clone().into_response()
                }
            }),
        )
        .route("/b.ts", get(move || async move { b.clone().into_response() }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, pages)
}

/// 本地网页上编号为 `id` 的格式（通用解析器只给扩展名）。
fn key(id: &str) -> FormatKey {
    FormatKey {
        id: id.to_owned(),
        ext: Some("ts".to_owned()),
        video: Track::Unknown,
        audio: Track::Unknown,
        height: None,
        language: None,
        size: None,
    }
}

fn web_draft(
    base: &Url,
    folder: PathBuf,
    video: Option<FormatKey>,
    audio: Option<FormatKey>,
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
            video,
            audio,
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
                Some(key("0")),
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

/// 只要一路、要转封装（交回 yt-dlp 下的 HLS 写出的是 TS）：下完转封装成 MP4。
#[test]
fn a_single_format_is_repacked_when_asked() {
    let dir = test_dir("repack");
    let root = dir.join("components");
    lay_out(&root);
    let (service, mut events) = with_events(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let base = runtime.block_on(start_page());
    let folder = dir.join("out");
    let draft = web_draft(
        &base,
        folder.clone(),
        Some(key("0")),
        None,
        Container::Mp4,
        "mp4",
    );
    let task = runtime.block_on(service.create(draft, 1)).unwrap().unwrap();
    assert_eq!(runtime.block_on(ended(&mut events, task.id)), Status::Done);
    let output = std::fs::read(folder.join("两路.mp4")).unwrap();
    assert_eq!(&output[4..8], b"ftyp");
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
                Some(key("0")),
                Some(key("1")),
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

/// 直链被拒（过期、与出口绑定）：重新解析换新的再下，不以 403 停下；一直被拒时以直链被拒停下（可继续）。
#[test]
fn a_refused_link_is_resolved_again() {
    let dir = test_dir("relink");
    let root = dir.join("components");
    lay_out(&root);
    let (service, mut events) = with_events(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for (name, refusals, pages) in [("once", 1, 2), ("always", usize::MAX, 4)] {
        let (base, seen) = runtime.block_on(start_page_refusing(refusals));
        let folder = dir.join(name);
        let task = runtime
            .block_on(service.create(
                web_draft(
                    &base,
                    folder.clone(),
                    Some(key("0")),
                    None,
                    Container::Original,
                    "ts",
                ),
                1,
            ))
            .unwrap()
            .unwrap();
        let status = runtime.block_on(ended(&mut events, task.id));
        assert_eq!(seen.load(Ordering::SeqCst), pages, "{name}");
        match (name, status) {
            ("once", Status::Done) => {
                assert_eq!(std::fs::read(folder.join("两路.ts")).unwrap(), samples().0);
            }
            ("always", Status::Failed(failure)) => {
                assert_eq!(failure.kind, FailureKind::WebDenied);
                assert!(failure.kind.retry_helps());
            }
            (name, other) => panic!("{name}：{other:?}"),
        }
    }
}

/// 认不出选中的格式：编号没了，或编号还在、属性不同（编号移了位）：以格式变了停下，不拿别的格式顶上。
#[test]
fn a_format_not_recognized_fails_the_task_instead_of_taking_another() {
    let dir = test_dir("changed");
    let root = dir.join("components");
    lay_out(&root);
    let (service, mut events) = with_events(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let base = runtime.block_on(start_page());
    let mut shifted = key("0");
    shifted.language = Some("ja".to_owned());
    for (name, format) in [("gone", key("9")), ("shifted", shifted)] {
        let task = runtime
            .block_on(service.create(
                web_draft(
                    &base,
                    dir.join(name),
                    Some(format),
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
                assert_eq!(failure.kind, FailureKind::WebChanged, "{name}");
                assert!(failure.kind.retry_helps());
            }
            other => panic!("{name} 应以格式变了停下：{other:?}"),
        }
        assert!(!dir.join(name).join("两路.ts").exists());
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
        Some(key("0")),
        None,
        Container::Original,
        "ts",
    );
    let mut tabbed = draft.clone();
    draft.headers.push(("Referer".to_owned(), "x".to_owned()));
    let refused = runtime.block_on(service.create(draft, 1)).unwrap();
    assert!(matches!(refused, Err(Error::InvalidInput(_))));
    // Cookie 的值同换请求头时一样校验（制表符会让 cookies.txt 错列）
    tabbed
        .headers
        .push(("Cookie".to_owned(), "a=b\tc".to_owned()));
    let refused = runtime.block_on(service.create(tabbed, 1)).unwrap();
    assert!(matches!(refused, Err(Error::InvalidInput(_))));
}

/// 扩展名拼进文件名：输出的与各路的只能是字母与数字。
#[test]
fn web_extensions_other_than_letters_and_digits_are_refused() {
    let dir = test_dir("ext");
    let root = dir.join("components");
    lay_out(&root);
    let service = service(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let base = Url::parse("http://127.0.0.1:9/").unwrap();
    let mut traversal = key("0");
    traversal.ext = Some("../x".to_owned());
    for (video, ext) in [(key("0"), "../ts"), (key("0"), ""), (traversal, "ts")] {
        let draft = web_draft(
            &base,
            dir.join("out"),
            Some(video),
            None,
            Container::Original,
            ext,
        );
        let refused = runtime.block_on(service.create(draft, 1)).unwrap();
        assert!(matches!(refused, Err(Error::InvalidInput(_))), "{ext}");
    }
}

/// 改用 MKV：停着的、合并成 MP4 的任务容器与输出的扩展名随之改；只有一路的不合并，拒绝。
#[test]
fn a_stopped_merging_task_switches_to_matroska() {
    let dir = test_dir("matroska");
    let root = dir.join("components");
    lay_out(&root);
    let (service, mut events) = with_events(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // 连不上：以网络失败停下
    let base = Url::parse("http://127.0.0.1:9/").unwrap();
    let mut ids = Vec::new();
    for (video, audio, container, ext) in [
        (Some(key("0")), Some(key("1")), Container::Mp4, "mp4"),
        (Some(key("0")), None, Container::Original, "ts"),
    ] {
        let draft = web_draft(&base, dir.join("out"), video, audio, container, ext);
        let task = runtime.block_on(service.create(draft, 1)).unwrap().unwrap();
        assert!(matches!(
            runtime.block_on(ended(&mut events, task.id)),
            Status::Failed(_)
        ));
        ids.push(task.id);
    }
    let switched = service.switch_web_to_matroska(ids[0]).unwrap();
    let web = switched.spec.web().unwrap();
    assert_eq!(web.container, Container::Matroska);
    assert_eq!(web.path, dir.join("out").join("两路.mkv"));
    assert_eq!(service.task(ids[0]).unwrap().spec, switched.spec);
    assert!(matches!(
        service.switch_web_to_matroska(ids[1]),
        Err(Error::InvalidInput(_))
    ));
}

/// 停着的任务已下的：下完的一路按文件，没下完的按 yt-dlp 的 `.part`；不是网页视频任务的报错。
#[test]
fn a_stopped_task_reports_what_it_has_saved() {
    let dir = test_dir("saved");
    let root = dir.join("components");
    lay_out(&root);
    let (service, mut events) = with_events(&dir, root);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let base = Url::parse("http://127.0.0.1:9/").unwrap();
    let draft = web_draft(
        &base,
        dir.join("out"),
        Some(key("0")),
        Some(key("1")),
        Container::Mp4,
        "mp4",
    );
    let task = runtime.block_on(service.create(draft, 1)).unwrap().unwrap();
    assert!(matches!(
        runtime.block_on(ended(&mut events, task.id)),
        Status::Failed(_)
    ));
    let work_dir = task.work_dir.clone().unwrap();
    assert_eq!(
        runtime
            .block_on(service.inspect_web(task.id))
            .unwrap()
            .unwrap(),
        0
    );
    std::fs::create_dir_all(&work_dir).unwrap();
    std::fs::write(work_dir.join("video.ts"), vec![0; 100]).unwrap();
    std::fs::write(work_dir.join("audio.ts.part"), vec![0; 30]).unwrap();
    assert_eq!(
        runtime
            .block_on(service.inspect_web(task.id))
            .unwrap()
            .unwrap(),
        130
    );
}
