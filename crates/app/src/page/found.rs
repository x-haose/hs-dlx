//! 网页里找到的一条流与它在后台探测到哪一步。

use hs_m3u8_core::{Probe, Url};

use super::Stream;

/// 一条流在后台探测到哪一步。
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Waiting,
    Running,
    /// 探测成了：结果与探测时带的请求头
    Ready {
        probe: Box<Probe>,
        headers: Vec<(String, String)>,
    },
    /// 没成：原因与探测时带的请求头；没配出请求头（读不出 Cookie）时为 None
    Failed {
        reason: String,
        headers: Option<Vec<(String, String)>>,
    },
}

/// 找到的一条流。
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// 去掉低延迟直播的刷新参数（`_HLS_` 起头的查询参数）与片段的地址：同一个框架报来的、它相同的算一条流
    pub key: String,
    /// 最近一次报来的地址；探测中与探测成了的不再换
    pub url: Url,
    /// 报它的框架的地址，网页引擎给的；认不出是哪个框架（Linux）或不是 http(s) 时为 None
    pub frame: Option<Url>,
    /// 报它时已显示的顶层页面的地址，网页引擎给的；不是 http(s) 时为 None
    pub top: Option<Url>,
    /// 网页的 User-Agent
    pub agent: String,
    pub check: Check,
    /// 配请求头时没带上的、放不进请求头的 Cookie 个数
    pub skipped: usize,
}

impl Found {
    pub fn new(stream: Stream) -> Found {
        let Stream {
            url,
            frame,
            top,
            agent,
        } = stream;
        Found {
            key: key(&url),
            url,
            frame,
            top,
            agent,
            check: Check::Waiting,
            skipped: 0,
        }
    }

    /// 探测它时要知道的：地址与来历。
    pub fn stream(&self) -> Stream {
        Stream {
            url: self.url.clone(),
            frame: self.frame.clone(),
            top: self.top.clone(),
            agent: self.agent.clone(),
        }
    }

    /// `other` 与它是同一条流：同一个框架报的（网页引擎认不出框架时都算同一个），地址只差低延迟直播的刷新参数。别的
    /// 框架报的另算一条，不能顶替这一条。
    pub fn same(&self, other: &Found) -> bool {
        self.key == other.key && self.frame == other.frame
    }

    /// 同一条流（[`Found::same`]）又报来了：还没探测或没成的换成新报来的地址与来历，没成的重新排队。探测中与探测成了
    /// 的不动。
    pub fn renew(&mut self, other: Found) {
        match self.check {
            Check::Waiting => {}
            Check::Failed { .. } if self.url != other.url => self.check = Check::Waiting,
            Check::Failed { .. } | Check::Running | Check::Ready { .. } => return,
        }
        self.url = other.url;
        self.frame = other.frame;
        self.top = other.top;
        self.agent = other.agent;
    }
}

/// 低延迟直播（RFC 8216bis 6.2.5）每次刷新都换的查询参数：`_HLS_msn`、`_HLS_part`、`_HLS_skip` 等，都以 `_HLS_` 起头。
const REFRESH_PARAMETER: &str = "_HLS_";

/// 地址去掉低延迟直播的刷新参数（见 [`REFRESH_PARAMETER`]）、片段；协议、主机、端口、路径与别的参数都留着（同一个
/// 接口常只靠参数区分不同的视频）。
fn key(url: &Url) -> String {
    let mut key = url.clone();
    key.set_fragment(None);
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| !name.starts_with(REFRESH_PARAMETER))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    if kept.is_empty() {
        key.set_query(None);
    } else {
        key.query_pairs_mut().clear().extend_pairs(kept);
    }
    key.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Found {
        Found::new(Stream {
            url: Url::parse(text).unwrap(),
            frame: None,
            top: None,
            agent: String::new(),
        })
    }

    #[test]
    fn a_stream_is_one_frame_s_address_without_refresh_parameters() {
        let mut first = found("https://cdn.example/live/index.m3u8?id=7&_HLS_msn=1");
        let again = found("https://cdn.example/live/index.m3u8?id=7&_HLS_msn=2&_HLS_part=0");
        assert!(first.same(&again));
        // 只靠参数区分的另一个视频、别的端口或协议、别的框架报的，都是另一条
        assert!(!first.same(&found("https://cdn.example/live/index.m3u8?id=8")));
        assert!(!first.same(&found("https://cdn.example:8443/live/index.m3u8?id=7")));
        assert!(!first.same(&found("http://cdn.example/live/index.m3u8?id=7")));
        let mut framed = found("https://cdn.example/live/index.m3u8?id=7");
        framed.frame = Some(Url::parse("https://ads.example/").unwrap());
        assert!(!first.same(&framed));
        // 还没探测的换成新地址
        first.renew(again);
        assert_eq!(first.url.query(), Some("id=7&_HLS_msn=2&_HLS_part=0"));
        // 没成的换地址并重新排队
        first.check = Check::Failed {
            reason: "403".into(),
            headers: None,
        };
        first.renew(found("https://cdn.example/live/index.m3u8?id=7&_HLS_msn=3"));
        assert!(matches!(first.check, Check::Waiting));
        // 探测中的不动
        first.check = Check::Running;
        first.renew(found("https://cdn.example/live/index.m3u8?id=7&_HLS_msn=4"));
        assert_eq!(first.url.query(), Some("id=7&_HLS_msn=3"));
    }
}
