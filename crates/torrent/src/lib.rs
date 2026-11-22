//! BT 引擎：静态链接的 libtorrent-rasterbar（2.1）上的一层很薄的绑定，只公开桌面应用要用的会话操作。C++ 的那一层在
//! src/shim.cpp，库由 third_party/libtorrent/build.sh 编出。
//!
//! 种子用加入时定下的编号（[`TorrentId`]）指认。一个种子的全部内容（种子信息、tracker、文件改名、选了哪些文件、已有
//! 哪些块）都在它的续传数据里：种子文件先转成续传数据（[`resume_from_torrent_file`]），磁力链接先加进会话取到种子
//! 信息、再要它的续传数据；之后定下内容放哪、下哪些（[`place`]），由 [`Session::add_resume`] 加入。排队与开停由应用
//! 决定，libtorrent 不自动开停。会话的操作都交给 libtorrent 的网络线程去做，可以在任意线程
//! 上调用；事件（[`Alert`]）由 [`Session::pop_alerts`] 取出，取出时已转成自有的数据。libtorrent 给的文字（种子名、
//! 文件路径、说明）不是 UTF-8 的字节换成了替换字符。

use std::fmt;

#[cxx::bridge(namespace = "hsdlx")]
mod ffi {
    /// 会话的设置。
    struct SessionConfig {
        /// 监听的地址与端口，libtorrent 的写法（如 `0.0.0.0:6881,[::]:6881`；端口为 0 时由系统挑）
        listen_interfaces: String,
        dht: bool,
        /// DHT 的引导节点，libtorrent 的写法（`主机:端口` 以逗号相连）；空时用 libtorrent 默认的。只在 DHT 启动时解析
        /// 一次
        dht_bootstrap_nodes: String,
        upnp: bool,
        natpmp: bool,
        /// 本地网络里的节点发现
        lsd: bool,
        /// 连 tracker 与 web seed 时的 User-Agent
        user_agent: String,
        /// 节点 ID 的前缀，标明客户端（如 `-HD0100-`）
        peer_fingerprint: String,
        /// 整个会话的上传限速，字节每秒；0 为不限
        upload_limit: i32,
        /// 节点连接、tracker、DHT 与 uTP 经的 SOCKS5 代理；`host` 为空时直连
        proxy: SocksProxy,
        /// 上次 [`Session::session_state`] 写出的 DHT 状态（路由表里的节点），启动时载入就不必从头找节点；空时从头找
        state: Vec<u8>,
    }

    /// 一个种子此刻的状态。
    struct Status {
        /// libtorrent 的 `torrent_status::state_t`，见 [`crate::State`]
        state: u8,
        /// 这一状态做到哪，百万分之几：校验时为校验了的，下载时为已下的
        progress_ppm: i32,
        /// 已取得种子信息（磁力链接要先从节点那里取）
        has_metadata: bool,
        paused: bool,
        /// 只上传、不下载（开始之前对方是做种的会断开，`peers` 与 `reachable_pieces` 因此偏少）：加入后还没 [`Session::start`]，或写文件出错（磁盘满、没权限、只读、文件太大）时 libtorrent
        /// 改成了这样（同时报 [`AlertKind::FileError`]）；处理好后 [`Session::start`] 继续下
        held: bool,
        /// 种子名；不是 UTF-8 的字节换成了替换字符，只用于显示
        name: String,
        /// 要下载的字节数（没选的文件不算）；没有种子信息时为 0
        total_wanted: i64,
        /// 其中已下完并校验过的；可能还在内存里没写到磁盘（libtorrent 先校验、再异步写盘），要确认已落盘用
        /// [`Session::flush`]
        total_wanted_done: i64,
        /// 这次加入会话以来收到的内容字节数（含校验没过、丢掉的）
        downloaded: i64,
        /// 累计上传的内容字节数，含之前各次加入时的（随续传数据存下）
        all_time_upload: i64,
        /// 选了的文件都有了之后累计在会话里（没暂停）的秒数，含之前各次加入时的（随续传数据存下）；只下了一部分文件的也
        /// 算（libtorrent 的 seeding_duration 只算全部文件都有的）
        finished_seconds: i64,
        /// 选了的文件都有了
        finished: bool,
        /// 私有种子（只经种子里的 tracker 找节点）；没有种子信息时为否
        private_torrent: bool,
        /// tracker 报的整个种子的做种与在下的人数；-1 为没报
        swarm_seeds: i32,
        swarm_leechers: i32,
        /// 下载与上传的速度，字节每秒，只算内容
        download_rate: i32,
        upload_rate: i32,
        /// 连着的节点数，其中的做种者
        peers: i32,
        seeds: i32,
        /// 要下的块数（选了的文件所在的块）；没有种子信息时为 0
        pieces: i32,
        /// 其中能凑齐的：自己已有的，或在线的节点里至少有一个有的
        reachable_pieces: i32,
        /// 种子出错的原因；没出错时为空
        error: String,
    }

    /// 事件的种类。
    enum AlertKind {
        Added,
        /// 加入失败：同步的加入已经报了错，这条只是 libtorrent 也发了一次，`torrent` 为空，可以不理
        AddFailed,
        /// 磁力链接取到了种子信息
        Metadata,
        /// 选了的文件都下完了
        Finished,
        /// [`Session::save_resume_data`] 的结果：`data` 为续传数据
        ResumeData,
        ResumeDataFailed,
        /// 续传数据与磁盘上的文件对不上，改为从头校验
        ResumeRejected,
        /// [`Session::flush`] 的结果，每次调用一条：调用那一刻已下的数据都已交给磁盘写完。写盘出错时这条照样会来，出错
        /// 的在它之前以 [`AlertKind::FileError`] 报来
        CacheFlushed,
        /// 种子本身出错（种子信息不对、内存不够等）；读写文件出的错报为 [`AlertKind::FileError`]
        TorrentError,
        /// 读写文件出错（磁盘满、没权限、只读、文件太大）
        FileError,
        /// 从会话里去掉了；要删文件时另有 [`AlertKind::Deleted`] 或 [`AlertKind::DeleteFailed`]，与这条先后不定
        /// （libtorrent 2.1 先发删的结果）
        Removed,
        Deleted,
        DeleteFailed,
        /// 事件来得太快、队列满了丢了一些：哪些种子的说不清（libtorrent 只给出丢了哪几类，这里没转出来），可能有续传
        /// 数据、写完缓存、移除的结果，在等的那一步可能再也等不到。已移除完的种子不再算移除中（[`Session::removing`]），
        /// 移除还没完的照常等它的事件
        AlertsDropped,
        ListenSucceeded,
        ListenFailed,
        PortMapped,
        PortMapFailed,
        /// 种子自己的 tracker 回了错误或连不上：`message` 里有 tracker 的地址与原因（英文）。某个监听地址路由不到它（如
        /// 回环地址）而跳过的不算；每个监听地址都路由不到时才报，原因为中文的说明
        TrackerError,
        /// 补的 tracker（[`Session::add_trackers`]）回了错误或连不上，同上
        AddedTrackerError,
        /// [`Session::request_stats`] 要的统计到了，[`Session::network`] 里已是新的
        Stats,
    }

    /// 会话的网络状况：监听、端口映射、DHT 与有没有节点从外面连进来。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct NetworkStatus {
        /// 在监听的端口；0 为没在监听
        listen_port: u16,
        /// 最近一次监听失败的说明（英文）；没失败过为空
        listen_error: String,
        /// UPnP 映射到的外部端口；0 为没映射成
        upnp_port: u16,
        /// UPnP 最近一次映射失败的说明（英文）；没失败过为空
        upnp_error: String,
        /// NAT-PMP（或 PCP）映射到的外部端口与最近一次失败的说明，同上
        natpmp_port: u16,
        natpmp_error: String,
        /// DHT 路由表里的节点数；-1 为还没统计过（[`Session::request_stats`]）
        dht_nodes: i64,
        /// 有过节点从外面连进来（统计过才知道）
        incoming: bool,
        /// 经代理的 UDP（DHT、uTP、UDP 的 tracker）最近一次连代理或握手失败的说明（英文）；没失败过、改过代理或重连过
        /// （[`Session::reconnect_proxy`]）之后为空。代理拒绝转发 UDP、认证被拒、回了 IPv6 地址时 libtorrent 不报这个，
        /// UDP 就此不通
        proxy_error: String,
    }

    /// 种子的一个 tracker 此刻的状况。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TrackerState {
        url: String,
        /// 最近一次通告成功了
        working: bool,
        /// 正在通告
        updating: bool,
        /// 连不上：每个监听地址的通告都失败过，且没有成功的
        failed: bool,
        /// tracker 回的错误或警告，或连不上的原因（英文，取第一个不是被跳过的监听地址的）；每个监听地址都路由不到它时
        /// 为中文的说明；没有时为空
        message: String,
        /// tracker 报的做种与在下的人数；-1 为没报
        seeds: i32,
        leechers: i32,
        /// 是补的（[`Session::add_trackers`]），不是种子自己的
        added: bool,
    }

    /// SOCKS5 代理（RFC 1928）：要转发 UDP（UDP ASSOCIATE）且回 IPv4 的转发地址，DHT、uTP 与 UDP 的 tracker 才经得了
    /// 它（libtorrent 经代理时只开一个 IPv4 的接口）。经它时不接受外部连入、不映射端口、不做本地发现；节点与 tracker
    /// 的主机名交给代理解析，DHT 引导节点（dht.libtorrent.org）的除外，照样在本机解析。
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    struct SocksProxy {
        /// 代理的主机名或 IP；空为不经代理
        host: String,
        port: u16,
        /// 用户名与密码（RFC 1929）；用户名为空时不认证
        username: String,
        password: String,
    }

    /// 建种子文件的选择（[`crate::create_torrent`]）。
    struct CreateOptions {
        /// 分块大小，字节；0 为按总大小自动选
        piece_size: i32,
        /// 只有 v1：文件首尾相接，一块可以跨两个文件（网上多数种子是这样）；否时建 v1 与 v2 兼有的（各文件从新的一块
        /// 开始，中间是对齐用的填充文件）
        v1_only: bool,
        /// 私有种子：只经种子里的 tracker 找节点
        private_torrent: bool,
        /// 种子里的 tracker 的地址；空时没有
        tracker: String,
    }

    /// 种子里的一个文件（不含对齐用的填充文件）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FileEntry {
        /// 在种子里的序号，选文件时用它
        index: i32,
        /// 相对保存目录的路径（多文件的种子以根名的目录开头），用本平台的分隔符；不是 UTF-8 的字节换成了替换字符，
        /// 只用于显示，选文件用 `index`
        path: String,
        /// 字节数
        size: i64,
        /// 要下载（续传数据里的文件优先级不为「不下载」，没记优先级时都要）
        wanted: bool,
    }

    /// 从会话里去掉种子时连带删什么。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Removal {
        /// 不删
        Keep,
        /// 删下了的文件、空了的目录与 `.parts` 文件（[`crate::Metadata::part_file`]）
        Files,
    }

    /// 由续传数据加入种子时的选择。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct AddOptions {
        /// 信续传数据里记的已有的块。否时去掉这些记录：磁盘上有这个种子的文件时 libtorrent 按块整体校验，用于续传数据
        /// 可能比磁盘上的多的时候（上次没正常停下）
        trust_pieces: bool,
        /// 加入后暂停：不连节点、不下载
        paused: bool,
        /// 只上传、不下载（做种）：缺了块也不补，[`Status::finished`] 为否即文件不全
        upload_only: bool,
    }

    /// 续传数据里的种子信息。
    struct Described {
        /// [`crate::TorrentId`] 的写法
        id: String,
        /// 种子名：单文件的为文件名，多文件的为顶层目录名；不是 UTF-8 的字节换成了替换字符
        name: String,
        /// 私有种子：只经种子里的 tracker 找节点，libtorrent 不用 DHT、节点交换与本地发现
        private_torrent: bool,
        files: Vec<FileEntry>,
        /// 续传数据里记的已有的块落在要下的文件里的字节数
        done: i64,
        /// 存不要的文件落在要下的块里的部分的文件名（在保存目录里）
        part_file: String,
    }

    /// 一个事件。
    struct Alert {
        kind: AlertKind,
        /// 关于哪个种子（[`crate::TorrentId`] 的写法）；与种子无关的为空
        torrent: String,
        /// libtorrent 的说明，英文
        message: String,
        /// 续传数据（[`AlertKind::ResumeData`]）；别的为空
        data: Vec<u8>,
    }

    unsafe extern "C++" {
        include!("hs-dlx-torrent/src/shim.h");

        type Session;

        fn new_session(config: &SessionConfig) -> Result<UniquePtr<Session>>;
        fn add_magnet(self: &Session, uri: &str, save_path: &str) -> Result<String>;
        fn add_resume(
            self: &Session,
            resume: &[u8],
            id: &str,
            save_path: &str,
            options: &AddOptions,
        ) -> Result<String>;
        fn removing(self: &Session, id: &str) -> bool;
        fn start(self: &Session, id: &str) -> Result<()>;
        fn flush(self: &Session, id: &str) -> Result<()>;
        fn session_state(self: &Session) -> Result<Vec<u8>>;
        fn status(self: &Session, id: &str) -> Result<Status>;
        fn connect_peer(self: &Session, id: &str, ip: &str, port: u16) -> Result<()>;
        fn pause(self: &Session, id: &str) -> Result<()>;
        fn resume(self: &Session, id: &str) -> Result<()>;
        fn remove(self: &Session, id: &str, removal: Removal) -> Result<()>;
        fn save_resume_data(self: &Session, id: &str) -> Result<()>;
        fn listen_port(self: &Session) -> u16;
        fn set_upload_limit(self: &Session, bytes_per_second: i32);
        fn set_proxy(self: &Session, proxy: &SocksProxy);
        fn reconnect_proxy(self: &Session, proxy: &SocksProxy);
        fn network(self: &Session) -> NetworkStatus;
        fn request_stats(self: &Session);
        fn trackers(self: &Session, id: &str) -> Result<Vec<TrackerState>>;
        fn add_trackers(self: &Session, id: &str, urls: &[String]) -> Result<u32>;
        fn pop_alerts(self: &Session, wait_ms: u32) -> Result<Vec<Alert>>;
        fn create_torrent(path: &str, options: &CreateOptions) -> Result<Vec<u8>>;
        fn resume_from_torrent_file(torrent: &[u8]) -> Result<Vec<u8>>;
        fn magnet_id(uri: &str) -> Result<String>;
        fn describe(resume: &[u8]) -> Result<Described>;
        fn place(resume: &[u8], root: &str, wanted: &[i32]) -> Result<Vec<u8>>;
    }
}

pub use ffi::{
    AddOptions, Alert, AlertKind, CreateOptions, FileEntry, NetworkStatus, Removal, SessionConfig,
    SocksProxy, Status, TrackerState,
};

// SAFETY: libtorrent 的会话把每个操作作为消息交给它自己的网络线程执行（docs/hacking.rst：「manipulates it by sending
// messages to the network thread」）；C++ 那一层自己的状态（种子表、取事件）各有一把锁。
unsafe impl Send for ffi::Session {}
// SAFETY: 同上，`&Session` 上的操作在多个线程上同时调用也只是各发各的消息、各自加锁。
unsafe impl Sync for ffi::Session {}

/// 种子的编号：加入时的信息哈希的十六进制串，有 v1 的（SHA-1）时为它，只有 v2 时为截到 20 字节的 SHA-256。取到种子
/// 信息后知道了另一种哈希也不变；存起来，从续传数据恢复时交回（[`Session::add_resume`]）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TorrentId(String);

impl TorrentId {
    /// 存起来的编号：40 个十六进制数字。
    pub fn parse(text: &str) -> Result<TorrentId, Error> {
        if text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(TorrentId(text.to_ascii_lowercase()))
        } else {
            Err(Error(format!("不是种子的编号：{text}")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TorrentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// libtorrent 报的错误（C++ 抛出的异常），说明为英文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<cxx::Exception> for Error {
    fn from(exception: cxx::Exception) -> Self {
        Error(exception.what().to_owned())
    }
}

/// 种子在做什么（libtorrent 的 `torrent_status::state_t`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 校验已有的文件
    CheckingFiles,
    /// 磁力链接在向节点取种子信息
    DownloadingMetadata,
    Downloading,
    /// 选了的文件下完了，没做种
    Finished,
    Seeding,
    /// 校验续传数据
    CheckingResumeData,
}

impl Status {
    /// libtorrent 报的状态；它新加了别的取值时为 None。
    pub fn state(&self) -> Option<State> {
        Some(match self.state {
            1 => State::CheckingFiles,
            2 => State::DownloadingMetadata,
            3 => State::Downloading,
            4 => State::Finished,
            5 => State::Seeding,
            7 => State::CheckingResumeData,
            _ => return None,
        })
    }
}

/// 一个 BT 会话：监听端口、连节点、管理加进来的种子。释放会阻塞：等 libtorrent 关好会话，其中向 tracker 报告停止最多
/// 等 2 秒，此外还有写完缓存在内存里的数据、关闭文件：界面还要响应时不在界面线程上释放（退出时等它写完是要的）。
pub struct Session(cxx::UniquePtr<ffi::Session>);

impl Session {
    pub fn new(config: &SessionConfig) -> Result<Session, Error> {
        Ok(Session(ffi::new_session(config)?))
    }

    fn inner(&self) -> &ffi::Session {
        self.0.as_ref().expect("new_session 成功时返回的指针不为空")
    }

    /// 加磁力链接，只取种子信息、不下内容（只上传，与做种的一方之后互相断开）：取到后（[`AlertKind::Metadata`]）要它的
    /// 续传数据、把它移除，再照新建任务的选择由 [`Session::add_resume`] 加入。`save_path` 为续传数据里记的保存目录，
    /// 加入时另给。已在会话里、或还在移除中（[`Session::remove`] 之后、它移除完的事件之前，见 [`Session::removing`]）
    /// 时报错。
    pub fn add_magnet(&self, uri: &str, save_path: &str) -> Result<TorrentId, Error> {
        Ok(TorrentId(self.inner().add_magnet(uri, save_path)?))
    }

    pub fn status(&self, id: &TorrentId) -> Result<Status, Error> {
        Ok(self.inner().status(id.as_str())?)
    }

    /// 由续传数据加入编号为 `id` 的种子，下到 `save_path` 下（种子的文件与目录建在它里面）。`options.trust_pieces` 时已
    /// 有的块不重下（与磁盘上的文件对不上时报 [`AlertKind::ResumeRejected`]，改为整体校验）；写盘出错时记下的只上传
    /// 去掉。续传数据不是这个种子的、没有种子信息也不是磁力链接的、已在会话里、或它还在移除中时报错。
    pub fn add_resume(
        &self,
        resume: &[u8],
        id: &TorrentId,
        save_path: &str,
        options: AddOptions,
    ) -> Result<(), Error> {
        self.inner()
            .add_resume(resume, id.as_str(), save_path, &options)?;
        Ok(())
    }

    /// 种子 `id` 移除了、还没移除完（移除的事件，要删文件时还有删的结果，都转出了才算完）：其间再加入它报错。
    pub fn removing(&self, id: &TorrentId) -> bool {
        self.inner().removing(id.as_str())
    }

    /// 把缓存里的数据写到磁盘并关上文件，结果以 [`AlertKind::CacheFlushed`] 报来：那时调用这一刻已下的数据都已写过（其间
    /// 没有这个种子的 [`AlertKind::FileError`] 即都写成了）；还在下的种子之后下的不在其内。之后移除种子不影响这条报来。
    /// 没有这个种子、或还没有种子信息时报错（没有要写的，也不会有这个事件）。
    pub fn flush(&self, id: &TorrentId) -> Result<(), Error> {
        Ok(self.inner().flush(id.as_str())?)
    }

    /// 写文件出错（[`AlertKind::FileError`]）改成只上传后，处理好了继续下。
    pub fn start(&self, id: &TorrentId) -> Result<(), Error> {
        Ok(self.inner().start(id.as_str())?)
    }

    /// DHT 的状态（路由表里的节点），存起来下次建会话时放进 [`SessionConfig::state`]。
    pub fn session_state(&self) -> Result<Vec<u8>, Error> {
        Ok(self.inner().session_state()?)
    }

    /// 主动连一个节点（`ip` 为 IPv4 或 IPv6 的写法）。
    pub fn connect_peer(&self, id: &TorrentId, ip: &str, port: u16) -> Result<(), Error> {
        Ok(self.inner().connect_peer(id.as_str(), ip, port)?)
    }

    /// 暂停：在途的块下完再停。
    pub fn pause(&self, id: &TorrentId) -> Result<(), Error> {
        Ok(self.inner().pause(id.as_str())?)
    }

    pub fn resume(&self, id: &TorrentId) -> Result<(), Error> {
        Ok(self.inner().resume(id.as_str())?)
    }

    /// 从会话里去掉，照 `removal` 连带删文件。结果见 [`AlertKind::Removed`]；删了文件的另有 [`AlertKind::Deleted`] 或
    /// [`AlertKind::DeleteFailed`]。
    pub fn remove(&self, id: &TorrentId, removal: Removal) -> Result<(), Error> {
        Ok(self.inner().remove(id.as_str(), removal)?)
    }

    /// 要求写出续传数据，结果以 [`AlertKind::ResumeData`] 或 [`AlertKind::ResumeDataFailed`] 报来。续传数据只记已校验并
    /// 交给磁盘写完的块（torrent.cpp 的 write_resume_data 按 is_piece_flushed）；还没校验完已有的文件时一块也不记（在
    /// 校验中的记到校验到的那块）。
    pub fn save_resume_data(&self, id: &TorrentId) -> Result<(), Error> {
        Ok(self.inner().save_resume_data(id.as_str())?)
    }

    /// 改整个会话的上传限速，字节每秒；0 为不限。
    pub fn set_upload_limit(&self, bytes_per_second: i32) {
        self.inner().set_upload_limit(bytes_per_second);
    }

    /// 改经的代理（见 [`SocksProxy`]；`host` 为空时改回直连）：libtorrent 随即重开监听，之后的连接照它，已连着的不断；
    /// 清掉记着的代理出错（[`NetworkStatus::proxy_error`]）。
    pub fn set_proxy(&self, proxy: &SocksProxy) {
        self.inner().set_proxy(proxy);
    }

    /// 让 libtorrent 重新连一次 SOCKS5 代理 `proxy`（与现在设的相同），并清掉记着的代理出错
    /// （[`NetworkStatus::proxy_error`]）。libtorrent 在代理握手、认证或转发 UDP 被拒后不再自己重试，代理修好之后要这样
    /// 才用得上；其间不会直连。
    pub fn reconnect_proxy(&self, proxy: &SocksProxy) {
        self.inner().reconnect_proxy(proxy);
    }

    /// 网络状况（监听与端口映射随事件更新，DHT 与连入要 [`Session::request_stats`] 之后才新）。
    pub fn network(&self) -> NetworkStatus {
        self.inner().network()
    }

    /// 要一次会话的统计（DHT 的节点数、有没有节点连进来），到了以 [`AlertKind::Stats`] 报来。
    pub fn request_stats(&self) {
        self.inner().request_stats();
    }

    /// 种子的各 tracker 此刻的状况；没有这个种子时报错。
    pub fn trackers(&self, id: &TorrentId) -> Result<Vec<TrackerState>, Error> {
        Ok(self.inner().trackers(id.as_str())?)
    }

    /// 给种子 `id` 补上 `urls` 里它还没有的 tracker（放在已有的之后，各占一层），返回补了几个；没有这个种子时报错。补的
    /// 只在这次加入期间有效：写出的续传数据（[`AlertKind::ResumeData`]）里没有它们，再加入时要再补；它们报的错为
    /// [`AlertKind::AddedTrackerError`]。会话对每层 tracker 各通告一个（不止最前面能用的那层），所以种子自己的
    /// tracker 能用时补的照样各自通告。
    pub fn add_trackers(&self, id: &TorrentId, urls: &[String]) -> Result<u32, Error> {
        Ok(self.inner().add_trackers(id.as_str(), urls)?)
    }

    /// 实际监听的端口；还没监听成功时为 0。
    pub fn listen_port(&self) -> u16 {
        self.inner().listen_port()
    }

    /// 取出积攒的事件；没有时最多等 `wait_ms` 毫秒。报错只在内存不够、或事件里的种子不在登记里（本库的不变量被破坏）
    /// 时：这一批事件已从 libtorrent 取走、丢了，会话里的状态不可信，要重建会话。
    pub fn pop_alerts(&self, wait_ms: u32) -> Result<Vec<Alert>, Error> {
        Ok(self.inner().pop_alerts(wait_ms)?)
    }
}

/// 磁力链接 `uri` 加入会话时会定下的编号（见 [`Session::add_magnet`]）；不是磁力链接时报错。
pub fn magnet_id(uri: &str) -> Result<TorrentId, Error> {
    Ok(TorrentId(ffi::magnet_id(uri)?))
}

/// 种子文件（bencode 写的内容）转成续传数据：什么也没下过，保存目录为空。
pub fn resume_from_torrent_file(torrent: &[u8]) -> Result<Vec<u8>, Error> {
    Ok(ffi::resume_from_torrent_file(torrent)?)
}

/// 续传数据里的种子信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub id: TorrentId,
    /// 种子名：单文件的为文件名，多文件的为顶层目录名；不是 UTF-8 的字节换成了替换字符
    pub name: String,
    /// 私有种子：只经种子里的 tracker 找节点，libtorrent 不用 DHT、节点交换与本地发现
    pub private: bool,
    /// 各文件，路径照续传数据里的改名
    pub files: Vec<FileEntry>,
    /// 续传数据里记的已有的块落在要下的文件里的字节数
    pub done: u64,
    /// `.parts` 文件的名字（在保存目录里）：没选的文件与选了的共用的块里没选的那部分存在这里，做种时读得到它们才凑得出
    /// 整块；只在有这样的块时才有这个文件
    pub part_file: String,
}

/// 读出续传数据里的种子信息；没有种子信息（磁力链接还没取到）或读不出时报错。
pub fn describe(resume: &[u8]) -> Result<Metadata, Error> {
    let described = ffi::describe(resume)?;
    Ok(Metadata {
        id: TorrentId(described.id),
        name: described.name,
        private: described.private_torrent,
        files: described.files,
        done: u64::try_from(described.done).expect("字节数由各块落在文件里的长度相加，不为负"),
        part_file: described.part_file,
    })
}

/// 新建任务前定下内容放哪、下哪些：根名（单文件的文件名、多文件的顶层目录名）改成 `root`（一个路径段），只下序号在
/// `wanted` 里的文件（[`FileEntry::index`]），去掉只上传、暂停与做种模式；返回改后的续传数据。`root` 不是一个路径段、
/// 序号不是种子里的文件、或续传数据里没有种子信息时报错。
pub fn place(resume: &[u8], root: &str, wanted: &[i32]) -> Result<Vec<u8>, Error> {
    Ok(ffi::place(resume, root, wanted)?)
}

/// 照 `options` 为 `path`（文件或目录）建种子文件，返回 bencode 写出的内容。
pub fn create_torrent(path: &str, options: &CreateOptions) -> Result<Vec<u8>, Error> {
    Ok(ffi::create_torrent(path, options)?)
}

impl fmt::Debug for Alert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Alert")
            .field("kind", &self.kind)
            .field("torrent", &self.torrent)
            .field("message", &self.message)
            .field("data", &self.data.len())
            .finish()
    }
}

impl fmt::Debug for AlertKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AlertKind({})", self.repr)
    }
}
