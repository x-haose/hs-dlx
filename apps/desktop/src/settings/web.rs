//! 「网页视频」一组：yt-dlp 的组件（安装、版本与更新）、更新走的渠道、画质的取向、从哪个浏览器读登录信息。

use gpui_kit::{Context, Div, ParentElement as _, Styled as _, Task, div, px};
use hs_dlx_app::WebState;
use hs_dlx_app::web::Preference;
use hs_dlx_app::ytdlp::{Browser, Channel, CookieSource, Profile};
use std::path::PathBuf;

use super::{SettingsView, row};
use crate::disk_access;
use crate::format;
use crate::ui::{self, Tone};

/// 网页视频一组的状态。
pub(super) struct WebSettings {
    /// 组件此刻的状态，随服务里的变
    pub state: WebState,
    pub channel: Channel,
    pub preference: Preference,
    /// 存着的：解析时从这个浏览器的这个用户资料读 Cookie；None 为不读
    pub browser: Option<CookieSource>,
    /// 点选着的浏览器：存着的，或点了、有多个用户资料、还没选资料的；None 为不读
    pub shown: Option<Browser>,
    /// 点选着的浏览器的用户资料：打开设置页与点浏览器时在后台查，还没查完时为 None
    pub listing: Option<Listing>,
    /// 在后台查 `listing`：释放即取消（又点了别的浏览器时换掉）
    pub listing_task: Option<Task<()>>,
    /// 这台机器上有的浏览器（打开设置页时找）
    pub browsers: Vec<Browser>,
    /// 查到的：有新版时为它，已是最新为 None；没查过时整个为 None，没查成为原因
    pub latest: Option<Result<Option<String>, String>>,
    /// 在查新版：释放即取消
    pub checking: Option<Task<()>>,
    /// 可换回的上一个 yt-dlp 版本：组件的状态变了时重新取
    pub previous: Option<String>,
    /// 最近一次操作没成的原因
    pub error: Option<String>,
}

impl SettingsView {
    /// 看着组件的状态，变了即重画。
    pub(super) fn watch_web(
        cx: &mut Context<Self>,
        mut states: tokio::sync::watch::Receiver<WebState>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            while states.changed().await.is_ok() {
                let state = states.borrow_and_update().clone();
                let open = this.update(cx, |this, cx| {
                    this.web.state = state;
                    this.web.previous = this.tasks.read(cx).service().web_previous();
                    // 换了版本，查到的不再作数
                    this.web.latest = None;
                    cx.notify();
                });
                if open.is_err() {
                    return;
                }
            }
        })
    }

    fn install_web(&mut self, cx: &mut Context<Self>) {
        self.web.error = self
            .tasks
            .read(cx)
            .service()
            .install_web()
            .err()
            .map(|error| error.to_string());
        cx.notify();
    }

    fn check_web(&mut self, cx: &mut Context<Self>) {
        let handle = match self.tasks.read(cx).service().check_web_update() {
            Ok(handle) => handle,
            Err(error) => {
                self.web.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        self.web.error = None;
        self.web.checking = Some(cx.spawn(async move |this, cx| {
            let result = handle.await;
            // 关了时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                this.web.checking = None;
                this.web.latest = Some(match result {
                    Ok(Ok(latest)) => Ok(latest),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(join) => Err(format!("查的中途出错：{join}")),
                });
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn update_web(&mut self, version: String, cx: &mut Context<Self>) {
        self.web.error = self
            .tasks
            .read(cx)
            .service()
            .update_web(version)
            .err()
            .map(|error| error.to_string());
        cx.notify();
    }

    fn rollback_web(&mut self, cx: &mut Context<Self>) {
        self.web.error = self
            .tasks
            .read(cx)
            .service()
            .rollback_web()
            .err()
            .map(|error| error.to_string());
        cx.notify();
    }

    fn pick_channel(&mut self, channel: Channel, cx: &mut Context<Self>) {
        match self.tasks.read(cx).service().set_web_channel(channel) {
            Ok(()) => {
                self.web.channel = channel;
                self.web.latest = None;
                self.web.error = None;
            }
            Err(error) => self.web.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn pick_preference(&mut self, preference: Preference, cx: &mut Context<Self>) {
        match self.tasks.read(cx).service().set_web_preference(preference) {
            Ok(()) => {
                self.web.preference = preference;
                self.web.error = None;
            }
            Err(error) => self.web.error = Some(error.to_string()),
        }
        cx.notify();
    }

    /// 组件一行：没装时安装，在装时为在下哪一样与进度，装好了为版本与查新版、更新。
    pub(super) fn render_web_components(&self, cx: &mut Context<Self>) -> Div {
        let line = div().flex().items_center().gap(px(8.));
        let state = match &self.web.state {
            WebState::Unavailable => {
                line.child(ui::field("这个平台上用不了网页视频的解析").flex_1())
            }
            WebState::Missing | WebState::Failed(_) => {
                line.child(ui::field("没装").flex_1()).child(ui::button(
                    "web-install",
                    "安装",
                    Tone::Plain,
                    cx.listener(|this, _, _, cx| this.install_web(cx)),
                ))
            }
            WebState::Installing(progress) => div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(ui::steps(format::web_install_steps(progress)))
                .child(
                    line.child(ui::field(format::web_install(progress)).flex_1())
                        .child(ui::button(
                            "web-cancel-install",
                            "取消",
                            Tone::Ghost,
                            cx.listener(|this, _, _, cx| {
                                this.tasks.read(cx).service().cancel_web_install();
                            }),
                        )),
                ),
            WebState::Broken(_) => line.child(ui::field("运行不了").flex_1()).child(ui::button(
                "web-install",
                "重装",
                Tone::Plain,
                cx.listener(|this, _, _, cx| this.install_web(cx)),
            )),
            WebState::Ready(version) | WebState::UpdateFailed { version, .. } => {
                let line = line.child(ui::field(format!("yt-dlp {version}")).flex_1());
                match (&self.web.latest, &self.web.checking) {
                    (_, Some(_)) => line.child(ui::caption("在查…")),
                    (Some(Ok(Some(latest))), None) => {
                        let latest = latest.clone();
                        line.child(ui::button(
                            "web-update",
                            format!("更新到 {latest}"),
                            Tone::Plain,
                            cx.listener(move |this, _, _, cx| this.update_web(latest.clone(), cx)),
                        ))
                    }
                    (Some(Ok(None)), None) => line.child(ui::caption("已是最新")),
                    (None | Some(Err(_)), None) => line.child(ui::button(
                        "web-check",
                        "查新版",
                        Tone::Ghost,
                        cx.listener(|this, _, _, cx| this.check_web(cx)),
                    )),
                }
                // 新版出了问题时换回更新前的
                .children(self.web.previous.clone().map(|previous| {
                    ui::button(
                        "web-rollback",
                        format!("换回 {previous}"),
                        Tone::Ghost,
                        cx.listener(|this, _, _, cx| this.rollback_web(cx)),
                    )
                }))
                // 文件坏了而进程还起得来时解析报错，没有别的办法认出：由用户重装
                .child(ui::button(
                    "web-reinstall",
                    "重装",
                    Tone::Ghost,
                    cx.listener(|this, _, _, cx| this.install_web(cx)),
                ))
            }
        };
        let failed = match (&self.web.state, &self.web.latest) {
            (WebState::Failed(reason), _) => Some(format!("上次没装成：{reason}")),
            (WebState::Broken(reason), _) => Some(format!("组件运行不了，要重装：{reason}")),
            (WebState::UpdateFailed { reason, .. }, _) => Some(format!("上次{reason}")),
            (_, Some(Err(reason))) => Some(format!("没查成：{reason}")),
            _ => None,
        };
        row(
            "组件",
            Some("要下载 yt-dlp，约 11 MB；Python 与 QuickJS-NG 随应用带"),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(state)
                .children(failed.map(ui::error_text))
                .children(self.web.error.clone().map(ui::error_text)),
        )
    }

    /// 渠道一行：稳定版与每日构建。
    pub(super) fn render_web_channel(&self, cx: &mut Context<Self>) -> Div {
        let chip =
            |id: &'static str, label: &'static str, value: Channel, cx: &mut Context<Self>| {
                ui::chip(
                    id,
                    label,
                    self.web.channel == value,
                    cx.listener(move |this, _, _, cx| this.pick_channel(value, cx)),
                )
            };
        row(
            "更新渠道",
            None,
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(chip("web-stable", "稳定版", Channel::Stable, cx))
                        .child(chip("web-nightly", "每日构建", Channel::Nightly, cx)),
                )
                .child(ui::caption(
                    "稳定版约每月一版；网站改版后的修复先到每日构建",
                )),
        )
    }

    /// 点了一个浏览器（None 为不读，随即存下）：查完它的用户资料后，没有用户资料之分或只有一个资料的存下；有多个的
    /// 等选了资料再存（见 [`SettingsView::pick_profile`]），已存着它的照旧；读不了它的数据或列不出资料的不存。
    fn pick_browser(&mut self, browser: Option<Browser>, cx: &mut Context<Self>) {
        self.web.shown = browser;
        self.web.listing = None;
        self.web.listing_task = browser.map(|browser| Self::list_profiles(browser, true, cx));
        if browser.is_none() {
            self.save_browser(None, cx);
        }
        cx.notify();
    }

    /// 在后台查 `browser` 的用户资料，查完放进 [`WebSettings::listing`]；`save` 时没有或只有一个资料的随即存下。
    pub(super) fn list_profiles(browser: Browser, save: bool, cx: &mut Context<Self>) -> Task<()> {
        let listing = cx
            .background_executor()
            .spawn(async move { Listing::of(browser) });
        cx.spawn(async move |this, cx| {
            let listing = listing.await;
            // 设置页关了时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                if save
                    && let Listing::Profiles(profiles) = &listing
                    && profiles.len() <= 1
                {
                    let profile = profiles.first().map(|profile| profile.dir.clone());
                    this.save_browser(Some(CookieSource { browser, profile }), cx);
                }
                this.web.listing = Some(listing);
                cx.notify();
            });
        })
    }

    /// 选了点选着的浏览器的用户资料 `dir`：存下。
    fn pick_profile(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if let Some(browser) = self.web.shown {
            self.save_browser(
                Some(CookieSource {
                    browser,
                    profile: Some(dir),
                }),
                cx,
            );
        }
        cx.notify();
    }

    fn save_browser(&mut self, source: Option<CookieSource>, cx: &mut Context<Self>) {
        match self
            .tasks
            .read(cx)
            .service()
            .set_web_browser(source.clone())
        {
            Ok(()) => {
                self.web.browser = source;
                self.web.error = None;
                self.tasks
                    .update(cx, |tasks, cx| tasks.check_cookie_access(cx));
            }
            Err(error) => self.web.error = Some(error.to_string()),
        }
    }

    /// 登录信息一行：不读，或这台机器上有的浏览器之一（存着的不在其中时也列出）。
    pub(super) fn render_web_browser(&self, cx: &mut Context<Self>) -> Div {
        let mut choices: Vec<Option<Browser>> = vec![None];
        choices.extend(self.web.browsers.iter().copied().map(Some));
        if let Some(saved) = self.web.browser.as_ref().map(|source| source.browser)
            && !self.web.browsers.contains(&saved)
        {
            choices.push(Some(saved));
        }
        let chips = choices.into_iter().map(|choice| {
            let (id, label) = match choice {
                None => ("web-browser-none".to_owned(), "不读"),
                Some(browser) => (format!("web-browser-{}", browser.code()), browser.label()),
            };
            ui::chip(
                id,
                label,
                self.web.shown == choice,
                cx.listener(move |this, _, _, cx| this.pick_browser(choice, cx)),
            )
        });
        let platform = if cfg!(target_os = "macos") {
            "Chrome、Edge 等读时系统弹钥匙串的授权，点「允许」（解析进程重开后再弹一次；Firefox 不弹）；Safari 要在「系统设置 → 隐私与安全性 → 完全磁盘访问权限」里打开 hs-dlx"
        } else if cfg!(windows) {
            "Chrome、Edge、Brave 把 Cookie 加密成了只有它们自己解得开的，读不了，不列出"
        } else {
            "Chrome 等的 Cookie 用系统的密钥环（GNOME 密钥环或 KWallet）解开，读时它要已解锁"
        };
        row(
            "登录信息",
            Some(
                "解析时从这个浏览器读你登录后的 Cookie，每次现读；用账号下载 YouTube 可能被暂时或永久封号",
            ),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(div().flex().flex_wrap().gap(px(8.)).children(chips))
                .children(self.render_listing(cx))
                .child(ui::caption(platform)),
        )
    }

    /// 点选着的浏览器的用户资料：读不了它的数据时是怎么开权限（开了、重开后要回来再点它），列不出时是原因；有多个、
    /// 或存着的资料已不在时逐个列出供选，还没选时写明选了才生效与现在生效的是哪个。没有或只有一个（且是存着的）时没有
    /// 这一块。
    fn render_listing(&self, cx: &mut Context<Self>) -> Option<Div> {
        let browser = self.web.shown?;
        let profiles = match self.web.listing.as_ref()? {
            Listing::Denied => {
                return Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(disk_access::card(browser))
                        .child(ui::caption(format!(
                            "重开后回到这里再点 {}（现在{}）",
                            browser.label(),
                            self.in_effect()
                        ))),
                );
            }
            Listing::Failed(reason) => return Some(ui::error_text(reason.clone())),
            Listing::Profiles(profiles) => profiles,
        };
        let saved = self
            .web
            .browser
            .as_ref()
            .filter(|source| source.browser == browser)
            .and_then(|source| source.profile.as_ref());
        let gone = saved.filter(|dir| !profiles.iter().any(|profile| &profile.dir == *dir));
        if profiles.len() <= 1 && gone.is_none() {
            return None;
        }
        let chips = profiles.iter().enumerate().map(|(index, profile)| {
            let label = match &profile.account {
                Some(account) => format!("{} · {account}", profile.name),
                None => profile.name.clone(),
            };
            let dir = profile.dir.clone();
            ui::chip(
                format!("web-profile-{index}"),
                label,
                saved == Some(&profile.dir),
                cx.listener(move |this, _, _, cx| this.pick_profile(dir.clone(), cx)),
            )
        });
        let hint = match (gone, saved) {
            (Some(dir), _) => Some(format!(
                "存着的用户资料 {} 已不在了：重选一个",
                dir.display()
            )),
            (None, Some(_)) => None,
            (None, None) => Some(format!(
                "{} 有多个用户资料：选你登录了要下载的网站的那个，选了才生效（现在{}）",
                browser.label(),
                self.in_effect()
            )),
        };
        let hint = hint.map(ui::caption);
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .children(hint)
                .child(div().flex().flex_wrap().gap(px(8.)).children(chips)),
        )
    }

    /// 现在生效的读法，接在「现在」后面：不读，或读哪个浏览器的哪个用户资料。
    fn in_effect(&self) -> String {
        match &self.web.browser {
            None => "不读".to_owned(),
            Some(CookieSource {
                browser,
                profile: None,
            }) => format!("读 {}", browser.label()),
            Some(CookieSource {
                browser,
                profile: Some(dir),
            }) => format!(
                "读 {} 的 {}",
                browser.label(),
                dir.file_name().map_or_else(
                    || dir.display().to_string(),
                    |name| name.to_string_lossy().into_owned()
                )
            ),
        }
    }

    /// 画质一行：兼容优先与画质优先。
    pub(super) fn render_web_preference(&self, cx: &mut Context<Self>) -> Div {
        let chip =
            |id: &'static str, label: &'static str, value: Preference, cx: &mut Context<Self>| {
                ui::chip(
                    id,
                    label,
                    self.web.preference == value,
                    cx.listener(move |this, _, _, cx| this.pick_preference(value, cx)),
                )
            };
        row(
            "画质",
            None,
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(chip("web-compatible", "兼容优先", Preference::Compatible, cx))
                        .child(chip("web-quality", "画质优先", Preference::Quality, cx)),
                )
                .child(ui::caption(
                    "兼容优先：同一清晰度先选 H.264 + AAC，各平台自带的播放器都能放；画质优先多为 AV1 或 VP9",
                )),
        )
    }
}

/// 点选着的浏览器的用户资料查到的样子。
pub(super) enum Listing {
    /// 本进程没有权限读它的数据（见 [`Browser::access`]）
    Denied,
    /// 它的用户资料（见 [`Browser::profiles`]）
    Profiles(Vec<Profile>),
    /// 列不出的原因
    Failed(String),
}

impl Listing {
    /// 现查 `browser`：先看读不读得了它的数据，再列用户资料。要读盘，不在界面线程上调。
    fn of(browser: Browser) -> Listing {
        if browser.access().is_err() {
            return Listing::Denied;
        }
        match browser.profiles() {
            Ok(profiles) => Listing::Profiles(profiles),
            Err(reason) => Listing::Failed(reason),
        }
    }
}
