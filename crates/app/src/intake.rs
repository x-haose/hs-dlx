//! 粘贴来的文字怎么拆成要建的任务：粘贴栏、剪贴板、启动参数、转交来的链接与控制协议都经这里。

use std::path::{Path, PathBuf};

use hs_m3u8_core::Url;

use crate::{Error, curl, special_link};

/// 一次粘贴最多多少行（空行不算）。
pub const MAX_LINES: usize = 50;

/// 拆好的一条：要建的一个任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intake {
    /// http(s) 链接、磁力链接（`magnet:`，带 `xt` 参数），或种子文件的 `file:` 地址（路径以 `.torrent` 结尾，不分
    /// 大小写）
    pub url: Url,
    /// 请求时带的请求头（curl 命令里的）；磁力链接与种子文件没有
    pub headers: Vec<(String, String)>,
    /// 保存目录，绝对路径
    pub folder: PathBuf,
}

/// 粘贴的文字里没建成任务的一段。
#[derive(Debug)]
pub struct Rejected {
    /// 那一行（整段不收时为整段）
    pub text: String,
    /// 原因：不是链接、行数太多为 [`Error::InvalidInput`]，种子已在别的任务里为 [`Error::TorrentTaken`]，其余为建任务
    /// 时的错误
    pub reason: Box<Error>,
}

/// 把粘贴的文字 `text` 拆成要建的任务，都存到 `folder`：整段是 curl 命令时为一条（带上命令里的请求头）；否则每个
/// 非空行一条，某一行不是链接时只那一行不收。没有非空行、超过 [`MAX_LINES`] 行、或 curl 命令拆不出时整段不收
/// （[`Error::InvalidInput`]）。
pub fn split(text: &str, folder: &Path) -> Result<Vec<Result<Intake, Rejected>>, Error> {
    if curl::is_command(text) {
        let request = curl::parse(text).map_err(|error| Error::InvalidInput(error.to_string()))?;
        return Ok(vec![Ok(Intake {
            url: request.url,
            headers: request.headers,
            folder: folder.to_path_buf(),
        })]);
    }
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return Err(Error::InvalidInput("没有链接".to_owned()));
    }
    if lines.len() > MAX_LINES {
        return Err(Error::InvalidInput(format!(
            "一次最多 {MAX_LINES} 行，这段有 {} 行",
            lines.len()
        )));
    }
    Ok(lines
        .into_iter()
        .map(|line| match link(line) {
            Ok(url) => Ok(Intake {
                url,
                headers: Vec::new(),
                folder: folder.to_path_buf(),
            }),
            Err(why) => Err(Rejected {
                text: line.to_owned(),
                reason: Box::new(Error::InvalidInput(why)),
            }),
        })
        .collect())
}

/// 一行文字里的链接：磁力链接或种子文件的地址（见 [`torrent_link`]），迅雷等专用链接里的地址（见
/// [`special_link::unwrap`]），或 http(s) 链接；都不是时为给用户的原因。
fn link(line: &str) -> Result<Url, String> {
    if let Some(url) = torrent_link(line) {
        return Ok(url);
    }
    let inner = special_link::unwrap(line)
        .transpose()
        .map_err(|error| error.to_string())?;
    match inner {
        Some(inner) => {
            http_link(&inner).ok_or_else(|| format!("专用链接里的地址不是 http 或 https：{inner}"))
        }
        None => http_link(line).ok_or_else(|| "要 http 或 https 开头的链接，或磁力链接".to_owned()),
    }
}

fn http_link(text: &str) -> Option<Url> {
    Url::parse(text.trim())
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https"))
}

/// `text`（去掉首尾空白）是磁力链接或种子文件的地址（见 [`torrent_source`]）时为它。
fn torrent_link(text: &str) -> Option<Url> {
    Url::parse(text.trim()).ok().filter(torrent_source)
}

/// `url` 是磁力链接（`magnet:`，带 `xt` 参数）或种子文件的 `file:` 地址（路径以 `.torrent` 结尾，不分大小写）。
pub(crate) fn torrent_source(url: &Url) -> bool {
    match url.scheme() {
        "magnet" => url.query_pairs().any(|(name, _)| name == "xt"),
        "file" => url.path().to_ascii_lowercase().ends_with(".torrent"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    fn folder() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\m" } else { "/m" })
    }

    fn urls(text: &str) -> Vec<Result<String, String>> {
        split(text, &folder())
            .unwrap()
            .into_iter()
            .map(|line| match line {
                Ok(intake) => Ok(intake.url.to_string()),
                Err(rejected) => Err(rejected.text),
            })
            .collect()
    }

    /// 每个非空行一条；不是链接的只那一行不收；专用链接取里面的地址。
    #[test]
    fn lines_become_links_one_by_one() {
        let thunder = "thunder://QUFodHRwczovL2EuZXhhbXBsZS9mLnppcFpa";
        let text = format!(
            " https://a.example/x.m3u8 \n\n看这个\nmagnet:?xt=urn:btih:{HASH}&dn=a\r\n{thunder}\nfile:///tmp/A.Torrent\n"
        );
        assert_eq!(
            urls(&text),
            [
                Ok("https://a.example/x.m3u8".to_owned()),
                Err("看这个".to_owned()),
                Ok(format!("magnet:?xt=urn:btih:{HASH}&dn=a")),
                Ok("https://a.example/f.zip".to_owned()),
                Ok("file:///tmp/A.Torrent".to_owned()),
            ]
        );
        // 不带 xt 的磁力链接、不是种子的本机文件都不是链接
        assert_eq!(
            urls("magnet:?dn=a\nfile:///tmp/a.mp4"),
            [
                Err("magnet:?dn=a".to_owned()),
                Err("file:///tmp/a.mp4".to_owned())
            ]
        );
    }

    /// 整段是 curl 命令时按一条拆，带上请求头；多行也只算一条。
    #[test]
    fn a_curl_command_is_one_link_with_its_headers() {
        let lines = split(
            "curl 'https://a.example/v.m3u8' \\\n  -H 'Referer: https://a.example/'",
            &folder(),
        )
        .unwrap();
        let [Ok(intake)] = lines.as_slice() else {
            panic!("curl 命令拆成一条：{lines:?}");
        };
        assert_eq!(intake.url.as_str(), "https://a.example/v.m3u8");
        assert_eq!(
            intake.headers,
            [("Referer".to_owned(), "https://a.example/".to_owned())]
        );
        assert_eq!(intake.folder, folder());
        assert!(matches!(
            split("curl", &folder()),
            Err(Error::InvalidInput(_))
        ));
    }

    /// 没有非空行、超过上限时整段不收；正好到上限的收。
    #[test]
    fn empty_and_oversized_pastes_are_refused_whole() {
        assert!(matches!(
            split(" \n\n", &folder()),
            Err(Error::InvalidInput(_))
        ));
        let lines = |count: usize| {
            (0..count)
                .map(|i| format!("https://a.example/{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            split(&lines(MAX_LINES), &folder()).unwrap().len(),
            MAX_LINES
        );
        assert!(matches!(
            split(&lines(MAX_LINES + 1), &folder()),
            Err(Error::InvalidInput(_))
        ));
    }
}
