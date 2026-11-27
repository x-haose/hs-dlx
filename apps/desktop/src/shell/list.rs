//! 左栏的任务列表：分四组（等你处理、进行中、已暂停、已完成；空的组不显示），组标题也是一行。虚拟列表，只排、只画
//! 看得见的行；它是单独缓存的视图，窗口为背景逐帧重画时不跟着重画，只在任务或选中变了、有准备中的行要转动时重画。

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ListAlignment, ListState, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, canvas, div, list,
    px,
};
use hs_dlx_app::{
    Ask, CloudProgress, Kind, Offer, PageAsk, Stage, Status, Step, TaskId, TorrentPhase,
};

use crate::tasks::{Amount, FileAmount, Row, Tasks, finished_text};
use crate::widgets::{self, Button, Kind as ButtonKind, Size, Toward};
use crate::{clock, format, theme};

/// 一行的高。
const ROW_HEIGHT: f32 = 58.;
/// 相邻两行的间距。
const ROW_GAP: f32 = 2.;
/// 行首小环的边长与环宽。
const MARK: f32 = 30.;
const MARK_WIDTH: f32 = 3.;
/// 准备中的小弧：占整圈多少、转一圈多久、多久重画一次。
const SEEK_SPAN: f32 = 0.28;
const SEEK_TURN: Duration = Duration::from_millis(1600);
const SEEK_FRAME: Duration = Duration::from_millis(50);

/// 列表里的一组。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    /// 停下等用户、失败、放弃没做完：进这个状态的先后，新的在上
    Waiting,
    /// 准备中、在运行：新建先后，旧的在上
    Active,
    /// 暂停：新建先后
    Paused,
    /// 完成：在做种的在最前，其余按完成时刻、新的在上，没有完成时刻的（旧版本完成的）在后、按新建先后
    Done,
}

impl Group {
    pub fn of(status: &Status) -> Group {
        match status {
            Status::Waiting(_) | Status::Failed(_) | Status::DiscardUnfinished => Group::Waiting,
            Status::Preparing | Status::Running => Group::Active,
            Status::Paused => Group::Paused,
            Status::Done => Group::Done,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Group::Waiting => "等你处理",
            Group::Active => "进行中",
            Group::Paused => "已暂停",
            Group::Done => "已完成",
        }
    }
}

/// 列表的一行：组标题（组与里面的任务数）或一个任务。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry {
    Group(Group, usize),
    Task(TaskId, Group),
}

/// 列表告诉窗口的。
pub enum ListEvent {
    /// 选中换了：`picked` 为用户点的（点行或行上的按钮），否则为选中的任务被移除后换的；没有任务时为 None
    Selected { id: Option<TaskId>, picked: bool },
}

pub struct TaskList {
    tasks: Entity<Tasks>,
    /// 按显示的顺序
    entries: Vec<Entry>,
    state: ListState,
    selected: Option<TaskId>,
    /// 要选中、还没加进列表的任务（粘贴的结果可能比任务的事件先到）：加进来时选中它
    wanted: Option<TaskId>,
    /// 选中的任务正显示在右边（主体是任务页、设置没开着）；不是时选中的一行不点亮
    showing: bool,
    /// 有准备中的行时让小弧转动的定时器；没有时为 None
    spinning: Option<Task<()>>,
    /// 小弧从哪一刻开始转
    spin_since: Instant,
    _tasks: Subscription,
}

impl EventEmitter<ListEvent> for TaskList {}

impl TaskList {
    pub fn new(tasks: Entity<Tasks>, cx: &mut Context<Self>) -> TaskList {
        let observing = cx.observe(&tasks, |this: &mut Self, _, cx| this.sync(cx));
        let mut list = TaskList {
            tasks,
            entries: Vec::new(),
            state: ListState::new(0, ListAlignment::Top, px(ROW_HEIGHT * 4.)),
            selected: None,
            wanted: None,
            showing: true,
            spinning: None,
            spin_since: Instant::now(),
            _tasks: observing,
        };
        list.sync(cx);
        list.selected = list.first_task();
        list
    }

    pub fn selected(&self) -> Option<TaskId> {
        self.selected
    }

    /// 选中任务 `id` 并滚到看得见（点的、新粘贴的、换到已有的）；它还不在列表里时等它加进来再选。`picked` 见
    /// [`ListEvent::Selected`]。
    pub fn select(&mut self, id: TaskId, picked: bool, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            self.wanted = Some(id);
            return;
        };
        self.wanted = None;
        self.state.scroll_to_reveal_item(index);
        self.selected = Some(id);
        cx.emit(ListEvent::Selected {
            id: Some(id),
            picked,
        });
        cx.notify();
    }

    /// 选中的任务是否正显示在右边，见 [`TaskList::showing`] 字段。
    pub fn set_showing(&mut self, showing: bool, cx: &mut Context<Self>) {
        if self.showing != showing {
            self.showing = showing;
            cx.notify();
        }
    }

    fn index_of(&self, id: TaskId) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| matches!(entry, Entry::Task(at, _) if *at == id))
    }

    fn first_task(&self) -> Option<TaskId> {
        self.entries.iter().find_map(|entry| match entry {
            Entry::Task(id, _) => Some(*id),
            Entry::Group(..) => None,
        })
    }

    /// 任务变了：重排各行，只把变了的一段告诉虚拟列表（滚动位置不动）；选中的任务不在了时换一个；有准备中的行时
    /// 让小弧转起来。
    fn sync(&mut self, cx: &mut Context<Self>) {
        let entries = arrange(self.tasks.read(cx).rows());
        let old = std::mem::replace(&mut self.entries, entries);
        let prefix = old
            .iter()
            .zip(&self.entries)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(self.entries[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        if prefix < old.len().max(self.entries.len()) {
            self.state.splice(
                prefix..old.len() - suffix,
                self.entries.len() - prefix - suffix,
            );
        }
        if let Some(id) = self.wanted
            && self.index_of(id).is_some()
        {
            self.select(id, true, cx);
        } else if let Some(id) = self.selected
            && self.index_of(id).is_none()
        {
            self.selected = successor(&old, &self.entries, id);
            cx.emit(ListEvent::Selected {
                id: self.selected,
                picked: false,
            });
        } else if self.selected.is_none() {
            // 别处建的任务（没有选中的时候）：选中列表里的第一个
            self.selected = self.first_task();
            if self.selected.is_some() {
                cx.emit(ListEvent::Selected {
                    id: self.selected,
                    picked: false,
                });
            }
        }
        let preparing = self
            .tasks
            .read(cx)
            .rows()
            .iter()
            .any(|row| row.task.status == Status::Preparing);
        if !preparing || cx.reduce_motion() {
            self.spinning = None;
        } else if self.spinning.is_none() {
            self.spin_since = Instant::now();
            self.spinning = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(SEEK_FRAME).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    fn render_entry(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(entry) = self.entries.get(index).copied() else {
            // 虚拟列表的项数与各行在同一次更新里改，取不到即不变量被破坏
            unreachable!("虚拟列表的项数随各行一起改");
        };
        match entry {
            Entry::Group(group, count) => group_title(group, count, index == 0).into_any_element(),
            Entry::Task(id, group) => {
                let tasks = self.tasks.read(cx);
                let Some(row) = tasks.row(id) else {
                    unreachable!("各行随任务的变化重排，列表里的任务都在");
                };
                let now = Instant::now();
                let pages = pages_open(tasks.rows());
                let look = look(row, now, unix_now(), (pages, tasks.web_pages()));
                let selected = self.selected == Some(id);
                let shown = Shown {
                    lit: selected && self.showing,
                    selected,
                    spin: self.spin_since.elapsed().as_secs_f32() / SEEK_TURN.as_secs_f32(),
                };
                self.render_row(row, group, look, shown, cx)
                    .into_any_element()
            }
        }
    }

    fn render_row(
        &self,
        row: &Row,
        group: Group,
        look: Look,
        shown: Shown,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let Shown {
            lit,
            selected,
            spin,
        } = shown;
        let id = row.task.id;
        let color = if group == Group::Waiting {
            theme::warn()
        } else if matches!(look.mark, Mark::Live) {
            theme::live()
        } else {
            theme::accent()
        };
        let item = div()
            .id(("task", id.0 as u64))
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .gap(px(14.))
            .px(px(14.))
            .rounded(px(widgets::CONTROL_RADIUS))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.select(id, true, cx)));
        let item = if lit {
            widgets::lit(item, Toward::Right, color)
        } else {
            item.border_1()
                .border_color(gpui_kit::transparent_black())
                .hover(|s| s.bg(theme::wash()))
        };
        let right = match look.right {
            Right::Ask(label) if selected => div()
                .text_size(px(13.))
                .text_color(theme::warn())
                .child(label)
                .into_any_element(),
            // 点了与点这一行相同：选中它，舞台上是要做的事
            Right::Ask(label) => Button::new(("ask", id.0 as u64), label, ButtonKind::Warn)
                .size(Size::Small)
                .into_any_element(),
            Right::Text { main, sub, color } => div()
                .flex()
                .flex_col()
                .items_end()
                .gap(px(2.))
                .text_size(px(13.))
                .text_color(color)
                .whitespace_nowrap()
                .child(main)
                .children(sub.map(|sub| {
                    div()
                        .text_size(px(11.))
                        .text_color(theme::ink_3())
                        .child(sub)
                }))
                .into_any_element(),
            Right::None => div().into_any_element(),
        };
        div().pb(px(ROW_GAP)).child(
            item.child(mark(look.mark, spin))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .truncate()
                                .child(row.task.spec.title.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::ink_2())
                                .truncate()
                                .child(look.detail),
                        ),
                )
                .child(div().flex_none().child(right)),
        )
    }
}

/// 一行此刻怎么画。
#[derive(Clone, Copy)]
struct Shown {
    /// 点亮（选中且正显示在右边）
    lit: bool,
    selected: bool,
    /// 准备中的小弧转到哪（整圈为 1）
    spin: f32,
}

impl Render for TaskList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        list(
            self.state.clone(),
            cx.processor(|this, index, window, cx| this.render_entry(index, window, cx)),
        )
        .size_full()
        .pb(px(8.))
    }
}

/// 组标题：组名与里面的任务数；等你处理的组名为警告色。
fn group_title(group: Group, count: usize, first: bool) -> impl IntoElement {
    div()
        .flex()
        .items_baseline()
        .gap(px(8.))
        .px(px(14.))
        .pt(px(if first { 10. } else { 16. }))
        .pb(px(6.))
        .text_size(px(11.))
        .text_color(theme::ink_3())
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(if group == Group::Waiting {
                    theme::warn()
                } else {
                    theme::alpha(theme::ink(), 0.8)
                })
                .child(group.title()),
        )
        .child(count.to_string())
}

/// 按组、组内按各自的顺序排好的各行（见 [`Group`]）。
fn arrange(rows: &[Row]) -> Vec<Entry> {
    let mut groups: [Vec<&Row>; 4] = Default::default();
    for row in rows {
        let at = match Group::of(&row.task.status) {
            Group::Waiting => 0,
            Group::Active => 1,
            Group::Paused => 2,
            Group::Done => 3,
        };
        groups[at].push(row);
    }
    // 各行本来按新建先后；排序是稳定的，相同的保持新建先后
    groups[0].sort_by_key(|row| std::cmp::Reverse(row.since));
    groups[3].sort_by_key(|row| {
        (
            !row.task.seeding,
            row.task.finished_at.is_none(),
            std::cmp::Reverse(row.task.finished_at),
        )
    });
    let mut entries = Vec::new();
    for (group, rows) in [Group::Waiting, Group::Active, Group::Paused, Group::Done]
        .into_iter()
        .zip(groups)
    {
        if rows.is_empty() {
            continue;
        }
        entries.push(Entry::Group(group, rows.len()));
        entries.extend(rows.iter().map(|row| Entry::Task(row.task.id, group)));
    }
    entries
}

/// 选中的任务 `gone` 不在了（放弃、移除）时换选哪个：原来同组里它后面还在的第一个，没有时为它前面还在的最近一个；
/// 组里都不在了时为「进行中」的第一个，再没有时为列表里的第一个。`old` 为它还在时的各行，`new` 为现在的。
fn successor(old: &[Entry], new: &[Entry], gone: TaskId) -> Option<TaskId> {
    let present = |id: &TaskId| {
        new.iter()
            .any(|entry| matches!(entry, Entry::Task(at, _) if at == id))
    };
    let in_group = |group: Group| -> Vec<TaskId> {
        old.iter()
            .filter_map(|entry| match entry {
                Entry::Task(id, at) if *at == group => Some(*id),
                _ => None,
            })
            .collect()
    };
    let group = old.iter().find_map(|entry| match entry {
        Entry::Task(id, group) if *id == gone => Some(*group),
        _ => None,
    });
    if let Some(group) = group {
        let siblings = in_group(group);
        let at = siblings.iter().position(|id| *id == gone).unwrap_or(0);
        let after = siblings[at..].iter().find(|id| present(id));
        let before = siblings[..at].iter().rev().find(|id| present(id));
        if let Some(id) = after.or(before) {
            return Some(*id);
        }
    }
    let first_in = |group: Option<Group>| {
        new.iter().find_map(|entry| match entry {
            Entry::Task(id, at) if group.is_none_or(|group| *at == group) => Some(*id),
            _ => None,
        })
    };
    first_in(Some(Group::Active)).or_else(|| first_in(None))
}

/// 开着内置网页的任务数：准备到在网页里找流的，与停在网页上等用户的。
fn pages_open(rows: &[Row]) -> usize {
    rows.iter()
        .filter(|row| {
            matches!(
                (&row.task.status, &row.preparation),
                (Status::Preparing, Some(Stage::Working(Step::Page { .. })))
                    | (Status::Waiting(Ask::Page(_)), Some(_))
            )
        })
        .count()
}

/// 此刻的 Unix 秒；系统时钟早于 1970 年时为 None。
fn unix_now() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
}

/// 行首的小环。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Mark {
    /// 准备中：一小段弧绕行
    Seeking,
    /// 等用户处理：警告色虚线圈加圆点
    Needs,
    /// 进度环：比例（0 到 1）与颜色
    Progress(f32, Hsla),
    /// 直播录制：直播色圈加实心点
    Live,
    /// 整圈：完成为灰，做种为 `deep`
    Full(Hsla),
}

/// 行的右边一栏。
#[derive(Debug, Clone, PartialEq)]
enum Right {
    /// 一行字，下面可以再有一行小字
    Text {
        main: String,
        sub: Option<String>,
        color: Hsla,
    },
    /// 等用户：没选中时是小的警告按钮，选中时是文字
    Ask(&'static str),
    None,
}

/// 一行的样子：行首、第二行与右边。
#[derive(Debug, Clone, PartialEq)]
struct Look {
    mark: Mark,
    detail: String,
    right: Right,
}

/// 任务 `row` 在列表里的样子；`now` 为算速度的时刻，`unix` 为此刻的 Unix 秒（系统时钟早于 1970 年时为 None，不写
/// 粘贴了多久），`pages` 为开着的内置网页数与上限。
fn look(row: &Row, now: Instant, unix: Option<i64>, pages: (usize, u8)) -> Look {
    let source = format::source(&row.task.spec.url);
    let text = |main: String| Right::Text {
        main,
        sub: None,
        color: theme::ink_2(),
    };
    if row.discarding {
        return Look {
            mark: Mark::Progress(0., theme::alpha(theme::ink(), 0.34)),
            detail: format!("{source} · 放弃中"),
            right: Right::None,
        };
    }
    match &row.task.status {
        Status::Preparing => {
            let step = match &row.preparation {
                Some(Stage::Working(step)) => Some(step),
                _ => None,
            };
            let right = match (step, unix) {
                (Some(Step::Page { .. }), _) => text(format!("网页 {} / {}", pages.0, pages.1)),
                (_, Some(unix)) => text(pasted_ago(unix - row.task.created_at)),
                (_, None) => Right::None,
            };
            Look {
                mark: Mark::Seeking,
                detail: step.map_or_else(|| "准备中".to_owned(), step_text),
                right,
            }
        }
        Status::Waiting(ask) => {
            let offer = match &row.preparation {
                Some(Stage::Asking(offer)) => Some(offer),
                _ => None,
            };
            Look {
                mark: Mark::Needs,
                detail: format!("{source} · {}", asking(*ask, offer)),
                right: Right::Ask(ask_label(*ask)),
            }
        }
        Status::Running if row.recording() => {
            let amount = Amount::of(row).unwrap_or_default();
            Look {
                mark: Mark::Live,
                detail: format!("{source} · 录制中 · {}", format::bytes(amount.bytes)),
                right: Right::Text {
                    main: format::clock(amount.duration),
                    sub: Some("已录".to_owned()),
                    color: theme::ink(),
                },
            }
        }
        Status::Running => {
            let fraction = row.progress_share().unwrap_or(0.);
            let parts = [Some(source), Some(kind(row)), running_amount(row)];
            // 写出（不联网）时环换成日冕核心的颜色
            let color = if row.written_share().is_some() {
                theme::core()
            } else {
                theme::accent()
            };
            Look {
                mark: Mark::Progress(fraction, color),
                detail: join(parts),
                right: speed(row, now),
            }
        }
        Status::Paused => {
            let fraction = kept_share(row);
            let amount = if row.task.spec.pending().is_some() {
                Some("继续时从头准备".to_owned())
            } else {
                kept_amount(row)
            };
            Look {
                mark: Mark::Progress(fraction, theme::alpha(theme::ink(), 0.5)),
                detail: join([Some(source), Some(kind(row)), amount]),
                right: text(
                    if row.resuming {
                        "正在继续"
                    } else {
                        "已暂停"
                    }
                    .to_owned(),
                ),
            }
        }
        Status::Failed(failure) => {
            let (short, hint) = format::failure(failure.kind);
            let kept = kept_amount(row).map(|amount| format!("已下 {amount}"));
            Look {
                mark: Mark::Needs,
                detail: join([Some(source), Some(first_clause(hint).to_owned()), kept]),
                right: Right::Text {
                    main: if row.resuming {
                        "正在继续".to_owned()
                    } else {
                        short
                    },
                    sub: None,
                    color: theme::warn(),
                },
            }
        }
        Status::DiscardUnfinished => Look {
            mark: Mark::Needs,
            detail: format!("{source} · 放弃没做完，要再放弃一次"),
            right: Right::None,
        },
        Status::Done if row.task.seeding => {
            let progress = row.torrent_progress.as_ref();
            let uploaded = progress.map_or_else(String::new, |progress| {
                format!(
                    " · 已上传 {} · 分享率 {}",
                    format::bytes(progress.uploaded),
                    format::ratio(progress.uploaded, progress.wanted)
                )
            });
            Look {
                mark: Mark::Full(theme::deep()),
                detail: format!("BT{uploaded}"),
                right: progress.map_or(Right::None, |progress| {
                    text(format!("↑ {}/s", format::bytes(progress.upload_rate)))
                }),
            }
        }
        Status::Done => Look {
            mark: Mark::Full(theme::alpha(theme::ink(), 0.34)),
            detail: row.task.result.as_ref().map_or_else(
                || row.file_name.clone(),
                |result| finished_text(&row.task.spec, result),
            ),
            right: row
                .task
                .finished_at
                .and_then(clock::day)
                .map_or(Right::None, text),
        },
    }
}

/// 准备中的一步。
fn step_text(step: &Step) -> String {
    match step {
        Step::Identifying => "正在认地址".to_owned(),
        Step::Resolving { .. } => "正在解析网页".to_owned(),
        Step::Installing(_) => "正在装解析组件".to_owned(),
        Step::Metadata(progress) => format!("取种子信息 · {} 个节点", progress.peers),
        Step::PageQueued { ahead: 0, .. } => "排队等网页".to_owned(),
        Step::PageQueued { ahead, .. } => format!("排队等网页 · 前面 {ahead} 个"),
        Step::Page { found: 0 } => "在后台网页里找视频流".to_owned(),
        Step::Page { found } => format!("在后台网页里找视频流 · 找到 {found} 条"),
        Step::Probing => "正在探测找到的流".to_owned(),
        Step::Starting => "正在开始下载".to_owned(),
    }
}

/// 等用户的任务在等什么；可选项还在时写出有几个可选的。
fn asking(ask: Ask, offer: Option<&Offer>) -> String {
    match (ask, offer) {
        (Ask::Quality, Some(Offer::Quality(probe))) => {
            let count = probe
                .master
                .as_ref()
                .map_or(1, |master| master.variants.len());
            format!("找到 {count} 档清晰度")
        }
        (Ask::Quality, Some(Offer::WebQuality(resolved))) => {
            format!("找到 {} 档画质", resolved.choices.len())
        }
        (Ask::Quality, _) => "等你选画质".to_owned(),
        (Ask::Files, Some(Offer::Files { info, .. })) => format!("{} 个文件", info.files.len()),
        (Ask::Files, _) => "等你选文件".to_owned(),
        (Ask::Stream, Some(Offer::Streams(streams))) => {
            format!("找到 {} 条视频流", streams.len())
        }
        (Ask::Stream, _) => "等你选视频流".to_owned(),
        (Ask::Confirm, _) => "开始前确认".to_owned(),
        (Ask::Page(PageAsk::Challenge), _) => "网站要验证是不是真人".to_owned(),
        (Ask::Page(PageAsk::Login), _) => "网站要登录".to_owned(),
        (Ask::Page(PageAsk::Play), _) => "网页里没找到视频，要你点播放".to_owned(),
        (Ask::Plugin, _) => "几个插件都认这个网址".to_owned(),
        (Ask::Components, _) => "要先装解析组件".to_owned(),
    }
}

/// 等用户的任务右边的按钮。
fn ask_label(ask: Ask) -> &'static str {
    match ask {
        Ask::Quality => "选画质",
        Ask::Files => "选文件",
        Ask::Stream => "选一条",
        Ask::Confirm => "确认",
        Ask::Page(PageAsk::Challenge) => "去验证",
        Ask::Page(PageAsk::Login) => "去登录",
        Ask::Page(PageAsk::Play) => "去点播放",
        Ask::Plugin => "选插件",
        Ask::Components => "装组件",
    }
}

/// 粘贴了多久（`seconds` 秒）：一分钟内写「刚刚粘贴」。
fn pasted_ago(seconds: i64) -> String {
    match seconds {
        ..60 => "刚刚粘贴".to_owned(),
        60..3600 => format!("粘贴了 {} 分钟", seconds / 60),
        _ => format!("粘贴了 {} 小时", seconds / 3600),
    }
}

/// 任务的种类；还没定下的为「还没定下」。
fn kind(row: &Row) -> String {
    match &row.task.spec.kind {
        Kind::Pending(_) => "还没定下",
        Kind::Hls(_) => "HLS",
        Kind::File(_) => "文件",
        Kind::Torrent(_) => "BT",
        Kind::Cloud(_) => "云离线",
        Kind::Web(_) => "网页视频",
    }
    .to_owned()
}

/// 在运行的任务下了多少：HLS 为片数（写出时为写出），BT 另写节点数，云离线在网盘那边下时为网盘报的百分比；还不
/// 知道时为 None。
fn running_amount(row: &Row) -> Option<String> {
    if row.written_share().is_some() {
        return Some("写出中".to_owned());
    }
    if let Some(amount) = Amount::of(row) {
        return match amount.total {
            0 if amount.done == 0 => None,
            0 => Some(format!("{} 片", amount.done)),
            total => Some(format!("{} / {total} 片", amount.done)),
        };
    }
    if let Some(CloudProgress::Remote { percent, .. }) = row.cloud_progress.as_ref() {
        return Some(format!("网盘在下 {percent}%"));
    }
    let bytes = FileAmount::of(row).map(|amount| bytes_text(&amount));
    match row.torrent_progress.as_ref() {
        Some(progress) if progress.phase == TorrentPhase::Downloading => Some(match bytes {
            Some(bytes) => format!("{} 个节点 · {bytes}", progress.peers),
            None => format!("{} 个节点", progress.peers),
        }),
        _ => bytes,
    }
}

/// 停着的任务已有的：按任务目录里读到的（BT 为续传数据，云离线为存储里记着的）；没有内容或还没读到时为 None。
fn kept_amount(row: &Row) -> Option<String> {
    if let Some(amount) = Amount::of(row) {
        if row.recording() {
            return (amount.done > 0).then(|| format!("已录 {}", format::clock(amount.duration)));
        }
        return match amount.total {
            _ if amount.done == 0 => None,
            0 => Some(format!("{} 片", amount.done)),
            total => Some(format!("{} / {total} 片", amount.done)),
        };
    }
    FileAmount::of(row)
        .filter(|amount| amount.bytes > 0)
        .map(|amount| bytes_text(&amount))
}

/// 停着的任务已有的比例（0 到 1）：直播录制与不知道合计的为 0。
fn kept_share(row: &Row) -> f32 {
    if let Some(amount) = Amount::of(row) {
        return if row.recording() {
            0.
        } else {
            amount.fraction()
        };
    }
    FileAmount::of(row)
        .and_then(|amount| amount.fraction())
        .unwrap_or(0.)
}

fn bytes_text(amount: &FileAmount) -> String {
    match amount.total {
        Some(total) => format!("{} / {}", format::bytes(amount.bytes), format::bytes(total)),
        None => format::bytes(amount.bytes),
    }
}

/// 在运行的任务右边：速度，下面是剩余时间（算得出时）。
fn speed(row: &Row, now: Instant) -> Right {
    let rate = row.rate.bytes_per_second(now);
    let left = if let Some(amount) = Amount::of(row).filter(|amount| amount.total > 0) {
        row.rate
            .remaining(now, amount.total.saturating_sub(amount.done))
    } else {
        FileAmount::of(row)
            .and_then(|amount| amount.total.map(|total| total.saturating_sub(amount.bytes)))
            .filter(|_| rate > 0.)
            .map(|left| Duration::from_secs_f64(left as f64 / rate))
    };
    Right::Text {
        main: format!("{}/s", format::bytes(rate as u64)),
        sub: left.map(|left| format!("剩 {}", format::duration(left))),
        color: theme::ink(),
    }
}

/// 失败说明的第一句：到第一个逗号、冒号、分号或括号为止。
fn first_clause(hint: &str) -> &str {
    hint.split(['，', '：', '；', '（']).next().unwrap_or(hint)
}

fn join<const N: usize>(parts: [Option<String>; N]) -> String {
    parts.into_iter().flatten().collect::<Vec<_>>().join(" · ")
}

/// 行首的小环；`spin` 为准备中的小弧此刻转到哪（整圈为 1）。
fn mark(mark: Mark, spin: f32) -> impl IntoElement {
    let frame = div().size(px(MARK)).flex_none();
    let track = theme::alpha(theme::ink(), 0.14);
    match mark {
        Mark::Needs => frame
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border(px(1.5))
            .border_dashed()
            .border_color(theme::alpha(theme::warn(), 0.9))
            .child(widgets::dot(theme::warn(), 6.))
            .into_any_element(),
        Mark::Live => frame
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .border_2()
            .border_color(theme::live())
            .child(widgets::dot(theme::live(), 8.))
            .into_any_element(),
        Mark::Seeking | Mark::Progress(..) | Mark::Full(_) => {
            let (from, span, color) = match mark {
                Mark::Seeking => (spin, SEEK_SPAN, theme::accent()),
                Mark::Progress(fraction, color) => (0., fraction, color),
                Mark::Full(color) => (0., 1., color),
                Mark::Needs | Mark::Live => unreachable!("上面两支已处理"),
            };
            frame
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, (), window, _| {
                            widgets::ring::arc(bounds, MARK_WIDTH, 0., 1., track, window);
                            widgets::ring::arc(bounds, MARK_WIDTH, from, span, color, window);
                        },
                    )
                    .size_full(),
                )
                .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use hs_dlx_app::{Failure, FailureKind, Status, TaskId};

    use super::*;
    use crate::tasks::sample;

    fn row(id: i64, status: Status, since: u64) -> Row {
        let mut row = sample::row(status, false, None);
        row.task.id = TaskId(id);
        row.since = since;
        row
    }

    fn failed() -> Status {
        Status::Failed(Failure {
            kind: FailureKind::Network,
            message: "断了".into(),
        })
    }

    /// 四组按定下的顺序；等你处理的新进的在上，完成的做种在前、再按完成时刻新的在上、没有完成时刻的在后。
    #[test]
    fn rows_are_grouped_and_ordered() {
        let mut done_old = row(5, Status::Done, 0);
        done_old.task.finished_at = Some(100);
        let mut done_new = row(6, Status::Done, 0);
        done_new.task.finished_at = Some(200);
        let done_unknown = row(7, Status::Done, 0);
        let mut seeding = row(8, Status::Done, 0);
        seeding.task.seeding = true;
        let rows = vec![
            row(1, failed(), 3),
            row(2, Status::Running, 0),
            row(3, Status::Waiting(Ask::Quality), 9),
            row(4, Status::Paused, 0),
            done_unknown,
            done_old,
            done_new,
            seeding,
            row(9, Status::Preparing, 0),
        ];
        let task = |id, group| Entry::Task(TaskId(id), group);
        assert_eq!(
            arrange(&rows),
            vec![
                Entry::Group(Group::Waiting, 2),
                task(3, Group::Waiting),
                task(1, Group::Waiting),
                Entry::Group(Group::Active, 2),
                task(2, Group::Active),
                task(9, Group::Active),
                Entry::Group(Group::Paused, 1),
                task(4, Group::Paused),
                Entry::Group(Group::Done, 4),
                task(8, Group::Done),
                task(6, Group::Done),
                task(5, Group::Done),
                task(7, Group::Done),
            ]
        );
    }

    /// 选中的被移除：选同组的下一个，没有时选前一个；组空了选进行中的第一个，再没有时选列表里的第一个。
    #[test]
    fn removing_the_selected_task_selects_a_neighbour() {
        let rows = vec![
            row(1, Status::Paused, 0),
            row(2, Status::Paused, 0),
            row(3, Status::Paused, 0),
            row(4, Status::Running, 0),
            row(5, Status::Done, 0),
        ];
        let old = arrange(&rows);
        let without = |gone: &[i64]| {
            let kept: Vec<Row> = rows
                .iter()
                .filter(|row| !gone.contains(&row.task.id.0))
                .map(|row| self::row(row.task.id.0, row.task.status.clone(), 0))
                .collect();
            arrange(&kept)
        };
        assert_eq!(successor(&old, &without(&[2]), TaskId(2)), Some(TaskId(3)));
        assert_eq!(successor(&old, &without(&[3]), TaskId(3)), Some(TaskId(2)));
        let only_paused = arrange(&rows[..3]);
        let none_left = arrange(&[row(4, Status::Running, 0), row(5, Status::Done, 0)]);
        assert_eq!(
            successor(&only_paused, &none_left, TaskId(1)),
            Some(TaskId(4))
        );
        let done_only = arrange(&[row(5, Status::Done, 0)]);
        assert_eq!(
            successor(&arrange(&rows[3..]), &done_only, TaskId(4)),
            Some(TaskId(5))
        );
        assert_eq!(successor(&done_only, &[], TaskId(5)), None);
    }

    /// 准备中的写这一步，在网页里找流时右边写开着几个网页；等用户的写在等什么与按钮；失败的写说明的第一句与简写。
    #[test]
    fn rows_say_what_the_task_is_doing() {
        let now = Instant::now();
        let mut preparing = row(1, Status::Preparing, 0);
        preparing.preparation = Some(Stage::Working(Step::Page { found: 2 }));
        let seen = look(&preparing, now, Some(0), (2, 2));
        assert_eq!(seen.detail, "在后台网页里找视频流 · 找到 2 条");
        assert!(matches!(seen.right, Right::Text { main, .. } if main == "网页 2 / 2"));
        preparing.preparation = Some(Stage::Working(Step::Identifying));
        let seen = look(&preparing, now, Some(30), (0, 2));
        assert!(matches!(seen.right, Right::Text { main, .. } if main == "刚刚粘贴"));

        let waiting = row(2, Status::Waiting(Ask::Page(PageAsk::Challenge)), 0);
        let seen = look(&waiting, now, Some(0), (0, 2));
        assert_eq!(seen.detail, "a.example · 网站要验证是不是真人");
        assert_eq!(seen.right, Right::Ask("去验证"));

        let forbidden = row(
            3,
            Status::Failed(Failure {
                kind: FailureKind::Status(403),
                message: "HTTP 403".into(),
            }),
            0,
        );
        let seen = look(&forbidden, now, Some(0), (0, 2));
        assert_eq!(seen.detail, "a.example · 服务器拒绝了请求");
        assert!(matches!(seen.right, Right::Text { main, .. } if main == "403"));
    }
}
