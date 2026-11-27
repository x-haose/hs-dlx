//! 待机：没有任务时。没有列表一栏；圆盘在窗口正中（日冕从它边上放出），粘贴栏在它下面居中，再下面是剪贴板提示，
//! 窗口底部一行写新任务存到哪、经哪里走、PikPak 登录了没有。有了第一个任务，圆盘滑到右边，列表出现。

use gpui_kit::{
    Context, FontWeight, IntoElement, ParentElement as _, Styled as _, Window, canvas, div, px,
};
use hs_dlx_scene::Mode;
use hs_m3u8_core::Proxy;

use super::Shell;
use super::column::clipboard_hint;
use crate::{backdrop, format, theme};

/// 以 900 点高的窗口为准：圆盘的半径、圆心离顶、圆盘到粘贴栏的间距。窗口矮时按高度缩，半径不小于 [`MIN_RADIUS`]。
const BASE_HEIGHT: f32 = 900.;
const RADIUS: f32 = 168.;
const MIN_RADIUS: f32 = 110.;
const CENTER_TOP: f32 = 366.;
const BAR_GAP: f32 = 130.;
/// 粘贴栏的宽；窗口窄时两边各留 [`SIDE`]。
const BAR_WIDTH: f32 = 640.;
const SIDE: f32 = 20.;
/// 底部那一行离窗口底边多远。
const FOOT_BOTTOM: f32 = 22.;

impl Shell {
    pub(super) fn render_standby(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let size = window.viewport_size();
        let (width, height) = (f32::from(size.width), f32::from(size.height));
        let scale = height / BASE_HEIGHT;
        let radius = (RADIUS * scale).clamp(MIN_RADIUS, RADIUS);
        let center = (width / 2., CENTER_TOP * scale);
        let bar_width = BAR_WIDTH.min(width - 2. * SIDE);
        let tasks = self.tasks.read(cx);
        let network = match &self.network {
            Ok(Proxy::System) => "网络跟随系统".to_owned(),
            Ok(Proxy::Direct) => "网络直连".to_owned(),
            Ok(Proxy::Url(_)) => "网络经指定代理".to_owned(),
            Err(reason) => format!("读不出网络设置：{reason}"),
        };
        let pikpak = match &tasks.pikpak().user {
            Some(Ok(_)) => "PikPak 已登录".to_owned(),
            Some(Err(reason)) => reason.clone(),
            None => "PikPak 没登录".to_owned(),
        };
        let disc = div()
            .absolute()
            .left(px(center.0 - radius))
            .top(px(center.1 - radius))
            .size(px(2. * radius))
            .rounded_full()
            .bg(theme::moon())
            .border_1()
            .border_color(theme::rule())
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        backdrop::report_disc(
                            backdrop::Disc {
                                center: bounds.center(),
                                radius,
                                mode: Mode::Standby,
                            },
                            cx,
                        );
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme::accent())
                    .child("就绪"),
            )
            .child(
                div()
                    .text_size(px(26.))
                    .font_weight(FontWeight::MEDIUM)
                    .child("粘贴链接开始"),
            )
            .child(
                div()
                    .max_w(px(2. * radius - 48.))
                    .text_center()
                    .text_size(px(12.))
                    .text_color(theme::ink_2())
                    .child("m3u8、有视频的网页、文件、磁力与种子"),
            );
        let below = div()
            .absolute()
            .left(px((width - bar_width) / 2.))
            .top(px(center.1 + radius + (BAR_GAP * scale).max(32.)))
            .w(px(bar_width))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(self.paste.clone())
            .children(self.clipboard.as_ref().map(|clip| clipboard_hint(clip, cx)));
        let foot = div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(FOOT_BOTTOM))
            .flex()
            .justify_center()
            .gap(px(22.))
            .text_size(px(12.))
            .text_color(theme::ink_3())
            .child(format!(
                "保存到 {}",
                format::path(tasks.folder(), self.home.as_deref())
            ))
            .child(network)
            .child(pikpak);
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(disc)
            .child(below)
            .child(foot)
    }
}
