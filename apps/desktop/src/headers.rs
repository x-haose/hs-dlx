//! 请求头的编辑：已有的一条一条列出、可以删，输入框里写「名称: 值」回车添加。添加时按核心的规则校验
//! （[`check_option_header`]），同名的（不分大小写）替换原有的一条。输入框里写好 curl 命令回车、或在空着（全选）的
//! 输入框里粘贴 curl 命令时，加入命令转成的请求头（见 [`curl::parse`]），命令里没带上的列在输入框下面；粘贴的命令
//! 不经单行的输入框（换行会被删掉）。

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
    div, px,
};
use hs_dlx_app::curl;
use hs_m3u8_core::{Url, check_option_header};

use crate::theme;
use crate::ui::{self, TEXT_LINE};

pub struct HeadersEditor {
    /// 这些请求头用于哪个地址：加入的 curl 命令要与它同源（见 [`curl::Request::check_target`]）
    target: Url,
    headers: Vec<(String, String)>,
    input: Entity<InputState>,
    /// 输入框里的内容加不进去的原因；改了内容时清掉
    error: Option<String>,
    /// 最近一次添加的 curl 命令里没带上的请求头名，列在输入框下面；再添加（不论成败）时换掉
    dropped: Vec<String>,
    _typed: Subscription,
}

impl HeadersEditor {
    /// 编辑发往 `target` 的请求头 `headers`；`dropped` 为它们出自的 curl 命令里没带上的请求头名。
    pub fn new(
        target: Url,
        headers: Vec<(String, String)>,
        dropped: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("名称: 值，回车添加；可粘贴 curl 命令")
        });
        let typed = cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    if this.add(&text) {
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    cx.notify();
                }
                InputEvent::Change => {
                    this.error = None;
                    cx.notify();
                }
                InputEvent::Focus | InputEvent::Blur => {}
            },
        );
        HeadersEditor {
            target,
            headers,
            input,
            error: None,
            dropped,
            _typed: typed,
        }
    }

    /// 让输入框获得焦点。
    /// 已加入的请求头（输入框里还没加入的不算）。
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// 编好的请求头，连同输入框里还没按回车的一条；那一条加不进去时为 None（原因显示在输入框下面）。
    pub fn confirm(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Vec<(String, String)>> {
        let pending = self.input.read(cx).value().to_string();
        // 上一次没加进去的已不在输入框里，原因不再适用
        self.error = None;
        if !pending.trim().is_empty() {
            if !self.add(&pending) {
                cx.notify();
                return None;
            }
            self.input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        Some(self.headers.clone())
    }

    /// 加一条「名称: 值」，或 curl 命令里的全部请求头；加不进去时记下原因，返回 false。
    fn add(&mut self, text: &str) -> bool {
        self.dropped = Vec::new();
        let added = if curl::is_command(text) {
            self.add_command(text)
        } else {
            parse(text).map(|header| merge(&mut self.headers, header))
        };
        self.error = added.err();
        self.error.is_none()
    }

    /// 加入 curl 命令 `text` 里的请求头；命令认不出、或与 [`HeadersEditor::target`] 不同源时一条也不加。
    fn add_command(&mut self, text: &str) -> Result<(), String> {
        let request = curl::parse(text).map_err(|error| error.to_string())?;
        request
            .check_target(&self.target)
            .map_err(|error| error.to_string())?;
        for header in request.headers {
            merge(&mut self.headers, header);
        }
        self.dropped = request.dropped;
        Ok(())
    }
}

/// 输入框下面对 curl 命令里没带上的请求头的说明；都带上了时为 None。
fn dropped_note(dropped: &[String]) -> Option<String> {
    (!dropped.is_empty()).then(|| format!("没带上命令里的 {}：由下载器决定", dropped.join("、")))
}

/// 把 `header` 加到 `headers` 末尾，同名的（不分大小写）先去掉。
fn merge(headers: &mut Vec<(String, String)>, header: (String, String)) {
    headers.retain(|(old, _)| !old.eq_ignore_ascii_case(&header.0));
    headers.push(header);
}

/// 名字里带这些词（不分大小写）的请求头当作凭据，列出时遮住：共享屏幕或截图时不露出来。
const SECRET: [&str; 6] = ["cookie", "auth", "token", "key", "secret", "session"];

/// 列出时的值：凭据只露出长度。
fn shown(name: &str, value: &str) -> String {
    let name = name.to_ascii_lowercase();
    if SECRET.iter().any(|word| name.contains(word)) {
        format!("已填 {} 个字符", value.chars().count())
    } else {
        value.to_owned()
    }
}

/// 把「名称: 值」拆开并校验；名称与值两边的空白去掉。
fn parse(text: &str) -> Result<(String, String), String> {
    let (name, value) = text
        .split_once(':')
        .ok_or_else(|| "要写成「名称: 值」".to_owned())?;
    let (name, value) = (name.trim(), value.trim());
    check_option_header(name, value)?;
    Ok((name.to_owned(), value.to_owned()))
}

impl Render for HeadersEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.headers.iter().enumerate().map(|(i, (name, value))| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(12.))
                .py(px(6.))
                .rounded(px(10.))
                .bg(theme::cell())
                .border_1()
                .border_color(theme::deck_edge())
                .text_size(px(12.))
                .line_height(px(TEXT_LINE))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(name.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme::muted())
                        .child(shown(name, value)),
                )
                .child(
                    div()
                        .id(("remove-header", i))
                        .flex_none()
                        .px(px(4.))
                        .text_color(theme::muted())
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme::text()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.headers.remove(i);
                            cx.notify();
                        }))
                        .child("×"),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .children(rows)
            .child(
                ui::input_frame().child(Input::new(&self.input).appearance(false).on_paste({
                    let editor = cx.entity().downgrade();
                    let input = self.input.clone();
                    move |item, window, cx| {
                        let Some(text) = item
                            .text()
                            .filter(|text| curl::is_command(text) && ui::pastes_over(&input, cx))
                        else {
                            return false;
                        };
                        // 视图已释放时照常粘贴
                        editor
                            .update(cx, |editor, cx| {
                                // 加进去了就清掉框里被这次粘贴选中替换的内容
                                if editor.add(&text) {
                                    editor
                                        .input
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                }
                                cx.notify();
                            })
                            .is_ok()
                    }
                })),
            )
            .children(self.error.clone().map(ui::error_text))
            .children(dropped_note(&self.dropped).map(ui::caption))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_split_at_the_first_colon_and_checked() {
        assert_eq!(
            parse(" Referer : https://a.example/x "),
            Ok(("Referer".into(), "https://a.example/x".into()))
        );
        assert!(parse("Referer https://a.example/").is_err());
        assert!(parse("Bad Name: x").is_err());
    }

    /// 同名的（不分大小写）换掉原有的一条，排到最后；凭据只露出长度。
    #[test]
    fn same_names_replace_and_secrets_are_hidden() {
        let mut headers = vec![("Referer".into(), "a".into()), ("X-Id".into(), "1".into())];
        merge(&mut headers, ("referer".into(), "b".into()));
        assert_eq!(
            headers,
            [
                ("X-Id".to_owned(), "1".to_owned()),
                ("referer".to_owned(), "b".to_owned())
            ]
        );
        assert_eq!(shown("COOKIE", "sid=12345"), "已填 9 个字符");
        assert_eq!(shown("X-Api-Key", "abc"), "已填 3 个字符");
        assert_eq!(shown("Referer", "https://a/"), "https://a/");
    }
}
