//! 探测网页里找到的流时带的请求头：Cookie 照浏览器在这个请求里会带的给，Referer 只给网站。

use hs_m3u8_core::{Url, check_option_header};

/// 网页引擎的 Cookie 存储里的一条 Cookie。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// 跨站的请求（发起请求的页面与请求的地址不在同一站点）也带它：SameSite=None，或没设 SameSite 而网页引擎按 None
    /// 处理
    pub cross_site: bool,
}

/// 网页里找到的一条流与它的来历。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub url: Url,
    /// 报它的框架的地址，网页引擎给的；认不出是哪个框架（Linux）或框架不是 http(s) 的（about:blank、srcdoc）时为 None
    pub frame: Option<Url>,
    /// 网页的 User-Agent，为空时不带
    pub agent: String,
    /// 找到这条流时已显示的顶层页面的地址，网页引擎给的；不是 http(s) 的为 None
    pub top: Option<Url>,
}

/// 探测时带的请求头与没带上的 Cookie 个数。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Headers {
    pub headers: Vec<(String, String)>,
    /// 放不进请求头的 Cookie 个数：没有名字（核心只收 `名=值`），或值里有控制字符
    pub skipped: usize,
}

/// 探测 `stream` 要带的请求头，`cookies` 为网页引擎里会发往它的 Cookie；都按核心的规则校验过：
///
/// - Referer：报它的框架的网站（origin，浏览器跨站时的默认做法；核心会把它发给分片与 key 所在的每个主机，完整的页面
///   地址里常有会话参数），认不出框架时为顶层页面的网站。只用网页引擎给的地址，网页自己说的一概不用（能伪造成别的
///   站点）；不是 http(s)，或从 https 页面请求 http 的流时不带。
/// - User-Agent：网页的。
/// - Cookie：流与顶层页面同一站点、且报它的框架（网页引擎认得出的）也与流同一站点时全带；否则只带跨站也带的
///   （[`Cookie::cross_site`]）。别的站点的框架（如广告）能报任意地址，不能借它带上这个站点的登录 Cookie。放不进请求
///   头的跳过，计入 [`Headers::skipped`]。
pub(crate) fn headers_for(stream: &Stream, cookies: &[Cookie]) -> Result<Headers, String> {
    let mut headers = Vec::new();
    if let Some(origin) = stream.frame.as_ref().or(stream.top.as_ref())
        && !(origin.scheme() == "https" && stream.url.scheme() == "http")
    {
        headers.push((
            "Referer".to_owned(),
            format!("{}/", origin.origin().ascii_serialization()),
        ));
    }
    if !stream.agent.is_empty() {
        headers.push(("User-Agent".to_owned(), stream.agent.clone()));
    }
    let same_site = [&stream.top, &stream.frame].into_iter().all(|page| {
        page.as_ref()
            .is_some_and(|page| same_site(page, &stream.url))
    });
    let mut skipped = 0;
    let pairs: Vec<String> = cookies
        .iter()
        .filter(|cookie| same_site || cookie.cross_site)
        .filter_map(|cookie| {
            let pair = pair(cookie);
            if pair.is_none() {
                skipped += 1;
            }
            pair
        })
        .collect();
    if !pairs.is_empty() {
        headers.push(("Cookie".to_owned(), pairs.join("; ")));
    }
    for (name, value) in &headers {
        check_option_header(name, value)?;
    }
    Ok(Headers { headers, skipped })
}

/// 一条 Cookie 在 Cookie 请求头里的 `名=值`；名为空或不合请求头要求的为 None。
fn pair(cookie: &Cookie) -> Option<String> {
    let pair = format!("{}={}", cookie.name, cookie.value);
    (!cookie.name.is_empty() && check_option_header("Cookie", &pair).is_ok()).then_some(pair)
}

/// 网页视频解析带的 Cookie 请求头（`cookies` 为网页引擎里会发往网页的）：合请求头要求、没有制表符（yt-dlp 的
/// cookies.txt 以它分列）的；一条都没有时为 None。
pub fn cookie_header(cookies: &[Cookie]) -> Option<(String, String)> {
    let writable: Vec<Cookie> = cookies
        .iter()
        .filter(|cookie| !cookie.name.contains('\t') && !cookie.value.contains('\t'))
        .cloned()
        .collect();
    cookie_pairs(&writable)
}

/// `cookies` 合成的 Cookie 请求头：放不进请求头的跳过；一条都没有时为 None。
pub(crate) fn cookie_pairs(cookies: &[Cookie]) -> Option<(String, String)> {
    let pairs: Vec<String> = cookies.iter().filter_map(pair).collect();
    (!pairs.is_empty()).then(|| ("Cookie".to_owned(), pairs.join("; ")))
}

/// `a` 与 `b` 同一站点：协议相同、可注册域名（按公共后缀列表）相同；IP 地址与列表里认不出的主机按整个主机比。
fn same_site(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme() && site(a).is_some_and(|site_a| Some(site_a) == site(b))
}

fn site(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    // IP 地址（IPv6 的带方括号）没有可注册域名，按整个主机比
    let ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok();
    Some(if ip {
        host
    } else {
        psl::domain_str(&host).map_or_else(|| host.clone(), str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn cookie(name: &str, value: &str, cross_site: bool) -> Cookie {
        Cookie {
            name: name.to_owned(),
            value: value.to_owned(),
            cross_site,
        }
    }

    fn found(stream: &Url, frame: Option<&Url>, top: Option<&Url>) -> Stream {
        Stream {
            url: stream.clone(),
            frame: frame.cloned(),
            agent: String::new(),
            top: top.cloned(),
        }
    }

    /// 同站的、跨站也带的、没名字的、值里有控制字符的各一个。
    fn cookies() -> Vec<Cookie> {
        vec![
            cookie("sid", "1", false),
            cookie("cdn", "1", true),
            cookie("", "1", true),
            cookie("bad", "a\u{1}b", true),
        ]
    }

    /// 网页视频的 Cookie 请求头：不论同站与否都带，没名字的、有控制字符或制表符的不带；一条都没有时没有这个头。
    #[test]
    fn the_web_cookie_header_keeps_only_writable_pairs() {
        let mut all = cookies();
        all.push(cookie("tab", "a\tb", false));
        assert_eq!(
            cookie_header(&all),
            Some(("Cookie".to_owned(), "sid=1; cdn=1".to_owned()))
        );
        assert_eq!(cookie_header(&[cookie("", "1", false)]), None);
    }

    #[test]
    fn referer_is_the_frame_site() {
        let stream = url("https://cdn.video.example/a/index.m3u8");
        let frame = url("https://www.video.example/watch?session=secret");
        let made = headers_for(
            &Stream {
                agent: "UA".to_owned(),
                ..found(&stream, Some(&frame), None)
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            made.headers,
            [
                (
                    "Referer".to_owned(),
                    "https://www.video.example/".to_owned()
                ),
                ("User-Agent".to_owned(), "UA".to_owned()),
            ]
        );
        // 认不出框架时为顶层页面的网站
        let top = url("https://www.video.example/list");
        let made = headers_for(&found(&stream, None, Some(&top)), &[]).unwrap();
        assert_eq!(made.headers[0].1, "https://www.video.example/");
        // 从 https 页面请求 http 的流不带
        let plain = url("http://cdn.video.example/a/index.m3u8");
        let made = headers_for(&found(&plain, Some(&frame), None), &[]).unwrap();
        assert_eq!(made.headers, []);
    }

    #[test]
    fn site_cookies_need_a_same_site_page_and_frame() {
        let stream = url("https://cdn.video.example/a/index.m3u8");
        let top = url("https://www.video.example/watch/3");
        // 页面与报它的框架都同站：全带；没名字的、值里有控制字符的跳过并计数，不连累别的
        let made = headers_for(&found(&stream, Some(&top), Some(&top)), &cookies()).unwrap();
        assert_eq!(made.headers.last().unwrap().1, "sid=1; cdn=1");
        assert_eq!(made.skipped, 2);
        // 别的站点的框架（广告）报的：只带跨站也带的
        let ad = url("https://ads.example.co.uk/slot");
        let made = headers_for(&found(&stream, Some(&ad), Some(&top)), &cookies()).unwrap();
        assert_eq!(made.headers.last().unwrap().1, "cdn=1");
        // 认不出框架时同样只带跨站也带的
        let made = headers_for(&found(&stream, None, Some(&top)), &cookies()).unwrap();
        assert_eq!(made.headers.last().unwrap().1, "cdn=1");
        // 可注册域名按公共后缀列表算（example.co.uk，不是 co.uk）；协议不同不算同一站点
        assert!(same_site(
            &url("https://a.example.co.uk/"),
            &url("https://b.example.co.uk/")
        ));
        assert!(!same_site(&url("https://a.other.co.uk/"), &ad));
        assert!(!same_site(&url("http://www.video.example/"), &stream));
    }
}
