//! 注入网页的脚本与它发回的消息。脚本找 HLS 播放列表的地址、报顶层文档的类型；发回的内容来自网页，按不可信
//! 数据校验，认不出的丢掉。页面的地址与标题、发消息的框架不经脚本，由网页引擎报（见嗅探页）。

use hs_m3u8_core::Url;
use serde::Deserialize;

use crate::links::http_url;

/// 注入网页的脚本。
///
/// - 在哪些框架里跑：WebKit（macOS、Linux）上每一层框架都直接经引擎的 `ipc` 消息通道发（不经别的框架转发，嵌入它的
///   页面截不到）；别的引擎（WebView2）上 wry 把脚本注入每一层框架，而只有顶层框架发得回来，脚本在子框架里直接退出。
///   简化：WebView2 上找不到嵌入框架里的流，接上它按框架发消息的接口（`ICoreWebView2Frame2::WebMessageReceived`）
///   并实测后去掉这一限制。
/// - 播放列表：fetch 与 XHR 的响应类型是 HLS 播放列表（`application/vnd.apple.mpegurl` 之类）的，与地址路径以
///   `.m3u8` 结尾的请求（含资源加载记录与 video、source 元素上的），每个地址只报一次。
/// - 文档类型：顶层框架在文档建出时报一次 `document.contentType`，正文就绪（DOMContentLoaded）时再报一次正文是否以
///   `#EXTM3U` 开头（按网页类型发来的播放列表）。
/// - 人机验证：顶层框架每秒看一次是不是整页的人机验证（Cloudflare 的验证页，页面里有它的 `_cf_chl_opt`），变了才报。
///   页面里嵌的验证框（登录框旁隐形的 reCAPTCHA 之类）不算：不该为它把网页弹出来。
/// - 发回 JSON（见 [`Report`]）。发送出的任何错都不抛进页面：脚本改写了页面的 fetch 与 XHR，抛出的异常会成为页面
///   自己的请求失败。
pub const SCRIPT: &str = r#"
(function () {
  if (window.__hsM3u8Sniff) return;
  Object.defineProperty(window, '__hsM3u8Sniff', { value: true });
  var handlers = window.webkit && window.webkit.messageHandlers;
  var webkit = handlers && handlers.ipc;
  if (!webkit && window !== window.top) return;
  var channel = webkit || window.ipc;
  var send = function (message) {
    try { channel.postMessage(JSON.stringify(message)); } catch (e) {}
  };
  if (window === window.top) {
    send({ kind: 'document', type: document.contentType });
    var content = function () {
      var text = document.body ? document.body.textContent : '';
      send({ kind: 'content', playlist: /^\s*#EXTM3U/.test(text.slice(0, 64)) });
    };
    if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', content);
    else content();
    var challenged = null;
    setInterval(function () {
      var now = typeof window._cf_chl_opt !== 'undefined';
      if (now === challenged) return;
      challenged = now;
      send({ kind: 'challenge', active: now });
    }, 1000);
  }
  var playlist = /^\s*(application\/(vnd\.apple\.mpegurl|x-mpegurl)|audio\/(x-)?mpegurl)\s*(;|$)/i;
  var named = function (raw) {
    var url;
    // 解析不了的不是地址，不算
    try { url = new URL(String(raw), location.href); } catch (e) { return false; }
    return /\.m3u8$/i.test(url.pathname);
  };
  var seen = new Set();
  var found = function (raw) {
    var url;
    try { url = new URL(String(raw), location.href).href; } catch (e) { return; }
    if (seen.has(url)) return;
    seen.add(url);
    send({ kind: 'stream', url: url, agent: navigator.userAgent });
  };
  var fetch0 = window.fetch;
  if (fetch0) {
    window.fetch = function (input) {
      var url = input instanceof Request ? input.url : String(input);
      if (named(url)) found(url);
      var pending = fetch0.apply(this, arguments);
      // 只看响应类型；请求失败由页面自己处理，这里另接一个处理只为不多出一个没人处理的失败
      pending.then(function (response) {
        if (playlist.test(response.headers.get('content-type') || '')) found(response.url);
      }, function () {});
      return pending;
    };
  }
  var open0 = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function (method, url) {
    if (named(url)) found(url);
    this.addEventListener('load', function () {
      if (playlist.test(this.getResponseHeader('content-type') || '')) found(this.responseURL);
    });
    return open0.apply(this, arguments);
  };
  new PerformanceObserver(function (list) {
    list.getEntries().forEach(function (entry) {
      if (named(entry.name)) found(entry.name);
    });
  }).observe({ type: 'resource', buffered: true });
  setInterval(function () {
    document.querySelectorAll('video, source').forEach(function (media) {
      var src = media.currentSrc || media.src;
      if (src && named(src)) found(src);
    });
  }, 1000);
})();
"#;

/// 一条消息最长多少字节：页面能发任意多、任意长的消息，超过的不收。
pub const MAX_MESSAGE: usize = 16 * 1024;
/// User-Agent、文档类型与网页标题最长多少字符，超出的截掉。
pub const MAX_TEXT: usize = 300;

/// 脚本发回、校验过的一条消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// 找到一个播放列表的地址；`agent` 为网页的 User-Agent
    Stream { url: Url, agent: String },
    /// 顶层文档的类型（MIME，如 `text/html`）
    Document { content_type: String },
    /// 顶层文档的正文就绪：`playlist` 为正文以 `#EXTM3U` 开头
    Content { playlist: bool },
    /// 顶层是不是整页的人机验证（`active`），变了才报
    Challenge { active: bool },
}

/// 消息的原样内容，未校验。
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Raw {
    Stream {
        url: String,
        agent: String,
    },
    Document {
        #[serde(rename = "type")]
        content_type: String,
    },
    Content {
        playlist: bool,
    },
    Challenge {
        active: bool,
    },
}

/// 校验一条消息；太长、不是认得的 JSON、地址不是 http(s) 的为 None。
pub fn parse(message: &str) -> Option<Report> {
    if message.len() > MAX_MESSAGE {
        return None;
    }
    Some(match serde_json::from_str(message).ok()? {
        Raw::Stream { url, agent } => Report::Stream {
            url: http_url(&url)?,
            agent: clip(&agent),
        },
        Raw::Document { content_type } => Report::Document {
            content_type: clip(&content_type),
        },
        Raw::Content { playlist } => Report::Content { playlist },
        Raw::Challenge { active } => Report::Challenge { active },
    })
}

/// 去掉控制字符、最多留 [`MAX_TEXT`] 个字符。
pub fn clip(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_checked_before_use() {
        let stream = r#"{"kind":"stream","url":"https://cdn.example/a/playlist?id=3",
            "agent":"Mozilla/5.0"}"#;
        assert_eq!(
            parse(stream),
            Some(Report::Stream {
                url: Url::parse("https://cdn.example/a/playlist?id=3").unwrap(),
                agent: "Mozilla/5.0".into(),
            })
        );
        assert_eq!(
            parse(r#"{"kind":"document","type":"text/html"}"#),
            Some(Report::Document {
                content_type: "text/html".into()
            })
        );
        assert_eq!(
            parse(r#"{"kind":"content","playlist":true}"#),
            Some(Report::Content { playlist: true })
        );
        assert_eq!(
            parse(r#"{"kind":"challenge","active":true}"#),
            Some(Report::Challenge { active: true })
        );
        // 不是 http(s)、认不出的种类、太长的都不要
        assert_eq!(
            parse(r#"{"kind":"stream","url":"javascript:alert(1)","agent":""}"#),
            None
        );
        assert_eq!(parse(r#"{"kind":"run","command":"rm"}"#), None);
        assert_eq!(parse(&"x".repeat(MAX_MESSAGE + 1)), None);
        // 控制字符去掉、过长的截短
        let long = format!(
            r#"{{"kind":"document","type":"{}\u0007"}}"#,
            "长".repeat(400)
        );
        let Some(Report::Document { content_type }) = parse(&long) else {
            panic!("文档类型消息应认得");
        };
        assert_eq!(content_type.chars().count(), MAX_TEXT);
        assert!(!content_type.contains('\u{7}'));
    }
}
