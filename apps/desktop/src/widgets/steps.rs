//! 步骤条：一条任务从粘贴到完成走的各步，横排，放不下时换行。

use gpui_kit::{Div, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _, div, px};

use crate::theme;

/// 一步走到哪了。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 走过了
    Done,
    /// 正在走
    Now,
    /// 停在这一步：没成，或等用户
    Stopped,
    /// 还没到
    Waiting,
}

/// 各步按先后排开，之间一小段线。正在走的一步为 `now`（平常为主色，直播录制时为直播色），停下的为警告色。
pub fn steps(items: impl IntoIterator<Item = (SharedString, Step)>, now: Hsla) -> Div {
    let mut row = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(10.))
        .gap_y(px(6.))
        .text_size(px(12.));
    for (index, (text, step)) in items.into_iter().enumerate() {
        if index > 0 {
            row = row.child(
                div()
                    .w(px(18.))
                    .h(px(1.))
                    .bg(theme::alpha(theme::ink(), 0.18)),
            );
        }
        let (dot, color, ring) = match step {
            Step::Done => (theme::ink_2(), theme::ink_2(), false),
            Step::Now => (now, now, true),
            Step::Stopped => (theme::warn(), theme::warn(), true),
            Step::Waiting => (theme::alpha(theme::ink(), 0.3), theme::ink_3(), false),
        };
        row = row.child(
            div()
                .flex()
                .items_center()
                // 圆点占 12 宽、两边各空 3：离字 6
                .gap(px(3.))
                .whitespace_nowrap()
                .text_color(color)
                .child(marker(dot, ring))
                .child(text),
        );
    }
    row
}

/// 一步前面的圆点：直径 6；正在走与停下的外面多一圈同色的淡环（实边，不是光）。
fn marker(color: Hsla, ring: bool) -> impl IntoElement {
    let dot = div().size(px(6.)).rounded_full().bg(color);
    if ring {
        div()
            .size(px(12.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(theme::alpha(color, 0.22))
            .child(dot)
    } else {
        div()
            .size(px(12.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .child(dot)
    }
}
