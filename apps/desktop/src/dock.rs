//! 粘贴栏：舞台底部的链接、磁力链接或 curl 命令的输入。回车或点「探测」发出 [`DockEvent::Probe`]；点「种子文件」选一个
//! `.torrent` 文件，框里换成它的 `file:` 地址并发出。框里空着或全选时粘贴的 curl
//! 命令不经输入框（单行的框会删掉换行，续行就断了），交给 [`Dock::put`] 并立即发出。外面告诉它正在探测的链接
//! （[`Dock::set_probing`]），框里正是它时右边写探测到哪一步；外面拒绝了输入时右边写原因，改了内容即清掉。任务页、
//! 待机页与新建页的舞台里放的是同一个视图。

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AppContext as _, Context, EventEmitter, Focusable as _, IntoElement, ParentElement as _,
    PathPromptOptions, Render, SharedString, Styled as _, Subscription, Window, div, px,
};

use hs_dlx_app::curl;
use hs_m3u8_core::Url;

use crate::links;
use crate::theme;
use crate::ui::DOCK_HEIGHT;
use crate::widgets::{self, Button, CONTROL_RADIUS, Kind, Size};

/// 胶囊的最大宽。
const MAX_WIDTH: f32 = 760.;

pub enum DockEvent {
    /// 框里的链接或命令；框里是记下的 curl 命令的地址时为整条命令（原样，含换行）
    Probe(String),
}

pub struct Dock {
    input: gpui_kit::Entity<InputState>,
    /// 外面拒绝这次输入的原因；改了内容时清掉
    refused: Option<String>,
    /// 显示着的新建页：探测的正是框里的链接时，右边写它探测到哪一步
    probing: Option<(Url, &'static str)>,
    /// 放进来的 curl 命令：框里显示的它的地址与整条命令；改了框里的内容即清掉
    command: Option<(SharedString, String)>,
    _typed: Subscription,
}

impl EventEmitter<DockEvent> for Dock {}

impl Dock {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("粘贴链接、磁力链接或 curl 命令，回车探测")
        });
        let typed = cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter { .. } => cx.emit(DockEvent::Probe(this.submission(cx))),
                InputEvent::Change => {
                    this.refused = None;
                    this.command = None;
                    cx.notify();
                }
                InputEvent::Focus | InputEvent::Blur => cx.notify(),
            },
        );
        Dock {
            input,
            refused: None,
            probing: None,
            command: None,
            _typed: typed,
        }
    }

    /// 回车或点「探测」时发出的：框里是记下的 curl 命令的地址时为整条命令，否则为框里的内容。
    fn submission(&self, cx: &gpui_kit::App) -> String {
        let value = self.text(cx);
        match &self.command {
            Some((shown, command)) if *shown == value => command.clone(),
            _ => value.to_string(),
        }
    }

    /// 把 `text` 放进框里。curl 命令换成它的地址（命令里的凭据不明文显示）并记下整条命令；认不出的命令不放，框里清空、
    /// 右边写原因。别的照原样放。
    pub fn put(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !curl::is_command(text) {
            self.set_text(text.to_owned(), window, cx);
            return;
        }
        match curl::parse(text) {
            Ok(request) => {
                let shown = SharedString::from(request.url.to_string());
                self.set_text(shown.to_string(), window, cx);
                self.command = Some((shown, text.to_owned()));
            }
            Err(error) => {
                self.set_text(String::new(), window, cx);
                self.refuse(error.to_string(), cx);
            }
        }
    }

    pub fn text(&self, cx: &gpui_kit::App) -> SharedString {
        self.input.read(cx).value()
    }

    pub fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.command = None;
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 键盘焦点在探测框里（用户可能正在输入）。
    pub fn typing(&self, window: &Window, cx: &gpui_kit::App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    /// 选一个种子文件：选了 `.torrent` 结尾的，框里换成它的 `file:` 地址并发出 [`DockEvent::Probe`]；别的在右边写原因。
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
            let _gone = this.update_in(cx, |this, window, cx| {
                let path = match result {
                    Ok(Ok(Some(mut paths))) if !paths.is_empty() => paths.swap_remove(0),
                    Ok(Ok(_)) => return,
                    Ok(Err(error)) => return this.refuse(format!("打不开文件选择：{error}"), cx),
                    Err(_) => return this.refuse("文件选择没有返回结果".to_owned(), cx),
                };
                let url = Url::from_file_path(&path)
                    .ok()
                    .and_then(|url| links::torrent_url(url.as_str()));
                match url {
                    Some(url) => {
                        let text = url.to_string();
                        this.set_text(text.clone(), window, cx);
                        cx.emit(DockEvent::Probe(text));
                    }
                    None => this.refuse("要选 .torrent 结尾的种子文件".to_owned(), cx),
                }
            });
        })
        .detach();
    }

    /// 外面拒绝了这次输入：右边写 `reason`，改了内容时清掉。
    pub fn refuse(&mut self, reason: String, cx: &mut Context<Self>) {
        self.refused = Some(reason);
        cx.notify();
    }

    pub fn accepted(&mut self, cx: &mut Context<Self>) {
        self.refused = None;
        cx.notify();
    }

    /// 正在探测的链接与探测到哪一步（显示着的新建页报的；没有新建页时为 None）：框里正是这个链接时右边写这一步。
    pub fn set_probing(&mut self, probing: Option<(Url, &'static str)>, cx: &mut Context<Self>) {
        if self.probing != probing {
            self.probing = probing;
            cx.notify();
        }
    }
}

impl Render for Dock {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let value = self.input.read(cx).value();
        let focused = self.typing(window, cx);
        let typed = links::target(&value);
        let probing = self
            .probing
            .as_ref()
            .filter(|(url, _)| typed.as_ref() == Some(url))
            .map(|(_, phase)| *phase);
        let trailing = if let Some(reason) = &self.refused {
            widgets::error_text(reason.clone())
                .pr(px(14.))
                .into_any_element()
        } else if let Some(phase) = probing {
            div()
                .pr(px(14.))
                .text_size(px(13.))
                .text_color(theme::ink_3())
                .child(phase)
                .into_any_element()
        } else {
            // 空着时按了也没东西可探测，不抢眼
            let kind = if value.trim().is_empty() {
                Kind::Secondary
            } else {
                Kind::Primary
            };
            Button::new("probe", "探测", kind)
                .on_click(cx.listener(|this, _, _, cx| {
                    let text = this.submission(cx);
                    cx.emit(DockEvent::Probe(text));
                }))
                .into_any_element()
        };
        div()
            .w_full()
            .max_w(px(MAX_WIDTH))
            .h(px(DOCK_HEIGHT))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.))
            .pl(px(18.))
            .pr(px(8.))
            .rounded(px(CONTROL_RADIUS))
            .bg(theme::wash())
            .border_1()
            // 输入时描边换成淡的主色
            .border_color(if focused {
                theme::alpha(theme::accent(), 0.5)
            } else {
                theme::rule()
            })
            .child(div().flex_1().min_w_0().text_size(px(14.)).child(
                Input::new(&self.input).appearance(false).on_paste({
                    let dock = cx.entity().downgrade();
                    let input = self.input.clone();
                    move |item, window, cx| {
                        let Some(text) = item.text().filter(|text| {
                            curl::is_command(text) && widgets::pastes_over(&input, cx)
                        }) else {
                            return false;
                        };
                        // 视图已释放时照常粘贴
                        dock.update(cx, |dock, cx| {
                            dock.put(&text, window, cx);
                            if dock.refused.is_none() {
                                cx.emit(DockEvent::Probe(text));
                            }
                        })
                        .is_ok()
                    }
                }),
            ))
            .child(
                Button::new("open-torrent", "种子文件", Kind::Text)
                    .size(Size::Small)
                    .on_click(cx.listener(|this, _, window, cx| this.open_torrent(window, cx))),
            )
            .child(trailing)
    }
}
