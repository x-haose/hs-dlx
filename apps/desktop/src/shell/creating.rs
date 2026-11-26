//! 新建：探测链接、开新建视图、新建任务（在 [`crate::tasks::Tasks`] 里）并把结果交回发起它的视图。

use gpui_kit::{AppContext as _, Context, Entity, Window};
use hs_dlx_app::Draft;
use hs_m3u8_core::Url;

use super::{Page, Shell};
use crate::links;
use crate::new_task::{Link, NewTask, NewTaskEvent, Phase};
use crate::sniff::{SniffEvent, SniffView, Verdict};

impl Shell {
    /// 探测 `text`：链接，或 curl 命令（地址与请求头一起交给新建页）；都不是时在粘贴栏里写原因。
    pub(super) fn probe(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let request = match links::request(text) {
            Ok(request) => request,
            Err(reason) => {
                // 嗅探页与设置页不放粘贴栏（转交来的内容可能在这时到）：关上设置露出主体的页，是嗅探页时再回任务页，
                // 原因才看得见
                self.close_settings(cx);
                if matches!(self.page, Page::Sniff(_)) {
                    self.show(Page::Task, cx);
                }
                self.dock.update(cx, |dock, cx| dock.refuse(reason, cx));
                return;
            }
        };
        let url = request.url;
        self.dock.update(cx, |dock, cx| dock.accepted(cx));
        self.confirm_discard = None;
        if self.clipboard.as_ref() == Some(&url) {
            self.clipboard = None;
        }
        self.open_new_task(
            Link {
                url,
                title: None,
                headers: request.headers,
                dropped: request.dropped,
                probe: None,
            },
            false,
            window,
            cx,
        );
    }

    /// 打开新建页探测 `link`（已开着的换成这个）；`from_sniff` 为嗅探页里找到的流（关掉时回到嗅探页），否则为粘贴、
    /// 启动参数或转交来的。
    pub(super) fn open_new_task(
        &mut self,
        link: Link,
        from_sniff: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 粘贴的、转交来的、嗅探页里点的：都是用户要看的，设置开着时关上
        self.close_settings(cx);
        let (tasks, dock) = (self.tasks.clone(), self.dock.clone());
        let view = cx.new(|cx| NewTask::new(tasks, link, from_sniff, dock, window, cx));
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this: &mut Self, view, event: &NewTaskEvent, window, cx| {
                match event {
                    NewTaskEvent::Create(draft) => {
                        this.create(view.clone(), (**draft).clone(), window, cx)
                    }
                    NewTaskEvent::Dismissed => {
                        if matches!(
                            this.page,
                            Page::NewTask {
                                from_sniff: true,
                                ..
                            }
                        ) && this.sniff.is_some()
                        {
                            this.show_sniff(cx);
                        } else {
                            this.show(Page::Task, cx);
                        }
                    }
                    NewTaskEvent::Ready => this.focus_new_task(view, window, cx),
                    NewTaskEvent::MaybePage => this.check_page(view, window, cx),
                    NewTaskEvent::Sniff(url) => this.open_sniff(url.clone(), window, cx),
                    // 同一个种子不能有两个任务：换到已有的那个，说一声（新建页随之关掉；设置开着时换在设置下面）。新建页
                    // 已换走的不再管
                    NewTaskEvent::Taken(existing) => {
                        let title = this
                            .tasks
                            .read(cx)
                            .row(*existing)
                            .map(|row| row.task.spec.title.clone());
                        if let Some(title) = title.filter(|_| this.new_task() == Some(view)) {
                            this.select(*existing, cx);
                            this.tell(format!("这个种子已在「{title}」里，换到了它"), cx);
                        }
                    }
                    // 这个任务照选的存；记成之后新建的默认位置，记不下时提示
                    NewTaskEvent::FolderChanged(folder) => {
                        let saved = this
                            .tasks
                            .update(cx, |tasks, cx| tasks.set_folder(folder.clone(), cx));
                        if let Err(error) = saved {
                            this.tell(
                                format!("新的保存位置没能记成默认的，下次新建时不在：{error}"),
                                cx,
                            );
                        }
                    }
                }
                cx.notify();
            },
        );
        // 探测到哪一步变了时告诉粘贴栏
        let observing = cx.observe(&view, |this: &mut Self, _, cx| this.sync_dock(cx));
        let ready = view.read(cx).phase() == Phase::Ready;
        self.show(
            Page::NewTask {
                view: view.clone(),
                _subscriptions: [subscription, observing],
                from_sniff,
                page_check: None,
            },
            cx,
        );
        if ready {
            self.focus_new_task(&view, window, cx);
        }
    }

    /// 新建页探测好了：把键盘焦点给它；用户还在粘贴栏里打别的链接时不抢，只在粘贴栏里还是这个链接、或焦点不在粘贴栏
    /// 时给。
    fn focus_new_task(
        &mut self,
        view: &Entity<NewTask>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 在设置后面探测好的不抢焦点（用户在设置里打字）
        if self.settings.is_some() || self.new_task() != Some(view) {
            return;
        }
        let dock = self.dock.read(cx);
        let typing = dock.typing(window, cx);
        let same = links::target(&dock.text(cx)).is_some_and(|url| &url == view.read(cx).url());
        if !typing || same {
            view.update(cx, |view, cx| view.focus(window, cx));
        }
    }

    /// 新建页 `view` 第一次探测的结果说明地址可能是网页：粘贴来的、地址也不像播放列表的，在后台的网页里打开它，认出是
    /// 网页就换到嗅探页找视频流（见 [`Shell::checked_page`]）；从嗅探页来的流、像播放列表的地址照常显示失败。
    fn check_page(&mut self, view: &Entity<NewTask>, window: &mut Window, cx: &mut Context<Self>) {
        let url = view.read(cx).url().clone();
        let Page::NewTask {
            view: shown,
            from_sniff: false,
            page_check,
            ..
        } = &mut self.page
        else {
            return;
        };
        if shown != view || links::stream_url(url.as_str()).is_some() {
            return;
        }
        let service = self.tasks.read(cx).service().clone();
        let sniff = cx.new(|cx| SniffView::new(service, url, window, cx));
        let subscription = cx.subscribe_in(
            &sniff,
            window,
            |this: &mut Self, sniff, event: &SniffEvent, window, cx| {
                if let SniffEvent::Decided = event {
                    this.checked_page(sniff, window, cx);
                }
            },
        );
        *page_check = Some((sniff.clone(), subscription));
        view.update(cx, |view, cx| view.opening_page(cx));
        // 建不出网页时一开始就有结论
        self.checked_page(&sniff, window, cx);
    }

    /// 后台的网页 `sniff` 有了是不是网页的结论（见 [`Verdict`]）：是网页就换到嗅探页，否则在新建页的失败下面写明（新建页
    /// 留着，可改请求头重新探测）。新建页已换走、或用户已改了请求头重新探测的，不再需要它。
    fn checked_page(
        &mut self,
        sniff: &Entity<SniffView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Page::NewTask {
            view, page_check, ..
        } = &mut self.page
        else {
            return;
        };
        if page_check
            .as_ref()
            .is_none_or(|(checked, _)| checked != sniff)
        {
            return;
        }
        if !view.read(cx).is_opening_page() {
            *page_check = None;
            return;
        }
        let note = match sniff.read(cx).verdict() {
            Verdict::Pending => return,
            Verdict::Page => None,
            Verdict::Refused(status) => Some(format!("网页引擎打开它回 HTTP {status}")),
            Verdict::Unknown => {
                Some("网页引擎没报出它的 HTTP 状态码，判断不了是不是网页".to_owned())
            }
            Verdict::NotPage(Some(kind)) => Some(format!("网页引擎打开它是 {kind}，不是网页")),
            Verdict::NotPage(None) => Some("网页引擎打开它也不是网页".to_owned()),
            Verdict::Playlist => Some("网页引擎打开它是 HLS 播放列表，不是网页".to_owned()),
            Verdict::Download => Some("网页引擎打开它是要下载的文件，不是网页".to_owned()),
            Verdict::Broken(reason) => Some(format!("在网页里也打不开：{reason}")),
        };
        *page_check = None;
        match note {
            Some(note) => view.update(cx, |view, cx| view.not_a_page(note, cx)),
            None => self.adopt_sniff(sniff.clone(), window, cx),
        }
        cx.notify();
    }

    /// 按新建视图里的选择新建任务；退出中不新建。新建期间新建视图显示正在开始；建成时新建视图还开着（用户没切走）
    /// 就关掉它、选中新任务、清空粘贴栏，失败时写在它上面，已切走时提示。
    pub(super) fn create(
        &mut self,
        view: Entity<NewTask>,
        draft: Draft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let created = match self.tasks.update(cx, |tasks, cx| tasks.create(draft, cx)) {
            Ok(created) => created,
            Err(reason) => {
                view.update(cx, |view, cx| view.failed(reason, cx));
                return;
            }
        };
        view.update(cx, |view, cx| view.creating(cx));
        let tasks = self.tasks.clone();
        let shown = hs_m3u8_core::bare_url(view.read(cx).url()).to_string();
        cx.spawn_in(window, async move |this, cx| {
            // 发送端在 Tasks 收尾时一定发出，只在应用退出时随它释放
            let Ok(result) = created.await else {
                return;
            };
            let failure = result.as_ref().err().cloned();
            let closed = this
                .update_in(cx, |this, window, cx| {
                    match result {
                        Ok(id) if this.showing(&view) => {
                            this.selected = Some(id);
                            this.show(Page::Task, cx);
                            this.dock
                                .update(cx, |dock, cx| dock.set_text(String::new(), window, cx));
                        }
                        Ok(_) => {}
                        Err(error) if this.showing(&view) => {
                            view.update(cx, |view, cx| view.failed(error, cx))
                        }
                        Err(error) => this.tell(format!("新建 {shown} 失败：{error}"), cx),
                    }
                    cx.notify();
                })
                .is_err();
            // 等待期间窗口关掉了：建成的已在列表里，失败改为提示
            if closed && let Some(error) = failure {
                tasks.update(cx, |tasks, cx| {
                    tasks.tell(format!("新建 {shown} 失败：{error}"), cx)
                });
            }
        })
        .detach();
    }

    /// 正显示着的新建视图是 `view`。
    pub(super) fn showing(&self, view: &Entity<NewTask>) -> bool {
        self.new_task() == Some(view)
    }

    /// 在嗅探页里打开网页 `url`（已开着的换成这个）。旧的视图连同它的网页与出口要到这一轮更新结束时才释放，新网页
    /// 推迟到那之后再建，出口才按此刻的设置定（见 [`crate::web_route::WebRoute::acquire`]）。
    fn open_sniff(&mut self, url: Url, window: &mut Window, cx: &mut Context<Self>) {
        self.sniff = None;
        cx.defer_in(window, move |this, window, cx| {
            let service = this.tasks.read(cx).service().clone();
            let view = cx.new(|cx| SniffView::new(service, url, window, cx));
            this.adopt_sniff(view, window, cx);
        });
    }

    /// 让 `view` 成为嗅探页（已开着的换成它）并显示。
    fn adopt_sniff(
        &mut self,
        view: Entity<SniffView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe_in(
            &view,
            window,
            |this: &mut Self, view, event: &SniffEvent, window, cx| {
                match event {
                    SniffEvent::Open(link) => this.open_new_task(link.clone(), true, window, cx),
                    SniffEvent::Dismissed => {
                        if matches!(&this.page, Page::Sniff(shown) if shown == view) {
                            this.show(Page::Task, cx);
                        }
                        this.sniff = None;
                    }
                    SniffEvent::PageToggled | SniffEvent::Decided => {}
                }
                cx.notify();
            },
        );
        self.sniff = Some((view, subscription));
        self.show_sniff(cx);
    }
}
