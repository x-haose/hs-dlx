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
    /// 打不开：建不出网页、导航没成、网页进程退出、到时还没有结论；原因
    Broken(String),
}

/// 判断到哪一步。网页类型的文档要正文就绪（或加载完）且知道了网页引擎拿到的状态码才定论：macOS、Linux 上状态码先到，
/// Windows 上随加载完到。
#[derive(Debug)]
pub(super) struct Judge {
    verdict: Verdict,
    /// 顶层文档报过网页的类型，等正文就绪再定（按网页类型发来的播放列表，如 PHP 的默认类型）
    html: bool,
    /// 正文就绪且不是播放列表：值为这一页是整页的人机验证
    content: Option<bool>,
    /// 网页引擎拿到的顶层文档的状态码
    status: Option<u16>,
}

impl Judge {
    pub fn new() -> Judge {
        Judge {
            verdict: Verdict::Pending,
            html: false,
            content: None,
            status: None,
        }
    }

    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// 顶层文档建出时报了类型：网页类型等正文就绪再定，别的即不是网页。有了结论时为 true（下同）。
    pub fn document(&mut self, content_type: &str) -> bool {
        if PAGE_TYPES
            .iter()
            .any(|page| content_type.eq_ignore_ascii_case(page))
        {
            self.html = true;
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

    /// 网页引擎拿到了顶层文档的响应：状态码 `status`。
    pub fn responded(&mut self, status: u16) -> bool {
        self.status = Some(status);
        if self.html { self.page() } else { false }
    }

    /// 顶层文档加载完：报过网页类型的按状态码定（正文没报来时不算人机验证），没报过类型的不是网页。
    pub fn finished(&mut self) -> bool {
        if !self.html {
            return self.decide(Verdict::NotPage(None));
        }
        self.content.get_or_insert(false);
        self.page()
    }

    /// 还没显示任何页面时，网页引擎要下载打开的地址。
    pub fn download(&mut self) -> bool {
        self.decide(Verdict::Download)
    }

    pub fn fail(&mut self, reason: String) -> bool {
        self.decide(Verdict::Broken(reason))
    }

    /// 网页类型的文档：正文与状态码都有了才定。
    fn page(&mut self) -> bool {
        let (Some(challenge), Some(status)) = (self.content, self.status) else {
            return false;
        };
        self.decide(if (200..300).contains(&status) || challenge {
            Verdict::Page
        } else {
            Verdict::Refused(status)
        })
    }

    fn decide(&mut self, verdict: Verdict) -> bool {
        if self.verdict != Verdict::Pending {
            return false;
        }
        self.verdict = verdict;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_top_document_decides_once() {
        // 网页：类型是 HTML、状态码 2xx、正文不是播放列表；状态码先到（macOS、Linux）
        let mut judge = Judge::new();
        assert!(!judge.responded(200));
        assert!(!judge.document("text/html"));
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Page);
        // 之后的事不改结论
        assert!(!judge.fail("网页进程退出了".into()));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 状态码后到（Windows 随加载完）：正文就绪时还不定
        let mut judge = Judge::new();
        judge.document("text/html");
        assert!(!judge.content(false, false));
        assert!(!judge.finished());
        assert!(judge.responded(204));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 网页引擎打开也是出错的页面：拒绝；是整页的人机验证（如 Cloudflare 回 403）：网页
        let mut judge = Judge::new();
        judge.responded(403);
        judge.document("text/html");
        assert!(judge.content(false, false));
        assert_eq!(judge.verdict(), &Verdict::Refused(403));
        let mut judge = Judge::new();
        judge.responded(403);
        judge.document("text/html");
        assert!(judge.content(false, true));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 按网页类型发来的播放列表：不等状态码
        let mut judge = Judge::new();
        judge.document("text/html");
        assert!(judge.content(true, false));
        assert_eq!(judge.verdict(), &Verdict::Playlist);

        // 视频文件：类型即定
        let mut judge = Judge::new();
        assert!(judge.document("video/mp4"));
        assert_eq!(judge.verdict(), &Verdict::NotPage(Some("video/mp4".into())));

        // 报过网页类型、正文没来就加载完了：按状态码定；没报过类型就加载完了：不是网页
        let mut judge = Judge::new();
        judge.responded(200);
        judge.document("text/html");
        assert!(judge.finished());
        assert_eq!(judge.verdict(), &Verdict::Page);
        let mut judge = Judge::new();
        assert!(judge.finished());
        assert_eq!(judge.verdict(), &Verdict::NotPage(None));

        // 正文就绪只在报过网页类型之后才算
        let mut judge = Judge::new();
        judge.responded(200);
        assert!(!judge.content(true, false));
        assert_eq!(judge.verdict(), &Verdict::Pending);
    }
}
