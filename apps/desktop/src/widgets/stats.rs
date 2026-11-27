//! 读数：一排标签与数，之间以竖线分开；速度的一格下面带最近 60 秒的折线。

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Div, IntoElement, ParentElement as _, PathBuilder, SharedString, Styled as _, canvas, div,
    point, px,
};

use crate::theme;

/// 读数里的一格。
pub struct Stat {
    label: SharedString,
    value: SharedString,
    /// 读数是一段文字（保存位置、代理）：字小一号，放不下时截断
    plain: bool,
    /// 最近一段时间的速度（字节每秒，旧的在前）
    history: Option<Vec<f64>>,
}

impl Stat {
    /// 数：20 号字。
    pub fn new(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Stat {
        Stat {
            label: label.into(),
            value: value.into(),
            plain: false,
            history: None,
        }
    }

    /// 一段文字（保存位置）：15 号字。
    pub fn text(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Stat {
        Stat {
            plain: true,
            ..Stat::new(label, value)
        }
    }

    /// 下面画 `history` 的折线。
    pub fn history(mut self, history: Vec<f64>) -> Stat {
        self.history = Some(history);
        self
    }
}

/// 读数，每行 `columns` 格（舞台上一行放下全部，窄的地方两格一行）；每行第一格左边不画竖线。
pub fn stats(cells: Vec<Stat>, columns: usize) -> Div {
    let columns = columns.max(1);
    div()
        .grid()
        .grid_cols(columns as u16)
        .gap_y(px(16.))
        .children(cells.into_iter().enumerate().map(|(index, stat)| {
            let first = index % columns == 0;
            div()
                .min_w_0()
                .pr(px(20.))
                .when(!first, |cell| {
                    cell.pl(px(20.)).border_l_1().border_color(theme::rule())
                })
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::ink_3())
                        .child(stat.label),
                )
                .child(value(stat.value, stat.plain))
                .children(stat.history.map(spark))
        }))
}

/// 读数：数 20 号；文字 15 号，与数同高。放不下时截断。
fn value(value: SharedString, plain: bool) -> Div {
    div()
        .mt(px(4.))
        .h(px(26.))
        .flex()
        .items_center()
        .text_size(px(if plain { 15. } else { 20. }))
        .child(div().min_w_0().truncate().child(value))
}

/// 折线的高。
const SPARK_HEIGHT: f32 = 26.;

/// 速度的折线：按其中最大的一格归一，最大的一格贴着上边；全为 0 时是贴着下边的一条直线。
fn spark(history: Vec<f64>) -> impl IntoElement {
    let peak = history.iter().copied().fold(0., f64::max);
    let color = theme::accent();
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            if history.len() < 2 {
                return;
            }
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            // 线宽 1.4：上下各留半个线宽，峰值与 0 都不被裁掉
            let span = height - 1.4;
            let step = width / (history.len() - 1) as f32;
            let mut path = PathBuilder::stroke(px(1.4));
            for (index, speed) in history.iter().enumerate() {
                let share = if peak > 0. { (speed / peak) as f32 } else { 0. };
                let at = point(
                    bounds.origin.x + px(index as f32 * step),
                    bounds.origin.y + px(0.7 + span * (1. - share)),
                );
                if index == 0 {
                    path.move_to(at);
                } else {
                    path.line_to(at);
                }
            }
            window.paint_path(
                path.build().expect("折线只有直线段，宽高都是有限的数"),
                color,
            );
        },
    )
    .mt(px(8.))
    .w_full()
    .h(px(SPARK_HEIGHT))
}
