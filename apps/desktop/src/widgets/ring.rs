//! 圆环：列表行首的小环。

use std::f32::consts::{FRAC_PI_2, TAU};

use gpui_kit::{Bounds, Hsla, PathBuilder, Pixels, Point, Window, point, px};

/// 在 `bounds` 里贴着边画一段宽 `width` 的圆弧：从 `from` 起顺时针 `span`（都以整圈为 1，0 为 12 点）。`span` 为 0
/// 时不画，不小于 1 时为整圈。
pub fn arc(
    bounds: Bounds<Pixels>,
    width: f32,
    from: f32,
    span: f32,
    color: Hsla,
    window: &mut Window,
) {
    let span = span.min(1.);
    if span <= 0. {
        return;
    }
    let center = bounds.center();
    let side = f32::from(bounds.size.width.min(bounds.size.height));
    let radius = side / 2. - width / 2.;
    let steps = ((span * 96.).ceil() as usize).max(2);
    let mut path = PathBuilder::stroke(px(width));
    for step in 0..=steps {
        let at = on_circle(center, radius, from + step as f32 / steps as f32 * span);
        if step == 0 {
            path.move_to(at);
        } else {
            path.line_to(at);
        }
    }
    let path = path
        .build()
        .expect("一百来个顶点的描边，远低于曲面细分的上限");
    window.paint_path(path, color);
}

/// 圆周上整圈为 1 的 `turn` 处（0 为 12 点，顺时针）。
fn on_circle(center: Point<Pixels>, radius: f32, turn: f32) -> Point<Pixels> {
    let (sin, cos) = (turn * TAU - FRAC_PI_2).sin_cos();
    point(center.x + px(radius * cos), center.y + px(radius * sin))
}
