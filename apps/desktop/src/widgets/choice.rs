//! 「选中」的画法与选项。

use gpui_kit::{
    App, ClickEvent, Div, ElementId, Hsla, InteractiveElement as _, ParentElement, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled, Window, div, linear_color_stop,
    linear_gradient, px,
};

use super::CONTROL_RADIUS;
use crate::theme;

/// 选中的那一项朝着圆盘的哪条边（光从那边来）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Toward {
    Left,
    Right,
}

/// 「选中」的画法，全界面只有这一种：朝着圆盘的那条边亮（`color` 的 .75），底为从那条边起的 `color` 渐变（.13 到
/// .03），其余三边是极淡的线。`color` 平常为主色，等用户处理的为警告色，直播录制的为直播色。`element` 是圆角为
/// [`CONTROL_RADIUS`] 的控件；没选中时它带 1 点透明的描边，选中前后大小不变。
pub fn lit<E: ParentElement + Styled>(element: E, toward: Toward, color: Hsla) -> E {
    // 渐变的方向：从亮的那条边往对边（0 度为朝上，顺时针）
    let angle = match toward {
        Toward::Left => 90.,
        Toward::Right => 270.,
    };
    let edge = div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .rounded(px(CONTROL_RADIUS))
        .border_color(theme::alpha(color, 0.75));
    let edge = match toward {
        Toward::Left => edge.border_l_1(),
        Toward::Right => edge.border_r_1(),
    };
    element
        .relative()
        .bg(linear_gradient(
            angle,
            linear_color_stop(theme::alpha(color, 0.13), 0.),
            linear_color_stop(theme::alpha(color, 0.03), 1.),
        ))
        .border_1()
        .border_color(theme::alpha(theme::ink(), 0.07))
        .child(edge)
}

/// 一组里可选的一项（输出格式、代理方式、主题）：选中时按 [`lit`] 画，`toward` 为圆盘所在的一边。
pub fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    toward: Toward,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let chip = div()
        .id(id)
        .h(px(30.))
        .px(px(12.))
        .flex()
        .flex_none()
        .items_center()
        .rounded(px(CONTROL_RADIUS))
        .text_size(px(12.5))
        .cursor_pointer()
        .on_click(on_click)
        .child(label.into());
    if selected {
        lit(chip, toward, theme::accent()).text_color(theme::ink())
    } else {
        chip.border_1()
            .border_color(theme::outline())
            .text_color(theme::ink_2())
            .hover(|s| s.bg(theme::wash()))
    }
}
