//! 界面里反复用到的尺寸与小件：面板、读数格、主操作、按钮、选项、卡片、步骤、列表项、小标题、等宽数字、发光。
//!
//! 版式：面板（[`deck`]）收住空白；当前对象的操作在检查面板（[`inspector`]）的底栏，主操作（[`primary`]）在前；
//! 整页面板的操作在它标题栏（[`page_head`]）的右侧。

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Bounds, BoxShadow, ClickEvent, ClipboardItem, Div, ElementId, FontFallbacks,
    FontFeatures, FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled, Window, canvas, div, fill,
    linear_color_stop, linear_gradient, point, px, relative,
};

use crate::{backdrop, motion, theme};

/// 顶栏高度；macOS 的红绿灯按它垂直居中，右上角的提示从它下面开始。
pub const TITLE_HEIGHT: f32 = 52.;
/// 主体左右与底部的内边距。
pub const PAD: f32 = 14.;
/// 各栏之间的间距。
pub const GAP: f32 = 14.;
/// 面板的圆角。
pub const DECK_RADIUS: f32 = 20.;
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
/// 按钮的高度。
pub const BUTTON_HEIGHT: f32 = 40.;
/// 输入框与只读的一格（[`field`]）的高度。
pub const FIELD_HEIGHT: f32 = 38.;
/// 选项（[`chip`]）的高度。
pub const CHIP_HEIGHT: f32 = 34.;
/// 小标题（12 号字）的行高。
pub const CAPTION_LINE: f32 = 18.;
/// 正文（13 号字）的行高。
pub const TEXT_LINE: f32 = 20.;
/// 两个尺寸里大的一个（常量表达式里用）。
pub const fn max(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}

/// 按钮的样子。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// 描边，深色底
    Plain,
    /// 发光的主色实心：每屏最该按的那一个（在检查面板里时用主操作 [`primary`]）
    Glow,
    /// 只有文字：退出当前流程
    Ghost,
}

/// 主操作（[`primary`]）的颜色。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Solid {
    /// 主色：每屏最该按的那一个
    Glow,
    /// 警告色：要处理时最该按的那一个
    Amber,
    /// 直播色：停止录制
    Rec,
}

impl Solid {
    fn color(self) -> Hsla {
        match self {
            Solid::Glow => theme::accent(),
            Solid::Amber => theme::warn(),
            Solid::Rec => theme::live(),
        }
    }
}

/// 主操作底部那团内光呼吸一次的时长。
const PRIMARY_BREATH: Duration = Duration::from_millis(3200);

/// 按钮。
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: Tone,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .h(px(BUTTON_HEIGHT))
        .px(px(18.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(12.))
        .text_size(px(14.))
        .cursor_pointer()
        .on_click(on_click)
        .child(label.into());
    if tone == Tone::Glow {
        return base
            .bg(theme::accent())
            .text_color(theme::on_accent())
            .font_weight(FontWeight::BOLD)
            .shadow(glow(theme::accent(), 18., 0.3))
            .hover(|s| s.bg(theme::accent_pale()));
    }
    match tone {
        Tone::Ghost => base
            .text_color(theme::muted())
            .hover(|s| s.text_color(theme::text()).bg(theme::selected())),
        _ => base
            .border_1()
            .border_color(theme::edge())
            .bg(theme::raised())
            .text_color(theme::text())
            .hover(|s| s.border_color(theme::hover_edge()).bg(theme::selected())),
    }
}

/// 当前对象的主操作：整宽、两行（做什么与它的后果，如「开始下载 · 1080p」「约 1.9 GB · MP4」），放在检查面板底栏的
/// 最前面。深色底、`solid` 色的描边与外光，底部一团同色的内光缓缓呼吸（[`motion::breathe`]）。`enabled` 为 false
/// 时变淡、不可点。
pub fn primary(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    solid: Solid,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let color = solid.color();
    let breath = motion::breathe(PRIMARY_BREATH);
    // 内光：底边中间一团，大部分在按钮下面、被按钮裁掉，只露出往上漫的一截
    let inner = div()
        .absolute()
        .left(relative(0.1))
        .top(relative(0.3))
        .w(relative(0.8))
        .h(relative(1.1))
        .rounded_full()
        .shadow(glow(color, 34., 0.7 * breath));
    let base = div()
        .id(id)
        .relative()
        .overflow_hidden()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(2.))
        .px(px(18.))
        .py(px(11.))
        .rounded(px(16.))
        .bg(theme::deck())
        .border_1()
        .border_color(theme::alpha(color, 0.45))
        .text_color(gpui_kit::white())
        .shadow(glow(color, 30., 0.25))
        .child(inner)
        .child(
            div()
                .relative()
                .text_size(px(15.))
                .font_weight(FontWeight::BOLD)
                .child(title.into()),
        )
        .children(detail.map(|detail| {
            num(div()
                .relative()
                .text_size(px(12.))
                .opacity(0.7)
                .child(detail))
        }));
    if enabled {
        base.cursor_pointer()
            .on_click(on_click)
            .hover(move |s| s.border_color(theme::alpha(color, 0.8)))
    } else {
        base.opacity(0.55)
    }
}

/// 可选中的胶囊：选中时实心浅色。
pub fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .h(px(CHIP_HEIGHT))
        .px(px(13.))
        .flex()
        .items_center()
        .rounded(px(10.))
        .border_1()
        .text_size(px(13.))
        .cursor_pointer()
        .on_click(on_click)
        .child(label.into());
    if selected {
        base.bg(theme::text())
            .border_color(theme::text())
            .text_color(theme::ground())
            .font_weight(FontWeight::SEMIBOLD)
    } else {
        base.border_color(theme::edge())
            .text_color(theme::soft())
            .hover(|s| s.border_color(theme::hover_edge()))
    }
}

/// 分组的小标题。
pub fn caption(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.))
        .line_height(px(CAPTION_LINE))
        .text_color(theme::faint())
        .child(text.into())
}

/// 面板：任务栏、检查面板与整页面板的外框（细边、大圆角）；底见 [`backdrop::panel_floor`]。
pub fn deck() -> Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .min_h_0()
        .overflow_hidden()
        .rounded(px(DECK_RADIUS))
        .border_1()
        .border_color(theme::deck_edge())
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
    deck()
        .flex_basis(px(INSPECTOR_WIDTH))
        .flex_shrink(1.)
        .min_w(px(INSPECTOR_MIN_WIDTH))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(5.))
                .px(px(22.))
                .pt(px(22.))
                .pb(px(18.))
                .border_b_1()
                .border_color(theme::hairline())
                .child(
                    div()
                        .text_size(px(19.))
                        .line_height(px(25.))
                        .font_weight(FontWeight::BOLD)
                        .line_clamp(2)
                        .child(title.into()),
                )
                .child(
                    num(div()
                        .text_size(px(13.))
                        .text_color(theme::muted())
                        .truncate())
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
        )
        .when(!foot.is_empty(), |inspector| {
            inspector.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .px(px(16.))
                    .pt(px(14.))
                    .pb(px(16.))
                    .border_t_1()
                    .border_color(theme::hairline())
                    .children(foot),
            )
        })
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
        .border_color(theme::hairline())
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
                        .font_weight(FontWeight::BOLD)
                        .truncate()
                        .child(title.into()),
                )
                .children(sub.map(|sub| {
                    num(div()
                        .text_size(px(13.))
                        .text_color(theme::muted())
                        .truncate())
                    .child(sub)
                })),
        )
        .children(actions)
}

/// 读数格：两列，每格上面是小标签、下面是读数（等宽数字；读数为文字时如「直连」照样放）。`wide` 为占满一行的一格
/// （长的读数，如代理地址），放在最后；`cells` 为奇数个时最后一格也占满一行，不留空格。
pub fn reads(
    cells: Vec<(SharedString, SharedString)>,
    wide: Option<(SharedString, SharedString)>,
) -> Div {
    let odd = cells.len() % 2 == 1;
    let last = cells.len().saturating_sub(1);
    let cell = |label: SharedString, value: SharedString, span: bool, size: f32| {
        div()
            .min_w_0()
            .when(span, |cell| cell.col_span(2))
            .flex()
            .flex_col()
            .gap(px(3.))
            .px(px(14.))
            .py(px(11.))
            .bg(theme::cell())
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme::faint())
                    .child(label),
            )
            .child(num(div().text_size(px(size)).truncate()).child(value))
    };
    div()
        .grid()
        .grid_cols(2)
        .gap(px(1.))
        .rounded(px(14.))
        .overflow_hidden()
        .bg(theme::deck_edge())
        .border_1()
        .border_color(theme::deck_edge())
        .children(
            cells
                .into_iter()
                .enumerate()
                .map(|(i, (label, value))| cell(label, value, odd && i == last, 16.)),
        )
        .children(wide.map(|(label, value)| cell(label, value, true, 14.)))
}

/// 一步的状态。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Done,
    /// 正在做
    Now,
    /// 在这一步没成
    Failed,
    Waiting,
}

/// 按先后的几步：做完的实心点，正在做的带光圈，没成的为警告色。
pub fn steps(items: impl IntoIterator<Item = (SharedString, Step)>) -> Div {
    div()
        .flex()
        .flex_col()
        .children(items.into_iter().map(|(text, step)| {
            let marker = div().size(px(10.)).flex_none().rounded_full().border_2();
            let marker = match step {
                Step::Done => marker.bg(theme::accent()).border_color(theme::accent()),
                Step::Now => {
                    marker
                        .border_color(theme::accent())
                        .shadow(glow(theme::accent(), 8., 0.5))
                }
                Step::Failed => {
                    marker
                        .border_color(theme::warn())
                        .shadow(glow(theme::warn(), 8., 0.5))
                }
                Step::Waiting => marker.border_color(theme::edge()),
            };
            let color = match step {
                Step::Done => theme::soft(),
                Step::Now => theme::text(),
                Step::Failed => theme::warn(),
                Step::Waiting => theme::muted(),
            };
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .py(px(6.))
                .child(div().w(px(14.)).flex().justify_center().child(marker))
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(color)
                        .when(matches!(step, Step::Now | Step::Failed), |s| {
                            s.font_weight(FontWeight::SEMIBOLD)
                        })
                        .child(text),
                )
        }))
}

/// 粘贴会换掉 `input` 里的全部内容（空着或全选了）：只在这时把粘贴的 curl 命令当作整条命令接走，否则照常插入。
pub fn pastes_over(
    input: &gpui_kit::Entity<gpui_kit::component::input::InputState>,
    cx: &gpui_kit::App,
) -> bool {
    let input = input.read(cx);
    input.selected_text().len() == input.value().len()
}

/// 输入框的外框（里面放 `Input`，或只读的字，见 [`field`]）：比面板暗一层的底、细边。
pub fn input_frame() -> Div {
    div()
        .min_w_0()
        .h(px(FIELD_HEIGHT))
        .px(px(12.))
        .flex()
        .items_center()
        .rounded(px(10.))
        .bg(theme::ground())
        .border_1()
        .border_color(theme::ring_idle())
        .text_size(px(13.))
}

/// 只读的一格（如保存位置）：与输入框同样的外观，放不下时截断。
pub fn field(text: impl Into<SharedString>) -> Div {
    input_frame()
        .text_color(theme::soft())
        .child(num(div().min_w_0().truncate()).child(text.into()))
}

/// 面板里抬起的一块（读数格之外的卡片：说明、流、网页要做的事）：读数格的底色、细边、圆角。
pub fn well() -> Div {
    div()
        .rounded(px(14.))
        .bg(theme::cell())
        .border_1()
        .border_color(theme::deck_edge())
}

/// 面板里的一段说明：`warn` 时为要处理的（警告色描边、暖色底）。
pub fn note(text: impl Into<SharedString>, warn: bool) -> Div {
    let note = well()
        .px(px(14.))
        .py(px(12.))
        .text_size(px(13.))
        .line_height(px(20.))
        .text_color(theme::soft())
        .child(text.into());
    if warn {
        note.border_color(theme::warn_edge())
            .bg(theme::warn_ground())
    } else {
        note
    }
}

/// 出错的说明：同 [`note`] 的警示样式，右下角有「复制」（见 [`copy_link`]）。
pub fn error_note(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Div {
    let text: SharedString = text.into();
    let link = copy_link(id, text.to_string());
    note(text, true).child(div().flex().justify_end().mt(px(6.)).child(link))
}

/// 小字的「复制」：点了把 `text` 原样放进剪贴板。界面上的文字选不中，报告问题时靠它取出完整的说明。
pub fn copy_link(id: impl Into<ElementId>, text: String) -> Stateful<Div> {
    div()
        .id(id)
        .text_size(px(12.))
        .text_color(theme::muted())
        .cursor_pointer()
        .hover(|s| s.text_color(theme::text()))
        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
        .child("复制")
}

/// 状态胶囊：发光的小圆点与一句状态（标题栏里用）。
pub fn status(color: Hsla, text: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .h(px(28.))
        .px(px(11.))
        .rounded_full()
        .bg(theme::raised())
        .border_1()
        .border_color(theme::deck_edge())
        .text_size(px(12.))
        .text_color(theme::soft())
        .child(dot(color, 7.))
        .child(text.into())
}

/// 数字的字体（随应用打包，见 assets/fonts）。
pub const NUMBER_FONT: &str = "Space Grotesk";

/// 数字：用 [`NUMBER_FONT`] 的等宽字形，跳动时不左右晃；同一段里的中文落到系统界面字体。
pub fn num<E: Styled>(mut element: E) -> E {
    element.text_style().font_fallbacks =
        Some(FontFallbacks::from_fonts(vec![".SystemUIFont".to_owned()]));
    element
        .font_family(NUMBER_FONT)
        .font_features(FontFeatures(Arc::new(vec![("tnum".into(), 1)])))
}

/// `color` 的外发光：模糊半径 `blur`、透明度 `strength`（0 到 1）。
pub fn glow(color: Hsla, blur: f32, strength: f32) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: theme::alpha(color, strength),
        offset: point(px(0.), px(0.)),
        blur_radius: px(blur),
        spread_radius: px(0.),
        inset: false,
    }]
}

/// 点阵速度表的高与每列几个点。
const MATRIX_HEIGHT: f32 = 78.;
const MATRIX_ROWS: usize = 9;

/// 点阵速度表：每列一格速度（`speeds`，旧的在左），按其中最大的一格归一，从下往上点亮；每列最上面亮的那个点最亮、
/// 带光晕。全为 0 时一个也不亮。
pub fn speed_matrix(speeds: Vec<f64>) -> impl IntoElement {
    let peak = speeds.iter().copied().fold(0., f64::max);
    let accent = theme::accent();
    let off = theme::alpha(theme::muted(), 0.1);
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            let columns = speeds.len();
            if columns == 0 {
                return;
            }
            let cell_w = f32::from(bounds.size.width) / columns as f32;
            let cell_h = f32::from(bounds.size.height) / MATRIX_ROWS as f32;
            let diameter = cell_w.min(cell_h) * 0.64;
            for (column, speed) in speeds.iter().enumerate() {
                let lit = if peak > 0. && *speed > 0. {
                    ((speed / peak * MATRIX_ROWS as f64).round() as usize).clamp(1, MATRIX_ROWS)
                } else {
                    0
                };
                for row in 0..MATRIX_ROWS {
                    let from_bottom = MATRIX_ROWS - row;
                    let center = point(
                        bounds.origin.x + px((column as f32 + 0.5) * cell_w),
                        bounds.origin.y + px((row as f32 + 0.5) * cell_h),
                    );
                    let mut paint = |size: f32, color: Hsla| {
                        let half = px(size / 2.);
                        window.paint_quad(
                            fill(
                                Bounds::new(
                                    point(center.x - half, center.y - half),
                                    gpui_kit::size(half * 2., half * 2.),
                                ),
                                color,
                            )
                            .corner_radii(half),
                        );
                    };
                    if from_bottom > lit {
                        paint(diameter, off);
                    } else if from_bottom == lit {
                        paint(diameter * 2.2, theme::alpha(accent, 0.22));
                        paint(diameter, accent);
                    } else {
                        paint(diameter, theme::alpha(accent, 0.55));
                    }
                }
            }
        },
    )
    .w_full()
    .h(px(MATRIX_HEIGHT))
}

/// 发光的小圆点，`size` 为直径。
pub fn dot(color: Hsla, size: f32) -> Div {
    div()
        .size(px(size))
        .flex_none()
        .rounded_full()
        .bg(color)
        .shadow(glow(color, size, 0.9))
}

/// 卡片：面板的底色、圆角，描边 `edge` 表示级别（普通、要留意）；浮在内容上的提示用。
pub fn card(edge: Hsla) -> Div {
    div()
        .px(px(14.))
        .py(px(12.))
        .rounded(px(14.))
        .bg(theme::deck())
        .shadow(vec![BoxShadow {
            color: theme::alpha(gpui_kit::black(), 0.5),
            offset: point(px(0.), px(12.)),
            blur_radius: px(32.),
            spread_radius: px(0.),
            inset: false,
        }])
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
        // 表盘的背光不画出舞台
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

/// 任务栏的一项：横排；选中时有自上而下变淡的主色底、主色描边与一圈淡光，否则悬停时有底色。
pub fn list_item(id: impl Into<ElementId>, selected: bool) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(12.))
        .p(px(10.))
        .rounded(px(14.))
        .border_1();
    if selected {
        let accent = theme::accent();
        base.border_color(theme::alpha(accent, 0.28))
            .bg(linear_gradient(
                180.,
                linear_color_stop(theme::alpha(accent, 0.10), 0.),
                linear_color_stop(theme::alpha(accent, 0.03), 1.),
            ))
            .shadow(glow(accent, 24., 0.10))
    } else {
        base.border_color(gpui_kit::transparent_black())
            .hover(|s| s.bg(theme::raised()))
    }
}

/// 要用户留意的一行字（操作没成、输入不对）。
pub fn error_text(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(13.))
        .line_height(px(TEXT_LINE))
        .text_color(theme::warn())
        .child(text.into())
}

/// 两个同心实线环（外环宽 `width`、亮色，内环小 `inset`、细而暗），边长 `size`：表示一个可以探测出几档清晰度的
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
        .child(ring(size - inset, 2., theme::accent_dim()))
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
        .child(div().pt(px(6.)).child(dot(color, 7.)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(13.))
                .line_height(px(TEXT_LINE))
                .child(text.to_owned()),
        )
}
