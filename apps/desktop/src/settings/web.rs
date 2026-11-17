//! 「网页视频」一组：yt-dlp 的组件（安装、版本与更新）、更新走的渠道、画质的取向。

use gpui_kit::{Context, Div, ParentElement as _, Styled as _, Task, div, px};
use hs_dlx_app::WebState;
use hs_dlx_app::web::Preference;
use hs_dlx_app::ytdlp::Channel;

use super::{SettingsView, row};
use crate::format;
use crate::ui::{self, Tone};

/// 网页视频一组的状态。
pub(super) struct WebSettings {
    /// 组件此刻的状态，随服务里的变
    pub state: WebState,
    pub channel: Channel,
    pub preference: Preference,
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
