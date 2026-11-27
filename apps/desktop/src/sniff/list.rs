//! 嗅探页的面板：网页收起时为一整块面板（标题栏、左边找到的流或找流中的表盘、右边「这个页面」一栏），网页显示着时
//! 为网页右边的一块（找到的流）。

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};
use hs_dlx_app::page::{Check, Found};
use hs_dlx_app::title_of;
use hs_m3u8_core::Url;

use super::{MAX_FOUND, SUGGEST_AFTER, SniffEvent, SniffView};
use crate::links::http_url;
use crate::new_task::describe;
use crate::ui;
use crate::widgets::{self, Button, Kind, Stat, Step};
use crate::{dial, format, ring, theme};

/// 「这个页面」一栏的宽。
const ABOUT_WIDTH: f32 = 300.;
/// 找流中的小表盘：边长、刻度环的半径、圆盘的直径。
const SCAN: f32 = 220.;
const SCAN_RADIUS: f32 = 93.;
const SCAN_DISC: f32 = 148.;
/// 流前面的小同心环（见 [`ui::rings`]）的边长。
const RINGS: f32 = 44.;
/// 网页显示着时右边一块的宽与窗口窄时最多收到的宽。
const ASIDE_WIDTH: f32 = 380.;
const ASIDE_MIN_WIDTH: f32 = 300.;

impl SniffView {
    /// 网页收起时的整块面板。
    pub(super) fn render_deck(&self, cx: &mut Context<Self>) -> Div {
        ui::deck()
            .flex_1()
            .min_w_0()
            .child(self.render_head(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_found(cx))
                    .child(self.render_about(cx)),
            )
    }

    /// 网页显示着时网页右边的一块：找到的流与网页要做的事。
    pub(super) fn render_aside(&self, cx: &mut Context<Self>) -> Div {
        let heading = if self.found.is_empty() {
            "还没找到视频流".to_owned()
        } else {
            format!("找到 {} 条视频流", self.found.len())
        };
        let rows: Vec<AnyElement> = if self.challenge {
            vec![
                widgets::note(
                    "网站要先验证你是真人：在网页里完成验证，再点播放，找到的流会列在这里",
                    true,
                )
                .into_any_element(),
            ]
        } else if self.found.is_empty() {
            vec![
                widgets::caption("在网页里登录、点了播放后，找到的流会列在这里").into_any_element(),
                self.probe_opened(cx).into_any_element(),
            ]
        } else {
            self.rows(true, cx)
        };
        ui::deck()
            .flex_basis(px(ASIDE_WIDTH))
            .flex_shrink(1.)
            .min_w(px(ASIDE_MIN_WIDTH))
            .child(ui::page_head(
                None,
                heading,
                Some(self.state().1.into()),
                [Button::new("hide-page", "收起网页", Kind::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.show_page(false, cx)))
                    .into_any_element()],
            ))
            .child(
                div()
                    .id("found")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(14.))
                    .children(rows),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .px(px(14.))
                    .pb(px(14.))
                    .children(self.failure_note())
                    .children(self.offers(cx))
                    .children(self.notes().into_iter().map(widgets::caption))
                    .children(self.error.clone().map(widgets::error_text)),
            )
    }

    /// 页面打不开（导航没成、网页进程退出）时的原因与做法；没有时为 None。
    fn failure_note(&self) -> Option<Div> {
        self.failure
            .as_ref()
            .map(|reason| widgets::note(format!("{reason}：刷新后重新找"), true))
    }

    /// 还没找到流时：打开的地址可能本身就是播放列表（缺请求头时源站回了网页），交回新建页改请求头。
    fn probe_opened(&self, cx: &mut Context<Self>) -> Button {
        Button::new("probe-opened", "当作播放列表探测", Kind::Secondary)
            .on_click(cx.listener(|this, _, _, cx| this.open_opened(cx)))
    }

    /// 网页加载到哪一步：状态点的颜色与说法。
    fn state(&self) -> (gpui_kit::Hsla, &'static str) {
        match (&self.webview, self.load.shown.is_some()) {
            (Err(_), _) => (theme::warn(), "建不出网页"),
            _ if self.failure.is_some() => (theme::warn(), "打不开"),
            (Ok(_), false) => (theme::accent(), "正在打开"),
            (Ok(_), true) if self.load.finished => (theme::accent(), "已加载"),
            (Ok(_), true) => (theme::accent(), "加载中"),
        }
    }

    /// 加载够久还没找到流：给出建议。
    fn suggest(&self) -> bool {
        self.found.is_empty() && self.load.since.elapsed() >= SUGGEST_AFTER
    }

    /// 标题栏：网页的标题与地址，右边是加载状态与操作（打不开时刷新、打开网页、关闭）。
    fn render_head(&self, cx: &mut Context<Self>) -> Div {
        let (color, state) = self.state();
        let failed = self.failure.is_some();
        let shown = self
            .load
            .shown
            .as_deref()
            .and_then(http_url)
            .unwrap_or_else(|| self.opened.clone());
        let mut actions = vec![ui::status(color, state).into_any_element()];
        if failed {
            actions.push(
                Button::new("reload", "刷新", Kind::Primary)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.navigate(wry::WebView::reload, "没能刷新", cx)
                    }))
                    .into_any_element(),
            );
        }
        if self.webview.is_ok() {
            actions.push(
                Button::new(
                    "show-page",
                    "打开网页",
                    if self.suggest() && !failed {
                        Kind::Primary
                    } else {
                        Kind::Secondary
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| this.show_page(true, cx)))
                .into_any_element(),
            );
        }
        if self.webview.is_ok() {
            actions.push(
                Button::new("page-as-web", "解析网页视频", Kind::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.open_page_as_web(cx)))
                    .into_any_element(),
            );
        }
        actions.push(
            Button::new("close-sniff", "关闭", Kind::Text)
                .on_click(cx.listener(|_, _, _, cx| cx.emit(SniffEvent::Dismissed)))
                .into_any_element(),
        );
        ui::page_head(
            Some(ui::rings(40., 3., 10.).into_any_element()),
            self.title
                .clone()
                .unwrap_or_else(|| "网页里的视频流".to_owned()),
            Some(meta(&shown).into()),
            actions,
        )
    }

    /// 左边：找到的流；还没有时为找流中的小表盘与说明，打不开时为原因。
    fn render_found(&self, cx: &mut Context<Self>) -> Div {
        let column = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(20.));
        let column = match &self.webview {
            Err(reason) => column
                .items_center()
                .justify_center()
                .gap(px(18.))
                .child(widgets::note(reason.clone(), true).max_w(px(440.)))
                .child(self.probe_opened(cx)),
            Ok(_) if self.found.is_empty() => column
                .items_center()
                .justify_center()
                .gap(px(18.))
                .children(self.failure.is_none().then(|| self.scan_dial(cx)))
                .child(self.waiting_text())
                .child(self.probe_opened(cx)),
            Ok(_) => {
                let checking = self
                    .found
                    .iter()
                    .any(|found| matches!(found.check, Check::Waiting | Check::Running));
                column
                    .children(self.failure_note())
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("找到 {} 条视频流", self.found.len())),
                            )
                            .child(widgets::caption(if checking {
                                "正在后台探测"
                            } else {
                                "已在后台探测好"
                            })),
                    )
                    .child(
                        div()
                            .id("found")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .children(self.rows(false, cx)),
                    )
            }
        };
        column.children(self.error.clone().map(widgets::error_text))
    }

    /// 还没找到流时表盘下面的说明：打不开时为原因。
    fn waiting_text(&self) -> Div {
        let (title, line) = match (&self.failure, self.load.shown.is_some(), self.suggest()) {
            (Some(reason), _, _) => (reason.clone(), "刷新后重新找"),
            (None, false, false) => (
                "正在打开网页".to_owned(),
                "网页在后台打开，不显示；找到的视频流会列在这里",
            ),
            (None, false, true) => ("网页还没打开".to_owned(), "可能连不上：打开网页看看"),
            (None, true, false) => (
                "网页在后台加载，找到的流会列在这里".to_owned(),
                "要登录、点验证或点了播放才出视频的网页，打开网页弄完再收起",
            ),
            (None, true, true) => (
                "还没找到视频流".to_owned(),
                "有的网页要登录、或点了播放才加载视频：打开网页弄好后，这里会列出来",
            ),
        };
        div()
            .max_w(px(420.))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.))
            .text_center()
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .when(self.failure.is_some(), |s| s.text_color(theme::warn()))
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .line_height(px(widgets::TEXT_LINE))
                    .text_color(theme::ink_3())
                    .child(line),
            )
    }

    /// 找流中的小表盘：彗尾按打开了多久转，圆盘里写打开了多久（逐帧重画见 [`SniffView::wait_to_suggest`]）。到了给出
    /// 建议的时候、嗅探页不显示或系统要求减少动态效果时停下，只写在找。
    fn scan_dial(&self, cx: &Context<Self>) -> Div {
        let elapsed = self.load.since.elapsed();
        let animate = !self.suggest() && self.on_screen && !self.page_shown && !cx.reduce_motion();
        let head = if animate {
            dial::head(elapsed, dial::TURN)
        } else {
            0.
        };
        let inset = (SCAN - SCAN_DISC) / 2.;
        div()
            .relative()
            .size(px(SCAN))
            .flex_none()
            .child(
                gpui_kit::canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        ring::paint_comet(bounds, SCAN_RADIUS, head, theme::accent(), window)
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .absolute()
                    .top(px(inset))
                    .left(px(inset))
                    .size(px(SCAN_DISC))
                    .rounded_full()
                    .bg(theme::moon())
                    .border_1()
                    .border_color(theme::rule())
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(4.))
                    .when(animate, |disc| {
                        disc.child(
                            div()
                                .text_size(px(28.))
                                .font_weight(FontWeight::BOLD)
                                .child(format::clock(elapsed)),
                        )
                    })
                    .child(widgets::caption(if self.load.shown.is_some() {
                        "在找视频流"
                    } else {
                        "正在打开网页"
                    })),
            )
    }

    /// 右边「这个页面」一栏：加载到哪一步、找到几条与网页经哪里出去，网页要做的事，与下载时带什么。
    fn render_about(&self, cx: &mut Context<Self>) -> Div {
        let mut reads = vec![Stat::new("找到", format!("{} 条", self.found.len()))];
        reads.extend(
            self.lease
                .as_ref()
                .map(|lease| Stat::text("网络", lease.egress())),
        );
        div()
            .w(px(ABOUT_WIDTH))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(18.))
            .p(px(20.))
            .border_l_1()
            .border_color(theme::rule())
            .child(widgets::caption("这个页面"))
            .child(widgets::steps(self.steps(), theme::accent()))
            .child(widgets::stats(reads, 1))
            .children(self.offers(cx))
            .children(self.notes().into_iter().map(widgets::caption))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(12.5))
                    .line_height(px(widgets::TEXT_LINE))
                    .text_color(theme::ink_3())
                    .child("下载时带上网页存下的 Cookie 与来源地址：要登录的网页，先打开网页登录"),
            )
    }

    /// 打开、加载、找流三步。
    fn steps(&self) -> Vec<(SharedString, Step)> {
        if self.webview.is_err() {
            return vec![("打开网页".into(), Step::Stopped)];
        }
        let shown = self.load.shown.is_some();
        let failed = self.failure.is_some();
        let open = match (shown, failed) {
            (true, _) => Step::Done,
            (false, true) => Step::Stopped,
            (false, false) => Step::Now,
        };
        let load = match (shown, failed, self.load.finished) {
            (false, _, _) => Step::Waiting,
            (true, true, _) => Step::Stopped,
            (true, false, true) => Step::Done,
            (true, false, false) => Step::Now,
        };
        let find = if shown && !failed {
            Step::Now
        } else {
            Step::Waiting
        };
        let find_text = if self.found.is_empty() {
            "找视频流".to_owned()
        } else {
            format!("找到 {} 条，接着找", self.found.len())
        };
        vec![
            ("打开网页".into(), open),
            ("加载网页".into(), load),
            (find_text.into(), find),
        ]
    }

    /// 网页要做、没有放行的事（开新窗口、下载），与让用户接着做的按钮。
    fn offers(&self, cx: &mut Context<Self>) -> Vec<Div> {
        let popup = self.popup.clone().map(|url| {
            let target = url.clone();
            offer(
                format!("网页要开新窗口：{}", meta(&url)),
                "open-popup",
                "在这里打开",
                cx.listener(move |this, _, _, cx| {
                    this.popup = None;
                    this.open(target.as_str(), cx);
                }),
            )
        });
        let download = self.download.clone().map(|url| {
            let target = url.clone();
            offer(
                format!("网页要下载文件（没有下）：{}", meta(&url)),
                "probe-download",
                "探测它",
                cx.listener(move |this, _, _, cx| {
                    this.download = None;
                    this.open_download(target.clone(), cx);
                }),
            )
        });
        popup.into_iter().chain(download).collect()
    }

    /// 列表之外要说明的：没列出的条数、出口没照设置、丢了的消息。
    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.overflow > 0 {
            notes.push(format!(
                "只列出前 {MAX_FOUND} 条，另有 {} 条没列出",
                self.overflow
            ));
        }
        if let Some(note) = &self.proxy_note {
            notes.push(note.clone());
        }
        let dropped = self.inbox.dropped();
        if dropped > 0 {
            notes.push(format!(
                "网页发来的消息太多或太长，丢了 {dropped} 条，可能有流没列出"
            ));
        }
        notes
    }

    /// 找到的流，最该下载的一条（见 [`SniffView::first_ready`]）突出；`compact` 为网页显示着时的窄列表。
    fn rows(&self, compact: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let first = self.first_ready();
        self.found
            .iter()
            .enumerate()
            .map(|(index, found)| self.render_row(index, found, first == Some(index), compact, cx))
            .collect()
    }

    /// 最该下载的一条：第一条探测好的主播放列表（有几档清晰度可挑），没有时第一条探测好的。
    fn first_ready(&self) -> Option<usize> {
        let ready = |master: bool| {
            self.found.iter().position(|found| {
                matches!(&found.check, Check::Ready { probe, .. } if !master || probe.master.is_some())
            })
        };
        ready(true).or_else(|| ready(false))
    }

    /// 一条流：同心环、名字、地址与探测结果，与能做的事：探测好的下载（几档可挑的写挑清晰度）；没成的重试（重新读
    /// Cookie），配好了请求头的还能打开新建页改请求头再试。
    fn render_row(
        &self,
        index: usize,
        found: &Found,
        best: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (tags, failed): (Vec<String>, Option<String>) = match &found.check {
            Check::Waiting => (vec!["等待探测".to_owned()], None),
            Check::Running => (vec!["探测中…".to_owned()], None),
            Check::Ready { probe, .. } => (describe(probe), None),
            Check::Failed { reason, .. } => (Vec::new(), Some(reason.clone())),
        };
        let skipped =
            (found.skipped > 0).then(|| format!("没带 {} 个放不进请求头的 Cookie", found.skipped));
        let open = |label: &'static str, tone: Kind, cx: &mut Context<Self>| {
            let url = found.url.clone();
            Button::new(("open-found", index), label, tone)
                .on_click(cx.listener(move |this, _, _, cx| this.open_found(index, &url, cx)))
        };
        let retry = |cx: &mut Context<Self>| {
            let url = found.url.clone();
            Button::new(("retry-found", index), "重试", Kind::Secondary)
                .on_click(cx.listener(move |this, _, _, cx| this.retry(Some((index, &url)), cx)))
        };
        let actions = match &found.check {
            Check::Waiting | Check::Running => Vec::new(),
            Check::Ready { probe, .. } => {
                let label = if probe.master.as_ref().is_some_and(|m| m.variants.len() > 1) {
                    "挑清晰度"
                } else {
                    "下载"
                };
                vec![open(
                    label,
                    if best { Kind::Primary } else { Kind::Secondary },
                    cx,
                )]
            }
            Check::Failed {
                headers: Some(_), ..
            } => vec![retry(cx), open("查看", Kind::Secondary, cx)],
            Check::Failed { headers: None, .. } => vec![retry(cx)],
        };
        let detail = if compact {
            div()
                .text_size(px(12.5))
                .text_color(theme::ink_3())
                .truncate()
                .child(tags.join("，"))
        } else {
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .children(tags.into_iter().map(tag))
        };
        ui::well()
            .flex()
            .items_center()
            .gap(px(14.))
            .px(px(16.))
            .py(px(14.))
            .when(best, |row| {
                row.border_color(theme::alpha(theme::accent(), 0.45))
            })
            .child(ui::rings(RINGS, 3., 10.).when(!best, |rings| rings.opacity(0.6)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title_of(&found.url)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme::ink_3())
                            .truncate()
                            .child(SharedString::from(meta(&found.url))),
                    )
                    .child(detail)
                    .children(failed.map(|reason| {
                        div()
                            .text_size(px(12.5))
                            .line_height(px(widgets::TEXT_LINE))
                            .text_color(theme::warn())
                            .line_clamp(2)
                            .child(reason)
                    }))
                    .children(skipped.map(widgets::caption)),
            )
            .child(div().flex().flex_none().gap(px(8.)).children(actions))
            .into_any_element()
    }
}

/// 流的一项探测结果（时长、几档清晰度、几条音轨、直播）。
fn tag(text: String) -> Div {
    div()
        .h(px(22.))
        .px(px(8.))
        .flex()
        .items_center()
        .rounded(px(6.))
        .bg(theme::hover())
        .text_size(px(12.))
        .text_color(theme::ink_2())
        .child(text)
}

/// 网页要做、没有放行的一件事与让用户接着做的按钮。
fn offer(
    text: String,
    id: &'static str,
    label: &'static str,
    act: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
) -> Div {
    ui::well()
        .flex()
        .flex_col()
        .gap(px(10.))
        .p(px(12.))
        .child(
            div()
                .text_size(px(12.5))
                .line_height(px(widgets::TEXT_LINE))
                .text_color(theme::ink_2())
                .child(text),
        )
        .child(Button::new(id, label, Kind::Secondary).on_click(act))
}

/// 流的说明：主机与路径；带查询参数的标出（同一个地址常带不同的令牌，只看主机与路径分不出来）。
fn meta(url: &Url) -> String {
    let mut text = format!("{}{}", url.host_str().unwrap_or_default(), url.path());
    if url.query().is_some() {
        text.push_str(" · 带参数");
    }
    text
}
