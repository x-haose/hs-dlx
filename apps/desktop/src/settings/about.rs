//! 「更新与关于」一组：自动检查更新、新版本的下载与安装、版本与第三方声明。

use std::path::PathBuf;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{Context, Div, IntoElement as _, ParentElement as _, Styled as _, div, px};

use super::{SettingsView, row};
use crate::updates::UpdateState;
use crate::widgets::{self, Button, Kind, Stat, Toward};

/// 随安装包附上的第三方声明目录（打包时由 cargo-packager 当作资源放进，见 apps/desktop/Cargo.toml）：macOS 在
/// `.app/Contents/Resources/LICENSES`，Windows 在程序旁的 `LICENSES`，Linux 在 `/usr/lib/<程序名>/LICENSES`（deb）。找不到（从源码直接运行）时为原因。
fn installed_notices() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|error| format!("找不到程序所在的位置：{error}"))?;
    let beside = exe.parent().ok_or("程序没有所在的目录")?;
    let folder = if cfg!(target_os = "macos") {
        beside.join("../Resources/LICENSES")
    } else if cfg!(target_os = "windows") {
        beside.join("LICENSES")
    } else {
        let name = exe.file_name().ok_or("程序没有文件名")?;
        beside.join("../lib").join(name).join("LICENSES")
    };
    if folder.join("NOTICE.txt").is_file() {
        Ok(folder)
    } else {
        Err("这不是安装包里的程序，第三方声明在源码的 apps/desktop/LICENSES 下".to_owned())
    }
}

impl SettingsView {
    /// 换「自动检查更新」：存进了才选中。
    fn pick_auto_update(&mut self, auto: bool, cx: &mut Context<Self>) {
        self.updates_error = self
            .updates
            .update(cx, |updates, cx| updates.set_auto(auto, cx))
            .err();
        cx.notify();
    }

    /// 更新一行：自动检查的开关，与查到的新版本、下载与安装到了哪一步。
    pub(super) fn render_updates(&self, cx: &mut Context<Self>) -> Div {
        let (auto, state, restarting) = {
            let updates = self.updates.read(cx);
            (
                updates.auto(),
                updates.state().clone(),
                updates.restarting(),
            )
        };
        let chips = div()
            .flex()
            .flex_wrap()
            .gap(px(8.))
            .child(widgets::chip(
                "update-auto",
                "启动时与每天检查",
                auto,
                Toward::Right,
                cx.listener(|this, _, _, cx| this.pick_auto_update(true, cx)),
            ))
            .child(widgets::chip(
                "update-manual",
                "不自动检查",
                !auto,
                Toward::Right,
                cx.listener(|this, _, _, cx| this.pick_auto_update(false, cx)),
            ));
        let check = |cx: &mut Context<Self>| {
            Button::new("update-check", "检查更新", Kind::Secondary)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.updates.update(cx, |updates, cx| updates.check(cx));
                }))
                .into_any_element()
        };
        let install = |label: &'static str, cx: &mut Context<Self>| {
            Button::new("update-install", label, Kind::Primary)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.updates.update(cx, |updates, cx| updates.install(cx));
                }))
                .into_any_element()
        };
        let page = |url: &hs_m3u8_core::Url, label: &'static str, cx: &mut Context<Self>| {
            let url = url.to_string();
            Button::new("update-page", label, Kind::Secondary)
                .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                .into_any_element()
        };
        let mut lines: Vec<gpui_kit::AnyElement> = Vec::new();
        let mut buttons: Vec<gpui_kit::AnyElement> = Vec::new();
        match &state {
            UpdateState::Idle => buttons.push(check(cx)),
            UpdateState::Checking => lines.push(widgets::caption("正在查…").into_any_element()),
            UpdateState::Latest => {
                lines.push(widgets::caption("已是最新").into_any_element());
                buttons.push(check(cx));
            }
            UpdateState::Available(release) => {
                lines.push(
                    widgets::stats(vec![Stat::new("新版本", release.version.to_string())], 1)
                        .into_any_element(),
                );
                match &release.package {
                    Some(package) => {
                        lines.push(
                            widgets::caption(format!(
                                "下载 {}；装好后退出并打开新版本，运行中的任务会暂停，要在新版本里点继续",
                                crate::format::bytes(package.size)
                            ))
                            .into_any_element(),
                        );
                        buttons.push(install("更新并重启", cx));
                    }
                    None => lines.push(
                        widgets::caption("这个平台不能自动更新，到下载页下载新版本")
                            .into_any_element(),
                    ),
                }
                buttons.push(page(&release.page, "更新内容", cx));
            }
            UpdateState::Downloading { release, received } => {
                let total = release.package.as_ref().map_or(0, |package| package.size);
                lines.push(
                    widgets::caption(format!(
                        "下载 {}：{} / {}",
                        release.version,
                        crate::format::bytes(*received),
                        crate::format::bytes(total)
                    ))
                    .into_any_element(),
                );
            }
            UpdateState::Installing(version) => {
                lines.push(widgets::caption(format!("正在装 {version}")).into_any_element());
            }
            UpdateState::Installed(version) => {
                lines.push(
                    widgets::caption(match (restarting, hs_dlx_app::INSTALLED_BEFORE_RESTART) {
                        (true, _) => format!("{version}：退出后打开新版本"),
                        (false, true) => format!("{version} 已装好，重启后用新版本"),
                        (false, false) => format!("{version} 已下载并验过：点「现在重启」时装上"),
                    })
                    .into_any_element(),
                );
                if !restarting {
                    buttons.push(
                        Button::new("update-restart", "现在重启", Kind::Primary)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.updates
                                    .update(cx, |updates, cx| updates.restart_now(cx));
                            }))
                            .into_any_element(),
                    );
                }
            }
            UpdateState::Stranded(reason) => {
                lines.push(widgets::error_text(reason.clone()).into_any_element());
            }
            UpdateState::Failed { release, reason } => {
                lines.push(widgets::error_text(reason.clone()).into_any_element());
                match release {
                    Some(release) => {
                        if release.package.is_some() {
                            buttons.push(install("再试一次", cx));
                        }
                        buttons.push(page(&release.page, "到下载页下载", cx));
                    }
                    None => buttons.push(check(cx)),
                }
            }
        }
        row(
            "更新",
            Some("经「下载」里设的代理查 GitHub 上的新版本"),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(chips)
                .children(lines)
                .children(self.updates_error.clone().map(widgets::error_text))
                .when(!buttons.is_empty(), |content| {
                    content.child(div().flex().flex_wrap().gap(px(8.)).children(buttons))
                }),
        )
    }

    /// 在访达（资源管理器、文件管理器）里打开随安装包附上的第三方声明目录；不是从安装包运行时写明原因。
    fn open_notices(&mut self, cx: &mut Context<Self>) {
        match installed_notices() {
            Ok(folder) => {
                self.notices_error = None;
                cx.open_with_system(&folder);
            }
            Err(reason) => self.notices_error = Some(reason),
        }
        cx.notify();
    }

    /// 「关于」一行：版本与第三方声明。
    pub(super) fn render_about(&self, cx: &mut Context<Self>) -> Div {
        row(
            "关于",
            None,
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(12.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .child(concat!("hs-dlx ", env!("CARGO_PKG_VERSION"))),
                        )
                        .child(
                            Button::new("settings-notices", "第三方声明", Kind::Secondary)
                                .on_click(cx.listener(|this, _, _, cx| this.open_notices(cx))),
                        ),
                )
                .children(self.notices_error.clone().map(widgets::error_text)),
        )
    }
}
