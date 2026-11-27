//! 界面上数字与路径的写法。

use std::path::Path;
use std::time::Duration;

use crate::widgets::Step;
use hs_dlx_app::FailureKind;
use hs_dlx_app::ytdlp::{InstallProgress, Stage};
use hs_m3u8_core::VariantInfo;

/// 在运行的任务数 `running` 与合计速度 `speed`（字节每秒）：「2 个进行中 · ↓ 18.4 MB/s」；没有在运行的任务时为 None。
pub fn activity(running: usize, speed: f64) -> Option<String> {
    (running > 0).then(|| format!("{running} 个进行中 · ↓ {}/s", bytes(speed as u64)))
}

/// 任务的来源：磁力链接、种子文件，其余为地址的主机。
pub fn source(url: &hs_m3u8_core::Url) -> String {
    match url.scheme() {
        "magnet" => "磁力链接".to_owned(),
        "file" => "种子文件".to_owned(),
        _ => url.host_str().unwrap_or_default().to_owned(),
    }
}

/// 字节数：1000 进制，保留一位小数（`812 B`、`1.9 GB`）；四舍五入后到 1000 的进到下一级。
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while unit < UNITS.len() - 1 && (value * 10.0).round() / 10.0 >= 1000.0 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 时长：不足一小时写 `分:秒`，否则 `时:分:秒`。
pub fn clock(duration: Duration) -> String {
    let total = duration.as_secs();
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 时长（剩余时间等）：向上取整到秒，只写最大的两级（`42 秒`、`3 分 10 秒`、`1 小时 2 分`）。
pub fn duration(duration: Duration) -> String {
    let total = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h} 小时 {m} 分")
    } else if m > 0 {
        format!("{m} 分 {s} 秒")
    } else {
        format!("{s} 秒")
    }
}

/// 码率：bit/s 写成 Mbps（一位小数）或 kbps（整数）；四舍五入后到 1000 kbps 的写成 Mbps。
pub fn bitrate(bits_per_second: u64) -> String {
    let kbps = (bits_per_second as f64 / 1e3).round();
    if kbps >= 1000.0 {
        format!("{:.1} Mbps", bits_per_second as f64 / 1e6)
    } else {
        format!("{kbps:.0} kbps")
    }
}

/// 照变体的码率（BANDWIDTH，播放列表里写的峰值）与时长估的大小的上限：`至多约 1.9 GB`。
pub fn size_bound(bits_per_second: u64, duration: Duration) -> String {
    let size = (bits_per_second as f64 * duration.as_secs_f64() / 8.0) as u64;
    format!("至多约 {}", bytes(size))
}

/// 一档清晰度的简称：有分辨率时为高度（如 1080p），否则为码率，都没有时为它在主播放列表里的序号。
pub fn variant(v: &VariantInfo) -> String {
    match (v.resolution, v.bandwidth) {
        (Some(r), _) => format!("{}p", r.height),
        (None, Some(bw)) => bitrate(bw),
        (None, None) => format!("第 {} 档", v.index + 1),
    }
}

/// 路径：在主目录 `home` 下的写成 `~/…`。
pub fn path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// 网页视频组件安装的这一步在做什么，下载的带字节进度，如「下载 Python · 12 MB / 30 MB（40%）」；还没收到字节时为
/// 「正在连接」。
pub fn web_install(progress: &InstallProgress) -> String {
    let (doing, downloading) = match progress.stage {
        Stage::Preparing => ("准备", false),
        Stage::Waiting => ("等在解析的解析完", false),
        Stage::Python => ("解出 Python", false),
        Stage::QuickJs => ("解出 QuickJS-NG", false),
        Stage::Resolving => ("查 yt-dlp 的版本与依赖", false),
        Stage::YtDlp => ("下载 yt-dlp 与依赖", true),
        Stage::Setup => ("安装 yt-dlp", false),
        Stage::Checking => ("检查装好的组件", false),
    };
    let amount = match progress.total {
        // 下载的还没收到字节：大小来自索引，先知道
        _ if downloading && progress.bytes == 0 => " · 正在连接…".to_owned(),
        Some(0) => String::new(),
        Some(total) => format!(
            " · {} / {}（{}%）",
            bytes(progress.bytes),
            bytes(total),
            progress.bytes.min(total) * 100 / total
        ),
        None if progress.bytes > 0 => format!(" · {}", bytes(progress.bytes)),
        None => "…".to_owned(),
    };
    format!("{doing}{amount}")
}

/// 网页视频组件安装的几步（解出随应用带的、下载 yt-dlp、安装、检查）各到了哪里：这一步之前的做完了、之后的还没做。
pub fn web_install_steps(progress: &InstallProgress) -> Vec<(gpui_kit::SharedString, Step)> {
    let at = match progress.stage {
        Stage::Preparing | Stage::Waiting => None,
        Stage::Python | Stage::QuickJs => Some(0),
        Stage::Resolving | Stage::YtDlp => Some(1),
        Stage::Setup => Some(2),
        Stage::Checking => Some(3),
    };
    [
        "解出 Python 与 QuickJS-NG",
        "下载 yt-dlp",
        "安装 yt-dlp",
        "检查",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, label)| {
        let step = match at {
            Some(at) if index < at => Step::Done,
            Some(at) if index == at => Step::Now,
            Some(_) | None => Step::Waiting,
        };
        (label.into(), step)
    })
    .collect()
}

/// 一类失败的简写与说明：简写（状态码或种类）是表盘圆盘里的大字；说明写这类失败多半是怎么回事，任务页写在检查面板，
/// 新建页写在圆盘里。完整的错误另外列出。
pub fn failure(kind: FailureKind) -> (String, &'static str) {
    match kind {
        FailureKind::Status(code @ (401 | 403)) => (
            code.to_string(),
            "服务器拒绝了请求，多要带上来源页的 Referer 或 Cookie",
        ),
        FailureKind::Status(code @ (404 | 410)) => {
            (code.to_string(), "服务器上已没有这个地址，多为链接过期")
        }
        FailureKind::Status(code) => (code.to_string(), "服务器没有正常回应"),
        FailureKind::Network => (
            "网络".into(),
            "连不上服务器或传输中断，检查网络或代理后重试",
        ),
        FailureKind::Certificate => ("证书".into(), "服务器的证书不受信任"),
        FailureKind::Disk => (
            "磁盘".into(),
            "读写本机的文件出错、空间不够，或读不到所在的盘",
        ),
        FailureKind::OutputTaken => ("已存在".into(), "保存位置上已有同名的文件或目录"),
        FailureKind::SourceChanged => (
            "不符".into(),
            "来源与已下载的接不上：点播只能重新下载，直播可以写出已录的",
        ),
        FailureKind::Malformed => (
            "内容".into(),
            "取到的不是视频该有的内容，多为错误页或登录页，常要补 Cookie",
        ),
        FailureKind::Stalled => ("停滞".into(), "直播长时间录不到新内容"),
        FailureKind::Mp4Unsupported => (
            "不兼容".into(),
            "内容放不进 MP4：改存本地 HLS，已下的照样用",
        ),
        FailureKind::NotExpected => (
            "已变".into(),
            "地址回的已不是之前看到的文件，多为链接过期或登录失效：换一个新链接，或更新 Cookie 等请求头",
        ),
        FailureKind::NothingToWrite => {
            ("空".into(), "没有可写出的已录内容：接着下载或录制后再写出")
        }
        FailureKind::Unsupported => (
            "不支持".into(),
            "来源用了不支持的特性或形式（如 DRM 加密），原因见错误信息",
        ),
        FailureKind::Unrecoverable => ("!".into(), "再试也一样，原因见错误信息"),
        FailureKind::CloudLogin => ("登录".into(), "PikPak 的登录失效了：在设置里重新登录后继续"),
        FailureKind::CloudQuota => (
            "额度".into(),
            "PikPak 的离线次数、空间或流量用完了：额度恢复或升级会员后继续",
        ),
        FailureKind::CloudFailed => (
            "云端".into(),
            "网盘那边没成（离线下载失败、文件不全或直链一直下不了），原因见错误信息；可以再试或改用本机 BT",
        ),
        FailureKind::CloudApi => (
            "接口".into(),
            "PikPak 回的内容认不出，多为接口变了：升级应用后再试",
        ),
        FailureKind::Integrity => (
            "校验".into(),
            "下完的文件坏了或与网盘上的不符，已删掉：继续时重下",
        ),
        FailureKind::WebLogin => (
            "登录".into(),
            "网站要登录或把这个出口当成了机器人：在嗅探页里打开这个网站登录，再把 Cookie 带进任务后继续；\
             走代理时也可以换个代理节点",
        ),
        FailureKind::WebGeo => ("地区".into(), "网站按地区限制：换个代理出口后继续"),
        FailureKind::WebCookies => (
            "Cookie".into(),
            "设置里选的浏览器的登录信息没读出来，原因见错误信息：照它处理，或在设置的「网页视频」里换个浏览器后继续",
        ),
        FailureKind::WebBlocked => (
            "拦截".into(),
            "网站拦下了请求：走代理时多为出口 IP 被风控（国内网站常拦境外出口），在设置里改直连或换个代理节点；直连时多为要\
             登录，或网站改了、要在设置里更新 yt-dlp",
        ),
        FailureKind::WebExtract => (
            "解析".into(),
            "网页解析不了，多为网站改版了：在设置里更新 yt-dlp 后继续",
        ),
        FailureKind::WebGone => (
            "没了".into(),
            "网站说视频不可用了，或网页现在是另一个视频：只能放弃，或重新粘贴网页另建",
        ),
        FailureKind::WebDenied => (
            "被拒".into(),
            "网站拒绝了下载地址：继续时重新解析；要登录的，改请求头补上登录后的 Cookie 再接着下",
        ),
        FailureKind::WebChanged => (
            "画质".into(),
            "网站这次给的格式里认不出选中的画质：再试一次；一直这样就放弃后重新粘贴网页另建",
        ),
        FailureKind::WebComponents => (
            "组件".into(),
            "网页视频的解析组件没装好或坏了：在设置里重新安装后继续",
        ),
        FailureKind::Plugin => (
            "插件".into(),
            "插件的进程起不来或回应不合协议：停用这个插件或重装组件后继续",
        ),
        FailureKind::Other => ("!".into(), "原因见错误信息"),
    }
}

/// 分享率（累计上传与选了的文件的字节数之比），两位小数；要下的为 0 时为「—」。
pub fn ratio(uploaded: u64, wanted: u64) -> String {
    if wanted == 0 {
        "—".to_owned()
    } else {
        format!("{:.2}", uploaded as f64 / wanted as f64)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn web_install_says_what_it_is_doing_and_where_it_is() {
        let at = |stage, bytes, total| InstallProgress {
            stage,
            bytes,
            total,
        };
        assert_eq!(
            web_install(&at(Stage::YtDlp, 0, Some(12 << 20))),
            "下载 yt-dlp 与依赖 · 正在连接…"
        );
        assert_eq!(web_install(&at(Stage::Python, 0, None)), "解出 Python…");
        assert_eq!(
            web_install(&at(Stage::YtDlp, 3 << 20, Some(12 << 20))),
            format!(
                "下载 yt-dlp 与依赖 · {} / {}（25%）",
                bytes(3 << 20),
                bytes(12 << 20)
            )
        );
        assert_eq!(web_install(&at(Stage::Setup, 0, None)), "安装 yt-dlp…");
        let marks: Vec<Step> = web_install_steps(&at(Stage::Resolving, 0, None))
            .into_iter()
            .map(|(_, step)| step)
            .collect();
        assert!(marks == [Step::Done, Step::Now, Step::Waiting, Step::Waiting]);
    }

    use super::*;

    #[test]
    fn numbers_are_written_for_people() {
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(1_940_000_000), "1.9 GB");
        assert_eq!(bytes(999_949), "999.9 KB");
        assert_eq!(bytes(999_999), "1.0 MB");
        assert_eq!(clock(Duration::from_secs(48 * 60 + 12)), "48:12");
        assert_eq!(clock(Duration::from_secs(3600 + 12 * 60 + 34)), "1:12:34");
        assert_eq!(duration(Duration::from_secs(42)), "42 秒");
        assert_eq!(duration(Duration::from_secs(190)), "3 分 10 秒");
        assert_eq!(duration(Duration::from_secs(3720)), "1 小时 2 分");
        assert_eq!(duration(Duration::from_millis(600)), "1 秒");
        assert_eq!(bitrate(6_300_000), "6.3 Mbps");
        assert_eq!(bitrate(128_000), "128 kbps");
        assert_eq!(bitrate(999_499), "999 kbps");
        assert_eq!(bitrate(999_999), "1.0 Mbps");
        assert_eq!(
            size_bound(8_000_000, Duration::from_secs(10)),
            "至多约 10.0 MB"
        );
    }

    #[test]
    fn paths_under_home_are_shortened() {
        let home = Path::new("/Users/a");
        assert_eq!(path(Path::new("/Users/a/Movies"), Some(home)), "~/Movies");
        assert_eq!(path(Path::new("/Users/a"), Some(home)), "~");
        assert_eq!(path(Path::new("/Volumes/x"), Some(home)), "/Volumes/x");
        assert_eq!(path(Path::new("/Users/a/Movies"), None), "/Users/a/Movies");
    }
}
