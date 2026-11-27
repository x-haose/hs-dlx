//! 任务此刻在界面上的样子：中央圆盘的字与颜色、刻度、任务栏的小环与失败时的说明。

use gpui_kit::Hsla;
use hs_dlx_app::web::Container;
use hs_dlx_app::{
    CloudPhase, CloudProgress, Failure, FailureKind, Status, TorrentPhase, WebProgress,
};
use hs_m3u8_core::Stage;

use crate::ring::Ticks;
use crate::tasks::{Amount, Disk, FileAmount, Output, Row, finished_text};
use crate::{format, theme};

/// 一个任务此刻的样子。
pub struct Look {
    /// 圆盘里最上面的小字与它的颜色
    pub state: String,
    pub color: Hsla,
    pub ticks: Ticks,
    pub big: String,
    pub line: String,
    /// 失败时这类失败多半是怎么回事（完整的错误在信号里）；不是失败时为 None
    pub hint: Option<&'static str>,
    /// 列表小环：进度（0 到 1）、颜色与中间的简写
    pub fraction: f32,
    pub orb: Hsla,
    pub short: String,
}

/// 向下取整的百分比（`fraction` 为 0 到 1）：没做完不写 100%。
fn percent(fraction: f32) -> String {
    format!("{}%", (fraction * 100.0).floor())
}

/// 任务为什么停着。
enum Stopped<'a> {
    Paused,
    Failed(&'a Failure),
}

pub fn look(row: &Row) -> Look {
    let torrent = row.task.spec.torrent().is_some();
    let cloud = row.task.spec.cloud().is_some();
    if row.discarding {
        let line = if torrent || cloud {
            "停下并删除已下的文件"
        } else {
            "停下并删除任务目录"
        };
        return discarding(row, "放弃中", line);
    }
    match &row.task.status {
        Status::DiscardUnfinished if torrent || cloud => discarding(
            row,
            "放弃没做完",
            "已下的文件没删干净：处理提示里留下的，再放弃一次",
        ),
        Status::DiscardUnfinished => discarding(
            row,
            "放弃没做完",
            "任务目录可能已删了一部分，再放弃一次即可",
        ),
        Status::Running if torrent => running_torrent(row),
        Status::Running if cloud => running_cloud(row),
        Status::Running if row.task.spec.web().is_some() => running_web(row),
        Status::Running if row.task.spec.hls().is_none() => running_file(row),
        Status::Running => running(row, Amount::of(row).unwrap_or_default()),
        Status::Paused => stopped(row, Stopped::Paused),
        Status::Failed(failure) => stopped(row, Stopped::Failed(failure)),
        Status::Done if row.task.seeding => seeding_look(row),
        Status::Done => done_look(row),
    }
}

/// 正在放弃或放弃没做完：刻度按已有的内容、为灰色；`state` 与 `line` 为状态与说明。
fn discarding(row: &Row, state: &str, line: &str) -> Look {
    let (done, total) = Amount::of(row).map_or((0, 0), |a| (a.done, a.total));
    Look {
        state: state.to_owned(),
        color: theme::ink_3(),
        ticks: Ticks {
            color: theme::alpha(theme::ink(), 0.34),
            ..Ticks::of(total, done, false)
        },
        big: "…".to_owned(),
        line: line.to_owned(),
        hint: None,
        fraction: 0.0,
        orb: theme::alpha(theme::ink(), 0.34),
        short: "…".to_owned(),
    }
}

/// 暂停或失败的任务：按任务目录里已完成的内容显示；失败的大字与说明按种类（见 [`format::failure`]），完整的
/// 错误在信号里。点了继续、还在读任务目录时状态写「正在继续」。
fn stopped(row: &Row, why: Stopped<'_>) -> Look {
    let kept = kept(row, matches!(why, Stopped::Paused));
    let (done, total, known, fraction) = (kept.done, kept.total, kept.known, kept.fraction);
    let recording = row.recording();
    let state = |state: &str| if row.resuming { "正在继续" } else { state }.to_owned();
    let ticks = |lit: Hsla| Ticks {
        color: lit,
        ..Ticks::of(total, done, false)
    };
    match why {
        Stopped::Paused => {
            let (big, line) = (kept.big, kept.line);
            Look {
                state: state("已暂停"),
                color: theme::ink_3(),
                ticks: ticks(theme::alpha(theme::ink(), 0.5)),
                short: if known && !recording {
                    big.clone()
                } else {
                    "‖".to_owned()
                },
                big,
                line,
                hint: None,
                fraction,
                orb: theme::alpha(theme::ink(), 0.5),
            }
        }
        Stopped::Failed(failure) => {
            let (big, mut hint) = format::failure(failure.kind);
            // 网页视频放不进 MP4：没有本地 HLS 可改
            if failure.kind == FailureKind::Mp4Unsupported
                && let Some(web) = row.task.spec.web()
            {
                hint = if web.container == Container::Original {
                    "这一路的内容放不进 MP4：放弃后重新新建，选别的画质"
                } else {
                    "内容放不进 MP4：改用 MKV，已下的照样用"
                };
            }
            let line = kept.line;
            Look {
                state: state("需要处理"),
                color: theme::warn(),
                ticks: ticks(theme::alpha(theme::ink(), 0.34)),
                big,
                line,
                hint: Some(hint),
                fraction,
                orb: theme::warn(),
                short: "!".to_owned(),
            }
        }
    }
}

/// 停着的任务已有的内容：刻度点亮多少（`done` / `total`，单位为刻度对应的片数或字节份数）、是否知道合计、比例，与
/// 圆盘里的大字与说明。
struct Kept {
    done: usize,
    total: usize,
    known: bool,
    fraction: f32,
    big: String,
    line: String,
}

/// 文件任务的刻度把总大小分成这么多份。
const FILE_TICKS: usize = 120;

/// 停着的任务的 [`Kept`]；`paused` 时说明里写明继续时接着下。
fn kept(row: &Row, paused: bool) -> Kept {
    if row.task.spec.torrent().is_some() {
        return kept_torrent(row, paused);
    }
    if row.task.spec.cloud().is_some() {
        return kept_cloud(row, paused);
    }
    if row.task.spec.web().is_some() {
        return Kept {
            done: 0,
            total: FILE_TICKS,
            known: false,
            fraction: 0.0,
            big: "‖".to_owned(),
            line: if paused {
                "已下的保留 · 继续时重新解析网页、接着下".to_owned()
            } else {
                "已下的保留".to_owned()
            },
        };
    }
    if row.task.spec.hls().is_none() {
        return kept_file(row, paused);
    }
    let amount = Amount::of(row);
    let (done, total) = amount.as_ref().map_or((0, 0), |a| (a.done, a.total));
    let (big, line) = kept_text(row, amount.as_ref(), row.recording(), paused);
    Kept {
        done,
        total,
        known: total > 0,
        fraction: amount.as_ref().map_or(0.0, Amount::fraction),
        big,
        line,
    }
}

/// 文件任务能不能续传：新建前认出服务器支持范围请求（这时总大小必已知道）。不能时已下的不沿用，中断后从头下。
fn resumable(row: &Row) -> bool {
    row.task
        .spec
        .file()
        .is_some_and(|file| file.expected.accepts_ranges)
}

/// 停着的文件任务的 [`Kept`]：按任务目录里已落盘的字节数。不能续传时（[`resumable`]）写明继续时从头下。
fn kept_file(row: &Row, paused: bool) -> Kept {
    let resumable = resumable(row);
    // 不能续传时已下的不算数，不按它显示比例
    let amount = FileAmount::of(row).filter(|_| resumable);
    let fraction = amount.as_ref().and_then(FileAmount::fraction);
    let then = if paused { " · 继续时接着下" } else { "" };
    let line = match (&amount, &row.disk) {
        (_, Disk::Unreadable(reason)) => format!("读不出任务目录：{reason}"),
        _ if !resumable => "服务器不支持续传：继续时从头下".to_owned(),
        (Some(a), _) if a.bytes > 0 => match a.total {
            Some(total) => format!(
                "已下 {} / {}{then}",
                format::bytes(a.bytes),
                format::bytes(total)
            ),
            None => format!("已下 {}{then}", format::bytes(a.bytes)),
        },
        (_, Disk::Unread) => "正在读取任务目录".to_owned(),
        _ => "还没有下载内容".to_owned(),
    };
    let lit = fraction.map_or(0, |f| (f * FILE_TICKS as f32) as usize);
    Kept {
        done: lit,
        total: FILE_TICKS,
        known: fraction.is_some(),
        fraction: fraction.unwrap_or(0.0),
        big: fraction.map_or_else(|| "‖".to_owned(), percent),
        line,
    }
}

/// 按已下的比例点亮的刻度份数（共 [`FILE_TICKS`] 份）。
fn lit(fraction: Option<f32>) -> usize {
    fraction.map_or(0, |f| (f * FILE_TICKS as f32) as usize)
}

/// 停着的 BT 任务的 [`Kept`]：按存着的续传数据里已有的字节数；上次没正常退出的，已有的要等继续时校验才知道。
fn kept_torrent(row: &Row, paused: bool) -> Kept {
    let amount = FileAmount::of(row);
    let fraction = amount.as_ref().and_then(FileAmount::fraction);
    let then = if paused { " · 继续时接着下" } else { "" };
    let line = match (&amount, &row.disk) {
        (_, Disk::Unreadable(reason)) => format!("读不出存着的进度：{reason}"),
        (_, Disk::Torrent(saved)) if !saved.trusted => "已下的要等继续时重新校验".to_owned(),
        (Some(a), _) if a.bytes > 0 => format!(
            "已下 {} / {}{then}",
            format::bytes(a.bytes),
            format::bytes(a.total.unwrap_or(0))
        ),
        (_, Disk::Unread) => "正在读取进度".to_owned(),
        _ => "还没有下载内容".to_owned(),
    };
    Kept {
        done: lit(fraction),
        total: FILE_TICKS,
        known: fraction.is_some(),
        fraction: fraction.unwrap_or(0.0),
        big: fraction.map_or_else(|| "‖".to_owned(), percent),
        line,
    }
}

/// 停着的云离线任务的 [`Kept`]：按存储里记着的已取回、校验过的文件；还没列出文件的，网盘那边还没下完。
fn kept_cloud(row: &Row, paused: bool) -> Kept {
    let amount = FileAmount::of(row);
    let fraction = amount.as_ref().and_then(FileAmount::fraction);
    let then = if paused {
        " · 继续时接着取回"
    } else {
        ""
    };
    let line = match (&amount, &row.disk) {
        (_, Disk::Unreadable(reason)) => format!("读不出进度：{reason}"),
        (_, Disk::Cloud(files)) if files.is_empty() => {
            let then = if paused { " · 继续时接着等" } else { "" };
            format!("还没取回：PikPak 那边还没下完{then}")
        }
        (Some(a), _) if a.bytes > 0 => format!(
            "已取回 {} / {}{then}",
            format::bytes(a.bytes),
            format::bytes(a.total.unwrap_or(0))
        ),
        (_, Disk::Unread) => "正在读取进度".to_owned(),
        _ => "还没有取回内容".to_owned(),
    };
    Kept {
        done: lit(fraction),
        total: FILE_TICKS,
        known: fraction.is_some(),
        fraction: fraction.unwrap_or(0.0),
        big: fraction.map_or_else(|| "‖".to_owned(), percent),
        line,
    }
}

/// 在跑的云离线任务：网盘在下时按网盘报的百分比，用暗一档的颜色（与之后本机取回的百分比分开）；网盘被暂停时为警告色。
/// 之后列出文件、按字节取回（多个文件时写第几个）、按 GCID 校验。
fn running_cloud(row: &Row) -> Look {
    let look = |state: &str, color: Hsla, fraction: f32, big: String, line: String| Look {
        state: state.to_owned(),
        color,
        ticks: Ticks {
            color,
            ..Ticks::of(FILE_TICKS, lit(Some(fraction)), true)
        },
        short: percent(fraction),
        big,
        line,
        hint: None,
        fraction,
        orb: color,
    };
    let preparing = |line: &str| Look {
        state: "准备中".to_owned(),
        color: theme::accent(),
        ticks: Ticks::of(0, 0, true),
        big: "—".to_owned(),
        line: line.to_owned(),
        hint: None,
        fraction: 0.0,
        orb: theme::accent(),
        short: "…".to_owned(),
    };
    let which = |index: usize, count: usize| {
        if count > 1 {
            format!("第 {} / {count} 个文件 · ", index + 1)
        } else {
            String::new()
        }
    };
    let amount = FileAmount::of(row);
    let fraction = amount
        .as_ref()
        .and_then(FileAmount::fraction)
        .unwrap_or(0.0);
    let taken = |a: &FileAmount| {
        format!(
            "{} / {}",
            format::bytes(a.bytes),
            format::bytes(a.total.unwrap_or(0))
        )
    };
    match (row.cloud_progress.as_ref(), amount.as_ref()) {
        (None, _) => preparing("连接 PikPak"),
        (Some(CloudProgress::Remote { phase, percent: p }), _) => {
            let share = f32::from(*p) / 100.0;
            let (state, color, line) = match phase {
                CloudPhase::Paused => (
                    "云端暂停",
                    theme::warn(),
                    "在网页或 App 里被暂停了，恢复后才接着下",
                ),
                CloudPhase::Queued => (
                    "云端排队",
                    theme::alpha(theme::accent(), 0.6),
                    "PikPak 收下了，在排队",
                ),
                CloudPhase::Running => (
                    "云端下载",
                    theme::alpha(theme::accent(), 0.6),
                    "PikPak 在下，下完后取回本机",
                ),
            };
            look(state, color, share, percent(share), line.to_owned())
        }
        (Some(CloudProgress::Listing), _) => preparing("PikPak 下完了，列出文件"),
        (Some(CloudProgress::Skipped { output, .. }), _) => {
            preparing(&format!("网盘上删了 {}，跳过", output.display()))
        }
        (
            Some(CloudProgress::Fetching {
                index, count, file, ..
            }),
            Some(a),
        ) => {
            let mut seen = look(
                "取回中",
                theme::accent(),
                fraction,
                percent(fraction),
                format!("{}{}", which(*index, *count), taken(a)),
            );
            seen.ticks.retrying = !file.retrying.is_empty();
            seen
        }
        (Some(CloudProgress::Verifying { index, count, .. }), Some(a)) => look(
            "校验中",
            theme::accent(),
            fraction,
            percent(fraction),
            format!("{}按 GCID 校验 · {}", which(*index, *count), taken(a)),
        ),
        (Some(CloudProgress::Fetching { .. } | CloudProgress::Verifying { .. }), None) => {
            unreachable!("取回与校验的进度总能算出字节数")
        }
    }
}

/// 在下的 BT 任务：校验时按校验了的比例；下载时刻度按已下的字节比例，连着节点而凑不齐全部内容时标「资源不足」；下完
/// 了正在存进度、从会话里移除。
fn running_torrent(row: &Row) -> Look {
    let look = |state: &str, color: Hsla, fraction: f32, big: String, line: String| Look {
        state: state.to_owned(),
        color,
        ticks: Ticks::of(FILE_TICKS, lit(Some(fraction)), true),
        short: percent(fraction),
        big,
        line,
        hint: None,
        fraction,
        orb: color,
    };
    let Some(progress) = row.torrent_progress.as_ref() else {
        return Look {
            state: "准备中".to_owned(),
            color: theme::accent(),
            ticks: Ticks::of(0, 0, true),
            big: "—".to_owned(),
            line: String::new(),
            hint: None,
            fraction: 0.0,
            orb: theme::accent(),
            short: "…".to_owned(),
        };
    };
    match progress.phase {
        TorrentPhase::Checking { progress: share } => look(
            "校验中",
            theme::accent(),
            share,
            percent(share),
            "校验磁盘上已有的文件".to_owned(),
        ),
        // 做种的进度只在任务完成之后（见 seeding_look）
        TorrentPhase::Finishing | TorrentPhase::Seeding => look(
            "收尾",
            theme::accent(),
            1.0,
            "100%".to_owned(),
            String::new(),
        ),
        TorrentPhase::Downloading => {
            let fraction = if progress.wanted > 0 {
                (progress.done as f64 / progress.wanted as f64).min(1.0) as f32
            } else {
                0.0
            };
            let amount = format!(
                "{} / {}",
                format::bytes(progress.done),
                format::bytes(progress.wanted)
            );
            if progress.peers == 0 {
                // 私有种子只经 tracker 找节点：它报错时多半一直连不上，原因在信号里
                let why = if progress.tracker_error.is_some() {
                    "tracker 报错"
                } else {
                    "还没连上节点"
                };
                return look(
                    "找节点",
                    theme::accent(),
                    fraction,
                    percent(fraction),
                    format!("{amount} · {why}"),
                );
            }
            if progress.scarce {
                let share = progress.reachable as f32 / progress.pieces as f32;
                return look(
                    "资源不足",
                    theme::warn(),
                    fraction,
                    percent(fraction),
                    format!("{amount} · 在线的节点只凑得齐 {}", percent(share)),
                );
            }
            look(
                "下载中",
                theme::accent(),
                fraction,
                percent(fraction),
                format!("{amount} · {} 个节点", progress.peers),
            )
        }
    }
}

/// 在下的文件任务：刻度按已下的字节比例；不知道总大小时只写已下多少。
fn running_file(row: &Row) -> Look {
    let progress = row.file_progress.as_ref();
    let stage = progress.map_or(Stage::Preparing, |p| p.stage);
    let amount = FileAmount::of(row);
    let fraction = amount.as_ref().and_then(FileAmount::fraction);
    let lit = fraction.map_or(0, |f| (f * FILE_TICKS as f32) as usize);
    if stage == Stage::Writing {
        return Look {
            state: "装上文件".to_owned(),
            color: theme::accent(),
            ticks: Ticks::of(FILE_TICKS, FILE_TICKS, false),
            big: "100%".to_owned(),
            line: "不联网".to_owned(),
            hint: None,
            fraction: 1.0,
            orb: theme::core(),
            short: "…".to_owned(),
        };
    }
    let Some(amount) = amount.filter(|_| progress.is_some_and(|p| p.limit > 0)) else {
        return Look {
            state: "准备中".to_owned(),
            color: theme::accent(),
            ticks: Ticks::of(0, 0, true),
            big: "—".to_owned(),
            line: "连接服务器".to_owned(),
            hint: None,
            fraction: 0.0,
            orb: theme::accent(),
            short: "…".to_owned(),
        };
    };
    let mut ticks = Ticks::of(FILE_TICKS, lit, true);
    ticks.retrying = progress.is_some_and(|p| !p.retrying.is_empty());
    let (big, mut line) = match amount.total {
        Some(total) => (
            percent(fraction.unwrap_or(0.0)),
            format!("{} / {}", format::bytes(amount.bytes), format::bytes(total)),
        ),
        None => (format::bytes(amount.bytes), "服务器没给出大小".to_owned()),
    };
    if !resumable(row) {
        line.push_str(" · 中断后要从头下");
    }
    Look {
        state: "下载中".to_owned(),
        color: theme::accent(),
        ticks,
        short: fraction.map_or_else(|| "…".to_owned(), percent),
        big,
        line,
        hint: None,
        fraction: fraction.unwrap_or(0.0),
        orb: theme::accent(),
    }
}

/// 在下的网页视频任务：解析、等网站允许开始时为准备中；下载与合并时刻度按字节的比例。
fn running_web(row: &Row) -> Look {
    let preparing = |line: String| Look {
        state: "准备中".to_owned(),
        color: theme::accent(),
        ticks: Ticks::of(0, 0, true),
        big: "—".to_owned(),
        line,
        hint: None,
        fraction: 0.0,
        orb: theme::accent(),
        short: "…".to_owned(),
    };
    let (state, which) = match row.web_progress.as_ref() {
        None | Some(WebProgress::Resolving) => return preparing("解析网页".to_owned()),
        Some(WebProgress::Waiting { until }) => {
            let at = crate::clock::local(*until, "%H:%M:%S").unwrap_or_default();
            return preparing(format!("网站要求等到 {at} 才能开始下"));
        }
        Some(WebProgress::Fetching { part, parts, .. }) => (
            "下载中",
            match (*parts, *part) {
                (1, _) => String::new(),
                (_, 0) => "视频 · ".to_owned(),
                _ => "音频 · ".to_owned(),
            },
        ),
        Some(WebProgress::Merging { .. }) => ("合并中", "不联网 · ".to_owned()),
    };
    let amount = FileAmount::of(row).unwrap_or(FileAmount {
        bytes: 0,
        total: None,
    });
    let fraction = amount.fraction();
    let line = match amount.total {
        Some(total) => format!(
            "{which}{} / {}",
            format::bytes(amount.bytes),
            format::bytes(total)
        ),
        None => format!("{which}{}", format::bytes(amount.bytes)),
    };
    Look {
        state: state.to_owned(),
        color: theme::accent(),
        ticks: Ticks::of(FILE_TICKS, lit(fraction), true),
        short: fraction.map_or_else(|| "…".to_owned(), percent),
        big: fraction.map_or_else(|| format::bytes(amount.bytes), percent),
        line,
        hint: None,
        fraction: fraction.unwrap_or(0.0),
        orb: theme::accent(),
    }
}

/// 停着的任务的大字与说明：录制写已录的时长，点播写百分比与片数，`paused` 时写明继续时接着下；没有内容时说明为什么。
fn kept_text(
    row: &Row,
    amount: Option<&Amount>,
    recording: bool,
    paused: bool,
) -> (String, String) {
    let known = amount.is_some_and(|a| a.total > 0);
    let then = if paused { " · 继续时接着下" } else { "" };
    let line = match (amount, &row.disk) {
        (Some(a), _) if recording => {
            format!(
                "已录 {} · {}",
                format::clock(a.duration),
                format::bytes(a.bytes)
            )
        }
        (Some(a), _) if a.total > 0 => format!("已下 {} / {} 片{then}", a.done, a.total),
        (Some(a), _) if a.done > 0 => {
            format!("已下 {} 片 · {}{then}", a.done, format::bytes(a.bytes))
        }
        (_, Disk::Unreadable(reason)) => format!("读不出任务目录：{reason}"),
        (_, Disk::Unread) => "正在读取任务目录".to_owned(),
        // 存储里记着总片数说明下过；macOS 上外接盘卸下与目录被删分不出来
        (_, Disk::Read(None)) if row.task.total.is_some() => {
            "任务目录不在：所在的盘没接上，或被删了".to_owned()
        }
        _ => "还没有下载内容".to_owned(),
    };
    let big = match amount {
        Some(a) if recording => format::clock(a.duration),
        Some(a) if known => percent(a.fraction()),
        _ => "‖".to_owned(),
    };
    (big, line)
}

/// 已完成的任务：整圈点亮（刻度数按存储里的总片数，与下载时一致），大字为时长，下面写输出的种类、视频的宽高与大小。
/// 在做种的 BT 任务：刻度全亮；大字为分享率，说明为已上传、上传速度与节点数。开始做种时先校验已下的文件，写校验到
/// 哪；还没报来进度时只写在做种。
fn seeding_look(row: &Row) -> Look {
    let ticks = Ticks::of(FILE_TICKS, FILE_TICKS, true);
    let (big, line) = match row.torrent_progress.as_ref() {
        Some(progress) if progress.phase == TorrentPhase::Seeding => (
            format::ratio(progress.uploaded, progress.wanted),
            format!(
                "已上传 {} · {}/s · {} 个节点",
                format::bytes(progress.uploaded),
                format::bytes(progress.upload_rate),
                progress.peers
            ),
        ),
        Some(progress) => match progress.phase {
            TorrentPhase::Checking { progress: share } => {
                (percent(share), "校验已下的文件".to_owned())
            }
            _ => ("↑".to_owned(), String::new()),
        },
        None => ("↑".to_owned(), String::new()),
    };
    Look {
        state: "做种中".to_owned(),
        color: theme::accent(),
        ticks,
        big,
        line,
        hint: None,
        fraction: 1.0,
        orb: theme::deep(),
        short: "↑".to_owned(),
    }
}

fn done_look(row: &Row) -> Look {
    let total = row.task.total.unwrap_or(0);
    let ticks = Ticks::of(total, total, false);
    let result = row.task.result.as_ref();
    let big = result
        .and_then(|r| r.duration)
        .map_or_else(|| "✓".to_owned(), format::clock);
    Look {
        state: "已完成".to_owned(),
        color: theme::ink_3(),
        ticks: Ticks {
            lit: ticks.count,
            ..ticks
        },
        big,
        line: result.map_or_else(
            || row.file_name.clone(),
            |r| finished_text(&row.task.spec, r),
        ),
        hint: None,
        fraction: 1.0,
        orb: theme::alpha(theme::ink(), 0.34),
        short: "✓".to_owned(),
    }
}

fn running(row: &Row, amount: Amount) -> Look {
    let stage = row.progress.as_ref().map_or(Stage::Preparing, |p| p.stage);
    if stage == Stage::Writing {
        let writing = row.writing();
        let kind = match writing {
            Some((Output::Mp4, _)) => "写出 MP4",
            Some((Output::Hls, _)) => "写出本地 HLS",
            None => "写出",
        };
        let count = writing.map(|(_, count)| count);
        let written = row.written_share().unwrap_or(0.0);
        let percent = percent(written);
        let ticks = Ticks::of(amount.total, amount.done, false);
        return Look {
            state: kind.to_owned(),
            color: theme::accent(),
            ticks: Ticks {
                lit: ticks.count,
                written: (written * ticks.count as f32) as usize,
                ..ticks
            },
            big: percent.clone(),
            line: count.map_or_else(
                || "不联网".to_owned(),
                |c| format!("已写 {} / {} 个文件 · 不联网", c.done, c.total),
            ),
            hint: None,
            fraction: written,
            orb: theme::core(),
            short: percent,
        };
    }
    if row.task.write_only {
        return Look {
            state: "准备写出".to_owned(),
            color: theme::accent(),
            ticks: Ticks::of(0, 0, true),
            big: format::clock(amount.duration.max(row.recorded_before_writing)),
            line: "不联网，只写出已录的".to_owned(),
            hint: None,
            fraction: 0.0,
            orb: theme::accent(),
            short: "…".to_owned(),
        };
    }
    if row.recording() {
        return Look {
            state: "● 录制中".to_owned(),
            color: theme::live(),
            ticks: Ticks {
                color: theme::live(),
                ..Ticks::of(amount.total, amount.done, true)
            },
            big: format::clock(amount.duration),
            line: format!("{} 片 · {}", amount.done, format::bytes(amount.bytes)),
            hint: None,
            fraction: 1.0,
            orb: theme::live(),
            short: "REC".to_owned(),
        };
    }
    if amount.total == 0 {
        return Look {
            state: "准备中".to_owned(),
            color: theme::accent(),
            ticks: Ticks::of(0, 0, true),
            big: "—".to_owned(),
            line: "拉取播放列表".to_owned(),
            hint: None,
            fraction: 0.0,
            orb: theme::accent(),
            short: "…".to_owned(),
        };
    }
    let mut ticks = Ticks::of(amount.total, amount.done, true);
    ticks.retrying = row
        .progress
        .as_ref()
        .is_some_and(|p| !p.retrying.is_empty());
    Look {
        state: "下载中".to_owned(),
        color: theme::accent(),
        ticks,
        big: percent(amount.fraction()),
        line: format!("{} / {} 片", amount.done, amount.total),
        hint: None,
        fraction: amount.fraction(),
        orb: theme::accent(),
        short: percent(amount.fraction()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use hs_dlx_app::{Failure, FailureKind, Finished, Status};
    use hs_m3u8_core::{FileCount, Progress, SegmentCounts, Stage, Writing};

    use super::look;
    use crate::tasks::Disk;
    use crate::tasks::sample::{row, saved};

    /// 暂停的任务按任务目录里读到的与存储里的总片数显示；读之前、读不出、目录不在各有说法。
    #[test]
    fn paused_tasks_show_what_is_in_the_work_dir() {
        let mut paused = row(Status::Paused, false, Some(160));
        assert_eq!(look(&paused).line, "正在读取任务目录");
        paused.disk = Disk::Unreadable("权限不够".into());
        assert_eq!(look(&paused).line, "读不出任务目录：权限不够");
        paused.disk = Disk::Read(None);
        assert!(look(&paused).line.starts_with("任务目录不在"));
        paused.disk = Disk::Read(Some(saved(40, false)));
        let seen = look(&paused);
        assert_eq!((seen.big.as_str(), seen.fraction), ("25%", 0.25));
        assert_eq!((seen.ticks.count, seen.ticks.lit), (160, 40));
        paused.task.total = None;
        assert_eq!(look(&paused).line, "已下 40 片 · 4.0 MB · 继续时接着下");
    }

    /// 继续后在准备阶段就失败：这次运行的进度是 0/0，读到任务目录后按目录里已有的显示。
    #[test]
    fn a_resume_that_fails_early_still_shows_the_work_dir() {
        let forbidden = Failure {
            kind: FailureKind::Status(403),
            message: "HTTP 状态 403".into(),
        };
        let mut failed = row(Status::Failed(forbidden), false, Some(420));
        failed.progress = Some(Progress::default());
        failed.disk = Disk::Read(Some(saved(143, false)));
        let seen = look(&failed);
        assert_eq!((seen.ticks.count, seen.ticks.lit), (360, 122));
        assert_eq!(seen.big, "403");
        assert!(seen.hint.is_some_and(|hint| hint.contains("Referer")));
        assert_eq!(seen.line, "已下 143 / 420 片");
    }

    /// 暂停的直播录制写已录的时长，不写片数与百分比。
    #[test]
    fn paused_recordings_show_recorded_time() {
        let mut paused = row(Status::Paused, true, None);
        paused.disk = Disk::Read(Some(saved(40, true)));
        let seen = look(&paused);
        assert_eq!((seen.big.as_str(), seen.short.as_str()), ("00:40", "‖"));
        assert!(seen.line.starts_with("已录 00:40"), "{}", seen.line);
    }

    /// 写出时整圈都已完成，按写出的文件比例点亮白色；百分比向下取整。
    #[test]
    fn writing_lights_the_written_share() {
        let mut writing = row(Status::Running, false, None);
        writing.progress = Some(Progress {
            stage: Stage::Writing,
            segments: SegmentCounts {
                done: 160,
                total: 160,
                ..SegmentCounts::default()
            },
            writing: Some(Writing {
                mp4: Some(FileCount {
                    done: 159,
                    total: 160,
                }),
                hls: None,
            }),
            ..Progress::default()
        });
        let seen = look(&writing);
        assert_eq!(
            (seen.state.as_str(), seen.big.as_str()),
            ("写出 MP4", "99%")
        );
        assert_eq!((seen.ticks.lit, seen.ticks.written), (160, 159));
    }

    #[test]
    fn finished_tasks_describe_their_output() {
        let mut done = row(Status::Done, false, Some(10));
        done.task.result = Some(Finished {
            duration: Some(Duration::from_secs(48 * 60 + 12)),
            bytes: 1_420_000_000,
            resolution: Some((1920, 1080)),
        });
        let seen = look(&done);
        assert_eq!((seen.ticks.count, seen.ticks.lit), (60, 60));
        assert_eq!(seen.big, "48:12");
        assert_eq!(seen.line, "MP4 · 1920×1080 · 1.4 GB");
    }
}
