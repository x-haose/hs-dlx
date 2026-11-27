//! 粘贴栏：链接、磁力链接或 curl 命令，可以一次多条。回车发出框里的；框里空着或全选时粘贴的不进框，直接发出
//! （[`PasteEvent::Submit`]）。右端是保存目录（点了选目录，选了即成为默认保存目录）、「种子文件」与粘贴键的键帽。
//! 外面交回结果（[`PasteBar::finish`]）：都建成了清空；没建成的行把原因写在栏下面，整段都没建成的原文留在框里。

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, IntoElement,
    ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _, Subscription, Window,
    div, px,
};
use hs_dlx_app::curl;
use hs_m3u8_core::Url;

use crate::tasks::{Refusal, Tasks};
use crate::widgets::{self, Button, CONTROL_RADIUS, Kind, Size};
use crate::{format, theme};

/// 列表一栏顶上的高；待机时在圆盘下面，高一档。
const HEIGHT: f32 = 44.;
const STANDBY_HEIGHT: f32 = 52.;
/// 栏下面最多写几行没建成的原因；多的合成一行。
const SHOWN_REFUSALS: usize = 3;

/// 粘贴键的写法。
const PASTE_KEY: &str = if cfg!(target_os = "macos") {
    "⌘V"
} else {
    "Ctrl+V"
};

pub enum PasteEvent {
    /// 要建任务的文字：粘贴的原样（含换行），或框里的；框里是放进来的 curl 命令的地址时为整条命令
    Submit(String),
    /// 框里空着时按了回车（有剪贴板提示时用它）
    UseClipboard,
}

pub struct PasteBar {
    tasks: Entity<Tasks>,
    home: Option<std::path::PathBuf>,
    input: Entity<InputState>,
    /// 放进框里的 curl 命令：框里显示的它的地址与整条命令；改了框里的内容即清掉
    command: Option<(SharedString, String)>,
    /// 写在栏下面的：上次没建成的行与原因、选目录没成的原因；改了框里的内容即清掉
    problems: Vec<String>,
    /// 在待机页的圆盘下面（高一档）
    standby: bool,
    _typed: Subscription,
}

impl EventEmitter<PasteEvent> for PasteBar {}

impl PasteBar {
    pub fn new(tasks: Entity<Tasks>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("粘贴链接、磁力或 curl 命令，可以一次粘贴多条")
        });
        let typed = cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = this.submission(cx);
                    cx.emit(if text.trim().is_empty() {
                        PasteEvent::UseClipboard
                    } else {
                        PasteEvent::Submit(text)
                    });
                }
                InputEvent::Change => {
                    this.problems.clear();
                    this.command = None;
                    cx.notify();
                }
                InputEvent::Focus | InputEvent::Blur => cx.notify(),
            },
        );
        PasteBar {
            tasks,
            home: dirs::home_dir(),
            input,
            command: None,
            problems: Vec::new(),
            standby: false,
            _typed: typed,
        }
    }

    /// 回车时发出的：框里是放进来的 curl 命令的地址时为整条命令，否则为框里的内容。
    fn submission(&self, cx: &gpui_kit::App) -> String {
        let value = self.input.read(cx).value();
        match &self.command {
            Some((shown, command)) if *shown == value => command.clone(),
            _ => value.to_string(),
        }
    }

    /// 交回一次提交的结果：`text` 为提交的文字，`refused` 为没建成的行。都建成了清空框与栏下面的；整段没建成时原因
    /// 写在下面，只有一行（或是一条 curl 命令）的原文放回框里；其余没建成的行连同原因写在下面，框清空。
    pub fn finish(
        &mut self,
        text: &str,
        refused: &[Refusal],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let whole = refused.len() == 1 && refused[0].text.trim() == text.trim();
        if whole && (text.trim().lines().count() == 1 || curl::is_command(text)) {
            self.put(text, window, cx);
        } else {
            self.set_text(String::new(), window, cx);
        }
        self.problems = if whole {
            vec![refused[0].reason.clone()]
        } else {
            let mut lines: Vec<String> = refused
                .iter()
                .take(SHOWN_REFUSALS)
                .map(|refusal| format!("{}：{}", first_line(&refusal.text), refusal.reason))
                .collect();
            if refused.len() > SHOWN_REFUSALS {
                lines.push(format!("还有 {} 行没建成", refused.len() - SHOWN_REFUSALS));
            }
            lines
        };
        cx.notify();
    }

    /// 外面没能提交（正在退出、系统时钟不对）：原因写在栏下面，框里的留着。
    pub fn refuse(&mut self, reason: String, cx: &mut Context<Self>) {
        self.problems = vec![reason];
        cx.notify();
    }

    /// 把 `text` 放进框里：curl 命令换成它的地址（命令里的凭据不明文显示）并记下整条命令，认不出的命令照原样放。
    fn put(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let command = curl::is_command(text)
            .then(|| curl::parse(text).ok())
            .flatten()
            .map(|request| SharedString::from(request.url.to_string()));
        match command {
            Some(shown) => {
                self.set_text(shown.to_string(), window, cx);
                self.command = Some((shown, text.to_owned()));
            }
            None => self.set_text(text.trim().to_owned(), window, cx),
        }
    }

    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.command = None;
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 键盘焦点在框里（用户可能正在输入）。
    pub fn typing(&self, window: &Window, cx: &gpui_kit::App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// 在待机页的圆盘下面（高一档）还是在列表一栏顶上。
    pub fn set_standby(&mut self, standby: bool, cx: &mut Context<Self>) {
        if self.standby != standby {
            self.standby = standby;
            cx.notify();
        }
    }

    /// 选一个种子文件，发出它的 `file:` 地址（不是种子文件的由建任务时说明原因）。
    fn open_torrent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("打开".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = picked.await;
            // 选择期间视图被释放时不再需要结果
            let _gone = this.update(cx, |this, cx| {
                let path = match result {
                    Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.swap_remove(0),
                    Ok(Ok(_)) => return,
                    Ok(Err(error)) => return this.refuse(format!("打不开文件选择：{error}"), cx),
                    Err(_) => return this.refuse("文件选择没有返回结果".to_owned(), cx),
                };
                match Url::from_file_path(&path) {
                    Ok(url) => cx.emit(PasteEvent::Submit(url.to_string())),
                    Err(()) => this.refuse(format!("不是绝对路径：{}", path.display()), cx),
                }
            });
        })
        .detach();
    }

    /// 选保存目录：选了即成为默认保存目录（之后新建的都存到这里）；记不下时原因写在栏下面。
    fn change_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("存到这里".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = picked.await;
            // 选择期间视图被释放时不再需要结果
            let _gone = this.update(cx, |this, cx| {
                let problem = match result {
                    Ok(Ok(Some(mut paths))) if !paths.is_empty() => {
                        let folder = paths.swap_remove(0);
                        this.tasks
                            .update(cx, |tasks, cx| tasks.set_folder(folder, cx))
                            .err()
                            .map(|error| format!("保存目录没换成：{error}"))
                    }
                    Ok(Ok(_)) => None,
                    Ok(Err(error)) => Some(format!("打不开目录选择：{error}")),
                    Err(_) => Some("目录选择没有返回结果".to_owned()),
                };
                if let Some(problem) = problem {
                    this.refuse(problem, cx);
                }
            });
        })
        .detach();
    }
}

/// 多行文字的第一行，后面还有时加省略号。
fn first_line(text: &str) -> String {
    let mut lines = text.trim().lines();
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() {
        format!("{first}…")
    } else {
        first.to_owned()
    }
}

impl Render for PasteBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.typing(window, cx);
        let folder = format::path(self.tasks.read(cx).folder(), self.home.as_deref());
        let bar = div()
            .w_full()
            .h(px(if self.standby { STANDBY_HEIGHT } else { HEIGHT }))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .pl(px(if self.standby { 18. } else { 14. }))
            .pr(px(10.))
            .rounded(px(CONTROL_RADIUS))
            .bg(theme::wash())
            .border_1()
            // 输入时描边换成淡的主色
            .border_color(if focused {
                theme::alpha(theme::accent(), 0.5)
            } else {
                theme::rule()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(if self.standby { 14. } else { 13. }))
                    .child(Input::new(&self.input).appearance(false).on_paste({
                        let bar = cx.entity().downgrade();
                        let input = self.input.clone();
                        move |item, _, cx| {
                            let Some(text) = item
                                .text()
                                .filter(|text| !text.trim().is_empty())
                                .filter(|_| widgets::pastes_over(&input, cx))
                            else {
                                return false;
                            };
                            // 视图已释放时照常粘贴进框
                            bar.update(cx, |_, cx| cx.emit(PasteEvent::Submit(text)))
                                .is_ok()
                        }
                    })),
            )
            .child(
                Button::new("change-folder", folder, Kind::Text)
                    .size(Size::Small)
                    .max_w(px(180.))
                    .on_click(cx.listener(|this, _, window, cx| this.change_folder(window, cx))),
            )
            .child(
                Button::new("open-torrent", "种子文件", Kind::Text)
                    .size(Size::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.open_torrent(window, cx))),
            )
            .child(widgets::kbd(PASTE_KEY));
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(bar)
            .when(!self.problems.is_empty(), |column| {
                column.child(div().flex().flex_col().gap(px(2.)).px(px(4.)).children(
                    self.problems.iter().map(|problem| {
                        widgets::error_text(problem.clone())
                            .text_size(px(12.))
                            .truncate()
                    }),
                ))
            })
    }
}
