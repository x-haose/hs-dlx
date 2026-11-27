//! 按钮：四种（[`Kind`]）、三种高（[`Size`]）。

use gpui_kit::{
    App, ClickEvent, ElementId, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Refineable as _, RenderOnce, SharedString, StatefulInteractiveElement as _,
    StyleRefinement, Styled, Window, div, px,
};

use super::CONTROL_RADIUS;
use crate::theme;

/// 按钮的种类。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 实底主色：这一屏最该按的那一个
    Primary,
    /// 描边
    Secondary,
    /// 只有文字：退出当前流程、次要的去处
    Text,
    /// 警告色描边：要处理时最该按的那一个
    Warn,
}

/// 按钮的高：小 30、常规 36、大 48。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Small,
    Regular,
    /// 一屏的主操作
    Large,
}

type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// 按钮。外面可以再加布局的样式（`flex_1`、`w_full` 等）。
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    detail: Option<SharedString>,
    kind: Kind,
    size: Size,
    fill: Option<Hsla>,
    disabled: bool,
    on_click: Option<OnClick>,
    style: StyleRefinement,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, kind: Kind) -> Button {
        Button {
            id: id.into(),
            label: label.into(),
            detail: None,
            kind,
            size: Size::Regular,
            fill: None,
            disabled: false,
            on_click: None,
            style: StyleRefinement::default(),
        }
    }

    pub fn size(mut self, size: Size) -> Button {
        self.size = size;
        self
    }

    /// 写在名字后面的小字：按下去的后果（「开始下载 · 1080p」后面的「约 810 MB」）。
    pub fn detail(mut self, detail: impl Into<SharedString>) -> Button {
        self.detail = Some(detail.into());
        self
    }

    /// 实底按钮（[`Kind::Primary`]）换成 `fill` 色：直播录制时的主操作用直播色。
    pub fn fill(mut self, fill: Hsla) -> Button {
        self.fill = Some(fill);
        self
    }

    /// 为 true 时变淡、不可点。
    pub fn disabled(mut self, disabled: bool) -> Button {
        self.disabled = disabled;
        self
    }

    pub fn on_click(
        mut self,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Button {
        self.on_click = Some(Box::new(on_click));
        self
    }
}

impl Styled for Button {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for Button {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let (height, pad, text) = match self.size {
            Size::Small => (30., 12., 12.),
            Size::Regular => (36., 16., 13.),
            Size::Large => (48., 24., 15.),
        };
        let mut button = div()
            .id(self.id)
            .h(px(height))
            .px(px(pad))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .rounded(px(CONTROL_RADIUS))
            .text_size(px(text))
            .font_weight(FontWeight::MEDIUM)
            .whitespace_nowrap();
        let fill = self.fill.unwrap_or_else(theme::accent);
        button = match self.kind {
            Kind::Primary => button
                .bg(fill)
                .text_color(theme::on())
                .font_weight(FontWeight::SEMIBOLD),
            Kind::Secondary => button
                .bg(theme::wash())
                .border_1()
                .border_color(theme::outline())
                .text_color(theme::ink()),
            Kind::Text => button.text_color(theme::ink_2()),
            Kind::Warn => button
                .bg(theme::alpha(theme::warn(), 0.1))
                .border_1()
                .border_color(theme::alpha(theme::warn(), 0.6))
                .text_color(theme::warn()),
        };
        if self.disabled {
            button = button.opacity(0.4);
        } else {
            let kind = self.kind;
            button = button.hover(move |s| match kind {
                Kind::Primary => s.bg(theme::alpha(fill, 0.86)),
                Kind::Secondary => s.bg(theme::hover()),
                Kind::Text => s.text_color(theme::ink()).bg(theme::wash()),
                Kind::Warn => s.bg(theme::alpha(theme::warn(), 0.16)),
            });
            if let Some(on_click) = self.on_click {
                button = button.cursor_pointer().on_click(on_click);
            }
        }
        button.style().refine(&self.style);
        button.child(self.label).children(self.detail.map(|detail| {
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::NORMAL)
                .opacity(0.78)
                .child(detail)
        }))
    }
}
