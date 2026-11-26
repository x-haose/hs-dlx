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

/// 判断到哪一步。网页类型的文档要正文就绪（或加载完）且知道了它的 HTTP 状态码才定论。状态码由网页引擎在文档显示
/// 出来之前报（macOS、Linux），到文档建出时归给这个文档；或由页面在正文就绪时一并报（Windows）。
#[derive(Debug)]
pub(super) struct Judge {
    verdict: Verdict,
    /// 顶层文档报过网页的类型，等正文就绪再定（按网页类型发来的播放列表，如 PHP 的默认类型）
    html: bool,
    /// 正文就绪且不是播放列表：值为这一页是整页的人机验证
    content: Option<bool>,
    /// 网页引擎报来、还没有文档认领的状态码：下一个建出的顶层文档的
    pending: Option<u16>,
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
            status: None,
        }
    }

    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// 网页引擎拿到了顶层文档的响应：状态码 `status`，归给接着建出的文档。
    pub fn responded(&mut self, status: u16) {
        self.pending = Some(status);
    }

    /// 顶层文档建出时报了类型：网页类型等正文就绪再定，别的即不是网页。有了结论时为 true（下同）。
    pub fn document(&mut self, content_type: &str) -> bool {
        self.status = self.pending.take();
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

    /// 顶层文档的正文就绪：`playlist` 为正文以 `#EXTM3U` 开头，`challenge` 为整页的人机验证，`status` 为页面读到的
    /// 状态码（网页引擎没报时用它）。
    pub fn content(&mut self, playlist: bool, challenge: bool, status: Option<u16>) -> bool {
        if !self.html {
            return false;
        }
        if playlist {
            return self.decide(Verdict::Playlist);
        }
        self.content = Some(challenge);
        self.status = self.status.or(status);
        self.page()
    }

    /// 顶层文档加载完：报过网页类型的按状态码定（正文没报来时不算人机验证），这时还不知道状态码即网页引擎与页面都
    /// 没报，定为打不开；没报过类型的不是网页。
    pub fn finished(&mut self) -> bool {
        if !self.html {
            return self.decide(Verdict::NotPage(None));
        }
        self.content.get_or_insert(false);
        if self.status.is_none() {
            return self.decide(Verdict::Broken(
                "网页引擎没报出这一页的 HTTP 状态码".to_owned(),
            ));
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
        // 网页：网页引擎先报状态码（macOS、Linux），类型是 HTML、状态码 2xx、正文不是播放列表
        let mut judge = Judge::new();
        judge.responded(200);
        assert!(!judge.document("text/html"));
        assert!(judge.content(false, false, None));
        assert_eq!(judge.verdict(), &Verdict::Page);
        // 之后的事不改结论
        assert!(!judge.fail("网页进程退出了".into()));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 页面随正文报状态码（Windows）
        let mut judge = Judge::new();
        judge.document("text/html");
        assert!(judge.content(false, false, Some(204)));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 网页引擎打开也是出错的页面：拒绝；是整页的人机验证（如 Cloudflare 回 403）：网页
        let mut judge = Judge::new();
        judge.responded(403);
        judge.document("text/html");
        assert!(judge.content(false, false, None));
        assert_eq!(judge.verdict(), &Verdict::Refused(403));
        let mut judge = Judge::new();
        judge.responded(403);
        judge.document("text/html");
        assert!(judge.content(false, true, None));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 第一页还没就绪时下一次导航的响应先到：归给下一个文档，不算这一页的
        let mut judge = Judge::new();
        judge.responded(200);
        judge.document("text/html");
        judge.responded(403);
        assert!(judge.content(false, false, None));
        assert_eq!(judge.verdict(), &Verdict::Page);

        // 按网页类型发来的播放列表：不等状态码
        let mut judge = Judge::new();
        judge.document("text/html");
        assert!(judge.content(true, false, None));
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

        // 加载完了也不知道状态码：打不开，写明原因（不等到时限）
        let mut judge = Judge::new();
        judge.document("text/html");
        judge.content(false, false, None);
        assert!(judge.finished());
        assert!(matches!(judge.verdict(), Verdict::Broken(_)));

        // 正文就绪只在报过网页类型之后才算
        let mut judge = Judge::new();
        judge.responded(200);
        assert!(!judge.content(true, false, None));
        assert_eq!(judge.verdict(), &Verdict::Pending);
    }
}
