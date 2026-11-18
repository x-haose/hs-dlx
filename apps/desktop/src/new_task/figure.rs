//! 新建页舞台中间的图形：探测中（与取种子信息时）与失败时是表盘，认出播放列表后是各档清晰度的同心环（几何、点击判定
//! 与画法），认出文件后是写着大小的表盘，取到种子信息后是写着所选大小的表盘。

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AbsoluteLength, AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Pixels, Point, StatefulInteractiveElement as _, Styled,
    canvas, div, px,
};
use hs_dlx_app::Failure;
use hs_m3u8_core::{FileInfo, Probe, VariantInfo};

use super::{NewTask, PageCheck, Probing, TorrentChoice, duration_of};
use crate::backdrop::{self, Disc};
use crate::ring::{self, Ticks};
use crate::{dial, format, theme, ui};

/// 同心环所在方框的边长。
const BOX: f32 = 480.;
/// 方框在整块区域里的上下边距。
const BOX_TOP: f32 = 20.;
/// 最外一环的半径（环宽的中线）。
const OUTER: f32 = 222.;
/// 最内一环的半径。
const INNER: f32 = 102.;
/// 只有一档（来源本身是媒体播放列表）时那一环的半径。
const SINGLE_RADIUS: f32 = 174.;
/// 只有一档时那一环的宽。
const SINGLE_WIDTH: f32 = 14.;
/// 圆心处写字的方框边长。
const CENTER_BOX: f32 = 180.;
/// 标签所在的横坐标（相对同心环区域）：引线从环上 45° 处水平拉到这里。
const LABEL_X: f32 = 496.;
/// 标签一列的宽：圆点加一行 12 号字的清晰度与码率（如「1080p · 4.2 Mbps」，不换行）。
const LABEL_WIDTH: f32 = 184.;
/// 同心环区域（连同右侧的标签）的宽。
pub(super) const FIGURE_WIDTH: f32 = LABEL_X + LABEL_WIDTH;
/// 同心环区域的高。
pub(super) const FIGURE_HEIGHT: f32 = BOX_TOP + BOX + BOX_TOP;
/// 同心环外面的日冕的强度（表盘的为 1）。
const RINGS_GLOW: f32 = 0.2;
/// 点在环宽之外多远以内仍算点中这一环。
const HIT_SLOP: f32 = 8.;

/// 一档清晰度的环：变体下标、半径（环宽的中线）与环宽。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Ring {
    pub(super) index: usize,
    radius: f32,
    width: f32,
}

/// 由外向内的各环：码率从高到低，最外一环半径 [`OUTER`]、最内不小于 [`INNER`]、相邻两环的半径差至多 48。环宽按码率
/// 在 4 到 26 之间，且与相邻环留出 8 的缝；档数多到半径差不足 11 时环宽压到 3，缝随之变窄。
pub(super) fn rings(variants: &[VariantInfo]) -> Vec<Ring> {
    let rates: Vec<(usize, Option<u64>)> =
        variants.iter().map(|v| (v.index, v.bandwidth)).collect();
    rings_of(&rates)
}

/// [`rings`] 的计算：`rates` 为各档的（变体下标, 码率）。
fn rings_of(rates: &[(usize, Option<u64>)]) -> Vec<Ring> {
    let mut order = rates.to_vec();
    order.sort_by_key(|&(_, bandwidth)| std::cmp::Reverse(bandwidth.unwrap_or(0)));
    let top = order.first().and_then(|&(_, bw)| bw).unwrap_or(1).max(1) as f32;
    let step = match order.len() {
        0 | 1 => 0.0,
        n => (48.0_f32).min((OUTER - INNER) / (n - 1) as f32),
    };
    order
        .iter()
        .enumerate()
        .map(|(i, &(index, bandwidth))| {
            let width = 4.0 + 22.0 * bandwidth.unwrap_or(0) as f32 / top;
            let room = if step > 0.0 { step - 8.0 } else { 26.0 };
            Ring {
                index,
                radius: OUTER - step * i as f32,
                width: width.min(room).max(3.0),
            }
        })
        .collect()
}

/// 离圆心 `distance` 的点落在哪一环上：在环宽之外 [`HIT_SLOP`] 以内的环里离得最近的那个；都不在时为 None。
fn hit(rings: &[Ring], distance: f32) -> Option<usize> {
    rings
        .iter()
        .map(|ring| {
            (
                ring.index,
                (distance - ring.radius).abs() - ring.width / 2.0,
            )
        })
        .filter(|(_, gap)| *gap <= HIT_SLOP)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}

/// 设任意宽度的四边边框（GPUI 只给了 0 到 8 的整数档）。
fn border_width<E: Styled>(mut element: E, width: f32) -> E {
    let width: AbsoluteLength = px(width).into();
    let widths = &mut element.style().border_widths;
    widths.top = Some(width);
    widths.right = Some(width);
    widths.bottom = Some(width);
    widths.left = Some(width);
    element
}

/// 一个环：半径 `radius`（环宽中线）、宽 `width`，画在 [`BOX`] 见方区域的中心。
fn ring_div(radius: f32, width: f32) -> Div {
    let size = 2. * radius + width;
    let offset = BOX / 2. - size / 2.;
    border_width(
        div()
            .absolute()
            .left(px(offset))
            .top(px(offset))
            .size(px(size))
            .rounded_full(),
        width,
    )
}

/// 选中的环：主色的环，外加两圈更宽更淡的同心环作光晕。阴影会透过透明的环心铺满整个圆，所以光晕用环画。
fn lit_ring(radius: f32, width: f32) -> Vec<Div> {
    vec![
        ring_div(radius, width + 18.).border_color(theme::alpha(theme::accent(), 0.06)),
        ring_div(radius, width + 8.).border_color(theme::alpha(theme::accent(), 0.14)),
        ring_div(radius, width).border_color(theme::accent()),
    ]
}

impl NewTask {
    /// 窗口坐标 `position` 落在哪一环上（见 [`hit`]）。
    fn ring_at(&self, position: Point<Pixels>) -> Option<usize> {
        let Probing::Ready(probe) = &self.probing else {
            return None;
        };
        let master = probe.master.as_ref()?;
        let center = self.rings_bounds.get().center();
        let (dx, dy) = (
            f32::from(position.x - center.x),
            f32::from(position.y - center.y),
        );
        hit(&rings(&master.variants), (dx * dx + dy * dy).sqrt())
    }

    /// 舞台中间的图形：探测中（与取到的不是播放列表、正在网页里看时，取种子信息时）是转着彗尾的表盘，失败时是警告色的
    /// 表盘，认出播放列表后是同心环，认出文件后是写着大小的表盘，取到种子信息后是写着所选大小的表盘。
    pub(super) fn render_figure(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.probing {
            Probing::Ready(probe) => self.rings_figure(probe, cx).into_any_element(),
            Probing::File(info) => file_dial(info).into_any_element(),
            Probing::Torrent(choice) => torrent_dial(choice).into_any_element(),
            Probing::Web(choice) => super::web::web_dial(choice).into_any_element(),
            Probing::Resolving => self
                .probing_dial("解析中", "yt-dlp 在解析网页".to_owned(), cx)
                .into_any_element(),
            Probing::NeedsWeb(_) => self
                .probing_dial("网页视频", "要先装网页视频的组件".to_owned(), cx)
                .into_any_element(),
            Probing::Failed(failure) if self.page_check != PageCheck::Opening => {
                failed_dial(failure).into_any_element()
            }
            Probing::Metadata(progress) => {
                let doing = match (self.url.scheme(), progress.peers) {
                    ("magnet", 0) => "找有这个种子的节点".to_owned(),
                    ("magnet", peers) => format!("连着 {peers} 个节点"),
                    _ => "读种子文件".to_owned(),
                };
                self.probing_dial("取种子信息", doing, cx)
                    .into_any_element()
            }
            Probing::Running | Probing::Failed(_) => {
                let doing = if self.page_check == PageCheck::Opening {
                    "在网页里打开看看"
                } else {
                    "读取播放列表"
                };
                self.probing_dial("探测中", doing.to_owned(), cx)
                    .into_any_element()
            }
        }
    }

    /// 探测中的表盘：彗尾按探测了多久转，圆盘里写探测了多久（逐帧重画见 [`NewTask::animate`]），上面写 `state`、下面
    /// 写在做的 `doing`。系统要求减少动态效果时彗尾停在 12 点、不写时长。
    fn probing_dial(&self, state: &'static str, doing: String, cx: &Context<Self>) -> Div {
        let animate = !cx.reduce_motion();
        let elapsed = self.since.elapsed();
        let head = if animate {
            dial::head(elapsed, dial::TURN)
        } else {
            0.
        };
        dial::dial(
            theme::accent(),
            dial::figure(move |bounds, window, _| {
                ring::paint_comet(bounds, ring::RADIUS, head, theme::accent(), window)
            }),
            dial::readout(
                state,
                theme::accent(),
                dial::Big::Figure(if animate {
                    format::clock(elapsed)
                } else {
                    "…".to_owned()
                }),
                doing,
            ),
        )
    }

    /// 探测完成时的同心环与右侧的标签，点环选中一档。
    fn rings_figure(&self, probe: &Probe, cx: &mut Context<Self>) -> Div {
        let Figure {
            rings,
            labels,
            center,
            note,
            edge,
        } = self.ready_figure(probe);
        let bounds_cell = self.rings_bounds.clone();
        div()
            .relative()
            .w(px(FIGURE_WIDTH))
            .h(px(FIGURE_HEIGHT))
            .flex_none()
            .child(
                div()
                    .id("rings")
                    .absolute()
                    .left_0()
                    .top(px(BOX_TOP))
                    .size(px(BOX))
                    .when(self.hovered.is_some(), |s| s.cursor_pointer())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &gpui_kit::MouseDownEvent, _, cx| {
                            if let Some(index) = this.ring_at(event.position) {
                                this.select_variant(index, cx);
                            }
                        }),
                    )
                    .on_mouse_move(
                        cx.listener(|this, event: &gpui_kit::MouseMoveEvent, _, cx| {
                            let hovered = this.ring_at(event.position);
                            if hovered != this.hovered {
                                this.hovered = hovered;
                                cx.notify();
                            }
                        }),
                    )
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered && this.hovered.is_some() {
                            this.hovered = None;
                            cx.notify();
                        }
                    }))
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                bounds_cell.set(bounds);
                                // 日冕弱一档：环与右侧的标签是这一页的主角
                                backdrop::report_disc(
                                    Disc {
                                        center: bounds.center(),
                                        radius: edge,
                                        strength: RINGS_GLOW,
                                    },
                                    cx,
                                );
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(rings)
                    .child(
                        div()
                            .absolute()
                            .left(px(BOX / 2. - CENTER_BOX / 2.))
                            .top(px(BOX / 2. - CENTER_BOX / 2.))
                            .size(px(CENTER_BOX))
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(4.))
                            .child(ui::num(
                                div()
                                    .text_size(px(34.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(center),
                            ))
                            .child(ui::num(
                                div()
                                    .text_size(px(13.))
                                    .text_color(theme::muted())
                                    .child(note),
                            )),
                    )
                    .children(labels),
            )
    }

    /// 探测完成后舞台里同心环下面的一行：按键在界面上看不出来。
    pub(super) fn keys_hint(&self) -> Option<AnyElement> {
        let probe = match &self.probing {
            Probing::Ready(probe) => probe,
            Probing::File(_) | Probing::Torrent(_) => {
                return Some(ui::caption("回车开始").into_any_element());
            }
            Probing::Web(choice) => {
                let text = if choice.resolved.choices.len() > 1 {
                    "↑ ↓ 换一档 · 回车开始"
                } else {
                    "回车开始"
                };
                return Some(ui::caption(text).into_any_element());
            }
            Probing::Running
            | Probing::Failed(_)
            | Probing::Metadata(_)
            | Probing::Resolving
            | Probing::NeedsWeb(_) => return None,
        };
        let text = if probe.master.as_ref().is_some_and(|m| m.variants.len() > 1) {
            "↑ ↓ 换一档 · 回车开始"
        } else {
            "回车开始"
        };
        Some(ui::caption(text).into_any_element())
    }

    /// 探测完成时的同心环：圆心写选中一档的清晰度与大小上限，只有一档时写时长与片数。
    fn ready_figure(&self, probe: &Probe) -> Figure {
        let duration = duration_of(probe);
        let Some(master) = &probe.master else {
            // 来源本身是媒体播放列表：只有一档，没有可选的。直播的时长与片数是当前窗口的，不写
            let segments = probe.tracks.first().map_or(0, |t| t.segments);
            let (center, note) = if probe.live {
                ("直播".to_owned(), "开始后一直录到停止".to_owned())
            } else {
                (format::clock(duration), format!("{segments} 个分片"))
            };
            return Figure {
                rings: lit_ring(SINGLE_RADIUS, SINGLE_WIDTH),
                labels: Vec::new(),
                center,
                note,
                edge: SINGLE_RADIUS + SINGLE_WIDTH / 2.,
            };
        };
        let rings = rings(&master.variants);
        // 相邻标签的竖向间距不够 16 时只给选中与悬停的环写标签
        let crowded = rings
            .windows(2)
            .any(|pair| (pair[0].radius - pair[1].radius) * std::f32::consts::FRAC_1_SQRT_2 < 16.);
        let c = BOX / 2.;
        let mut ring_divs = Vec::new();
        let mut labels = Vec::new();
        for ring in &rings {
            let on = ring.index == self.variant;
            let hover = self.hovered == Some(ring.index);
            let color = if on {
                theme::accent()
            } else if hover {
                theme::hover_edge()
            } else {
                theme::ring_idle()
            };
            if on {
                ring_divs.extend(lit_ring(ring.radius, ring.width));
            } else {
                ring_divs.push(ring_div(ring.radius, ring.width).border_color(color));
            }
            if crowded && !on && !hover {
                continue;
            }
            let v = &master.variants[ring.index];
            let anchor = ring.radius * std::f32::consts::FRAC_1_SQRT_2;
            let (x, y) = (c + anchor, c - anchor);
            let label = format::variant(v);
            let rate = v
                .bandwidth
                .filter(|_| v.resolution.is_some())
                .map(format::bitrate);
            labels.push(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(LABEL_X - 4. - x))
                    .h(px(1.))
                    .bg(if on { theme::accent() } else { theme::edge() }),
            );
            labels.push(
                div()
                    .absolute()
                    .left(px(LABEL_X))
                    .top(px(y - 11.))
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .whitespace_nowrap()
                    .text_size(px(15.))
                    .child(ui::num(
                        div()
                            .text_color(if on { theme::text() } else { theme::muted() })
                            .when(on, |s| s.font_weight(FontWeight::BOLD))
                            .child(label),
                    ))
                    .children(rate.map(|rate| {
                        ui::num(
                            div()
                                .text_size(px(12.))
                                .text_color(theme::muted())
                                .child(rate),
                        )
                    })),
            );
        }
        let picked = &master.variants[self.variant];
        let size = picked
            .bandwidth
            .filter(|_| !probe.live)
            .map(|bw| format::size_bound(bw, duration))
            .unwrap_or_default();
        let outer = rings.first().expect("主播放列表至少有一档");
        Figure {
            rings: ring_divs,
            labels,
            center: format::variant(picked),
            note: size,
            edge: outer.radius + outer.width / 2.,
        }
    }
}

/// 认出文件的表盘：整圈暗的主色，圆盘里写大小（服务器没给出时写「大小未知」）与类型。
fn file_dial(info: &FileInfo) -> Div {
    let ticks = Ticks::full(
        ring::COMET_TICKS,
        theme::alpha(theme::accent(), 0.45),
        false,
    );
    let big = info
        .length
        .map_or_else(|| "大小未知".to_owned(), format::bytes);
    let line = info
        .content_type
        .clone()
        .unwrap_or_else(|| "类型未知".to_owned());
    dial::dial(
        theme::accent(),
        dial::figure(move |bounds, window, _| {
            ring::paint_ticks(bounds, ring::RADIUS, &ticks, window)
        }),
        dial::readout("文件", theme::accent(), dial::Big::Phrase(big), line),
    )
}

/// 取到种子信息后的表盘：圆盘里写选了的文件的大小与几个文件。
fn torrent_dial(choice: &TorrentChoice) -> Div {
    let ticks = Ticks::full(
        ring::COMET_TICKS,
        theme::alpha(theme::accent(), 0.45),
        false,
    );
    let (count, bytes) = choice.selected();
    let line = format!("{count} / {} 个文件", choice.info.files.len());
    dial::dial(
        theme::accent(),
        dial::figure(move |bounds, window, _| {
            ring::paint_ticks(bounds, ring::RADIUS, &ticks, window)
        }),
        dial::readout(
            "BT",
            theme::accent(),
            dial::Big::Phrase(format::bytes(bytes)),
            line,
        ),
    )
}

/// 探测失败的表盘：整圈暗的警告色，圆盘里写状态码或种类（见 [`format::failure`]）；怎么办写在检查面板里。
fn failed_dial(failure: &Failure) -> Div {
    let (mark, _) = format::failure(failure.kind);
    let ticks = Ticks::full(ring::COMET_TICKS, theme::alpha(theme::warn(), 0.45), false);
    dial::dial(
        theme::warn(),
        dial::figure(move |bounds, window, _| {
            ring::paint_ticks(bounds, ring::RADIUS, &ticks, window)
        }),
        dial::readout("探测失败", theme::warn(), dial::Big::Figure(mark), ""),
    )
}

/// 同心环区域里随探测状态变的部分。
struct Figure {
    /// 各环；选中的一环是叠起来的几层
    rings: Vec<Div>,
    /// 引线与标签
    labels: Vec<Div>,
    /// 圆心的大字
    center: String,
    /// 圆心大字下面的小字
    note: String,
    /// 最外一环的外沿离圆心多远：窗口背景的日冕从这里放出
    edge: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四档：由外向内按码率排，环宽随码率，最内一环不小于下限；点在两环之间离谁近算谁，离得远的不算。
    #[test]
    fn rings_follow_bitrate_and_clicks_pick_the_nearest() {
        let rates = [
            (0, Some(1_200_000)),
            (1, Some(18_200_000)),
            (2, Some(6_300_000)),
            (3, Some(3_100_000)),
        ];
        let rings = rings_of(&rates);
        let order: Vec<usize> = rings.iter().map(|r| r.index).collect();
        assert_eq!(order, [1, 2, 3, 0]);
        assert_eq!((rings[0].radius, rings[0].width), (OUTER, 26.));
        assert_eq!(rings[3].radius, INNER);
        assert!(rings.windows(2).all(|pair| pair[0].width > pair[1].width));
        assert_eq!(hit(&rings, OUTER), Some(1));
        assert_eq!(hit(&rings, OUTER + 13. + HIT_SLOP), Some(1));
        assert_eq!(hit(&rings, OUTER + 13. + HIT_SLOP + 1.), None);
        assert_eq!(hit(&rings, INNER - 30.), None);
    }

    /// 档数很多时半径差变小，环宽压到下限，仍从外到内排开、不重叠。
    #[test]
    fn many_variants_still_fit() {
        let rates: Vec<(usize, Option<u64>)> = (0..20)
            .map(|i| (i, Some(1_000_000 * (i as u64 + 1))))
            .collect();
        let rings = rings_of(&rates);
        assert_eq!(rings.len(), 20);
        assert!(rings.iter().all(|r| r.width == 3.));
        assert!(
            rings
                .windows(2)
                .all(|pair| pair[0].radius - pair[1].radius >= pair[0].width)
        );
        let innermost = rings.last().map_or(0., |r| r.radius);
        assert!((innermost - INNER).abs() < 1e-3, "{innermost}");
    }
}
