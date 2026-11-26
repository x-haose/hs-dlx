//! 打开的地址是不是网页：按网页引擎打开它后的第一个顶层文档判断，只定一次，之后的换页不改。

/// 网页引擎当作网页显示的顶层文档类型。
const PAGE_TYPES: [&str; 2] = ["text/html", "application/xhtml+xml"];

/// 结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 还不知道
    Pending,
    /// 网页：网页引擎拿到的是 2xx，或是整页的人机验证（要用户在网页里过了才看得到内容）
    Page,
    /// 网页引擎打开它也是 HTTP 出错的页面（防盗链、过期的地址之类的拒绝页、找不到的页）：状态码
    Refused(u16),
    /// 不是网页：网页引擎认出的类型（如 `video/mp4`）；文档加载完也没报类型（脚本没在里面跑，如 PDF）时为 None
    NotPage(Option<String>),
    /// 按网页类型发来、正文是 HLS 播放列表（如 PHP 的默认类型）
    Playlist,
    /// 打开的地址本身是要下载的文件
    Download,
    /// 网页类型的文档加载完了，网页引擎与页面都没报它的 HTTP 状态码：判断不了是不是出错的页面
    Unknown,
    /// 打不开：建不出网页、导航没成、网页进程退出、到时还没有结论；原因
    Broken(String),
}

/// 判断到哪一步。网页类型的文档要正文就绪（或加载完）且知道了它的 HTTP 状态码才定论。状态码有三个来处：网页引擎在
/// 响应到时报（macOS、Linux，带地址：与开始显示的地址是同一文档时归给它，两者谁先到都行；没成为文档的响应不会被别的
/// 文档领走），页面在文档建出时从导航记录读（Chromium），网页引擎在加载完时报（Windows）；有前面的就不用后面的。整页
/// 的人机验证不等状态码（它本身多为 403、503，不论状态码都是网页）。
#[derive(Debug)]
pub(super) struct Judge {
    verdict: Verdict,
    /// 顶层文档报过网页的类型，等正文就绪再定（按网页类型发来的播放列表，如 PHP 的默认类型）
    html: bool,
    /// 正文就绪且不是播放列表：值为这一页是整页的人机验证
    content: Option<bool>,
    /// 网页引擎报来、还没有文档认领的响应：地址与状态码
    pending: Option<(String, u16)>,
    /// 开始显示了、还没报类型的文档的地址
    shown: Option<String>,
    /// 开始显示的文档认领到的网页引擎的状态码，等它报类型时归给它
    committed: Option<u16>,
    /// 当前顶层文档的状态码
    status: Option<u16>,
}

impl Judge {
    pub fn new() -> Judge {
        Judge {
            verdict: Verdict::Pending,
            html: false,
            content: None,
            pending: None,
            shown: None,
            committed: None,
            status: None,
        }
    }

    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// 网页引擎拿到了顶层的响应：地址 `url` 与状态码 `status`。是开始显示了、还没报类型的那个文档的即归给它，否则等
    /// 开始显示它时认领。
    pub fn responded(&mut self, url: String, status: u16) {
        if self
            .shown
            .as_deref()
            .is_some_and(|shown| same_document(shown, &url))
        {
            self.committed = Some(status);
        } else {
            self.pending = Some((url, status));
        }
    }

    /// 开始显示地址 `url`（导航已提交）：认领同一文档的响应的状态码，还没报来的等 [`Judge::responded`]。
    pub fn started(&mut self, url: &str) {
        self.committed = self
            .pending
            .take()
            .filter(|(responded, _)| same_document(responded, url))
            .map(|(_, status)| status);
        self.shown = Some(url.to_owned());
    }

    /// 顶层文档建出时报了类型与页面读到的状态码 `status`：网页类型等正文就绪再定，别的即不是网页。有了结论时为 true
    /// （下同）。
    pub fn document(&mut self, content_type: &str, status: Option<u16>) -> bool {
        self.shown = None;
        self.status = self.committed.take().or(status);
        self.content = None;
        self.html = PAGE_TYPES
            .iter()
            .any(|page| content_type.eq_ignore_ascii_case(page));
        if self.html {
            false
        } else {
            self.decide(Verdict::NotPage(Some(content_type.to_owned())))
        }
    }

    /// 顶层文档的正文就绪：`playlist` 为正文以 `#EXTM3U` 开头，`challenge` 为整页的人机验证。
    pub fn content(&mut self, playlist: bool, challenge: bool) -> bool {
        if !self.html {
            return false;
        }
        if playlist {
            return self.decide(Verdict::Playlist);
        }
        self.content = Some(challenge);
        self.page()
    }

    /// 网页引擎在顶层文档加载完时给了它的状态码 `status`。
    pub fn completed(&mut self, status: u16) -> bool {
        self.status = self.status.or(Some(status));
        self.html && self.page()
    }

    /// 顶层文档加载完：报过网页类型的按状态码定（正文没报来时不算人机验证），这时还不知道状态码为判断不了；没报过
    /// 类型的不是网页。
    pub fn finished(&mut self) -> bool {
        if !self.html {
            return self.decide(Verdict::NotPage(None));
        }
        self.content.get_or_insert(false);
        if self.status.is_none() {
            return self.decide(Verdict::Unknown);
        }
        self.page()
    }

    /// 还没显示任何页面时，网页引擎要下载打开的地址。
    pub fn download(&mut self) -> bool {
        self.decide(Verdict::Download)
    }

    pub fn fail(&mut self, reason: String) -> bool {
        self.decide(Verdict::Broken(reason))
    }

    /// 网页类型的文档：正文就绪且是人机验证即定，不是的等状态码。
    fn page(&mut self) -> bool {
        match (self.content, self.status) {
            (Some(true), _) => self.decide(Verdict::Page),
            (Some(false), Some(status)) if (200..300).contains(&status) => {
                self.decide(Verdict::Page)
            }
            (Some(false), Some(status)) => self.decide(Verdict::Refused(status)),
            (None, _) | (Some(false), None) => false,
        }
    }

    fn decide(&mut self, verdict: Verdict) -> bool {
        if self.verdict != Verdict::Pending {
            return false;
        }
        self.verdict = verdict;
        true
    }
}

/// 两个地址是同一个文档：不计片段与账号口令（WebKit 给的响应地址去掉了账号口令，显示的地址留着）；解析不了的按原样比。
fn same_document(a: &str, b: &str) -> bool {
    let document = |raw: &str| match url::Url::parse(raw) {
        Ok(mut url) => {
            url.set_fragment(None);
            // 没有主机的地址（about:、data:）设不了，本就没有账号口令
            let _no_host = url.set_username("").and_then(|()| url.set_password(None));
            url.to_string()
        }
        Err(_) => raw.to_owned(),
    };
    document(a) == document(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "https://a.example/v";

    /// 网页引擎报了 `A` 的响应、开始显示它、它报了网页类型。
    fn shown(status: u16) -> Judge {
        let mut judge = Judge::new();
        judge.responded(A.into(), status);
        judge.started(A);
        assert!(!judge.document("text/html", None));
        judge
    }

    #[test]
    fn the_first_top_document_decides_once() {
        // 网页：网页引擎先报状态码（macOS、Linux），2xx、正文不是播放列表
        let mut judge = shown(200);
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Page);
        // 之后的事不改结论
        assert!(!judge.fail("网页进程退出了".into()));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 页面在文档建出时报状态码（Chromium）
        let mut judge = Judge::new();
        judge.document("text/html", Some(204));
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 网页引擎报的优先于页面报的（页面改不了它）
        let mut judge = Judge::new();
        judge.responded(A.into(), 403);
        judge.started(A);
        judge.document("text/html", Some(200));
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Refused(403));

        // 网页引擎打开也是出错的页面：拒绝；是整页的人机验证（如 Cloudflare 回 403）：网页
        let mut judge = shown(403);
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Refused(403));
        let mut judge = shown(403);
        assert!(judge.content(false, true));
        assert_eq!(judge.verdict(), &Verdict::Page);
        // 人机验证页不等状态码
        let mut judge = Judge::new();
        judge.document("text/html", None);
        assert!(judge.content(false, true));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 开始显示先于响应报来（Linux 上 wry 的处理先接上）：同样归给它
        let mut judge = Judge::new();
        judge.started(A);
        judge.responded(A.into(), 403);
        judge.document("text/html", None);
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Refused(403));

        // 显示的地址带账号口令、响应的没有，片段不同：同一个文档
        let mut judge = Judge::new();
        judge.responded("https://a.example/v".into(), 200);
        judge.started("https://user:pw@a.example/v#t=1");
        judge.document("text/html", None);
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 第一页还没就绪时下一次导航的响应先到：归给下一个文档，不算这一页的
        let mut judge = shown(200);
        judge.responded("https://b.example/".into(), 403);
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 没成为文档的响应（转为下载）不被下一个别的地址的文档领走
        let mut judge = Judge::new();
        judge.responded("https://a.example/file.zip".into(), 200);
        judge.started("about:blank");
        judge.document("text/html", None);
        judge.content(false, false);
        assert!(judge.finished());
        assert_eq!(judge.verdict(), &Verdict::Unknown);

        // 加载完时网页引擎才给状态码（Windows）：正文就绪时还不定
        let mut judge = Judge::new();
        judge.document("text/html", None);
        assert!(!judge.content(false, false));
        assert!(judge.completed(200));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 按网页类型发来的播放列表：不等状态码
        let mut judge = Judge::new();
        judge.document("text/html", None);
        assert!(judge.content(true, false));
        assert_eq!(judge.verdict(), &Verdict::Playlist);

        // 视频文件：类型即定
        let mut judge = Judge::new();
        assert!(judge.document("video/mp4", None));
        assert_eq!(judge.verdict(), &Verdict::NotPage(Some("video/mp4".into())));

        // 报过网页类型、正文没来就加载完了：按状态码定；没报过类型就加载完了：不是网页
        let mut judge = shown(200);
        assert!(judge.finished());
        assert_eq!(judge.verdict(), &Verdict::Page);
        let mut judge = Judge::new();
        assert!(judge.finished());
        assert_eq!(judge.verdict(), &Verdict::NotPage(None));

        // 正文就绪只在报过网页类型之后才算
        let mut judge = Judge::new();
        assert!(!judge.content(true, false));
        assert_eq!(judge.verdict(), &Verdict::Pending);
    }
}
