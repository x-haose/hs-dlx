//! 读浏览器的数据没有权限时的说明与去处：macOS 上直接打开系统设置里「完全磁盘访问权限」那一页，本应用已在列表里。

use gpui_kit::{App, Div, ParentElement as _, Stateful, Styled as _, div, px};
use hs_dlx_app::ytdlp::Browser;

use crate::ui::{self, Tone};

/// 系统设置里「隐私与安全性 → 完全磁盘访问权限」那一页
const FULL_DISK_ACCESS: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles";

/// 只受完全磁盘访问权限保护的目录（相对用户目录）：Safari 的数据。不在时系统不列，说明里有点「+」加的办法
const PROTECTED: &str = "Library/Safari";

/// `browser` 的数据读不了时的警告：怎么办，macOS 上另有打开那一页设置的按钮（见 [`button`]）。
pub fn card(browser: Browser) -> Div {
    ui::note(browser.denied_hint(), true)
        .flex()
        .flex_col()
        .gap(px(12.))
        .children(button().map(|button| div().flex().child(button)))
}

/// 打开系统设置里「完全磁盘访问权限」那一页的按钮；不是 macOS 时为 None（没有这一页）。失败说明里已写了怎么办时单
/// 用它。
pub fn button() -> Option<Stateful<Div>> {
    cfg!(target_os = "macos").then(|| {
        ui::button(
            "open-full-disk-access",
            "打开系统设置",
            Tone::Plain,
            |_, _, cx| open_settings(cx),
        )
    })
}

/// 先列一次只受完全磁盘访问权限保护的目录，再打开那一页设置：没有权限时系统拒绝这次读，同时把本应用加进那一页的列表
/// （开关关着），用户打开开关即可，不用自己点「+」找应用。读的结果不用：有没有权限以 [`Browser::access`] 为准。
fn open_settings(cx: &mut App) {
    if let Some(home) = dirs::home_dir() {
        let _listed = std::fs::read_dir(home.join(PROTECTED));
    }
    cx.open_url(FULL_DISK_ACCESS);
}
