//! 输入框、只读的一格、小标题与键帽。

use gpui_kit::component::input::InputState;
use gpui_kit::{App, Div, Entity, ParentElement as _, SharedString, Styled as _, div, px};

use super::{CAPTION_LINE, CONTROL_RADIUS, KEY_RADIUS, mono};
use crate::theme;

/// 输入框与只读的一格的高。
const FIELD_HEIGHT: f32 = 36.;

/// 一组东西上面的小标题：11 号、最次一级的字。
pub fn caption(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.))
        .line_height(px(CAPTION_LINE))
        .text_color(theme::ink_3())
        .child(text.into())
}

/// 输入框的外框（里面放 `Input`，或只读的字，见 [`field`]）：比窗口底色凹下去的底、细边。
pub fn input_frame() -> Div {
    div()
        .min_w_0()
        .h(px(FIELD_HEIGHT))
        .px(px(12.))
        .flex()
        .items_center()
        .rounded(px(CONTROL_RADIUS))
        .bg(theme::sunken())
        .border_1()
        .border_color(theme::rule())
        .text_size(px(12.5))
}

/// 只读的一格（保存位置、代理地址）：与输入框同样的外框，等宽字，放不下时截断。
pub fn field(text: impl Into<SharedString>) -> Div {
    input_frame()
        .text_color(theme::ink_2())
        .child(mono(div().min_w_0().truncate()).child(text.into()))
}

/// 键帽：按哪个键（⌘V、↵、Esc）。
pub fn kbd(key: impl Into<SharedString>) -> Div {
    mono(div())
        .flex_none()
        .px(px(6.))
        .py(px(2.))
        .rounded(px(KEY_RADIUS))
        .border_1()
        .border_color(theme::outline())
        .text_size(px(11.))
        .text_color(theme::ink_2())
        .child(key.into())
}

/// 粘贴会换掉 `input` 里的全部内容（空着或全选了）：只在这时把粘贴的 curl 命令当作整条命令接走，否则照常插入。
pub fn pastes_over(input: &Entity<InputState>, cx: &App) -> bool {
    let input = input.read(cx);
    input.selected_text().len() == input.value().len()
}
