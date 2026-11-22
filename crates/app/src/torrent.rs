//! BT 任务的引擎：整个进程一个 libtorrent 会话，第一次用到时才建（不用 BT 时不开监听端口、不连 DHT）。会话里只有
//! 运行中的 BT 任务与正在取种子信息的磁力链接，每个种子归一个异步任务管：一条线程取会话的事件，按种子的编号转给
//! 管它的那个（[`Owner`]）。
//!
//! 种子的一切（种子信息、tracker、根名、选了的文件、已有的块）都在它的续传数据里，存进任务存储；停下时先把缓存写到
//! 磁盘、再要续传数据（libtorrent 只记写完的块），存下的已有块不多于磁盘上的（标为可信）。公共 tracker（见
//! [`crate::public_trackers`]）不在其内：每次加入会话后，列表取到了就补给不是私有的种子。

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle as ThreadHandle;
use std::time::Duration;

use hs_dlx_torrent::{
    AddOptions, Alert, AlertKind, Removal, Session, SessionConfig, State, TorrentId,
};
use hs_m3u8_core::Url;
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::diagnosis::{Check, Facts, ProxyFact, Public, Tracker, checks};
use crate::netcheck::Resolved;
use crate::public_trackers::{PublicTrackers, SOURCES};
use crate::socks::{ProxyCheck, libtorrent_proxy};
use crate::{Error, Failure, FailureKind, Seeding};

/// 取事件时最多等多久（毫秒）：关闭时取事件的线程最晚这么久后发现要退出。
const PUMP_WAIT_MS: u32 = 250;
/// 运行中多久查一次种子的状态、发一次进度。
const STATUS_INTERVAL: Duration = Duration::from_millis(500);
/// 等 libtorrent 一步的结果（续传数据、写完缓存、移除完、删完文件）最多等多久：都只是本机的操作。
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// 做种时多久存一次进度（累计上传与做种时长随续传数据存下）：应用异常退出时最多丢这么久的统计。
const SAVE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// 连着节点而凑不齐全部内容要连续这么多次状态（每次 [`STATUS_INTERVAL`]，共 3 秒）才算资源不足：刚连上的节点在报来
/// 它有哪些块之前也计入节点数，开始时会短暂凑不齐。
const SCARCE_TICKS: u32 = 6;
/// 操作系统在用户打开过的目录里自己建的文件（访达的显示设置、Windows 资源管理器的缩略图与目录设置）：放弃 BT 任务时
/// 随下了的文件一起删，不算用户放进去的东西。
const OS_LITTER: [&str; 3] = [".DS_Store", "Thumbs.db", "desktop.ini"];

/// BT 会话连哪里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentNetwork {
    /// 监听本机的全部地址（端口沿用上次的，第一次由系统挑），用 DHT、UPnP、NAT-PMP 与本地发现，从网上取公共 tracker
    /// 列表
    Internet,
    /// 只监听回环地址、不找外面的节点：用于测试。公共 tracker 列表依次从 `public_trackers` 取，空时不补
    Loopback { public_trackers: Vec<Url> },
}

impl TorrentNetwork {
    /// 公共 tracker 列表的来源；空时不补。
    fn public_trackers(&self) -> Vec<Url> {
        match self {
            TorrentNetwork::Internet => SOURCES
                .iter()
                .map(|source| Url::parse(source).expect("列表的来源是写死的有效地址"))
                .collect(),
            TorrentNetwork::Loopback { public_trackers } => public_trackers.clone(),
        }
    }
}

/// 种子里的一个文件（不含对齐用的填充文件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    /// 在种子里的序号，选文件时用它
    pub index: i32,
    /// 相对保存目录的路径（多文件的以根名的目录开头），本平台的分隔符；不是 UTF-8 的字节换成了替换字符，只用于显示
    pub path: String,
    /// 字节数
    pub size: u64,
    /// 要下载
    pub wanted: bool,
}

/// 续传数据里的种子信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentInfo {
    /// 种子的编号：信息哈希的十六进制（有 v1 的为它，只有 v2 时为截到 20 字节的 v2 的）
    pub info_hash: String,
    /// 种子名：单文件的为文件名，多文件的为顶层目录名；不是 UTF-8 的字节换成了替换字符
    pub name: String,
    /// 私有种子：只经种子里的 tracker 找节点
    pub private: bool,
    pub files: Vec<TorrentFile>,
    /// 续传数据里记的已有的块落在要下的文件里的字节数
    pub done: u64,
    /// 保存目录里存没选的文件与选了的共用的块里没选的部分的文件名（只在有这样的块时才有这个文件）
    pub part_file: String,
}

impl TorrentInfo {
    /// 读出续传数据里的种子信息；读不出（不是续传数据、没有种子信息）时报 [`Error::InvalidInput`]。
    pub fn of(resume: &[u8]) -> Result<TorrentInfo, Error> {
        let metadata = hs_dlx_torrent::describe(resume)
            .map_err(|error| Error::InvalidInput(format!("读不出种子信息：{error}")))?;
        Ok(TorrentInfo {
            info_hash: metadata.id.as_str().to_owned(),
            name: metadata.name,
            private: metadata.private,
            files: metadata
                .files
                .into_iter()
                .map(|file| TorrentFile {
                    index: file.index,
                    path: file.path,
                    size: u64::try_from(file.size).expect("libtorrent 的文件大小不为负"),
                    wanted: file.wanted,
                })
                .collect(),
            done: metadata.done,
            part_file: metadata.part_file,
        })
    }

    /// 要下的字节数。
    pub fn wanted(&self) -> u64 {
        self.files
            .iter()
            .filter(|file| file.wanted)
            .map(|file| file.size)
            .sum()
    }

    /// 单文件的种子（内容的根是文件，不是目录）。
    pub fn single_file(&self) -> bool {
        matches!(self.files.as_slice(), [only] if only.path == self.name)
    }
}

/// BT 任务存着的续传数据里的种子信息（[`crate::Service::inspect_torrent`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedTorrent {
    pub info: TorrentInfo,
    /// 已有块的记录可信；否时（上次没正常停下）`info.done` 不作数，下次继续时整体校验磁盘上的文件
    pub trusted: bool,
}

/// 新建任务前定下内容放哪、下哪些（见 [`hs_dlx_torrent::place`]）；定不下（根名不是一个路径段、序号不是种子里的文件、
/// 读不出种子信息）时报 [`Error::InvalidInput`]。
pub(crate) fn place(resume: &[u8], root: &str, wanted: &[i32]) -> Result<Vec<u8>, Error> {
    hs_dlx_torrent::place(resume, root, wanted)
        .map_err(|error| Error::InvalidInput(format!("定不下种子的根名与要下的文件：{error}")))
}

/// 种子文件（bencode 写的内容）转成续传数据，供新建任务（[`crate::DraftKind::Torrent`]）；不是种子文件时报
/// [`Error::InvalidInput`]。
pub fn torrent_from_file(torrent: &[u8]) -> Result<Vec<u8>, Error> {
    hs_dlx_torrent::resume_from_torrent_file(torrent)
        .map_err(|error| Error::InvalidInput(format!("不是种子文件：{error}")))
}

/// 磁力链接取种子信息的进度。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataProgress {
    /// 连着的节点数
    pub peers: u32,
    /// 最近一次 tracker 报的错（libtorrent 的说明，英文，含 tracker 的地址）；没报过时为 None
    pub tracker_error: Option<String>,
    /// 磁力链接没带 tracker、要补公共 tracker，而列表这次没取到时为原因（隔一阵再取）；别的时候为 None
    pub public_trackers_error: Option<String>,
    /// BT 代理的问题（探出用不了，或经它的 UDP 最近出错）；直连或没问题时为 None
    pub proxy_problem: Option<String>,
}

/// 运行中的 BT 任务在做什么。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TorrentPhase {
    /// 校验磁盘上已有的文件；`progress` 为校验了的比例，0 到 1
    Checking {
        progress: f32,
    },
    Downloading,
    /// 选了的文件都下完了，正在存续传数据、转去做种或从会话里移除
    Finishing,
    /// 下完了，在做种
    Seeding,
}

/// 运行中的 BT 任务的进度快照。
#[derive(Debug, Clone, PartialEq)]
pub struct TorrentProgress {
    pub phase: TorrentPhase,
    /// 要下的字节数与其中已下完并校验过的
    pub wanted: u64,
    pub done: u64,
    /// 这次运行收到的内容字节数（含校验没过、丢掉的）：算速度用
    pub received: u64,
    /// 累计上传的内容字节数与下完之后在会话里的秒数（做种时长），含之前各次运行的
    pub uploaded: u64,
    pub seeding_seconds: u64,
    /// 下载与上传的速度，字节每秒，只算内容
    pub download_rate: u64,
    pub upload_rate: u64,
    /// 连着的节点数，其中的做种者
    pub peers: u32,
    pub seeds: u32,
    /// 要下的块数，其中能凑齐的（自己已有，或在线的节点里至少有一个有）：少于 `pieces` 时眼下的节点凑不齐全部内容
    pub pieces: u32,
    pub reachable: u32,
    /// 资源不足：下载中连着节点，凑不齐全部内容已持续 3 秒以上
    pub scarce: bool,
    /// 这次运行里最近一次 tracker 报的错（libtorrent 的说明，英文，含 tracker 的地址）；没报过时为 None。私有种子只经
    /// tracker 找节点，它报错时多半一直连不上节点
    pub tracker_error: Option<String>,
    /// BT 代理的问题（同 [`MetadataProgress::proxy_problem`]）
    pub proxy_problem: Option<String>,
}

impl TorrentProgress {
    fn of(status: &hs_dlx_torrent::Status) -> TorrentProgress {
        let count = |value: i32| u32::try_from(value).expect("libtorrent 的计数不为负");
        let bytes = |value: i64| u64::try_from(value).expect("libtorrent 的字节数不为负");
        let phase = match status.state() {
            Some(State::CheckingFiles | State::CheckingResumeData) => TorrentPhase::Checking {
                progress: status.progress_ppm as f32 / 1e6,
            },
            Some(State::Finished | State::Seeding) => TorrentPhase::Finishing,
            Some(State::Downloading | State::DownloadingMetadata) | None => {
                TorrentPhase::Downloading
            }
        };
        TorrentProgress {
            phase,
            wanted: bytes(status.total_wanted),
            done: bytes(status.total_wanted_done),
            received: bytes(status.downloaded),
            uploaded: bytes(status.all_time_upload),
            seeding_seconds: bytes(status.finished_seconds),
            download_rate: bytes(status.download_rate.into()),
            upload_rate: bytes(status.upload_rate.into()),
            peers: count(status.peers),
            seeds: count(status.seeds),
            pieces: count(status.pieces),
            reachable: count(status.reachable_pieces),
            scarce: false,
            tracker_error: None,
            proxy_problem: None,
        }
    }
}

/// 存进任务存储的续传数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resume {
    pub data: Vec<u8>,
    /// 记的已有块不多于磁盘上的：否时下次加入不信这些记录、整体校验磁盘上的文件
    pub trusted: bool,
}

/// 转给管种子的异步任务的。
enum Routed {
    Alert(Alert),
    /// 事件队列满了丢了一些，可能有这个种子的：在等的那一步可能再也等不到
    Lost,
}

/// 有会话之后的引擎。
pub(crate) struct Engine {
    session: Session,
    /// 种子的编号 → 管它的异步任务
    routes: Mutex<HashMap<TorrentId, mpsc::UnboundedSender<Routed>>>,
    /// 每有一个种子移除了（或删完文件）就通知：等 [`Session::removing`] 变为否的在等它
    removed: Notify,
    /// 每到一次会话的统计（[`Session::request_stats`]）就通知
    stats: Notify,
    /// 关闭了：取事件的线程退出
    stop: AtomicBool,
    /// 公共 tracker 列表的状况（[`Bt::set_public`] 改）
    public: watch::Receiver<PublicTrackers>,
    /// BT 代理探到的情况（[`Bt::set_proxy_check`] 改）
    proxy_check: watch::Receiver<ProxyCheck>,
}

impl Engine {
    /// 要一次会话的统计（DHT 的节点数、有没有节点连进来），到了之后 [`Engine::session_facts`] 里是新的。
    pub(crate) fn request_stats(&self) {
        self.session.request_stats();
    }

    /// 监听、端口映射（UPnP 或 NAT-PMP 映射到的外部端口；都没结果时为还没有结果）与有没有节点从外面连进来过。
    pub(crate) fn session_facts(&self) -> crate::netcheck::Session {
        use crate::netcheck::Mapping;
        let network = self.session.network();
        let mapped = [network.upnp_port, network.natpmp_port]
            .into_iter()
            .find(|&port| port != 0);
        let errors: Vec<String> = [
            ("UPnP", &network.upnp_error),
            ("NAT-PMP", &network.natpmp_error),
        ]
        .into_iter()
        .filter(|(_, error)| !error.is_empty())
        .map(|(via, error)| format!("{via}：{error}"))
        .collect();
        let mapping = match mapped {
            Some(port) => Mapping::Mapped(port),
            None if errors.is_empty() => Mapping::Pending,
            None => Mapping::Failed(errors.join("；")),
        };
        let listen_error = (network.listen_port == 0).then(|| {
            if network.listen_error.is_empty() {
                "还没监听上".to_owned()
            } else {
                network.listen_error.clone()
            }
        });
        crate::netcheck::Session {
            listen_error,
            mapping,
            incoming: network.incoming,
        }
    }

    /// BT 代理的问题：探出用不了的原因，或 libtorrent 经它的 UDP 最近一次出的错；直连或没问题时为 None。
    fn proxy_problem(&self) -> Option<String> {
        match &*self.proxy_check.borrow() {
            // 改成直连之后才取出的、经旧代理的出错不算
            ProxyCheck::Direct => return None,
            ProxyCheck::Unreachable(why) | ProxyCheck::Refused(why) | ProxyCheck::Silent(why) => {
                return Some(why.clone());
            }
            ProxyCheck::Unchecked | ProxyCheck::Checking | ProxyCheck::Usable => {}
        }
        let error = self.session.network().proxy_error;
        (!error.is_empty()).then(|| format!("代理出错：{error}"))
    }
}

impl Engine {
    fn pump(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            let alerts = self
                .session
                .pop_alerts(PUMP_WAIT_MS)
                .unwrap_or_else(|error| {
                    panic!("BT 会话的事件取不出，会话里的状态不再可信：{error}")
                });
            for alert in alerts {
                self.dispatch(alert);
            }
        }
    }

    fn dispatch(&self, alert: Alert) {
        match alert.kind {
            AlertKind::AlertsDropped => {
                for route in lock(&self.routes).values() {
                    // 接收端随管种子的异步任务结束而丢弃：那个种子已没人在等
                    let _gone = route.send(Routed::Lost);
                }
                // 丢的可能有移除的事件：在等移除完的重新查（绑定这时按种子对象还在不在判断）
                self.removed.notify_waiters();
            }
            AlertKind::Stats => self.stats.notify_waiters(),
            // 监听与端口映射的结果由绑定记下、诊断时读（Session::network）；加入失败时同步的加入已报错
            AlertKind::ListenSucceeded
            | AlertKind::ListenFailed
            | AlertKind::PortMapped
            | AlertKind::PortMapFailed
            | AlertKind::AddFailed => {}
            kind => {
                let settled = matches!(
                    kind,
                    AlertKind::Removed | AlertKind::Deleted | AlertKind::DeleteFailed
                );
                if let Ok(id) = TorrentId::parse(&alert.torrent)
                    && let Some(route) = lock(&self.routes).get(&id)
                {
                    // 同上
                    let _gone = route.send(Routed::Alert(alert));
                }
                if settled {
                    self.removed.notify_waiters();
                }
            }
        }
    }

    /// 由管种子 `id` 的异步任务接手它的事件；已有别的在管时报错（同一个种子正在取种子信息或下载）。
    fn own(self: &Arc<Self>, id: &TorrentId) -> Result<Owner, Error> {
        let (sender, alerts) = mpsc::unbounded_channel();
        let mut routes = lock(&self.routes);
        if routes.contains_key(id) {
            return Err(Error::Torrent(format!(
                "种子 {id} 正在别处使用（取种子信息或下载）"
            )));
        }
        routes.insert(id.clone(), sender.clone());
        Ok(Owner {
            engine: self.clone(),
            id: id.clone(),
            alerts,
            route: sender,
        })
    }

    /// 撤掉种子 `id` 转给 `route` 的登记；已换成别的登记（同一个种子之后的）时不动。
    fn release(&self, id: &TorrentId, route: &mpsc::UnboundedSender<Routed>) {
        let mut routes = lock(&self.routes);
        if routes
            .get(id)
            .is_some_and(|sender| sender.same_channel(route))
        {
            routes.remove(id);
        }
    }

    /// 等种子 `id` 移除完（之前移除的还没完时再加入会报错）；[`STEP_TIMEOUT`] 内没完报 [`Error::Torrent`]。
    async fn settled(&self, id: &TorrentId) -> Result<(), Error> {
        let waiting = async {
            loop {
                // 先登记再查：查完到等待之间来的通知不会漏掉
                let notified = self.removed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !self.session.removing(id) {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(STEP_TIMEOUT, waiting)
            .await
            .map_err(|_| Error::Torrent(format!("种子 {id} 之前的移除 10 秒内没做完")))
    }
}

/// 管一个种子的异步任务手里的：它的事件；释放时撤掉登记（见 [`Engine::release`]），不再接收。
struct Owner {
    engine: Arc<Engine>,
    id: TorrentId,
    alerts: mpsc::UnboundedReceiver<Routed>,
    /// 登记的发送端，撤登记时认是不是自己的
    route: mpsc::UnboundedSender<Routed>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.engine.release(&self.id, &self.route);
    }
}

impl Owner {
    fn session(&self) -> &Session {
        &self.engine.session
    }

    fn libtorrent(&self, what: &str) -> impl FnOnce(hs_dlx_torrent::Error) -> Error + use<> {
        let what = format!("{what}（种子 {}）", self.id);
        move |error| Error::Torrent(format!("{what}：{error}"))
    }

    /// 下一条事件；事件丢了时为 [`Routed::Lost`]。接收不会结束：自己持有一个发送端（`route`）。
    async fn next(&mut self) -> Routed {
        self.alerts
            .recv()
            .await
            .expect("Owner 自己持有一个发送端，接收不会结束")
    }

    /// 等到 `wanted` 认的那条事件并返回它，[`STEP_TIMEOUT`] 内没等到、或事件丢了时报 [`Error::Torrent`]；其间的别的事件
    /// 交给 `others`。
    async fn step(
        &mut self,
        what: &str,
        mut wanted: impl FnMut(&Alert) -> bool,
        mut others: impl FnMut(&Alert),
    ) -> Result<Alert, Error> {
        let id = self.id.clone();
        let waiting = async {
            loop {
                match self.next().await {
                    Routed::Alert(alert) if wanted(&alert) => return Ok(alert),
                    Routed::Alert(alert) => others(&alert),
                    Routed::Lost => {
                        return Err(Error::Torrent(format!(
                            "等{what}时 BT 会话的事件丢了（种子 {id}）"
                        )));
                    }
                }
            }
        };
        tokio::time::timeout(STEP_TIMEOUT, waiting)
            .await
            .map_err(|_| Error::Torrent(format!("10 秒内没等到{what}（种子 {}）", self.id)))?
    }

    /// 等着给这个种子补公共 tracker；`wanted` 为否时不补。
    fn supplement(&self, wanted: bool) -> Supplement {
        Supplement {
            public: self.engine.public.clone(),
            done: !wanted,
        }
    }

    /// 补上公共 tracker `trackers`；种子已不在会话里时报 [`Error::Torrent`]。
    fn add_public(&self, trackers: &[String]) -> Result<(), Error> {
        self.session()
            .add_trackers(&self.id, trackers)
            .map(drop)
            .map_err(self.libtorrent("补公共 tracker"))
    }

    /// 从会话里移除（照 `removal` 连带删文件）并等它移除完；删文件失败时报 [`Error::Torrent`]，说明为 libtorrent 的。
    async fn remove(&mut self, removal: Removal) -> Result<(), Error> {
        self.session()
            .remove(&self.id, removal)
            .map_err(self.libtorrent("移除"))?;
        let (mut removed, mut deleted) = (false, removal == Removal::Keep);
        let mut failed = None;
        while !(removed && deleted) {
            let alert = self
                .step(
                    "移除完",
                    |alert| {
                        matches!(
                            alert.kind,
                            AlertKind::Removed | AlertKind::Deleted | AlertKind::DeleteFailed
                        )
                    },
                    |_| {},
                )
                .await?;
            match alert.kind {
                AlertKind::Removed => removed = true,
                AlertKind::Deleted => deleted = true,
                _ => {
                    deleted = true;
                    failed = Some(alert.message);
                }
            }
        }
        match failed {
            Some(message) => Err(Error::Torrent(format!("删文件没删干净：{message}"))),
            None => Ok(()),
        }
    }
}

/// 一个种子等着补的公共 tracker。
struct Supplement {
    public: watch::Receiver<PublicTrackers>,
    /// 补过了，或不补
    done: bool,
}

impl Supplement {
    /// 列表取到了、还没补时返回它（之后算补过了）；补过、不补或不会再取到（BT 引擎已释放）时一直不返回，放在
    /// `select!` 里等。
    async fn ready(&mut self) -> Arc<[String]> {
        while !self.done {
            if let PublicTrackers::Ready(trackers) = &*self.public.borrow_and_update() {
                self.done = true;
                return trackers.clone();
            }
            if self.public.changed().await.is_err() {
                break;
            }
        }
        std::future::pending().await
    }

    /// 要补而列表这次没取到时为原因。
    fn failure(&self) -> Option<String> {
        match &*self.public.borrow() {
            PublicTrackers::Failed(why) if !self.done => Some(why.clone()),
            _ => None,
        }
    }
}

/// 引擎的持有者：第一次用到时建会话与取事件的线程，关闭时收掉。
pub(crate) struct Bt {
    network: TorrentNetwork,
    /// 关闭了（[`Bt::close`]）：不再建会话
    closed: AtomicBool,
    /// 上传限速（字节每秒，None 为不限）：建会话时照它，之后改了随即照新的；持着 `running` 的锁改与读
    upload_limit: Mutex<Option<NonZeroU32>>,
    /// 经的 SOCKS5 代理（None 为直连），同上
    proxy: Mutex<Option<Url>>,
    /// 代理探到的情况；改代理时随之重置
    proxy_check: watch::Sender<ProxyCheck>,
    /// 记着能用时探到经它的 UDP 没回（[`ProxyCheck::Silent`]）、还没改记：再探一次也是才改记（UDP 偶尔整轮丢包）；
    /// 持着 `running` 的锁改与读
    proxy_doubted: AtomicBool,
    /// libtorrent 可能已放弃连代理、还没让它重连：探出过握手被拒（[`ProxyCheck::Refused`]），或用户在还没探到能用时
    /// 重新保存了同一个；之后握手与申请转发成了（能用，或接了转发而 UDP 没回）时重连并清掉。持着 `running` 的锁改与读
    proxy_reconnect: AtomicBool,
    /// BT 会话在时探出过引导节点被 TUN 代理解析成假地址、经代理问不通：libtorrent 只在启动 DHT 时解析一次，手里的多半
    /// 也是假地址，之后解析到真实地址也要重启才生效。与代理的设置无关，到进程退出都在；持着 `running` 的锁改与读。
    /// 简化：从探测推断，两种情况会多提示一次重启：会话建起之后才开 TUN 的 fake-ip（libtorrent 手里是真实地址）；代理
    /// 就是 TUN 代理自己的 SOCKS 端口而引导节点只排除了一部分（libtorrent 手里的假地址经它本来就通）。这类误报成了问题
    /// 时，改为建会话时照同一个解析器解析一次引导节点来定，并看回应来自真实地址还是假地址。
    proxy_fake_ip: AtomicBool,
    /// 磁力链接取种子信息时种子的保存目录：只取信息、不下内容，libtorrent 也可能在里面建空文件，不放进用户的目录
    probe_dir: PathBuf,
    /// 公共 tracker 列表的来源（空时不补）与状况
    public_sources: Vec<Url>,
    public: watch::Sender<PublicTrackers>,
    running: Mutex<Option<Running>>,
}

struct Running {
    engine: Arc<Engine>,
    pump: ThreadHandle<()>,
}

/// 关闭时要存下的会话状态。
pub(crate) struct SessionState {
    /// 监听的端口；还没监听成功、或经代理（不接受连入，libtorrent 报 0）时为 None
    pub port: Option<u16>,
    /// DHT 的状态（路由表里的节点）
    pub dht: Vec<u8>,
}

impl Bt {
    /// `upload_limit` 与 `proxy` 为设置里的上传限速与 BT 代理。
    pub fn new(
        network: TorrentNetwork,
        upload_limit: Option<NonZeroU32>,
        proxy: Option<Url>,
    ) -> Bt {
        let public_sources = network.public_trackers();
        let public = if public_sources.is_empty() {
            PublicTrackers::Off
        } else {
            PublicTrackers::Idle
        };
        Bt {
            network,
            upload_limit: Mutex::new(upload_limit),
            proxy_check: watch::Sender::new(if proxy.is_some() {
                ProxyCheck::Unchecked
            } else {
                ProxyCheck::Direct
            }),
            proxy: Mutex::new(proxy),
            proxy_doubted: AtomicBool::new(false),
            proxy_reconnect: AtomicBool::new(false),
            proxy_fake_ip: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            probe_dir: std::env::temp_dir().join("hs-dlx-magnet"),
            public_sources,
            public: watch::Sender::new(public),
            running: Mutex::new(None),
        }
    }

    /// 公共 tracker 列表还没开始取时记为在取，返回列表的来源（由调用方去取，结果交给 [`Bt::set_public`]）；已开始取
    /// 过或不补时为 None。
    pub fn begin_public(&self) -> Option<Vec<Url>> {
        self.public
            .send_if_modified(|public| {
                let idle = *public == PublicTrackers::Idle;
                if idle {
                    *public = PublicTrackers::Fetching;
                }
                idle
            })
            .then(|| self.public_sources.clone())
    }

    /// 记下取公共 tracker 列表的结果：取到的随即补给会话里该补的种子。
    pub fn set_public(&self, public: PublicTrackers) {
        self.public.send_replace(public);
    }

    /// 引擎；还没有时照上次存下的 `saved`（端口与 DHT 状态）建会话。已关闭时报 [`Error::Closed`]，会话建不出时报
    /// [`Error::Torrent`]。
    pub fn engine(&self, saved: SessionState) -> Result<Arc<Engine>, Error> {
        let mut running = lock(&self.running);
        let upload_limit = *lock(&self.upload_limit);
        let proxy = libtorrent_proxy(lock(&self.proxy).as_ref());
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        if let Some(running) = &*running {
            return Ok(running.engine.clone());
        }
        let version = env!("CARGO_PKG_VERSION");
        let (listen, discovery) = match &self.network {
            TorrentNetwork::Internet => {
                let port = saved.port.unwrap_or(0);
                (format!("0.0.0.0:{port},[::]:{port}"), true)
            }
            TorrentNetwork::Loopback { .. } => ("127.0.0.1:0".to_owned(), false),
        };
        let session = Session::new(&SessionConfig {
            listen_interfaces: listen,
            dht: discovery,
            // 与探 BT 代理、网络检测问的同一份
            dht_bootstrap_nodes: crate::netcheck::DHT_BOOTSTRAP
                .iter()
                .map(|(host, port)| format!("{host}:{port}"))
                .collect::<Vec<_>>()
                .join(","),
            upnp: discovery,
            natpmp: discovery,
            lsd: discovery,
            user_agent: format!("hs-dlx/{version}"),
            peer_fingerprint: fingerprint(version),
            upload_limit: limit_of(upload_limit),
            proxy,
            state: saved.dht,
        })
        .map_err(|error| Error::Torrent(format!("BT 会话建不出：{error}")))?;
        // 新会话照现在的设置连代理，之前的放弃不算
        self.proxy_reconnect.store(false, Ordering::Relaxed);
        let engine = Arc::new(Engine {
            session,
            routes: Mutex::new(HashMap::new()),
            removed: Notify::new(),
            stats: Notify::new(),
            stop: AtomicBool::new(false),
            public: self.public.subscribe(),
            proxy_check: self.proxy_check.subscribe(),
        });
        let pumped = engine.clone();
        let pump = std::thread::Builder::new()
            .name("hs-dlx-bt-alerts".into())
            .spawn(move || pumped.pump())
            .map_err(|cause| Error::Io {
                action: "启动",
                path: "BT 事件线程".into(),
                cause,
            })?;
        *running = Some(Running {
            engine: engine.clone(),
            pump,
        });
        Ok(engine)
    }

    /// 关闭：停下取事件的线程（最多等 [`PUMP_WAIT_MS`]），返回要存下的会话状态；没建过会话时为 None。会话随最后一个
    /// 持有引擎的释放而关，释放时阻塞（见 [`Session`]）。
    pub fn close(&self) -> Result<Option<SessionState>, Error> {
        let taken = {
            let mut running = lock(&self.running);
            self.closed.store(true, Ordering::Relaxed);
            running.take()
        };
        let Some(running) = taken else {
            return Ok(None);
        };
        running.engine.stop.store(true, Ordering::Relaxed);
        if let Err(panic) = running.pump.join() {
            std::panic::resume_unwind(panic);
        }
        // 取事件的线程停了，统计不会再来：在等的诊断随即结束
        running.engine.stats.notify_waiters();
        let session = &running.engine.session;
        let dht = session
            .session_state()
            .map_err(|error| Error::Torrent(format!("BT 会话的 DHT 状态写不出：{error}")))?;
        let port = Some(session.listen_port()).filter(|&port| port != 0);
        Ok(Some(SessionState { port, dht }))
    }

    /// 改上传限速（字节每秒，None 为不限）：已建的会话随即照它，之后建的也照它。
    pub fn set_upload_limit(&self, upload_limit: Option<NonZeroU32>) {
        let running = lock(&self.running);
        *lock(&self.upload_limit) = upload_limit;
        if let Some(running) = &*running {
            running
                .engine
                .session
                .set_upload_limit(limit_of(upload_limit));
        }
    }

    /// 改经的代理（None 为直连；调用方已校验，见 [`crate::socks::check`]）：已建的会话随即照它，之后建的也照它。交给
    /// libtorrent 的设置不变时（重新保存同一个，或写法不同而等价，如 socks5 与 socks5h）只换记着的地址，探到的情况与
    /// libtorrent 的连接都照旧；只是记着的不是能用时，记下下次握手与申请转发成了时让 libtorrent 重连（它在握手被拒后
    /// 不再自己重试，有的还不报错）。
    pub fn set_proxy(&self, proxy: Option<Url>) {
        let running = lock(&self.running);
        let socks = libtorrent_proxy(proxy.as_ref());
        let direct = proxy.is_none();
        let same = {
            let mut current = lock(&self.proxy);
            let same = !direct && libtorrent_proxy(current.as_ref()) == socks;
            *current = proxy;
            same
        };
        if same {
            if *self.proxy_check.borrow() != ProxyCheck::Usable {
                self.proxy_reconnect.store(true, Ordering::Relaxed);
            }
            return;
        }
        self.proxy_doubted.store(false, Ordering::Relaxed);
        // 换了设置，libtorrent 随即照新的连
        self.proxy_reconnect.store(false, Ordering::Relaxed);
        self.proxy_check.send_replace(if direct {
            ProxyCheck::Direct
        } else {
            ProxyCheck::Unchecked
        });
        if let Some(running) = &*running {
            running.engine.session.set_proxy(&socks);
        }
    }

    /// 经的代理（去掉凭据的写法）与探到的情况；直连时为 None。
    pub fn proxy(&self) -> Option<(String, ProxyCheck)> {
        lock(&self.proxy).as_ref().map(|url| {
            (
                hs_m3u8_core::bare_url(url),
                self.proxy_check.borrow().clone(),
            )
        })
    }

    /// 经的代理的地址；直连时为 None。
    pub fn proxy_url(&self) -> Option<Url> {
        lock(&self.proxy).clone()
    }

    /// 公共 tracker 列表现在的状况。
    pub(crate) fn public(&self) -> PublicTrackers {
        self.public.borrow().clone()
    }

    /// 探 BT 代理时问的 DHT 引导节点：只连本机时（测试）为空，只握手。
    pub(crate) fn dht_bootstrap(&self) -> &'static [(&'static str, u16)] {
        match self.network {
            TorrentNetwork::Internet => &crate::netcheck::DHT_BOOTSTRAP,
            TorrentNetwork::Loopback { .. } => &[],
        }
    }

    /// 后台看着时记下探 `url` 的结果（见 [`Bt::record_proxy_check`]）：记着能用而这次经它的 UDP 没回
    /// （[`ProxyCheck::Silent`]）时先不改记，再探一次也是才改记（UDP 偶尔整轮丢包）。
    pub fn set_proxy_check(&self, url: &Url, check: ProxyCheck, resolved: Resolved) {
        self.record(url, check, resolved, true);
    }

    /// 记下探 `url` 的结果，不存疑（用户点的网络检测：报告与记下的一致）；代理已改成别的时不记。这次握手与申请转发成了
    /// （能用，或接了转发而 UDP 没回）、而 libtorrent 可能已放弃连代理（见 `proxy_reconnect`）或经它报过错时，让它重连
    /// 并清掉那条出错：报过错即原来的转发已断，它之后的重试可能在握手时被拒而不再重试、也不报错。重连会断开经代理的
    /// uTP 连接，别的时候不重连。BT 会话在时解析到过 TUN 的假地址而问不通（见 `proxy_fake_ip`），之后这次有引导节点解析到真实地址而能用时，记为没转发
    /// 出去并说明要重启；全是假地址而能用的（代理就是 TUN 代理自己的 SOCKS 端口）照实记。`resolved` 为这次引导节点解析
    /// 得怎样。返回现在记着的（报告照它出）；代理已改成别的时原样返回 `check`。
    pub fn record_proxy_check(
        &self,
        url: &Url,
        check: ProxyCheck,
        resolved: Resolved,
    ) -> ProxyCheck {
        self.record(url, check, resolved, false)
    }

    fn record(&self, url: &Url, check: ProxyCheck, resolved: Resolved, doubt: bool) -> ProxyCheck {
        let running = lock(&self.running);
        if lock(&self.proxy).as_ref() != Some(url) {
            return check;
        }
        // 这次探到的代理本身能用（决定要不要让 libtorrent 重连），与记下的分开
        let reachable = check == ProxyCheck::Usable;
        let fake_seen = self.proxy_fake_ip.load(Ordering::Relaxed);
        let recorded = self.proxy_check.borrow().clone();
        let about_fake_ip = recorded.problem().is_some_and(|why| {
            why == crate::netcheck::PROXY_FAKE_IP.as_str()
                || why == crate::netcheck::RESTART_AFTER_FAKE_IP
        });
        let check = if reachable && fake_seen && about_fake_ip && !resolved.real && !resolved.fake {
            // 记着的是假地址的结论，这次引导节点一个都没解析出来、判断不了：照旧
            recorded.clone()
        } else if reachable && resolved.real && fake_seen {
            // 假地址改好了，BT 还用着旧的：直到重启都算没转发出去。仍是假地址而能用的（代理是 TUN 自己的 SOCKS 端口）
            // 照实记
            ProxyCheck::Silent(crate::netcheck::RESTART_AFTER_FAKE_IP.to_owned())
        } else {
            check
        };
        // 握手与申请转发成了（能用，或接了转发而 UDP 没回）就照需要重连，不论记下的是什么、存不存疑：libtorrent 在这之后
        // 经代理的 UDP 至少能连上代理
        let associated = matches!(check, ProxyCheck::Usable | ProxyCheck::Silent(_));
        if associated && let Some(running) = &*running {
            let session = &running.engine.session;
            if self.proxy_reconnect.swap(false, Ordering::Relaxed)
                || !session.network().proxy_error.is_empty()
            {
                session.reconnect_proxy(&libtorrent_proxy(Some(url)));
            }
        }
        if doubt
            && matches!(check, ProxyCheck::Silent(_))
            && recorded == ProxyCheck::Usable
            && !self.proxy_doubted.swap(true, Ordering::Relaxed)
        {
            return recorded;
        }
        if running.is_some() && resolved.fake && check.problem().is_some() {
            self.proxy_fake_ip.store(true, Ordering::Relaxed);
        }
        self.proxy_doubted.store(false, Ordering::Relaxed);
        if matches!(check, ProxyCheck::Refused(_)) {
            self.proxy_reconnect.store(true, Ordering::Relaxed);
        }
        self.proxy_check.send_replace(check.clone());
        check
    }

    /// 经的代理还是 `url` 时，该不该再探：还没探（随即记为在探）、在探、探出用不了、记着能用而上次没回（见
    /// [`Bt::set_proxy_check`]），或 libtorrent 经它报过错时该；代理已改成别的或直连时为 None。
    pub fn proxy_due(&self, url: &Url) -> Option<bool> {
        let running = lock(&self.running);
        if lock(&self.proxy).as_ref() != Some(url) {
            return None;
        }
        let errored = running
            .as_ref()
            .is_some_and(|running| !running.engine.session.network().proxy_error.is_empty());
        let mut due = errored || self.proxy_doubted.load(Ordering::Relaxed);
        self.proxy_check.send_if_modified(|check| match check {
            ProxyCheck::Unchecked => {
                *check = ProxyCheck::Checking;
                due = true;
                true
            }
            ProxyCheck::Checking
            | ProxyCheck::Unreachable(_)
            | ProxyCheck::Refused(_)
            | ProxyCheck::Silent(_) => {
                due = true;
                false
            }
            ProxyCheck::Direct | ProxyCheck::Usable => false,
        });
        Some(due)
    }

    /// 已建的引擎；还没建会话时为 None。
    pub fn existing(&self) -> Option<Arc<Engine>> {
        lock(&self.running)
            .as_ref()
            .map(|running| running.engine.clone())
    }

    pub fn probe_dir(&self) -> &Path {
        &self.probe_dir
    }
}

/// 上传限速在 libtorrent 里的写法：字节每秒，0 为不限；超过 i32 的当作不限。
fn limit_of(limit: Option<NonZeroU32>) -> i32 {
    limit.map_or(0, |limit| i32::try_from(limit.get()).unwrap_or(0))
}

/// 节点 ID 的前缀（Azureus 的写法）：`-HD` + 版本号的主、次、修订各一位 + `0-`，超过 9 的写 9。
fn fingerprint(version: &str) -> String {
    let digit = |part: Option<&str>| {
        part.and_then(|part| part.parse::<u32>().ok())
            .map_or('0', |n| char::from_digit(n.min(9), 10).expect("0 到 9"))
    };
    let mut parts = version.split('.');
    let (major, minor, patch) = (
        digit(parts.next()),
        digit(parts.next()),
        digit(parts.next()),
    );
    format!("-HD{major}{minor}{patch}0-")
}

/// 磁力链接 `uri` 取到种子信息，返回它的续传数据；其间把连着的节点数报进 `progress`。同一个种子之前的移除还没做完
/// 时先等它（最多 10 秒）。不是磁力链接、这个种子正在下载或取种子信息、或之前的移除 10 秒内没做完时报
/// [`Error::Torrent`]。任务被取消（句柄释放）时种子随之从会话里移除。
pub(crate) async fn fetch_metadata(
    engine: Arc<Engine>,
    uri: String,
    probe_dir: PathBuf,
    progress: watch::Sender<MetadataProgress>,
    slot: FetchSlot,
) -> Result<Vec<u8>, Error> {
    let save_path = utf8(&probe_dir)?;
    let not_added =
        |error: hs_dlx_torrent::Error| Error::Torrent(format!("加不进磁力链接：{error}"));
    // 同一个磁力链接刚取消或取完、还在移除时，等它移除完再加
    let pending = hs_dlx_torrent::magnet_id(&uri).map_err(not_added)?;
    engine.settled(&pending).await?;
    let mut owner = slot.add(&engine, || {
        engine
            .session
            .add_magnet(&uri, save_path)
            .map_err(not_added)
    })?;
    let id = owner.id.clone();
    // 还不知道私不私有：链接没带 tracker 时才补（带了的可能是私有种子的 tracker，不让信息哈希经公共 tracker 传出去）
    let bare = owner
        .session()
        .trackers(&id)
        .map_err(owner.libtorrent("查 tracker"))?
        .is_empty();
    let mut public = owner.supplement(bare);
    let mut ticks = tokio::time::interval(STATUS_INTERVAL);
    let mut tracker_error = None;
    // 登记之前已取到的，事件不会再来：每次先看状态
    loop {
        let status = owner
            .session()
            .status(&id)
            .map_err(owner.libtorrent("查状态"))?;
        if status.has_metadata {
            break;
        }
        progress.send_replace(MetadataProgress {
            peers: u32::try_from(status.peers).expect("libtorrent 的计数不为负"),
            tracker_error: tracker_error.clone(),
            public_trackers_error: public.failure(),
            proxy_problem: owner.engine.proxy_problem(),
        });
        // 取到种子信息的事件丢了也不要紧：每次都看状态
        tokio::select! {
            routed = owner.next() => {
                if let Routed::Alert(alert) = routed
                    && alert.kind == AlertKind::TrackerError
                {
                    tracker_error = Some(alert.message);
                }
            }
            trackers = public.ready() => owner.add_public(&trackers)?,
            _ = ticks.tick() => {}
        }
    }
    owner
        .session()
        .save_resume_data(&id)
        .map_err(owner.libtorrent("要续传数据"))?;
    let alert = owner
        .step(
            "续传数据",
            |alert| {
                matches!(
                    alert.kind,
                    AlertKind::ResumeData | AlertKind::ResumeDataFailed
                )
            },
            |_| {},
        )
        .await?;
    if alert.kind == AlertKind::ResumeDataFailed {
        return Err(Error::Torrent(format!(
            "种子信息取到了，续传数据写不出：{}",
            alert.message
        )));
    }
    slot.finish();
    drop(owner);
    Ok(alert.data)
}

/// 取种子信息用的种子：释放（取到、出错或被取消）时从会话里移除，不等移除完（再加入同一个种子的会等）。
struct Probe {
    engine: Arc<Engine>,
    id: TorrentId,
}

impl Drop for Probe {
    fn drop(&mut self) {
        // 移除不了只在它已不在会话里时：那正是要的结果
        let _absent = self.engine.session.remove(&self.id, Removal::Keep);
    }
}

/// 取种子信息加进会话的那个种子与它事件的登记，由取的任务与取的句柄共有：句柄释放时当场撤登记、移除种子（不等任务
/// 被运行时丢弃），之后马上再取同一个磁力链接也加得进、登记得上。
#[derive(Clone, Default)]
pub(crate) struct FetchSlot(Arc<Mutex<Slot>>);

#[derive(Default)]
enum Slot {
    /// 还没加进会话
    #[default]
    Empty,
    /// 加进去了、登记了：释放时撤登记（只撤自己的）、移除种子
    Added {
        probe: Probe,
        route: mpsc::UnboundedSender<Routed>,
    },
    /// 句柄已释放：不再加，加了的已移除
    Closed,
}

impl FetchSlot {
    /// 在锁内加入（`add` 返回种子编号）、接手它的事件（见 [`Engine::own`]）并记下：句柄已释放时不加，报
    /// [`Error::Torrent`]；接手不了时移除刚加的再报错。加入与释放互斥，释放之后不会再有加进去的。
    fn add(
        &self,
        engine: &Arc<Engine>,
        add: impl FnOnce() -> Result<TorrentId, Error>,
    ) -> Result<Owner, Error> {
        let mut slot = lock(&self.0);
        if matches!(*slot, Slot::Closed) {
            return Err(Error::Torrent("取种子信息已取消".to_owned()));
        }
        let id = add()?;
        let probe = Probe {
            engine: engine.clone(),
            id: id.clone(),
        };
        let owner = engine.own(&id)?;
        *slot = Slot::Added {
            probe,
            route: owner.route.clone(),
        };
        Ok(owner)
    }

    /// 取完或句柄释放（`closed` 时之后不再加）：撤登记、移除加进去的种子。
    fn release(&self, closed: bool) {
        let next = if closed { Slot::Closed } else { Slot::Empty };
        if let Slot::Added { probe, route } = std::mem::replace(&mut *lock(&self.0), next) {
            probe.engine.release(&probe.id, &route);
            drop(probe);
        }
    }

    /// 取完：撤登记、移除加进去的种子。
    fn finish(&self) {
        self.release(false);
    }

    /// 句柄释放：撤登记、移除加进去的种子，之后不再加。
    pub(crate) fn close(&self) {
        self.release(true);
    }
}

/// 一次 BT 任务的运行怎么结束的。
#[derive(Debug)]
pub(crate) enum Outcome {
    /// 选了的文件都下完了；值为要下的字节数
    Finished(u64),
    Paused,
    Failed(Failure),
}

/// 一次运行的结果：怎么结束的，与停下时要存的续传数据（None 时存储里原有的照旧：没要到，或运行没开始）；下完了照设置
/// 接着做种时为还在会话里的种子（[`seed`]）。
pub(crate) struct Ended {
    pub outcome: Outcome,
    pub resume: Option<Resume>,
    pub seeder: Option<Seeder>,
}

/// 下完了、要接着做种的种子：还在会话里，由它的异步任务接着管。
pub(crate) struct Seeder {
    owner: Owner,
}

/// 运行一个 BT 任务：由续传数据 `resume`（`trusted` 见 [`Resume::trusted`]）加入种子 `info_hash`，下到 `save_path`，进度
/// 交给 `each`，直到下完、出错或 `cancel` 取消（暂停）。停下时要到续传数据；下完了而做种的设置 `seeding` 还没到停的
/// 条件时留在会话里接着做种（[`Ended::seeder`]），否则从会话里移除。
pub(crate) async fn run(
    engine: Arc<Engine>,
    info_hash: &str,
    resume: Resume,
    save_path: PathBuf,
    seeding: &watch::Receiver<Seeding>,
    cancel: CancellationToken,
    mut each: impl FnMut(TorrentProgress),
) -> Ended {
    let failed = |error: Error| Ended {
        outcome: Outcome::Failed(Failure {
            kind: FailureKind::Other,
            message: error.to_string(),
        }),
        resume: None,
        seeder: None,
    };
    let id = match TorrentId::parse(info_hash) {
        Ok(id) => id,
        // 存储里读出的编号认不出：再试也一样
        Err(error) => {
            return Ended {
                outcome: Outcome::Failed(Failure {
                    kind: FailureKind::Unrecoverable,
                    message: Error::Corrupt(format!("种子的编号：{error}")).to_string(),
                }),
                resume: None,
                seeder: None,
            };
        }
    };
    let mut owner = match engine.own(&id) {
        Ok(owner) => owner,
        Err(error) => return failed(error),
    };
    if let Err(error) = engine.settled(&id).await {
        return failed(error);
    }
    let options = AddOptions {
        trust_pieces: resume.trusted,
        paused: false,
        upload_only: false,
    };
    let added = utf8(&save_path).and_then(|path| {
        owner
            .session()
            .add_resume(&resume.data, &id, path, options)
            .map_err(owner.libtorrent("加入"))
    });
    if let Err(error) = added {
        return failed(error);
    }
    let outcome = drive(&mut owner, &cancel, &mut each).await;
    if let Outcome::Finished(wanted) = outcome
        && !reached(&owner, *seeding.borrow(), wanted)
    {
        // 接着做种：存下进度，种子留在会话里
        let (resume, problem) = snapshot(&mut owner).await;
        let Some(failure) = problem else {
            return Ended {
                outcome: Outcome::Finished(wanted),
                resume,
                seeder: Some(Seeder { owner }),
            };
        };
        // 写盘出错：已下的不一定都在磁盘上，不做种，照下面停下时出错处理（移除时出的错不盖过它）
        let _removed = owner.remove(Removal::Keep).await;
        return Ended {
            outcome: Outcome::Failed(failure),
            resume,
            seeder: None,
        };
    }
    // 下完了不做种时 .parts 照旧留着：之后再做种要从它读没选的文件与选了的共用的块，从列表移除时才删
    let (resume, stopped) = stop(&mut owner, Removal::Keep).await;
    // 停下时写盘出错或等不到结果：已下的不一定都在磁盘上、之后不一定立即加得进，下完与暂停都改报这个失败
    let outcome = match (outcome, stopped) {
        (Outcome::Failed(failure), _) => Outcome::Failed(failure),
        (_, Err(failure)) => Outcome::Failed(failure),
        (outcome, Ok(())) => outcome,
    };
    Ended {
        outcome,
        resume,
        seeder: None,
    }
}

/// 下完了、选了的文件共 `wanted` 字节的种子到了做种的设置 `seeding` 停的条件没有（按 libtorrent 记的累计上传与做种
/// 时长）；查不到状态时算到了（不接着做种）。
fn reached(owner: &Owner, seeding: Seeding, wanted: u64) -> bool {
    match owner.session().status(&owner.id) {
        Ok(status) => seeding.reached(
            u64::try_from(status.all_time_upload).expect("字节数不为负"),
            wanted,
            u64::try_from(status.finished_seconds).expect("秒数不为负"),
        ),
        // 不接着做种，移除时照常报出问题
        Err(_) => true,
    }
}

/// 做种为什么停了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedEnd {
    /// 到了做种设置里停的条件（或设置改成了下完不做种）
    Reached,
    /// 手动停止，或应用退出
    Stopped,
    /// 磁盘上的文件不全（挪走、删了或大小变了；只改了内容、大小不变的认不出）：不补下，不再做种
    Incomplete,
    /// 出错（读文件出错、BT 会话出错等）：说明
    Failed(String),
}

/// 一次做种的结果：为什么停了，与停下时要存的续传数据（None 时存储里原有的照旧）。
pub(crate) struct Seeded {
    pub end: SeedEnd,
    pub resume: Option<Resume>,
}

/// 由续传数据 `resume` 加入已下完的种子 `info_hash`（在 `save_path` 下）去做种：只上传，缺了的块不补下。没有这个种子
/// 的编号、已有别的在管它、之前的移除没做完或加不进时报错。
pub(crate) async fn start_seeding(
    engine: Arc<Engine>,
    info_hash: &str,
    resume: &Resume,
    save_path: &Path,
) -> Result<Seeder, Error> {
    let id = TorrentId::parse(info_hash)
        .map_err(|error| Error::Corrupt(format!("种子的编号：{error}")))?;
    let owner = engine.own(&id)?;
    engine.settled(&id).await?;
    let options = AddOptions {
        trust_pieces: resume.trusted,
        paused: false,
        upload_only: true,
    };
    owner
        .session()
        .add_resume(&resume.data, &id, utf8(save_path)?, options)
        .map_err(owner.libtorrent("加入"))?;
    Ok(Seeder { owner })
}

/// 做种，进度（阶段为校验或 [`TorrentPhase::Seeding`]）交给 `each`，文件齐全时每 [`SAVE_INTERVAL`] 存一次进度（续传数据
/// 交给 `save`；要不到时把原因交给它，这次不存、照常做种），直到到了做种的设置 `seeding`（随时可改）停的条件、`cancel`
/// 取消、校验出文件不全或出错；停下时要到续传数据并从会话里移除。文件不全时不要续传数据（它记的已有块是这次没找着文件
/// 时的，存下会盖掉好的，文件回来后也认不出），存储里的照旧。
pub(crate) async fn seed(
    seeder: Seeder,
    mut seeding: watch::Receiver<Seeding>,
    cancel: CancellationToken,
    mut each: impl FnMut(TorrentProgress),
    mut save: impl FnMut(Result<Resume, String>),
) -> Seeded {
    let mut owner = seeder.owner;
    let mut public = owner.supplement(public_wanted(&owner));
    let mut ticks = tokio::time::interval(STATUS_INTERVAL);
    let mut saves =
        tokio::time::interval_at(tokio::time::Instant::now() + SAVE_INTERVAL, SAVE_INTERVAL);
    let mut tracker_error = None;
    let end = loop {
        tokio::select! {
            () = cancel.cancelled() => break SeedEnd::Stopped,
            _ = saves.tick() => {
                // 只存文件齐全时的：校验中或刚校验出不全的不存（下一次状态即停下）
                let whole = owner
                    .session()
                    .status(&owner.id)
                    .is_ok_and(|status| {
                        status.finished
                            && matches!(status.state(), Some(State::Finished | State::Seeding))
                    });
                if whole {
                    match snapshot(&mut owner).await {
                        (_, Some(failure)) => save(Err(failure.message)),
                        (Some(resume), None) => save(Ok(resume)),
                        (None, None) => save(Err("续传数据写不出".to_owned())),
                    }
                }
            }
            routed = owner.next() => match routed {
                Routed::Alert(alert) => match alert.kind {
                    AlertKind::FileError => break SeedEnd::Failed(format!("读文件出错：{}", alert.message)),
                    AlertKind::TorrentError => break SeedEnd::Failed(format!("种子出错：{}", alert.message)),
                    AlertKind::TrackerError => tracker_error = Some(alert.message),
                    _ => {}
                },
                // 做种不等哪一步的结果，状态每半秒照查，丢了事件不影响
                Routed::Lost => {}
            },
            trackers = public.ready() => {
                if let Err(error) = owner.add_public(&trackers) {
                    break SeedEnd::Failed(error.to_string());
                }
            }
            // 设置改了：下一次状态时按新的判
            _ = seeding.changed() => {}
            _ = ticks.tick() => {
                let status = match owner.session().status(&owner.id) {
                    Ok(status) => status,
                    Err(error) => break SeedEnd::Failed(owner.libtorrent("查状态")(error).to_string()),
                };
                // 种子出错（事件可能在存进度时被那边收走了）
                if !status.error.is_empty() {
                    break SeedEnd::Failed(format!("种子出错：{}", status.error));
                }
                let mut progress = TorrentProgress::of(&status);
                progress.tracker_error.clone_from(&tracker_error);
                progress.proxy_problem = owner.engine.proxy_problem();
                if matches!(progress.phase, TorrentPhase::Checking { .. }) {
                    each(progress);
                    continue;
                }
                if !status.finished {
                    break SeedEnd::Incomplete;
                }
                progress.phase = TorrentPhase::Seeding;
                let done = *seeding.borrow_and_update();
                if done.reached(progress.uploaded, progress.wanted, progress.seeding_seconds) {
                    break SeedEnd::Reached;
                }
                each(progress);
            }
        }
    };
    let (resume, stopped) = stop(&mut owner, Removal::Keep).await;
    let resume = resume.filter(|_| end != SeedEnd::Incomplete);
    let end = match (end, stopped) {
        (SeedEnd::Failed(message), _) => SeedEnd::Failed(message),
        (_, Err(failure)) => SeedEnd::Failed(failure.message),
        (end, Ok(())) => end,
    };
    Seeded { end, resume }
}

/// 运行到下完、出错或取消。
async fn drive(
    owner: &mut Owner,
    cancel: &CancellationToken,
    each: &mut impl FnMut(TorrentProgress),
) -> Outcome {
    let mut public = owner.supplement(public_wanted(owner));
    let mut ticks = tokio::time::interval(STATUS_INTERVAL);
    // 连续几次状态凑不齐全部内容（见 SCARCE_TICKS）
    let mut short = 0;
    let mut tracker_error = None;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Outcome::Paused,
            routed = owner.next() => match routed {
                Routed::Alert(alert) => match alert.kind {
                    AlertKind::Finished => return finished(owner),
                    AlertKind::FileError => {
                        return Outcome::Failed(Failure {
                            kind: FailureKind::Disk,
                            message: format!("BT 下载读写文件出错：{}", alert.message),
                        });
                    }
                    AlertKind::TorrentError => {
                        return Outcome::Failed(Failure {
                            kind: FailureKind::Other,
                            message: format!("BT 种子出错：{}", alert.message),
                        });
                    }
                    AlertKind::TrackerError => tracker_error = Some(alert.message),
                    // 续传数据与磁盘对不上时 libtorrent 自己改为整体校验，照常运行
                    _ => {}
                },
                Routed::Lost => {
                    return Outcome::Failed(Failure {
                        kind: FailureKind::Other,
                        message: "BT 会话的事件丢了，这个任务的状态说不清；继续即可".to_owned(),
                    });
                }
            },
            trackers = public.ready() => {
                if let Err(error) = owner.add_public(&trackers) {
                    return Outcome::Failed(Failure {
                        kind: FailureKind::Other,
                        message: error.to_string(),
                    });
                }
            }
            _ = ticks.tick() => match owner.session().status(&owner.id) {
                Ok(status) => {
                    let mut progress = TorrentProgress::of(&status);
                    if progress.phase == TorrentPhase::Finishing {
                        return finished(owner);
                    }
                    let shortfall = progress.phase == TorrentPhase::Downloading
                        && progress.peers > 0
                        && progress.reachable < progress.pieces;
                    short = if shortfall { short + 1 } else { 0 };
                    progress.scarce = short >= SCARCE_TICKS;
                    progress.tracker_error.clone_from(&tracker_error);
                    progress.proxy_problem = owner.engine.proxy_problem();
                    each(progress);
                }
                Err(error) => {
                    return Outcome::Failed(Failure {
                        kind: FailureKind::Other,
                        message: owner.libtorrent("查状态")(error).to_string(),
                    });
                }
            },
        }
    }
}

/// 加入了的种子要补公共 tracker：不是私有的（私有种子只许经它自己的 tracker 找节点）。查不到状态时不补，接下来查状态
/// 时照常报出。
fn public_wanted(owner: &Owner) -> bool {
    owner
        .session()
        .status(&owner.id)
        .is_ok_and(|status| !status.private_torrent)
}

/// 下完了：结果为要下的字节数。
fn finished(owner: &Owner) -> Outcome {
    match owner.session().status(&owner.id) {
        Ok(status) => Outcome::Finished(u64::try_from(status.total_wanted).expect("字节数不为负")),
        Err(error) => Outcome::Failed(Failure {
            kind: FailureKind::Other,
            message: owner.libtorrent("查状态")(error).to_string(),
        }),
    }
}

/// 停下：存下进度（[`snapshot`]），再从会话里移除（照 `removal` 连带删文件）并等它移除完。返回要存的续传数据与停下时
/// 出的问题。
async fn stop(owner: &mut Owner, removal: Removal) -> (Option<Resume>, Result<(), Failure>) {
    let (resume, mut problem) = snapshot(owner).await;
    if let Err(error) = owner.remove(removal).await {
        problem.get_or_insert(Failure {
            kind: FailureKind::Other,
            message: format!("BT 任务停下时出错：{error}"),
        });
    }
    (resume, problem.map_or(Ok(()), Err))
}

/// 存下进度：先把缓存写到磁盘、等写完，再要续传数据。libtorrent 的续传数据只记交给磁盘写完的块（torrent.cpp 的
/// write_resume_data 按 is_piece_flushed；写失败的块撤回、不算已有），所以先写完才记得全，记的也不会多于磁盘上的。还没
/// 校验完已有的文件的（这时写出的续传数据里一块也没有，write_resume_data 按已校验到哪写），不要续传数据，存储里的照旧；
/// 这时也还没开始下，没有要写盘的。返回要存的续传数据（不要或没要到时为 None），与出的问题：写盘出错
/// （[`FailureKind::Disk`]，写失败的块没记进续传数据），或要不到、等不到结果。
async fn snapshot(owner: &mut Owner) -> (Option<Resume>, Option<Failure>) {
    let other = |error: Error| Failure {
        kind: FailureKind::Other,
        message: format!("BT 任务停下时出错：{error}"),
    };
    let checked = match owner.session().status(&owner.id) {
        Ok(status) => matches!(
            status.state(),
            Some(State::Downloading | State::Finished | State::Seeding)
        ),
        Err(error) => return (None, Some(other(owner.libtorrent("查状态")(error)))),
    };
    if !checked {
        return (None, None);
    }
    let mut write_error = None;
    let mut note = |alert: &Alert| {
        if alert.kind == AlertKind::FileError {
            write_error.get_or_insert_with(|| alert.message.clone());
        }
    };
    if let Err(error) = owner.session().flush(&owner.id) {
        return (None, Some(other(owner.libtorrent("写盘")(error))));
    }
    if let Err(error) = owner
        .step(
            "写完缓存",
            |alert| alert.kind == AlertKind::CacheFlushed,
            &mut note,
        )
        .await
    {
        return (None, Some(other(error)));
    }
    if let Err(error) = owner.session().save_resume_data(&owner.id) {
        return (None, Some(other(owner.libtorrent("要续传数据")(error))));
    }
    let alert = owner
        .step(
            "续传数据",
            |alert| {
                matches!(
                    alert.kind,
                    AlertKind::ResumeData | AlertKind::ResumeDataFailed
                )
            },
            &mut note,
        )
        .await;
    let mut problem = write_error.map(|message| Failure {
        kind: FailureKind::Disk,
        message: format!("BT 下载写文件出错：{message}"),
    });
    let resume = match alert {
        Ok(alert) if alert.kind == AlertKind::ResumeData => Some(Resume {
            data: alert.data,
            trusted: true,
        }),
        Ok(_) => None,
        Err(error) => {
            problem.get_or_insert(other(error));
            None
        }
    };
    (resume, problem)
}

/// 删掉停着的 BT 任务下了的文件：以暂停加入会话，再连同文件一起移除（libtorrent 删它建的文件、空了的目录与存不要的
/// 文件的部分块的 `.parts` 文件），之后删掉内容的根 `root` 里操作系统自己建的文件（[`OS_LITTER`]）与空了的目录。根还在
/// （里面有别的东西，或删除出错）时报 [`Error::Torrent`]，写明留下了什么与 libtorrent 报的错。
pub(crate) async fn delete(
    engine: Arc<Engine>,
    info_hash: &str,
    resume: &[u8],
    root: &Path,
) -> Result<(), Error> {
    let id = TorrentId::parse(info_hash)
        .map_err(|error| Error::Corrupt(format!("种子的编号：{error}")))?;
    let save_path = root
        .parent()
        .ok_or_else(|| Error::Corrupt(format!("内容的根没有所在目录：{}", root.display())))?;
    let mut owner = engine.own(&id)?;
    engine.settled(&id).await?;
    let options = AddOptions {
        trust_pieces: true,
        paused: true,
        upload_only: false,
    };
    owner
        .session()
        .add_resume(resume, &id, utf8(save_path)?, options)
        .map_err(owner.libtorrent("加入"))?;
    // 目录里有操作系统自己建的文件时 libtorrent 删不掉目录、报删除失败：那些文件下面一起删，以根还在不在为准
    let deleted = owner.remove(Removal::Files).await;
    drop(owner);
    let swept = root.to_path_buf();
    let left = tokio::task::spawn_blocking(move || sweep(&swept))
        .await
        .expect("清理目录不会 panic，运行时关闭时本任务也一起丢弃");
    if left.is_empty() {
        return Ok(());
    }
    let shown: Vec<String> = left
        .iter()
        .take(5)
        .map(|path| path.display().to_string())
        .collect();
    let more = if left.len() > shown.len() {
        format!(" 等 {} 项", left.len())
    } else {
        String::new()
    };
    let cause = deleted
        .err()
        .map_or_else(String::new, |error| format!("（{error}）"));
    Err(Error::Torrent(format!(
        "留下了 {}{more}{cause}",
        shown.join("、")
    )))
}

/// 删掉 `path`（BT 内容的根）下操作系统自己建的文件（[`OS_LITTER`]）与空了的目录，返回删不掉的（还在的文件、删除出错
/// 的）。`path` 已不在时为空；符号链接不跟进，当作文件。
fn sweep(path: &Path) -> Vec<PathBuf> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        // 不在了；在而读不了属性的，下面删的时候报出来
        return if path.exists() {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    };
    if !meta.is_dir() {
        let litter = path
            .file_name()
            .is_some_and(|name| OS_LITTER.iter().any(|litter| name == *litter));
        return if litter && std::fs::remove_file(path).is_ok() {
            Vec::new()
        } else {
            vec![path.to_path_buf()]
        };
    }
    let mut left = Vec::new();
    match std::fs::read_dir(path) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => left.extend(sweep(&entry.path())),
                    Err(_) => left.push(path.to_path_buf()),
                }
            }
        }
        Err(_) => left.push(path.to_path_buf()),
    }
    if left.is_empty() && std::fs::remove_dir(path).is_err() {
        left.push(path.to_path_buf());
    }
    left
}

/// 诊断在下或做种（`seeding`）的种子 `info_hash`：要一次会话的统计、等它到，再查网络状况、种子的状态与 tracker；上传
/// 限速为 `upload_limit`，经的代理为 `proxy`（去掉凭据的写法与探到的情况，直连为 None）。种子不在会话里（刚开始、还没加进去）、统计 [`STEP_TIMEOUT`] 内没到时报 [`Error::Torrent`]；
/// 等的期间 BT 会话关闭时报 [`Error::Closed`]。
pub(crate) async fn diagnose(
    engine: &Engine,
    info_hash: &str,
    seeding: bool,
    upload_limit: Option<NonZeroU32>,
    proxy: Option<(String, ProxyCheck)>,
) -> Result<Vec<Check>, Error> {
    let id = TorrentId::parse(info_hash)
        .map_err(|error| Error::Corrupt(format!("种子的编号：{error}")))?;
    let absent = |_| {
        Error::Torrent("种子还没加进 BT 会话（刚开始，或在等上次的移除），稍后再诊断".to_owned())
    };
    let status = engine.session.status(&id).map_err(absent)?;
    let trackers = engine.session.trackers(&id).map_err(absent)?;
    let arrived = engine.stats.notified();
    tokio::pin!(arrived);
    arrived.as_mut().enable();
    engine.session.request_stats();
    tokio::time::timeout(STEP_TIMEOUT, arrived)
        .await
        .map_err(|_| Error::Torrent("10 秒内没等到 BT 会话的统计".to_owned()))?;
    if engine.stop.load(Ordering::Relaxed) {
        return Err(Error::Closed);
    }
    let network = engine.session.network();
    let (added, own): (Vec<_>, Vec<_>) = trackers.iter().partition(|tracker| tracker.added);
    let fact = |tracker: &hs_dlx_torrent::TrackerState| Tracker {
        working: tracker.working,
        failed: tracker.failed,
        message: tracker.message.clone(),
        unroutable: tracker.unroutable,
    };
    let public = match &*engine.public.borrow() {
        PublicTrackers::Off => Public::Off,
        PublicTrackers::Idle | PublicTrackers::Fetching => Public::Fetching,
        PublicTrackers::Failed(why) => Public::Failed(why.clone()),
        PublicTrackers::Ready(list)
            if added.is_empty()
                && list
                    .iter()
                    .all(|url| own.iter().any(|tracker| &tracker.url == url)) =>
        {
            Public::Covered
        }
        PublicTrackers::Ready(_) => Public::Added(added.into_iter().map(fact).collect()),
    };
    let count = |value: i32| u32::try_from(value).ok();
    let most =
        |of_status: i32, of_trackers: Option<i32>| count(of_status.max(of_trackers.unwrap_or(-1)));
    let facts = Facts {
        proxy: proxy.map(|(address, check)| ProxyFact {
            address,
            check,
            error: network.proxy_error.clone(),
        }),
        listen_port: network.listen_port,
        listen_error: network.listen_error,
        upnp_port: network.upnp_port,
        upnp_error: network.upnp_error,
        natpmp_port: network.natpmp_port,
        natpmp_error: network.natpmp_error,
        dht_nodes: u64::try_from(network.dht_nodes).ok(),
        incoming: network.incoming,
        private: status.private_torrent,
        seeding,
        peers: u32::try_from(status.peers).expect("libtorrent 的计数不为负"),
        seeds: most(
            status.swarm_seeds,
            trackers.iter().map(|tracker| tracker.seeds).max(),
        ),
        leechers: most(
            status.swarm_leechers,
            trackers.iter().map(|tracker| tracker.leechers).max(),
        ),
        trackers: own.into_iter().map(fact).collect(),
        public,
        upload_limit,
    };
    Ok(checks(&facts))
}

fn utf8(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .ok_or_else(|| Error::InvalidInput(format!("路径不是 UTF-8：{}", path.display())))
}

/// 加锁；持锁期间只读写表，不会 panic，锁中毒即不变量被破坏。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("持锁期间不会 panic")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 记着能用时经代理的 UDP 一次没回不改记，再一次才改；改成写法不同而等价的地址时探到的情况照旧，改成别的才重置。
    #[test]
    fn the_proxy_verdict_waits_for_a_second_silence_and_survives_equivalent_addresses() {
        let url = Url::parse("socks5://127.0.0.1:7897").unwrap();
        let bt = Bt::new(
            TorrentNetwork::Loopback {
                public_trackers: Vec::new(),
            },
            None,
            Some(url.clone()),
        );
        let recorded = |bt: &Bt| bt.proxy().unwrap().1;
        bt.set_proxy_check(
            &url,
            ProxyCheck::Usable,
            Resolved {
                fake: false,
                real: true,
            },
        );
        let silent = ProxyCheck::Silent("没回".into());
        bt.set_proxy_check(
            &url,
            silent.clone(),
            Resolved {
                fake: false,
                real: true,
            },
        );
        assert_eq!(recorded(&bt), ProxyCheck::Usable);
        assert_eq!(bt.proxy_due(&url), Some(true), "存疑时下一轮要再探");
        bt.set_proxy_check(
            &url,
            silent.clone(),
            Resolved {
                fake: false,
                real: true,
            },
        );
        assert_eq!(recorded(&bt), silent);

        // 握手被拒之后要重连，中间探到经它的 UDP 没回也不忘
        bt.set_proxy_check(
            &url,
            ProxyCheck::Refused("认证被拒".into()),
            Resolved {
                fake: false,
                real: true,
            },
        );
        bt.set_proxy_check(
            &url,
            silent.clone(),
            Resolved {
                fake: false,
                real: true,
            },
        );
        assert!(bt.proxy_reconnect.load(Ordering::Relaxed));
        let equivalent = Url::parse("socks5h://127.0.0.1:7897/").unwrap();
        bt.set_proxy(Some(equivalent.clone()));
        assert_eq!(recorded(&bt), silent);
        assert_eq!(bt.proxy_url(), Some(equivalent.clone()));
        bt.set_proxy_check(
            &equivalent,
            ProxyCheck::Usable,
            Resolved {
                fake: false,
                real: true,
            },
        );
        // 没有会话时没得重连，记着的照旧要重连
        assert!(bt.proxy_reconnect.load(Ordering::Relaxed));
        bt.set_proxy(Some(Url::parse("socks5://127.0.0.1:1080").unwrap()));
        assert_eq!(recorded(&bt), ProxyCheck::Unchecked);
        bt.set_proxy(None);
        assert_eq!(bt.proxy(), None);
    }

    #[test]
    fn fingerprints_take_one_digit_per_part() {
        assert_eq!(fingerprint("0.2.0"), "-HD0200-");
        assert_eq!(fingerprint("1.12.3"), "-HD1930-");
    }
}
