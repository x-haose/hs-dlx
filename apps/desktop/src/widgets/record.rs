//! 信号的时间线：一行一条，左边是时刻，右边是一句话。

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Div, ElementId, ParentElement as _, SharedString, Styled as _, div, px};

use super::{copy_link, mono};
use crate::theme;

/// 时间线的一行。
pub struct Line {
    /// 时刻（时:分:秒）
    pub at: SharedString,
    pub text: SharedString,
    /// 连续发生了几次；1 时不写
    pub repeat: usize,
    /// 要留意：整行警告色，右边有「复制」（报告问题时要完整的原文）
    pub warn: bool,
}

/// 各行按给的顺序（新的在前）排下来。`id` 区分同一窗口里的几条时间线（「复制」要各自的 id）。
pub fn record(id: &'static str, lines: impl IntoIterator<Item = Line>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .text_size(px(12.))
        .children(lines.into_iter().enumerate().map(|(index, line)| {
            let copy = line.warn.then(|| {
                copy_link(
                    ElementId::NamedInteger(id.into(), index as u64),
                    line.text.to_string(),
                )
            });
            div()
                .flex()
                .items_start()
                .gap(px(12.))
                .child(
                    mono(div())
                        .flex_none()
                        .w(px(60.))
                        .pt(px(1.))
                        .text_size(px(11.))
                        .text_color(theme::ink_3())
                        .child(line.at),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(if line.warn {
                            theme::warn()
                        } else {
                            theme::ink_2()
                        })
                        .child(line.text),
                )
                .when(line.repeat > 1, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_color(theme::ink_3())
                            .child(format!("×{}", line.repeat)),
                    )
                })
                .children(copy)
        }))
}
