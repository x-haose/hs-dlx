//! 文字与小件：窗口根的排字、等宽字、说明、出错的一行与说明、「复制」、圆点、检测的一项。

use std::sync::Arc;

use gpui_kit::{
    ClipboardItem, Div, ElementId, FontFallbacks, FontFeatures, Hsla, InteractiveElement as _,
    ParentElement as _, SharedString, Stateful, StatefulInteractiveElement as _, Styled, div, px,
};

use super::{MONO, TEXT_LINE};
use crate::theme;

/// 窗口的根：字体（[`super::SANS`]）里没有的字（中文）落到系统界面字体；数字一律用等宽字形，跳动时不左右晃。
pub fn typeset<E: Styled>(mut root: E) -> E {
    root.text_style().font_fallbacks =
        Some(FontFallbacks::from_fonts(vec![".SystemUIFont".to_owned()]));
    root.font_family(super::SANS)
        .font_features(FontFeatures(Arc::new(vec![("tnum".into(), 1)])))
}

/// 用等宽字（地址、路径、时刻）；同一段里的中文落到系统字体。
pub fn mono<E: Styled>(element: E) -> E {
    element.font_family(MONO)
}

/// 一段说明；`warn` 时为要处理的，用警告色。
pub fn note(text: impl Into<SharedString>, warn: bool) -> Div {
    div()
        .text_size(px(13.))
        .line_height(px(TEXT_LINE))
        .text_color(if warn { theme::warn() } else { theme::ink_2() })
        .child(text.into())
}

/// 要用户留意的一行字（操作没成、输入不对）。
pub fn error_text(text: impl Into<SharedString>) -> Div {
    note(text, true)
}

/// 出错的说明：警告色，右边有「复制」（见 [`copy_link`]）。
pub fn error_note(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Div {
    let text: SharedString = text.into();
    let copy = copy_link(id, text.to_string());
    div()
        .flex()
        .items_start()
        .gap(px(12.))
        .child(note(text, true).flex_1().min_w_0())
        .child(copy)
}

/// 小字的「复制」：点了把 `text` 原样放进剪贴板。界面上的文字选不中，报告问题时靠它取出完整的说明。
pub fn copy_link(id: impl Into<ElementId>, text: String) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .text_size(px(12.))
        .text_color(theme::ink_3())
        .cursor_pointer()
        .hover(|s| s.text_color(theme::ink()))
        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
        .child("复制")
}

/// 实心的小圆点，`size` 为直径。
pub fn dot(color: Hsla, size: f32) -> Div {
    div().size(px(size)).flex_none().rounded_full().bg(color)
}

/// 检测或诊断的一项：按结论着色的圆点与说明。
pub fn check_line(level: hs_dlx_app::CheckLevel, text: &str) -> Div {
    let color = match level {
        hs_dlx_app::CheckLevel::Ok => theme::accent(),
        hs_dlx_app::CheckLevel::Warn => theme::alpha(theme::warn(), 0.55),
        hs_dlx_app::CheckLevel::Bad => theme::warn(),
    };
    div()
        .flex()
        .items_start()
        .gap(px(10.))
        .child(div().pt(px(7.)).child(dot(color, 6.)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(13.))
                .line_height(px(TEXT_LINE))
                .child(text.to_owned()),
        )
}
