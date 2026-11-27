//! 什么文字算链接：粘贴栏、剪贴板、启动参数与再次打开应用时转交来的。

use hs_m3u8_core::Url;

/// 不是认得的链接时给用户的说法。
pub const NOT_HTTP: &str = "要 http 或 https 开头的链接，或磁力链接";

/// `text`（去掉首尾空白）是 http 或 https 链接时为它。
pub fn http_url(text: &str) -> Option<Url> {
    Url::parse(text.trim())
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// `text`（去掉首尾空白）是磁力链接（`magnet:` 开头、带 `xt` 参数）或种子文件的 `file:` 地址（路径以 `.torrent`
/// 结尾，不分大小写）时为它：交给新建页取种子信息或读文件。
pub fn torrent_url(text: &str) -> Option<Url> {
    let url = Url::parse(text.trim()).ok()?;
    let taken = match url.scheme() {
        "magnet" => url.query_pairs().any(|(name, _)| name == "xt"),
        "file" => url.path().to_ascii_lowercase().ends_with(".torrent"),
        _ => false,
    };
    taken.then_some(url)
}

/// `text` 是路径以 `.m3u8` 结尾的 http(s) 链接时为它：看着像 HLS 播放列表（剪贴板里的提示、取到的不是播放列表时要不要
/// 看它是不是网页，只按地址猜；不是这样的地址也可能是播放列表）。
pub fn stream_url(text: &str) -> Option<Url> {
    http_url(text).filter(|url| url.path().to_ascii_lowercase().ends_with(".m3u8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_links_are_taken() {
        assert!(http_url(" https://a.example/x \n").is_some());
        assert!(http_url("file:///tmp/x.m3u8").is_none());
        assert!(http_url("看这个 https://a.example/x.m3u8").is_none());
        assert!(stream_url("https://a.example/v/index.m3u8?t=1").is_some());
        assert!(stream_url("https://a.example/v/INDEX.M3U8").is_some());
        assert!(stream_url("https://a.example/v/clip.mp4").is_none());
    }

    #[test]
    fn magnet_links_and_torrent_files_are_taken() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        assert!(torrent_url(&format!(" magnet:?xt=urn:btih:{hash}&dn=a ")).is_some());
        assert!(torrent_url("magnet:?dn=a").is_none());
        assert!(torrent_url("file:///tmp/A.Torrent").is_some());
        assert!(torrent_url("file:///tmp/a.mp4").is_none());
        assert!(torrent_url("https://a.example/a.torrent").is_none());
    }
}
