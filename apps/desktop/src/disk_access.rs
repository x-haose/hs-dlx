//! 读浏览器的数据没有权限时的说明与去处：macOS 上直接打开系统设置里「完全磁盘访问权限」那一页，并在 Finder 里选中
//! 本应用，列表里没有它时拖进去。

use std::path::{Path, PathBuf};

use gpui_kit::{App, Div, ParentElement as _, Styled as _, div, px};
use hs_dlx_app::ytdlp::Browser;

use crate::widgets::{self, Button, Kind};

/// 系统设置里「隐私与安全性 → 完全磁盘访问权限」那一页
const FULL_DISK_ACCESS: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles";

/// 只受完全磁盘访问权限保护的目录（相对用户目录）：Safari 的数据
const PROTECTED: &str = "Library/Safari";

/// `browser` 的数据读不了时的警告：怎么办，macOS 上另有去开权限的按钮（见 [`button`]）。
pub fn card(browser: Browser) -> Div {
    widgets::note(browser.denied_hint(), true)
        .flex()
        .flex_col()
        .gap(px(12.))
        .children(button())
}

/// 打开系统设置里「完全磁盘访问权限」那一页的按钮与它做了什么；不是 macOS 时为 None（没有这一页）。失败说明里已写了
/// 怎么办时单用它。
pub fn button() -> Option<Div> {
    cfg!(target_os = "macos").then(|| {
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div().flex().child(
                    Button::new("open-full-disk-access", "打开系统设置", Kind::Secondary)
                        .on_click(|_, _, cx| open_settings(cx)),
                ),
            )
            .child(widgets::caption(
                "同时在 Finder 里选中 hs-dlx：列表里没有它时，把它拖进列表",
            ))
    })
}

/// 先列一次只受完全磁盘访问权限保护的目录（被拒时系统多会把本应用加进那一页的列表、开关关着；之前给过又删掉的不一定
/// 再加），再打开那一页设置，并在 Finder 里选中本应用的 .app 供拖进列表。读的结果不用：有没有权限以
/// [`Browser::access`] 为准。
fn open_settings(cx: &mut App) {
    if let Some(home) = dirs::home_dir() {
        let _listed = std::fs::read_dir(home.join(PROTECTED));
    }
    cx.open_url(FULL_DISK_ACCESS);
    if let Some(bundle) = app_bundle() {
        cx.reveal_path(&bundle);
    }
}

/// 本应用的 .app 包（可执行文件在 `<包>/Contents/MacOS/` 里）；不在 .app 里运行（开发时直接跑编出的程序，权限算在
/// 启动它的终端上）或取不到可执行文件的位置时为 None，这时只打开设置页。
fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}
