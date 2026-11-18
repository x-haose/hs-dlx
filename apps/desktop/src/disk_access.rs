//! 读浏览器的数据没有权限时的说明与去处：macOS 上直接打开系统设置里「完全磁盘访问权限」那一页，本应用已在列表里。

use gpui_kit::{App, Div, ParentElement as _, Styled as _, div, px};
use hs_dlx_app::ytdlp::Browser;

use crate::ui::{self, Tone};

/// 系统设置里「隐私与安全性 → 完全磁盘访问权限」那一页
const FULL_DISK_ACCESS: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles";

/// 只受完全磁盘访问权限保护的目录（相对用户目录）：系统各项隐私授权的记录
const PROTECTED: &str = "Library/Application Support/com.apple.TCC";

/// `browser` 的数据读不了时的警告：怎么办，macOS 上另有打开那一页设置的按钮。
pub fn card(browser: Browser) -> Div {
    let card = ui::note(browser.denied_hint(), true)
        .flex()
        .flex_col()
        .gap(px(12.));
    if !cfg!(target_os = "macos") {
        return card;
    }
    card.child(div().flex().child(ui::button(
        "open-full-disk-access",
        "打开系统设置",
        Tone::Plain,
        |_, _, cx| open_settings(cx),
    )))
}

/// 先列一次只受完全磁盘访问权限保护的目录，再打开那一页设置：没有权限时系统拒绝这次读，同时把本应用加进那一页的列表
/// （开关关着），用户打开开关即可，不用自己点「+」找应用。读的结果不用：有没有权限以 [`Browser::access`] 为准。
fn open_settings(cx: &mut App) {
    if let Some(home) = dirs::home_dir() {
        let _listed = std::fs::read_dir(home.join(PROTECTED));
    }
    cx.open_url(FULL_DISK_ACCESS);
}
