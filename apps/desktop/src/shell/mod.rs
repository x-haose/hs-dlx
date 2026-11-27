//! 主窗口：顶栏下面，左边一栏是粘贴栏与分组的任务列表，右边是选中任务的舞台；没有任务时是待机（圆盘在正中，粘贴栏
//! 在它下面）。设置页、新建页与嗅探页占列表右边。提示在列表一栏的底部。任务的状态在 [`crate::tasks`] 里，这里管这个
//! 窗口自己的状态与事件；粘贴、新建、对任务的操作、退出时的询问与各块的画法在子模块里。

mod column;
mod creating;
mod list;
mod live_view;
mod look;
mod notices;
mod operations;
mod paste;
mod quit;
mod standby;
mod task_view;
mod title_bar;

pub use self::quit::Leave;

use std::path::PathBuf;
use std::time::Instant;

use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, Window, actions, div, px,
};
use hs_dlx_app::{Status, TaskId, curl};
use hs_dlx_instance::Handoff;
use hs_m3u8_core::{Proxy, Url};

use self::list::{ListEvent, TaskList};
use self::paste::{PasteBar, PasteEvent};
use crate::backdrop::{self, Backdrop};
use crate::headers::HeadersEditor;
use crate::new_task;
use crate::new_task::NewTask;
use crate::settings::{Section, SettingsEvent, SettingsView};
use crate::sniff::SniffView;
use crate::tasks::{Pasted, Row, Tasks};
use crate::theme;
use crate::ui::{PAD, TITLE_HEIGHT, max};
use crate::updates::Updates;
use crate::widgets;

actions!(shell, [PasteLink]);

/// 粘贴键（焦点不在输入框里时）在主窗口里的绑定：读剪贴板，建任务。
pub fn key_bindings() -> [gpui_kit::KeyBinding; 1] {
    let key = if cfg!(target_os = "macos") {
        "cmd-v"
    } else {
        "ctrl-v"
    };
    [gpui_kit::KeyBinding::new(key, PasteLink, Some(KEY_CONTEXT))]
}

const KEY_CONTEXT: &str = "Shell";

/// 列表一栏离窗口左边多远、与右边的间距、离顶栏多远。
const COLUMN_LEFT: f32 = 20.;
const COLUMN_GAP: f32 = 24.;
const COLUMN_TOP: f32 = 8.;
/// 列表一栏的宽：窗口宽的 41%，限在这两者之间。
const COLUMN_MIN: f32 = 420.;
const COLUMN_MAX: f32 = 592.;

/// 窗口的最小宽：放得下收到最窄的列表一栏与右边最宽的一个视图。
pub const MIN_WIDTH: f32 =
    COLUMN_LEFT + COLUMN_MIN + COLUMN_GAP + max(new_task::MIN_WIDTH, task_view::MIN_WIDTH) + PAD;
/// 窗口的最小高：放得下顶栏、最高的一个视图与下边距。
pub const MIN_HEIGHT: f32 = TITLE_HEIGHT + max(task_view::MIN_HEIGHT, new_task::MIN_HEIGHT) + PAD;

/// 列表一栏的宽：窗口宽 `width` 的 41%，限在 [`COLUMN_MIN`] 到 [`COLUMN_MAX`]；右边放不下它的最小宽 `right` 时先收
/// 这一栏。
fn column_width(width: f32, right: f32) -> f32 {
    let room = width - COLUMN_LEFT - COLUMN_GAP - PAD - right;
    (width * 0.41)
        .clamp(COLUMN_MIN, COLUMN_MAX)
        .min(room)
        .max(COLUMN_MIN)
}

/// 主体显示的页。
enum Page {
    /// 选中的任务；没有任务时为待机
    Task,
    /// 新建页：视图与对它的订阅（它的事件、它的探测进度），离开新建页时一起释放；`from_sniff` 为从嗅探页打开的，关掉
    /// 时回到嗅探页；`page_check` 为第一次探测说明地址可能是网页时，在后台打开它、看是不是网页的嗅探视图与对它的订阅
    NewTask {
        view: Entity<NewTask>,
        _subscriptions: [Subscription; 1],
        from_sniff: bool,
        page_check: Option<(Entity<SniffView>, Subscription)>,
    },
    /// 网页里的视频流：只经 [`Shell::show_sniff`] 换上，视图即 [`Shell::sniff`] 里的；网页显示着时占满主体，不显示列表
    /// 一栏
    Sniff(Entity<SniffView>),
}

/// 剪贴板里一条可以直接建任务的链接（见 [`clipboard_link`]）。
struct Clip {
    /// 剪贴板里的原文：用它时照原样粘贴
    text: String,
    /// 认出的地址
    url: Url,
}

/// 主窗口的视图：任务状态在 [`Tasks`] 里，这里是这个窗口自己的（显示哪一页、粘贴栏、列表、嗅探页）。
pub struct Shell {
    tasks: Entity<Tasks>,
    /// 更新：顶栏有新版本时提示，设置页里装
    updates: Entity<Updates>,
    /// 分组的任务列表，持有选中的任务
    list: Entity<TaskList>,
    paste: Entity<PasteBar>,
    /// 焦点没落在别处时在主窗口上：粘贴键经它建任务
    focus: FocusHandle,
    /// 主体的页，只经 [`Shell::show`] 换；设置开着时在它后面照常运行（新建页照常探测、后台的结论照常换页）
    page: Page,
    /// 设置页与对它的订阅：开着时盖在主体的页上，关上即露出那一页；关上时一起释放
    settings: Option<(Entity<SettingsView>, Subscription)>,
    /// 嗅探页与对它的订阅：关掉之前一直留着（不显示时网页藏起来、不出声），列表一栏里有一项可以回去
    sniff: Option<(Entity<SniffView>, Subscription)>,
    home: Option<PathBuf>,
    /// 剪贴板里一条还不是任务的链接
    clipboard: Option<Clip>,
    /// 设置里的代理（待机时底部那一行写新任务经哪里走）；打开窗口、关上设置时重读，读不出时为原因
    network: Result<Proxy, String>,
    /// 窗口的动态背景
    backdrop: Backdrop,
    /// 每隔 [`backdrop::FRAME`] 给背景送一次参数、换上画好的图；换了图时让窗口重画（列表是缓存的视图，不跟着重画）
    _frames: gpui_kit::Task<()>,
    /// 点过一次「放弃任务」、等着再点一次确认的任务
    confirm_discard: Option<TaskId>,
    /// 正在改请求头的停着的任务与编辑框
    editing_headers: Option<(TaskId, Entity<HeadersEditor>)>,
    /// BT 任务的诊断（见 task_view 的诊断一节）
    diagnosis: Option<task_view::Diagnosis>,
    _subscriptions: Vec<Subscription>,
}

impl Shell {
    /// `link` 为启动参数里给的链接：有就建任务。
    pub fn new(
        tasks: Entity<Tasks>,
        updates: Entity<Updates>,
        link: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let updated = cx.observe(&updates, |_, _, cx| cx.notify());
        let paste = cx.new(|cx| PasteBar::new(tasks.clone(), window, cx));
        paste.update(cx, |paste, cx| paste.focus(window, cx));
        let typed = cx.subscribe_in(
            &paste,
            window,
            |this: &mut Self, _, event: &PasteEvent, window, cx| match event {
                PasteEvent::Submit(text) => this.submit(text.clone(), window, cx),
                PasteEvent::UseClipboard => this.use_clipboard(window, cx),
            },
        );
        let list = cx.new(|cx| TaskList::new(tasks.clone(), cx));
        let picked = cx.subscribe(&list, |this: &mut Self, _, event: &ListEvent, cx| {
            let ListEvent::Selected { id, picked } = *event;
            this.selected_changed(id, picked, cx);
        });
        let activated = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.refresh_clipboard(cx);
            }
        });
        let changed = cx.observe(&tasks, |this: &mut Self, _, cx| {
            this.drop_used_clipboard(cx);
            this.sync_standby(cx);
            cx.notify();
        });
        let focus = cx.focus_handle();
        // 焦点所在的元素没了（关掉了设置、编辑框）：回到主窗口，粘贴键照样建任务
        let focus_lost = cx.on_focus_lost(window, |this: &mut Self, window, cx| {
            this.focus.focus(window, cx);
        });
        let frames = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(backdrop::FRAME).await;
                let ticked = this.update_in(cx, |this, _, cx| {
                    let changed = if this.backdrop.stopped() {
                        false
                    } else {
                        match this.backdrop.tick(cx) {
                            Ok(changed) => changed,
                            Err(backdrop::Stopped(error)) => {
                                this.tell(format!("动态背景没有启用，界面改用纯色底：{error}"), cx);
                                true
                            }
                        }
                    };
                    if changed {
                        cx.notify();
                    }
                });
                // 窗口关了
                if ticked.is_err() {
                    break;
                }
            }
        });
        let network = read_network(&tasks, cx);
        let mut shell = Shell {
            tasks,
            updates,
            list,
            paste,
            focus,
            page: Page::Task,
            settings: None,
            sniff: None,
            home: dirs::home_dir(),
            clipboard: None,
            network,
            backdrop: Backdrop::new(cx),
            _frames: frames,
            confirm_discard: None,
            editing_headers: None,
            diagnosis: None,
            _subscriptions: vec![typed, picked, activated, changed, focus_lost, updated],
        };
        shell.refresh_clipboard(cx);
        shell.sync_standby(cx);
        if let Some(link) = link {
            shell.submit(link, window, cx);
        }
        shell
    }

    /// 再次打开应用时转交来的（已接手，窗口已在前台）：有链接就建任务。
    pub fn handed_over(&mut self, handoff: Handoff, window: &mut Window, cx: &mut Context<Self>) {
        if let Handoff::Link(link) = handoff {
            self.submit(link, window, cx);
        }
        cx.notify();
    }

    /// 粘贴 `text`（粘贴栏、粘贴键、剪贴板提示、启动参数、转交来的、改用本机 BT）：建任务，结果交回粘贴栏；建出的
    /// 第一个选中并滚到看得见，同一个种子已在任务里的换到那个任务。
    pub(super) fn submit(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let pasted = match self.tasks.update(cx, |tasks, cx| tasks.paste(&text, cx)) {
            Ok(pasted) => pasted,
            Err(reason) => {
                self.paste.update(cx, |paste, cx| paste.refuse(reason, cx));
                return;
            }
        };
        if self
            .clipboard
            .as_ref()
            .is_some_and(|clip| clip.text.trim() == text.trim())
        {
            self.clipboard = None;
        }
        cx.spawn_in(window, async move |this, cx| {
            // 发送端在后台建完时一定发出，只在应用退出时随之释放
            let Ok(pasted) = pasted.await else {
                return;
            };
            // 等待期间窗口关了：建成的已在列表里，没建成的不再显示
            let _closed = this.update_in(cx, |this, window, cx| {
                this.pasted(&text, pasted, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn pasted(&mut self, text: &str, pasted: Pasted, window: &mut Window, cx: &mut Context<Self>) {
        self.paste.update(cx, |paste, cx| {
            paste.finish(text, &pasted.refused, window, cx)
        });
        let target = pasted
            .created
            .first()
            .copied()
            .or_else(|| pasted.refused.iter().find_map(|refusal| refusal.existing));
        if let Some(id) = target {
            self.list.update(cx, |list, cx| list.select(id, true, cx));
        }
    }

    /// 粘贴栏空着时按了回车：有剪贴板提示就用它。
    fn use_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(clip) = self.clipboard.take() {
            self.submit(clip.text, window, cx);
        }
    }

    /// 焦点不在输入框里时按了粘贴键：剪贴板里的文字建任务。
    fn paste_link(&mut self, _: &PasteLink, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        match text.filter(|text| !text.trim().is_empty()) {
            Some(text) => self.submit(text, window, cx),
            None => self.paste.update(cx, |paste, cx| {
                paste.refuse("剪贴板里没有文字".to_owned(), cx)
            }),
        }
    }

    /// 选中的任务：没有任务时为 None。
    fn selected(&self, cx: &gpui_kit::App) -> Option<TaskId> {
        self.list.read(cx).selected()
    }

    /// 选中的一行。
    fn selected_row<'a>(&self, tasks: &'a Tasks, cx: &gpui_kit::App) -> Option<&'a Row> {
        self.selected(cx).and_then(|id| tasks.row(id))
    }

    /// 选中换了：这个任务的放弃确认、请求头编辑框与诊断不再适用。用户点的（`picked`）换回任务页、关上设置；它在
    /// 等用户而可选项已不在（重开应用后、网页被关掉）时重新准备，准备好了再停在同一处。
    fn selected_changed(&mut self, id: Option<TaskId>, picked: bool, cx: &mut Context<Self>) {
        self.confirm_discard = None;
        self.editing_headers = None;
        self.diagnosis = None;
        if picked {
            self.show(Page::Task, cx);
            self.close_settings(cx);
            let stale = id
                .and_then(|id| self.tasks.read(cx).row(id))
                .is_some_and(|row| {
                    matches!(row.task.status, Status::Waiting(_))
                        && row.preparation.is_none()
                        && row.idle()
                });
            if let (true, Some(id)) = (stale, id) {
                self.tasks.update(cx, |tasks, cx| {
                    // 开始不了时原因已提示
                    let _started = tasks.resume(id, cx);
                });
            }
        }
        cx.notify();
    }

    /// 选中任务 `id`（换到已有的种子时）并换回任务页。
    fn select(&mut self, id: TaskId, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| list.select(id, true, cx));
    }

    /// 加一条要用户关掉的提示。
    fn tell(&mut self, text: String, cx: &mut Context<Self>) {
        self.tasks.update(cx, |tasks, cx| tasks.tell(text, cx));
    }

    /// 重读剪贴板：是认得的链接（见 [`clipboard_link`]）、还不是任务时记下，提示用它。
    fn refresh_clipboard(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|item| item.text());
        self.clipboard = text.and_then(|text| {
            let url = clipboard_link(&text)?;
            Some(Clip { text, url })
        });
        self.drop_used_clipboard(cx);
        cx.notify();
    }

    /// 剪贴板提示的链接已是任务了：不再提示。
    fn drop_used_clipboard(&mut self, cx: &mut Context<Self>) {
        let tasks = self.tasks.read(cx);
        if self
            .clipboard
            .as_ref()
            .is_some_and(|clip| tasks.rows().iter().any(|row| row.task.spec.url == clip.url))
        {
            self.clipboard = None;
        }
    }

    /// 换到 `page`（原来的新建页随之释放）；设置开着时照样换，关上设置即露出它。
    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        self.change_screen(|this| this.page = page, cx);
    }

    /// 在屏幕上的嗅探视图：主体是嗅探页、设置没开着时为它。
    fn sniff_on_screen(&self) -> Option<Entity<SniffView>> {
        match &self.page {
            Page::Sniff(view) if self.settings.is_none() => Some(view.clone()),
            Page::Sniff(_) | Page::Task | Page::NewTask { .. } => None,
        }
    }

    /// 做 `change`（换主体的页、开关设置），再告诉离开与换上屏幕的嗅探视图（见 [`SniffView::set_on_screen`]）、列表
    /// （选中的那一行是否正显示在右边）与待机。
    fn change_screen(&mut self, change: impl FnOnce(&mut Self), cx: &mut Context<Self>) {
        let was = self.sniff_on_screen();
        change(self);
        let will = self.sniff_on_screen();
        if was != will {
            if let Some(was) = was {
                was.update(cx, |sniff, cx| sniff.set_on_screen(false, cx));
            }
            if let Some(will) = will {
                will.update(cx, |sniff, cx| sniff.set_on_screen(true, cx));
            }
        }
        let showing = matches!(self.page, Page::Task) && self.settings.is_none();
        self.list
            .update(cx, |list, cx| list.set_showing(showing, cx));
        self.sync_standby(cx);
        cx.notify();
    }

    /// 待机（没有任务、主体是任务页、设置没开着）时粘贴栏在圆盘下面，高一档。
    fn standby(&self, cx: &gpui_kit::App) -> bool {
        self.tasks.read(cx).rows().is_empty()
            && matches!(self.page, Page::Task)
            && self.settings.is_none()
    }

    fn sync_standby(&mut self, cx: &mut Context<Self>) {
        let standby = self.standby(cx);
        self.paste
            .update(cx, |paste, cx| paste.set_standby(standby, cx));
    }

    /// 打开设置页；已开着时关上。
    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings(cx);
        } else {
            self.open_settings(window, cx);
        }
    }

    /// 关上设置页，露出主体的页，重读设置里的代理；没开着时什么也不做。
    pub(super) fn close_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.change_screen(|this| this.settings = None, cx);
            self.network = read_network(&self.tasks, cx);
        }
    }

    /// 打开设置页；已开着时什么也不做。读不出设置时提示原因。
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
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
        self.change_screen(|this| this.settings = Some((view, subscription)), cx);
    }

    /// 打开设置页的「更新与关于」一组。
    fn open_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_at(Section::About, window, cx);
    }

    /// 打开设置页（已开着时照旧）并换到 `section` 这一组。
    fn open_settings_at(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(window, cx);
        if let Some((view, _)) = &self.settings {
            view.update(cx, |view, cx| view.show(section, cx));
        }
    }

    /// 换到嗅探页（[`Shell::sniff`] 里的视图）；嗅探页已关时什么也不做（旧帧上的点击）。
    fn show_sniff(&mut self, cx: &mut Context<Self>) {
        if let Some((view, _)) = &self.sniff {
            self.show(Page::Sniff(view.clone()), cx);
        }
    }

    /// 主体的新建视图（设置开着时在它后面）；主体不是新建页时为 None。
    fn new_task(&self) -> Option<&Entity<NewTask>> {
        match &self.page {
            Page::NewTask { view, .. } => Some(view),
            Page::Task | Page::Sniff(_) => None,
        }
    }
}

/// 设置里的代理；读不出时为原因。
fn read_network(tasks: &Entity<Tasks>, cx: &gpui_kit::App) -> Result<Proxy, String> {
    tasks
        .read(cx)
        .service()
        .settings()
        .map(|settings| settings.proxy)
        .map_err(|error| error.to_string())
}

/// 剪贴板里的文字 `text` 是不是值得提示的链接：一条 curl 命令，或单独一行的 m3u8 链接（路径以 `.m3u8` 结尾的 http(s)
/// 地址）、磁力链接（带 `xt` 参数）。别的 http 链接不算：剪贴板里常有与下载无关的网址。是时为它的地址。
fn clipboard_link(text: &str) -> Option<Url> {
    let text = text.trim();
    if curl::is_command(text) {
        return curl::parse(text).ok().map(|request| request.url);
    }
    if text.lines().count() != 1 {
        return None;
    }
    let url = Url::parse(text).ok()?;
    let taken = match url.scheme() {
        "magnet" => url.query_pairs().any(|(name, _)| name == "xt"),
        "http" | "https" => url.path().to_ascii_lowercase().ends_with(".m3u8"),
        _ => false,
    };
    taken.then_some(url)
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        let width = f32::from(window.viewport_size().width);
        let right = match (&self.settings, &self.page) {
            (None, Page::NewTask { .. }) => new_task::MIN_WIDTH,
            _ => task_view::MIN_WIDTH,
        };
        let column = column_width(width, right);
        let standby = self.standby(cx);
        let body = if standby {
            self.render_standby(window, cx).into_any_element()
        } else {
            self.render_body(column, now, cx).into_any_element()
        };
        let cx = &*cx;
        let tasks = self.tasks.read(cx);
        widgets::typeset(div())
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::paste_link))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::ink())
            .text_size(px(14.))
            .child(backdrop::layer())
            .child(self.render_header(tasks, now, cx))
            .child(body)
            .child(self.render_notices(tasks, column, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_streams_magnets_and_curl_commands_are_offered_from_the_clipboard() {
        let hash = "0123456789abcdef0123456789abcdef01234567";
        assert!(clipboard_link(" https://a.example/v/index.m3u8?t=1 \n").is_some());
        assert!(clipboard_link(&format!("magnet:?xt=urn:btih:{hash}")).is_some());
        assert!(
            clipboard_link("curl 'https://a.example/v.mp4' -H 'Referer: https://a.example/'")
                .is_some()
        );
        assert!(clipboard_link("https://a.example/watch?v=1").is_none());
        assert!(clipboard_link("magnet:?dn=a").is_none());
        assert!(clipboard_link("https://a.example/a.m3u8\nhttps://a.example/b.m3u8").is_none());
    }

    #[test]
    fn the_column_follows_the_window_width() {
        let right = task_view::MIN_WIDTH;
        let least = COLUMN_LEFT + COLUMN_MIN + COLUMN_GAP + right + PAD;
        assert_eq!(column_width(2000., right), COLUMN_MAX);
        assert_eq!(column_width(least, right), COLUMN_MIN);
        assert_eq!(column_width(least - 100., right), COLUMN_MIN);
        // 41% 比右边放得下的宽：收到右边刚好放得下
        let width = least + 50.;
        assert!(width * 0.41 > COLUMN_MIN + 50.);
        assert!((column_width(width, right) - (COLUMN_MIN + 50.)).abs() < 0.01);
    }
}
