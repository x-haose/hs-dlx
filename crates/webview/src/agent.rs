//! 网页的 User-Agent。

/// 网页用的 User-Agent：macOS 上为本机 Safari 的写法（读出它的版本号拼成），网页引擎与 Safari 同为系统的 WebKit，而
/// WKWebView 默认的不带 Safari 的 `Version/… Safari/…` 一段，有的网站的人机验证据此判为不是浏览器；读不出 Safari 的
/// 版本时与别的平台一样为 None（照网页引擎默认的）。Safari 的写法里系统版本固定为 10_15_7、WebKit 固定为 605.1.15。
pub fn user_agent() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        safari_version().map(|version| {
            format!(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/{version} Safari/605.1.15"
            )
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 本机 Safari 的版本：Info.plist 里 CFBundleShortVersionString 的前两段（如 `27.0`，Safari 的 User-Agent 只写到这一级）；
/// 读不出时为 None。
#[cfg(target_os = "macos")]
fn safari_version() -> Option<String> {
    use objc2_foundation::{NSBundle, NSString};

    let bundle = NSBundle::bundleWithPath(&NSString::from_str("/Applications/Safari.app"))?;
    let version =
        bundle.objectForInfoDictionaryKey(&NSString::from_str("CFBundleShortVersionString"))?;
    let version = version.downcast::<NSString>().ok()?.to_string();
    let mut parts = version.split('.');
    let major = parts.next().filter(|part| part.parse::<u32>().is_ok())?;
    let minor = parts
        .next()
        .filter(|part| part.parse::<u32>().is_ok())
        .unwrap_or("0");
    Some(format!("{major}.{minor}"))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// 本机的 Safari 读得出版本，拼出的是 Safari 的写法。
    #[test]
    fn the_agent_is_safari_s() {
        let agent = user_agent().expect("macOS 上有 Safari");
        assert!(
            agent.starts_with("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)"),
            "{agent}"
        );
        assert!(agent.ends_with(" Safari/605.1.15"), "{agent}");
        assert!(agent.contains(" Version/"), "{agent}");
    }
}
