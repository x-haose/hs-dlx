//! 读浏览器的数据没有权限时的说明与去处：macOS 上直接打开系统设置里「完全磁盘访问权限」那一页。

use gpui_kit::{Div, ParentElement as _, Styled as _, div, px};
use hs_dlx_app::ytdlp::Browser;

use crate::ui::{self, Tone};

/// 系统设置里「隐私与安全性 → 完全磁盘访问权限」那一页
const FULL_DISK_ACCESS: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_AllFiles";

/// `browser` 的数据读不了时的警告：怎么办，macOS 上另有打开那一页设置的按钮（回到窗口时重查，见
/// [`crate::tasks::Tasks::check_cookie_access`]，给了权限即不再显示）。
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
        |_, _, cx| cx.open_url(FULL_DISK_ACCESS),
    )))
}
