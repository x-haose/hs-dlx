//! 直播录制的任务视图：舞台中间是钟面（见 [`crate::live`]），检查面板里是录制的读数与钟面的图例。

use std::time::Duration;

use gpui_kit::{
    Div, FontWeight, Hsla, ParentElement as _, SharedString, Styled as _, canvas, div, px,
};

use hs_dlx_app::Status;

use super::look::Look;
use crate::live::{self, Face};
use crate::ring::{AREA, angle};
use crate::tasks::Row;
use crate::widgets::{self, CAPTION_LINE, Stat, TEXT_LINE};
use crate::{dial, format, theme};

/// 钟面上 00、15、30、45 分标记的半径。
const MARK_RADIUS: f32 = 210.;
/// 刻度标记一格的宽。
const MARK_WIDTH: f32 = 24.;
/// 圆心下方文字一栏的上边距（相对圆心）。
const READOUT_TOP: f32 = 18.;
/// 圆心下方文字一栏的宽：连同上下 4 的内边距，整栏落在内圈刻度里面（底边距圆心 110，那里刻度内沿的半宽约 96），
/// 垫着的底色不挡刻度。
const READOUT_WIDTH: f32 = 168.;
/// 最近一处缺片或另起会话的说明离外圈的距离。
const NOTE_GAP: f32 = 14.;
/// 说明的最大宽，超出的截断；说明整个留在钟面区域里，不伸进两侧的栏。
const NOTE_MAX_WIDTH: f32 = 150.;

/// 钟面：刻度、标记、圆心下方的状态与已录时长、最近一处缺片或另起会话的说明。
pub(super) fn figure(row: &Row, look: &Look) -> Div {
    let recorded = row.recorded();
    let recording = row.task.status == Status::Running;
    let painted = row.timeline.clone();
    let half = AREA / 2.;
    let marks = live::dial(MARK_RADIUS).map(|(text, x, y)| {
        div()
            .absolute()
            .left(px(half + x - MARK_WIDTH / 2.))
            .top(px(half + y - CAPTION_LINE / 2.))
            .w(px(MARK_WIDTH))
            .text_center()
            .child(widgets::caption(text))
    });
    // 第三行：录制中写当前的会话；停着的写状态的说明（失败的原因、放弃的情况；会话在概况里）
    let third = if recording {
        match row.timeline.session_at(recorded) {
            Some((start, session)) if Some(start) == row.timeline.since => {
                format!("会话 {}", session + 1)
            }
            Some((start, session)) => {
                format!("会话 {} · 自 {}", session + 1, format::clock(start))
            }
            None => String::new(),
        }
    } else {
        look.line.clone()
    };
    dial::face()
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let face = Face {
                        recorded,
                        timeline: &painted,
                        recording,
                    };
                    live::paint(bounds, &face, window);
                },
            )
            .absolute()
            .size_full(),
        )
        .children(marks)
        .child(
            div()
                .absolute()
                .left(px(half - READOUT_WIDTH / 2.))
                .top(px(half + READOUT_TOP))
                .w(px(READOUT_WIDTH))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(4.))
                // 与窗口同色的底：指针扫到下半圈时从字后面过，不压字
                .py(px(4.))
                .rounded(px(12.))
                .bg(theme::bg())
                .child(
                    div()
                        .text_size(px(12.))
                        .line_height(px(CAPTION_LINE))
                        .text_color(look.color)
                        .child(look.state.clone()),
                )
                .child(
                    div()
                        .text_size(px(36.))
                        .line_height(px(40.))
                        .font_weight(FontWeight::LIGHT)
                        .child(look.big.clone()),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(px(12.))
                        .line_height(px(CAPTION_LINE))
                        .text_color(theme::ink_2())
                        .text_center()
                        .truncate()
                        .child(SharedString::from(third)),
                ),
        )
        .children(note(&row.timeline, recorded))
}

/// 钟面上画着的两圈里最近一处缺片或另起会话的说明（同一位置的合成一句），贴在它在外圈上的位置旁边。
fn note(timeline: &live::Timeline, recorded: Duration) -> Option<Div> {
    let (lap, _) = live::lap_of(recorded);
    let mark = timeline
        .marks(live::lap_start(lap.saturating_sub(1)))
        .pop()?;
    let mut parts = vec![format::clock(mark.at)];
    if mark.missed > 0 {
        parts.push(format!("缺 {} 片", mark.missed));
    }
    if let Some(session) = mark.session {
        parts.push(format!("另起会话 {}", session + 1));
    }
    let rad = angle(live::lap_of(mark.at).1);
    let radius = live::OUTER + NOTE_GAP;
    let half = AREA / 2.;
    let (x, y) = (half + radius * rad.cos(), half + radius * rad.sin());
    let pill = div()
        .absolute()
        .top(px(y - TEXT_LINE / 2. - 4.))
        .max_w(px(NOTE_MAX_WIDTH))
        .px(px(8.))
        .py(px(4.))
        .rounded(px(8.))
        .bg(theme::bg())
        .border_1()
        .border_color(theme::alpha(theme::warn(), 0.35))
        .text_size(px(12.))
        .line_height(px(TEXT_LINE))
        .text_color(theme::warn())
        .truncate()
        .child(div().child(parts.join(" · ")));
    // 圆心右边的往右伸、左边的往左伸，都不出钟面区域
    Some(if x >= half {
        pill.left(px(x.min(AREA - NOTE_MAX_WIDTH)))
    } else {
        pill.right(px((AREA - x).min(AREA - NOTE_MAX_WIDTH)))
    })
}

/// 录制的读数：已录、这次打开以来缺失、会话、刷新。
pub(super) fn reads(row: &Row) -> Vec<Stat> {
    let recorded = row.completed().map_or_else(
        || "—".to_owned(),
        |(segments, bytes)| format!("{segments} 片 · {}", format::bytes(bytes)),
    );
    // 刷新只在录制中有意义；停下的不沿用最后一次进度里的
    let progress = row
        .progress
        .as_ref()
        .filter(|_| row.task.status == Status::Running);
    let timeline = &row.timeline;
    // 缺片只知道这次打开应用以来的：还没看到过录制进度时不知道
    let missed = timeline.since.map_or_else(
        || "—".to_owned(),
        |_| match timeline
            .misses
            .iter()
            .map(|&(_, count)| count)
            .sum::<usize>()
        {
            0 => "无".to_owned(),
            n => format!("{n} 片"),
        },
    );
    let session = progress
        .and_then(|p| p.session)
        .or(timeline.sessions.last().map(|&(_, session)| session))
        .map_or_else(
            || "—".to_owned(),
            |session| format!("第 {} 个", session + 1),
        );
    let refresh = progress
        .and_then(|p| p.refresh_errors.iter().flatten().next())
        .map_or_else(
            || if progress.is_some() { "正常" } else { "—" }.to_owned(),
            |reason| format!("失败：{reason}"),
        );
    vec![
        Stat::new("已录", recorded),
        Stat::new("这次打开以来缺失", missed),
        Stat::new("会话", session),
        Stat::text("刷新", refresh),
    ]
}

/// 钟面的图例：各会话的颜色、这次打开以前录的与缺片。
pub(super) fn legend(row: &Row) -> Div {
    let timeline = &row.timeline;
    // 只有一个会话时不提会话；有几个时按钟面上交替的颜色列出各自的会话，每种最多列三个
    let colors = live::session_colors();
    let mut sessions_legend: Vec<(Hsla, String)> = match timeline.sessions.as_slice() {
        [] | [_] => vec![(colors[0], "录到的".to_owned())],
        all => (0..colors.len())
            .map(|k| {
                let numbers: Vec<String> = all
                    .iter()
                    .skip(k)
                    .step_by(colors.len())
                    .map(|&(_, session)| (session + 1).to_string())
                    .collect();
                let more = if numbers.len() > 3 { "…" } else { "" };
                let shown = numbers[..numbers.len().min(3)].join("、");
                (colors[k], format!("会话 {shown}{more}"))
            })
            .collect(),
    };
    // 与钟面画暗色格的条件一致：录到的里有不知道会话的部分
    let unknown_before = timeline
        .sessions
        .first()
        .map_or(Duration::MAX, |&(start, _)| start);
    if row.recorded() > Duration::ZERO && unknown_before > Duration::ZERO {
        sessions_legend.push((live::before_color(), "这次打开以前录的".to_owned()));
    }
    let legend = |color: Hsla, half: f32, text: String| {
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .text_size(px(12.))
            .line_height(px(CAPTION_LINE))
            .text_color(theme::ink_2())
            .child(div().w(px(2.)).h(px(2. * half)).rounded(px(1.)).bg(color))
            .child(text)
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(div().pb(px(2.)).child(widgets::caption("钟面")))
        .children(
            sessions_legend
                .into_iter()
                .map(|(color, text)| legend(color, live::HALF, text)),
        )
        .child(legend(
            theme::warn(),
            live::MISS_HALF,
            "缺片（取不到或刷新前已滑出窗口）".to_owned(),
        ))
}
