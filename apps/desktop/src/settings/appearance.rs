//! 「外观」一组：日蚀与血月。

use gpui_kit::{Context, Div, ParentElement as _, Styled as _, div, px};

use super::{SettingsView, row};
use crate::theme::{self, Theme};
use crate::tray::Tray;
use crate::widgets::{self, Toward};

impl SettingsView {
    /// 换外观：存进了才换，整个窗口随之重画。
    fn pick_theme(&mut self, picked: Theme, cx: &mut Context<Self>) {
        match self.tasks.read(cx).service().set_theme(picked) {
            Ok(()) => {
                self.theme_error = None;
                theme::set(picked);
                Tray::theme_changed(&self.tasks, cx);
                cx.refresh_windows();
            }
            Err(error) => self.theme_error = Some(error.to_string()),
        }
        cx.notify();
    }

    pub(super) fn render_theme(&self, cx: &mut Context<Self>) -> Div {
        let chip = |id: &'static str, label: &'static str, value: Theme, cx: &mut Context<Self>| {
            widgets::chip(
                id,
                label,
                theme::current() == value,
                Toward::Right,
                cx.listener(move |this, _, _, cx| this.pick_theme(value, cx)),
            )
        };
        row(
            "外观",
            None,
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.))
                        .child(chip("theme-eclipse", "日蚀", Theme::Eclipse, cx))
                        .child(chip("theme-blood-moon", "血月", Theme::BloodMoon, cx)),
                )
                .children(self.theme_error.clone().map(widgets::error_text)),
        )
    }
}
