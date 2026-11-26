//! 左边的任务栏面板：标题栏写有几个任务，下面每个任务一个进度小环；新建时最上面是一个虚线环的「新任务」，在网页里
//! 找视频流时有网页那一项。

use std::time::Duration;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Context, Div, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Stateful,
    StatefulInteractiveElement as _, Styled as _, canvas, div, px,
};

use super::look::look;
use super::{Page, Shell};
use crate::new_task::Phase;
use crate::tasks::{Row, Tasks};
use crate::{motion, ring, theme, ui};
use hs_dlx_app::Status;

/// 任务栏的宽度。
const WIDTH: f32 = 288.;
/// 窗口窄时任务栏最多收到的宽度。
pub(super) const MIN_WIDTH: f32 = 240.;

/// 小环的边长与环宽。
const ORB: f32 = 46.;
const ORB_WIDTH: f32 = 5.;
/// 小环上的光点绕一圈要多久。
const ORBIT: Duration = Duration::from_millis(2800);

impl Shell {
    pub(super) fn render_list(&self, tasks: &Tasks, cx: &Context<Self>) -> impl IntoElement {
        let new_task = self
            .new_task()
            .map(|view| new_task_item(view.read(cx).phase()));
        let dim = self.new_task().is_some();
        let sniff = self.sniff.as_ref().map(|(view, _)| {
            let on = matches!(&self.page, Page::Sniff(shown) if shown == view);
            let read = view.read(cx);
            let (title, found) = (read.page_title(), read.found_count());
            sniff_item(title, found, on, dim, cx)
        });
        let empty = tasks.rows().is_empty() && new_task.is_none() && sniff.is_none();
        let selected = self.selected_row(tasks).map(|row| row.task.id);
        let orbs: Vec<_> = tasks
            .rows()
            .iter()
            .rev()
            .map(|row| self.render_orb(row, selected, dim, cx))
            .collect();
        ui::deck()
            .flex_basis(px(WIDTH))
            .flex_shrink(1.)
            .min_w(px(MIN_WIDTH))
            .p(px(8.))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .px(px(12.))
                    .pt(px(10.))
                    .pb(px(8.))
                    .text_size(px(13.))
                    .text_color(theme::muted())
                    .child("任务")
                    .child(ui::num(div().child(tasks.rows().len().to_string()))),
            )
            .child(
                div()
                    .id("tasks")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(new_task)
                    .children(sniff)
                    .children(orbs)
                    .when(empty, |list| list.children(empty_slots())),
            )
    }

    /// `selected` 为选中的任务（见 [`Shell::selected_row`]）。
    fn render_orb(
        &self,
        row: &Row,
        selected: Option<hs_dlx_app::TaskId>,
        dim: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let id = row.task.id;
        let look = look(row);
        let on = selected == Some(id) && matches!(self.page, Page::Task);
        let (fraction, color) = (look.fraction, look.orb);
        // 在下的任务：一颗光点沿小环绕行
        let orbit = (row.task.status == Status::Running)
            .then(|| motion::turn(ORBIT))
            .flatten();
        ui::list_item(("task", id.0 as u64), on)
            .when(dim, |s| s.opacity(0.5))
            .cursor_pointer()
            // 设置开着时点的：关上设置、看点的这一项
            .on_click(cx.listener(move |this, _, _, cx| {
                this.close_settings(cx);
                this.select(id, cx);
            }))
            .child(
                div()
                    .relative()
                    .size(px(ORB))
                    .flex_none()
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                ring::paint_arc(
                                    bounds,
                                    ORB_WIDTH,
                                    fraction,
                                    color,
                                    theme::line(),
                                    window,
                                );
                                if let Some(turn) = orbit {
                                    ring::paint_orbit(bounds, ORB_WIDTH, turn, color, window);
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    // 新建时其余任务只留环，中间空着，视线落在新建上
                    .when(!dim, |orb| {
                        orb.child(ui::num(
                            div()
                                .absolute()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(10.5))
                                .text_color(theme::soft())
                                .child(look.short),
                        ))
                    }),
            )
            .child(two_lines(
                row.task.spec.title.clone(),
                format!("{} · {}", look.state, look.line),
            ))
    }
}

/// 列表项右边的两行：标题与状态。
fn two_lines(title: String, state: String) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .min_w_0()
        .child(
            div()
                .text_size(px(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .truncate()
                .child(title),
        )
        .child(ui::num(
            div()
                .text_size(px(12.))
                .text_color(theme::muted())
                .truncate()
                .child(state),
        ))
}

/// 网页里的视频流那一项：两个同心实线环（与列表里每条流前面的一样），写网页的标题与找到了几条；`on` 为正显示着。
fn sniff_item(
    title: Option<String>,
    found: usize,
    on: bool,
    dim: bool,
    cx: &Context<Shell>,
) -> Stateful<Div> {
    ui::list_item("sniff", on)
        .when(dim, |item| item.opacity(0.5))
        .cursor_pointer()
        .on_click(cx.listener(|this, _, _, cx| {
            this.close_settings(cx);
            this.show_sniff(cx);
        }))
        .child(ui::rings(ORB, 3., 10.))
        .child(two_lines(
            title.unwrap_or_else(|| "网页里的视频流".to_owned()),
            if found == 0 {
                "还没找到视频流".to_owned()
            } else {
                format!("找到 {found} 条视频流")
            },
        ))
}

fn new_task_item(phase: Phase) -> impl IntoElement {
    ui::list_item("new-task", true)
        .child(
            div()
                .size(px(ORB))
                .flex_none()
                .rounded_full()
                .border(px(2.5))
                .border_dashed()
                .border_color(theme::accent()),
        )
        .child(two_lines("新任务".to_owned(), phase.label().to_owned()))
}

/// 还没有任务时：三个渐隐的空槽位（暗环与两道暗条，与任务一项同样的版式；虚线环留给「新任务」）与一句说明。
fn empty_slots() -> Vec<gpui_kit::AnyElement> {
    let slot = |opacity: f32, width: f32| {
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .p(px(10.))
            .opacity(opacity)
            .child(
                div()
                    .size(px(ORB))
                    .flex_none()
                    .rounded_full()
                    .border_2()
                    .border_color(theme::ring_idle()),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .h(px(9.))
                            .w(px(width))
                            .rounded(px(5.))
                            .bg(theme::selected()),
                    )
                    .child(
                        div()
                            .h(px(7.))
                            .w(px(width * 0.6))
                            .rounded(px(4.))
                            .bg(theme::raised()),
                    ),
            )
            .into_any_element()
    };
    vec![
        slot(0.9, 136.),
        slot(0.6, 112.),
        slot(0.35, 124.),
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(12.))
            .pt(px(14.))
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("还没有任务"),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .line_height(px(ui::TEXT_LINE))
                    .text_color(theme::muted())
                    .child("探测好、开始下载后列在这里；关窗时可以让它在后台接着下"),
            )
            .into_any_element(),
    ]
}
