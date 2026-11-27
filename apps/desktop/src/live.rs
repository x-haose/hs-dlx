//! 直播钟面：一圈一小时，每格 15 秒。内圈是正在录的这一小时，外圈是上一小时；录到的格子按会话交替着色（这次打开应用
//! 以前录的不知道会话，用暗色），缺片处是加长的警告色格；录制中有一根直播色的指针指着录到的位置。时刻都按已录时长（第 0 条
//! 轨已完成分片的声明时长之和）算。

use std::time::Duration;

use gpui_kit::{Bounds, Hsla, Pixels, Point, Window, fill, point, px, size};
use hs_m3u8_core::Progress;

use crate::ring::{Stroke, TESSELLATED, angle, on_circle};
use crate::theme;

/// 一圈的时长。
pub const LAP: Duration = Duration::from_secs(3600);
/// 一圈的格数：每格 15 秒。
pub const TICKS: usize = 240;
/// 外圈的半径（绘制区域边长为 [`crate::ring::AREA`]）。
pub const OUTER: f32 = 181.;
/// 内圈的半径。
pub const INNER: f32 = 153.;
/// 刻度的半长。
pub const HALF: f32 = 7.;
/// 缺片格的半长：比普通格长，一眼能看到。
pub const MISS_HALF: f32 = 10.;
/// 圆心红点的半径。
const HUB: f32 = 7.;

/// 相邻会话交替的颜色。
pub fn session_colors() -> [Hsla; 2] {
    [theme::accent(), theme::ink()]
}

/// 录到的里不知道是哪个会话的部分（这次打开应用以前录的）的颜色。
pub fn before_color() -> Hsla {
    theme::alpha(theme::accent(), 0.45)
}

/// 这次打开应用以来看到的录制时间线：会话从哪里起、哪里缺了片。只在内存里，重开应用后从下一次进度重新记。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Timeline {
    /// 从哪里起有记录（这次打开应用以来第一次运行开始时、或第一次看到录制进度时的已录时长）；在它之前录的不知道会话与
    /// 缺片。都还没有时为 None
    pub since: Option<Duration>,
    /// 会话的起点（已录时长）与会话编号，按先后；第一项的起点为 `since`
    pub sessions: Vec<(Duration, u32)>,
    /// 缺片：记下时的已录时长与片数，按先后
    pub misses: Vec<(Duration, usize)>,
}

/// 时间线上一处要说的事：同一位置的缺片与另起会话合在一起。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub at: Duration,
    /// 缺的片数；没缺时为 0
    pub missed: usize,
    /// 从这里另起的会话编号
    pub session: Option<u32>,
}

impl Timeline {
    /// 一次运行从已录时长 `start` 开始：还没有记录时从这里记起（这次运行录到的都看得到会话）。
    pub fn begin(&mut self, start: Duration) {
        self.since.get_or_insert(start);
    }

    /// 记下一次录制进度：缺的片数比 `before`（同一次运行的上一次进度，新的运行为 None）多的在当时的位置记一处缺片；
    /// 会话编号与之前看到的不同即从这里另起一段。返回另起的会话编号。
    pub fn observe(&mut self, before: Option<&Progress>, now: &Progress) -> Option<u32> {
        let since = *self.since.get_or_insert(now.duration);
        let missed = |p: &Progress| p.segments.failed + p.segments.expired;
        let new = missed(now).saturating_sub(before.map_or(0, missed));
        if new > 0 {
            self.misses.push((now.duration, new));
        }
        let session = now.session?;
        match self.sessions.last() {
            None => {
                self.sessions.push((since, session));
                None
            }
            Some(&(_, current)) if current != session => {
                self.sessions.push((now.duration, session));
                Some(session)
            }
            Some(_) => None,
        }
    }

    /// 已录时长 `at` 处的会话在 [`Timeline::sessions`] 里的下标；在有记录之前时为 None。
    pub fn session_index(&self, at: Duration) -> Option<usize> {
        self.sessions.iter().rposition(|&(start, _)| start <= at)
    }

    /// 已录时长 `at` 处的会话起点与编号；在有记录之前时为 None。
    pub fn session_at(&self, at: Duration) -> Option<(Duration, u32)> {
        self.session_index(at).map(|index| self.sessions[index])
    }

    /// 从 `from` 起的缺片与另起会话，按位置先后，同一位置的合成一处。
    pub fn marks(&self, from: Duration) -> Vec<Mark> {
        let mut marks: Vec<Mark> = Vec::new();
        let missed = self.misses.iter().map(|&(at, count)| (at, count, None));
        let restarted = self
            .sessions
            .iter()
            .skip(1)
            .map(|&(at, session)| (at, 0, Some(session)));
        let mut all: Vec<_> = missed
            .chain(restarted)
            .filter(|&(at, ..)| at >= from)
            .collect();
        all.sort_by_key(|&(at, ..)| at);
        for (at, count, session) in all {
            match marks.last_mut() {
                Some(last) if last.at == at => {
                    last.missed += count;
                    last.session = last.session.or(session);
                }
                _ => marks.push(Mark {
                    at,
                    missed: count,
                    session,
                }),
            }
        }
        marks
    }
}

/// 第 `lap` 圈的起点。
pub fn lap_start(lap: u64) -> Duration {
    Duration::from_secs(lap * LAP.as_secs())
}

/// 一圈里的位置：已录时长 `at` 落在第几圈、圈里的比例（0 到 1，从 12 点顺时针）。
pub fn lap_of(at: Duration) -> (u64, f32) {
    let lap = at.as_secs() / LAP.as_secs();
    let within = at - lap_start(lap);
    (lap, within.as_secs_f32() / LAP.as_secs_f32())
}

/// 钟面的状态。
pub struct Face<'a> {
    pub recorded: Duration,
    pub timeline: &'a Timeline,
    /// 正在录：画指针
    pub recording: bool,
}

/// 第 `lap` 圈画在半径 `radius` 的环上，录到 `recorded` 为止；`lap` 为 None 时整圈是空格（还没有上一小时）。
fn paint_lap(
    center: Point<Pixels>,
    radius: f32,
    lap: Option<u64>,
    face: &Face<'_>,
    window: &mut Window,
) {
    let colors = session_colors();
    let mut idle = Stroke::new(2., theme::outline(), HALF);
    let mut unknown = Stroke::new(2., before_color(), HALF);
    let mut lit: Vec<Stroke> = colors
        .iter()
        .map(|&color| Stroke::new(2., color, HALF))
        .collect();
    let start = lap.map(lap_start);
    for i in 0..TICKS {
        let rad = angle(i as f32 / TICKS as f32);
        let at = start.map(|start| start + LAP / TICKS as u32 * i as u32);
        match at {
            Some(at) if at < face.recorded => match face.timeline.session_index(at) {
                Some(index) => lit[index % colors.len()].tick(center, radius, rad),
                None => unknown.tick(center, radius, rad),
            },
            _ => idle.tick(center, radius, rad),
        }
    }
    idle.paint(window);
    unknown.paint(window);
    for stroke in lit {
        stroke.paint(window);
    }
    if let Some(start) = start {
        let mut miss = Stroke::new(3., theme::warn(), MISS_HALF);
        for &(at, _) in &face.timeline.misses {
            if at >= start && at < start + LAP {
                miss.tick(center, radius, angle(lap_of(at).1));
            }
        }
        miss.paint(window);
    }
}

/// 在 `bounds` 中心画钟面：内圈是已录时长所在的这一圈，外圈是上一圈；录制中再画指针与圆心红点。
pub fn paint(bounds: Bounds<Pixels>, face: &Face<'_>, window: &mut Window) {
    let center = bounds.center();
    let (lap, within) = lap_of(face.recorded);
    paint_lap(center, OUTER, lap.checked_sub(1), face, window);
    paint_lap(center, INNER, Some(lap), face, window);
    if !face.recording {
        return;
    }
    let tip = on_circle(center, INNER - HALF - 4., angle(within));
    let mut hand = gpui_kit::PathBuilder::stroke(px(2.));
    hand.move_to(center);
    hand.line_to(tip);
    window.paint_path(hand.build().expect(TESSELLATED), theme::live());
    let hub = Bounds::new(
        point(center.x - px(HUB), center.y - px(HUB)),
        size(px(2. * HUB), px(2. * HUB)),
    );
    window.paint_quad(fill(hub, theme::live()).corner_radii(px(HUB)));
}

/// 钟面四个刻度标记（00、15、30、45 分）相对圆心的位置：半径 `radius`、从 12 点顺时针。
pub fn dial(radius: f32) -> [(&'static str, f32, f32); 4] {
    let at = |turn: f32| {
        let a = angle(turn);
        (radius * a.cos(), radius * a.sin())
    };
    let [a, b, c, d] = [0., 0.25, 0.5, 0.75].map(at);
    [
        ("00", a.0, a.1),
        ("15", b.0, b.1),
        ("30", c.0, c.1),
        ("45", d.0, d.1),
    ]
}

#[cfg(test)]
mod tests {
    use hs_m3u8_core::SegmentCounts;

    use super::*;

    fn progress(session: Option<u32>, minutes: u64, failed: usize, expired: usize) -> Progress {
        Progress {
            session,
            duration: Duration::from_secs(minutes * 60),
            segments: SegmentCounts {
                failed,
                expired,
                ..SegmentCounts::default()
            },
            ..Progress::default()
        }
    }

    fn minutes(m: u64) -> Duration {
        Duration::from_secs(m * 60)
    }

    /// 从第一次看到的进度起记；缺片按上一次进度算增量（新的运行从 0 起），会话变了另起一段并返回编号。
    #[test]
    fn the_timeline_follows_sessions_and_misses() {
        let mut timeline = Timeline::default();
        let steps = [
            (None, progress(None, 20, 0, 1)),
            (Some(progress(None, 20, 0, 1)), progress(Some(2), 30, 0, 1)),
            (
                Some(progress(Some(2), 30, 0, 1)),
                progress(Some(3), 41, 0, 4),
            ),
            // 新的运行：之前的计数不算，开头就有的 2 片照记
            (None, progress(Some(3), 60, 0, 2)),
        ];
        let started: Vec<Option<u32>> = steps
            .iter()
            .map(|(before, now)| timeline.observe(before.as_ref(), now))
            .collect();
        assert_eq!(started, [None, None, Some(3), None]);
        assert_eq!(timeline.since, Some(minutes(20)));
        assert_eq!(timeline.sessions, [(minutes(20), 2), (minutes(41), 3)]);
        assert_eq!(
            timeline.misses,
            [(minutes(20), 1), (minutes(41), 3), (minutes(60), 2)]
        );
        assert_eq!(timeline.session_index(minutes(10)), None);
        assert_eq!(timeline.session_at(minutes(45)), Some((minutes(41), 3)));
        // 同一位置的缺片与另起会话合成一处
        assert_eq!(
            timeline.marks(minutes(30)),
            [
                Mark {
                    at: minutes(41),
                    missed: 3,
                    session: Some(3)
                },
                Mark {
                    at: minutes(60),
                    missed: 2,
                    session: None
                },
            ]
        );
    }

    #[test]
    fn positions_wrap_every_hour() {
        assert_eq!(lap_of(Duration::ZERO), (0, 0.0));
        assert_eq!(lap_of(Duration::from_secs(15 * 60)), (0, 0.25));
        let (lap, within) = lap_of(Duration::from_secs(3600 + 45 * 60));
        assert_eq!(lap, 1);
        assert!((within - 0.75).abs() < 1e-6, "{within}");
        assert_eq!(lap_start(2), Duration::from_secs(7200));
    }
}
