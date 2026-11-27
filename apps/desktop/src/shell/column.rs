//! 有任务时的主体：左边一栏（粘贴栏、剪贴板提示、回到新建页与嗅探页的两项、分组的任务列表），右边是选中任务的舞台、
//! 或设置页、新建页、嗅探页。

use std::time::Instant;

use gpui_kit::{
    AnyElement, Context, Div, FontWeight, IntoElement, ParentElement as _, Stateful,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, div, px,
};

use super::{COLUMN_GAP, COLUMN_LEFT, COLUMN_TOP, Clip, Page, Shell};
use crate::new_task::Phase;
use crate::theme;
use crate::ui::{self, PAD};
use crate::widgets::{self, Button, Kind, Size};

/// 回到新建页、嗅探页那两项的小环边长。
const ORB: f32 = 30.;

impl Shell {
    /// 主体：`column` 为列表一栏的宽。网页显示着时嗅探页占满主体，不显示列表一栏。
    pub(super) fn render_body(&self, column: f32, now: Instant, cx: &Context<Self>) -> Div {
        let page_shown = self
            .sniff_on_screen()
            .is_some_and(|sniff| sniff.read(cx).page_shown());
        let mut body = div()
            .flex_1()
            .min_h_0()
            .flex()
            .gap(px(COLUMN_GAP))
            .pl(px(COLUMN_LEFT))
            .pr(px(PAD))
            .pt(px(COLUMN_TOP))
            .pb(px(PAD));
        if !page_shown {
            body = body.child(self.render_column(column, cx));
        }
        let tasks = self.tasks.read(cx);
        match (&self.settings, &self.page) {
            (Some((view, _)), _) => body.child(view.clone()),
            (None, Page::NewTask { view, .. }) => body.child(view.clone()),
            (None, Page::Sniff(view)) => body.child(view.clone()),
            (None, Page::Task) => match self.selected_row(tasks, cx) {
                Some(row) => {
                    let (stage, inspector) = self.render_task(row, now, cx);
                    body.child(stage).child(inspector)
                }
                None => body,
            },
        }
    }

    /// 列表一栏，宽 `width`。
    fn render_column(&self, width: f32, cx: &Context<Self>) -> Div {
        // 设置开着时盖住了主体的页：哪一项都不算正显示着
        let shown = self.settings.is_none();
        let new_task = self
            .new_task()
            .map(|view| new_task_item(view.read(cx).phase(), shown, cx));
        let sniff = self.sniff.as_ref().map(|(view, _)| {
            let on = self.sniff_on_screen().as_ref() == Some(view);
            let read = view.read(cx);
            sniff_item(read.page_title(), read.found_count(), on, cx)
        });
        div()
            .w(px(width))
            .flex_none()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(self.paste.clone())
            .children(self.clipboard.as_ref().map(|clip| clipboard_hint(clip, cx)))
            .children(new_task)
            .children(sniff)
            // 撑满一栏剩下的高
            .child(
                self.list
                    .clone()
                    .cached(StyleRefinement::default().flex_1().w_full().min_h_0()),
            )
    }
}

/// 剪贴板里有一条还不是任务的链接：「剪贴板里有一条链接 … 用它 ↵」（粘贴栏空着时回车即用它）。
pub(super) fn clipboard_hint(clip: &Clip, cx: &Context<Shell>) -> AnyElement {
    let shown = match clip.url.scheme() {
        "magnet" => clip
            .url
            .query_pairs()
            .find(|(name, _)| name == "dn")
            .map_or_else(|| "磁力链接".to_owned(), |(_, name)| name.into_owned()),
        _ => format!(
            "{}{}",
            clip.url.host_str().unwrap_or_default(),
            clip.url.path()
        ),
    };
    div()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(10.))
        .px(px(4.))
        .text_size(px(12.))
        .child(
            div()
                .flex_none()
                .text_color(theme::ink_3())
                .child("剪贴板里有一条链接"),
        )
        .child(
            widgets::mono(div())
                .min_w_0()
                .truncate()
                .text_color(theme::ink_2())
                .child(shown),
        )
        .child(
            Button::new("use-clipboard", "用它", Kind::Secondary)
                .size(Size::Small)
                .detail("↵")
                .on_click(cx.listener(|this, _, window, cx| this.use_clipboard(window, cx))),
        )
        .into_any_element()
}

/// 回到新建页那一项：`on` 为正显示着（设置开着时没有）；点它关上设置、露出新建页。
fn new_task_item(phase: Phase, on: bool, cx: &Context<Shell>) -> impl IntoElement {
    ui::list_item("new-task", on)
        .cursor_pointer()
        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
        .child(
            div()
                .size(px(ORB))
                .flex_none()
                .rounded_full()
                .border(px(2.))
                .border_dashed()
                .border_color(theme::accent()),
        )
        .child(two_lines("新任务".to_owned(), phase.label().to_owned()))
}

/// 回到嗅探页那一项：写网页的标题与找到了几条；`on` 为正显示着。
fn sniff_item(title: Option<String>, found: usize, on: bool, cx: &Context<Shell>) -> Stateful<Div> {
    ui::list_item("sniff", on)
        .cursor_pointer()
        .on_click(cx.listener(|this, _, _, cx| {
            this.show_sniff(cx);
            this.close_settings(cx);
        }))
        .child(ui::rings(ORB, 2.5, 8.))
        .child(two_lines(
            title.unwrap_or_else(|| "网页里的视频流".to_owned()),
            if found == 0 {
                "还没找到视频流".to_owned()
            } else {
                format!("找到 {found} 条视频流")
            },
        ))
}

/// 一项右边的两行：名字与状态。
fn two_lines(title: String, state: String) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .min_w_0()
        .child(
            div()
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .truncate()
                .child(title),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(theme::ink_2())
                .truncate()
                .child(state),
        )
}
