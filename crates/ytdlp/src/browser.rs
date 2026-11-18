//! 解析时从哪个浏览器读登录后的 Cookie（yt-dlp 的 `cookiesfrombrowser`）。每次解析现读：浏览器里登录着、Cookie 轮换
//! 了也是新的。
//!
//! 浏览器有多个用户资料时读哪一个由用户选（见 [`CookieSource`]、[`Browser::profiles`]），不交给 yt-dlp 挑：它挑最近
//! 改过 Cookie 文件的那个，几个资料都开着时每次可能不同。
//!
//! 能不能读因平台而异：macOS 上 Chromium 系读时系统弹钥匙串的授权（一个解析进程里给过一次即留着），Safari 与受系统
//! 保护的浏览器数据目录要给应用完全磁盘访问权限（见 [`Browser::access`]）；Linux 上 Chromium 系的钥匙在系统的密钥环
//! 里；Windows 上 Chrome、Edge、Brave 把 Cookie 加密成只有它们自己解得开的（app-bound encryption），yt-dlp 读不了，
//! 不列出；Firefox 各平台都能读。读不了时解析报 [`crate::ExtractError::Cookies`]。

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// 解析时从哪里读 Cookie：浏览器与它的一个用户资料。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieSource {
    pub browser: Browser,
    /// 用户资料的目录（绝对路径，见 [`Profile::dir`]）；浏览器没有用户资料之分（Safari、Opera）或一个也没列出来时为
    /// None，由 yt-dlp 在数据目录里找
    pub profile: Option<PathBuf>,
}

/// 浏览器的一个用户资料。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// 资料的目录（绝对路径），交给 yt-dlp 时作 `cookiesfrombrowser` 的 profile
    pub dir: PathBuf,
    /// 浏览器里显示的名字
    pub name: String,
    /// 登录在这个资料里的浏览器账号（Chromium 系为 Google 等账号的邮箱）；没登录或浏览器不记时为 None
    pub account: Option<String>,
}

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
                "macOS 不让 hs-dlx 读 {name} 的数据：在「系统设置 → 隐私与安全性 → 完全磁盘访问权限」里打开 hs-dlx（列表里没有时点「+」加上），再重开 hs-dlx"
            )
        } else {
            format!("没有权限读 {name} 的数据目录")
        }
    }

    /// 它的各个用户资料，按浏览器里的次序；没有用户资料之分（Safari、Opera）或还没用过时为空。Chromium 系读用户数据
    /// 目录里的 `Local State`（`profile.info_cache` 与 `profile.profiles_order`），Firefox 读各个配置目录里的
    /// `profiles.ini`；目录已不在的资料不列。读不出（没有权限、格式认不出）时报原因。
    pub fn profiles(self) -> Result<Vec<Profile>, String> {
        match self {
            Browser::Safari | Browser::Opera => Ok(Vec::new()),
            Browser::Firefox => {
                let mut profiles = Vec::new();
                for dir in self.data_dirs() {
                    // Windows 与 macOS 上 yt-dlp 找的是 Profiles 子目录，profiles.ini 在它的上一级
                    let root = if dir.file_name().is_some_and(|name| name == "Profiles") {
                        dir.parent().map_or(dir.clone(), Path::to_path_buf)
                    } else {
                        dir
                    };
                    let ini = root.join("profiles.ini");
                    if let Some(text) = read_optional(&ini)? {
                        profiles.extend(firefox_profiles(&root, &text));
                    }
                }
                Ok(profiles.into_iter().filter(|p| p.dir.is_dir()).collect())
            }
            _ => {
                let mut profiles = Vec::new();
                for root in self.data_dirs() {
                    if let Some(text) = read_optional(&root.join("Local State"))? {
                        profiles.extend(chromium_profiles(&root, &text)?);
                    }
                }
                Ok(profiles.into_iter().filter(|p| p.dir.is_dir()).collect())
            }
        }
    }

    fn present(self) -> bool {
        if self == Browser::Safari {
            return cfg!(target_os = "macos");
        }
        self.data_dirs().iter().any(|dir| dir.is_dir())
    }

    /// yt-dlp 在这个平台上找它的 Cookie 的目录（Chromium 系为用户数据目录，Firefox 为几处放配置的目录，Safari 为放
    /// Cookies.binarycookies 的两处）；这个平台上没有的为空。
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
            let support =
                |path: &str| join(home.clone(), &format!("Library/Application Support/{path}"));
            match self {
                Browser::Safari => vec![
                    join(home.clone(), "Library/Cookies"),
                    join(
                        home,
                        "Library/Containers/com.apple.Safari/Data/Library/Cookies",
                    ),
                ],
                Browser::Firefox => vec![support("Firefox/Profiles")],
                Browser::Chrome => vec![support("Google/Chrome")],
                Browser::Edge => vec![support("Microsoft Edge")],
                Browser::Brave => vec![support("BraveSoftware/Brave-Browser")],
                Browser::Chromium => vec![support("Chromium")],
                Browser::Opera => vec![support("com.operasoftware.Opera")],
                Browser::Vivaldi => vec![support("Vivaldi")],
                Browser::Whale => vec![support("Naver/Whale")],
            }
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

/// 读 `path` 的全文；不在时为 None。
fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读不出 {}：{error}", path.display())),
    }
}

/// Chromium 系用户数据目录 `root` 里 `Local State`（`text`）列的用户资料：按 `profile.profiles_order`，它没列的接在
/// 后面按目录名排。
fn chromium_profiles(root: &Path, text: &str) -> Result<Vec<Profile>, String> {
    let state: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| format!("{} 认不出：{error}", root.join("Local State").display()))?;
    let Some(cache) = state
        .pointer("/profile/info_cache")
        .and_then(|v| v.as_object())
    else {
        return Ok(Vec::new());
    };
    let mut order: Vec<&str> = state
        .pointer("/profile/profiles_order")
        .and_then(|v| v.as_array())
        .map(|order| order.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    order.retain(|dir| cache.contains_key(*dir));
    let mut rest: Vec<&str> = cache
        .keys()
        .map(String::as_str)
        .filter(|dir| !order.contains(dir))
        .collect();
    rest.sort_unstable();
    order.extend(rest);
    Ok(order
        .into_iter()
        .map(|dir| {
            let info = &cache[dir];
            let field = |key: &str| {
                info.get(key)
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            };
            Profile {
                dir: root.join(dir),
                name: field("name").unwrap_or_else(|| dir.to_owned()),
                account: field("user_name"),
            }
        })
        .collect())
}

/// Firefox 配置目录 `root` 里 `profiles.ini`（`text`）列的用户资料（`[Profile*]` 各节），按文件里的次序。`Path` 在
/// `IsRelative=1` 时相对 `root`。
fn firefox_profiles(root: &Path, text: &str) -> Vec<Profile> {
    #[derive(Default)]
    struct Section {
        name: Option<String>,
        path: Option<String>,
        relative: bool,
    }
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.extend(current.take());
            current = header.starts_with("Profile").then(Section::default);
        } else if let (Some(section), Some((key, value))) = (current.as_mut(), line.split_once('='))
        {
            match key.trim() {
                "Name" => section.name = Some(value.trim().to_owned()),
                "Path" => section.path = Some(value.trim().to_owned()),
                "IsRelative" => section.relative = value.trim() == "1",
                _ => {}
            }
        }
    }
    sections.extend(current);
    sections
        .into_iter()
        .filter_map(|section| {
            let path = section.path?;
            let dir = if section.relative {
                root.join(&path)
            } else {
                PathBuf::from(&path)
            };
            Some(Profile {
                name: section.name.unwrap_or(path),
                dir,
                account: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 profiles_order 排，它没列的按目录名接在后面；空的名字与账号不算。
    #[test]
    fn chromium_profiles_follow_the_browser_order() {
        let text = r#"{"profile": {
            "info_cache": {
                "Default": {"name": "个人", "user_name": "a@example.com"},
                "Profile 2": {"name": "工作", "user_name": ""},
                "Profile 9": {"user_name": "c@example.com"},
                "Profile 1": {"name": "旧的"}
            },
            "profiles_order": ["Profile 2", "Default", "Gone"]
        }}"#;
        let root = Path::new("/u/Chrome");
        let profiles = chromium_profiles(root, text).unwrap();
        let shown: Vec<(&str, &str, Option<&str>)> = profiles
            .iter()
            .map(|p| {
                (
                    p.dir.file_name().unwrap().to_str().unwrap(),
                    p.name.as_str(),
                    p.account.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                ("Profile 2", "工作", None),
                ("Default", "个人", Some("a@example.com")),
                ("Profile 1", "旧的", None),
                ("Profile 9", "Profile 9", Some("c@example.com")),
            ]
        );
        assert!(chromium_profiles(root, "{}").unwrap().is_empty());
        assert!(chromium_profiles(root, "not json").is_err());
    }

    /// 只取 [Profile*] 各节；相对路径接在配置目录下，绝对的原样。
    #[test]
    fn firefox_profiles_come_from_profile_sections() {
        let text = "[Install4F96D1932A9F858E]\nDefault=Profiles/b.default-release\nLocked=1\n\n\
                    [Profile1]\nName=default\nIsRelative=1\nPath=Profiles/a.default\nDefault=1\n\n\
                    [Profile0]\nName=work\nIsRelative=0\nPath=/elsewhere/w.work\n\n\
                    [General]\nStartWithLastProfile=1\n";
        let root = Path::new("/u/Firefox");
        let profiles = firefox_profiles(root, text);
        let shown: Vec<(&str, PathBuf)> = profiles
            .iter()
            .map(|p| (p.name.as_str(), p.dir.clone()))
            .collect();
        assert_eq!(
            shown,
            [
                ("default", root.join("Profiles/a.default")),
                ("work", PathBuf::from("/elsewhere/w.work")),
            ]
        );
    }
}
