//! 解析时从哪个浏览器读登录后的 Cookie（yt-dlp 的 `cookiesfrombrowser`）。每次解析现读：浏览器里登录着、Cookie 轮换
//! 了也是新的。
//!
//! 能不能读因平台而异：macOS 上 Chromium 系读时系统弹钥匙串的授权（一个解析进程里给过一次即留着），Safari 与受系统
//! 保护的浏览器数据目录要给应用完全磁盘访问权限（见 [`Browser::access`]）；Linux 上 Chromium 系的钥匙在系统的密钥环里；Windows 上 Chrome、Edge、Brave 把 Cookie 加密成只有
//! 它们自己解得开的（app-bound encryption），yt-dlp 读不了，不列出；Firefox 各平台都能读。读不了时解析报
//! [`crate::ExtractError::Cookies`]。

use std::fmt;
use std::io;
use std::path::PathBuf;

/// yt-dlp 能从中读 Cookie 的浏览器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Browser {
    Chrome,
    Edge,
    Firefox,
    /// 只在 macOS 上有
    Safari,
    Brave,
    Chromium,
    Opera,
    Vivaldi,
    Whale,
}

impl Browser {
    pub const ALL: [Browser; 9] = [
        Browser::Chrome,
        Browser::Edge,
        Browser::Firefox,
        Browser::Safari,
        Browser::Brave,
        Browser::Chromium,
        Browser::Opera,
        Browser::Vivaldi,
        Browser::Whale,
    ];

    /// yt-dlp 里的名字，也是存进设置的写法。
    pub fn code(self) -> &'static str {
        match self {
            Browser::Chrome => "chrome",
            Browser::Edge => "edge",
            Browser::Firefox => "firefox",
            Browser::Safari => "safari",
            Browser::Brave => "brave",
            Browser::Chromium => "chromium",
            Browser::Opera => "opera",
            Browser::Vivaldi => "vivaldi",
            Browser::Whale => "whale",
        }
    }

    /// [`Browser::code`] 的反面；认不出时为 None。
    pub fn from_code(code: &str) -> Option<Browser> {
        Browser::ALL
            .into_iter()
            .find(|browser| browser.code() == code)
    }

    /// 给人看的名字。
    pub fn label(self) -> &'static str {
        match self {
            Browser::Chrome => "Chrome",
            Browser::Edge => "Edge",
            Browser::Firefox => "Firefox",
            Browser::Safari => "Safari",
            Browser::Brave => "Brave",
            Browser::Chromium => "Chromium",
            Browser::Opera => "Opera",
            Browser::Vivaldi => "Vivaldi",
            Browser::Whale => "Whale",
        }
    }

    /// 这台机器上有它的数据目录（用过它）、yt-dlp 读得了的浏览器，按 [`Browser::ALL`] 的次序。目录的位置同 yt-dlp
    /// 找 Cookie 的地方（见 [`Browser::data_dirs`]）；Safari 在 macOS 上总算有（它的目录在应用沙盒里，没有完全磁盘访问
    /// 权限时看不到）。
    pub fn installed() -> Vec<Browser> {
        Browser::ALL
            .into_iter()
            .filter(|browser| browser.readable() && browser.present())
            .collect()
    }

    /// yt-dlp 在这个平台上读得了：Windows 上 Chrome、Edge、Brave 的 Cookie 加密成了只有它们自己解得开的。
    fn readable(self) -> bool {
        !(cfg!(windows) && matches!(self, Browser::Chrome | Browser::Edge | Browser::Brave))
    }

    /// 本进程现在列不列得出它的各个数据目录（见 [`Browser::data_dirs`]）：在却没有权限列时为 [`Denied`]。macOS 上
    /// 浏览器的数据目录受系统保护，没给本应用完全磁盘访问权限时系统不弹窗、直接拒绝；yt-dlp 搜 Cookie 文件时把列不出
    /// 的目录当成空的，报的是没找到。不在的目录与别的错误不算：那时以 yt-dlp 读的结果为准。
    pub fn access(self) -> Result<(), Denied> {
        for dir in self.data_dirs() {
            if let Err(error) = std::fs::read_dir(&dir)
                && error.kind() == io::ErrorKind::PermissionDenied
            {
                return Err(Denied { dir, error });
            }
        }
        Ok(())
    }

    /// 没有权限读它的数据时怎么办。
    pub fn denied_hint(self) -> String {
        let name = self.label();
        if cfg!(target_os = "macos") {
            format!(
                "macOS 不让 hs-dlx 读 {name} 的数据：在「系统设置 → 隐私与安全性 → 完全磁盘访问权限」里打开 hs-dlx（列表里没有时点「+」加上）"
            )
        } else {
            format!("没有权限读 {name} 的数据目录")
        }
    }

    fn present(self) -> bool {
        if self == Browser::Safari {
            return cfg!(target_os = "macos");
        }
        self.data_dirs().iter().any(|dir| dir.is_dir())
    }

    /// yt-dlp 在这个平台上找它的 Cookie 的目录（Chromium 系为用户数据目录，Firefox 为几处放配置的目录）；Safari 与
    /// 这个平台上没有的为空。
    pub fn data_dirs(self) -> Vec<PathBuf> {
        let home = dirs::home_dir();
        let join = |base: Option<PathBuf>, path: &str| base.map(|base| base.join(path));
        let dirs: Vec<Option<PathBuf>> = if cfg!(windows) {
            let local = dirs::data_local_dir();
            let roaming = dirs::data_dir();
            match self {
                Browser::Firefox => vec![
                    join(roaming.clone(), r"Mozilla\Firefox\Profiles"),
                    join(
                        local.clone(),
                        r"Packages\Mozilla.Firefox_n80bbvh6b1yt2\LocalCache\Roaming\Mozilla\Firefox\Profiles",
                    ),
                ],
                Browser::Chrome => vec![join(local, r"Google\Chrome\User Data")],
                Browser::Edge => vec![join(local, r"Microsoft\Edge\User Data")],
                Browser::Brave => vec![join(local, r"BraveSoftware\Brave-Browser\User Data")],
                Browser::Chromium => vec![join(local, r"Chromium\User Data")],
                Browser::Opera => vec![join(roaming, r"Opera Software\Opera Stable")],
                Browser::Vivaldi => vec![join(local, r"Vivaldi\User Data")],
                Browser::Whale => vec![join(local, r"Naver\Naver Whale\User Data")],
                Browser::Safari => Vec::new(),
            }
        } else if cfg!(target_os = "macos") {
            let support = join(home, "Library/Application Support");
            let path = match self {
                Browser::Firefox => "Firefox/Profiles",
                Browser::Chrome => "Google/Chrome",
                Browser::Edge => "Microsoft Edge",
                Browser::Brave => "BraveSoftware/Brave-Browser",
                Browser::Chromium => "Chromium",
                Browser::Opera => "com.operasoftware.Opera",
                Browser::Vivaldi => "Vivaldi",
                Browser::Whale => "Naver/Whale",
                Browser::Safari => return Vec::new(),
            };
            vec![join(support, path)]
        } else {
            let config = dirs::config_dir();
            match self {
                Browser::Firefox => vec![
                    join(config.clone(), "mozilla/firefox"),
                    join(home.clone(), ".mozilla/firefox"),
                    join(
                        home.clone(),
                        ".var/app/org.mozilla.firefox/config/mozilla/firefox",
                    ),
                    join(
                        home.clone(),
                        ".var/app/org.mozilla.firefox/.mozilla/firefox",
                    ),
                    join(home, "snap/firefox/common/.mozilla/firefox"),
                ],
                Browser::Chrome => vec![join(config, "google-chrome")],
                Browser::Edge => vec![join(config, "microsoft-edge")],
                Browser::Brave => vec![join(config, "BraveSoftware/Brave-Browser")],
                Browser::Chromium => vec![join(config, "chromium")],
                Browser::Opera => vec![join(config, "opera")],
                Browser::Vivaldi => vec![join(config, "vivaldi")],
                Browser::Whale => vec![join(config, "naver-whale")],
                Browser::Safari => Vec::new(),
            }
        };
        dirs.into_iter().flatten().collect()
    }
}

/// 浏览器的一个数据目录在，本进程没有权限列出它（见 [`Browser::access`]）。
#[derive(Debug)]
pub struct Denied {
    pub dir: PathBuf,
    pub error: io::Error,
}

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "列不出 {}：{}", self.dir.display(), self.error)
    }
}
