//! 「下载」一组：新建任务默认保存到哪、请求经哪个代理。

use gpui_kit::{Context, Div, ParentElement as _, PathPromptOptions, Styled as _, div, px};
use hs_m3u8_core::{Proxy, Url};

use super::{Mode, SettingsView, mode_of, row};
use crate::route::Route;
use crate::web_route::WebRoute;
use crate::widgets::{self, Button, Kind, Stat, Toward};

impl SettingsView {
    /// 选了一种方式：跟随系统、直连即存，存进了才选中；指定代理要填了端口、核对过再存。
    fn pick(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.proxy_error = None;
        if mode != Mode::Custom {
            // 在做的核对回来时不再存
            self.address.update(cx, |editor, cx| editor.cancel(cx));
        }
        match mode {
            Mode::System => self.save(Proxy::System, cx),
            Mode::Direct => self.save(Proxy::Direct, cx),
            Mode::Custom => self.mode = Mode::Custom,
        }
        cx.notify();
    }

    /// 存核对过的代理地址 `url`。
    pub(super) fn save_custom(&mut self, url: Url, cx: &mut Context<Self>) {
        self.save(Proxy::Url(url), cx);
        cx.notify();
    }

    /// 存 `proxy`：存进了才换成它，没存进时选中的方式回到存着的。
    fn save(&mut self, proxy: Proxy, cx: &mut Context<Self>) {
        match self.tasks.read(cx).service().set_proxy(&proxy) {
            Ok(()) => {
                if let Some(route) = WebRoute::existing(cx) {
                    route.update(cx, |route, _| route.setting_changed(&proxy));
                }
                self.mode = mode_of(&proxy);
                self.proxy = proxy;
            }
            Err(error) => {
                self.mode = mode_of(&self.proxy);
                self.proxy_error = Some(error.to_string());
            }
        }
    }

    fn change_folder(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("存到这里".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = picked.await;
            // 选择期间视图被关掉时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(Some(mut paths))) if !paths.is_empty() => {
                        let folder = paths.swap_remove(0);
                        this.folder_error = this
                            .tasks
                            .update(cx, |tasks, cx| tasks.set_folder(folder, cx))
                            .err();
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => this.folder_error = Some(format!("打不开目录选择：{error}")),
                    Err(_) => this.folder_error = Some("目录选择没有返回结果".to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_proxy(&self, cx: &mut Context<Self>) -> Div {
        // 选了指定代理、还没存进地址，或改了地址还没保存：写明此刻照旧按什么走
        let edited = self.address.read(cx).edited();
        let unsaved = match (self.mode, &self.proxy) {
            (Mode::Custom, Proxy::System) => Some("保存之前照旧跟随系统".to_owned()),
            (Mode::Custom, Proxy::Direct) => Some("保存之前照旧直连".to_owned()),
            (Mode::Custom, Proxy::Url(url)) if edited => Some(format!(
                "改的还没保存，照旧经 {}",
                hs_m3u8_core::bare_url(url)
            )),
            _ => None,
        };
        let chip = |id: &'static str, label: &'static str, mode: Mode, cx: &mut Context<Self>| {
            widgets::chip(
                id,
                label,
                self.mode == mode,
                Toward::Right,
                cx.listener(move |this, _, _, cx| this.pick(mode, cx)),
            )
        };
        let now = |value: String| widgets::stats(vec![Stat::text("现在", value)], 1);
        let line = match self.mode {
            Mode::System => self.system.as_ref().map(|route| match route {
                Route::Proxy(proxy, source) => now(format!("经 {proxy}（{source}）")),
                Route::Direct(why) => now(format!("直连 · {why}")),
                Route::Broken(why) => widgets::error_text(why.clone()),
            }),
            Mode::Direct => Some(now("不经任何代理".to_owned())),
            Mode::Custom => Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(self.address.clone())
                    .children(unsaved.map(widgets::caption)),
            ),
        };
        row(
            "代理",
            Some("探测、下载与嗅探页的网页经哪里出去；BT 不经它，另在「BT」里设"),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(chip("proxy-system", "跟随系统", Mode::System, cx))
                    .child(chip("proxy-direct", "直连", Mode::Direct, cx))
                    .child(chip("proxy-custom", "指定代理", Mode::Custom, cx)),
            )
            .children(line)
            .children(self.proxy_error.clone().map(widgets::error_text))
            .child(widgets::caption(
                match hs_dlx_webview::custom_proxy_supported() {
                    Ok(()) if hs_dlx_webview::PROXY_FIXED_FOR_PROCESS => "在下的任务暂停再继续后才换；嗅探页的网页在直连与指定代理之间立即换，与跟随系统之间换要重启应用".to_owned(),
                    Ok(()) => "在下的任务暂停再继续后才换；嗅探页的网页在直连与指定代理之间立即换，与跟随系统之间换要把它们都关掉再开".to_owned(),
                    Err(reason) => format!(
                        "在下的任务暂停再继续后才换；{reason}，直连或指定代理时嗅探页打不开网页"
                    ),
                },
            )),
        )
    }

    /// 「保存到」一行。
    pub(super) fn render_folder(&self, cx: &mut Context<Self>) -> Div {
        row(
            "保存到",
            Some("新建任务默认存到这里，每个任务可以另选"),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            widgets::field(crate::format::path(
                                self.tasks.read(cx).folder(),
                                self.home.as_deref(),
                            ))
                            .flex_1(),
                        )
                        .child(
                            Button::new("settings-folder", "更改", Kind::Secondary)
                                .on_click(cx.listener(|this, _, _, cx| this.change_folder(cx))),
                        ),
                )
                .children(self.folder_error.clone().map(widgets::error_text)),
        )
    }
}
