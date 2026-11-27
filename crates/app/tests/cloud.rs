//! 云离线任务的端到端测试：真实的 SQLite 文件、服务与核心的分段下载，对着测试内的本地模拟 PikPak（设备授权登录、离线
//! 任务、文件夹、带签名的直链；直链支持范围请求，一般带 ETag）。服务自带 tokio 运行时，在测试自己的运行时外面建、外面
//! 丢弃；等事件与本地服务在测试的运行时里。

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hs_dlx_app::pikpak::gcid::Gcid;
use hs_dlx_app::{
    CloudProgress, CloudProvider, Error, Event, FailureKind, Kind, Service, Status, Step, Store,
    Task, TaskId,
};
use hs_m3u8_core::Url;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc};

const MAGNET: &str = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Show";

/// 网盘上的一个文件。
struct Remote {
    id: &'static str,
    name: &'static str,
    body: Vec<u8>,
    /// 网盘给的 GCID；None 时照内容算
    gcid: Option<&'static str>,
}

/// 模拟接口的状态。
struct Fake {
    /// 离线任务下到的：文件夹 Show（a.bin 与子文件夹 Sub/b.bin）
    files: Vec<Remote>,
    /// 各文件的直链被取了几次
    links: HashMap<String, u32>,
    /// 这些文件的直链第一次下载时回 403（过期）
    expire_once: Vec<&'static str>,
    /// 下载 `held` 这个文件时挂着，直到打开
    hold_b: Arc<Gate>,
    /// 收到下载 `held` 的请求时通知
    b_requested: Arc<Notify>,
    /// 挂着的是哪个文件，默认 b.bin
    held: &'static str,
    /// 提交离线下载的次数
    submits: u32,
    /// 提交时照常建任务，但响应体发到一半断开
    break_submit: bool,
    /// 离线任务列表里的任务编号，新的在前；提交一次加一个 `VTask<次数>`
    jobs: Vec<String>,
    /// 登录得到的用户编号
    user: &'static str,
    /// b.bin 的直链不带 ETag、每次最多给 64 KiB，第一个直链的第二个请求回 403
    b_unverifiable: bool,
    /// 下载 b.bin 的各个请求：（第几个直链，Range 的起点）
    b_requests: Vec<(String, u64)>,
    /// b.bin 在网盘上还没处理完（没有 GCID）
    b_incomplete: bool,
    /// b.bin 在回收站里
    b_trashed: bool,
    /// 离线任务列表是空的（任务被删了，或刚提交的还没列出来）
    jobs_hidden: bool,
    /// 离线任务列表里的任务都是失败的
    jobs_failed: bool,
    /// 子文件夹 Sub 在回收站里
    sub_trashed: bool,
    /// b.bin 被彻底删了：取它回 404，Sub 里也列不出它
    b_gone: bool,
    /// 下到的整个文件夹 Show 被彻底删了：取 Show 与 b.bin 都回 404
    show_gone: bool,
    /// b.bin 的直链每次最多给 64 KiB（带 ETag）
    b_chunked: bool,
    /// 只挂着 `held` 从中间开始的请求（第一个请求照常回，任务目录里已有记录）
    hold_tail: bool,
    /// 直链不随取的次数变（同一个签名地址）
    stable_links: bool,
    /// b.bin 的 ETag 后缀：换了即同一直链给出了另一个验证器（落到了另一个 CDN 节点）
    b_etag: &'static str,
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    opened: Notify,
}

impl Gate {
    async fn pass(&self) {
        loop {
            let opened = self.opened.notified();
            if *self.open.lock().unwrap() {
                return;
            }
            opened.await;
        }
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_waiters();
    }
}

type Shared = Arc<Mutex<Fake>>;

fn body(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8)
        .collect()
}

fn gcid(body: &[u8]) -> String {
    let mut gcid = Gcid::new(body.len() as u64);
    gcid.update(body);
    gcid.finish()
}

async fn device_code() -> Response {
    Json(json!({ "device_code": "dc", "user_code": "ABCD", "expires_in": 300, "interval": 1 }))
        .into_response()
}

async fn auth_token(State(fake): State<Shared>, Json(body): Json<Value>) -> Response {
    assert_ne!(body["grant_type"], "refresh_token", "测试里令牌不会过期");
    let user = fake.lock().unwrap().user;
    Json(json!({
        "access_token": "a1", "refresh_token": "r1", "expires_in": 7200, "sub": user,
    }))
    .into_response()
}

async fn captcha_init() -> Response {
    Json(json!({ "captcha_token": "c1", "expires_in": 300 })).into_response()
}

fn authorized(headers: &HeaderMap) -> bool {
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer a1")
}

fn task_body(id: &str, failed: bool) -> Value {
    let (phase, message) = if failed {
        ("PHASE_TYPE_ERROR", "没有可用的连接")
    } else {
        ("PHASE_TYPE_COMPLETE", "")
    };
    json!({
        "kind": "drive#task", "id": id, "name": "Show", "phase": phase, "message": message,
        "progress": 100, "file_id": "VShow", "file_size": "0", "params": { "url": MAGNET },
    })
}

async fn submit(
    State(fake): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert!(authorized(&headers));
    assert_eq!(body["url"]["url"], MAGNET);
    let mut fake = fake.lock().unwrap();
    fake.submits += 1;
    let id = format!("VTask{}", fake.submits);
    fake.jobs.insert(0, id.clone());
    if fake.break_submit {
        // 声明的长度比发出的长：发完这几个字节连接即断，客户端收到了响应头、没读完响应体
        return ([(header::CONTENT_LENGTH, "1000")], "{\"task\"").into_response();
    }
    Json(json!({ "task": task_body(&id, false) })).into_response()
}

/// 离线任务列表，最近的在前；`filters` 只认按编号筛（`{"id":{"in":"<编号>"}}`），`{}` 为不筛。
async fn tasks(
    State(fake): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    assert!(authorized(&headers));
    let filters: Value = serde_json::from_str(&query["filters"]).unwrap();
    let only = filters["id"]["in"].as_str();
    let fake = fake.lock().unwrap();
    let tasks: Vec<Value> = if fake.jobs_hidden {
        Vec::new()
    } else {
        fake.jobs
            .iter()
            .filter(|id| only.is_none_or(|only| only == id.as_str()))
            .map(|id| task_body(id, fake.jobs_failed))
            .collect()
    };
    Json(json!({ "tasks": tasks })).into_response()
}

fn file_json(fake: &Fake, remote: &Remote, base: &str) -> Value {
    let count = if fake.stable_links {
        0
    } else {
        fake.links.get(remote.id).copied().unwrap_or(0)
    };
    let incomplete = remote.id == "VB" && fake.b_incomplete;
    let (phase, hash) = if incomplete {
        ("PHASE_TYPE_RUNNING", String::new())
    } else {
        (
            "PHASE_TYPE_COMPLETE",
            remote
                .gcid
                .map(str::to_owned)
                .unwrap_or_else(|| gcid(&remote.body))
                .to_uppercase(),
        )
    };
    json!({
        "kind": "drive#file", "id": remote.id, "name": remote.name, "phase": phase,
        "trashed": remote.id == "VB" && fake.b_trashed,
        "size": remote.body.len().to_string(),
        "hash": hash,
        "web_content_link": format!("{base}dl/{}?n={count}", remote.id),
    })
}

fn base(headers: &HeaderMap) -> String {
    format!(
        "http://{}/",
        headers.get(header::HOST).unwrap().to_str().unwrap()
    )
}

async fn children(
    State(fake): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    assert!(authorized(&headers));
    let fake = fake.lock().unwrap();
    let base = base(&headers);
    let files = match query["parent_id"].as_str() {
        "VShow" => vec![
            file_json(&fake, &fake.files[0], &base),
            json!({ "kind": "drive#folder", "id": "VSub", "name": "Sub", "trashed": fake.sub_trashed }),
        ],
        "VSub" if fake.b_gone => Vec::new(),
        "VSub" => vec![file_json(&fake, &fake.files[1], &base)],
        other => panic!("没有这个文件夹：{other}"),
    };
    Json(json!({ "files": files, "next_page_token": "" })).into_response()
}

async fn entry(
    State(fake): State<Shared>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    assert!(authorized(&headers));
    let mut fake = fake.lock().unwrap();
    if (id == "VShow" || id == "VB") && fake.show_gone {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "file_not_found", "error_code": 5, "error_description": "" })),
        )
            .into_response();
    }
    if id == "VShow" {
        return Json(json!({ "kind": "drive#folder", "id": "VShow", "name": "Show" }))
            .into_response();
    }
    if id == "VB" && fake.b_gone {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "file_not_found", "error_code": 5, "error_description": "" })),
        )
            .into_response();
    }
    *fake.links.entry(id.clone()).or_default() += 1;
    let remote = fake.files.iter().find(|remote| remote.id == id).unwrap();
    Json(file_json(&fake, remote, &base(&headers))).into_response()
}

/// 直链：支持 `Range: bytes=a-` 与 `bytes=a-b`，带强 ETag；`expire_once` 里的第一个链接回 403；`b_unverifiable` 时
/// b.bin 见 [`Fake::b_unverifiable`]。
async fn download(
    State(fake): State<Shared>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let range_start = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
        .map_or(0, |(start, _)| start.parse::<u64>().unwrap());
    let (data, hold, unverifiable, chunked) = {
        let mut fake = fake.lock().unwrap();
        if query["n"] == "1" && fake.expire_once.contains(&id.as_str()) {
            return StatusCode::FORBIDDEN.into_response();
        }
        if id == "VB" {
            fake.b_requests.push((query["n"].clone(), range_start));
        }
        let unverifiable = id == "VB" && fake.b_unverifiable;
        if unverifiable && query["n"] == "1" && range_start > 0 {
            return StatusCode::FORBIDDEN.into_response();
        }
        let remote = fake.files.iter().find(|remote| remote.id == id).unwrap();
        let held = id == fake.held && (!fake.hold_tail || range_start > 0);
        if held {
            fake.b_requested.notify_one();
        }
        (
            remote.body.clone(),
            held.then(|| fake.hold_b.clone()),
            unverifiable,
            id == "VB" && fake.b_chunked,
        )
    };
    if let Some(gate) = hold {
        gate.pass().await;
    }
    let len = data.len() as u64;
    let etag = if id == "VB" {
        format!("\"{id}{}\"", fake.lock().unwrap().b_etag)
    } else {
        format!("\"{id}\"")
    };
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'))
        .map(|(start, end)| {
            let start: u64 = start.parse().unwrap();
            let end = if end.is_empty() {
                len - 1
            } else {
                end.parse::<u64>().unwrap().min(len - 1)
            };
            // 每次最多给 64 KiB：客户端从给到的地方接着请求
            let end = if unverifiable || chunked {
                end.min(start + 65_535)
            } else {
                end
            };
            (start, end)
        });
    let mut response = match range {
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}")),
                (header::ACCEPT_RANGES, "bytes".to_owned()),
            ],
            data[start as usize..=end as usize].to_vec(),
        )
            .into_response(),
        None => data.into_response(),
    };
    if !unverifiable {
        response
            .headers_mut()
            .insert(header::ETAG, etag.parse().unwrap());
    }
    response
}

/// 起模拟接口，返回它的地址与状态。
async fn start(files: Vec<Remote>) -> (Url, Shared) {
    let fake = Shared::new(Mutex::new(Fake {
        files,
        links: HashMap::new(),
        expire_once: Vec::new(),
        hold_b: Arc::new(Gate::default()),
        b_requested: Arc::new(Notify::new()),
        submits: 0,
        break_submit: false,
        jobs: Vec::new(),
        user: "user-1",
        b_unverifiable: false,
        b_requests: Vec::new(),
        b_incomplete: false,
        b_trashed: false,
        jobs_hidden: false,
        jobs_failed: false,
        sub_trashed: false,
        b_gone: false,
        show_gone: false,
        b_chunked: false,
        hold_tail: false,
        stable_links: false,
        b_etag: "",
        held: "VB",
    }));
    fake.lock().unwrap().hold_b.open();
    let app = axum::Router::new()
        .route("/v1/auth/device/code", post(device_code))
        .route("/v1/auth/token", post(auth_token))
        .route("/v1/shield/captcha/init", post(captcha_init))
        .route("/drive/v1/files", post(submit).get(children))
        .route("/drive/v1/files/{id}", get(entry))
        .route("/drive/v1/tasks", get(tasks))
        .route("/dl/{id}", get(download))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, fake)
}

fn remotes(b_gcid: Option<&'static str>) -> Vec<Remote> {
    vec![
        Remote {
            id: "VA",
            name: "a.bin",
            body: body(300_007, 1),
            gcid: None,
        },
        Remote {
            id: "VB",
            name: "b.bin",
            body: body(70_001, 2),
            gcid: b_gcid,
        },
    ]
}

fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("cloud")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn service(dir: &Path, base: &Url) -> (Service, mpsc::UnboundedReceiver<Event>) {
    let service = Service::new(
        Store::open(&dir.join("tasks.sqlite")).unwrap(),
        NonZeroUsize::new(8).unwrap(),
        hs_dlx_app::TorrentNetwork::Loopback {
            public_trackers: Vec::new(),
        },
        hs_dlx_app::pikpak::Endpoints::at(base.clone()),
        // 网页视频不在这里测：不给组件
        hs_dlx_app::WebConfig {
            root: std::env::temp_dir().join("hs-dlx-no-web"),
            bundle: None,
            sources: hs_dlx_app::ytdlp::Sources::official(),
            update_every: None,
        },
    )
    .unwrap();
    service.0.set_proxy(&hs_m3u8_core::Proxy::Direct).unwrap();
    service
}

/// 走一遍设备授权登录。
fn log_in(rt: &Runtime, service: &Service) {
    let login = rt.block_on(service.login_pikpak()).unwrap().unwrap();
    assert_eq!(login.user_code, "ABCD");
    rt.block_on(login.done).unwrap().unwrap();
    assert_eq!(service.pikpak_user().unwrap().as_deref(), Some("user-1"));
}

/// 粘贴磁力链接、存到 `folder`，在它取种子信息时交给 PikPak 云离线；返回定下的云离线任务。
fn offload(rt: &Runtime, service: &Service, folder: PathBuf) -> Task {
    let mut events = service.subscribe();
    let mut pasted = rt.block_on(service.paste(MAGNET, &folder, 1)).unwrap();
    let id = match pasted.pop() {
        Some(Ok(task)) if pasted.is_empty() => task.id,
        other => panic!("应建出一个任务：{other:?}"),
    };
    service.offload(id, CloudProvider::PikPak).unwrap();
    rt.block_on(async {
        loop {
            match events.recv().await.expect("服务还在，事件通道不会关闭") {
                Event::Changed { task } if task.id == id && task.spec.pending().is_none() => {
                    return *task;
                }
                Event::Ended { id: at, status, .. } if at == id => {
                    panic!("没定下就停了：{status:?}")
                }
                _ => {}
            }
        }
    })
}

/// 等到任务 `id` 停下、状态写进存储，返回它的状态；中途的云离线进度交给 `on_progress`。
async fn ended(
    events: &mut mpsc::UnboundedReceiver<Event>,
    id: TaskId,
    mut on_progress: impl FnMut(&CloudProgress),
) -> Status {
    loop {
        match events.recv().await.expect("服务还在，事件通道不会关闭") {
            Event::CloudProgress { id: at, progress } if at == id => on_progress(&progress),
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

/// 提交、等网盘下完、列出文件定下根（标题换成根名），逐个下载并校验；直链过期的重取一次。
#[test]
fn a_cloud_task_fetches_and_verifies_every_file() {
    let dir = test_dir("done");
    let folder = dir.join("out");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    fake.lock().unwrap().expire_once = vec!["VA"];
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);

    let task = offload(&rt, &service, folder.clone());
    assert_eq!(task.spec.title, "Show");
    let mut verified = false;
    let status = rt.block_on(ended(&mut events, task.id, |progress| {
        verified |= matches!(progress, CloudProgress::Verifying { .. });
    }));
    assert_eq!(status, Status::Done);
    assert!(verified);
    let root = folder.join("Show");
    let files = remotes(None);
    assert_eq!(std::fs::read(root.join("a.bin")).unwrap(), files[0].body);
    assert_eq!(
        std::fs::read(root.join("Sub").join("b.bin")).unwrap(),
        files[1].body
    );
    // 各文件的任务目录与它们的父目录都已删
    assert!(!folder.join("Show.hsdl").exists());
    let task = service.task(task.id).unwrap();
    let Kind::Cloud(spec) = &task.spec.kind else {
        panic!("还是云离线任务");
    };
    assert_eq!(spec.root.as_deref(), Some(root.as_path()));
    assert_eq!(task.result.unwrap().bytes, 370_008);
    // a.bin 的第一个直链过期，重取了一次
    assert_eq!(fake.lock().unwrap().links["VA"], 2);
}

/// 下完的文件与网盘给的 GCID 不符：删掉它，以校验不符失败；之前下完的照样记着。
#[test]
fn a_file_that_does_not_match_its_gcid_is_removed() {
    let dir = test_dir("mismatch");
    let folder = dir.join("out");
    let rt = runtime();
    let (base, _fake) = rt.block_on(start(remotes(Some(
        "0000000000000000000000000000000000000000",
    ))));
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, folder.clone());
    match rt.block_on(ended(&mut events, task.id, |_| {})) {
        Status::Failed(failure) => assert_eq!(failure.kind, FailureKind::Integrity),
        other => panic!("应以校验不符失败：{other:?}"),
    }
    assert!(folder.join("Show").join("a.bin").exists());
    assert!(!folder.join("Show").join("Sub").join("b.bin").exists());
}

/// 下到一半暂停，再放弃：下了的文件、各文件的任务目录与内容的根都删掉，记录也删掉。
#[test]
fn discarding_a_paused_cloud_task_removes_what_it_fetched() {
    let dir = test_dir("discard");
    let folder = dir.join("out");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    let gate = Arc::new(Gate::default());
    fake.lock().unwrap().hold_b = gate.clone();
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, folder.clone());
    // 等到在下第二个文件（b.bin 挂着）时暂停：挂着的请求收不到响应，进度要到读超时才变，所以等服务器收到请求
    let requested = fake.lock().unwrap().b_requested.clone();
    rt.block_on(requested.notified());
    service.pause(task.id);
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Paused
    );
    assert!(folder.join("Show").join("a.bin").exists());
    rt.block_on(service.discard(task.id)).unwrap().unwrap();
    gate.open();
    assert!(!folder.join("Show").exists());
    assert!(!folder.join("Show.hsdl").exists());
    assert!(matches!(service.task(task.id), Err(Error::NotFound(_))));
}

/// 没登录时交给网盘报没登录，任务照旧在取种子信息；定下了的任务、不是磁力链接的不能交。
#[test]
fn offloading_needs_a_login_and_an_unsettled_magnet() {
    let dir = test_dir("no-login");
    let rt = runtime();
    let (base, _fake) = rt.block_on(start(remotes(None)));
    let (service, _events) = service(&dir, &base);
    let mut pasted = rt
        .block_on(service.paste(MAGNET, &dir.join("out"), 1))
        .unwrap();
    let id = pasted.remove(0).unwrap().id;
    match service.offload(id, CloudProvider::PikPak) {
        Err(Error::NotLoggedIn) => {}
        other => panic!("应报没登录：{other:?}"),
    }
    let task = service.task(id).unwrap();
    assert_eq!(task.status, Status::Preparing);
    assert!(matches!(
        service.stage(id).unwrap(),
        Some(hs_dlx_app::Stage::Working(Step::Metadata(_)))
    ));
    // 同一条磁力链接已在这个任务里
    let again = rt
        .block_on(service.paste(MAGNET, &dir.join("out"), 2))
        .unwrap();
    assert!(
        matches!(again.as_slice(), [Err(rejected)] if matches!(*rejected.reason, Error::TorrentTaken(at) if at == id)),
        "{again:?}"
    );

    log_in(&rt, &service);
    let mut events = service.subscribe();
    let cloud = service.offload(id, CloudProvider::PikPak).unwrap();
    assert_eq!(cloud.status, Status::Preparing);
    rt.block_on(async {
        loop {
            if let Event::Changed { task } = events.recv().await.unwrap()
                && task.id == id
                && task.spec.cloud().is_some()
            {
                break;
            }
        }
    });
    // 定下为云离线任务之后种子不再占着：同一条磁力链接可以另建任务由本机下
    let mut local = rt
        .block_on(service.paste(MAGNET, &dir.join("out"), 3))
        .unwrap();
    let local = local.remove(0).unwrap().id;
    assert!(service.offload(local, CloudProvider::PikPak).is_ok());
    rt.block_on(service.discard(local)).unwrap().unwrap();
    rt.block_on(service.discard(id)).unwrap().unwrap();
    assert!(service.tasks().unwrap().is_empty());
}

/// 提交时收到了响应头、没读完响应体（网盘已建了任务）：在提交前没有的任务里按链接找回它，不重发、也不认领之前提交过
/// 同一链接的旧任务；网盘上只新建了一个。
#[test]
fn a_submit_cut_off_after_the_server_answered_is_found_not_resent() {
    let dir = test_dir("cut-off");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    {
        let mut fake = fake.lock().unwrap();
        fake.break_submit = true;
        fake.jobs = vec!["VTask0".to_owned()];
    }
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, dir.join("out"));
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
    let task = service.task(task.id).unwrap();
    let Kind::Cloud(spec) = &task.spec.kind else {
        panic!("是云离线任务");
    };
    assert_eq!(spec.job.as_deref(), Some("VTask1"));
    assert_eq!(fake.lock().unwrap().submits, 1);
}

/// 重开应用后（登录状态还没建）继续暂停了的云离线任务：照常接着下完。
#[test]
fn a_paused_cloud_task_resumes_after_reopening() {
    let dir = test_dir("reopen");
    let folder = dir.join("out");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    let gate = Arc::new(Gate::default());
    fake.lock().unwrap().hold_b = gate.clone();
    let id = {
        let (service, mut events) = service(&dir, &base);
        log_in(&rt, &service);
        let task = offload(&rt, &service, folder.clone());
        let requested = fake.lock().unwrap().b_requested.clone();
        rt.block_on(requested.notified());
        service.pause(task.id);
        assert_eq!(
            rt.block_on(ended(&mut events, task.id, |_| {})),
            Status::Paused
        );
        task.id
    };
    gate.open();
    let (service, mut events) = service(&dir, &base);
    service.resume(id).unwrap();
    assert_eq!(rt.block_on(ended(&mut events, id, |_| {})), Status::Done);
    assert_eq!(
        std::fs::read(folder.join("Show").join("Sub").join("b.bin")).unwrap(),
        remotes(None)[1].body
    );
}

/// 直链不带 ETag：取回到一半直链被拒，换了直链后凭担保的内容身份接着已下的范围下完，校验通过。
#[test]
fn a_link_without_a_validator_resumes_by_the_vouched_identity() {
    let dir = test_dir("unverifiable");
    let folder = dir.join("out");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    fake.lock().unwrap().b_unverifiable = true;
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, folder.clone());
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
    assert_eq!(
        std::fs::read(folder.join("Show").join("Sub").join("b.bin")).unwrap(),
        remotes(None)[1].body
    );
    let fake = fake.lock().unwrap();
    // 第一个被拒，第二个接着下完
    assert_eq!(fake.links["VB"], 2);
    let resumed: Vec<u64> = fake
        .b_requests
        .iter()
        .filter(|(link, _)| link == "2")
        .map(|&(_, start)| start)
        .collect();
    assert!(
        !resumed.is_empty() && !resumed.contains(&0),
        "第二个直链应从已下的地方接着请求：{:?}",
        fake.b_requests
    );
}

/// 网盘上有文件还没处理完：不取回缺了的内容当作完成，以云端失败报出。
#[test]
fn an_incomplete_cloud_file_fails_the_task() {
    let dir = test_dir("incomplete");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    fake.lock().unwrap().b_incomplete = true;
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, dir.join("out"));
    match rt.block_on(ended(&mut events, task.id, |_| {})) {
        Status::Failed(failure) => {
            assert_eq!(failure.kind, FailureKind::CloudFailed);
            assert!(failure.message.contains("b.bin"), "{}", failure.message);
        }
        other => panic!("应以云端失败停下：{other:?}"),
    }
}

/// 换了账号再继续：以登录失效停下、写明要登录哪个账号，不当成网盘上没有这个任务。
#[test]
fn resuming_under_another_account_asks_for_the_right_one() {
    let dir = test_dir("other-account");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    let gate = Arc::new(Gate::default());
    fake.lock().unwrap().hold_b = gate.clone();
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, dir.join("out"));
    let requested = fake.lock().unwrap().b_requested.clone();
    rt.block_on(requested.notified());
    service.pause(task.id);
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Paused
    );
    gate.open();
    service.logout_pikpak().unwrap();
    assert!(matches!(service.resume(task.id), Err(Error::NotLoggedIn)));
    fake.lock().unwrap().user = "user-2";
    let login = rt.block_on(service.login_pikpak()).unwrap().unwrap();
    rt.block_on(login.done).unwrap().unwrap();
    service.resume(task.id).unwrap();
    match rt.block_on(ended(&mut events, task.id, |_| {})) {
        Status::Failed(failure) => {
            assert_eq!(failure.kind, FailureKind::CloudLogin);
            assert!(failure.message.contains("user-1"), "{}", failure.message);
        }
        other => panic!("应以登录失效停下：{other:?}"),
    }
}

/// 用户在网页里把 b.bin 删进了回收站：跳过它，只取回 a.bin。
#[test]
fn files_in_the_recycle_bin_are_skipped() {
    let dir = test_dir("trashed");
    let folder = dir.join("out");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    fake.lock().unwrap().b_trashed = true;
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, folder.clone());
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
    assert!(folder.join("Show").join("a.bin").exists());
    assert!(!folder.join("Show").join("Sub").exists());
}

/// 网盘上的离线任务不在了：以云端失败停下、清掉编号；继续即重新提交，照常下完。
#[test]
fn a_vanished_cloud_job_is_submitted_again_on_resume() {
    let dir = test_dir("vanished");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    fake.lock().unwrap().jobs_hidden = true;
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, dir.join("out"));
    match rt.block_on(ended(&mut events, task.id, |_| {})) {
        Status::Failed(failure) => assert_eq!(failure.kind, FailureKind::CloudFailed),
        other => panic!("应以云端失败停下：{other:?}"),
    }
    assert_eq!(
        service.task(task.id).unwrap().spec.cloud().unwrap().job,
        None
    );
    fake.lock().unwrap().jobs_hidden = false;
    service.resume(task.id).unwrap();
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
    assert_eq!(fake.lock().unwrap().submits, 2);
}

/// 提交结果不明、当时也没查到新建的任务：记着提交中，继续时先查到它，不再提交第二次。
#[test]
fn an_unclear_submit_is_looked_up_again_before_resubmitting() {
    let dir = test_dir("unclear");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    {
        let mut fake = fake.lock().unwrap();
        fake.break_submit = true;
        fake.jobs_hidden = true;
    }
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, dir.join("out"));
    assert!(matches!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Failed(_)
    ));
    assert!(
        service
            .task(task.id)
            .unwrap()
            .spec
            .cloud()
            .unwrap()
            .before
            .is_some()
    );
    {
        let mut fake = fake.lock().unwrap();
        fake.break_submit = false;
        fake.jobs_hidden = false;
    }
    service.resume(task.id).unwrap();
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
    assert_eq!(fake.lock().unwrap().submits, 1);
}

/// 同一账号重新登录：在跑的任务换上新令牌接着下（之后取 b.bin 的直链照常），不被打断。
#[test]
fn logging_in_again_as_the_same_user_keeps_running_tasks() {
    let dir = test_dir("relogin");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    let gate = Arc::new(Gate::default());
    {
        let mut fake = fake.lock().unwrap();
        fake.hold_b = gate.clone();
        fake.held = "VA";
    }
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, dir.join("out"));
    let requested = fake.lock().unwrap().b_requested.clone();
    rt.block_on(requested.notified());
    log_in(&rt, &service);
    gate.open();
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
}

/// 用户把子文件夹删进了回收站：里面的一概不要，只取回其余的。
#[test]
fn folders_in_the_recycle_bin_are_skipped() {
    let dir = test_dir("trashed-folder");
    let folder = dir.join("out");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    fake.lock().unwrap().sub_trashed = true;
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, folder.clone());
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
    assert!(folder.join("Show").join("a.bin").exists());
    assert!(!folder.join("Show").join("Sub").exists());
}

/// 列出文件之后用户在网盘上删了 b.bin（删进回收站、彻底删了，或连整个文件夹一起删了）：取到它时跳过，其余的照常
/// 下完。
#[test]
fn a_file_deleted_after_listing_is_skipped() {
    for (name, how) in [
        ("deleted-trashed", "trashed"),
        ("deleted-gone", "gone"),
        ("deleted-folder", "folder"),
    ] {
        let dir = test_dir(name);
        let folder = dir.join("out");
        let rt = runtime();
        let (base, fake) = rt.block_on(start(remotes(None)));
        let gate = Arc::new(Gate::default());
        {
            let mut fake = fake.lock().unwrap();
            fake.hold_b = gate.clone();
            fake.held = "VA";
        }
        let (service, mut events) = service(&dir, &base);
        log_in(&rt, &service);
        let task = offload(&rt, &service, folder.clone());
        let requested = fake.lock().unwrap().b_requested.clone();
        rt.block_on(requested.notified());
        {
            let mut fake = fake.lock().unwrap();
            match how {
                "gone" => fake.b_gone = true,
                "folder" => fake.show_gone = true,
                _ => fake.b_trashed = true,
            }
        }
        gate.open();
        let mut skipped = Vec::new();
        let status = rt.block_on(ended(&mut events, task.id, |progress| {
            if let CloudProgress::Skipped { output, .. } = progress {
                skipped.push(output.clone());
            }
        }));
        assert_eq!(status, Status::Done, "{name}");
        assert_eq!(skipped, [folder.join("Show").join("Sub").join("b.bin")]);
        assert!(folder.join("Show").join("a.bin").exists());
        let files = service.cloud_files(task.id).unwrap();
        assert!(files[0].done && !files[1].fetched && files[1].skipped);
    }
}

/// 网盘上的离线下载失败了：清掉编号，继续即重新提交。
#[test]
fn a_failed_cloud_job_is_submitted_again_on_resume() {
    let dir = test_dir("job-failed");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    fake.lock().unwrap().jobs_failed = true;
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, dir.join("out"));
    match rt.block_on(ended(&mut events, task.id, |_| {})) {
        Status::Failed(failure) => {
            assert_eq!(failure.kind, FailureKind::CloudFailed);
            assert!(
                failure.message.contains("没有可用的连接"),
                "{}",
                failure.message
            );
        }
        other => panic!("应以云端失败停下：{other:?}"),
    }
    assert_eq!(
        service.task(task.id).unwrap().spec.cloud().unwrap().job,
        None
    );
    fake.lock().unwrap().jobs_failed = false;
    service.resume(task.id).unwrap();
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
    assert_eq!(fake.lock().unwrap().submits, 2);
}

/// 文件的任务目录续不了（同一个直链续传时给出了另一个 ETag，多为落到了另一个 CDN 节点）：删掉它从头下这个文件，
/// 照样下完、校验通过。
#[test]
fn an_unresumable_file_work_dir_is_started_over() {
    let dir = test_dir("unresumable");
    let folder = dir.join("out");
    let rt = runtime();
    let (base, fake) = rt.block_on(start(remotes(None)));
    let gate = Arc::new(Gate::default());
    {
        let mut fake = fake.lock().unwrap();
        fake.hold_b = gate.clone();
        fake.b_chunked = true;
        fake.hold_tail = true;
        fake.stable_links = true;
    }
    let (service, mut events) = service(&dir, &base);
    log_in(&rt, &service);
    let task = offload(&rt, &service, folder.clone());
    let requested = fake.lock().unwrap().b_requested.clone();
    rt.block_on(requested.notified());
    service.pause(task.id);
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Paused
    );
    assert!(
        folder.join("Show.hsdl").join("1").join("job.json").exists(),
        "第一个请求之后任务目录里应已有记录"
    );
    fake.lock().unwrap().b_etag = "-v2";
    gate.open();
    service.resume(task.id).unwrap();
    assert_eq!(
        rt.block_on(ended(&mut events, task.id, |_| {})),
        Status::Done
    );
    assert_eq!(
        std::fs::read(folder.join("Show").join("Sub").join("b.bin")).unwrap(),
        remotes(None)[1].body
    );
}
