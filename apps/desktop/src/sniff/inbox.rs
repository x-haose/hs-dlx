//! 网页引擎报来、还没处理的事：一条按先后的通道。页面加载与故障不丢；网页能任意多发的（脚本的消息、标题、新窗口、
//! 下载）限量，积多了、太长的丢掉并计数。脚本报顶层文档的类型与正文的消息（定论要用）单独限量，不被别的消息挤掉。

use std::cell::Cell;
use std::rc::Rc;

use tokio::sync::mpsc;

use super::script;

/// 网页能任意多发、还没处理的事最多积多少条：网页发得比处理得快时，再来的丢掉并计数。
const QUEUE: usize = 256;
/// 脚本报顶层文档的消息最多积多少条：每个文档两条，正常远到不了；页面自己伪造着发时同样丢掉并计数。
const REPORTS: usize = 16;

/// 网页引擎报来的事。
pub(super) enum Engine {
    /// 开始显示这个地址（导航已提交）：之前找到的都属于上一个页面
    Started(String),
    Finished(String),
    /// 顶层导航没成：网页引擎给的原因（见 [`hs_dlx_webview::Event`]）
    LoadFailed(String),
    /// 顶层收到了 HTTP 响应（见 [`hs_dlx_webview::Event::Responded`]）：地址与状态码
    Responded {
        url: String,
        status: u16,
    },
    /// 顶层文档加载完时网页引擎给的状态码（见 [`hs_dlx_webview::Event::Completed`]）
    Completed(u16),
    /// 网页进程退出或卡死了
    Crashed,
    /// 脚本发回的消息与发它的框架的地址（网页引擎给的，见 [`hs_dlx_webview::ipc_frame`]）
    Message {
        body: String,
        frame: Option<String>,
    },
    Title(String),
    /// 网页要在新窗口里打开的地址（链接的 target=_blank、window.open）
    Popup(String),
    /// 网页要下载的文件的地址
    Download(String),
}

/// 能丢的事各自限量的那一份。
#[derive(Clone, Copy)]
enum Lane {
    /// 脚本报顶层文档的类型与正文（见 [`script::reports_page`]），限 [`REPORTS`] 条
    Reports,
    /// 别的网页能任意多发的，限 [`QUEUE`] 条
    Flood,
}

impl Engine {
    /// 积多了能丢的属于哪一份；网页引擎自己报的页面加载与故障不丢，为 None。
    fn lane(&self) -> Option<Lane> {
        match self {
            Engine::Started(_)
            | Engine::Finished(_)
            | Engine::LoadFailed(_)
            | Engine::Responded { .. }
            | Engine::Completed(_)
            | Engine::Crashed => None,
            Engine::Message { body, .. } if script::reports_page(body) => Some(Lane::Reports),
            Engine::Message { .. } | Engine::Title(_) | Engine::Popup(_) | Engine::Download(_) => {
                Some(Lane::Flood)
            }
        }
    }
}

pub(super) struct Inbox {
    sender: mpsc::UnboundedSender<Engine>,
    /// 在通道里的能丢的事的条数：报顶层文档的、别的
    pending: [Cell<usize>; 2],
    /// 积压满了或太长而丢掉的条数，从打开起算
    dropped: Cell<u64>,
}

impl Inbox {
    /// 新的收件箱与处理它的一方收事件的一端；收到的每一件都要交回 [`Inbox::received`]。
    pub fn new() -> (Rc<Inbox>, mpsc::UnboundedReceiver<Engine>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let inbox = Inbox {
            sender,
            pending: [Cell::new(0), Cell::new(0)],
            dropped: Cell::new(0),
        };
        (Rc::new(inbox), receiver)
    }

    /// 交给处理它的一方：能丢的积满了、消息太长时丢掉并计数，标题先截短；处理的一方已不在时什么也不做。
    pub fn offer(&self, event: Engine) {
        let event = match event {
            Engine::Message { body, .. } if body.len() > script::MAX_MESSAGE => {
                self.dropped.set(self.dropped.get() + 1);
                return;
            }
            Engine::Title(title) => Engine::Title(script::clip(&title)),
            event => event,
        };
        if let Some(lane) = event.lane() {
            let (pending, limit) = self.share(lane);
            if pending.get() >= limit {
                self.dropped.set(self.dropped.get() + 1);
                return;
            }
            pending.set(pending.get() + 1);
        }
        // 处理的一方已不在时没有人收
        let _closed = self.sender.send(event);
    }

    /// 处理的一方从通道里收到了 `event`。
    pub fn received(&self, event: &Engine) {
        if let Some(lane) = event.lane() {
            let (pending, _) = self.share(lane);
            pending.set(
                pending
                    .get()
                    .checked_sub(1)
                    .expect("能丢的事都经 offer 入队、入队时计过数"),
            );
        }
    }

    /// `lane` 那一份在通道里的条数与上限。
    fn share(&self, lane: Lane) -> (&Cell<usize>, usize) {
        match lane {
            Lane::Reports => (&self.pending[0], REPORTS),
            Lane::Flood => (&self.pending[1], QUEUE),
        }
    }

    /// 丢掉的条数，从打开起算。
    pub fn dropped(&self) -> u64 {
        self.dropped.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(text: &str) -> Engine {
        Engine::Title(text.to_owned())
    }

    #[test]
    fn page_loads_are_kept_while_what_pages_can_flood_is_bounded() {
        let (inbox, mut events) = Inbox::new();
        for index in 0..QUEUE + 3 {
            inbox.offer(title(&index.to_string()));
        }
        // 积满后再来的能丢的丢掉并计数；页面加载与故障照样进
        assert_eq!(inbox.dropped(), 3);
        inbox.offer(Engine::Started("https://a.example/".into()));
        inbox.offer(Engine::Crashed);
        let long = "x".repeat(script::MAX_MESSAGE + 1);
        inbox.offer(Engine::Message {
            body: long,
            frame: None,
        });
        assert_eq!(inbox.dropped(), 4);
        // 收走一件，就又能进一件
        let first = events.try_recv().unwrap();
        inbox.received(&first);
        inbox.offer(title("again"));
        assert_eq!(inbox.dropped(), 4);
        let mut received = vec![first];
        while let Ok(event) = events.try_recv() {
            inbox.received(&event);
            received.push(event);
        }
        // 按先后：能丢的在前，之后是页面加载、故障与收走一件后再进的那件
        assert_eq!(received.len(), QUEUE + 3);
        assert!(matches!(received[QUEUE], Engine::Started(_)));
        assert!(matches!(received[QUEUE + 1], Engine::Crashed));
        assert!(matches!(&received[QUEUE + 2], Engine::Title(text) if text == "again"));
    }

    /// 别的消息积满时，报顶层文档的消息照样进（它们另有限量）。
    #[test]
    fn page_reports_are_not_crowded_out() {
        let (inbox, mut events) = Inbox::new();
        for index in 0..QUEUE + 1 {
            inbox.offer(title(&index.to_string()));
        }
        assert_eq!(inbox.dropped(), 1);
        let report = r#"{"kind":"content","playlist":false,"challenge":false}"#;
        for _ in 0..REPORTS + 1 {
            inbox.offer(Engine::Message {
                body: report.to_owned(),
                frame: None,
            });
        }
        assert_eq!(inbox.dropped(), 2);
        let mut reports = 0;
        while let Ok(event) = events.try_recv() {
            inbox.received(&event);
            reports += usize::from(matches!(event, Engine::Message { .. }));
        }
        assert_eq!(reports, REPORTS);
    }
}
