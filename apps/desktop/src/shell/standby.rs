//! 没有任务时的待机页：舞台是垫着引导线的表盘，彗尾慢慢扫、背光起伏（逐帧重画见 [`Shell::sync_standby`]）；粘贴栏
//! 获得焦点时背光提亮，框里是可探测的链接时整圈点亮、圆盘写认出了什么。检查面板是新任务照着开始的设置；剪贴板里有
//! 链接时，底栏的主操作是探测它。

use std::f32::consts::{PI, TAU};

use gpui_kit::{
    AnyElement, Context, Div, FontWeight, IntoElement, ParentElement as _, PathBuilder,
    Styled as _, Window, canvas, div, point, px,
};
use hs_dlx_scene::Mode;
use hs_m3u8_core::{Proxy, bare_url};

use super::Shell;
use crate::links;
use crate::tasks::Tasks;
use crate::ui::{self, Solid};
use crate::{dial, format, ring, theme};

/// 背光起伏一次的秒数。
const BREATH: f32 = 6.;
/// 背光起伏的幅度，与输入时、认出链接时提亮多少（都是加在 [`dial::LIGHT_STRENGTH`] 上的强度）。
const BREATH_DEPTH: f32 = 0.025;
const LIFT_TYPING: f32 = 0.03;
const LIFT_LINK: f32 = 0.07;
/// 「支持的链接」里小图形的边长。
const KIND_ICON: f32 = 28.;

impl Shell {
    pub(super) fn render_standby(
        &self,
        tasks: &Tasks,
        window: &Window,
        cx: &Context<Self>,
    ) -> (Div, Div) {
        let dock = self.dock.read(cx);
        let link = links::target(&dock.text(cx));
        let animate = !cx.reduce_motion();
        let elapsed = self.standby_since.elapsed();
        let breath = if animate {
            (elapsed.as_secs_f32() * TAU / BREATH).sin() * BREATH_DEPTH
        } else {
            0.
        };
        let lift = match (&link, dock.typing(window, cx)) {
            (Some(_), _) => LIFT_LINK,
            (None, true) => LIFT_TYPING,
            (None, false) => 0.,
        };
        let head = if animate {
            dial::head(elapsed, dial::STANDBY_TURN)
        } else {
            0.
        };
        let charged = link.is_some();
        let figure = dial::figure(move |bounds, window, _| {
            if charged {
                let ticks = ring::Ticks::full(ring::COMET_TICKS, theme::accent(), true);
                ring::paint_ticks(bounds, ring::RADIUS, &ticks, window)
            } else {
                ring::paint_comet(bounds, ring::RADIUS, head, theme::accent(), window)
            }
        });
        let readout = match &link {
            Some(url) if links::stream_url(url.as_str()).is_some() => dial::readout(
                "认出 m3u8 地址",
                theme::accent(),
                dial::Big::Phrase(url.host_str().unwrap_or_default().to_owned()),
                "回车探测清晰度与音轨",
            ),
            Some(url) if links::torrent_url(url.as_str()).is_some() => dial::readout(
                "认出链接",
                theme::accent(),
                dial::Big::Phrase(links::source(url)),
                "回车取种子信息，再选要下的文件",
            ),
            Some(url) => dial::readout(
                "认出链接",
                theme::accent(),
                dial::Big::Phrase(links::source(url)),
                "回车探测；是网页就在后台找视频流，是文件就下载",
            ),
            None => dial::readout(
                "就绪",
                theme::accent(),
                dial::Big::Phrase("粘贴链接开始".to_owned()),
                "m3u8 地址、网页、文件或磁力链接",
            ),
        };
        let figure = dial::lit_dial(
            theme::accent(),
            dial::LIGHT_STRENGTH + breath + lift,
            Mode::Standby,
            figure,
            readout,
        );
        let stage = ui::stage(ring::AREA, figure, None, self.dock.clone());
        (stage, self.standby_inspector(tasks, cx))
    }

    /// 待机页的检查面板：新任务照着开始的保存位置、可用空间与网络，与支持哪几种链接；剪贴板里有链接时底栏是探测它。
    fn standby_inspector(&self, tasks: &Tasks, cx: &Context<Self>) -> Div {
        let (free, no_space) = match tasks.free_space() {
            None => ("—".to_owned(), None),
            Some(Ok(bytes)) => (format::bytes(*bytes), None),
            Some(Err(reason)) => ("读不出".to_owned(), Some(reason.clone())),
        };
        let (network, unreadable) = match &self.standby_proxy {
            Ok(Proxy::System) => ("跟随系统", None),
            Ok(Proxy::Direct) => ("直连", None),
            Ok(Proxy::Url(_)) => ("指定代理", None),
            Err(reason) => ("读不出", Some(format!("读不出设置：{reason}"))),
        };
        let mut body = vec![
            ui::reads(
                vec![
                    ("可用空间".into(), free.into()),
                    ("网络".into(), network.into()),
                ],
                Some((
                    "保存到".into(),
                    format::path(tasks.folder(), self.home.as_deref()).into(),
                )),
            )
            .into_any_element(),
        ];
        body.extend(
            [no_space, unreadable]
                .into_iter()
                .flatten()
                .map(|reason| ui::error_text(reason).into_any_element()),
        );
        body.push(
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(ui::caption("支持的链接"))
                .child(kind(
                    ui::rings(KIND_ICON, 2.5, 14.).into_any_element(),
                    "m3u8 地址",
                    "直接探测出几档清晰度与音轨",
                ))
                .child(kind(
                    page_icon(),
                    "有视频的网页",
                    "在后台打开，找出里面的视频流",
                ))
                .child(kind(
                    file_icon(),
                    "文件",
                    "服务器支持时分段并行下载、断点续传",
                ))
                .child(kind(
                    magnet_icon(),
                    "磁力链接与种子文件",
                    "取到种子信息后选要下的文件；磁力链接也能交给 PikPak 云离线",
                ))
                .into_any_element(),
        );
        let foot = self.clipboard.clone().map(|url| {
            let shown = bare_url(&url).to_string();
            ui::primary(
                "probe-clipboard",
                "探测剪贴板里的链接",
                Some(shown.into()),
                Solid::Glow,
                true,
                cx.listener(move |this, _, window, cx| this.probe_text(url.as_str(), window, cx)),
            )
            .into_any_element()
        });
        ui::inspector("新任务", "照这些设置开始，在设置里改", body, foot)
    }
}

/// 「支持的链接」一行：图形、名字与它会怎样。
fn kind(icon: AnyElement, name: &'static str, does: &'static str) -> Div {
    div().flex().items_start().gap(px(12.)).child(icon).child(
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme::muted())
                    .child(does),
            ),
    )
}

/// 「文件」的小图形：一个往下的箭头落在一条横线上。
fn file_icon() -> AnyElement {
    canvas(
        |_, _, _| {},
        |bounds, (), window, _| {
            let at = |x: f32, y: f32| point(bounds.origin.x + px(x), bounds.origin.y + px(y));
            let mut path = PathBuilder::stroke(px(2.));
            path.move_to(at(14., 5.));
            path.line_to(at(14., 18.));
            path.move_to(at(9., 13.));
            path.line_to(at(14., 18.));
            path.line_to(at(19., 13.));
            path.move_to(at(6., 23.));
            path.line_to(at(22., 23.));
            let path = path
                .build()
                .expect("几条线段的描边，顶点远低于曲面细分的上限");
            window.paint_path(path, theme::hover_edge());
        },
    )
    .size(px(KIND_ICON))
    .flex_none()
    .into_any_element()
}

/// 磁力链接的小图形：U 形磁铁，两极各一道横线。
fn magnet_icon() -> AnyElement {
    canvas(
        |_, _, _| {},
        |bounds, (), window, _| {
            let at = |x: f32, y: f32| point(bounds.origin.x + px(x), bounds.origin.y + px(y));
            let mut path = PathBuilder::stroke(px(2.));
            path.move_to(at(6., 4.));
            path.line_to(at(6., 14.));
            // 底下的半圆：圆心 (14, 14)、半径 8，从左经最低点到右，折线逼近
            const STEPS: u8 = 16;
            for step in 1..=STEPS {
                let angle = PI * (1. - f32::from(step) / f32::from(STEPS));
                path.line_to(at(14. + 8. * angle.cos(), 14. + 8. * angle.sin()));
            }
            path.line_to(at(22., 4.));
            path.move_to(at(3., 9.));
            path.line_to(at(9., 9.));
            path.move_to(at(19., 9.));
            path.line_to(at(25., 9.));
            let path = path
                .build()
                .expect("几十个顶点的描边，远低于曲面细分的上限");
            window.paint_path(path, theme::hover_edge());
        },
    )
    .size(px(KIND_ICON))
    .flex_none()
    .into_any_element()
}

/// 网页的小图形：圆角方框里两道横线。
fn page_icon() -> AnyElement {
    let bar = |width: f32| div().h(px(2.)).w(px(width)).bg(theme::hover_edge());
    div()
        .size(px(KIND_ICON))
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(3.))
        .p(px(6.))
        .rounded(px(7.))
        .border_2()
        .border_color(theme::hover_edge())
        .child(bar(12.))
        .child(bar(8.))
        .into_any_element()
}
