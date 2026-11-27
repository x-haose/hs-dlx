//! 网页宿主的端口：服务在准备任务时要在网页里找视频流，网页由宿主开（桌面应用用内置网页实现），服务只经这里的
//! 类型与它打交道。同一条流的判定（[`Found`]）、配请求头并探测（[`Handle::check_stream`](crate::Handle::check_stream)）
//! 是不依赖网页引擎的计算，也在这里。
//!
//! 分工：宿主管与网页引擎打交道的事（注入脚本、收消息并限量、按第一个顶层文档定论、暂停与静音、摆放与显示），服务
//! 管流程（同时开几个、何时采用找到的流、何时让宿主把网页显示出来）。

mod check;
mod found;
mod request;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use hs_m3u8_core::{Proxy, Url};
use tokio::sync::mpsc;

pub use self::check::COOKIE_TIMEOUT;
pub(crate) use self::check::{check, document_headers};
pub use self::found::{Check, Found};
pub use self::request::{Cookie, Stream, cookie_header};
use crate::{PageAsk, TaskId};

/// 能开网页的一方。没有注册（[`Handle::set_page_host`](crate::Handle::set_page_host)）时，要到网页里找流的任务以失败
/// 结束。
pub trait PageHost: Send + Sync {
    /// 为任务 `task` 开一个网页并加载 `url`，出口照 `proxy`；网页在后台、不显示、不出声。返回的会话释放即关掉网页。
    /// 不阻塞：网页引擎要在别的线程上建时，建不出的原因经 [`PageEvent::Failed`] 报来。
    fn open(&self, task: TaskId, url: &Url, proxy: &Proxy) -> Result<PageSession, Unopened>;
}

/// 网页没开成。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unopened {
    /// 宿主此刻开不了网页（桌面应用的主窗口关着）：服务不再让任务去开，等宿主说又能开了
    /// （[`Handle::set_page_host_ready`](crate::Handle::set_page_host_ready)）
    Away,
    /// 建不出网页：原因
    Failed(String),
}

/// 宿主里开着的一个网页，由宿主实现；释放即关掉网页。各方法可以在任何线程上调，网页已不在时为原因。
pub trait Page: Send {
    /// 把网页显示给用户，`reason` 为要用户在里面做什么。
    fn show(&self, reason: PageAsk) -> Result<(), String>;

    /// 收起网页，回到后台。
    fn hide(&self) -> Result<(), String>;

    /// 网页引擎里会随请求发往 `url` 的 Cookie；引擎一直不回时一直不完成，时限由调用方定（[`COOKIE_TIMEOUT`]）。
    fn cookies(
        &self,
        url: &Url,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Cookie>, String>> + Send>>;
}

/// 为一个任务开着的网页：宿主报来的事与对它的操作。
pub struct PageSession {
    /// 宿主报来的事，按发生的先后。页面加载、定论与故障不丢；网页能任意多发的（找到的流、标题）由宿主限量。发送端
    /// 没发 [`PageEvent::Closed`] 就释放的，服务按宿主出了故障处理（任务失败）
    pub events: mpsc::UnboundedReceiver<PageEvent>,
    pub page: Box<dyn Page>,
}

/// 宿主报来的事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageEvent {
    /// 打开的地址是不是网页有了结论：只报一次，之后的换页不改
    Verdict(Verdict),
    /// 当前的顶层文档是不是整页的人机验证；每个顶层文档就绪时报一次
    Challenge(bool),
    /// 找到一条流
    Stream(Stream),
    /// 当前顶层文档的标题（宿主已截短）
    Title(String),
    /// 顶层开始显示这个地址（含打开时的第一页）：之前找到的流属于上一个页面，作废
    Navigated(Url),
    /// 这个网页用不了了：建不出、顶层导航没成、网页进程退出；原因
    Failed(String),
    /// 宿主关掉了它而不是出了故障（主窗口关了）：宿主此刻开不了网页，同 [`Unopened::Away`]
    Closed,
}

/// 打开的地址是不是网页：宿主按网页引擎打开它后的第一个顶层文档判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 网页：网页引擎拿到的是 2xx，或是整页的人机验证（要用户在网页里过了才看得到内容）
    Page,
    /// 网页引擎打开它也是 HTTP 出错的页面（防盗链、过期的地址之类的拒绝页、找不到的页）：状态码
    Refused(u16),
    /// 不是网页：网页引擎认出的类型（如 `video/mp4`）；文档加载完也没报类型（脚本没在里面跑，如 PDF）时为 None
    NotPage(Option<String>),
    /// 按网页类型发来、正文是 HLS 播放列表（如 PHP 的默认类型）
    Playlist,
    /// 打开的地址本身是要下载的文件
    Download,
    /// 网页类型的文档加载完了，网页引擎与页面都没报它的 HTTP 状态码：判断不了是不是出错的页面
    Unknown,
}

/// 在网页里找流的几个时长。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagePace {
    /// 打开网页后最多等多久有是不是网页的结论，到时按打不开算
    pub decide_within: Duration,
    /// 第一条探测成功的流出现后再等多久：这期间没有第二条探测成功的即采用它，有就让用户选
    pub adopt_after: Duration,
    /// 定论为网页（或换了页、过了人机验证）之后多久还没有探测成功的流，就把网页显示出来等用户点播放
    pub play_after: Duration,
}

impl Default for PagePace {
    fn default() -> Self {
        PagePace {
            decide_within: Duration::from_secs(30),
            adopt_after: Duration::from_secs(3),
            play_after: Duration::from_secs(12),
        }
    }
}
