//! 设置页：左边是分组，右边只列当前一组的各行。改了即存进任务存储（见 [`hs_dlx_app::Settings`]），存进了界面才换。

mod about;
mod appearance;
mod cloud;
mod general;
mod torrent;
mod web;

use std::num::NonZeroU32;
use std::path::PathBuf;

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Div, ElementId, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle, Stateful,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px,
};
use hs_dlx_app::{Seeding, Settings};
use hs_m3u8_core::Proxy;

use self::general::PROXY_EXAMPLE;
use self::torrent::TORRENT_PROXY_EXAMPLE;
use crate::route::Route;
use crate::tasks::Tasks;
use crate::theme;
use crate::ui::{self, Tone};
use crate::updates::Updates;

/// 设置各行的最大宽度。
const MAX_WIDTH: f32 = 720.;
/// 每行左边名字一栏的宽。
const LABEL_WIDTH: f32 = 160.;
/// 左边分组一栏的宽。
const NAV_WIDTH: f32 = 148.;

/// 设置的一组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// 保存到、代理
    Download,
    /// 做种、上传限速、BT 代理、网络检测
    Torrent,
    /// 云离线的网盘账号
    Cloud,
    /// 网页视频的组件、更新渠道与画质
    Web,
    Appearance,
    /// 更新、版本与第三方声明
    About,
}

impl Section {
    const ALL: [Section; 6] = [
        Section::Download,
        Section::Torrent,
        Section::Cloud,
        Section::Web,
        Section::Appearance,
        Section::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Section::Download => "下载",
            Section::Torrent => "BT",
            Section::Cloud => "网盘",
            Section::Web => "网页视频",
            Section::Appearance => "外观",
            Section::About => "更新与关于",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Section::Download => "settings-download",
            Section::Torrent => "settings-torrent",
            Section::Cloud => "settings-cloud",
            Section::Web => "settings-web",
            Section::Appearance => "settings-appearance",
            Section::About => "settings-about",
        }
    }
}

/// 发给外面的事件。
pub enum SettingsEvent {
    Dismissed,
}

/// 代理的三种方式；选中「指定代理」后要填了地址、保存才生效。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    System,
    Direct,
    Custom,
}

pub struct SettingsView {
    /// 保存目录在这里（见 [`Tasks::set_folder`]）
    tasks: Entity<Tasks>,
    home: Option<PathBuf>,
    /// 存着的代理
    proxy: Proxy,
    /// 选中的方式：选了「指定代理」而还没保存地址时与 `proxy` 不同
    mode: Mode,
    address: Entity<InputState>,
    /// 跟随系统时一般的网址经哪里走；还没读到时为 None
    system: Option<Route>,
    /// 保存目录最近一次没换成的原因
    folder_error: Option<String>,
    /// 代理最近一次没存进去的原因
    proxy_error: Option<String>,
    /// 外观最近一次没存进去的原因
    theme_error: Option<String>,
    /// 存着的做种设置与上传限速
    seeding: Seeding,
    upload_limit: Option<NonZeroU32>,
    /// 做种或上传限速最近一次没存进去的原因
    seeding_error: Option<String>,
    /// 第三方声明打不开的原因
    notices_error: Option<String>,
    updates: Entity<Updates>,
    /// 「自动检查更新」最近一次没存进去的原因
    updates_error: Option<String>,
    _system: Task<()>,
    _typed: Subscription,
    _updates: Subscription,
    /// 任务状态变了（PikPak 登录的进展在那里）即重画
    _tasks: Subscription,
    /// 存着的 BT 代理（None 为直连）与选中的方式：选了经代理而还没存进地址时为 true
    torrent_proxy: Option<hs_m3u8_core::Url>,
    torrent_proxy_picked: bool,
    torrent_address: Entity<InputState>,
    /// BT 代理最近一次没存进去的原因
    torrent_proxy_error: Option<String>,
    /// 网络检测：在做时有（释放即取消），做完的结果或没做成的原因
    network_check: Option<Task<()>>,
    network_report: Option<Result<hs_dlx_app::NetworkReport, String>>,
    _torrent_typed: Subscription,
    /// 正显示的一组
    section: Section,
    /// 右边各行的滚动位置：换组时回到顶上
    scroll: ScrollHandle,
    web: web::WebSettings,
    /// 看着网页视频组件的状态
    _web: Task<()>,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

impl SettingsView {
    /// `settings` 为存着的设置。
    pub fn new(
        tasks: Entity<Tasks>,
        updates: Entity<Updates>,
        home: Option<PathBuf>,
        settings: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let watched = cx.observe(&updates, |_, _, cx| cx.notify());
        let tasks_watched = cx.observe(&tasks, |_, _, cx| cx.notify());
        let proxy = settings.proxy;
        let address = cx.new(|cx| InputState::new(window, cx).placeholder(PROXY_EXAMPLE));
        let torrent_address = cx.new(|cx| {
            InputState::new(window, cx).placeholder(format!("如 {TORRENT_PROXY_EXAMPLE}"))
        });
        if let Some(url) = &settings.torrent_proxy {
            torrent_address.update(cx, |input, cx| input.set_value(url.to_string(), window, cx));
        }
        let torrent_typed = cx.subscribe_in(
            &torrent_address,
            window,
            |this: &mut Self, _, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter { .. } => this.save_torrent_proxy(cx),
                InputEvent::Change => {
                    this.torrent_proxy_error = None;
                    cx.notify();
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );
        if let Proxy::Url(url) = &proxy {
            address.update(cx, |input, cx| input.set_value(url.to_string(), window, cx));
        }
        let typed = cx.subscribe_in(
            &address,
            window,
            |this: &mut Self, _, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter { .. } => this.save_custom(cx),
                InputEvent::Change => {
                    this.proxy_error = None;
                    cx.notify();
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );
        // 读系统代理要调操作系统的接口，在后台读
        let reading = cx.background_executor().spawn(async { Route::of_system() });
        let system = cx.spawn(async move |this, cx| {
            let route = reading.await;
            // 关了时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                this.system = Some(route);
                cx.notify();
            });
        });
        let mode = mode_of(&proxy);
        let web_states = tasks.read(cx).service().web_state();
        let web = web::WebSettings {
            state: web_states.borrow().clone(),
            channel: settings.web_channel,
            preference: settings.web_preference,
            latest: None,
            checking: None,
            previous: tasks.read(cx).service().web_previous(),
            error: None,
        };
        let web_watch = Self::watch_web(cx, web_states);
        SettingsView {
            tasks,
            home,
            proxy,
            mode,
            address,
            system: None,
            folder_error: None,
            proxy_error: None,
            theme_error: None,
            seeding: settings.seeding,
            upload_limit: settings.upload_limit,
            seeding_error: None,
            notices_error: None,
            updates,
            updates_error: None,
            _system: system,
            _typed: typed,
            _updates: watched,
            _tasks: tasks_watched,
            torrent_proxy_picked: settings.torrent_proxy.is_some(),
            torrent_proxy: settings.torrent_proxy,
            torrent_address,
            torrent_proxy_error: None,
            network_check: None,
            network_report: None,
            _torrent_typed: torrent_typed,
            section: Section::Download,
            scroll: ScrollHandle::new(),
            web,
            _web: web_watch,
        }
    }

    /// 换到 `section` 这一组，从顶上看起。
    pub fn show(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        self.scroll = ScrollHandle::new();
        cx.notify();
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<Div> = match self.section {
            Section::Download => vec![self.render_folder(cx), self.render_proxy(cx)],
            Section::Torrent => {
                let [seeding, upload] = self.render_seeding(cx);
                vec![
                    seeding,
                    upload,
                    self.render_torrent_proxy(cx),
                    self.render_network_check(cx),
                ]
            }
            Section::Cloud => vec![self.render_pikpak(cx)],
            Section::Web => vec![
                self.render_web_components(cx),
                self.render_web_channel(cx),
                self.render_web_preference(cx),
            ],
            Section::Appearance => vec![self.render_theme(cx)],
            Section::About => vec![self.render_updates(cx), self.render_about(cx)],
        };
        let last = rows.len() - 1;
        let rows = rows.into_iter().enumerate().map(|(index, row)| {
            row.when(index < last, |row| {
                row.border_b_1().border_color(theme::hairline())
            })
            .flex_none()
            .w_full()
            .max_w(px(MAX_WIDTH))
        });
        let nav = Section::ALL.map(|section| {
            nav_item(
                section.id(),
                section.label(),
                section == self.section,
                cx.listener(move |this, _, _, cx| this.show(section, cx)),
            )
        });
        ui::deck()
            .flex_1()
            .min_w_0()
            .child(ui::page_head(
                None,
                "设置",
                None,
                [ui::button(
                    "close-settings",
                    "关闭",
                    Tone::Ghost,
                    cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Dismissed)),
                )
                .into_any_element()],
            ))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        div()
                            .w(px(NAV_WIDTH))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .px(px(12.))
                            .pt(px(14.))
                            .border_r_1()
                            .border_color(theme::hairline())
                            .children(nav),
                    )
                    .child(
                        div()
                            .id("settings")
                            .track_scroll(&self.scroll)
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .px(px(32.))
                            .pt(px(8.))
                            .pb(px(24.))
                            .children(rows),
                    ),
            )
    }
}

/// 左边的一组：选中的底色略亮、字为正文色。
fn nav_item(
    id: impl Into<ElementId>,
    label: &'static str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> Stateful<Div> {
    let item = div()
        .id(id)
        .px(px(12.))
        .py(px(8.))
        .rounded(px(10.))
        .text_size(px(13.5))
        .cursor_pointer()
        .on_click(on_click)
        .child(label);
    if selected {
        item.bg(theme::alpha(theme::text(), 0.08))
            .text_color(theme::text())
            .font_weight(FontWeight::SEMIBOLD)
    } else {
        item.text_color(theme::muted())
            .hover(|style| style.text_color(theme::text()))
    }
}

/// 存着 `proxy` 时选中的方式。
fn mode_of(proxy: &Proxy) -> Mode {
    match proxy {
        Proxy::System => Mode::System,
        Proxy::Direct => Mode::Direct,
        Proxy::Url(_) => Mode::Custom,
    }
}

/// 一行：左边是名字与它管什么（`about`），右边是 `content`。
fn row(title: &'static str, about: Option<&'static str>, content: Div) -> Div {
    div()
        .flex()
        .gap(px(24.))
        .py(px(22.))
        .child(
            div()
                .w(px(LABEL_WIDTH))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                .children(about.map(|about| {
                    div()
                        .text_size(px(12.))
                        .line_height(px(ui::CAPTION_LINE))
                        .text_color(theme::faint())
                        .child(about)
                })),
        )
        .child(content.flex_1().min_w_0())
}
