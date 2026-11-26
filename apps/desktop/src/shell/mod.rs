//! 主窗口：顶栏下面，左边的任务栏面板列出任务的小环；中间的舞台是选中任务的表盘（新建时换成同心环，没有任务时是
//! 待机的表盘）与底部的粘贴栏；右边的检查面板是这个任务的读数、信号与操作。嗅探页与设置页占满任务栏右边。右上角是
//! 要用户关掉的提示。任务的状态在 [`crate::tasks`] 里，这里管这个窗口自己的状态与事件；新建、对任务的操作、退出时
//! 的询问与各块的画法在子模块里。

mod creating;
mod live_view;
mod look;
mod notices;
mod operations;
mod plan;
mod quit;
mod standby;
mod task_list;
mod task_view;
mod title_bar;

pub use self::quit::Leave;

use std::path::PathBuf;
use std::time::Instant;

use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    Subscription, Task, Window, div, px,
};
use hs_dlx_app::TaskId;
use hs_dlx_instance::Handoff;

use crate::backdrop::{self, Backdrop};
use crate::dial;
use crate::dock::{Dock, DockEvent};
use crate::headers::HeadersEditor;
use crate::links;
use crate::motion;
use crate::new_task;
use crate::new_task::NewTask;
use crate::settings::{Section, SettingsEvent, SettingsView};
use crate::sniff::SniffView;
use crate::tasks::{Row, Tasks};
use crate::theme;
use crate::ui::{GAP, PAD, TITLE_HEIGHT, max};
use crate::updates::Updates;

/// 窗口的最小宽：放得下任务栏收到最窄时、最宽的一个视图。
pub const MIN_WIDTH: f32 =
    2. * PAD + task_list::MIN_WIDTH + GAP + max(new_task::MIN_WIDTH, task_view::MIN_WIDTH);
/// 窗口的最小高：放得下顶栏、最高的一个视图与下边距。
pub const MIN_HEIGHT: f32 = TITLE_HEIGHT + max(task_view::MIN_HEIGHT, new_task::MIN_HEIGHT) + PAD;

/// 主体显示的页。
enum Page {
    /// 选中的任务；没有任务时为待机页
    Task,
    /// 新建页：视图与对它的订阅（它的事件、它的探测进度推给粘贴栏），离开新建页时一起释放；`from_sniff` 为从嗅探页
    /// 打开的，关掉时回到嗅探页；
    /// `page_check` 为第一次探测取到的不是播放列表时，在后台打开它、看是不是网页的嗅探视图与对它的订阅
    NewTask {
        view: Entity<NewTask>,
        _subscriptions: [Subscription; 2],
        from_sniff: bool,
        page_check: Option<(Entity<SniffView>, Subscription)>,
    },
    /// 设置页：视图与对它的订阅，离开时一起释放；`back` 为打开设置时换下的那一页，原样留着（新建页照常探测），关上设置
    /// 时回到它
    Settings {
        view: Entity<SettingsView>,
        _subscription: Subscription,
        back: Box<Page>,
    },
    /// 网页里的视频流：只经 [`Shell::show_sniff`] 换上，视图即 [`Shell::sniff`] 里的；网页显示着时占满主体，不显示任务
    /// 列表
    Sniff(Entity<SniffView>),
}

/// 显示着的这一页 `page`；设置页开着时为它后面的那一页（新建页在后面照常探测，它引起的换页落在这一页上）。
fn underlying(page: &mut Page) -> &mut Page {
    match page {
        Page::Settings { back, .. } => back,
        page => page,
    }
}

/// 主窗口的视图：任务状态在 [`Tasks`] 里，这里是这个窗口自己的（选中哪个任务、显示哪一页、粘贴栏、嗅探页）。
pub struct Shell {
    tasks: Entity<Tasks>,
    /// 更新：顶栏有新版本时提示，设置页里装
    updates: Entity<Updates>,
    /// 选中的任务：打开窗口时与选中的被移除时为最新的一个；没有任务时为 None
    selected: Option<TaskId>,
    /// 主体显示的页，只经 [`Shell::show`] 换
    page: Page,
    /// 嗅探页与对它的订阅：关掉之前一直留着（不显示时网页藏起来、不出声），任务栏里有一项可以回去
    sniff: Option<(Entity<SniffView>, Subscription)>,
    /// 粘贴栏：任务页、待机页与新建页的舞台里放的同一个
    dock: Entity<Dock>,
    home: Option<PathBuf>,
    /// 剪贴板里一条还没下过、也不在探测中的 m3u8 链接
    clipboard: Option<hs_m3u8_core::Url>,
    /// 待机页逐帧重画的定时器：在待机页时有，离开即释放（见 [`Shell::sync_standby`]）
    standby_frames: Option<Task<()>>,
    /// 窗口的动态背景
    backdrop: Backdrop,
    /// 每隔 [`backdrop::FRAME`] 给背景送一次参数、换上画好的图，并让窗口重画（装饰动效的节拍，见
    /// [`crate::motion`]）：窗口在前台、系统没要求减少动态效果时每次都重画，否则只在背景换了图时重画。
    /// 简化：整窗每次都重画（任务栏每一项的小环都重新细分），任务很多、机器很弱时成为瓶颈再把任务栏拆成缓存的视图
    _frames: Task<()>,
    /// 这次进入待机页的时刻：彗尾与背光按它走
    standby_since: Instant,
    /// 进入待机页时读到的代理设置（待机页写新任务经哪里走；在设置页改了之后回来会重读）；读不出时为原因
    standby_proxy: Result<hs_m3u8_core::Proxy, String>,
    /// 点过一次「放弃任务」、等着再点一次确认的任务
    confirm_discard: Option<TaskId>,
    /// 正在改请求头的停着的任务与编辑框
    editing_headers: Option<(TaskId, Entity<HeadersEditor>)>,
    /// BT 任务的诊断（见 task_view 的诊断一节）
    diagnosis: Option<task_view::Diagnosis>,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    /// `link` 为启动参数里给的链接：有就填进粘贴栏并探测。
    pub fn new(
        tasks: Entity<Tasks>,
        updates: Entity<Updates>,
        link: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let updated = cx.observe(&updates, |_, _, cx| cx.notify());
        let dock = cx.new(|cx| Dock::new(window, cx));
        dock.update(cx, |dock, cx| dock.focus(window, cx));
        let typed = cx.subscribe_in(
            &dock,
            window,
            |this: &mut Self, _, event: &DockEvent, window, cx| match event {
                DockEvent::Probe(text) => this.probe(text, window, cx),
            },
        );
        let activated = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh_clipboard(cx);
            }
        });
        // 选中的一行被移除（放弃、从列表移除）时改选当时最新的一行
        let changed = cx.observe(&tasks, |this: &mut Self, tasks, cx| {
            let tasks = tasks.read(cx);
            if this.selected.is_none_or(|id| tasks.row(id).is_none()) {
                this.selected = tasks.rows().last().map(|row| row.task.id);
            }
            this.sync_standby(cx);
            cx.notify();
        });
        let selected = tasks.read(cx).rows().last().map(|row| row.task.id);
        let frames = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(backdrop::FRAME).await;
                let ticked = this.update_in(cx, |this, window, cx| {
                    let changed = if this.backdrop.stopped() {
                        false
                    } else {
                        let speed = this.tasks.read(cx).activity(Instant::now()).speed;
                        match this.backdrop.tick(speed, cx) {
                            Ok(changed) => changed,
                            Err(backdrop::Stopped(error)) => {
                                this.tell(format!("动态背景没有启用，界面改用纯色底：{error}"), cx);
                                true
                            }
                        }
                    };
                    if changed || (window.is_window_active() && !cx.reduce_motion()) {
                        cx.notify();
                    }
                });
                // 窗口关了
                if ticked.is_err() {
                    break;
                }
            }
        });
        let mut shell = Shell {
            tasks,
            updates,
            selected,
            page: Page::Task,
            sniff: None,
            dock,
            home: dirs::home_dir(),
            clipboard: None,
            standby_frames: None,
            standby_since: Instant::now(),
            backdrop: Backdrop::new(cx),
            _frames: frames,
            standby_proxy: Ok(hs_m3u8_core::Proxy::System),
            confirm_discard: None,
            editing_headers: None,
            diagnosis: None,
            _subscriptions: vec![typed, activated, changed, updated],
        };
        shell.refresh_clipboard(cx);
        shell.sync_standby(cx);
        if let Some(link) = link {
            shell.probe_text(&link, window, cx);
        }
        shell
    }

    /// 再次打开应用时转交来的（已接手，窗口已在前台）：有链接就探测，新建页开着时不替换它，只填进粘贴栏。
    pub fn handed_over(&mut self, handoff: Handoff, window: &mut Window, cx: &mut Context<Self>) {
        if let Handoff::Link(link) = handoff {
            if self.new_task().is_some() {
                self.dock.update(cx, |dock, cx| dock.put(&link, window, cx));
                self.tell(
                    "再次打开应用时带来的链接已填进粘贴栏：新建页还开着，回车即换成探测它"
                        .to_owned(),
                    cx,
                );
            } else {
                self.probe_text(&link, window, cx);
            }
        }
        cx.notify();
    }

    /// 把 `text` 放进粘贴栏（见 [`Dock::put`]）并探测。
    pub(super) fn probe_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.dock.update(cx, |dock, cx| dock.put(text, window, cx));
        self.probe(text, window, cx);
    }

    /// 选中的一行；没有任务时为 None。
    fn selected_row<'a>(&self, tasks: &'a Tasks) -> Option<&'a Row> {
        self.selected.and_then(|id| tasks.row(id))
    }

    /// 加一条要用户关掉的提示。
    fn tell(&mut self, text: String, cx: &mut Context<Self>) {
        self.tasks.update(cx, |tasks, cx| tasks.tell(text, cx));
    }

    /// 重读剪贴板：是 http(s) 的 m3u8 链接、不是已有任务也不在探测中时记下，供一键探测。
    fn refresh_clipboard(&mut self, cx: &mut Context<Self>) {
        let found = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .and_then(|text| links::stream_url(&text));
        let probing = self.new_task().map(|view| view.read(cx).url().clone());
        let tasks = self.tasks.read(cx);
        self.clipboard = found.filter(|url| {
            !tasks.rows().iter().any(|row| &row.task.spec.url == url)
                && probing.as_ref() != Some(url)
        });
        cx.notify();
    }

    /// 选中任务 `id`；它已不在列表里（点在移除之前画的那一帧上）时什么也不做。
    fn select(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if self.tasks.read(cx).row(id).is_none() {
            return;
        }
        self.selected = Some(id);
        self.confirm_discard = None;
        self.editing_headers = None;
        // 诊断只对诊断那一刻：换了任务就不再显示
        self.diagnosis = None;
        self.show(Page::Task, cx);
    }

    /// 换到 `page`（原来的新建页随之释放）；告诉离开与换上的嗅探视图（见 [`SniffView::set_on_screen`]）。
    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        let sniff_of = |page: &Page| match page {
            Page::Sniff(view) => Some(view.clone()),
            Page::Task | Page::NewTask { .. } | Page::Settings { .. } => None,
        };
        let (was, will) = (sniff_of(&self.page), sniff_of(&page));
        if was != will {
            if let Some(was) = was {
                was.update(cx, |sniff, cx| sniff.set_on_screen(false, cx));
            }
            if let Some(will) = will {
                will.update(cx, |sniff, cx| sniff.set_on_screen(true, cx));
            }
        }
        self.page = page;
        self.sync_dock(cx);
        self.sync_standby(cx);
        cx.notify();
    }

    /// 把显示着的新建页探测的链接与探测到哪一步告诉粘贴栏（没有新建页时告诉它没有）。
    fn sync_dock(&mut self, cx: &mut Context<Self>) {
        let probing = self.new_task().map(|view| {
            let view = view.read(cx);
            (view.url().clone(), view.phase().label())
        });
        self.dock
            .update(cx, |dock, cx| dock.set_probing(probing, cx));
    }

    /// 显示着待机页（任务页、没有任务）时每隔 [`dial::frame`]（按 [`dial::STANDBY_TURN`]） 重画一次，让彗尾慢慢扫、背光起伏（系统要求减少动态
    /// 效果时不画）；离开待机页即停。
    fn sync_standby(&mut self, cx: &mut Context<Self>) {
        let standby = matches!(self.page, Page::Task) && self.selected.is_none();
        if !standby {
            self.standby_frames = None;
            return;
        }
        if self.standby_frames.is_some() {
            return;
        }
        self.standby_since = Instant::now();
        self.standby_proxy = self
            .tasks
            .read(cx)
            .service()
            .settings()
            .map(|settings| settings.proxy)
            .map_err(|error| error.to_string());
        self.standby_frames = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(dial::frame(dial::STANDBY_TURN))
                    .await;
                let closed = this.update(cx, |_, cx| {
                    if !cx.reduce_motion() {
                        cx.notify();
                    }
                });
                if closed.is_err() {
                    break;
                }
            }
        }));
    }

    /// 打开设置页；已开着时关上。
    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.page, Page::Settings { .. }) {
            self.close_settings(cx);
        } else {
            self.open_settings(window, cx);
        }
    }

    /// 换到 `page`；设置页开着时不打断它，换掉它后面的那一页（关上设置即看到）。用于不是用户点出来的换页（后台的结论）。
    fn show_under(&mut self, page: Page, cx: &mut Context<Self>) {
        match &mut self.page {
            Page::Settings { back, .. } => **back = page,
            _ => self.show(page, cx),
        }
    }

    /// 关上设置页，回到打开它时换下的那一页；不在设置页时什么也不做。
    pub(super) fn close_settings(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.page, Page::Settings { .. }) {
            return;
        }
        if let Page::Settings { back, .. } = std::mem::replace(&mut self.page, Page::Task) {
            self.show(*back, cx);
        }
    }

    /// 打开设置页；已开着时什么也不做。读不出设置时提示原因。
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.page, Page::Settings { .. }) {
            return;
        }
        let settings = match self.tasks.read(cx).service().settings() {
            Ok(settings) => settings,
            Err(error) => {
                self.tell(format!("读不出设置：{error}"), cx);
                return;
            }
        };
        let home = self.home.clone();
        let (tasks, updates) = (self.tasks.clone(), self.updates.clone());
        let view = cx.new(|cx| SettingsView::new(tasks, updates, home, settings, window, cx));
        let subscription = cx.subscribe(&view, |this: &mut Self, _, event: &SettingsEvent, cx| {
            match event {
                SettingsEvent::Dismissed => this.close_settings(cx),
            }
            cx.notify();
        });
        let back = Box::new(std::mem::replace(&mut self.page, Page::Task));
        // 换下的是嗅探页时告诉它不在屏幕上了（show 只看换下的这一页，这时已是任务页）
        if let Page::Sniff(sniff) = &*back {
            sniff.update(cx, |sniff, cx| sniff.set_on_screen(false, cx));
        }
        self.show(
            Page::Settings {
                view,
                _subscription: subscription,
                back,
            },
            cx,
        );
    }

    /// 打开设置页的「更新与关于」一组。
    fn open_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_at(Section::About, window, cx);
    }

    /// 打开设置页（已开着时照旧）并换到 `section` 这一组。
    fn open_settings_at(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(window, cx);
        if let Page::Settings { view, .. } = &self.page {
            view.update(cx, |view, cx| view.show(section, cx));
        }
    }

    /// 换到嗅探页（[`Shell::sniff`] 里的视图）；嗅探页已关时什么也不做（旧帧上的点击）。
    fn show_sniff(&mut self, cx: &mut Context<Self>) {
        if let Some((view, _)) = &self.sniff {
            self.show(Page::Sniff(view.clone()), cx);
        }
    }

    /// 显示着的新建视图；不在新建页时为 None。
    fn new_task(&self) -> Option<&Entity<NewTask>> {
        match &self.page {
            Page::NewTask { view, .. } => Some(view),
            Page::Task | Page::Sniff(_) | Page::Settings { .. } => None,
        }
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        motion::set_still(cx.reduce_motion());
        let cx = &*cx;
        let tasks = self.tasks.read(cx);
        let mut body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .gap(px(GAP))
            .px(px(PAD))
            .pb(px(PAD));
        // 网页显示着时占满主体，不显示任务栏
        let page_shown = matches!(&self.page, Page::Sniff(sniff) if sniff.read(cx).page_shown());
        if !page_shown {
            body = body.child(self.render_list(tasks, cx));
        }
        body = match &self.page {
            Page::NewTask { view, .. } => body.child(view.clone()),
            Page::Sniff(view) => body.child(view.clone()),
            Page::Settings { view, .. } => body.child(view.clone()),
            Page::Task => match self.selected_row(tasks) {
                Some(row) => {
                    let (stage, inspector) = self.render_task(row, now, cx);
                    body.child(stage).child(inspector)
                }
                None => {
                    let (stage, inspector) = self.render_standby(tasks, window, cx);
                    body.child(stage).child(inspector)
                }
            },
        };
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::ground())
            .text_color(theme::text())
            .text_size(px(14.))
            .child(backdrop::layer())
            .child(self.render_header(tasks, now, cx))
            .child(body)
            .child(self.render_notices(tasks, cx))
    }
}
