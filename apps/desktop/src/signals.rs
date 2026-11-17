//! 任务的「信号」：比较前后两次进度，得出这期间新发生的、值得告诉人的事。

use std::collections::BTreeMap;
use std::time::Duration;

use hs_dlx_app::{CloudPhase, CloudProgress, TorrentPhase, TorrentProgress, WebProgress};
use hs_m3u8_core::{
    FetchErrorKind, FetchFailure, FileProgress, HttpErrorKind, Progress, Purpose, Retrying, Stage,
};

use crate::format;

/// 一条信号的级别，决定卡片的描边颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    /// 重试等自己会恢复的问题
    Warn,
    /// 任务因此停下
    Error,
}

/// 一条带时刻的信号；与上一条相同的不另起一条，只加 `repeat`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    /// 最近一次发生的本地时刻
    pub at: String,
    pub level: Level,
    pub text: String,
    /// 连续发生了几次，至少 1
    pub repeat: usize,
}

/// 每个任务最多留多少条，旧的先丢。
pub const KEEP: usize = 50;

/// 记下一条信号：与最后一条的级别和文字都相同时只加次数、更新时刻。
pub fn push(signals: &mut Vec<Signal>, at: String, level: Level, text: String) {
    if let Some(last) = signals.last_mut()
        && last.level == level
        && last.text == text
    {
        last.repeat += 1;
        last.at = at;
        return;
    }
    signals.push(Signal {
        at,
        level,
        text,
        repeat: 1,
    });
    if signals.len() > KEEP {
        signals.remove(0);
    }
}

/// 从 `before`（这次运行里上一次的进度；刚开始时为 None）到 `now` 之间新发生的事。
pub fn between(before: Option<&Progress>, now: &Progress) -> Vec<(Level, String)> {
    let mut found = Vec::new();
    let known_total = before.is_some_and(|b| b.segments.total > 0);
    if !known_total && now.segments.total > 0 {
        let quality = now
            .selected
            .as_ref()
            .map(|s| format!(" · {}", format::variant(&s.variant)))
            .unwrap_or_default();
        let text = match now.stage {
            Stage::Recording => format!("开始录制{quality}"),
            _ => format!("开始下载{quality} · {} 片", now.segments.total),
        };
        found.push((Level::Info, text));
    }
    let gaps_before = before.map_or(0, |b| b.segments.gap);
    if now.segments.gap > gaps_before {
        found.push((
            Level::Info,
            format!("{} 片来源标为空缺，不下载", now.segments.gap - gaps_before),
        ));
    }
    found.extend(retries(before.map_or(&[], |b| &b.retrying), &now.retrying));
    // 直播的缺片按本次运行计，新的运行（`before` 为 None）从 0 起；另起会话由钟面的时间线报（它也认得续录时的）
    let failed_before = before.map_or(0, |b| b.segments.failed);
    if now.segments.failed > failed_before {
        found.push((
            Level::Warn,
            format!("{} 片取不到，录不回来", now.segments.failed - failed_before),
        ));
    }
    let expired_before = before.map_or(0, |b| b.segments.expired);
    if now.segments.expired > expired_before {
        found.push((
            Level::Warn,
            format!(
                "{} 片在刷新前已滑出窗口，录不回来",
                now.segments.expired - expired_before
            ),
        ));
    }
    if now.stage == Stage::Writing && before.is_none_or(|b| b.stage != Stage::Writing) {
        found.push((
            Level::Info,
            format!("{} 片下载完，开始写出", now.segments.done),
        ));
    }
    found
}

/// 文件任务从 `before`（这次运行里上一次的进度；刚开始时为 None）到 `now` 之间新发生的事。
pub fn between_file(before: Option<&FileProgress>, now: &FileProgress) -> Vec<(Level, String)> {
    let mut found = Vec::new();
    if before.is_none_or(|b| b.limit == 0) && now.limit > 0 {
        let text = if now.limit == 1 {
            "开始下载 · 一个连接（服务器不支持分段或没给出大小）".to_owned()
        } else {
            format!("开始下载 · 最多 {} 个连接", now.limit)
        };
        found.push((Level::Info, text));
    }
    if let Some(throttled) = &now.throttled
        && before.is_none_or(|b| b.throttled.as_ref() != Some(throttled))
    {
        found.push((
            Level::Warn,
            format!(
                "服务器限制连接数（{}），降到 {} 个",
                failure(&FetchFailure::Http(throttled.failure.clone())),
                throttled.limit
            ),
        ));
    }
    found.extend(retries(before.map_or(&[], |b| &b.retrying), &now.retrying));
    if now.stage == Stage::Writing && before.is_none_or(|b| b.stage != Stage::Writing) {
        found.push((Level::Info, "下载完，装上文件".to_owned()));
    }
    found
}

/// 网页视频任务从 `before`（这次运行里上一次的进度；刚开始时为 None）到 `now` 之间新发生的事：解析网页、等网站允许
/// 开始、开始下第几路、开始合并。
pub fn between_web(before: Option<&WebProgress>, now: &WebProgress) -> Vec<(Level, String)> {
    let line = match (before, now) {
        (Some(WebProgress::Resolving), WebProgress::Resolving) => None,
        (_, WebProgress::Resolving) => Some("解析网页".to_owned()),
        (Some(WebProgress::Waiting { .. }), WebProgress::Waiting { .. }) => None,
        (_, WebProgress::Waiting { until }) => {
            let at = crate::clock::local(*until, "%H:%M:%S").unwrap_or_default();
            Some(format!("网站要求等到 {at} 才能开始下（片头广告的时长）"))
        }
        (Some(WebProgress::Fetching { part: was, .. }), WebProgress::Fetching { part, .. })
            if was == part =>
        {
            None
        }
        (_, WebProgress::Fetching { part, parts, .. }) => Some(match (*parts, *part) {
            (1, _) => "开始下载".to_owned(),
            (_, 0) => "下载视频".to_owned(),
            _ => "下载音频".to_owned(),
        }),
        (Some(WebProgress::Merging { .. }), WebProgress::Merging { .. }) => None,
        (_, WebProgress::Merging { .. }) => Some("合并视频与音频".to_owned()),
    };
    line.map(|line| (Level::Info, line)).into_iter().collect()
}

/// 云离线任务从 `before`（这次运行里上一次的进度；刚开始时为 None）到 `now` 之间新发生的事：网盘排队、在下、被暂停、
/// 下完；开始下第几个文件（同一个文件里的照 [`between_file`]）；开始校验。
pub fn between_cloud(before: Option<&CloudProgress>, now: &CloudProgress) -> Vec<(Level, String)> {
    let mut found = Vec::new();
    match now {
        CloudProgress::Remote { phase, .. } => {
            let was = match before {
                Some(CloudProgress::Remote { phase, .. }) => Some(*phase),
                _ => None,
            };
            if was != Some(*phase) {
                let (level, text) = match phase {
                    CloudPhase::Queued => (Level::Info, "PikPak 收下了，在排队"),
                    CloudPhase::Running => (Level::Info, "PikPak 在下，下完后取回本机"),
                    CloudPhase::Paused => (
                        Level::Warn,
                        "PikPak 上的离线下载被暂停了（在网页或 App 里），恢复后才接着下",
                    ),
                };
                found.push((level, text.to_owned()));
            }
        }
        CloudProgress::Listing => {
            if before != Some(now) {
                found.push((Level::Info, "PikPak 下完了，列出文件".to_owned()));
            }
        }
        CloudProgress::Fetching {
            index, count, file, ..
        } => {
            let same = match before {
                Some(CloudProgress::Fetching {
                    index: at,
                    file: was,
                    ..
                }) if at == index => Some(was.as_ref()),
                _ => None,
            };
            if same.is_none() && *count > 1 {
                found.push((
                    Level::Info,
                    format!("取回第 {} / {count} 个文件", index + 1),
                ));
            }
            found.extend(between_file(same, file));
        }
        CloudProgress::Skipped { output, .. } => {
            found.push((
                Level::Warn,
                format!(
                    "{} 在网盘上被删了（或删进了回收站），跳过它",
                    output.display()
                ),
            ));
        }
        CloudProgress::Verifying { index, count, .. } => {
            let again =
                matches!(before, Some(CloudProgress::Verifying { index: at, .. }) if at == index);
            if !again {
                let which = if *count > 1 {
                    format!("第 {} 个文件", index + 1)
                } else {
                    "文件".to_owned()
                };
                found.push((Level::Info, format!("按网盘给的 GCID 校验{which}")));
            }
        }
    }
    found
}

/// BT 任务从 `before`（这次运行里上一次的进度；刚开始时为 None）到 `now` 之间新发生的事：开始与校验完磁盘上已有的
/// 文件；资源不足（[`TorrentProgress::scarce`]）与又凑得齐了；tracker 报了新的错。
pub fn between_torrent(
    before: Option<&TorrentProgress>,
    now: &TorrentProgress,
) -> Vec<(Level, String)> {
    let checking = |p: &TorrentProgress| matches!(p.phase, TorrentPhase::Checking { .. });
    let mut found = Vec::new();
    if checking(now) && !before.is_some_and(checking) {
        found.push((Level::Info, "校验磁盘上已有的文件".to_owned()));
    }
    if before.is_some_and(checking) && !checking(now) {
        found.push((
            Level::Info,
            format!("校验完 · 已有 {}", format::bytes(now.done)),
        ));
    }
    let scarce = |p: &TorrentProgress| p.scarce;
    match (before.is_some_and(scarce), now.scarce) {
        (false, true) => found.push((Level::Warn, "在线的节点凑不齐全部内容".to_owned())),
        (true, false) if now.reachable == now.pieces => {
            found.push((Level::Info, "在线的节点又凑得齐了".to_owned()));
        }
        _ => {}
    }
    if let Some(error) = &now.tracker_error
        && before.is_none_or(|b| b.tracker_error.as_ref() != Some(error))
    {
        found.push((Level::Warn, format!("tracker 报错：{error}")));
    }
    if let Some(problem) = &now.proxy_problem
        && before.is_none_or(|b| b.proxy_problem.as_ref() != Some(problem))
    {
        found.push((Level::Warn, format!("BT 代理：{problem}")));
    }
    found
}

/// `now` 里新开始退避的请求（`before` 为上一次进度里的）：按多重集合比较，同一批里完全相同的几条各算一次；同一时刻
/// 因同一原因重试的一批按（请求的是什么、原因）合成一条。
/// 简化：重试里没有是哪个请求，一条退避结束重试、另一条完全相同的同时开始时，后一条不报；要准确得让核心带上请求身份。
fn retries(before: &[Retrying], now: &[Retrying]) -> Vec<(Level, String)> {
    let mut unmatched: Vec<&Retrying> = before.iter().collect();
    let mut fresh = Vec::new();
    for retry in now {
        match unmatched.iter().position(|old| *old == retry) {
            Some(at) => {
                unmatched.swap_remove(at);
            }
            None => fresh.push(retry),
        }
    }
    let mut groups: BTreeMap<(&'static str, String), Vec<&Retrying>> = BTreeMap::new();
    for retry in fresh {
        groups
            .entry((purpose(retry.purpose), failure(&retry.failure)))
            .or_default()
            .push(retry);
    }
    groups
        .into_iter()
        .map(|((what, why), retries)| {
            let delay = retries.iter().map(|r| r.delay).min().expect("每组至少一条");
            let text = match retries.as_slice() {
                [one] => format!(
                    "{}，{}后第 {} 次重试",
                    joined(what, &why),
                    seconds(delay),
                    one.retry
                ),
                many => format!(
                    "{} 个{}，{}后重试",
                    many.len(),
                    joined(what, &why),
                    seconds(delay)
                ),
            };
            (Level::Warn, text)
        })
        .collect()
}

fn purpose(purpose: Purpose) -> &'static str {
    match purpose {
        Purpose::Playlist => "播放列表",
        Purpose::Key => "密钥",
        Purpose::Init => "init 段",
        Purpose::Segment => "分片",
        Purpose::File => "文件的一段",
        Purpose::Call => "接口",
    }
}

/// 「请求的是什么」与「原因」连成一句：原因以字母或数字开头时（`HTTP 503`）中间空一格。
fn joined(what: &str, why: &str) -> String {
    if why.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        format!("{what} {why}")
    } else {
        format!("{what}{why}")
    }
}

/// 退避时长：不足 10 秒保留一位小数。
fn seconds(delay: Duration) -> String {
    let secs = delay.as_secs_f64();
    if secs < 10.0 {
        format!("{secs:.1} 秒")
    } else {
        format!("{secs:.0} 秒")
    }
}

/// 失败原因的简短说法；HTTP 库给的完整错误链太长，放不进信号卡片。
fn failure(failure: &FetchFailure) -> String {
    match failure.kind() {
        FetchErrorKind::Http(kind) => match kind {
            HttpErrorKind::Status(code) => format!("HTTP {code}"),
            HttpErrorKind::Timeout => "超时".to_owned(),
            HttpErrorKind::Connect(_) => "连不上服务器".to_owned(),
            HttpErrorKind::Transport(_) => "传输中断".to_owned(),
            HttpErrorKind::Certificate(_) => "证书校验失败".to_owned(),
            HttpErrorKind::Redirect(_) => "重定向失败".to_owned(),
            HttpErrorKind::Proxy(_) => "被代理拒绝".to_owned(),
            HttpErrorKind::RangeIgnored(_) | HttpErrorKind::RangeMismatch(_) => {
                "字节范围不符".to_owned()
            }
            HttpErrorKind::InvalidRequest(_) => "请求不合法".to_owned(),
        },
        FetchErrorKind::Incomplete => "内容没写完".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use hs_m3u8_core::SegmentCounts;

    use super::*;

    fn progress(stage: Stage, done: usize, total: usize) -> Progress {
        Progress {
            stage,
            segments: SegmentCounts {
                done,
                total,
                ..SegmentCounts::default()
            },
            ..Progress::default()
        }
    }

    fn retry(retry: u32) -> Retrying {
        Retrying {
            purpose: Purpose::Segment,
            failure: FetchFailure::Incomplete,
            retry,
            delay: Duration::from_millis(500),
        }
    }

    /// 核心先发出准备阶段（总片数未知），下载开始的那次更新给出总片数；续传前已有的片数之后才计入，不影响这条。
    #[test]
    fn the_start_is_reported_once() {
        let preparing = progress(Stage::Preparing, 0, 0);
        assert_eq!(between(None, &preparing), []);
        let first = progress(Stage::Downloading, 0, 160);
        assert_eq!(
            between(Some(&preparing), &first),
            [(Level::Info, "开始下载 · 160 片".to_owned())]
        );
        let resumed = progress(Stage::Downloading, 40, 160);
        assert_eq!(between(Some(&first), &resumed), []);
    }

    #[test]
    fn gaps_retries_and_writing_are_reported_when_they_appear() {
        let before = progress(Stage::Downloading, 10, 160);
        let mut now = before.clone();
        now.segments.gap = 2;
        now.retrying = vec![retry(1), retry(2), retry(3)];
        assert_eq!(
            between(Some(&before), &now),
            [
                (Level::Info, "2 片来源标为空缺，不下载".to_owned()),
                (Level::Warn, "3 个分片内容没写完，0.5 秒后重试".to_owned()),
            ]
        );
        // 同一次重试再报一遍不算新的；与已有的完全相同的又来一条，算一条新的
        assert_eq!(between(Some(&now), &now), []);
        let mut again = now.clone();
        again.retrying.push(retry(1));
        assert_eq!(
            between(Some(&now), &again),
            [(
                Level::Warn,
                "分片内容没写完，0.5 秒后第 1 次重试".to_owned()
            )]
        );

        let mut writing = now.clone();
        writing.stage = Stage::Writing;
        writing.retrying.clear();
        assert_eq!(
            between(Some(&now), &writing),
            [(Level::Info, "10 片下载完，开始写出".to_owned())]
        );
    }

    /// 直播本次运行新缺的片各报一次；新的运行（之前没有进度）从 0 起算。
    #[test]
    fn misses_are_reported() {
        let before = progress(Stage::Recording, 10, 10);
        let mut now = before.clone();
        now.segments.failed = 2;
        now.segments.expired = 45;
        let texts: Vec<String> = between(Some(&before), &now)
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert_eq!(
            texts,
            ["2 片取不到，录不回来", "45 片在刷新前已滑出窗口，录不回来"]
        );
        assert_eq!(between(Some(&now), &now), []);
    }

    #[test]
    fn repeated_signals_are_counted_not_listed() {
        let mut signals = Vec::new();
        push(
            &mut signals,
            "10:00:00".into(),
            Level::Warn,
            "分片超时".into(),
        );
        push(
            &mut signals,
            "10:00:01".into(),
            Level::Warn,
            "分片超时".into(),
        );
        push(
            &mut signals,
            "10:00:02".into(),
            Level::Info,
            "开始写出".into(),
        );
        assert_eq!(signals.len(), 2);
        assert_eq!((signals[0].repeat, signals[0].at.as_str()), (2, "10:00:01"));
        assert_eq!(signals[1].repeat, 1);
    }

    #[test]
    fn reasons_read_as_one_phrase() {
        assert_eq!(joined("分片", "HTTP 503"), "分片 HTTP 503");
        assert_eq!(joined("密钥", "超时"), "密钥超时");
    }
}
