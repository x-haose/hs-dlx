//! 任务服务：持有核心的 [`Engine`] 与它的 tokio 运行时，按存储里的任务开始、暂停、继续、停止与放弃，把节流后的
//! 进度与结束状态经事件通道交给界面。
//!
//! 服务在运行时里起的任务 panic 即不变量被破坏，服务不兜：由应用决定（桌面应用直接中止进程），存储里还标着运行中的
//! 任务下次打开时按暂停处理。

use std::collections::{HashMap, HashSet};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use hs_m3u8_core::hls::Preference;
use hs_m3u8_core::remux::Shape;
use hs_m3u8_core::{
    CancellationToken, Engine, FileOutput, FileOutputOptions, FileProgress, FileRequest, Job,
    JobControl, JobRequest, Leftover, LeftoverKind, Output, OutputOptions, Probe, Progress, Proxy,
    Resource, Saved, Source, Stage as JobStage, Unsettled, Url, check_option_header,
};
use rusqlite::Transaction;
use tokio::runtime::Runtime;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::task::{AbortOnDropHandle, TaskTracker};

mod cloud;
mod in_page;
mod pages;
mod prepare;
mod web;
mod web_task;

pub use cloud::{CloudPhase, CloudProgress};
pub use prepare::{Answer, Offer, Recognized, Stage, Step, StreamOption};
pub use web::{Resolved, WebConfig, WebState};
pub use web_task::WebProgress;

use crate::intake::{self, Intake, Rejected};
use crate::preference::StoredPreference;
use crate::public_trackers::{self, PublicTrackers};
use crate::store::{self, Store};
use crate::torrent::{self, Bt, Outcome, Resume, SeedEnd, Seeder};
use crate::update::{self, Release, Restart, UpdateFeed, Version};
use crate::{
    CloudSpec, Draft, DraftKind, Error, Failure, FileSpec, Finished, HlsSpec, Kind,
    MetadataProgress, OutputSpec, SavedTorrent, Seeding, Settings, Status, Task, TaskId, TaskSpec,
    Theme, TorrentInfo, TorrentNetwork, TorrentProgress, TorrentSpec, WebSpec, naming,
};
use hs_dlx_cloud::pikpak::{self, Session};

/// 同一任务两次进度事件之间至少隔多久：界面每秒最多重画十次进度。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// 读任务目录最多等多久：网络盘没有响应时不让调用方一直等。
const INSPECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 关闭服务时，等取消了的任务停下并写进结束状态、等运行时的阻塞线程，各最多等多久。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// 新建时检查保存目录里的名字最多等多久：网络盘没有响应时不让新建一直挂着。
const NAMING_TIMEOUT: Duration = Duration::from_secs(10);
/// 网络检测最多做多久：解析各 3 秒、两轮各等 2 秒、经代理的握手最多 10 秒，并行做完约 10 秒。
const NETWORK_CHECK_TIMEOUT: Duration = Duration::from_secs(25);
/// 设了 BT 代理时多久看一眼它（见 `Shared::watch_torrent_proxy`）。
const PROXY_RECHECK: Duration = Duration::from_secs(30);
/// BT 代理能用时多久复查一次：经代理问各 DHT 引导节点一次。代理切到不转发 UDP 的节点时 libtorrent 不报错，只能靠复查
/// 发现。
const PROXY_RECHECK_USABLE: Duration = Duration::from_secs(10 * 60);
/// 放弃（等任务停下、删任务目录）最多等多久；删大目录也在这之内做完。
const DISCARD_TIMEOUT: Duration = Duration::from_secs(30);

/// 正在下载的更新（[`Handle::download_update`]）。
pub struct UpdateDownload {
    /// 已下的字节数
    pub received: watch::Receiver<u64>,
    pub done: AbortOnDropHandle<Result<Downloaded, Error>>,
}

/// 下完并验过签名的安装包。
#[derive(Debug)]
pub struct Downloaded {
    pub version: Version,
    path: PathBuf,
    /// 下载目录（[`UpdateFeed::dir`]）
    dir: PathBuf,
}

/// 服务发给各订阅方（[`Handle::subscribe`]）的事件。同一任务的事件按发生的先后到。
#[derive(Debug, Clone)]
pub enum Event {
    /// 建了任务（不论由谁建的）；同一任务的其他事件都在它之后。之后存储写不进而没建成时随即发 [`Event::Removed`]
    Added { task: Box<Task> },
    /// 任务在存储里的内容变了（状态、标题、种类与输出、请求头、是否做种、完成的时刻与运行时长）：`task` 为变了之后的。
    /// 任务停下时先发它，再发带着附带信息的 [`Event::Ended`]
    Changed { task: Box<Task> },
    /// 还没定下的任务准备到了哪一步，或停下等用户时的可选项（同 [`Handle::stage`]）。任务不再准备（定下、暂停、
    /// 失败、放弃）时不另发，见那时的 [`Event::Changed`] 或 [`Event::Removed`]。None：任务照旧在等用户，而可选项
    /// 不在了（它停在网页上，宿主关掉了网页），要 [`Handle::resume`] 重新准备
    Stage {
        id: TaskId,
        stage: Option<Box<Stage>>,
    },
    /// 任务的记录删了（放弃做完、从列表移除，或刚建的任务没能记进存储）
    Removed { id: TaskId },
    /// 运行中的 HLS 任务的进度快照；同一任务两次之间至少隔 100 毫秒，进度没变时不发
    Progress { id: TaskId, progress: Box<Progress> },
    /// 运行中的文件任务的进度快照，节流同上
    FileProgress {
        id: TaskId,
        progress: Box<FileProgress>,
    },
    /// 运行中或在做种的 BT 任务的进度快照，约每半秒一次
    TorrentProgress {
        id: TaskId,
        progress: Box<TorrentProgress>,
    },
    /// 运行中的云离线任务的进度：网盘在下时约每 5 秒一次，下文件时节流同上
    CloudProgress {
        id: TaskId,
        progress: Box<CloudProgress>,
    },
    /// 运行中的网页视频任务的进度：解析、等待、各路的下载（节流同上）与合并
    WebProgress {
        id: TaskId,
        progress: Box<WebProgress>,
    },
    /// 已完成的 BT 任务开始做种（下完接着做、[`Handle::seed`]，或打开应用时接着上次的）
    SeedingStarted { id: TaskId },
    /// 做种时定时存进度没存成（`reason` 为原因）：照常做种，下一次再存；这期间的上传与做种时长要等存成了才记下
    SeedingNotSaved { id: TaskId, reason: String },
    /// 做种停了：`end` 为原因。`unsaved` 为 Some 时停下时的进度与「不再做种」没能写进存储，下次打开时照存储里的（可能
    /// 接着做种）
    SeedingEnded {
        id: TaskId,
        end: SeedEnd,
        unsaved: Option<String>,
    },
    /// 任务停下；还没定下的任务的一轮准备停下（暂停、失败、停下等用户）时也发。停在网页上等用户（[`crate::Ask::Page`]）
    /// 时这一轮没停下，不发；它因网页没了而停下时发，状态照旧。`result` 在状态为 [`Status::Done`] 时为写出的结果，`total` 同 [`Task::total`]，`leftovers` 为完成时
    /// 没删掉的东西（被替换的旧输出、没删干净的任务目录等，输出不受影响）。`unsaved` 为 Some 时状态没能写进存储：服务
    /// 记着它，下一次写存储（任何操作或 [`Handle::flush`]）时补写，[`Handle::tasks`] 也按它返回；补写之前服务关闭
    /// 而补写不进的，下次打开时按暂停处理
    Ended {
        id: TaskId,
        status: Status,
        result: Option<Finished>,
        total: Option<usize>,
        leftovers: Vec<Leftover>,
        unsaved: Option<String>,
    },
    /// 运行中记点播的总片数时存储写不进（`error` 为原因）；任务照常运行，停下时随结束状态再写一次
    TotalUnsaved { id: TaskId, error: String },
    /// 新建任务时存储写不进，已开始的任务取消了，但磁盘上留下了东西：没删干净的任务目录，或取消前已写出的输出。它们
    /// 不在任何任务名下，要用户处理
    Orphaned { paths: Vec<PathBuf>, reason: String },
}

/// 事件的接收端，在任何执行器上都可等待。
pub type Events = mpsc::UnboundedReceiver<Event>;

/// 事件的发送端：每个订阅方各有一条无界队列、各收到一份，先后一致；接收端已丢弃的订阅方随下一次发送去掉。
#[derive(Clone, Default)]
pub(crate) struct Outbox(Arc<Mutex<Vec<mpsc::UnboundedSender<Event>>>>);

impl Outbox {
    fn subscribe(&self) -> Events {
        let (sender, receiver) = mpsc::unbounded_channel();
        lock(&self.0).push(sender);
        receiver
    }

    fn send(&self, event: Event) {
        lock(&self.0).retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }
}

/// 磁力链接取种子信息（[`Handle::fetch_metadata`]）。
pub struct MetadataFetch {
    pub progress: watch::Receiver<MetadataProgress>,
    /// 结果为种子的续传数据；释放即停止取
    pub resume: MetadataHandle,
}

/// 取种子信息的句柄：可等待，结果同 [`tokio::task::JoinHandle`]。释放即停止取，加进 BT 会话的种子当场移除。
pub struct MetadataHandle {
    task: AbortOnDropHandle<Result<Vec<u8>, Error>>,
    slot: torrent::FetchSlot,
}

impl Future for MetadataHandle {
    type Output = Result<Result<Vec<u8>, Error>, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.task).poll(cx)
    }
}

impl Drop for MetadataHandle {
    fn drop(&mut self) {
        self.slot.close();
    }
}

/// 进行中的 PikPak 登录（[`Handle::login_pikpak`]）：在系统浏览器里打开 `url`，登录后点确认。
pub struct PikPakLogin {
    /// 确认页上显示的码，供用户核对
    pub user_code: String,
    pub url: Url,
    /// 登录码到期的时刻，Unix 秒
    pub expires_at: i64,
    /// 确认了、账号记进存储即完成；释放即不再等
    pub done: AbortOnDropHandle<Result<(), Error>>,
}

/// 一个正在运行的任务：在下载、录制，或还没定下、在准备。
struct Running {
    control: Control,
    /// 停下（状态已写进存储或记为待补写、已从运行表里去掉）时变为 true
    stopped: watch::Receiver<bool>,
    /// 这次登记的编号：同一任务的登记被换掉（准备完定下后换成下载的，或准备的一轮被另一轮顶掉）之后，旧登记释放时
    /// 不动新的
    serial: u64,
    /// 在准备时：这一刻的步骤；在下载或录制时、准备的一轮停在网页上等用户时为 None
    step: Option<prepare::Step>,
    /// 在网页里找流的一轮在内置网页名额里的位子（排着或占着）；别的为 None。随登记撤掉或换掉而让出
    seat: Option<pages::Seat>,
}

/// 运行中任务的控制：各种任务的进度不同。
#[derive(Clone)]
enum Control {
    Hls(JobControl),
    File(JobControl<FileProgress>),
    /// 取消即暂停
    Torrent(CancellationToken),
    /// 取消即暂停
    Cloud(CancellationToken),
    /// 取消即暂停
    Web(CancellationToken),
    /// 还没定下的任务的一轮准备：取消即暂停
    Prepare(CancellationToken),
}

impl Control {
    fn cancel(&self) {
        match self {
            Control::Hls(control) => control.cancel(),
            Control::File(control) => control.cancel(),
            Control::Torrent(cancel)
            | Control::Cloud(cancel)
            | Control::Web(cancel)
            | Control::Prepare(cancel) => {
                cancel.cancel();
            }
        }
    }

    /// 停止录制；文件、BT、云离线、网页视频任务与准备中的任务没有录制，什么也不做。
    fn stop(&self) {
        match self {
            Control::Hls(control) => control.stop(),
            Control::File(_)
            | Control::Torrent(_)
            | Control::Cloud(_)
            | Control::Web(_)
            | Control::Prepare(_) => {}
        }
    }
}

/// 开始了的任务：各种任务的结果与进度不同。
enum Launched {
    Hls(Job),
    File(Job<FileOutput, FileProgress>),
    Torrent(TorrentRun),
    Cloud(cloud::CloudRun),
    Web(web_task::WebRun),
}

/// 开始了的 BT 任务。
struct TorrentRun {
    cancel: CancellationToken,
    done: JoinHandle<torrent::Ended>,
    /// 种子的编号、加入时的续传数据与内容的根：新建没记进存储时删它下了的文件
    info_hash: String,
    resume: Vec<u8>,
    root: PathBuf,
}

impl Launched {
    fn cancel(&self) {
        match self {
            Launched::Hls(job) => job.control().cancel(),
            Launched::File(job) => job.control().cancel(),
            Launched::Torrent(run) => run.cancel.cancel(),
            Launched::Cloud(run) => run.cancel.cancel(),
            Launched::Web(run) => run.cancel.cancel(),
        }
    }
}

/// 任务停下时要写进存储的。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ending {
    status: Status,
    result: Option<Finished>,
    /// 点播的总片数；None 时不改存储里的
    total: Option<usize>,
    /// BT 任务停下时要到的续传数据；None 时不改存储里的
    resume: Option<Resume>,
    /// BT 任务下完时接着做不做种；None 时不改存储里的
    seeding: Option<bool>,
    /// 这次运行了多久，累加进 [`Task::active`]；准备的一轮停下时为零
    ran: Duration,
    /// 完成的时刻，Unix 秒；没完成时为 None，不改存储里的
    finished_at: Option<i64>,
}

impl Ending {
    /// 以 `status` 停下，别的都不改。
    fn of(status: Status) -> Ending {
        Ending {
            status,
            result: None,
            total: None,
            resume: None,
            seeding: None,
            ran: Duration::ZERO,
            finished_at: None,
        }
    }

    /// 接在还没写进存储的 `earlier` 之后停下的：两次的运行时长相加，这一次没给的总片数、续传数据沿用那一次的。
    fn after(mut self, earlier: &Ending) -> Ending {
        self.ran += earlier.ran;
        self.total = self.total.or(earlier.total);
        self.resume = self.resume.or_else(|| earlier.resume.clone());
        self
    }

    /// 把还没写进存储的结束状态叠到从存储读出的 `task` 上。
    fn overlay(&self, task: &mut Task) {
        task.status = self.status.clone();
        task.active += self.ran;
        if self.finished_at.is_some() {
            task.finished_at = self.finished_at;
        }
        task.result.clone_from(&self.result);
        if self.total.is_some() {
            task.total = self.total;
        }
        if let Some(seeding) = self.seeding {
            task.seeding = seeding;
        }
    }
}

/// 服务的状态，运行时里的任务与界面共用。锁的顺序：先 `store`，再其余各把（其余各把之间不嵌套；`pages` 里的账
/// 自己有锁，同样排在 `store` 之后、不与别的嵌套）。
struct Shared {
    engine: Engine,
    store: Mutex<Store>,
    /// 关闭了（[`Service::close`]）：不再开始任务。在 `store` 锁内置上与查看（释放服务时独占，不用锁），关闭之后开始的
    /// 任务不会漏取消
    closed: AtomicBool,
    /// 运行中的任务；登记与查询放弃中都在 `store` 锁内，继续与放弃不会交错
    running: Mutex<HashMap<TaskId, Running>>,
    /// 正在放弃的任务：放弃做完（成功或失败）之前不能继续或移除
    discarding: Mutex<HashSet<TaskId>>,
    /// 停下了、但状态没能写进存储的任务：下一次写存储时补写
    unsaved: Mutex<HashMap<TaskId, Ending>>,
    /// 服务在运行时里起的、会改状态的任务（运行中任务的收尾、放弃、存储写不进时不要了的任务的收尾）；关闭时等它们
    tracker: TaskTracker,
    events: Outbox,
    /// 下一次运行登记的编号（见 [`Running::serial`]）
    serials: AtomicU64,
    /// 停下等用户的任务的可选项；只在内存里，重开应用后没有（磁力任务选文件的除外，见 `Shared::restore_offers`）。
    /// 增删都在 `store` 锁内，与任务状态的改动不交错
    offers: Mutex<HashMap<TaskId, prepare::Asked>>,
    /// 内置网页的宿主与名额
    pages: pages::Pages,
    /// BT 引擎，第一次用到时建会话
    bt: Bt,
    /// 取公共 tracker 列表的任务（BT 会话建起时开始，取到为止）；关闭时取消
    public_fetch: Mutex<Option<AbortOnDropHandle<()>>>,
    /// 定期把 yt-dlp 换成新版的任务（见 [`web::keep_web_current`]，启动时开始）；关闭时取消
    web_updates: Mutex<Option<AbortOnDropHandle<()>>>,
    /// 看着 BT 代理的任务与它看着的地址（BT 会话已建、设了代理时有，见 `Shared::watch_torrent_proxy`）；改了代理或
    /// 关闭时取消
    proxy_probe: Mutex<Option<(Url, AbortOnDropHandle<()>)>>,
    /// 代理设置改了（[`Handle::set_proxy`]）：没取到公共 tracker 列表时不等退避、立即再取
    proxy_changed: Arc<Notify>,
    /// 服务的运行时：在运行时外面（界面线程）用到 BT 引擎时，取公共 tracker 列表的任务也起在这里
    runtime: tokio::runtime::Handle,
    /// 做种的设置，随 [`Handle::set_seeding`] 改；在做种的随时照它判停不停
    seeding: watch::Sender<Seeding>,
    /// 在做种的（含正在加入会话、还没开始的）：停止做种经它找到，登记与查询都在 `store` 锁内
    seeders: Mutex<HashMap<TaskId, SeedControl>>,
    /// PikPak 的登录状态，各云离线任务共用（refresh_token 只能用一次，各用各的会互相作废）；第一次用到时照存储里的
    /// 账号建，登录换掉、退出登录清掉
    pikpak: Mutex<Option<Arc<Session>>>,
    /// PikPak 的接口所在
    pikpak_endpoints: pikpak::Endpoints,
    /// 网页视频的组件与解析
    web: web::Web,
}

/// 一个在做种的任务的控制。
struct SeedControl {
    cancel: CancellationToken,
    /// 用户停止了做种（[`Handle::stop_seeding`]）：停下后不再做种，即使停的期间应用正在退出
    by_user: Arc<AtomicBool>,
    /// 停下了（进度与「不再做种」已写进存储、已撤登记）时为 true
    ended: watch::Receiver<bool>,
}

/// 任务服务：持有运行时，负责关闭与释放；对任务与设置的操作都在 [`Handle`] 上，服务可直接当它用。不能在 tokio
/// 运行时里释放：释放时要在自己的运行时上等任务停下（见 [`Service::close`]）。
pub struct Service {
    handle: Handle,
    /// 只在释放时取走（释放要按值关闭运行时）
    runtime: Option<Runtime>,
}

/// 任务服务的操作入口：可克隆，可跨线程用。服务（[`Service`]）关闭之后新建与继续报 [`Error::Closed`]，释放之后在
/// 运行时里做的操作（返回句柄的那些）不再有结果。
#[derive(Clone)]
pub struct Handle {
    shared: Arc<Shared>,
}

impl std::ops::Deref for Service {
    type Target = Handle;

    fn deref(&self) -> &Handle {
        &self.handle
    }
}

impl Service {
    /// 用 `store` 建服务，HTTP 同时最多 `max_requests` 个请求（见 [`hs_m3u8_core::Engine::new`]），BT 会话连
    /// `network`，PikPak 的接口在 `pikpak`（官方的为 [`pikpak::Endpoints::official`]）；返回服务与第一个订阅的事件
    /// 接收端（另见 [`Handle::subscribe`]）。上次退出时还标着运行中、准备中的任务改为暂停；其中 BT 任务存的续传数据
    /// 改为不可信（可能比磁盘上的多），下次继续时先整体校验已下的文件。停下等用户的任务照旧等着，可选项要
    /// [`Handle::resume`] 重新准备才有（磁力任务选文件的由存着的种子信息得出，随后以 [`Event::Stage`] 报来）。
    pub fn new(
        mut store: Store,
        max_requests: NonZeroUsize,
        network: TorrentNetwork,
        pikpak: pikpak::Endpoints,
        web: WebConfig,
    ) -> Result<(Service, Events), Error> {
        let tx = store.transaction()?;
        store::pause_interrupted(&tx)?;
        let settings = store::settings(&tx)?;
        let tasks = store::list(&tx)?;
        let seeding: Vec<TaskId> = tasks
            .iter()
            .filter(|task| task.seeding)
            .map(|task| task.id)
            .collect();
        // 完成了的网页视频任务的任务目录：完成时写存储没成、事后补写的，或写进后、删之前退出的，留到这里删
        let finished_web: Vec<PathBuf> = tasks
            .into_iter()
            .filter(|task| task.status == Status::Done && task.spec.web().is_some())
            .filter_map(|task| task.work_dir)
            .collect();
        tx.commit()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("hs-dlx-engine")
            .build()
            .map_err(|cause| Error::Io {
                action: "启动",
                path: "tokio 运行时".into(),
                cause,
            })?;
        let engine = {
            let _entered = runtime.enter();
            Engine::new(max_requests)
        };
        let events = Outbox::default();
        let receiver = events.subscribe();
        let update_every = web.update_every;
        let web = web::Web::new(web, engine.clone());
        let shared = Arc::new(Shared {
            engine,
            store: Mutex::new(store),
            closed: AtomicBool::new(false),
            running: Mutex::new(HashMap::new()),
            discarding: Mutex::new(HashSet::new()),
            unsaved: Mutex::new(HashMap::new()),
            tracker: TaskTracker::new(),
            events,
            serials: AtomicU64::new(0),
            offers: Mutex::new(HashMap::new()),
            pages: pages::Pages::new(usize::from(settings.web_pages)),
            bt: Bt::new(
                network,
                settings.upload_limit,
                settings.torrent_proxy.clone(),
            ),
            public_fetch: Mutex::new(None),
            web_updates: Mutex::new(None),
            proxy_probe: Mutex::new(None),
            proxy_changed: Arc::new(Notify::new()),
            runtime: runtime.handle().clone(),
            seeding: watch::Sender::new(settings.seeding),
            seeders: Mutex::new(HashMap::new()),
            pikpak: Mutex::new(None),
            pikpak_endpoints: pikpak,
            web,
        });
        let service = Service {
            handle: Handle { shared },
            runtime: Some(runtime),
        };
        // 上次更新 yt-dlp 换下来的旧版本：这时没有任务在用。完成了的网页视频任务留下的任务目录另删，不等前者（它等着
        // 组件的锁，安装期间一直被占）
        {
            let _entered = service.runtime().enter();
            let shared = service.shared.clone();
            service
                .shared
                .tracker
                .spawn(async move { shared.web.sweep().await });
            service.shared.unpack_bundled_on_start();
            if let Some(every) = update_every {
                let updating = web::keep_web_current(Arc::downgrade(&service.shared), every);
                *lock(&service.shared.web_updates) =
                    Some(AbortOnDropHandle::new(service.runtime().spawn(updating)));
            }
            let shared = service.shared.clone();
            service.shared.tracker.spawn(async move {
                for work_dir in finished_web {
                    // 这期间移除了那个任务、又新建了同一输出的任务时，目录是新任务的
                    let taken = shared.write(|tx| {
                        Ok(store::list(tx)?.iter().any(|task| {
                            task.status != Status::Done
                                && task.work_dir.as_deref() == Some(work_dir.as_path())
                        }))
                    });
                    let removed = match taken {
                        Ok(true) => continue,
                        Ok(false) => web_task::remove_work_dir(&work_dir).await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = removed {
                        shared.events.send(Event::Orphaned {
                            paths: vec![work_dir],
                            reason: error.to_string(),
                        });
                    }
                }
            });
        }
        // 上次退出时还在做种的接着做；开始不了的与做种中途开始不了的一样，以做种停了报出、记为不再做种（可再点做种）
        {
            let _entered = service.runtime().enter();
            for id in seeding {
                if let Err(error) = service.shared.start_seeding(id) {
                    let cleared = service.shared.change(|tx| {
                        store::set_seeding_task(tx, id, false)?;
                        store::get(tx, id)?.ok_or(Error::NotFound(id))
                    });
                    service.shared.events.send(Event::SeedingEnded {
                        id,
                        end: SeedEnd::Failed(error.to_string()),
                        unsaved: cleared.err().map(|error| error.to_string()),
                    });
                }
            }
        }
        service.shared.restore_offers();
        Ok((service, receiver))
    }

    /// 退出前停下所有任务：不再开始任务（新建与继续报 [`Error::Closed`]），取消运行中的，最多等 2 秒让它们与放弃等
    /// 收尾任务做完、写进结束状态，再补写没写进去的；建过 BT 会话时记下它的监听端口与 DHT 状态，会话随之关闭（向
    /// tracker 报告停止最多再等 2 秒）。返回补写与记下会话状态的结果。没来得及停下的与补写不进的，存储里还是运行中，
    /// 下次打开时按暂停处理。不能在 tokio 运行时里调用。
    pub fn close(&self) -> Result<(), Error> {
        {
            let _store = lock(&self.shared.store);
            self.shared.closed.store(true, Ordering::Relaxed);
            for running in lock(&self.shared.running).values() {
                running.control.cancel();
            }
            // 停下做种；存储里照旧记着在做种，下次打开接着做
            for seeder in lock(&self.shared.seeders).values() {
                seeder.cancel.cancel();
            }
        }
        // 安装中的网页视频组件：已下的组件包留着下次续传
        self.shared.web.cancel_install();
        // 取公共 tracker 列表、探 BT 代理、查新的 yt-dlp 的不改存储，不用等（在做的 yt-dlp 更新随上面的取消停下）
        drop(lock(&self.shared.public_fetch).take());
        drop(lock(&self.shared.web_updates).take());
        drop(lock(&self.shared.proxy_probe).take());
        let runtime = self.runtime.as_ref().expect("运行时只在释放时取走");
        let tracker = &self.shared.tracker;
        tracker.close();
        let _timed_out =
            runtime.block_on(async { tokio::time::timeout(CLOSE_TIMEOUT, tracker.wait()).await });
        // 在途的换令牌发出后旧的 refresh_token 即作废：等它收下、存好新的（一次接口请求的时限内）
        let session = lock(&self.shared.pikpak).clone();
        if let Some(session) = session {
            runtime.block_on(session.settle());
        }
        let flushed = self.flush();
        let saved = match self.shared.bt.close() {
            Ok(Some(state)) => self
                .shared
                .write(|tx| store::set_torrent_session(tx, &state)),
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        flushed.and(saved)
    }
}

impl Handle {
    fn runtime(&self) -> &tokio::runtime::Handle {
        &self.shared.runtime
    }

    /// 订阅事件：之后发生的事件各订阅方都收到一份；释放接收端即退订。
    pub fn subscribe(&self) -> Events {
        self.shared.events.subscribe()
    }

    /// 把粘贴的文字 `text` 拆成一条或多条（见 [`intake::split`]），每条建一个还没定下的任务、存到 `folder` 并开始
    /// 准备；`at` 为新建时间（Unix 秒）。返回各行的结果：建出的任务（准备中），或这一行没建成的原因；整段不收时只有
    /// 一项。返回的句柄在任何执行器上都可等待。
    pub fn paste(
        &self,
        text: &str,
        folder: &Path,
        at: i64,
    ) -> JoinHandle<Vec<Result<Task, Rejected>>> {
        let (shared, text, folder) = (self.shared.clone(), text.to_owned(), folder.to_path_buf());
        self.runtime().spawn(async move {
            let lines = match intake::split(&text, &folder) {
                Ok(lines) => lines,
                Err(reason) => {
                    return vec![Err(Rejected {
                        text,
                        reason: Box::new(reason),
                    })];
                }
            };
            let mut results = Vec::with_capacity(lines.len());
            for line in lines {
                results.push(match line {
                    Ok(intake) => {
                        let text = intake.url.to_string();
                        shared.add(intake, at).await.map_err(|reason| Rejected {
                            text,
                            reason: Box::new(reason),
                        })
                    }
                    Err(rejected) => Err(rejected),
                });
            }
            results
        })
    }

    /// 按拆好的一条 `intake` 建一个还没定下的任务并开始准备，返回存进存储的任务（准备中）；`at` 为新建时间（Unix
    /// 秒）。之后服务自己往下走：认地址、解析、取种子信息、在内置网页里找流，到要用户决定的一步停下等（[`Status::Waiting`]，可选项见
    /// [`Handle::stage`]，选择经 [`Handle::choose`]），都定了即定下是哪种任务并开始下载；每一步以 [`Event`] 报来。
    /// 请求头不合要求、保存目录不是绝对路径、磁力链接用不了、种子文件不是种子时报 [`Error::InvalidInput`]，种子文件
    /// 读不出时报 [`Error::Io`]，种子已在某个任务里时报 [`Error::TorrentTaken`]，正在退出时报 [`Error::Closed`]。
    /// 返回的句柄在任何执行器上都可等待。
    pub fn add(&self, intake: Intake, at: i64) -> JoinHandle<Result<Task, Error>> {
        let shared = self.shared.clone();
        self.runtime()
            .spawn(async move { shared.add(intake, at).await })
    }

    /// 还没定下的任务 `id` 此刻准备到哪一步，或停下等用户时的可选项。定下了的、暂停或失败的为 None；停下等用户而
    /// 可选项已不在的（重开应用后，或它停着的网页被宿主关掉了）也为 None，要 [`Handle::resume`] 重新准备。没有这个
    /// 任务时报 [`Error::NotFound`]。
    pub fn stage(&self, id: TaskId) -> Result<Option<Stage>, Error> {
        self.shared.stage(id)
    }

    /// 交上用户对停下等用户的任务 `id` 的选择：任务回到准备中接着走（都定了的随即定下、开始下载），返回准备中的
    /// 任务。任务没在等选择、可选项已不在、`answer` 与在等的对不上、选的不在可选项里、保存目录不是绝对路径时报
    /// [`Error::InvalidInput`]；没有这个任务、正在放弃、正在退出时报 [`Error::NotFound`]、[`Error::Discarding`]、
    /// [`Error::Closed`]。停在网页上等用户的任务换请求头（[`Answer::Headers`]）时，看着网页的那一轮随之作废、网页关掉。
    pub fn choose(&self, id: TaskId, answer: Answer) -> Result<Task, Error> {
        let _entered = self.runtime().enter();
        self.shared.choose(id, answer)
    }

    /// 把还没定下的磁力任务 `id` 交给网盘 `provider` 云离线（整包离线，不分文件），存到建任务时给的保存目录：在取
    /// 种子信息的停掉，任务随即定下为云离线任务并开始；返回准备中的任务。任务已定下、来源不是磁力链接时报
    /// [`Error::InvalidInput`]，没登录网盘时报 [`Error::NotLoggedIn`]，正在放弃时报 [`Error::Discarding`]。
    pub fn offload(&self, id: TaskId, provider: crate::CloudProvider) -> Result<Task, Error> {
        let _entered = self.runtime().enter();
        self.shared.offload(id, provider)
    }

    /// 记下没有要选的任务开始前要不要先停下确认（见 [`Settings::confirm_before_start`]）：之后开始的准备照它。
    pub fn set_confirm_before_start(&self, confirm: bool) -> Result<(), Error> {
        self.shared
            .write(|tx| store::set_confirm_before_start(tx, confirm))
    }

    /// 注册网页宿主：之后要到网页里找流的任务经它开网页，`pace` 为找流的几个时长。注册时宿主算作开得了网页。
    pub fn set_page_host(&self, host: Arc<dyn crate::PageHost>, pace: crate::PagePace) {
        let _store = lock(&self.shared.store);
        self.shared.pages.set_host(host, pace);
    }

    /// 宿主此刻开不开得了网页（桌面应用：主窗口开着没有）。服务从宿主关掉网页（[`crate::PageEvent::Closed`]）或没开成
    /// （[`crate::Unopened::Away`]）得知它开不了、不再让任务去开；又开得了时由宿主的一方在这里说一声，排着的随即依次
    /// 放行。
    pub fn set_page_host_ready(&self, ready: bool) {
        let _store = lock(&self.shared.store);
        self.shared.pages.set_ready(ready);
    }

    /// 记下同时最多开几个内置网页（见 [`Settings::web_pages`]）：调大时排着的随即放行，调小时已开着的不关、等它们
    /// 自己结束。不在 1 到 3 之间时报 [`Error::InvalidInput`]。
    pub fn set_web_pages(&self, pages: u8) -> Result<(), Error> {
        if !Settings::WEB_PAGES.contains(&pages) {
            return Err(Error::InvalidInput(format!(
                "内置网页同时开 {} 到 {} 个：{pages}",
                Settings::WEB_PAGES.start(),
                Settings::WEB_PAGES.end()
            )));
        }
        let mut store = lock(&self.shared.store);
        self.shared
            .write_in(&mut store, |tx| store::set_web_pages(tx, pages))?;
        self.shared.pages.set_limit(usize::from(pages));
        Ok(())
    }

    /// 任务 `id` 此刻能做的操作，最该先做的排第一（见 [`crate::plan`]）；正在放弃时没有。没有这个任务时报
    /// [`Error::NotFound`]。
    pub fn actions(&self, id: TaskId) -> Result<crate::Plan, Error> {
        let mut store = lock(&self.shared.store);
        let task = self.shared.task_in(&mut store, id)?;
        if lock(&self.shared.discarding).contains(&id) {
            return Ok(crate::Plan {
                actions: Vec::new(),
                resume_after_headers: false,
            });
        }
        // 在运行的 HLS 任务按此刻的阶段：准备阶段还不知道是不是直播，照新建时的选择。停着的照新建时的选择，记下过
        // 点播总片数的不算（按直播新建、实际是点播的）
        let live = task.spec.hls().is_some_and(|hls| hls.live);
        let stage = match lock(&self.shared.running)
            .get(&id)
            .map(|running| &running.control)
        {
            Some(Control::Hls(control)) => Some(control.progress().borrow().stage),
            _ => None,
        };
        let recording = match (&task.status, stage) {
            (Status::Running, _) if task.write_only => false,
            (Status::Running, Some(JobStage::Recording)) => true,
            (Status::Running, Some(JobStage::Preparing) | None) => live,
            (Status::Running, Some(JobStage::Downloading | JobStage::Writing | JobStage::Done)) => {
                false
            }
            _ => live && task.total.is_none(),
        };
        let offered = lock(&self.shared.offers).contains_key(&id);
        Ok(crate::plan(&task, crate::Situation { recording, offered }))
    }

    /// 探测网页里找到的一条流 `stream`，经设置里的代理：请求头照网页引擎里会发往它的 `cookies` 配（没读出时为原因，
    /// 不探测）。结果为探测到哪一步（成了，或没成的原因）与没带上的、放不进请求头的 Cookie 个数；30 秒没有结果算没成。
    /// 返回的句柄在任何执行器上都可等待，释放它即取消。
    pub fn check_stream(
        &self,
        stream: crate::page::Stream,
        cookies: Result<Vec<crate::page::Cookie>, String>,
    ) -> AbortOnDropHandle<(crate::page::Check, usize)> {
        let shared = self.shared.clone();
        AbortOnDropHandle::new(self.runtime().spawn(async move {
            match shared.settings() {
                Ok(settings) => {
                    crate::page::check(&shared.engine, settings.proxy, &stream, cookies, &[]).await
                }
                Err(error) => (
                    crate::page::Check::Failed {
                        reason: format!("读不出设置：{error}"),
                        headers: None,
                    },
                    0,
                ),
            }
        }))
    }

    /// 全部任务，按新建先后；停下了而状态还没补写进存储的，按停下时的状态。
    pub fn tasks(&self) -> Result<Vec<Task>, Error> {
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        let mut tasks = store::list(&tx)?;
        let unsaved = lock(&self.shared.unsaved);
        for task in &mut tasks {
            if let Some(ending) = unsaved.get(&task.id) {
                ending.overlay(task);
            }
        }
        Ok(tasks)
    }

    /// 把停下了而状态还没写进存储的补写进去。
    pub fn flush(&self) -> Result<(), Error> {
        self.shared.write(|_| Ok(()))
    }

    /// 设置。
    pub fn settings(&self) -> Result<Settings, Error> {
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        store::settings(&tx)
    }

    /// 记下新建任务默认保存到的目录；不是绝对路径时报 [`Error::InvalidInput`]。
    pub fn set_folder(&self, folder: &Path) -> Result<(), Error> {
        if !folder.is_absolute() {
            return Err(Error::InvalidInput(format!(
                "保存目录要是绝对路径：{}",
                folder.display()
            )));
        }
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        store::set_folder(&tx, folder)?;
        tx.commit()?;
        Ok(())
    }

    /// 记下做种的设置；在做种的随即照它判停不停。
    pub fn set_seeding(&self, seeding: Seeding) -> Result<(), Error> {
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        store::set_seeding(&tx, seeding)?;
        tx.commit()?;
        self.shared.seeding.send_replace(seeding);
        Ok(())
    }

    /// 记下 BT 的上传限速（字节每秒；None 为不限），BT 会话已建时随即照它限。
    pub fn set_upload_limit(&self, limit: Option<NonZeroU32>) -> Result<(), Error> {
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        store::set_upload_limit(&tx, limit)?;
        tx.commit()?;
        self.shared.bt.set_upload_limit(limit);
        Ok(())
    }

    /// 诊断在下或做种的 BT 任务（见 [`crate::Check`]）：要一次 BT 会话的统计并查种子与 tracker 的状况。不是在下或做种
    /// 的 BT 任务时报 [`Error::InvalidInput`]（BT 会话还没建也是），统计 10 秒内没到时报 [`Error::Torrent`]。返回的
    /// 句柄在任何执行器上都可等待。
    pub fn diagnose(&self, id: TaskId) -> JoinHandle<Result<Vec<crate::Check>, Error>> {
        let shared = self.shared.clone();
        self.runtime().spawn(async move {
            let (spec, limit) = {
                let mut store = lock(&shared.store);
                let tx = store.transaction()?;
                let task = store::get(&tx, id)?.ok_or(Error::NotFound(id))?;
                (
                    task.spec.torrent().cloned(),
                    store::settings(&tx)?.upload_limit,
                )
            };
            let not_running = || Error::InvalidInput("只有在下或做种的 BT 任务能诊断".to_owned());
            let spec = spec.ok_or_else(not_running)?;
            let seeding = if lock(&shared.running).contains_key(&id) {
                false
            } else if lock(&shared.seeders).contains_key(&id) {
                true
            } else {
                return Err(not_running());
            };
            // 在下或做种的已建过会话；关闭之后没有了
            let engine = shared.bt.existing().ok_or(Error::Closed)?;
            torrent::diagnose(&engine, &spec.info_hash, seeding, limit, shared.bt.proxy()).await
        })
    }

    /// 检测 BT 用的网络（见 [`crate::NetworkReport`]）：并行探直连与 IPv6 的 DHT、UDP 的 tracker（公共 tracker 列表
    /// 取到了时）、经 BT 代理的 DHT（设了时；探到的结果同时记进 BT 引擎），再读 BT 会话报的监听、端口映射与连入（还没
    /// 建会话时不建，这几项不测）。设了 BT 代理时也从本机直连测一次。约 10 秒，最多 25 秒，过了报
    /// [`Error::TimedOut`]；开不了 UDP 套接字时报 [`Error::Io`]。返回的句柄在任何执行器上都可等待，释放它即取消。
    pub fn check_network(&self) -> AbortOnDropHandle<Result<crate::NetworkReport, Error>> {
        let shared = self.shared.clone();
        AbortOnDropHandle::new(self.runtime().spawn(async move {
            tokio::time::timeout(NETWORK_CHECK_TIMEOUT, shared.check_network())
                .await
                .map_err(|_| Error::TimedOut {
                    action: "检测网络".to_owned(),
                    after: NETWORK_CHECK_TIMEOUT,
                })?
        }))
    }

    /// 让下完的 BT 任务做种（照做种的设置，到了停的条件即停）：记下在做种，加入 BT 会话、校验已下的文件后只上传；文件
    /// 不全时停下（[`SeedEnd::Incomplete`]），不补下。开始与停下照常以 [`Event::SeedingStarted`] 与
    /// [`Event::SeedingEnded`] 报来。不是下完的 BT 任务时报 [`Error::InvalidInput`]，已在做种时报 [`Error::Seeding`]，
    /// 正在退出时报 [`Error::Closed`]。
    pub fn seed(&self, id: TaskId) -> Result<(), Error> {
        let _entered = self.runtime().enter();
        self.shared.start_seeding(id)
    }

    /// 停止做种：随即记为不再做种（之后应用退出或崩溃也不再接着做），再存下进度、从 BT 会话里移除；停下后发
    /// [`Event::SeedingEnded`]。没在做种时什么也不做；记不进存储时报错，做种照旧。
    pub fn stop_seeding(&self, id: TaskId) -> Result<(), Error> {
        self.shared.stop_seeding(id).map(drop)
    }

    /// 记下 BT 经的 SOCKS5 代理（None 为直连），BT 会话已建时随即照它连。不是 `socks5://` 或 `socks5h://` 开头、没写
    /// 主机或端口时报 [`Error::InvalidInput`]。
    pub fn set_torrent_proxy(&self, proxy: Option<Url>) -> Result<(), Error> {
        if let Some(url) = &proxy {
            crate::socks::check(url).map_err(Error::InvalidInput)?;
        }
        self.shared
            .write(|tx| store::set_torrent_proxy(tx, proxy.as_ref()))?;
        self.shared.bt.set_proxy(proxy);
        // 重新开始看着（存的与之前相同时也随即再探）：BT 会话已建时随即开始；还没建时等建起时
        drop(lock(&self.shared.proxy_probe).take());
        if self.shared.bt.existing().is_some() {
            self.shared.watch_torrent_proxy();
        }
        Ok(())
    }

    /// 认出 `host:port` 上的代理（见 [`crate::ProxyProtocol`]），返回存进设置的地址；不存。返回的句柄在任何执行器上都
    /// 可等待，释放它即取消。最多十几秒（HTTP 代理要先连上外面的地址才回）。
    pub fn identify_proxy(
        &self,
        host: String,
        port: u16,
        login: Option<crate::ProxyLogin>,
        wanted: crate::ProxyWanted,
    ) -> AbortOnDropHandle<Result<Url, crate::ProxyProbeError>> {
        AbortOnDropHandle::new(self.runtime().spawn(async move {
            crate::proxy_probe::identify(&host, port, login.as_ref(), wanted).await
        }))
    }

    /// 记下要不要自动检查更新。
    pub fn set_check_updates(&self, check: bool) -> Result<(), Error> {
        self.shared.write(|tx| store::set_check_updates(tx, check))
    }

    /// 查 `feed` 上有没有比本应用新的版本，经设置里的代理；没有时为 None。返回的句柄在任何执行器上都可等待，释放它即
    /// 取消。查不到等见 [`Error::Update`]；读不出设置时直接报错。
    pub fn check_update(
        &self,
        feed: &UpdateFeed,
    ) -> Result<AbortOnDropHandle<Result<Option<Release>, Error>>, Error> {
        let proxy = self.settings()?.proxy;
        let (engine, feed) = (self.shared.engine.clone(), feed.clone());
        Ok(AbortOnDropHandle::new(self.runtime().spawn(async move {
            update::check(&engine, &feed, &proxy, Version::current()).await
        })))
    }

    /// 下载 `release` 里本平台的安装包（经设置里的代理，放在 `feed.dir`，没下完的再下时接着下），先取签名、核对它是
    /// 这个版本这个文件的，下完验内容。返回已下的字节数与句柄：句柄的结果为验过的安装包，交给
    /// [`Handle::install_update`]；释放它即取消。`release` 没有本平台的安装包时报 [`Error::Update`]，读不出设置时直接
    /// 报错；其余失败（取不到、签名不符等）经句柄报 [`Error::Update`]，签名不符的文件删掉。
    pub fn download_update(
        &self,
        feed: &UpdateFeed,
        release: &Release,
    ) -> Result<UpdateDownload, Error> {
        let package = release
            .package
            .clone()
            .ok_or_else(|| Error::Update("这个平台不能自动更新，到下载页下载新版本".to_owned()))?;
        let proxy = self.settings()?.proxy;
        let (engine, key, dir, release) = (
            self.shared.engine.clone(),
            feed.public_key.clone(),
            feed.dir.clone(),
            release.clone(),
        );
        let (progress, received) = watch::channel(0);
        let handle = self.runtime().spawn(async move {
            let (key, signature) =
                update::signature(&engine, &proxy, &key, &release, &package).await?;
            let path = dir.join(&package.name);
            let mut output = FileOutputOptions::new(path.clone());
            output.overwrite = true;
            let mut source = Source::new(package.url.clone());
            source.http.proxy = proxy;
            let failed = |error: hs_m3u8_core::Error| {
                Error::Update(format!("下载不了 {}：{error}", package.name))
            };
            let job = engine
                .start_file(FileRequest::new(source, output))
                .map_err(failed)?;
            let mut bytes = job.control().progress();
            let waiting = job.wait();
            tokio::pin!(waiting);
            let downloaded = loop {
                tokio::select! {
                    result = &mut waiting => break result.map_err(failed)?,
                    Ok(()) = bytes.changed() => {
                        progress.send_replace(bytes.borrow_and_update().bytes);
                    }
                }
            };
            // 下完与最后一次进度同时到时上面可能先选了下完
            progress.send_replace(downloaded.bytes);
            let checked = downloaded.path.clone();
            tokio::task::spawn_blocking(move || {
                let verified = update::verify(&checked, &key, &signature);
                if verified.is_err() {
                    // 不是发布的那个文件：留着下次也只会接着用它
                    let _gone = std::fs::remove_file(&checked);
                }
                verified
            })
            .await
            .expect("验签名不会 panic，运行时关闭时本任务也一起丢弃")?;
            Ok(Downloaded {
                version: release.version,
                path: downloaded.path,
                dir,
            })
        });
        Ok(UpdateDownload {
            received,
            done: AbortOnDropHandle::new(handle),
        })
    }

    /// 装上验过签名的安装包，返回退出之后怎么接着做（见 [`crate::Restart`]：退出已不可撤回时起它）。没装上时报
    /// [`Error::Update`]（已装的版本不受影响，可再试），换到一半时报 [`Error::UpdateStranded`]。返回的句柄在任何执行器
    /// 上都可等待。
    pub fn install_update(&self, downloaded: Downloaded) -> JoinHandle<Result<Restart, Error>> {
        self.runtime()
            .spawn_blocking(move || update::install(&downloaded.path, &downloaded.dir))
    }

    /// 开始登录 PikPak（经设置里的代理）：要一个登录码，之后在后台按间隔问确认了没有，确认了即把账号记进存储、换上新的
    /// 登录状态（之后开始的云离线任务用它）。返回的句柄在任何执行器上都可等待，释放它即取消；登录码过期报
    /// [`Error::PikPak`]（[`pikpak::Error::LoginExpired`]）。
    pub fn login_pikpak(&self) -> AbortOnDropHandle<Result<PikPakLogin, Error>> {
        let shared = self.shared.clone();
        AbortOnDropHandle::new(self.runtime().spawn(async move {
            let http = hs_m3u8_core::HttpOptions {
                proxy: shared.settings()?.proxy,
                ..hs_m3u8_core::HttpOptions::default()
            };
            let endpoints = shared.pikpak_endpoints.clone();
            let mut login = pikpak::login::start(&shared.engine, &http, &endpoints).await?;
            let (user_code, url, expires_at) =
                (login.user_code.clone(), login.url.clone(), login.expires_at);
            let runtime = shared.runtime.clone();
            let polling = runtime.spawn(async move {
                loop {
                    tokio::time::sleep(login.interval).await;
                    match login.poll(&shared.engine, &http, &endpoints).await? {
                        pikpak::login::Poll::Pending => {}
                        pikpak::login::Poll::Done(account) => {
                            // 照存储里的账号取（还没建时建出），与同时开始的任务用的是同一个
                            let current = match shared.pikpak_session() {
                                Ok(session) => Some(session),
                                Err(Error::NotLoggedIn) => None,
                                Err(error) => return Err(error),
                            };
                            match current {
                                // 同一账号重新登录：在用的登录状态换上新令牌，在跑的任务不受影响
                                Some(session) if session.user_id() == account.user_id => {
                                    session.adopt(account).await?;
                                }
                                _ => {
                                    let session = shared.new_pikpak_session(account.clone());
                                    shared.set_pikpak(Some(&account), Some(session))?;
                                }
                            }
                            return Ok(());
                        }
                    }
                }
            });
            Ok(PikPakLogin {
                user_code,
                url,
                expires_at,
                done: AbortOnDropHandle::new(polling),
            })
        }))
    }

    /// 退出 PikPak：从存储里删掉账号，登录状态作废：在运行的云离线任务下一次调网盘时以登录失效停下，之后的要重新登录。
    pub fn logout_pikpak(&self) -> Result<(), Error> {
        self.shared.set_pikpak(None, None)
    }

    /// 云离线任务的文件与各自下完没有（列出文件之前为空），任务不在运行时显示已下多少；没有这个任务时报
    /// [`Error::NotFound`]。
    pub fn cloud_files(&self, id: TaskId) -> Result<Vec<crate::CloudFile>, Error> {
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        store::get(&tx, id)?.ok_or(Error::NotFound(id))?;
        store::cloud_files(&tx, id)
    }

    /// 登录了的 PikPak 账号的用户编号；没登录时为 None。
    pub fn pikpak_user(&self) -> Result<Option<String>, Error> {
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        Ok(store::pikpak_account(&tx)?.map(|account| account.user_id))
    }

    /// 记下外观主题。
    pub fn set_theme(&self, theme: Theme) -> Result<(), Error> {
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        store::set_theme(&tx, theme)?;
        tx.commit()?;
        Ok(())
    }

    /// 记下代理：之后的探测与开始的任务照它，已在运行的任务不变。指定的代理地址用不了（协议不是 http、https、socks4、
    /// socks4a、socks5、socks5h，或没有主机）时报 [`Error::InvalidInput`]。
    pub fn set_proxy(&self, proxy: &Proxy) -> Result<(), Error> {
        if let Proxy::Url(url) = proxy {
            Proxy::check_url(url).map_err(|error| Error::InvalidInput(error.to_string()))?;
        }
        let mut store = lock(&self.shared.store);
        let tx = store.transaction()?;
        store::set_proxy(&tx, proxy)?;
        tx.commit()?;
        self.shared.proxy_changed.notify_one();
        Ok(())
    }

    /// 探测来源，经设置里的代理（`source.http.proxy` 照设置换掉）；返回的句柄在任何执行器上都可等待，释放它即取消
    /// 探测（连同在途的请求）。读不出设置时为原因。
    pub fn probe(
        &self,
        mut source: Source,
    ) -> Result<AbortOnDropHandle<Result<Probe, hs_m3u8_core::Error>>, Error> {
        source.http.proxy = self.settings()?.proxy;
        let engine = self.shared.engine.clone();
        Ok(AbortOnDropHandle::new(
            self.runtime()
                .spawn(async move { engine.probe(&source).await }),
        ))
    }

    /// 认出地址是什么（播放列表、网页还是文件，见 [`Engine::identify`]），经设置里的代理；句柄与读不出设置时同
    /// [`Handle::probe`]。
    pub fn identify(
        &self,
        mut source: Source,
    ) -> Result<AbortOnDropHandle<Result<Resource, hs_m3u8_core::Error>>, Error> {
        source.http.proxy = self.settings()?.proxy;
        let engine = self.shared.engine.clone();
        Ok(AbortOnDropHandle::new(
            self.runtime()
                .spawn(async move { engine.identify(&source).await }),
        ))
    }

    /// 磁力链接 `uri` 取种子信息（连 DHT 与链接里的 tracker、节点找有它的节点），返回取的进度与句柄：句柄的结果为种子的
    /// 续传数据，供新建任务（[`DraftKind::Torrent`]）；句柄在任何执行器上都可等待，释放它即停止取。这个种子已在任务里
    /// （不论在下、做种还是停着）时报 [`Error::TorrentTaken`]，不是磁力链接、或这个种子正在别处取种子信息时报
    /// [`Error::Torrent`]；BT 会话建不出时直接报错。取不到时一直等，由调用方决定何时放弃。
    pub fn fetch_metadata(&self, uri: String) -> Result<MetadataFetch, Error> {
        let id = hs_dlx_torrent::magnet_id(&uri)
            .map_err(|error| Error::Torrent(format!("加不进磁力链接：{error}")))?;
        torrent_free(&self.shared, id.as_str())?;
        self.shared.fetch_metadata(uri)
    }

    /// 读种子文件（bencode 写的内容 `torrent`）：结果为续传数据（供新建任务，[`DraftKind::Torrent`]）与其中的种子信息。
    /// 不是种子文件时报 [`Error::InvalidInput`]，这个种子已在任务里（不论在下、做种还是停着）时报
    /// [`Error::TorrentTaken`]。返回的句柄在任何执行器上都可等待。
    pub fn open_torrent_file(
        &self,
        torrent: Vec<u8>,
    ) -> JoinHandle<Result<(Vec<u8>, TorrentInfo), Error>> {
        let shared = self.shared.clone();
        self.runtime().spawn(async move {
            // 大种子有几 MB，在阻塞线程上解析
            let (resume, info) = tokio::task::spawn_blocking(move || {
                let resume = torrent::torrent_from_file(&torrent)?;
                let info = TorrentInfo::of(&resume)?;
                Ok::<_, Error>((resume, info))
            })
            .await
            .expect("解析种子文件不会 panic，运行时关闭时本任务也一起丢弃")?;
            torrent_free(&shared, &info.info_hash)?;
            Ok((resume, info))
        })
    }

    /// BT 任务存着的续传数据里的种子信息：任务不在运行时显示要下多少、已下多少。没有这个 BT 任务时报
    /// [`Error::NotFound`]；返回的句柄在任何执行器上都可等待。
    pub fn inspect_torrent(&self, id: TaskId) -> JoinHandle<Result<SavedTorrent, Error>> {
        let shared = self.shared.clone();
        self.runtime().spawn(async move {
            let resume = {
                let mut store = lock(&shared.store);
                let tx = store.transaction()?;
                let stored = store::torrent_resume(&tx, id)?;
                // 停下时要到的还没写进存储的（待补写）比存储里的新
                lock(&shared.unsaved)
                    .get(&id)
                    .and_then(|ending| ending.resume.clone())
                    .unwrap_or(stored)
            };
            // 读种子信息要解析整个续传数据（大种子有几 MB），在阻塞线程上做；存储里的读不出即被改动过
            let info = tokio::task::spawn_blocking(move || TorrentInfo::of(&resume.data))
                .await
                .expect("读种子信息不会 panic，运行时关闭时本任务也一起丢弃")
                .map_err(|error| Error::Corrupt(format!("{id}的续传数据：{error}")))?;
            Ok(SavedTorrent {
                info,
                trusted: resume.trusted,
            })
        })
    }

    /// 按 `draft` 新建任务并开始，返回存进存储的任务；`created_at` 为新建时间（Unix 秒）。输出放在 `draft.folder` 里：
    /// HLS 的主名为任务的标题（见 [`crate::task_title`]），文件的文件名为服务器给的或由地址得出（见
    /// [`crate::file_name`]）；避开磁盘上已有的与存储里各任务占用的输出与任务目录（不分大小写与 Unicode 写法），被占用时
    /// 主名后加编号；任务的标题即这个主名（文件为文件名），任务目录为核心的默认位置。`draft.folder` 不是绝对路径、
    /// 选轨偏好存不了时报 [`Error::InvalidInput`]。取名要查磁盘（网络盘上可能慢），在运行时的阻塞线程上、存储锁外做，
    /// 10 秒内查不完报 [`Error::TimedOut`]；返回的句柄在任何执行器上都可等待。存储写不进时什么也不留：已开始的任务
    /// 取消，它建出的任务目录删掉，磁盘上还留下东西时发 [`Event::Orphaned`]。
    pub fn create(&self, draft: Draft, created_at: i64) -> JoinHandle<Result<Task, Error>> {
        let shared = self.shared.clone();
        self.runtime().spawn(async move {
            shared
                .establish(draft, |spec, work_dir, resume| {
                    shared.insert_and_start(spec, work_dir, resume, created_at)
                })
                .await
        })
    }

    /// 继续暂停或失败的任务：照新建时的请求再开始，任务目录里已完成的不重下（文件任务在服务器不支持范围请求或没给出
    /// 大小时从头下）；之前选了只写出已录的，从此改回续录
    /// （[`Task::write_only`] 为否）。在运行、正在放弃、已完成、放弃没做完的报 [`Error::Running`]、[`Error::Discarding`]、
    /// [`Error::AlreadyDone`]、[`Error::DiscardUnfinished`]，没有自己的任务目录的报 [`Error::NoWorkDir`]。返回开始后的
    /// 任务（运行中）。存储写不进时任务已开始，随即取消，停下后照常发 [`Event::Ended`]。
    pub fn resume(&self, id: TaskId) -> Result<Task, Error> {
        let _entered = self.runtime().enter();
        self.shared.resume(id, Run::Resume)
    }

    /// 不再续录，只把停着的直播录制任务目录里已录的写成输出（见 [`Engine::merge_recorded`]）：用于来源变了、接不上，
    /// 或不想再录的时候。记下这个选择（[`Task::write_only`]），之后由 [`Handle::resume`] 改回续录。拒绝的情形同
    /// [`Handle::resume`]；目录里是点播的下载、或没有录到可合并的分片时，任务以失败
    /// （[`crate::FailureKind::NothingToWrite`]）停下。返回开始后的任务，停下后照常发 [`Event::Ended`]。
    pub fn write_recorded(&self, id: TaskId) -> Result<Task, Error> {
        let _entered = self.runtime().enter();
        self.shared.resume(id, Run::WriteRecorded)
    }

    /// 把停着的任务的输出改为只要本地 HLS，任务目录不变，已下的分片照样用：用于内容放不进 MP4
    /// （[`crate::FailureKind::Mp4Unsupported`]）之后，再继续或只写出已录的。只要 MP4 的，目录与 MP4 同名、去掉扩展名；
    /// 两者都要的留下原来的本地 HLS；只要本地 HLS 的不变。返回改后的任务。新路径已被别的任务占着时报
    /// [`Error::PathTaken`]；拒绝的情形同 [`Handle::set_headers`]。
    pub fn switch_to_hls(&self, id: TaskId) -> Result<Task, Error> {
        self.shared.change(|tx| {
            let mut task = self.shared.stopped(tx, id)?;
            let Kind::Hls(spec) = &mut task.spec.kind else {
                return Err(Error::NotHls(id));
            };
            let hls = match &spec.output {
                OutputSpec::Hls(_) => return Ok(task),
                OutputSpec::Mp4(mp4) => mp4.with_extension(""),
                OutputSpec::Both { hls, .. } => hls.clone(),
            };
            spec.output = OutputSpec::Hls(hls);
            store::set_output(tx, id, &spec.output)?;
            Ok(task)
        })
    }

    /// 把停着的网页视频任务的输出容器从 MP4 改为 MKV（输出的扩展名随之改为 `.mkv`），任务目录不变，已下的各路照样用：
    /// 用于合并或转封装时内容放不进 MP4（[`crate::FailureKind::Mp4Unsupported`]）之后再继续。已是 MKV 的不变；保持原样
    /// 的（一路、不转封装）报 [`Error::InvalidInput`]。返回改后的任务。新路径已被别的任务占着时报 [`Error::PathTaken`]；拒绝的情形同
    /// [`Handle::set_headers`]。
    pub fn switch_web_to_matroska(&self, id: TaskId) -> Result<Task, Error> {
        self.shared.change(|tx| {
            let mut task = self.shared.stopped(tx, id)?;
            let Kind::Web(spec) = &mut task.spec.kind else {
                return Err(Error::InvalidInput(format!("{id}不是网页视频任务")));
            };
            match spec.container {
                crate::web::Container::Matroska => return Ok(task),
                crate::web::Container::Original => {
                    return Err(Error::InvalidInput(format!("{id}保持原样，不转封装")));
                }
                crate::web::Container::Mp4 => {}
            }
            spec.container = crate::web::Container::Matroska;
            spec.path = spec.path.with_extension("mkv");
            store::set_web_output(tx, id, spec.container, &spec.path)?;
            Ok(task)
        })
    }

    /// 存储里的任务 `id`，停下了而状态还没补写进存储的按停下时的状态；没有时报 [`Error::NotFound`]。
    pub fn task(&self, id: TaskId) -> Result<Task, Error> {
        self.shared.task(id)
    }

    /// 任务目录里已完成的内容（见 [`Engine::inspect`]）：任务不在运行时显示已下了多少、继续前算出续传的起点。任务没有
    /// 自己的任务目录时为 None；10 秒内读不完报 [`Error::TimedOut`]。返回的句柄在任何执行器上都
    /// 可等待。
    pub fn inspect(&self, id: TaskId) -> JoinHandle<Result<Option<Saved>, Error>> {
        let shared = self.shared.clone();
        self.runtime().spawn(async move {
            let Some(work_dir) = shared.get(id)?.work_dir else {
                return Ok(None);
            };
            tokio::time::timeout(INSPECT_TIMEOUT, shared.engine.inspect(&work_dir))
                .await
                .map_err(|_| Error::TimedOut {
                    action: format!("读{id}的任务目录"),
                    after: INSPECT_TIMEOUT,
                })?
                .map_err(Error::from)
        })
    }

    /// 停着的网页视频任务在任务目录里已下的字节（见 `web_task::saved_web`）：不在运行时显示已下了多少。不是网页视频任务
    /// 时报 [`Error::InvalidInput`]；10 秒内读不完报 [`Error::TimedOut`]。返回的句柄在任何执行器上都可等待。
    pub fn inspect_web(&self, id: TaskId) -> JoinHandle<Result<u64, Error>> {
        let shared = self.shared.clone();
        self.runtime().spawn(async move {
            let task = shared.get(id)?;
            let (Some(spec), Some(work_dir)) = (task.spec.web(), task.work_dir.as_deref()) else {
                return Err(Error::InvalidInput(format!("{id}不是网页视频任务")));
            };
            tokio::time::timeout(
                INSPECT_TIMEOUT,
                web_task::saved_web(&shared.engine, spec, work_dir),
            )
            .await
            .map_err(|_| Error::TimedOut {
                action: format!("读{id}的任务目录"),
                after: INSPECT_TIMEOUT,
            })?
        })
    }

    /// 从存储里删掉任务，输出不动：用于已完成的（任务目录已在完成时删掉，输出归用户；BT 另删保存目录里它的 `.parts`
    /// 文件，删不掉时发 [`Event::Orphaned`]）与没有自己的任务目录的 HLS、文件任务。别的报 [`Error::NotFinished`]：它们
    /// 的任务目录（BT 为下了的文件）要用 [`Handle::discard`] 一起删，还没定下的任务也只经它删；在运行的报 [`Error::Running`]。在做种的先停止做种（同 [`Handle::stop_seeding`]，
    /// 记不进存储时报那个错，做种照旧），停下后再删；等的期间又开始做种时报 [`Error::Seeding`]。返回的句柄在任何执行器
    /// 上都可等待。
    pub fn forget(&self, id: TaskId) -> JoinHandle<Result<(), Error>> {
        let shared = self.shared.clone();
        let stopping = shared.stop_seeding(id);
        let _entered = self.runtime().enter();
        self.shared.tracker.spawn(async move {
            if let Some(mut ended) = stopping? {
                // 发送端随做种的任务结束而释放：那时已停下
                let _ended = ended.wait_for(|ended| *ended).await;
            }
            shared.forget(id)
        })
    }

    /// 换掉停着的任务（暂停、失败，或还没定下、在等用户）的请求头，之后继续时照它请求。名与值不合要求（见
    /// [`check_option_header`]）、或任务没有请求头（BT、云离线，来源是磁力链接或种子文件的）的报
    /// [`Error::InvalidInput`]；任务在运行（含准备中、停在网页上等用户：后者换请求头经 [`Answer::Headers`]）或正在放弃时报 [`Error::Running`]、[`Error::Discarding`]，已完成、
    /// 放弃没做完的报 [`Error::AlreadyDone`]、[`Error::DiscardUnfinished`]。
    pub fn set_headers(&self, id: TaskId, headers: Vec<(String, String)>) -> Result<(), Error> {
        for (name, value) in &headers {
            check_option_header(name, value).map_err(Error::InvalidInput)?;
        }
        self.shared
            .change(|tx| {
                let mut task = self.shared.stopped(tx, id)?;
                let headerless = task.spec.torrent().is_some()
                    || task.spec.cloud().is_some()
                    || intake::torrent_source(&task.spec.url);
                if headerless {
                    return Err(Error::InvalidInput(format!("{id}没有请求头")));
                }
                if task.spec.web().is_some() {
                    web_task::check_headers(&headers)?;
                }
                store::set_headers(tx, id, &headers)?;
                task.spec.headers = headers;
                Ok(task)
            })
            .map(drop)
    }

    /// 暂停：取消运行中的任务，保留任务目录；停下后状态为 [`Status::Paused`]。准备中的任务停掉在做的那一步（杀掉
    /// 这次解析、不再取种子信息、关掉内置网页并让出名额），之后继续时从头准备；停在网页上等用户的任务同样。没在运行
    /// 时（含别的停下等用户的）什么也不做。
    pub fn pause(&self, id: TaskId) {
        if let Some(running) = lock(&self.shared.running).get(&id) {
            running.control.cancel();
        }
    }

    /// 停止录制：把已列出的分片下完后写出（见 [`JobControl::stop`]）。没在运行时、或任务是点播或文件时什么也不做。
    pub fn stop(&self, id: TaskId) {
        if let Some(running) = lock(&self.shared.running).get(&id) {
            running.control.stop();
        }
    }

    /// 放弃任务：在运行就先取消并等它停下，把状态记为 [`Status::DiscardUnfinished`]，再删掉任务目录（见 [`Engine::discard`]；
    /// BT 任务为 libtorrent 建的文件、空了的目录与存部分块的 `.parts` 文件）与存储里的记录；返回留下的东西（BT 任务
    /// 没有）。还没定下的任务磁盘上没有东西，停下后直接删记录。删了记录发 [`Event::Removed`]。
    /// 任务目录没删干净时记录留着，报 [`Error::WorkDirLeft`]（BT 任务的文件没删干净时报 [`Error::ContentLeft`]）；目录删了而记录没删掉时报 [`Error::RecordNotDeleted`]；
    /// 30 秒内做不完报 [`Error::TimedOut`]（之后再放弃从做到的地方接着做）。做完之前任务记为正在放弃，继续与移除都报
    /// [`Error::Discarding`]，再放弃一次同样报这个错误。返回的句柄在任何执行器上都可等待。
    pub fn discard(&self, id: TaskId) -> JoinHandle<Result<Vec<Leftover>, Error>> {
        let stopped = {
            let _store = lock(&self.shared.store);
            if !lock(&self.shared.discarding).insert(id) {
                return self
                    .runtime()
                    .spawn(async move { Err(Error::Discarding(id)) });
            }
            lock(&self.shared.running).get(&id).map(|running| {
                running.control.cancel();
                running.stopped.clone()
            })
        };
        let guard = DiscardGuard {
            id,
            shared: self.shared.clone(),
        };
        let _entered = self.runtime().enter();
        self.shared.tracker.spawn(async move {
            let shared = &guard.shared;
            let discarding = run_discard(id, shared, stopped);
            let result = tokio::time::timeout(DISCARD_TIMEOUT, discarding)
                .await
                .unwrap_or_else(|_| {
                    Err(Error::TimedOut {
                        action: format!("放弃{id}"),
                        after: DISCARD_TIMEOUT,
                    })
                });
            if result.is_err() {
                // 没放弃成而记录还在：状态多半已改为放弃没做完。读不出时报的仍是放弃的错误
                let mut store = lock(&shared.store);
                if let Ok(task) = shared.task_in(&mut store, id) {
                    shared.events.send(Event::Changed {
                        task: Box::new(task),
                    });
                }
            }
            drop(guard);
            result
        })
    }
}

impl Drop for Service {
    /// 还没 [`Service::close`] 时先关闭，再关掉运行时（阻塞线程最多等 2 秒）；没停下的任务随运行时丢弃。
    fn drop(&mut self) {
        if !self.shared.closed.load(Ordering::Relaxed) {
            // 释放时没有调用方能处理补写的错误，结果见 close 的说明；要知道结果的调用方先调 close
            let _unsaved = self.close();
        }
        let runtime = self.runtime.take().expect("运行时只在这里取走");
        runtime.shutdown_timeout(CLOSE_TIMEOUT);
    }
}

/// 放弃中的登记：放弃做完、超时，或运行时关闭时随任务一起丢弃，都从放弃中集合里去掉。
struct DiscardGuard {
    id: TaskId,
    shared: Arc<Shared>,
}

impl Drop for DiscardGuard {
    fn drop(&mut self) {
        lock(&self.shared.discarding).remove(&self.id);
    }
}

/// 从 `sources` 取公共 tracker 列表，经设置里的代理（每次取时读），结果交给 BT 引擎（[`Bt::set_public`]）；没取到隔一阵
/// 再取（[`public_trackers::FIRST_RETRY`] 起每次翻倍），其间代理设置改了即刻再取、间隔从头算；取到或服务已释放即止。
async fn fetch_public_trackers(shared: Weak<Shared>, engine: Engine, sources: Vec<Url>) {
    let mut wait = public_trackers::FIRST_RETRY;
    loop {
        let proxy = match shared.upgrade() {
            Some(shared) => {
                let mut store = lock(&shared.store);
                store
                    .transaction()
                    .and_then(|tx| store::settings(&tx))
                    .map(|settings| settings.proxy)
            }
            None => return,
        };
        let fetched = match proxy {
            Ok(proxy) => public_trackers::fetch(&engine, &sources, &proxy).await,
            Err(error) => Err(format!("读不出代理设置：{error}")),
        };
        let Some(shared) = shared.upgrade() else {
            return;
        };
        match fetched {
            Ok(trackers) => {
                shared.bt.set_public(PublicTrackers::Ready(trackers.into()));
                return;
            }
            Err(why) => shared.bt.set_public(PublicTrackers::Failed(why)),
        }
        // 等的时候不持有服务
        let proxy_changed = shared.proxy_changed.clone();
        drop(shared);
        let changed = tokio::select! {
            () = proxy_changed.notified() => true,
            () = tokio::time::sleep(wait) => false,
        };
        wait = if changed {
            public_trackers::FIRST_RETRY
        } else {
            (wait * 2).min(public_trackers::LAST_RETRY)
        };
    }
}

/// [`Handle::discard`] 的步骤：等任务停下、删任务目录、删存储里的记录。
async fn run_discard(
    id: TaskId,
    shared: &Arc<Shared>,
    stopped: Option<watch::Receiver<bool>>,
) -> Result<Vec<Leftover>, Error> {
    if let Some(mut stopped) = stopped {
        // 运行登记释放时先置为 true 再丢弃发送端，等到值或发送端已丢弃都说明任务已停下
        let _stopped = stopped.wait_for(|done| *done).await;
    }
    // 还没定下的任务磁盘上没有东西（取种子信息加进会话的种子已随那一轮准备停下而移除）
    let pending = {
        let mut store = lock(&shared.store);
        let pending = shared.write_in(&mut store, |tx| {
            let task = store::get(tx, id)?.ok_or(Error::NotFound(id))?;
            if task.spec.pending().is_none() {
                return Ok(false);
            }
            store::delete(tx, id)?;
            Ok(true)
        })?;
        if pending {
            lock(&shared.offers).remove(&id);
            shared.events.send(Event::Removed { id });
        }
        pending
    };
    if pending {
        return Ok(Vec::new());
    }
    // 先记为放弃没做完：之后被打断、超时或删不掉记录，任务都停在这个状态，不能继续，只能再放弃
    shared.write(|tx| store::set_status(tx, id, &Status::DiscardUnfinished))?;
    let task = shared.get(id)?;
    if task.spec.web().is_some() {
        shared.discard_web(id, task.work_dir.as_deref()).await?;
        return shared
            .remove(id)
            .map(|()| Vec::new())
            .map_err(|cause| Error::RecordNotDeleted {
                id,
                leftovers: Vec::new(),
                cause: Box::new(cause),
            });
    }
    if let Some(spec) = task.spec.cloud() {
        shared
            .discard_cloud(id, spec, task.work_dir.as_deref())
            .await?;
        return shared
            .remove(id)
            .map(|()| Vec::new())
            .map_err(|cause| Error::RecordNotDeleted {
                id,
                leftovers: Vec::new(),
                cause: Box::new(cause),
            });
    }
    if let Some(spec) = task.spec.torrent() {
        let resume = {
            let mut store = lock(&shared.store);
            let tx = store.transaction()?;
            store::torrent_resume(&tx, id)?
        };
        let engine = shared.bt_engine()?;
        torrent::delete(engine, &spec.info_hash, &resume.data, &spec.root)
            .await
            .map_err(|cause| Error::ContentLeft {
                id,
                cause: Box::new(cause),
            })?;
        return shared
            .remove(id)
            .map(|()| Vec::new())
            .map_err(|cause| Error::RecordNotDeleted {
                id,
                leftovers: Vec::new(),
                cause: Box::new(cause),
            });
    }
    let leftovers = match task.work_dir {
        // 放弃超时时这个 future 被丢弃，核心在步与步之间停下，再放弃时从做到的地方接着做
        Some(work_dir) => {
            shared
                .engine
                .discard(&work_dir, Unsettled::Fail, &CancellationToken::new())
                .await?
        }
        None => Vec::new(),
    };
    if leftovers
        .iter()
        .any(|leftover| leftover.kind == LeftoverKind::WorkDir)
    {
        return Err(Error::WorkDirLeft { id, leftovers });
    }
    // 核心报出的残留已没有记录，只在这里报一次，删不掉记录时也要带着
    if let Err(cause) = shared.remove(id) {
        return Err(Error::RecordNotDeleted {
            id,
            leftovers,
            cause: Box::new(cause),
        });
    }
    Ok(leftovers)
}

/// 运行中任务的登记：任务停下（或运行时关闭时收尾的任务被丢弃）时从运行表里去掉，并告诉等它停下的放弃。
struct RunningGuard {
    id: TaskId,
    /// 见 [`Running::serial`]
    serial: u64,
    shared: Arc<Shared>,
    stopped: watch::Sender<bool>,
    /// 登记的时刻：停下时算这次运行了多久
    started: Instant,
}

impl RunningGuard {
    /// 任务停下时把这次运行了多久、完成的时刻（状态为 [`Status::Done`] 时）记进 `ending`。
    fn stamp(&self, ending: &mut Ending) {
        ending.ran = self.started.elapsed();
        if ending.status == Status::Done {
            ending.finished_at = Some(unix_now());
        }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let removed = {
            let mut running = lock(&self.shared.running);
            let mine = running
                .get(&self.id)
                .is_some_and(|running| running.serial == self.serial);
            mine.then(|| running.remove(&self.id)).flatten()
        };
        // 登记里的网页名额在 `running` 锁外让出（各把锁之间不嵌套）
        drop(removed);
        self.stopped.send_replace(true);
    }
}

/// 现在的时刻，Unix 秒。
fn unix_now() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟不早于 1970 年");
    i64::try_from(since.as_secs()).expect("Unix 秒远小于 i64 的上限")
}

impl Shared {
    /// 把 PikPak 账号记成 `account`（None 为退出登录），提交了才换上登录状态 `session`、作废旧的；提交与换在 `store` 锁内
    /// 做，与旧登录状态换了令牌要写存储的（见 [`Shared::new_pikpak_session`]）不会交错。写不进时什么也不换。
    fn set_pikpak(
        &self,
        account: Option<&pikpak::Account>,
        session: Option<Arc<Session>>,
    ) -> Result<(), Error> {
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        store::set_pikpak_account(&tx, account)?;
        self.commit(tx, applied)?;
        self.replace_pikpak(session);
        Ok(())
    }

    /// 在一个事务里先补写没写进去的结束状态，再做 `f`，然后提交（见 [`Shared::commit`]）。
    fn write<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T, Error>) -> Result<T, Error> {
        self.write_in(&mut lock(&self.store), f)
    }

    /// 同 [`Shared::write`]，调用方已持着 `store` 锁。
    fn write_in<T>(
        &self,
        store: &mut Store,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        let value = f(&tx)?;
        self.commit(tx, applied)?;
        Ok(value)
    }

    /// 同 [`Shared::write`]，`f` 给出改了之后的任务：提交后在 `store` 锁内发 [`Event::Changed`]，返回它。
    fn change(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<Task, Error>,
    ) -> Result<Task, Error> {
        let mut store = lock(&self.store);
        let task = self.write_in(&mut store, f)?;
        self.events.send(Event::Changed {
            task: Box::new(task.clone()),
        });
        Ok(task)
    }

    /// 删掉任务 `id` 的记录，在 `store` 锁内发 [`Event::Removed`]。
    fn remove(&self, id: TaskId) -> Result<(), Error> {
        let mut store = lock(&self.store);
        self.write_in(&mut store, |tx| store::delete(tx, id))?;
        self.events.send(Event::Removed { id });
        Ok(())
    }

    /// 存储里的任务 `id`，停下了而状态还没补写进存储的按停下时的状态；没有时报 [`Error::NotFound`]。
    fn task(&self, id: TaskId) -> Result<Task, Error> {
        self.task_in(&mut lock(&self.store), id)
    }

    /// 同 [`Shared::task`]，调用方已持着 `store` 锁。
    fn task_in(&self, store: &mut Store, id: TaskId) -> Result<Task, Error> {
        let tx = store.transaction()?;
        let mut task = store::get(&tx, id)?.ok_or(Error::NotFound(id))?;
        if let Some(ending) = lock(&self.unsaved).get(&id) {
            ending.overlay(&mut task);
        }
        Ok(task)
    }

    /// 提交补写了 `applied`（[`Shared::apply_unsaved`] 的返回）的事务 `tx`；提交了的补写从待补写里去掉，其间同一任务
    /// 又记了新的结束状态时留着新的。调用方持着 `store` 锁直到返回，提交与清理之间没有别的写入。
    fn commit(&self, tx: Transaction<'_>, applied: Vec<(TaskId, Ending)>) -> Result<(), Error> {
        tx.commit()?;
        let mut unsaved = lock(&self.unsaved);
        for (id, ending) in applied {
            if unsaved.get(&id) == Some(&ending) {
                unsaved.remove(&id);
            }
        }
        Ok(())
    }

    /// 停着的任务 `id`（暂停、失败或停下等用户；存储里标着运行中、准备中而已不在运行的按停着算）：在运行（含准备中）、
    /// 正在放弃、已完成、放弃没做完的
    /// 报 [`Error::Running`]、[`Error::Discarding`]、[`Error::AlreadyDone`]、[`Error::DiscardUnfinished`]，没有时报
    /// [`Error::NotFound`]。调用方持着 `store` 锁（`tx` 是它的事务），检查与之后的写入之间任务不会开始或开始放弃。
    fn stopped(&self, tx: &Transaction<'_>, id: TaskId) -> Result<Task, Error> {
        if lock(&self.running).contains_key(&id) {
            return Err(Error::Running(id));
        }
        if lock(&self.discarding).contains(&id) {
            return Err(Error::Discarding(id));
        }
        let task = store::get(tx, id)?.ok_or(Error::NotFound(id))?;
        match task.status {
            Status::Done => Err(Error::AlreadyDone(id)),
            Status::DiscardUnfinished => Err(Error::DiscardUnfinished(id)),
            Status::Preparing
            | Status::Waiting(_)
            | Status::Running
            | Status::Paused
            | Status::Failed(_) => Ok(task),
        }
    }

    /// 存储里的任务 `id`；没有时报 [`Error::NotFound`]。
    fn get(&self, id: TaskId) -> Result<Task, Error> {
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        store::get(&tx, id)?.ok_or(Error::NotFound(id))
    }

    /// 磁力链接 `uri` 取种子信息（见 [`Handle::fetch_metadata`]），不查种子在不在任务里。
    fn fetch_metadata(self: &Arc<Self>, uri: String) -> Result<MetadataFetch, Error> {
        let engine = self.bt_engine()?;
        let (progress, receiver) = watch::channel(MetadataProgress::default());
        let probe_dir = self.bt.probe_dir().to_path_buf();
        let slot = torrent::FetchSlot::default();
        let handle = self.runtime.spawn(torrent::fetch_metadata(
            engine,
            uri,
            probe_dir,
            progress,
            slot.clone(),
        ));
        Ok(MetadataFetch {
            progress: receiver,
            resume: MetadataHandle {
                task: AbortOnDropHandle::new(handle),
                slot,
            },
        })
    }

    /// BT 引擎；还没有时照存储里上次关闭时的端口与 DHT 状态建会话（见 [`Shared::torrent_engine`]）。不持 `store` 锁调用。
    fn bt_engine(self: &Arc<Self>) -> Result<Arc<torrent::Engine>, Error> {
        let saved = {
            let mut store = lock(&self.store);
            let tx = store.transaction()?;
            store::torrent_session(&tx)?
        };
        self.torrent_engine(saved)
    }

    /// BT 引擎；还没有时照 `saved` 建会话（见 [`Bt::engine`]），第一次建时开始取公共 tracker 列表。
    fn torrent_engine(
        self: &Arc<Self>,
        saved: torrent::SessionState,
    ) -> Result<Arc<torrent::Engine>, Error> {
        let engine = self.bt.engine(saved)?;
        if let Some(sources) = self.bt.begin_public() {
            let fetching = self.runtime.spawn(fetch_public_trackers(
                Arc::downgrade(self),
                self.engine.clone(),
                sources,
            ));
            *lock(&self.public_fetch) = Some(AbortOnDropHandle::new(fetching));
        }
        self.watch_torrent_proxy();
        Ok(engine)
    }

    async fn check_network(self: &Arc<Self>) -> Result<crate::NetworkReport, Error> {
        use crate::netcheck::{self, DHT_BOOTSTRAP, Udp};
        let socket = |error: std::io::Error| Error::Io {
            action: "打开",
            path: "检测网络用的 UDP 套接字".into(),
            cause: error,
        };
        let engine = self.bt.existing();
        if let Some(engine) = &engine {
            engine.request_stats();
        }
        let (v4, trackers) = (
            Udp::direct(false).await.map_err(socket)?,
            Udp::direct(false).await.map_err(socket)?,
        );
        let direct = async {
            let (found, unresolved) = netcheck::bootstrap(&DHT_BOOTSTRAP, false).await;
            netcheck::dht(&v4, &found, unresolved).await
        };
        let ipv6 = async {
            if !netcheck::has_public_ipv6().await {
                return None;
            }
            Some(match Udp::direct(true).await {
                Ok(udp) => {
                    let (found, unresolved) = netcheck::bootstrap(&DHT_BOOTSTRAP, true).await;
                    let measured = netcheck::dht(&udp, &found, unresolved).await;
                    match measured.failure {
                        Some(why) => Err(why),
                        None => Ok(measured.bootstrap),
                    }
                }
                Err(error) => Err(error.to_string()),
            })
        };
        let udp_trackers = async {
            match self.bt.public() {
                PublicTrackers::Ready(list) => netcheck::udp_trackers(&trackers, &list).await,
                PublicTrackers::Off => Err("不补公共 tracker".to_owned()),
                PublicTrackers::Idle => Err("公共 tracker 列表在 BT 用过之后才取".to_owned()),
                PublicTrackers::Fetching => Err("公共 tracker 列表还在取".to_owned()),
                PublicTrackers::Failed(why) => Err(format!("公共 tracker 列表没取到（{why}）")),
            }
        };
        let proxy_url = self.bt.proxy_url();
        let proxied = async {
            let url = proxy_url.as_ref()?;
            Some(netcheck::through_proxy(url, &DHT_BOOTSTRAP).await)
        };
        let (dht, ipv6, trackers, proxied) = tokio::join!(direct, ipv6, udp_trackers, proxied);
        let proxy = match (proxy_url, proxied) {
            (Some(url), Some(through)) => {
                let through = through.map_err(socket)?;
                // 记进 BT 引擎（取种子信息的界面与诊断随之更新），报告照记下的出
                let check = self.bt.record_proxy_check(
                    &url,
                    netcheck::judge_proxy(&through),
                    netcheck::resolved(&through),
                );
                Some(netcheck::Proxied {
                    address: hs_m3u8_core::bare_url(&url),
                    check,
                    dht: through.ok(),
                })
            }
            _ => None,
        };
        Ok(netcheck::report(&netcheck::Measured {
            dht,
            trackers,
            ipv6,
            proxy,
            session: engine.map(|engine| engine.session_facts()),
        }))
    }

    /// 照现在的 BT 代理看着它：已在看着同一个地址时不动；改了或改成直连时停下看着旧的，设了时开始看着新的。开始时探
    /// 一次（之前记下的结果可能是 BT 会话建起前量的）；之后每 [`PROXY_RECHECK`] 看一眼，还没探、探出用不了，或
    /// libtorrent 经它报过错时再探，能用时每 [`PROXY_RECHECK_USABLE`] 复查一次。探只经代理发包（见
    /// [`crate::netcheck::probe_proxy`]），探到的记下（见 [`Bt::set_proxy_check`]）。服务释放即止。
    fn watch_torrent_proxy(self: &Arc<Self>) {
        let mut watching = lock(&self.proxy_probe);
        // 在锁内读：与同时改代理的不会把新地址的看护换成旧地址的
        let url = self.bt.proxy_url();
        if let (Some((watched, _)), Some(url)) = (&*watching, &url)
            && watched == url
        {
            return;
        }
        *watching = None;
        let Some(url) = url else {
            return;
        };
        let shared = Arc::downgrade(self);
        let hosts = self.bt.dht_bootstrap();
        let watched = url.clone();
        let probing = self.runtime.spawn(async move {
            let mut ticks = tokio::time::interval(PROXY_RECHECK);
            let mut last_probe: Option<tokio::time::Instant> = None;
            loop {
                ticks.tick().await;
                let stale = last_probe.is_none_or(|at| at.elapsed() >= PROXY_RECHECK_USABLE);
                match shared.upgrade().map(|shared| shared.bt.proxy_due(&url)) {
                    Some(Some(due)) if due || stale => {}
                    Some(Some(_)) => continue,
                    // 代理改了或服务已释放
                    Some(None) | None => return,
                }
                last_probe = Some(tokio::time::Instant::now());
                let (check, fake) = crate::netcheck::probe_proxy(&url, hosts).await;
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                shared.bt.set_proxy_check(&url, check, fake);
            }
        });
        *watching = Some((watched, AbortOnDropHandle::new(probing)));
    }

    /// 让下完的 BT 任务 `id` 做种（见 [`Handle::seed`]）：在 `store` 锁内核对、记下在做种并登记，再在运行时里加入会话；
    /// 调用方须在运行时里。
    fn start_seeding(self: &Arc<Self>, id: TaskId) -> Result<(), Error> {
        let (engine, info_hash, resume, root) = {
            let mut store = lock(&self.store);
            if self.closed.load(Ordering::Relaxed) {
                return Err(Error::Closed);
            }
            if lock(&self.seeders).contains_key(&id) {
                return Err(Error::Seeding(id));
            }
            let tx = store.transaction()?;
            let applied = self.apply_unsaved(&tx)?;
            let task = store::get(&tx, id)?.ok_or(Error::NotFound(id))?;
            let spec = match (&task.status, task.spec.torrent()) {
                (Status::Done, Some(spec)) => spec.clone(),
                _ => {
                    return Err(Error::InvalidInput(format!(
                        "{id}不是下完的 BT 任务，不能做种"
                    )));
                }
            };
            let engine = self.torrent_engine(store::torrent_session(&tx)?)?;
            let resume = store::torrent_resume(&tx, id)?;
            store::set_seeding_task(&tx, id, true)?;
            self.commit(tx, applied)?;
            self.events.send(Event::Changed {
                task: Box::new(Task {
                    seeding: true,
                    ..task
                }),
            });
            (engine, spec.info_hash, resume, spec.root)
        };
        let started = async move {
            let folder = root
                .parent()
                .expect("内容的根是以文件名结尾的绝对路径")
                .to_path_buf();
            torrent::start_seeding(engine, &info_hash, &resume, &folder).await
        };
        self.track_seeding(id, started);
        Ok(())
    }

    /// 登记任务 `id` 在做种，等 `started` 给出加进会话的种子后做种（期间每 5 分钟存一次进度），直到停下；停下时存下进度，
    /// 发出 [`Event::SeedingEnded`]。应用退出时停下的（用户没先停止做种，也不是到了条件或文件不全）记着在做种，下次打开
    /// 接着做，别的记为不再做种。调用方须在运行时里。
    fn track_seeding(
        self: &Arc<Self>,
        id: TaskId,
        started: impl Future<Output = Result<Seeder, Error>> + Send + 'static,
    ) {
        let cancel = CancellationToken::new();
        let by_user = Arc::new(AtomicBool::new(false));
        let (ended_tx, ended) = watch::channel(false);
        {
            let _store = lock(&self.store);
            lock(&self.seeders).insert(
                id,
                SeedControl {
                    cancel: cancel.clone(),
                    by_user: by_user.clone(),
                    ended,
                },
            );
            // 关闭之后才登记的（下完时正赶上退出）随即停下
            if self.closed.load(Ordering::Relaxed) {
                cancel.cancel();
            }
        }
        self.events.send(Event::SeedingStarted { id });
        let shared = self.clone();
        let policy = self.seeding.subscribe();
        self.tracker.spawn(async move {
            let events = shared.events.clone();
            let seeded = match started.await {
                Ok(seeder) => {
                    let saving = shared.clone();
                    torrent::seed(
                        seeder,
                        policy,
                        cancel,
                        |progress| {
                            events.send(Event::TorrentProgress {
                                id,
                                progress: Box::new(progress),
                            });
                        },
                        |resume| {
                            let saved = resume.and_then(|resume| {
                                saving
                                    .write(|tx| store::set_torrent_resume(tx, id, &resume))
                                    .map_err(|error| error.to_string())
                            });
                            if let Err(reason) = saved {
                                saving.events.send(Event::SeedingNotSaved { id, reason });
                            }
                        },
                    )
                    .await
                }
                Err(error) => torrent::Seeded {
                    end: SeedEnd::Failed(error.to_string()),
                    resume: None,
                },
            };
            // 应用退出时停下的（停下时出了错也算），下次打开接着做
            let keep = shared.closed.load(Ordering::Relaxed)
                && !by_user.load(Ordering::Relaxed)
                && !matches!(seeded.end, SeedEnd::Reached | SeedEnd::Incomplete);
            let saved = shared.change(|tx| {
                if let Some(resume) = &seeded.resume {
                    store::set_torrent_resume(tx, id, resume)?;
                }
                store::set_seeding_task(tx, id, keep)?;
                store::get(tx, id)?.ok_or(Error::NotFound(id))
            });
            lock(&shared.seeders).remove(&id);
            ended_tx.send_replace(true);
            let unsaved = saved.err().map(|error| error.to_string());
            shared.events.send(Event::SeedingEnded {
                id,
                end: seeded.end,
                unsaved,
            });
        });
    }

    /// 见 [`Handle::stop_seeding`]；在做种时返回它停下的信号。
    fn stop_seeding(&self, id: TaskId) -> Result<Option<watch::Receiver<bool>>, Error> {
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        let Some((cancel, by_user, ended)) = lock(&self.seeders).get(&id).map(|seeder| {
            (
                seeder.cancel.clone(),
                seeder.by_user.clone(),
                seeder.ended.clone(),
            )
        }) else {
            return Ok(None);
        };
        store::set_seeding_task(&tx, id, false)?;
        self.commit(tx, applied)?;
        // 提交了才停（提交失败时做种照旧，与报的错一致），仍在 `store` 锁内：与关闭的先后分明
        by_user.store(true, Ordering::Relaxed);
        cancel.cancel();
        Ok(Some(ended))
    }

    /// 见 [`Handle::forget`]：不在做种的任务。在做种的报 [`Error::Seeding`]，在运行的（含刚停下、还在收尾的）报
    /// [`Error::Running`]。删了记录在 `store` 锁内发 [`Event::Removed`]。
    fn forget(&self, id: TaskId) -> Result<(), Error> {
        let mut store = lock(&self.store);
        let part = self.write_in(&mut store, |tx| {
            if lock(&self.running).contains_key(&id) {
                return Err(Error::Running(id));
            }
            if lock(&self.discarding).contains(&id) {
                return Err(Error::Discarding(id));
            }
            if lock(&self.seeders).contains_key(&id) {
                return Err(Error::Seeding(id));
            }
            let task = store::get(tx, id)?.ok_or(Error::NotFound(id))?;
            let unfinished = task.work_dir.is_some()
                || task.spec.torrent().is_some()
                || task.spec.pending().is_some();
            if task.status != Status::Done && unfinished {
                return Err(Error::NotFinished(id));
            }
            // BT：没选的文件与选了的共用的块里没选的部分存在保存目录里的 .parts（做种时要读），不归用户；记录删了之后在
            // 后台删
            let part = match task.spec.torrent() {
                Some(spec) => Some((torrent_folder(spec), store::torrent_resume(tx, id)?.data)),
                None => None,
            };
            store::delete(tx, id)?;
            Ok(part)
        })?;
        self.events.send(Event::Removed { id });
        drop(store);
        if let Some((folder, resume)) = part {
            self.forget_part_file(folder, resume);
        }
        Ok(())
    }

    /// 在运行时的阻塞线程上删掉移除了的 BT 任务在保存目录 `folder` 里的 `.parts`（名字由续传数据 `resume` 得出）；删不掉
    /// 或读不出名字时发 [`Event::Orphaned`]。调用方须在运行时里。
    fn forget_part_file(&self, folder: PathBuf, resume: Vec<u8>) {
        let events = self.events.clone();
        self.tracker.spawn(async move {
            let removed = tokio::task::spawn_blocking(move || {
                let name = TorrentInfo::of(&resume)
                    .map_err(|error| (folder.clone(), format!("读不出 .parts 的名字：{error}")))?
                    .part_file;
                let part = folder.join(name);
                match std::fs::remove_file(&part) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err((part, format!("删不掉：{error}"))),
                }
            })
            .await
            .expect("删文件不会 panic，运行时关闭时本任务也一起丢弃");
            if let Err((path, reason)) = removed {
                events.send(Event::Orphaned {
                    paths: vec![path],
                    reason,
                });
            }
        });
    }

    /// 把待补写的结束状态写进事务 `tx`，返回写了的；任务已不在存储里的不写（它被放弃或移除了）。
    fn apply_unsaved(&self, tx: &Transaction<'_>) -> Result<Vec<(TaskId, Ending)>, Error> {
        let unsaved: Vec<(TaskId, Ending)> = lock(&self.unsaved)
            .iter()
            .map(|(id, ending)| (*id, ending.clone()))
            .collect();
        for (id, ending) in &unsaved {
            match save_ending(tx, *id, ending) {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(unsaved)
    }

    /// 按 `draft` 定下任务的规格（取名、定任务目录，见 [`Handle::create`]），交给 `commit` 记进存储并开始；`commit`
    /// 报路径被这次取名时还没见过的任务占了时重新取名。
    async fn establish(
        self: &Arc<Self>,
        draft: Draft,
        commit: impl Fn(&TaskSpec, Option<PathBuf>, Option<Vec<u8>>) -> Result<Task, Error>,
    ) -> Result<Task, Error> {
        // 与换请求头时同样校验：网页视频的 Cookie 写进交给 yt-dlp 的 cookies.txt，不经核心
        for (name, value) in &draft.headers {
            check_option_header(name, value).map_err(Error::InvalidInput)?;
        }
        if !draft.folder.is_absolute() {
            return Err(Error::InvalidInput(format!(
                "保存目录不是绝对路径：{}",
                draft.folder.display()
            )));
        }
        // 规整成与核心算任务目录时相同的写法（去掉 `.` 与重复的分隔符），取名查的与插入时比较的才是同一个路径
        let folder = std::path::absolute(&draft.folder).map_err(|cause| Error::Io {
            action: "规整",
            path: draft.folder.clone(),
            cause,
        })?;
        let info = match &draft.kind {
            DraftKind::Torrent { resume, wanted } => {
                // 解析整个续传数据（大种子有几 MB），在阻塞线程上做
                let (headers, resume, wanted) =
                    (draft.headers.is_empty(), resume.clone(), wanted.clone());
                let info =
                    tokio::task::spawn_blocking(move || torrent_draft(headers, &resume, &wanted))
                        .await
                        .expect("读种子信息不会 panic，运行时关闭时本任务也一起丢弃")?;
                Some(info)
            }
            DraftKind::Hls { .. } | DraftKind::File { .. } => None,
            DraftKind::Web {
                video, audio, ext, ..
            } => {
                // 扩展名拼进输出与任务目录里各路的文件名
                let exts = [Some(ext.as_str())]
                    .into_iter()
                    .chain(
                        [video, audio]
                            .into_iter()
                            .flatten()
                            .map(|key| key.ext.as_deref()),
                    )
                    .flatten();
                for ext in exts {
                    if ext.is_empty()
                        || ext.len() > 16
                        || !ext.bytes().all(|b| b.is_ascii_alphanumeric())
                    {
                        return Err(Error::InvalidInput(format!(
                            "扩展名只能是字母与数字：{ext:?}"
                        )));
                    }
                }
                web_task::check_headers(&draft.headers)?;
                None
            }
            DraftKind::Cloud { provider } => {
                let spec =
                    self.cloud_spec(draft.url, folder, *provider, !draft.headers.is_empty())?;
                return commit(&spec, None, None);
            }
        };
        let base = match (&draft.kind, &info) {
            (DraftKind::Hls { title, .. }, _) => naming::task_title(&draft.url, title.as_deref()),
            (DraftKind::File { name, .. }, _) => naming::file_name(&draft.url, name.as_deref()),
            (DraftKind::Torrent { .. }, Some(info)) => info.name.clone(),
            (DraftKind::Torrent { .. }, None) => unreachable!("BT 的草稿上面已读出种子信息"),
            (DraftKind::Cloud { .. }, _) => unreachable!("云离线的草稿上面已交给 create_cloud"),
            (DraftKind::Web { title, ext, .. }, _) => {
                format!("{}.{ext}", naming::clean_title(title))
            }
        };
        let preference = match &draft.kind {
            DraftKind::Hls { preference, .. } => Some(StoredPreference::try_from(preference)?),
            DraftKind::File { .. }
            | DraftKind::Torrent { .. }
            | DraftKind::Cloud { .. }
            | DraftKind::Web { .. } => None,
        };
        // 取名与插入之间别的新建可能插入了占用同一路径的任务，插入时报 PathTaken，就重新取名：每重来一次存储里就多了
        // 一个这次取名时没见过的任务。冲突的路径取名时已见过，即两处算出的路径不一致，是不变量被破坏，照原样报出
        loop {
            let taken = {
                let mut store = lock(&self.store);
                let tx = store.transaction()?;
                store::taken_paths(&tx)?
            };
            let seen = taken.clone();
            let (folder, base) = (folder.clone(), base.clone());
            let (title, work_dir, resume, kind) = match &draft.kind {
                DraftKind::Hls { output, live, .. } => {
                    let output_kind = *output;
                    let (title, output) = named(&draft.folder, move || {
                        naming::output_for(&folder, &base, output_kind, &taken)
                    })
                    .await?;
                    let work_dir = output.options().resolved_work_dir()?;
                    let preference = preference.clone().expect("HLS 的草稿上面已转好选轨偏好");
                    let kind = Kind::Hls(HlsSpec {
                        preference,
                        output,
                        live: *live,
                    });
                    (title, Some(work_dir), None, kind)
                }
                DraftKind::File {
                    expected,
                    chunk_size,
                    ..
                } => {
                    let (name, path) = named(&draft.folder, move || {
                        naming::file_output_for(&folder, &base, &taken)
                    })
                    .await?;
                    let work_dir = naming::file_work_dir(&path);
                    let kind = Kind::File(FileSpec {
                        path,
                        expected: expected.clone(),
                        chunk_size: *chunk_size,
                    });
                    (name, Some(work_dir), None, kind)
                }
                DraftKind::Torrent { resume, wanted } => {
                    let info = info.as_ref().expect("BT 的草稿上面已读出种子信息");
                    let (single, info_hash) = (info.single_file(), info.info_hash.clone());
                    let fallback = info_hash.clone();
                    let (root_name, root) = named(&draft.folder, move || {
                        naming::torrent_root_for(&folder, &base, single, &fallback, &taken)
                    })
                    .await?;
                    let (resume, root_named, wanted) =
                        (resume.clone(), root_name.clone(), wanted.clone());
                    let placed = tokio::task::spawn_blocking(move || {
                        torrent::place(&resume, &root_named, &wanted)
                    })
                    .await
                    .expect("改根名不会 panic，运行时关闭时本任务也一起丢弃")?;
                    let kind = Kind::Torrent(TorrentSpec { info_hash, root });
                    (root_name, None, Some(placed), kind)
                }
                DraftKind::Cloud { .. } => unreachable!("云离线的草稿上面已交给 create_cloud"),
                DraftKind::Web {
                    extractor,
                    video_id,
                    video,
                    audio,
                    container,
                    ..
                } => {
                    let (name, path) = named(&draft.folder, move || {
                        naming::file_output_for(&folder, &base, &taken)
                    })
                    .await?;
                    let work_dir = naming::file_work_dir(&path);
                    let kind = Kind::Web(WebSpec {
                        extractor: extractor.clone(),
                        video_id: video_id.clone(),
                        video: video.clone(),
                        audio: audio.clone(),
                        container: *container,
                        path,
                        plugin: WebSpec::YT_DLP.to_owned(),
                    });
                    (name, Some(work_dir), None, kind)
                }
            };
            let spec = TaskSpec {
                title,
                url: draft.url.clone(),
                headers: draft.headers.clone(),
                kind,
            };
            match commit(&spec, work_dir, resume) {
                Err(Error::PathTaken(path))
                    if !seen.contains(&naming::path_key(&path.to_string_lossy())) => {}
                result => return result,
            }
        }
    }

    /// 云离线任务的规格：把磁力链接 `url` 交给网盘 `provider`（属于现在登录的账号）；提交、等网盘下完都在任务的运行里
    /// 做，失败的以任务失败报出。内容的根到列出文件时才在保存目录 `folder` 里定下。带了请求头（`has_headers`）、不是
    /// 磁力链接时报 [`Error::InvalidInput`]，没登录时报 [`Error::NotLoggedIn`]。
    fn cloud_spec(
        self: &Arc<Self>,
        url: Url,
        folder: PathBuf,
        provider: crate::CloudProvider,
        has_headers: bool,
    ) -> Result<TaskSpec, Error> {
        if has_headers {
            return Err(Error::InvalidInput("云离线任务没有请求头".to_owned()));
        }
        if url.scheme() != "magnet" {
            return Err(Error::InvalidInput("云离线只收磁力链接".to_owned()));
        }
        let crate::CloudProvider::PikPak = provider;
        let session = self.pikpak_session()?;
        let spec = CloudSpec {
            provider,
            user: session.user_id().to_owned(),
            job: None,
            before: None,
            folder,
            root: None,
        };
        Ok(cloud::draft_spec(spec, url))
    }

    /// 设置（[`Handle::settings`] 的服务内用法）。
    fn settings(&self) -> Result<Settings, Error> {
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        store::settings(&tx)
    }

    /// 把任务存进存储并开始，任务目录为 `work_dir`、BT 任务的续传数据为 `resume`（见 [`store::insert`]）；调用方须在
    /// 运行时里（[`Engine::start`] 的要求）。
    fn insert_and_start(
        self: &Arc<Self>,
        spec: &TaskSpec,
        work_dir: Option<PathBuf>,
        resume: Option<Vec<u8>>,
        created_at: i64,
    ) -> Result<Task, Error> {
        let mut store = lock(&self.store);
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        let id = store::insert(
            &tx,
            spec,
            work_dir.as_deref(),
            resume.as_deref(),
            created_at,
        )?;
        let task = store::get(&tx, id)?.expect("同一事务里刚插入的行读得到");
        // 开始了的任务随即会发进度：先发建了任务，没建成时再发删了
        self.events.send(Event::Added {
            task: Box::new(task.clone()),
        });
        let job = match self.launch(&tx, &task) {
            Ok(job) => job,
            Err(error) => {
                self.events.send(Event::Removed { id });
                return Err(error);
            }
        };
        if let Err(error) = self.commit(tx, applied) {
            self.events.send(Event::Removed { id });
            drop(store);
            self.abandon(job, work_dir);
            return Err(error);
        }
        self.track(&task, job, Run::Resume);
        Ok(task)
    }

    /// 照任务 `task` 开始下载或录制，经设置里的代理（BT 不经代理），设置与 BT 任务的续传数据从 `tx` 读；调用方须在
    /// 运行时里。
    fn launch(self: &Arc<Self>, tx: &Transaction<'_>, task: &Task) -> Result<Launched, Error> {
        Ok(match &task.spec.kind {
            Kind::Pending(_) => unreachable!("还没定下的任务不下载：继续时重新准备"),
            Kind::Hls(_) => {
                let proxy = store::settings(tx)?.proxy;
                Launched::Hls(self.engine.start(request(task, proxy)?)?)
            }
            Kind::File(file) => {
                let proxy = store::settings(tx)?.proxy;
                Launched::File(self.engine.start_file(file_request(task, file, proxy)?)?)
            }
            Kind::Torrent(spec) => Launched::Torrent(self.launch_torrent(tx, task.id, spec)?),
            Kind::Cloud(_) => {
                let proxy = store::settings(tx)?.proxy;
                Launched::Cloud(self.launch_cloud(tx, task, proxy)?)
            }
            Kind::Web(_) => {
                let settings = store::settings(tx)?;
                Launched::Web(self.launch_web(task, settings.proxy, settings.web_browser)?)
            }
        })
    }

    /// 开始 BT 任务 `id`：BT 会话还没有时照 `tx` 里存的端口与 DHT 状态建，续传数据从 `tx` 读；调用方须在运行时里。
    fn launch_torrent(
        self: &Arc<Self>,
        tx: &Transaction<'_>,
        id: TaskId,
        spec: &TorrentSpec,
    ) -> Result<TorrentRun, Error> {
        let engine = self.torrent_engine(store::torrent_session(tx)?)?;
        let resume = store::torrent_resume(tx, id)?;
        let data = resume.data.clone();
        let cancel = CancellationToken::new();
        let events = self.events.clone();
        let (info_hash, folder) = (spec.info_hash.clone(), torrent_folder(spec));
        let seeding = self.seeding.subscribe();
        let done = tokio::spawn({
            let (info_hash, cancel) = (info_hash.clone(), cancel.clone());
            async move {
                torrent::run(
                    engine,
                    &info_hash,
                    resume,
                    folder,
                    &seeding,
                    cancel,
                    |progress| {
                        events.send(Event::TorrentProgress {
                            id,
                            progress: Box::new(progress),
                        });
                    },
                )
                .await
            }
        });
        Ok(TorrentRun {
            cancel,
            done,
            info_hash,
            resume: data,
            root: spec.root.clone(),
        })
    }

    /// 让停着的任务 `id` 再运行一次：`run` 为 [`Run::Resume`] 时照新建时的请求续传续录，为 [`Run::WriteRecorded`] 时
    /// 不联网，只把任务目录里已录的直播写成输出。返回开始后的任务。
    fn resume(self: &Arc<Self>, id: TaskId, run: Run) -> Result<Task, Error> {
        let mut store = lock(&self.store);
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        let mut task = self.stopped(&tx, id)?;
        if task.spec.pending().is_some() {
            return match run {
                Run::Resume => self.begin_flow(tx, applied, task, prepare::Entry::Fresh, false),
                Run::WriteRecorded => Err(Error::NotHls(id)),
            };
        }
        let job = match run {
            Run::Resume => self.launch(&tx, &task)?,
            Run::WriteRecorded => {
                Launched::Hls(self.engine.merge_recorded(output_options(&task)?)?)
            }
        };
        task.status = Status::Running;
        task.write_only = run == Run::WriteRecorded;
        store::set_status(&tx, id, &task.status)?;
        store::set_write_only(&tx, id, task.write_only)?;
        let committed = self.commit(tx, applied);
        if committed.is_err() {
            // 任务目录是这个任务原有的：照常登记，停下后按暂停写存储（同样写不进时记为待补写）
            job.cancel();
        }
        // 先发开始了，再登记：这次运行的进度与结束都在这之后
        if committed.is_ok() {
            self.events.send(Event::Changed {
                task: Box::new(task.clone()),
            });
        }
        self.track(&task, job, run);
        drop(store);
        committed.map(|()| task)
    }

    /// 新建时存储写不进而不要的任务：取消，等它停下，再删掉它建出的任务目录（`work_dir`；BT 任务没有，删下了的文件）；
    /// 磁盘上还留下东西时发 [`Event::Orphaned`]。
    fn abandon(self: &Arc<Self>, job: Launched, work_dir: Option<PathBuf>) {
        job.cancel();
        let shared = self.clone();
        self.tracker.spawn(async move {
            let written = match job {
                Launched::Hls(job) => job.wait().await.map(|output| written(&output)),
                Launched::File(job) => job.wait().await.map(|output| vec![output.path]),
                Launched::Torrent(run) => {
                    // 已取消：结果只能是暂停或出错，任务都已不要
                    let _ended = run.done.await;
                    let deleted = match shared.bt_engine() {
                        Ok(engine) => {
                            torrent::delete(engine, &run.info_hash, &run.resume, &run.root).await
                        }
                        Err(error) => Err(error),
                    };
                    if let Err(error) = deleted {
                        shared.events.send(Event::Orphaned {
                            paths: vec![run.root],
                            reason: error.to_string(),
                        });
                    }
                    return;
                }
                // 新建时还没定下内容的根，磁盘上没有它的东西
                Launched::Cloud(run) => {
                    let _ended = run.done.await;
                    return;
                }
                Launched::Web(run) => {
                    let _ended = run.done.await;
                    let work_dir = work_dir.expect("网页视频任务有任务目录");
                    if let Err(error) = web_task::remove_work_dir(&work_dir).await {
                        shared.events.send(Event::Orphaned {
                            paths: vec![work_dir],
                            reason: error.to_string(),
                        });
                    }
                    return;
                }
            };
            let work_dir = work_dir.expect("HLS 与文件任务有任务目录");
            let (paths, reason) = match written {
                Ok(paths) => (paths, "新建的任务没能记进存储，取消前已写出输出".to_owned()),
                // 已取消，错误只能是取消或取消前就出的错，任务都已不要
                Err(_) => match shared
                    .engine
                    .discard(&work_dir, Unsettled::Fail, &CancellationToken::new())
                    .await
                {
                    Ok(leftovers) if leftovers.is_empty() => return,
                    Ok(leftovers) => (
                        vec![work_dir],
                        leftovers
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("；"),
                    ),
                    Err(error) => (vec![work_dir], error.to_string()),
                },
            };
            shared.events.send(Event::Orphaned { paths, reason });
        });
    }

    /// 登记运行中的任务 `task`，转发它的进度，等它停下后把状态写进存储并发出 [`Event::Ended`]。调用方须在运行时里。
    fn track(self: &Arc<Self>, task: &Task, job: Launched, run: Run) {
        match job {
            Launched::Hls(job) => self.track_hls(task, job, run),
            Launched::File(job) => self.track_file(task, job),
            Launched::Torrent(job) => self.track_torrent(task, job),
            Launched::Cloud(run) => self.track_cloud(task, run),
            Launched::Web(run) => self.track_web(task, run),
        }
    }

    /// 登记运行中的任务 `id`：暂停与放弃经它找到任务，返回的登记释放时去掉。
    fn register(self: &Arc<Self>, id: TaskId, control: Control) -> RunningGuard {
        self.register_with(id, control, None)
    }

    /// 同 [`Shared::register`]；`step` 为准备的一轮开始时的步骤（下载、录制的为 None）。这个任务已有登记时换掉它：
    /// 旧登记释放时不动新的。
    fn register_with(
        self: &Arc<Self>,
        id: TaskId,
        control: Control,
        step: Option<prepare::Step>,
    ) -> RunningGuard {
        let serial = self.serials.fetch_add(1, Ordering::Relaxed);
        let (stopped_tx, stopped) = watch::channel(false);
        let replaced = lock(&self.running).insert(
            id,
            Running {
                control,
                stopped,
                serial,
                step,
                seat: None,
            },
        );
        // 换掉的登记里的网页名额在 `running` 锁外让出（各把锁之间不嵌套）
        drop(replaced);
        RunningGuard {
            id,
            serial,
            shared: self.clone(),
            stopped: stopped_tx,
            started: Instant::now(),
        }
    }

    /// 任务 `id` 停下了：在 `store` 锁内把 `ending`（连同这次运行了多久）写进存储（写不进时记为待补写）、发出
    /// [`Event::Changed`] 与 [`Event::Ended`]，再去掉运行登记 `guard`：等它停下的操作（放弃、之后的继续）的事件都在
    /// 这之后。
    fn conclude(
        &self,
        id: TaskId,
        mut ending: Ending,
        leftovers: Vec<Leftover>,
        guard: RunningGuard,
    ) {
        guard.stamp(&mut ending);
        let mut store = lock(&self.store);
        let saved = self.save_in(&mut store, id, &ending);
        self.announce_in(&mut store, id, ending, None, leftovers, saved);
        drop(guard);
    }

    /// 把任务 `id` 的结束状态 `ending` 写进存储，返回写了之后的任务；写不进时记为待补写（并上之前没写进的），报出
    /// 原因。调用方持着 `store` 锁。
    fn save_in(&self, store: &mut Store, id: TaskId, ending: &Ending) -> Result<Task, Error> {
        let saved = self.write_in(store, |tx| {
            save_ending(tx, id, ending)?;
            store::get(tx, id)?.ok_or(Error::NotFound(id))
        });
        if let Err(error) = &saved {
            // 违反约束的结束状态永远写不进：留着待补写会让之后的每次写入都失败
            assert!(
                !error.violates_constraint(),
                "{id}的结束状态与存储的表结构不一致：{error}"
            );
            // 之前的待补写这次也没写进：并进这一次，不丢它的运行时长
            let mut unsaved = lock(&self.unsaved);
            let pending = match unsaved.get(&id) {
                Some(earlier) => ending.clone().after(earlier),
                None => ending.clone(),
            };
            unsaved.insert(id, pending);
        }
        saved
    }

    /// 任务 `id` 的结束状态 `ending` 写进存储之后（`saved` 为结果）：发出 [`Event::Changed`]、[`Event::Stage`]（停下
    /// 等用户、`offer` 为可选项时）与 [`Event::Ended`]。调用方持着 `store` 锁、还没释放运行登记：别的操作的事件排不到
    /// 这几个之间或之前。
    fn announce_in(
        &self,
        store: &mut Store,
        id: TaskId,
        ending: Ending,
        offer: Option<Offer>,
        leftovers: Vec<Leftover>,
        saved: Result<Task, Error>,
    ) {
        let (task, unsaved) = match saved {
            Ok(task) => (Some(task), None),
            // 没写进的已记为待补写，照它读；这时读也读不出的不发 Changed，原因随 Ended 报出
            Err(error) => (self.task_in(store, id).ok(), Some(error.to_string())),
        };
        if let Some(task) = task {
            self.events.send(Event::Changed {
                task: Box::new(task),
            });
        }
        if let Some(offer) = offer {
            self.events.send(Event::Stage {
                id,
                stage: Some(Box::new(Stage::Asking(offer))),
            });
        }
        self.events.send(Event::Ended {
            id,
            status: ending.status,
            result: ending.result,
            total: ending.total,
            leftovers,
            unsaved,
        });
    }

    /// HLS 任务的 [`Shared::track`]：下载开始后的点播总片数与存储里记着的不同时记下。
    fn track_hls(self: &Arc<Self>, task: &Task, job: Job, run: Run) {
        let (id, total) = (task.id, task.total);
        let live = task.spec.hls().is_some_and(|hls| hls.live);
        let control = job.control();
        let guard = self.register(id, Control::Hls(control.clone()));
        let shared = self.clone();
        self.tracker.spawn(async move {
            let done = CancellationToken::new();
            let forwarding = tokio::spawn(forward_hls(
                shared.clone(),
                id,
                control.clone(),
                total,
                done.clone(),
            ));
            let result = job.wait().await;
            done.cancel();
            let forwarded = forwarding
                .await
                .expect("转发进度的任务不会 panic，运行时关闭时本任务也一起丢弃");
            // 转发按节流取样，下得快时可能没见过下载阶段；按最后的进度再定一次点播的总片数。停在下载或写出阶段的，
            // 请求不开录制的必是点播，开了录制的要没见过录制阶段才是
            let last = control.progress().borrow().clone();
            let vod = match &result {
                Ok(output) => output.live.is_none(),
                // 只写出已录的必是直播
                Err(_) => {
                    run == Run::Resume
                        && matches!(last.stage, JobStage::Downloading | JobStage::Writing)
                        && !(live && (forwarded.recorded || last.session.is_some()))
                }
            };
            let total = if vod && last.segments.total > 0 {
                Some(last.segments.total)
            } else {
                forwarded.total
            };
            let (status, result, leftovers) = match result {
                Ok(output) => {
                    let result = finished_result(&output, last.duration);
                    (Status::Done, Some(result), output.leftovers)
                }
                Err(hs_m3u8_core::Error::Cancelled) => (Status::Paused, None, Vec::new()),
                Err(error) => (Status::Failed(Failure::of(&error)), None, Vec::new()),
            };
            let ending = Ending {
                result,
                total,
                ..Ending::of(status)
            };
            shared.conclude(id, ending, leftovers, guard);
        });
    }

    /// 文件任务的 [`Shared::track`]。
    fn track_file(self: &Arc<Self>, task: &Task, job: Job<FileOutput, FileProgress>) {
        let id = task.id;
        let control = job.control();
        let guard = self.register(id, Control::File(control.clone()));
        let shared = self.clone();
        self.tracker.spawn(async move {
            let done = CancellationToken::new();
            let events = shared.events.clone();
            let forwarding = tokio::spawn(throttled(
                control.progress(),
                done.clone(),
                move |progress| {
                    events.send(Event::FileProgress {
                        id,
                        progress: Box::new(progress),
                    });
                },
            ));
            let result = job.wait().await;
            done.cancel();
            forwarding
                .await
                .expect("转发进度的任务不会 panic，运行时关闭时本任务也一起丢弃");
            let (status, result, leftovers) = match result {
                Ok(output) => {
                    let result = Finished {
                        duration: None,
                        bytes: output.bytes,
                        resolution: None,
                    };
                    (Status::Done, Some(result), output.leftovers)
                }
                Err(hs_m3u8_core::Error::Cancelled) => (Status::Paused, None, Vec::new()),
                Err(error) => (Status::Failed(Failure::of(&error)), None, Vec::new()),
            };
            let ending = Ending {
                result,
                ..Ending::of(status)
            };
            shared.conclude(id, ending, leftovers, guard);
        });
    }

    /// BT 任务的 [`Shared::track`]：停下时要到的续传数据随结束状态写进存储。
    fn track_torrent(self: &Arc<Self>, task: &Task, run: TorrentRun) {
        let id = task.id;
        let guard = self.register(id, Control::Torrent(run.cancel.clone()));
        let shared = self.clone();
        self.tracker.spawn(async move {
            let ended = run
                .done
                .await
                .expect("BT 任务的运行不会 panic，运行时关闭时本任务也一起丢弃");
            let (status, result) = match ended.outcome {
                Outcome::Finished(bytes) => (
                    Status::Done,
                    Some(Finished {
                        duration: None,
                        bytes,
                        resolution: None,
                    }),
                ),
                Outcome::Paused => (Status::Paused, None),
                Outcome::Failed(failure) => (Status::Failed(failure), None),
            };
            let finished = status == Status::Done;
            let ending = Ending {
                result,
                resume: ended.resume,
                seeding: finished.then_some(ended.seeder.is_some()),
                ..Ending::of(status)
            };
            shared.conclude(id, ending, Vec::new(), guard);
            if let Some(seeder) = ended.seeder {
                shared.track_seeding(id, async move { Ok(seeder) });
            }
        });
    }

    /// 云离线任务的 [`Shared::track`]。
    fn track_cloud(self: &Arc<Self>, task: &Task, run: cloud::CloudRun) {
        let id = task.id;
        let guard = self.register(id, Control::Cloud(run.cancel.clone()));
        let shared = self.clone();
        self.tracker.spawn(async move {
            let (outcome, leftovers) = run
                .done
                .await
                .expect("云离线任务的运行不会 panic，运行时关闭时本任务也一起丢弃");
            let (status, result) = cloud::ending(outcome);
            let ending = Ending {
                result,
                ..Ending::of(status)
            };
            shared.conclude(id, ending, leftovers, guard);
        });
    }

    /// 网页视频任务的 [`Shared::track`]。
    /// 网页视频任务的 [`Shared::track`]：完成且写进了存储之后才删任务目录（里面有换上输出的标记，见
    /// `web_task::placed`）：写进之前中断的，下次开始时据它认出输出是自己的。
    fn track_web(self: &Arc<Self>, task: &Task, run: web_task::WebRun) {
        let id = task.id;
        let work_dir = task.work_dir.clone();
        let guard = self.register(id, Control::Web(run.cancel.clone()));
        let shared = self.clone();
        self.tracker.spawn(async move {
            let (outcome, leftovers) = run
                .done
                .await
                .expect("网页视频任务的运行不会 panic，运行时关闭时本任务也一起丢弃");
            let (status, result) = web_task::ending(outcome);
            let mut ending = Ending {
                result,
                ..Ending::of(status)
            };
            guard.stamp(&mut ending);
            let saved = shared.save_in(&mut lock(&shared.store), id, &ending);
            // 登记还在：删目录的这一段里任务不能继续、放弃要等、移除不收
            if saved.is_ok()
                && ending.status == Status::Done
                && let Some(work_dir) = work_dir
                && let Err(error) = web_task::remove_work_dir(&work_dir).await
            {
                // 输出已写好，留下的目录不影响结果
                shared.events.send(Event::Orphaned {
                    paths: vec![work_dir],
                    reason: error.to_string(),
                });
            }
            let mut store = lock(&shared.store);
            shared.announce_in(&mut store, id, ending, None, leftovers, saved);
            drop(guard);
        });
    }
}

/// 种子 `info_hash`（[`hs_dlx_torrent::TorrentId`] 的写法）不在任何任务里；在时报 [`Error::TorrentTaken`]。
fn torrent_free(shared: &Shared, info_hash: &str) -> Result<(), Error> {
    match shared.write(|tx| store::task_with_torrent(tx, info_hash))? {
        Some(existing) => Err(Error::TorrentTaken(existing)),
        None => Ok(()),
    }
}

/// 新建 BT 任务的草稿（种类为 [`DraftKind::Torrent`]）里续传数据 `resume` 的种子信息；草稿要没有请求头（`no_headers`）、
/// `wanted` 至少选了一个文件且都是种子里的，不然报 [`Error::InvalidInput`]。
fn torrent_draft(no_headers: bool, resume: &[u8], wanted: &[i32]) -> Result<TorrentInfo, Error> {
    if !no_headers {
        return Err(Error::InvalidInput("BT 任务没有请求头".to_owned()));
    }
    let info = TorrentInfo::of(resume)?;
    if wanted.is_empty() {
        return Err(Error::InvalidInput("至少要选一个文件".to_owned()));
    }
    if let Some(index) = wanted
        .iter()
        .find(|index| !info.files.iter().any(|file| file.index == **index))
    {
        return Err(Error::InvalidInput(format!(
            "种子里没有序号为 {index} 的文件"
        )));
    }
    Ok(info)
}

/// BT 任务的保存目录：内容的根所在的目录。
fn torrent_folder(spec: &TorrentSpec) -> PathBuf {
    spec.root
        .parent()
        .expect("内容的根是以文件名结尾的绝对路径")
        .to_path_buf()
}

/// 转发进度时看到的。
struct Forwarded {
    /// 最后知道的点播总片数
    total: Option<usize>,
    /// 见过录制阶段
    recorded: bool,
}

/// 转发 HLS 任务 `id` 的进度，同时记下点播的总片数，直到 `done` 取消或进度不再更新；`total` 为开始时知道的总片数。
async fn forward_hls(
    shared: Arc<Shared>,
    id: TaskId,
    control: JobControl,
    total: Option<usize>,
    done: CancellationToken,
) -> Forwarded {
    let mut seen = Forwarded {
        total,
        recorded: false,
    };
    throttled(control.progress(), done, |snapshot: Progress| {
        seen.recorded |= snapshot.stage == JobStage::Recording;
        // 点播的总片数在下载开始时就定了；直播与准备阶段不记
        let count = snapshot.segments.total;
        if snapshot.stage == JobStage::Downloading && count > 0 && seen.total != Some(count) {
            seen.total = Some(count);
            if let Err(error) = shared.write(|tx| store::set_total(tx, id, count)) {
                // 停下时随结束状态再写一次
                shared.events.send(Event::TotalUnsaved {
                    id,
                    error: error.to_string(),
                });
            }
        }
        shared.events.send(Event::Progress {
            id,
            progress: Box::new(snapshot),
        });
    })
    .await;
    seen
}

/// 进度每变一次把快照交给 `each`，两次之间至少隔 [`PROGRESS_INTERVAL`]，直到 `done` 取消或进度不再更新。
async fn throttled<P: Clone>(
    mut progress: watch::Receiver<P>,
    done: CancellationToken,
    mut each: impl FnMut(P),
) {
    loop {
        match done.run_until_cancelled(progress.changed()).await {
            Some(Ok(())) => {}
            None | Some(Err(_)) => return,
        }
        each(progress.borrow_and_update().clone());
        if done
            .run_until_cancelled(tokio::time::sleep(PROGRESS_INTERVAL))
            .await
            .is_none()
        {
            return;
        }
    }
}

/// 在运行时的阻塞线程上取名（`choose` 要查保存目录 `folder` 里的名字，网络盘上可能慢），[`NAMING_TIMEOUT`] 内取不完
/// 报 [`Error::TimedOut`]。
async fn named<T: Send + 'static>(
    folder: &Path,
    choose: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::time::timeout(NAMING_TIMEOUT, tokio::task::spawn_blocking(choose))
        .await
        .map_err(|_| Error::TimedOut {
            action: format!("检查保存目录 {}", folder.display()),
            after: NAMING_TIMEOUT,
        })?
        .expect("取名不会 panic，运行时关闭时本任务也一起丢弃")
}

/// 输出写出的路径：MP4 与本地 HLS 目录。
fn written(output: &Output) -> Vec<PathBuf> {
    output
        .mp4
        .iter()
        .map(|mp4| mp4.path.clone())
        .chain(output.hls.clone())
        .collect()
}

/// 完成时要记下的结果：MP4 里视频流的时长与宽高，没有时时长取 `declared`（第 0 条轨分片的声明时长之和，为 0 时不记）。
fn finished_result(output: &Output, declared: Duration) -> Finished {
    let video = output.mp4.as_ref().and_then(|mp4| {
        mp4.report
            .streams
            .iter()
            .find_map(|stream| match stream.shape {
                Shape::Video { width, height, .. } => Some((stream.duration, (width, height))),
                Shape::Audio { .. } => None,
            })
    });
    Finished {
        duration: video
            .map(|(duration, _)| duration)
            .or((declared > Duration::ZERO).then_some(declared)),
        bytes: output.bytes,
        resolution: video
            .map(|(_, resolution)| resolution)
            .filter(|&(width, height)| width > 0 && height > 0),
    }
}

fn save_ending(tx: &Transaction<'_>, id: TaskId, ending: &Ending) -> Result<(), Error> {
    store::set_status(tx, id, &ending.status)?;
    store::add_run(tx, id, ending.ran, ending.finished_at)?;
    if let Some(result) = &ending.result {
        store::set_result(tx, id, result)?;
    }
    if let Some(total) = ending.total {
        store::set_total(tx, id, total)?;
    }
    if let Some(resume) = &ending.resume {
        store::set_torrent_resume(tx, id, resume)?;
    }
    if let Some(seeding) = ending.seeding {
        store::set_seeding_task(tx, id, seeding)?;
    }
    Ok(())
}

/// 照 HLS 任务建请求，经代理 `proxy`（设置里的）：任务目录照 [`Task::work_dir`] 指定，没有时报 [`Error::NoWorkDir`]；
/// 不是直播时不开录制，来源是直播就报 [`hs_m3u8_core::Error::LiveNotEnabled`]。不是 HLS 任务时报 [`Error::NotHls`]。
fn request(task: &Task, proxy: Proxy) -> Result<JobRequest, Error> {
    let hls = task.spec.hls().ok_or(Error::NotHls(task.id))?;
    let mut source = Source::new(task.spec.url.clone());
    source.preference = Preference::from(&hls.preference);
    source.http.headers.clone_from(&task.spec.headers);
    source.http.proxy = proxy;
    let mut request = JobRequest::new(source, output_options(task)?);
    if !hls.live {
        request.live = None;
    }
    Ok(request)
}

/// 照文件任务建请求，经代理 `proxy`（设置里的）：任务目录照 [`Task::work_dir`] 指定，没有时报 [`Error::NoWorkDir`]；
/// 第一次下载按新建前认出的核对（见 [`hs_m3u8_core::Expected`]）。
fn file_request(task: &Task, file: &FileSpec, proxy: Proxy) -> Result<FileRequest, Error> {
    let mut source = Source::new(task.spec.url.clone());
    source.http.headers.clone_from(&task.spec.headers);
    source.http.proxy = proxy;
    let mut output = FileOutputOptions::new(file.path.clone());
    output.work_dir = Some(task.work_dir.clone().ok_or(Error::NoWorkDir(task.id))?);
    let mut request = FileRequest::new(source, output);
    request.chunk_size = file.chunk_size;
    request.expected = Some((&file.expected).into());
    Ok(request)
}

/// HLS 任务的输出选项：任务目录照 [`Task::work_dir`] 指定，没有时报 [`Error::NoWorkDir`]；不是 HLS 任务时报
/// [`Error::NotHls`]。
fn output_options(task: &Task) -> Result<OutputOptions, Error> {
    let hls = task.spec.hls().ok_or(Error::NotHls(task.id))?;
    let mut options = hls.output.options();
    options.work_dir = Some(task.work_dir.clone().ok_or(Error::NoWorkDir(task.id))?);
    Ok(options)
}

/// 调用接口失败时服务器可能已处理了请求（见 [`Engine::call`]）：可能已发出，或已收到响应头而没读完响应体。
fn may_have_taken_effect(error: &hs_m3u8_core::Error) -> bool {
    matches!(
        error,
        hs_m3u8_core::Error::Call {
            sent: hs_m3u8_core::Sent::Maybe | hs_m3u8_core::Sent::Answered { .. },
            ..
        }
    )
}

/// 停着的任务再运行一次的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// 照新建时的请求续传、续录
    Resume,
    /// 不联网，只把已录的直播写成输出
    WriteRecorded,
}

/// 加锁；持锁期间只做内存与 SQLite 的读写，不会 panic，锁中毒即不变量被破坏。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("持锁期间不会 panic")
}
