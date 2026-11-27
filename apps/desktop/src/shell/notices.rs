//! 列表一栏底部的提示：要用户看到、关掉才消失的；正在退出时最上面一条说明在等什么。浮在列表上，不挤占布局，也不
//! 与右边的舞台（内置网页会盖住它所在区域里的一切）重叠。

use gpui_kit::{Context, IntoElement, ParentElement as _, SharedString, Styled as _, div, px};

use super::{COLUMN_LEFT, Shell};
use crate::tasks::{Quit, Tasks};
use crate::theme;
use crate::ui::{self, PAD};
use crate::widgets::{Button, Kind};

/// 同时显示几条。
const SHOWN: usize = 4;

impl Shell {
    /// `width` 为列表一栏的宽。
    pub(super) fn render_notices(
        &self,
        tasks: &Tasks,
        width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let quitting =
            (tasks.quit() == Quit::Waiting).then(|| {
                ui::card(theme::outline())
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(format!("正在退出：等 {} 个任务停下", tasks.busy()))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new("quit-now", "立即退出", Kind::Secondary).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.tasks.update(cx, |tasks, cx| tasks.leave(cx))
                                    }),
                                ),
                            )
                            .child(Button::new("stay", "不退出了", Kind::Text).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.tasks.update(cx, |tasks, cx| {
                                        tasks.stay();
                                        cx.notify();
                                    })
                                }),
                            )),
                    )
            });
        // 只显示最近几条；关掉一条露出更早的
        let notices: Vec<_> = tasks
            .notices()
            .iter()
            .rev()
            .take(SHOWN)
            .rev()
            .map(|notice| {
                let id = notice.id;
                ui::card(theme::alpha(theme::warn(), 0.35))
                    .flex()
                    .items_start()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(SharedString::from(notice.text.clone())),
                    )
                    .child(Button::new(("dismiss", id), "知道了", Kind::Text).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.tasks
                                .update(cx, |tasks, cx| tasks.dismiss_notice(id, cx))
                        }),
                    ))
            })
            .collect();
        div()
            .absolute()
            .bottom(px(PAD))
            .left(px(COLUMN_LEFT))
            .w(px(width))
            .flex()
            .flex_col()
            .gap(px(8.))
            .children(quitting)
            .children(notices)
    }
}
