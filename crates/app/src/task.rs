//! 任务：用户新建的一次下载或录制，及其在存储里的状态。

use std::num::NonZeroU64;
use std::path::PathBuf;

use hs_m3u8_core::hls::Preference;
use hs_m3u8_core::{OutputOptions, Target, Url};

use crate::Failure;
use crate::preference::StoredPreference;

/// 输出要哪几种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Mp4,
    Hls,
    Both,
}

/// 任务存储里的编号，新建时由 SQLite 分配。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub i64);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "任务 {}", self.0)
    }
}

/// 新建任务时用户给定的：输出路径由服务按它取（见 [`crate::Service::create`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub url: Url,
    /// 额外的请求头
    pub headers: Vec<(String, String)>,
    /// 输出放在哪个目录，须是绝对路径
    pub folder: PathBuf,
    pub kind: DraftKind,
}

/// 新建的是哪种任务，及这种任务特有的选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftKind {
    /// HLS 的点播或直播
    Hls {
        /// 视频所在网页的标题，任务的标题与主名照它取（见 [`crate::task_title`]）；None 时由地址得出
        title: Option<String>,
        /// 选轨偏好；按下标选的存不了（主播放列表变了下标就不是同一条），报 [`crate::Error::InvalidInput`]
        preference: Preference,
        output: OutputKind,
        /// 来源是直播时录制；点播时不起作用
        live: bool,
    },
    /// 单个文件
    File {
        /// 文件名，服务器给的（Content-Disposition）或由重定向之后的地址得出（见 [`crate::file_name`]），服务再整理一遍；
        /// None 时由 [`Draft::url`] 得出
        name: Option<String>,
        expected: Expected,
        /// 每个请求最多取多少字节；None 时不限（见 [`FileSpec::chunk_size`]）
        chunk_size: Option<NonZeroU64>,
    },
    /// BT：[`Draft::url`] 为磁力链接或种子文件的 `file:` 地址，没有请求头
    Torrent {
        /// 种子的续传数据：种子文件转成的（[`crate::torrent_from_file`]）或磁力链接取到的
        /// （[`crate::Service::fetch_metadata`]）
        resume: Vec<u8>,
        /// 要下的文件（[`crate::TorrentFile::index`]），至少一个
        wanted: Vec<i32>,
    },
    /// 云离线：[`Draft::url`] 为磁力链接，没有请求头；新建时提交给网盘（要先登录），网盘下完后取回全部文件
    Cloud { provider: CloudProvider },
    /// 网页视频（yt-dlp 解析）：[`Draft::url`] 为网页地址，请求头只能有 Cookie（解析与下载时带上）
    Web {
        /// 解析出的标题：任务的标题与输出的主名照它取
        title: String,
        /// 解析器与视频编号（见 [`crate::ytdlp::Info`]）
        extractor: String,
        video_id: String,
        /// 选中的画质（[`crate::web::Choice`] 里的）：视频与另配的音频的格式、合并后的容器与扩展名
        video: Option<hs_dlx_ytdlp::FormatKey>,
        audio: Option<hs_dlx_ytdlp::FormatKey>,
        container: crate::web::Container,
        ext: String,
    },
}

/// 新建任务时给定的内容，续传与续录照它重建请求；之后只有请求头可在任务停着时改（[`crate::Service::set_headers`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    /// 列表里显示的名字：HLS 为输出的主名，文件为文件名，BT 为内容的根名，云离线为网盘给的名字（定下根之后为根名），
    /// 网页视频为输出的主名
    pub title: String,
    pub url: Url,
    /// 额外的请求头，原样存储（含凭据时同样明文，存储文件只有本人可读写）
    pub headers: Vec<(String, String)>,
    pub kind: Kind,
}

/// 任务的种类及这种任务特有的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Hls(HlsSpec),
    File(FileSpec),
    Torrent(TorrentSpec),
    Cloud(CloudSpec),
    Web(WebSpec),
}

/// HLS 任务特有的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsSpec {
    pub preference: StoredPreference,
    pub output: OutputSpec,
    /// 来源是直播时录制；点播时不起作用
    pub live: bool,
}

/// 文件任务特有的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    /// 输出：以文件名结尾的绝对路径
    pub path: PathBuf,
    /// 新建前认出地址时服务器报的，第一次下载按它核对是不是同一个文件
    pub expected: Expected,
    /// 每个请求最多取多少字节：对整段请求限速的站点（解析器给出）；None 时不限
    pub chunk_size: Option<NonZeroU64>,
}

/// BT 任务特有的内容；种子的续传数据另存（不随任务读出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentSpec {
    /// 种子的编号：信息哈希的十六进制，40 个小写字母与数字
    pub info_hash: String,
    /// 内容的根：保存目录下种子的文件（单文件的）或目录（多文件的），以文件名结尾的绝对路径；种子里的文件下到它的
    /// 所在目录
    pub root: PathBuf,
}

/// 云离线任务特有的内容；各文件的进度另存（见 [`CloudFile`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudSpec {
    pub provider: CloudProvider,
    /// 建任务时登录的账号的用户编号：网盘上的离线任务与文件只在这个账号下，换了账号要换回来才能继续
    pub user: String,
    /// 网盘上离线任务的编号；提交成了才有（新建时在本机先记下任务，开始运行后才提交）
    pub job: Option<String>,
    /// 提交中：发出提交前网盘上已有的同一链接的离线任务编号；没在提交（还没开始提交或已拿到编号）时为 None。提交结果
    /// 不明时下次按它找之后新建的那个，不重复提交
    pub before: Option<Vec<String>>,
    /// 保存目录（绝对路径），内容的根放在这里
    pub folder: PathBuf,
    /// 内容的根：保存目录下的文件（网盘下到的是单个文件）或目录（文件夹），以文件名结尾的绝对路径。网盘下完、列出
    /// 文件时定下，之前为 None；定下时任务目录（[`Task::work_dir`]）随之定下
    pub root: Option<PathBuf>,
}

/// 网页视频任务特有的内容：每次开始与继续都重新解析网页，按格式编号找到这次的地址（地址会过期、与出口 IP 绑定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSpec {
    /// 解析器的名字，如 `Youtube`
    pub extractor: String,
    /// 视频在这个网站上的编号
    pub video_id: String,
    /// 视频的格式（重新解析后按它认出同一个）；只要音频时为 None
    pub video: Option<hs_dlx_ytdlp::FormatKey>,
    /// 另配的音频（或只要音频时那一路）的格式；视频自带音频时为 None。`video` 与 `audio` 至少有一个
    pub audio: Option<hs_dlx_ytdlp::FormatKey>,
    pub container: crate::web::Container,
    /// 输出：以文件名结尾的绝对路径
    pub path: PathBuf,
}

/// 提供云离线的网盘。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudProvider {
    PikPak,
}

/// 云离线任务里的一个文件：网盘下完、列出文件时记下。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudFile {
    /// 网盘上的文件编号
    pub id: String,
    /// 相对内容的根的路径，以 `/` 分隔、各段都是清理过的文件名；内容的根就是这个文件时为空串
    pub path: String,
    /// 字节数
    pub size: u64,
    /// 网盘给的 GCID（见 [`hs_dlx_cloud::pikpak::gcid`]），40 位小写十六进制
    pub gcid: String,
    /// 已取回、核心装到了输出位置（还没校验）：输出位置上的东西是这个任务的，可以校验、不符时删
    pub fetched: bool,
    /// 已校验过；为 true 时 `fetched` 一定为 true
    pub done: bool,
    /// 定下根之后用户在网盘上删了它（或删进回收站），不取回、不算进总量；为 true 时 `fetched` 一定为 false
    pub skipped: bool,
}

/// 新建前认出地址是文件时服务器报的（响应头原样）。总字节数、ETag、Last-Modified、Content-Type 都没有时第一次下载
/// 无从核对。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    /// 总字节数
    pub length: Option<u64>,
    /// 支持范围请求：第一次下载据此带 If-Range 核对验证器（不支持的服务器一律回整个文件）
    pub accepts_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
}

impl TaskSpec {
    /// 输出占用的路径（以文件名结尾的绝对路径）：HLS 为 MP4 与本地 HLS 目录中要的那些（MP4 在前），文件任务为文件，
    /// BT 与定下了根的云离线任务为内容的根，还没定下根的云离线任务没有。首项是主要的那个：显示的文件名、所在的目录取它。
    pub fn outputs(&self) -> Vec<&std::path::Path> {
        match &self.kind {
            Kind::Hls(hls) => match &hls.output {
                OutputSpec::Mp4(path) | OutputSpec::Hls(path) => vec![path],
                OutputSpec::Both { mp4, hls } => vec![mp4, hls],
            },
            Kind::File(file) => vec![&file.path],
            Kind::Torrent(torrent) => vec![&torrent.root],
            Kind::Cloud(cloud) => cloud.root.iter().map(PathBuf::as_path).collect(),
            Kind::Web(web) => vec![&web.path],
        }
    }

    /// HLS 任务特有的内容；别的任务为 None。
    pub fn hls(&self) -> Option<&HlsSpec> {
        match &self.kind {
            Kind::Hls(hls) => Some(hls),
            Kind::File(_) | Kind::Torrent(_) | Kind::Cloud(_) | Kind::Web(_) => None,
        }
    }

    /// 文件任务特有的内容；别的任务为 None。
    pub fn file(&self) -> Option<&FileSpec> {
        match &self.kind {
            Kind::File(file) => Some(file),
            Kind::Hls(_) | Kind::Torrent(_) | Kind::Cloud(_) | Kind::Web(_) => None,
        }
    }

    /// BT 任务特有的内容；别的任务为 None。
    pub fn torrent(&self) -> Option<&TorrentSpec> {
        match &self.kind {
            Kind::Torrent(torrent) => Some(torrent),
            Kind::Hls(_) | Kind::File(_) | Kind::Cloud(_) | Kind::Web(_) => None,
        }
    }

    /// 云离线任务特有的内容；别的任务为 None。
    pub fn cloud(&self) -> Option<&CloudSpec> {
        match &self.kind {
            Kind::Cloud(cloud) => Some(cloud),
            Kind::Hls(_) | Kind::File(_) | Kind::Torrent(_) | Kind::Web(_) => None,
        }
    }

    /// 网页视频任务特有的内容；别的任务为 None。
    pub fn web(&self) -> Option<&WebSpec> {
        match &self.kind {
            Kind::Web(web) => Some(web),
            Kind::Hls(_) | Kind::File(_) | Kind::Torrent(_) | Kind::Cloud(_) => None,
        }
    }
}

/// 输出：路径都是以文件名结尾的绝对路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSpec {
    Mp4(PathBuf),
    Hls(PathBuf),
    Both { mp4: PathBuf, hls: PathBuf },
}

impl OutputSpec {
    /// 核心的输出选项，任务目录取核心的默认位置（与输出同级的 `<主名>.hsdl`）。新建任务时由此定下任务目录，记在
    /// [`Task::work_dir`]，之后照它指定。
    pub fn options(&self) -> OutputOptions {
        OutputOptions::new(match self.clone() {
            OutputSpec::Mp4(path) => Target::Mp4(path),
            OutputSpec::Hls(path) => Target::Hls(path),
            OutputSpec::Both { mp4, hls } => Target::Both { mp4, hls },
        })
    }
}

/// 任务在存储里的状态。运行中的细节（进度、阶段）不存，见 [`crate::Event`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// 在下载或录制
    Running,
    /// 用户暂停；任务目录保留，继续时续传。应用异常退出时还在运行的，重开后也是这个状态
    Paused,
    /// 以错误结束；任务目录保留
    Failed(Failure),
    /// 已写出输出
    Done,
    /// 放弃了而没做完（中途出错、超时或应用退出）：任务目录可能已删了一部分，不能继续，只能再放弃
    DiscardUnfinished,
}

/// 存储里的一个任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub spec: TaskSpec,
    pub status: Status,
    /// 任务目录（绝对路径），定下后不变。HLS 与文件任务新建时定下，为 None 时没有自己的任务目录：旧版本存储里已完成
    /// 的，或与别的任务共用任务目录、迁移时让给了那个任务的（状态为失败），这样的任务不能继续，只能从列表移除。BT 任务
    /// 没有任务目录（内容直接下到保存目录，续传数据存在存储里），总是 None。云离线任务与内容的根
    /// （[`CloudSpec::root`]）一起定下，之前为 None；里面每个文件一个核心的任务目录
    pub work_dir: Option<PathBuf>,
    /// 新建时间，Unix 秒
    pub created_at: i64,
    /// HLS 点播的总片数（各轨合计），第一次运行拿到播放列表后记下，用于任务没在运行时显示进度；直播、文件任务与还没
    /// 拿到时为 None
    pub total: Option<usize>,
    /// 完成时写出的结果；没完成时为 None
    pub result: Option<Finished>,
    /// 最近一次运行是只写出已录的（[`crate::Service::write_recorded`] 置上，[`crate::Service::resume`] 清掉）：用户选了
    /// 不再续录；文件任务总是 false
    pub write_only: bool,
    /// BT 任务在做种，或下次打开应用要接着做（只在已完成时）；别的任务总是 false
    pub seeding: bool,
}

/// 完成时写出的结果，供之后显示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// MP4 里视频流的时长，没有 MP4 或没有视频流时为第 0 条轨分片的声明时长之和；都没有时与文件任务为 None
    pub duration: Option<std::time::Duration>,
    /// HLS 为写出的分片与 init 段的字节数（解密后），文件任务为文件的字节数，BT 为选了的文件的字节数
    pub bytes: u64,
    /// MP4 里视频流的宽与高；没有时与文件任务为 None
    pub resolution: Option<(u32, u32)>,
}

impl From<&hs_m3u8_core::FileInfo> for Expected {
    fn from(info: &hs_m3u8_core::FileInfo) -> Self {
        Expected {
            length: info.length,
            accepts_ranges: info.accepts_ranges,
            etag: info.etag.clone(),
            last_modified: info.last_modified.clone(),
            content_type: info.content_type.clone(),
        }
    }
}

impl From<&Expected> for hs_m3u8_core::Expected {
    fn from(expected: &Expected) -> Self {
        hs_m3u8_core::Expected {
            length: expected.length,
            accepts_ranges: expected.accepts_ranges,
            etag: expected.etag.clone(),
            last_modified: expected.last_modified.clone(),
            content_type: expected.content_type.clone(),
        }
    }
}
