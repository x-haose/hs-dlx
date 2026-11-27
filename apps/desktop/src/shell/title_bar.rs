//! 顶栏：标志与名字，右边是有新版本时的提示、进行中的任务数、总速度与保存目录所在盘的可用空间。

use std::time::Instant;

use gpui_kit::component::TitleBar;
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, canvas, div, px,
};

use super::Shell;
use crate::tasks::Tasks;
use crate::ui::{self, TITLE_HEIGHT};
use crate::{format, ring, theme};

/// 标志的小环（画到 [`ring::LOGO_ARC`]）的边长与环宽。
const LOGO: f32 = 18.;
const LOGO_WIDTH: f32 = 2.4;
/// 标志离红绿灯多远、与名字的间距、右边状态离窗口边多远。
const LOGO_MARGIN: f32 = 14.;
const LOGO_GAP: f32 = 9.;
const STATUS_MARGIN: f32 = 20.;
/// 可用空间少于这么多时标成警告色：约 10 Mbps 的视频再下十几分钟就满。
const LOW_SPACE: u64 = 1 << 30;
/// 设置按钮的边长。
const SETTINGS: f32 = 34.;

impl Shell {
    pub(super) fn render_header(
        &self,
        tasks: &Tasks,
        now: Instant,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let activity = tasks.activity(now);
        let activity = format::activity(activity.running, activity.speed);
        // 可用空间：不足或读不出时标警告色，只标这一段
        let space = tasks.free_space().map(|free| match free {
            Ok(bytes) => (
                format!("可用 {}", format::bytes(*bytes)),
                *bytes < LOW_SPACE,
            ),
            Err(reason) => (reason.clone(), true),
        });
        // TitleBar 把放子元素的整条标成拖动区。Windows 上鼠标下的命中列表里有拖动区时，系统把按下当成拖标题栏，点击
        // 到不了元素；能点的元素都要 occlude，把拖动区挡在命中列表外
        TitleBar::new()
            .h(px(TITLE_HEIGHT))
            // 透明：窗口的背景铺到顶
            .bg(gpui_kit::transparent_black())
            .border_color(gpui_kit::transparent_black())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(LOGO_GAP))
                    .ml(px(LOGO_MARGIN))
                    .child(
                        canvas(
                            |_, _, _| {},
                            |bounds, _, window, _| {
                                ring::paint_arc(
                                    bounds,
                                    LOGO_WIDTH,
                                    ring::LOGO_ARC,
                                    theme::accent(),
                                    theme::line(),
                                    window,
                                );
                            },
                        )
                        .size(px(LOGO)),
                    )
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::BOLD)
                            .child("hs-dlx"),
                    ),
            )
            .child(
                div()
                    .pr(px(STATUS_MARGIN))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .children(self.updates.read(cx).pending().map(|version| {
                        div()
                            .id("update-pending")
                            .occlude()
                            .text_size(px(13.))
                            .text_color(theme::accent())
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme::accent_pale()))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_updates(window, cx);
                            }))
                            .child(format!("新版本 {version}"))
                    }))
                    .child(ui::num(
                        div()
                            .flex()
                            .gap(px(18.))
                            .text_size(px(13.))
                            .text_color(theme::soft())
                            .children(activity)
                            .children(space.map(|(text, warn)| {
                                div()
                                    .when(warn, |s| s.text_color(theme::warn()))
                                    .child(text)
                            })),
                    ))
                    .child(self.settings_button(cx)),
            )
    }

    /// 设置的按钮：齿轮，设置页开着时发光。
    fn settings_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let on = self.settings.is_some();
        div()
            .id("settings")
            .occlude()
            .size(px(SETTINGS))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(10.))
            .border_1()
            .cursor_pointer()
            .map(|button| {
                if on {
                    button
                        .border_color(theme::edge())
                        .bg(theme::selected())
                        .text_color(theme::accent())
                } else {
                    button
                        .border_color(gpui_kit::transparent_black())
                        .text_color(theme::muted())
                        .hover(|s| s.bg(theme::selected()))
                }
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_settings(window, cx)))
            .child(Icon::new(IconName::Settings).size(px(17.)))
    }
}
