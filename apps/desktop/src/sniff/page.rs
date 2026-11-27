//! 网页的面板：后退、前进、刷新、地址栏与关闭嗅探页，下面是网页本身。

use gpui_kit::component::input::Input;
use gpui_kit::{Context, Div, ParentElement as _, Styled as _, div, px};

use super::{SniffEvent, SniffView};
use crate::theme;
use crate::ui;
use crate::widgets::{self, Button, Kind};

/// 网页的面板最窄多宽：常见网页的移动版布局从这里起。
const PAGE_MIN_WIDTH: f32 = 480.;
/// 后退、前进、刷新按钮的边长。
const NAV: f32 = 36.;

impl SniffView {
    /// 后退、前进或刷新：`act` 调网页的那个操作，`what` 为没做成时的说法。
    pub(super) fn navigate(
        &mut self,
        act: fn(&wry::WebView) -> wry::Result<()>,
        what: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(webview) = self.webview() {
            self.error = act(webview.read(cx).raw())
                .err()
                .map(|error| format!("{what}：{error}"));
        }
        cx.notify();
    }

    pub(super) fn render_page(&self, cx: &mut Context<Self>) -> Div {
        let nav = |id: &'static str,
                   label: &'static str,
                   act: fn(&wry::WebView) -> wry::Result<()>,
                   what: &'static str,
                   cx: &mut Context<Self>| {
            Button::new(id, label, Kind::Text)
                .on_click(cx.listener(move |this, _, _, cx| this.navigate(act, what, cx)))
                .w(px(NAV))
                .h(px(NAV))
                .px_0()
                .text_size(px(17.))
        };
        let toolbar = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(12.))
            .py(px(10.))
            .border_b_1()
            .border_color(theme::rule())
            .child(nav("back", "‹", wry::WebView::go_back, "没能后退", cx))
            .child(nav(
                "forward",
                "›",
                wry::WebView::go_forward,
                "没能前进",
                cx,
            ))
            .child(nav("reload", "↻", wry::WebView::reload, "没能刷新", cx))
            .child(
                widgets::input_frame()
                    .flex_1()
                    .ml(px(6.))
                    .px(px(6.))
                    .child(Input::new(&self.address).appearance(false)),
            )
            .child(
                Button::new("close-sniff-page", "关闭", Kind::Text)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SniffEvent::Dismissed))),
            );
        // 网页建不出来时没有「打开网页」，到不了这里
        let page = self
            .webview()
            .map(|webview| div().flex_1().min_h_0().child(webview.clone()));
        let trouble = self
            .webview()
            .and_then(|webview| webview.read(cx).trouble());
        // 原生网页盖住下边的圆角，下边不留圆角
        ui::deck()
            .flex_1()
            .min_w(px(PAGE_MIN_WIDTH))
            .rounded_b_none()
            .child(toolbar)
            .children(page)
            .children(trouble.map(|reason| div().p(px(10.)).child(widgets::error_text(reason))))
    }
}
