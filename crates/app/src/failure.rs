//! 任务失败的种类：把核心的错误与网页视频的解析错误翻译成界面要分开处理的几类，随失败的状态存进存储。

use hs_dlx_ytdlp::ExtractError;
use hs_m3u8_core::remux::Error as RemuxError;
use hs_m3u8_core::{
    Error as CoreError, FetchFailure, HttpErrorKind, RefreshReason, StallReason, Unsupported,
    WorkDirProblem,
};

/// 任务以失败结束：种类与说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    /// 给用户看的说明（核心错误的文字）
    pub message: String,
}

impl Failure {
    /// 核心报的错误。
    pub fn of(error: &CoreError) -> Failure {
        Failure {
            kind: FailureKind::of(error),
            message: error.to_string(),
        }
    }

    /// 网页视频解析（或交回 yt-dlp 下载）报的错误。
    pub fn of_extract(error: &ExtractError) -> Failure {
        Failure {
            kind: FailureKind::of_extract(error),
            message: error.to_string(),
        }
    }
}

/// 探测的错误 `error` 说明地址可能是网页、值得在网页引擎里打开看看：取到的不是播放列表（没有 `#EXTM3U`：网页、视频
/// 文件之类），或被拒（403、503：网站对不是浏览器发的请求常这样回，如人机验证、按请求特征拦截；也可能就是拒绝，如
/// 防盗链的地址）。是哪一层（来源本身，还是主播放列表里的变体）的看错误里的地址。
pub fn maybe_a_page(error: &CoreError) -> bool {
    match error {
        CoreError::Playlist { cause, .. } => **cause == hs_m3u8_core::hls::Malformed::NotAPlaylist,
        CoreError::Http {
            kind: hs_m3u8_core::HttpErrorKind::Status(403 | 503),
            ..
        } => true,
        _ => false,
    }
}

/// 失败的种类：调用方据此决定怎么提示、能不能重试、该不该先改请求头或换输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 服务器回了这个 HTTP 状态码（如 403：多要带上 Referer、Cookie 之类的请求头）
    Status(u16),
    /// 连不上、超时、传输中断、内容没写完，或代理出错
    Network,
    /// 服务器的证书不受信任
    Certificate,
    /// 读写本机的文件出错（含写 MP4 时），或读不到所在的盘
    Disk,
    /// 输出路径、或要用的任务目录上已有别的东西
    OutputTaken,
    /// 来源与任务目录里已下载的不是同一份内容（换了内容、换了一档、轨道变了、直播接不上之前录的；文件任务续传时文件
    /// 变了）：已下的不能沿用，只能放弃后重新下载
    SourceChanged,
    /// 文件任务第一次下载时，地址回的已不是新建前认出的那个文件：多为签名地址过期或登录状态失效后换成了错误页、登录页。
    /// 更新请求头（Cookie 等）或换一个新链接重新新建
    NotExpected,
    /// 取到的不是播放列表、分片或 key 该有的样子：多为源站返回了错误页或登录页
    Malformed,
    /// 直播长时间录不到新分片
    Stalled,
    /// 没有可写出的：直播没录到可合并的分片，或只写出已录的时任务目录里是点播的下载；接着下载或录制可以，再写出也一样
    NothingToWrite,
    /// 内容放不进 MP4：改存本地 HLS 可以，已下的分片照样用
    Mp4Unsupported,
    /// 来源用了不支持的特性（DRM 等加密、只有关键帧的轨、增量更新的播放列表等），或不是 HLS
    Unsupported,
    /// 再试也一样的其余失败：任务目录认不出、参数不对、重定向过多、服务器不照范围请求回应、合并时内容不对等
    Unrecoverable,
    /// 云离线：网盘的登录失效了（令牌被拒、账号退出了或没登录），要重新登录后再继续
    CloudLogin,
    /// 云离线：网盘的离线次数、空间或传输额度用尽，等额度恢复或升级会员后再继续
    CloudQuota,
    /// 云离线：网盘上的离线下载失败了（没有资源、违规内容等，原因照网盘给的）
    CloudFailed,
    /// 云离线：网盘回的内容认不出（接口变了），要升级应用
    CloudApi,
    /// 下完的文件坏了，已删掉，再继续时重下它：云离线的与网盘给的 GCID 不符，网页视频的一路合并时读不出
    Integrity,
    /// 网页视频：网站要登录（或登录过期了），在嗅探页里登录这个网站后再继续
    WebLogin,
    /// 网页视频：网站按地区限制，换个代理（出口）后再继续
    WebGeo,
    /// 网页视频：网页解析不了，多为网站改版了、yt-dlp 要更新，更新后再继续
    WebExtract,
    /// 网页视频：网站说视频不可用了（删了、转为私密等），或网页现在是另一个视频：只能放弃或另建
    WebGone,
    /// 网页视频：网站这次给的格式里认不出选中的画质（没了、编号移位、内容重新编码过）。网站有时少给一些格式，再试可能
    /// 就有；一直认不出只能放弃后重新新建
    WebChanged,
    /// 网页视频：自己下时网站拒绝了这次给的下载地址（401、403、404、410 或大小不符），换过几次直链仍然这样。继续时
    /// 重新解析；要登录的补上登录后的 Cookie
    WebDenied,
    /// 网页视频：解析时网站拦下了这个出口（网页或接口回 403、412：多为出口 IP 被风控，如 B 站对境外代理）。换直连或
    /// 别的出口、登录后再继续
    WebBlocked,
    /// 网页视频：设置里选的浏览器的 Cookie 没读出或没读全（没装、系统不让读、钥匙串没给钥匙、Windows 上加密了等），还没
    /// 向网站发请求。照说明处理或改选浏览器后再继续
    WebCookies,
    /// 网页视频：解析组件没装好或坏了，重新安装后再继续
    WebComponents,
    /// 其余（任务目录正被别的任务或没做完的放弃占着、本机建不出 HTTP 客户端、不经暂停的取消）
    Other,
}

impl FailureKind {
    pub fn of(error: &CoreError) -> FailureKind {
        match error {
            CoreError::Http { kind, .. } => FailureKind::of_http(kind),
            CoreError::Segment { cause, .. } | CoreError::Key { cause, .. } => {
                FailureKind::of(cause)
            }
            CoreError::Cleanup { failure, .. } => FailureKind::of(failure),
            // 调接口（网盘）没拿到完整的响应：按原因
            CoreError::Call { cause, .. } => FailureKind::of(cause),
            CoreError::LiveStalled { cause, .. } => match cause {
                // 刷新或下载一直被服务器拒绝时按状态码
                StallReason::RefreshFailed(RefreshReason::Http(failure))
                | StallReason::Unrecordable(FetchFailure::Http(failure)) => match failure.kind {
                    HttpErrorKind::Status(code) => FailureKind::Status(code),
                    _ => FailureKind::Stalled,
                },
                StallReason::RefreshFailed(_)
                | StallReason::RefreshPending
                | StallReason::Unrecordable(_)
                | StallReason::TrackStopped(_) => FailureKind::Stalled,
            },
            CoreError::Incomplete { .. } | CoreError::SystemProxy(_) => FailureKind::Network,
            CoreError::Io { .. }
            | CoreError::VolumeUnavailable { .. }
            | CoreError::NoSpace { .. } => FailureKind::Disk,
            CoreError::NotExpected { .. } => FailureKind::NotExpected,
            CoreError::Remux(error) => FailureKind::of_remux(error),
            CoreError::OutputExists(_) | CoreError::OutputOccupied { .. } => {
                FailureKind::OutputTaken
            }
            CoreError::WorkDir { problem, .. } => match problem {
                WorkDirProblem::NotEmpty => FailureKind::OutputTaken,
                WorkDirProblem::KindMismatch { .. }
                | WorkDirProblem::PreferenceChanged
                | WorkDirProblem::TracksMismatch
                | WorkDirProblem::PlanChanged
                | WorkDirProblem::SelectionGone
                | WorkDirProblem::SourceUnverified
                | WorkDirProblem::FileChanged
                | WorkDirProblem::Unaligned { .. } => FailureKind::SourceChanged,
                WorkDirProblem::Locked => FailureKind::Other,
                WorkDirProblem::NotLiveRecording => FailureKind::NothingToWrite,
                WorkDirProblem::Corrupt(_) | WorkDirProblem::UnsupportedVersion { .. } => {
                    FailureKind::Unrecoverable
                }
            },
            CoreError::Select(_) => FailureKind::SourceChanged,
            CoreError::Playlist { .. }
            | CoreError::KeyLength { .. }
            | CoreError::Integrity { .. } => FailureKind::Malformed,
            CoreError::Unsupported(unsupported) => match unsupported {
                Unsupported::Mp4(_) => FailureKind::Mp4Unsupported,
                Unsupported::EmptyPlaylist(_)
                | Unsupported::NoTargetDuration(_)
                | Unsupported::Encryption { .. }
                | Unsupported::KeyWithoutUri(_)
                | Unsupported::IFramesOnly { .. }
                | Unsupported::DeltaUpdate { .. }
                | Unsupported::Hls(_)
                | Unsupported::InitChangesWithinGroup { .. }
                | Unsupported::DiscontinuityMismatch { .. }
                | Unsupported::NoMergeableGroup(_)
                | Unsupported::Fmp4BaseDataOffset { .. }
                | Unsupported::TsInit(_) => FailureKind::Unsupported,
            },
            // 地址多半指向视频文件或直播流，不是 HLS
            CoreError::NotMediaPlaylist { .. } | CoreError::ResponseTooLarge { .. } => {
                FailureKind::Unsupported
            }
            CoreError::NothingRecorded => FailureKind::NothingToWrite,
            CoreError::InvalidInput(_) | CoreError::LiveNotEnabled | CoreError::Hook { .. } => {
                FailureKind::Unrecoverable
            }
            CoreError::HttpClientInit(_) | CoreError::Cancelled => FailureKind::Other,
        }
    }

    fn of_http(kind: &HttpErrorKind) -> FailureKind {
        match kind {
            HttpErrorKind::Status(code) => FailureKind::Status(*code),
            HttpErrorKind::Timeout
            | HttpErrorKind::Connect(_)
            | HttpErrorKind::Transport(_)
            | HttpErrorKind::Proxy(_) => FailureKind::Network,
            HttpErrorKind::Certificate(_) => FailureKind::Certificate,
            // 重定向打转、次数过多多为登录状态不对（缺 Cookie），与取到登录页同类
            HttpErrorKind::Redirect(_) => FailureKind::Malformed,
            HttpErrorKind::RangeIgnored(_)
            | HttpErrorKind::RangeMismatch(_)
            | HttpErrorKind::InvalidRequest(_) => FailureKind::Unrecoverable,
        }
    }

    /// 网页视频解析（或交回 yt-dlp 下载）报的错误。取消算其余：任务里的取消即暂停，由调用方先分出去。
    pub fn of_extract(error: &ExtractError) -> FailureKind {
        match error {
            ExtractError::Login(_) => FailureKind::WebLogin,
            ExtractError::Geo(_) => FailureKind::WebGeo,
            ExtractError::Unavailable(_) => FailureKind::WebGone,
            ExtractError::Drm(_) => FailureKind::Unsupported,
            ExtractError::Changed(_) => FailureKind::WebChanged,
            ExtractError::RateLimited(_) => FailureKind::Status(429),
            ExtractError::Blocked(_) => FailureKind::WebBlocked,
            ExtractError::Cookies(_) => FailureKind::WebCookies,
            ExtractError::Network(_) | ExtractError::TimedOut(_) => FailureKind::Network,
            ExtractError::Broken(_) => FailureKind::WebComponents,
            ExtractError::Disk(_) => FailureKind::Disk,
            // 解析器认不出了、yt-dlp 内部出错：多为网站改版，要更新 yt-dlp
            ExtractError::Unsupported(_) | ExtractError::Other(_) | ExtractError::Crashed(_) => {
                FailureKind::WebExtract
            }
            ExtractError::Proxy(_)
            | ExtractError::System(_)
            | ExtractError::Replacing
            | ExtractError::Cancelled => FailureKind::Other,
        }
    }

    /// 合并（写 MP4、核对能否合并）时的错误。
    fn of_remux(error: &RemuxError) -> FailureKind {
        match error {
            RemuxError::Io { .. } | RemuxError::OpenOutput { .. } => FailureKind::Disk,
            RemuxError::Unsupported(_) => FailureKind::Mp4Unsupported,
            RemuxError::Cancelled => FailureKind::Other,
            RemuxError::Init(_)
            | RemuxError::NoGroups
            | RemuxError::StreamsCount { .. }
            | RemuxError::TrackCount { .. }
            | RemuxError::EmptyTrack { .. }
            | RemuxError::InvalidGaps { .. }
            | RemuxError::EmptyGroup { .. }
            | RemuxError::NonUtf8Path(_)
            | RemuxError::Unrecognized(_)
            | RemuxError::Fmp4 { .. }
            | RemuxError::Fmp4OpenEnded { .. }
            | RemuxError::OpenInput { .. }
            | RemuxError::DuplicateKind { .. }
            | RemuxError::UnplacedPacket { .. }
            | RemuxError::RejectedOptions(_)
            | RemuxError::Read { .. }
            | RemuxError::Mux(_)
            | RemuxError::Reread { .. }
            | RemuxError::VerifyStreams { .. }
            | RemuxError::VerifyPackets { .. }
            // 只有合并完整文件时才有（网页视频的音视频）
            | RemuxError::NoInputs
            | RemuxError::OutputIsInput { .. }
            | RemuxError::MissingStream { .. }
            | RemuxError::FileFormat { .. }
            | RemuxError::EmptyInput { .. } => FailureKind::Unrecoverable,
        }
    }

    /// 用户处理了外面的情况（网络恢复、腾出空间、挪走挡路的文件、重新登录网盘、等网盘额度恢复）后再试，有没有可能
    /// 成功；不需要处理什么的暂时性故障（408、429、5xx、超时、停滞、云端下载失败、校验不符）同样算，网页视频的要登录、
    /// 地区限制、被拦截、浏览器的 Cookie 没读出、解析失败、被拒、组件坏了（登录、换代理出口、照说明处理或换浏览器、
    /// 更新或重装组件之后）与认不出选中的画质（网站有时少给）也算。证书、内容不对、来源变了、放不进 MP4、没有可写出的、不支持、网盘接口变了、网页视频没了与其余再试也
    /// 一样的不算：它们要改请求头、换输出、改为续录、升级应用，或只能放弃。核心的 `Error::retryable` 只说「什么都不改」时，这里多算了要用户先处理的。
    pub fn retry_helps(self) -> bool {
        match self {
            FailureKind::Status(code) => matches!(code, 408 | 429 | 500..=599),
            FailureKind::Network
            | FailureKind::Disk
            | FailureKind::OutputTaken
            | FailureKind::Stalled
            | FailureKind::CloudLogin
            | FailureKind::CloudQuota
            | FailureKind::CloudFailed
            | FailureKind::Integrity
            | FailureKind::WebLogin
            | FailureKind::WebGeo
            | FailureKind::WebExtract
            | FailureKind::WebComponents
            | FailureKind::WebChanged
            | FailureKind::WebDenied
            | FailureKind::WebBlocked
            | FailureKind::WebCookies
            | FailureKind::Other => true,
            FailureKind::WebGone
            | FailureKind::CloudApi
            | FailureKind::Certificate
            | FailureKind::SourceChanged
            | FailureKind::NotExpected
            | FailureKind::Malformed
            | FailureKind::Mp4Unsupported
            | FailureKind::NothingToWrite
            | FailureKind::Unsupported
            | FailureKind::Unrecoverable => false,
        }
    }

    /// 改了请求头再跑可能有不同结果：失败出在请求上（被拒、错误页、网络、停滞），或还没跑出结果（没有可写出的，接着
    /// 录制），或网页视频要登录、被拦截、下载地址被拒（换上登录后的 Cookie）。证书、不支持、放不进 MP4、来源变了、本机磁盘、保存位置被占、
    /// 云离线的各种、网页视频的其余与再试也一样的，与请求头无关（云离线任务没有请求头）。
    pub fn headers_can_change_outcome(self) -> bool {
        match self {
            FailureKind::Status(_)
            | FailureKind::Malformed
            | FailureKind::NotExpected
            | FailureKind::Network
            | FailureKind::Stalled
            | FailureKind::NothingToWrite
            | FailureKind::WebLogin
            | FailureKind::WebDenied
            | FailureKind::WebBlocked
            | FailureKind::Other => true,
            FailureKind::Certificate
            | FailureKind::WebCookies
            | FailureKind::WebGeo
            | FailureKind::WebExtract
            | FailureKind::WebGone
            | FailureKind::WebChanged
            | FailureKind::WebComponents
            | FailureKind::CloudLogin
            | FailureKind::CloudQuota
            | FailureKind::CloudFailed
            | FailureKind::CloudApi
            | FailureKind::Integrity
            | FailureKind::Disk
            | FailureKind::OutputTaken
            | FailureKind::SourceChanged
            | FailureKind::Mp4Unsupported
            | FailureKind::Unsupported
            | FailureKind::Unrecoverable => false,
        }
    }

    /// 多半要先改请求头（补上 Referer、Cookie、Authorization）：被服务器拒绝，或取到的是错误页、登录页、重定向打转，
    /// 或文件任务的地址回的已不是之前的文件（登录状态失效）。
    /// 是 [`FailureKind::headers_can_change_outcome`] 的一部分：这里为真的，那里一定为真。
    pub fn headers_likely_missing(self) -> bool {
        matches!(
            self,
            FailureKind::Status(401 | 403) | FailureKind::Malformed | FailureKind::NotExpected
        )
    }

    /// 在存储里的写法：`status:<状态码>` 或种类名。存储格式的一部分：存储的 failure 列有 CHECK 列出全部写法，改写法或
    /// 加种类要随表结构一起改（已发布的表结构版本只能追加迁移），旧版本应用因此打不开新写法的存储，而不是读出认不出的行。
    pub(crate) fn code(self) -> String {
        match self {
            FailureKind::Status(code) => format!("status:{code}"),
            FailureKind::Network => "network".into(),
            FailureKind::Certificate => "certificate".into(),
            FailureKind::Disk => "disk".into(),
            FailureKind::OutputTaken => "output-taken".into(),
            FailureKind::SourceChanged => "source-changed".into(),
            FailureKind::NotExpected => "not-expected".into(),
            FailureKind::Mp4Unsupported => "mp4-unsupported".into(),
            FailureKind::Unrecoverable => "unrecoverable".into(),
            FailureKind::Malformed => "malformed".into(),
            FailureKind::Stalled => "stalled".into(),
            FailureKind::NothingToWrite => "nothing-to-write".into(),
            FailureKind::Unsupported => "unsupported".into(),
            FailureKind::CloudLogin => "cloud-login".into(),
            FailureKind::CloudQuota => "cloud-quota".into(),
            FailureKind::CloudFailed => "cloud-failed".into(),
            FailureKind::CloudApi => "cloud-api".into(),
            FailureKind::Integrity => "integrity".into(),
            FailureKind::WebLogin => "web-login".into(),
            FailureKind::WebGeo => "web-geo".into(),
            FailureKind::WebExtract => "web-extract".into(),
            FailureKind::WebGone => "web-gone".into(),
            FailureKind::WebChanged => "web-changed".into(),
            FailureKind::WebDenied => "web-denied".into(),
            FailureKind::WebBlocked => "web-blocked".into(),
            FailureKind::WebCookies => "web-cookies".into(),
            FailureKind::WebComponents => "web-components".into(),
            FailureKind::Other => "other".into(),
        }
    }

    /// [`FailureKind::code`] 的反过来；认不出时为 None。
    pub(crate) fn parse(code: &str) -> Option<FailureKind> {
        Some(match code {
            "network" => FailureKind::Network,
            "certificate" => FailureKind::Certificate,
            "disk" => FailureKind::Disk,
            "output-taken" => FailureKind::OutputTaken,
            "source-changed" => FailureKind::SourceChanged,
            "not-expected" => FailureKind::NotExpected,
            "mp4-unsupported" => FailureKind::Mp4Unsupported,
            "unrecoverable" => FailureKind::Unrecoverable,
            "malformed" => FailureKind::Malformed,
            "stalled" => FailureKind::Stalled,
            "nothing-to-write" => FailureKind::NothingToWrite,
            "unsupported" => FailureKind::Unsupported,
            "cloud-login" => FailureKind::CloudLogin,
            "cloud-quota" => FailureKind::CloudQuota,
            "cloud-failed" => FailureKind::CloudFailed,
            "cloud-api" => FailureKind::CloudApi,
            "integrity" => FailureKind::Integrity,
            "web-login" => FailureKind::WebLogin,
            "web-geo" => FailureKind::WebGeo,
            "web-extract" => FailureKind::WebExtract,
            "web-gone" => FailureKind::WebGone,
            "web-changed" => FailureKind::WebChanged,
            "web-denied" => FailureKind::WebDenied,
            "web-blocked" => FailureKind::WebBlocked,
            "web-cookies" => FailureKind::WebCookies,
            "web-components" => FailureKind::WebComponents,
            "other" => FailureKind::Other,
            _ => FailureKind::Status(code.strip_prefix("status:")?.parse().ok()?),
        })
    }
}

/// 每一种失败（状态码取一个），存储与写法的测试逐个用。下面的 match 列出全部种类：加种类时它先编译不过，提醒把新种类
/// 也加进列表。
#[cfg(test)]
pub(crate) fn every() -> Vec<FailureKind> {
    let all = vec![
        FailureKind::Status(403),
        FailureKind::Network,
        FailureKind::Certificate,
        FailureKind::Disk,
        FailureKind::OutputTaken,
        FailureKind::SourceChanged,
        FailureKind::NotExpected,
        FailureKind::Malformed,
        FailureKind::Stalled,
        FailureKind::NothingToWrite,
        FailureKind::Mp4Unsupported,
        FailureKind::Unsupported,
        FailureKind::Unrecoverable,
        FailureKind::CloudLogin,
        FailureKind::CloudQuota,
        FailureKind::CloudFailed,
        FailureKind::CloudApi,
        FailureKind::Integrity,
        FailureKind::WebLogin,
        FailureKind::WebGeo,
        FailureKind::WebExtract,
        FailureKind::WebGone,
        FailureKind::WebChanged,
        FailureKind::WebDenied,
        FailureKind::WebBlocked,
        FailureKind::WebCookies,
        FailureKind::WebComponents,
        FailureKind::Other,
    ];
    for kind in &all {
        match kind {
            FailureKind::Status(_)
            | FailureKind::Network
            | FailureKind::Certificate
            | FailureKind::Disk
            | FailureKind::OutputTaken
            | FailureKind::SourceChanged
            | FailureKind::NotExpected
            | FailureKind::Malformed
            | FailureKind::Stalled
            | FailureKind::NothingToWrite
            | FailureKind::Mp4Unsupported
            | FailureKind::Unsupported
            | FailureKind::Unrecoverable
            | FailureKind::CloudLogin
            | FailureKind::CloudQuota
            | FailureKind::CloudFailed
            | FailureKind::CloudApi
            | FailureKind::Integrity
            | FailureKind::WebLogin
            | FailureKind::WebGeo
            | FailureKind::WebExtract
            | FailureKind::WebGone
            | FailureKind::WebChanged
            | FailureKind::WebDenied
            | FailureKind::WebBlocked
            | FailureKind::WebCookies
            | FailureKind::WebComponents
            | FailureKind::Other => {}
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use hs_m3u8_core::{HttpFailure, StallReason, Url};

    use super::*;

    fn http(kind: HttpErrorKind) -> CoreError {
        CoreError::Http {
            url: Box::new(Url::parse("https://a.example/x.m3u8").unwrap()),
            kind,
            retry_after: None,
        }
    }

    /// 不是播放列表、被拒（403、503）的可能是网页；空的播放列表、别的状态不是。
    #[test]
    fn pages_and_refusals_may_be_pages() {
        let playlist = |cause| CoreError::Playlist {
            url: Box::new(Url::parse("https://a.example/watch").unwrap()),
            cause: Box::new(cause),
        };
        assert!(maybe_a_page(&playlist(
            hs_m3u8_core::hls::Malformed::NotAPlaylist
        )));
        assert!(!maybe_a_page(&playlist(
            hs_m3u8_core::hls::Malformed::Empty
        )));
        assert!(maybe_a_page(&http(HttpErrorKind::Status(403))));
        assert!(maybe_a_page(&http(HttpErrorKind::Status(503))));
        assert!(!maybe_a_page(&http(HttpErrorKind::Status(404))));
    }

    /// 分片里包着的 403、直播刷新一直 403 都按状态码；其余按种类。
    #[test]
    fn errors_are_classified_through_their_causes() {
        let segment = CoreError::Segment {
            track: 0,
            sequence: 7,
            url: Box::new(Url::parse("https://a.example/7.ts").unwrap()),
            cause: Box::new(http(HttpErrorKind::Status(403))),
        };
        assert_eq!(FailureKind::of(&segment), FailureKind::Status(403));
        let stalled = CoreError::LiveStalled {
            track: 0,
            cause: StallReason::RefreshFailed(RefreshReason::Http(HttpFailure {
                kind: HttpErrorKind::Status(401),
                retry_after: None,
            })),
        };
        assert_eq!(FailureKind::of(&stalled), FailureKind::Status(401));
        assert_eq!(
            FailureKind::of(&http(HttpErrorKind::Timeout)),
            FailureKind::Network
        );
        assert_eq!(
            FailureKind::of(&CoreError::OutputExists(PathBuf::from("/m/a.mp4"))),
            FailureKind::OutputTaken
        );
    }

    /// 再试也一样的不给重试；被拒或取到错误页的要先改请求头。
    #[test]
    fn kinds_tell_whether_retrying_or_headers_help() {
        assert!(FailureKind::Status(503).retry_helps());
        assert!(!FailureKind::Status(404).retry_helps());
        assert!(!FailureKind::Malformed.retry_helps());
        assert!(FailureKind::Malformed.headers_likely_missing());
        assert!(FailureKind::Status(403).headers_likely_missing());
        assert!(!FailureKind::Network.headers_likely_missing());
        assert!(!FailureKind::Unrecoverable.headers_can_change_outcome());
        let unaligned = CoreError::WorkDir {
            path: PathBuf::from("/m/a.hsdl"),
            problem: WorkDirProblem::Unaligned { track: 1 },
        };
        assert_eq!(FailureKind::of(&unaligned), FailureKind::SourceChanged);
    }

    #[test]
    fn codes_round_trip() {
        for kind in every() {
            assert_eq!(FailureKind::parse(&kind.code()), Some(kind));
        }
        assert_eq!(FailureKind::parse("status:x"), None);
        assert_eq!(FailureKind::parse("gone"), None);
    }
}
