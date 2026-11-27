//! 探测网页里找到的一条流：照网页引擎里发往它的 Cookie 配好请求头，探测。

use std::time::Duration;

use hs_m3u8_core::{Engine, Proxy, Source};

use super::found::Check;
use super::request::{Cookie, Stream, headers_for};

/// 等网页引擎回出 Cookie 最多多久：引擎卡住时不让探测一直挂着。
pub const COOKIE_TIMEOUT: Duration = Duration::from_secs(5);
/// 探测一条流最多多久，到时取消（连同核心在途的请求与重试）：源站不回应时不让后面的一直排着。核心默认连接超时 15
/// 秒、第一次重试前等约 0.5 秒，30 秒容得下一次连接超时后再试一次。
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// 探测 `stream`，经代理 `proxy`：请求头照网页引擎里会发往它的 `cookies` 配（见 [`headers_for`]；没读出 Cookie 时为
/// 原因，不探测），任务自己的请求头 `own` 盖过配出的同名的。返回结果与没带上的 Cookie 个数。调用方须在 tokio 运行时里。
pub(crate) async fn check(
    engine: &Engine,
    proxy: Proxy,
    stream: &Stream,
    cookies: Result<Vec<Cookie>, String>,
    own: &[(String, String)],
) -> (Check, usize) {
    let made = match cookies.and_then(|cookies| headers_for(stream, &cookies)) {
        Ok(made) => made,
        Err(reason) => {
            return (
                Check::Failed {
                    reason,
                    headers: None,
                },
                0,
            );
        }
    };
    let headers = overridden(made.headers, own);
    let mut source = Source::new(stream.url.clone());
    source.http.headers.clone_from(&headers);
    source.http.proxy = proxy;
    // 超时即丢弃探测，在途的请求随之取消
    let check = match tokio::time::timeout(PROBE_TIMEOUT, engine.probe(&source)).await {
        Ok(Ok(probe)) => Check::Ready {
            probe: Box::new(probe),
            headers,
        },
        Ok(Err(error)) => Check::Failed {
            reason: error.to_string(),
            headers: Some(headers),
        },
        Err(_) => Check::Failed {
            reason: format!("探测 {} 秒没有结果，已停下", PROBE_TIMEOUT.as_secs()),
            headers: Some(headers),
        },
    };
    (check, made.skipped)
}

/// 重新取打开的地址本身（网页引擎打开它不是网页）时带的请求头：任务自己的 `own`；它没有 Cookie 时补上网页引擎里会
/// 发往这个地址的 `cookies`（顶层导航，同站与否都带；放不进请求头的跳过）。
pub(crate) fn document_headers(
    own: &[(String, String)],
    cookies: &[Cookie],
) -> Vec<(String, String)> {
    let mut headers = own.to_vec();
    let has_cookie = own
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("cookie"));
    if !has_cookie {
        headers.extend(super::request::cookie_pairs(cookies));
    }
    headers
}

/// `made` 里与 `own` 同名（不分大小写）的换成 `own` 的，`own` 里其余的接在后面。
fn overridden(made: Vec<(String, String)>, own: &[(String, String)]) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = made
        .into_iter()
        .filter(|(name, _)| !own.iter().any(|(mine, _)| mine.eq_ignore_ascii_case(name)))
        .collect();
    headers.extend(own.iter().cloned());
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(name: &str, value: &str) -> (String, String) {
        (name.to_owned(), value.to_owned())
    }

    #[test]
    fn the_task_s_own_headers_win_over_the_page_s() {
        let made = vec![
            pair("Referer", "https://a.example/"),
            pair("User-Agent", "UA"),
            pair("Cookie", "sid=1"),
        ];
        let own = [pair("cookie", "sid=2"), pair("Authorization", "Bearer x")];
        assert_eq!(
            overridden(made, &own),
            [
                pair("Referer", "https://a.example/"),
                pair("User-Agent", "UA"),
                pair("cookie", "sid=2"),
                pair("Authorization", "Bearer x"),
            ]
        );
    }

    #[test]
    fn a_document_gets_the_page_s_cookies_unless_the_task_has_its_own() {
        let cookies = [
            Cookie {
                name: "sid".into(),
                value: "1".into(),
                cross_site: false,
            },
            Cookie {
                name: String::new(),
                value: "x".into(),
                cross_site: true,
            },
        ];
        assert_eq!(
            document_headers(&[pair("Referer", "https://a.example/")], &cookies),
            [
                pair("Referer", "https://a.example/"),
                pair("Cookie", "sid=1")
            ]
        );
        assert_eq!(
            document_headers(&[pair("Cookie", "mine=1")], &cookies),
            [pair("Cookie", "mine=1")]
        );
        assert_eq!(document_headers(&[], &[]), []);
    }
}
