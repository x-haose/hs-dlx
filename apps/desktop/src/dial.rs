//! 中央表盘的版式：撑满 [`AREA`] 的刻度（任务的分片、直播钟面、探测的彗尾）、深色圆盘与圆盘里的字（[`readout`]），
//! 以及彗尾转动的快慢。任务页、待机页与新建页（探测中与失败）用；嗅探页找流中的小表盘只用彗尾。

use std::time::Duration;

use gpui_kit::{
    AnyElement, Div, FontWeight, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _,
    canvas, div, px,
};

use hs_dlx_scene::Mode;

use crate::ring::{self, AREA};
use crate::{backdrop, theme};

/// 圆盘离绘制区域边缘多远。
const DISC_INSET: f32 = 70.;
/// 圆盘里的字离圆盘边多远。
const DISC_PADDING: f32 = 24.;
/// 探测、找流时彗尾（[`ring::paint_comet`]）转一圈要多久。
pub const TURN: Duration = Duration::from_secs(4);
/// 待机时彗尾扫一圈要多久：慢到只是在动。
pub const STANDBY_TURN: Duration = Duration::from_secs(20);

/// 彗尾转一圈要 `turn` 时，一帧的间隔：彗尾的位置按格取整（一圈 [`ring::COMET_TICKS`] 格），一帧正好前进一格，更密的
/// 帧画出来也一样。
pub const fn frame(turn: Duration) -> Duration {
    Duration::from_micros((turn.as_micros() / ring::COMET_TICKS as u128) as u64)
}

/// 彗尾此刻转到哪里（整圈为 1）：转一圈要 `turn`，已转了 `elapsed`。
pub fn head(elapsed: Duration, turn: Duration) -> f32 {
    elapsed.as_secs_f32() / turn.as_secs_f32()
}

/// 圆盘里一行说明最宽多少：两行时仍在圆盘里。
pub const TEXT_WIDTH: f32 = 240.;
/// 大字的字号：数字（百分比、时长、状态码）与一句话（「粘贴链接开始」、主机名）。
const FIGURE_SIZE: f32 = 68.;
const PHRASE_SIZE: f32 = 26.;

/// 圆盘的半径：窗口背景的日冕从这圈边上放出（见 [`crate::backdrop`]）。
pub const DISC_RADIUS: f32 = AREA / 2. - DISC_INSET;

/// 圆盘里的大字。
pub enum Big {
    /// 数字：百分比、时长、状态码
    Figure(String),
    /// 一句话或一个名字
    Phrase(String),
}

/// 表盘：`figure` 为撑满整块区域的刻度，圆盘压在中间，`center` 放在圆盘里。圆心与 `mode`（窗口背景的日冕代表的状态）
/// 报给窗口背景（[`backdrop::report_disc`]），日冕从圆盘边上放出。
pub fn dial(mode: Mode, figure: AnyElement, center: impl IntoElement) -> Div {
    let disc = AREA - 2. * DISC_INSET;
    let place = || {
        div()
            .absolute()
            .top(px(DISC_INSET))
            .left(px(DISC_INSET))
            .size(px(disc))
    };
    face()
        .child(figure)
        .child(
            place()
                .rounded_full()
                .bg(theme::moon())
                .border_1()
                .border_color(theme::rule())
                .child(
                    canvas(
                        move |bounds, _, cx| {
                            backdrop::report_disc(
                                backdrop::Disc {
                                    center: bounds.center(),
                                    radius: DISC_RADIUS,
                                    mode,
                                },
                                cx,
                            );
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                ),
        )
        .child(
            place()
                .px(px(DISC_PADDING))
                .flex()
                .items_center()
                .justify_center()
                .child(center),
        )
}

/// 表盘占的一块：边长 [`AREA`]，刻度与圆盘叠在里面（直播钟面没有圆盘，指针扫过圆心）。
pub fn face() -> Div {
    div().relative().size(px(AREA)).flex_none()
}

/// 撑满表盘的刻度画布：`paint` 在整块区域上画。
pub fn figure(
    paint: impl FnOnce(gpui_kit::Bounds<gpui_kit::Pixels>, &mut gpui_kit::Window, &mut gpui_kit::App)
    + 'static,
) -> AnyElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| paint(bounds, window, cx),
    )
    .absolute()
    .size_full()
    .into_any_element()
}

/// 圆盘里的字：`state`（颜色 `color`）、大字 `big` 与下面最多两行的说明 `line`。
pub fn readout(
    state: impl Into<SharedString>,
    color: Hsla,
    big: Big,
    line: impl Into<SharedString>,
) -> Div {
    let big = match big {
        Big::Figure(text) => div()
            .text_size(px(FIGURE_SIZE))
            .line_height(px(FIGURE_SIZE))
            .font_weight(FontWeight::LIGHT)
            .child(text),
        Big::Phrase(text) => div()
            .max_w(px(TEXT_WIDTH))
            .text_size(px(PHRASE_SIZE))
            .font_weight(FontWeight::MEDIUM)
            .truncate()
            .child(text),
    };
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(10.))
        .text_center()
        .child(
            div()
                .text_size(px(12.))
                .text_color(color)
                .child(state.into()),
        )
        .child(big)
        .child(
            div()
                .max_w(px(TEXT_WIDTH))
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(theme::ink_2())
                .line_clamp(2)
                .child(line.into()),
        )
}
