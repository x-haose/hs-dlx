//! 新建页、嗅探页、任务页与设置页共用的骨架：面板、检查面板、整页面板的标题栏、舞台、列表项、浮在内容上的提示、
//! 面板里的一块。基础部件在 [`crate::widgets`]。
//!
//! 版式：面板（[`deck`]）收住空白；当前对象的操作在检查面板（[`inspector`]）的底栏，主操作在前；整页面板的操作在它
//! 标题栏（[`page_head`]）的右侧。

use gpui_kit::{
    AnyElement, Div, ElementId, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled, div, px,
};

use crate::widgets::{self, CONTAINER_RADIUS, CONTROL_RADIUS, Toward};
use crate::{backdrop, theme};

/// 顶栏高度；macOS 的红绿灯按它垂直居中，右上角的提示从它下面开始。
pub const TITLE_HEIGHT: f32 = 52.;
/// 主体左右与底部的内边距。
pub const PAD: f32 = 14.;
/// 各栏之间的间距。
pub const GAP: f32 = 14.;
/// 检查面板的宽。
pub const INSPECTOR_WIDTH: f32 = 344.;
/// 窗口窄时检查面板最多收到的宽。
pub const INSPECTOR_MIN_WIDTH: f32 = 296.;
/// 粘贴栏的高。
pub const DOCK_HEIGHT: f32 = 52.;
/// 舞台里表盘与粘贴栏的最小间距。
pub const DOCK_GAP: f32 = 18.;
/// 舞台里表盘下面各行（提示、按键说明、粘贴栏）之间的间距。
const STAGE_GAP: f32 = 12.;
/// 粘贴栏离舞台底边多远。
pub const DOCK_BOTTOM: f32 = 4.;
/// 两个尺寸里大的一个（常量表达式里用）。
pub const fn max(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

/// 面板：任务栏、检查面板与整页面板的外框（细边、容器圆角）；底见 [`backdrop::panel_floor`]。
pub fn deck() -> Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .min_h_0()
        .overflow_hidden()
        .rounded(px(CONTAINER_RADIUS))
        .border_1()
        .border_color(theme::rule())
        .child(backdrop::panel_floor())
}

/// 检查面板：标题栏（`title` 与下面一行 `sub`）、可以滚动的正文 `body`、底栏 `foot`（当前对象的操作）。
pub fn inspector(
    title: impl Into<SharedString>,
    sub: impl Into<SharedString>,
    body: impl IntoIterator<Item = AnyElement>,
    foot: impl IntoIterator<Item = AnyElement>,
) -> Div {
    let foot: Vec<AnyElement> = foot.into_iter().collect();
    let panel = deck()
        .flex_basis(px(INSPECTOR_WIDTH))
        .flex_shrink(1.)
        .min_w(px(INSPECTOR_MIN_WIDTH))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .px(px(22.))
                .pt(px(22.))
                .pb(px(18.))
                .border_b_1()
                .border_color(theme::rule())
                .child(
                    div()
                        .text_size(px(20.))
                        .line_height(px(26.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .line_clamp(2)
                        .child(title.into()),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::ink_2())
                        .truncate()
                        .child(sub.into()),
                ),
        )
        .child(
            div()
                .id("inspector-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(20.))
                .px(px(22.))
                .py(px(18.))
                .children(body),
        );
    if foot.is_empty() {
        return panel;
    }
    panel.child(
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(px(16.))
            .pt(px(14.))
            .pb(px(16.))
            .border_t_1()
            .border_color(theme::rule())
            .children(foot),
    )
}

/// 整页面板的标题栏：左边 `lead`（图形）、标题 `title` 与下面一行 `sub`，右边 `actions`（这一页的操作）。
pub fn page_head(
    lead: Option<AnyElement>,
    title: impl Into<SharedString>,
    sub: Option<SharedString>,
    actions: impl IntoIterator<Item = AnyElement>,
) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(14.))
        .pl(px(20.))
        .pr(px(16.))
        .py(px(14.))
        .border_b_1()
        .border_color(theme::rule())
        .children(lead)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(17.))
                        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                        .truncate()
                        .child(title.into()),
                )
                .children(sub.map(|sub| {
                    div()
                        .text_size(px(12.))
                        .text_color(theme::ink_2())
                        .truncate()
                        .child(sub)
                })),
        )
        .children(actions)
}

/// 面板里的一块（请求头的编辑、流、网页要做的事）：容器圆角、细边、抬起一层的底。
pub fn well() -> Div {
    div()
        .rounded(px(CONTAINER_RADIUS))
        .bg(theme::wash())
        .border_1()
        .border_color(theme::rule())
}

/// 状态：小圆点与一句状态（整页面板的标题栏里用）。
pub fn status(color: Hsla, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .text_color(theme::ink_2())
        .child(widgets::dot(color, 6.))
        .child(text.into())
}

/// 浮在内容上的提示：窗口底色、容器圆角，描边 `edge` 表示级别（普通、要留意）。
pub fn card(edge: Hsla) -> Div {
    div()
        .px(px(14.))
        .py(px(12.))
        .rounded(px(CONTAINER_RADIUS))
        .bg(theme::bg())
        .border_1()
        .border_color(edge)
        .text_size(px(13.))
        .line_height(px(19.))
}

/// 舞台：两块面板之间、直接在窗口底色上的一栏。`figure`（表盘、同心环）在剩下的高里居中，下面依次是 `below`（如
/// 剪贴板里的链接、按键说明）与粘贴栏 `dock`。`min_width` 为放得下 `figure` 的宽：窗口窄时先收两侧的面板。
pub fn stage(
    min_width: f32,
    figure: impl IntoElement,
    below: impl IntoIterator<Item = AnyElement>,
    dock: impl IntoElement,
) -> Div {
    div()
        .flex_1()
        .min_w(px(min_width))
        .min_h_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(STAGE_GAP))
        .pb(px(DOCK_BOTTOM))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .pb(px(DOCK_GAP - STAGE_GAP))
                .child(figure),
        )
        .children(below)
        .child(dock)
}

/// 舞台最少要多高：`figure` 高 `figure_height`、下面的 `below` 合计高 `below_height`（没有时为 0）时，加上它们与粘贴栏
/// 之间的间距、粘贴栏与它的下边距。
pub const fn stage_height(figure_height: f32, below_height: f32) -> f32 {
    let below = if below_height > 0. {
        below_height + STAGE_GAP
    } else {
        0.
    };
    figure_height + below + DOCK_GAP + DOCK_HEIGHT + DOCK_BOTTOM
}

/// 任务栏的一项：横排；选中时按 [`widgets::lit`] 画（舞台在任务栏右边），否则悬停时有底色。
pub fn list_item(id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    let item = div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(12.))
        .p(px(10.))
        .rounded(px(CONTROL_RADIUS));
    if selected {
        widgets::lit(item, Toward::Right, theme::accent())
    } else {
        item.border_1()
            .border_color(gpui_kit::transparent_black())
            .hover(|s| s.bg(theme::wash()))
    }
}

/// 两个同心实线环（外环宽 `width`、主色，内环小 `inset`、细而淡），边长 `size`：表示一个可以探测出几档清晰度的
/// 地址，嗅探页里的流与任务列表里回到嗅探页的一项用它。
pub fn rings(size: f32, width: f32, inset: f32) -> Div {
    let ring = |ring: f32, width: f32, color| {
        div()
            .absolute()
            .left(px((size - ring) / 2.))
            .top(px((size - ring) / 2.))
            .size(px(ring))
            .rounded_full()
            .border(px(width))
            .border_color(color)
    };
    div()
        .relative()
        .size(px(size))
        .flex_none()
        .child(ring(size, width, theme::accent()))
        .child(ring(size - inset, 2., theme::alpha(theme::accent(), 0.4)))
}
