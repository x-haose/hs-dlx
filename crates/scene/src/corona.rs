//! 日冕随任务状态的样子与状态之间的过渡：[`Mode`] 定各状态的目标，[`Motion`] 按经过的时间把 [`Corona`] 滑向目标。

use std::time::Duration;

use crate::Palette;

/// 状态切换时各项滑过去的时长：指数逼近，走完这段时间剩下的约 2%，再过约 0.3 秒落到目标上。
const FADE: Duration = Duration::from_millis(400);
/// 光芒在动的状态里走完一个循环的时长：循环首尾相接，画面不跳。
const LOOP: Duration = Duration::from_secs(24);
/// 不定进度的弧绕一圈的时长。
const SPIN: Duration = Duration::from_millis(2400);
/// 不定进度时点亮的弧长（整圈为 1）。
const SPIN_ARC: f32 = 0.28;
/// 「慢」的状态里光芒走的快慢（相对 [`LOOP`]）。
const SLOW: f32 = 0.5;
/// 离目标多近就落到目标上：0 到 1 的各项与颜色按这个差，圆心与半径按 [`NEAR_POINTS`]（逻辑点）。小于一个灰阶，看不出。
const NEAR: f32 = 1e-3;
const NEAR_POINTS: f32 = 0.1;

/// 两套主题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Eclipse,
    BloodMoon,
}

/// 日冕此刻代表的状态。进度、速度由调用方从任务里取。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// 没有任务
    Standby,
    /// 准备中（认地址、解析、取种子信息、排队等网页、在后台网页里找流）：一小段弧绕行
    Preparing,
    /// 等用户选（画质、文件、流、插件）、确认或装组件
    Choosing,
    /// 网页要用户操作（人机验证、登录）：圆盘缩成舞台标题左边的小点
    Asking,
    /// 下载中：`progress` 为进度（0 到 1），`speed` 为下载速度（字节每秒）
    Downloading {
        progress: f32,
        speed: f64,
    },
    /// 直播录制：`hour` 为这一小时走到哪（0 到 1，第 42 分为 0.7），`rate` 为码率（字节每秒）
    Recording {
        hour: f32,
        rate: f64,
    },
    Paused {
        progress: f32,
    },
    /// 失败：失败时的进度（0 到 1）；不知道进度时为 None，点亮整圈
    Failed {
        progress: Option<f32>,
    },
    Done,
    /// 做种：`upload` 为上传速度（字节每秒）
    Seeding {
        upload: f64,
    },
}

/// 光用主题的哪种颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hue {
    Accent,
    Warn,
    Live,
    Deep,
}

/// 一个状态下各项的目标。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Aim {
    progress: f32,
    reach: f32,
    strength: f32,
    bead: f32,
    hue: Hue,
    /// 光芒走的快慢（相对 [`LOOP`]）：0 为停住
    pace: f32,
    /// 不定进度：弧绕行
    spin: bool,
}

impl Mode {
    fn aim(self) -> Aim {
        let aim = |progress: f32, reach: f32, strength: f32, hue: Hue, pace: f32| Aim {
            progress: progress.clamp(0., 1.),
            reach,
            strength,
            bead: 0.,
            hue,
            pace,
            spin: false,
        };
        match self {
            Mode::Standby => aim(1., 0.8, 0.62, Hue::Accent, SLOW),
            Mode::Preparing => Aim {
                spin: true,
                ..aim(SPIN_ARC, 0.5, 0.7, Hue::Accent, 1.)
            },
            Mode::Choosing => aim(1., 0.7, 0.82, Hue::Accent, SLOW),
            Mode::Asking => aim(1., 0.3, 0.7, Hue::Warn, 0.),
            Mode::Downloading { progress, speed } => Aim {
                bead: 1.,
                ..aim(progress, reach(speed), 1., Hue::Accent, 1.)
            },
            Mode::Recording { hour, rate } => Aim {
                bead: 1.,
                ..aim(hour, reach(rate), 1., Hue::Live, 1.)
            },
            Mode::Paused { progress } => aim(progress, 0.25, 0.5, Hue::Accent, 0.),
            Mode::Failed { progress } => aim(progress.unwrap_or(1.), 0.22, 0.6, Hue::Warn, 0.),
            Mode::Done => aim(1., 0.4, 0.55, Hue::Accent, 0.),
            Mode::Seeding { upload } => aim(1., reach(upload), 0.7, Hue::Deep, 1.),
        }
    }
}

/// 光丝伸出多远的倍数随速度（字节每秒）：`0.45 + 0.39 × log10(1 + 速度 / 100 KB/s)`，到 1.6 为止。没有速度时 0.45，
/// 100 KB/s 约 0.57，1 MB/s 约 0.86，4.5 MB/s 约 1.1（设计稿主稿的长度），10 MB/s 约 1.23，约 90 MB/s 到顶。
fn reach(bytes_per_second: f64) -> f32 {
    let hundreds = (bytes_per_second.max(0.) / 1e5) as f32;
    (0.45 + 0.39 * (1. + hundreds).log10()).min(1.6)
}

/// 主题里一种颜色的光（线性的 0 到 1）。警告、直播、`deep` 三种取设计稿 `hs.css` 里同名的颜色作主色。
fn palette(theme: Theme, hue: Hue) -> Palette {
    match (theme, hue) {
        (Theme::Eclipse, Hue::Accent) => Palette {
            light: [0.25, 0.85, 1.0],
            deep: [0.10, 0.18, 1.0],
            core: [0.85, 0.97, 1.0],
        },
        // #FFAC60
        (Theme::Eclipse, Hue::Warn) => Palette {
            light: [1.0, 0.42, 0.12],
            deep: [0.55, 0.12, 0.02],
            core: [1.0, 0.88, 0.70],
        },
        // #FF626A
        (Theme::Eclipse, Hue::Live) => Palette {
            light: [1.0, 0.13, 0.16],
            deep: [0.45, 0.02, 0.06],
            core: [1.0, 0.85, 0.85],
        },
        // #3A60FF
        (Theme::Eclipse, Hue::Deep) => Palette {
            light: [0.05, 0.13, 1.0],
            deep: [0.03, 0.05, 0.55],
            core: [0.72, 0.80, 1.0],
        },
        (Theme::BloodMoon, Hue::Accent) => Palette {
            light: [1.0, 0.26, 0.05],
            deep: [0.32, 0.01, 0.015],
            core: [1.0, 0.80, 0.50],
        },
        // #FFD870
        (Theme::BloodMoon, Hue::Warn) => Palette {
            light: [1.0, 0.68, 0.16],
            deep: [0.50, 0.20, 0.02],
            core: [1.0, 0.94, 0.76],
        },
        // #FF609C
        (Theme::BloodMoon, Hue::Live) => Palette {
            light: [1.0, 0.12, 0.33],
            deep: [0.40, 0.02, 0.10],
            core: [1.0, 0.85, 0.90],
        },
        // #C42212
        (Theme::BloodMoon, Hue::Deep) => Palette {
            light: [0.55, 0.016, 0.006],
            deep: [0.20, 0.005, 0.004],
            core: [1.0, 0.62, 0.42],
        },
    }
}

/// 日冕一帧的样子。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corona {
    /// 光芒在一个循环里走到哪，0 到 1；从 1 回到 0 时画面不跳
    pub phase: f32,
    /// 圆盘的中心与半径（逻辑点，窗口坐标）；半径大于 0
    pub center: [f32; 2],
    pub radius: f32,
    /// 整体强度，0 到 1：0 时只剩底色与颗粒
    pub strength: f32,
    /// 点亮的弧：从起点顺时针到这里，0 到 1；1 为整圈
    pub progress: f32,
    /// 弧的起点从 12 点顺时针转过的圈数，0 到 1
    pub turn: f32,
    /// 光丝与光晕伸出多远的倍数，大于 0；1 为不放大
    pub reach: f32,
    /// 弧前端亮点的亮度，0 到 1
    pub bead: f32,
    pub palette: Palette,
}

/// 日冕这一帧要往哪去：圆盘的位置（逻辑点，窗口坐标）与状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    pub center: [f32; 2],
    pub radius: f32,
    pub mode: Mode,
}

/// 日冕在状态之间的过渡：每一步把各项向目标滑一段，光芒与不定进度的弧按状态走。
#[derive(Debug, Default)]
pub struct Motion {
    /// 第一次有目标之前为 None
    corona: Option<Corona>,
}

impl Motion {
    pub fn new() -> Motion {
        Motion::default()
    }

    /// 日冕此刻的样子；还没有过目标时为 None。
    pub fn corona(&self) -> Option<Corona> {
        self.corona
    }

    /// 走 `elapsed`，主题为 `theme`。`target` 为 None（页面上没有圆盘）时只把强度降到 0，其余留在原处。第一次有目标
    /// 时直接到它的位置与样子，强度从 0 亮起。`still`（窗口不在前台、系统要求减少动态效果）时各项直接到目标，光芒
    /// 停住，不定进度的弧停在 12 点。
    pub fn step(&mut self, target: Option<&Target>, theme: Theme, elapsed: Duration, still: bool) {
        let share = if still {
            1.
        } else {
            1. - (-elapsed.as_secs_f32() / (FADE.as_secs_f32() / 4.)).exp()
        };
        let Some(target) = target else {
            if let Some(corona) = &mut self.corona {
                corona.strength = approach(corona.strength, 0., share, NEAR);
            }
            return;
        };
        let aim = target.mode.aim();
        let palette = palette(theme, aim.hue);
        let corona = self.corona.get_or_insert(Corona {
            phase: 0.,
            center: target.center,
            radius: target.radius,
            strength: 0.,
            progress: aim.progress,
            turn: 0.,
            reach: aim.reach,
            bead: aim.bead,
            palette,
        });
        let seconds = elapsed.as_secs_f32();
        if !still {
            corona.phase = (corona.phase + seconds * aim.pace / LOOP.as_secs_f32()).rem_euclid(1.);
        }
        corona.turn = if still {
            0.
        } else if aim.spin {
            (corona.turn + seconds / SPIN.as_secs_f32()).rem_euclid(1.)
        } else if corona.turn > 0. {
            // 起点接着顺时针转回 12 点，不倒着走
            let turn = approach(corona.turn, 1., share, NEAR);
            if turn >= 1. { 0. } else { turn }
        } else {
            0.
        };
        for (value, aimed) in corona.center.iter_mut().zip(target.center) {
            *value = approach(*value, aimed, share, NEAR_POINTS);
        }
        corona.radius = approach(corona.radius, target.radius, share, NEAR_POINTS);
        corona.strength = approach(corona.strength, aim.strength, share, NEAR);
        corona.progress = approach(corona.progress, aim.progress, share, NEAR);
        corona.reach = approach(corona.reach, aim.reach, share, NEAR);
        corona.bead = approach(corona.bead, aim.bead, share, NEAR);
        let now = &mut corona.palette;
        for (color, aimed) in [&mut now.light, &mut now.deep, &mut now.core]
            .into_iter()
            .zip([palette.light, palette.deep, palette.core])
        {
            for (value, aimed) in color.iter_mut().zip(aimed) {
                *value = approach(*value, aimed, share, NEAR);
            }
        }
    }
}

/// `value` 向 `target` 走剩下的 `share`（0 到 1）；走完后离目标不到 `near` 时落到目标上。
fn approach(value: f32, target: f32, share: f32, near: f32) -> f32 {
    let next = value + (target - value) * share;
    if (target - next).abs() < near {
        target
    } else {
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(33);

    fn target(mode: Mode) -> Target {
        Target {
            center: [700., 380.],
            radius: 150.,
            mode,
        }
    }

    /// 以每秒 30 帧走 `total`。
    fn run(motion: &mut Motion, target: Option<&Target>, total: Duration) {
        let mut left = total;
        while !left.is_zero() {
            let step = left.min(FRAME);
            motion.step(target, Theme::Eclipse, step, false);
            left -= step;
        }
    }

    #[test]
    fn reach_follows_the_documented_curve() {
        assert_eq!(reach(0.), 0.45);
        assert!((reach(1e5) - 0.567).abs() < 0.005);
        assert!((reach(1e6) - 0.856).abs() < 0.005);
        assert!((reach(4.5e6) - 1.10).abs() < 0.005);
        assert!((reach(1e7) - 1.232).abs() < 0.005);
        assert_eq!(reach(1e12), 1.6);
    }

    /// 切换状态后各项在 400 毫秒里走完大半、不久之后正好落到目标上（之后参数不再变，渲染线程不再画）。
    #[test]
    fn a_switch_settles_exactly_on_the_target() {
        let mut motion = Motion::new();
        let downloading = target(Mode::Downloading {
            progress: 0.42,
            speed: 4.5e6,
        });
        run(&mut motion, Some(&downloading), Duration::from_secs(2));
        let failed = Target {
            center: [1030., 352.],
            ..target(Mode::Failed {
                progress: Some(0.73),
            })
        };
        run(&mut motion, Some(&failed), FADE);
        let aim = failed.mode.aim();
        let corona = motion.corona().expect("有过目标");
        assert!((corona.progress - aim.progress).abs() < 0.03 * (0.73 - 0.42));
        assert!((corona.center[0] - 1030.).abs() < 0.03 * 330.);
        run(&mut motion, Some(&failed), Duration::from_secs(1));
        let settled = motion.corona().expect("有过目标");
        assert_eq!(settled.progress, aim.progress);
        assert_eq!(settled.strength, aim.strength);
        assert_eq!(settled.bead, 0.);
        assert_eq!(settled.center, failed.center);
        assert_eq!(settled.palette, palette(Theme::Eclipse, Hue::Warn));
        // 失败是不动的状态：再走也不变
        run(&mut motion, Some(&failed), Duration::from_secs(1));
        assert_eq!(motion.corona(), Some(settled));
    }

    /// 不定进度的弧停下后，起点顺时针转回 12 点并停在 0，不倒着走。
    #[test]
    fn the_spinning_arc_returns_forward_to_twelve() {
        let mut motion = Motion::new();
        run(
            &mut motion,
            Some(&target(Mode::Preparing)),
            Duration::from_millis(900),
        );
        let spun = motion.corona().expect("有过目标").turn;
        assert!(spun > 0.3 && spun < 0.6, "转了 {spun} 圈");
        let downloading = target(Mode::Downloading {
            progress: 0.1,
            speed: 1e6,
        });
        let mut last = spun;
        for _ in 0..60 {
            motion.step(Some(&downloading), Theme::Eclipse, FRAME, false);
            let turn = motion.corona().expect("有过目标").turn;
            assert!(turn == 0. || turn >= last, "从 {last} 退到 {turn}");
            last = turn;
        }
        assert_eq!(last, 0.);
    }

    /// 减少动态效果时直接到目标，不定进度的弧停在 12 点，光芒不走。
    #[test]
    fn still_jumps_to_the_target_and_holds() {
        let mut motion = Motion::new();
        let preparing = target(Mode::Preparing);
        motion.step(Some(&preparing), Theme::Eclipse, FRAME, true);
        let corona = motion.corona().expect("有过目标");
        assert_eq!(corona.turn, 0.);
        assert_eq!(corona.phase, 0.);
        assert_eq!(corona.strength, preparing.mode.aim().strength);
        motion.step(Some(&preparing), Theme::Eclipse, FRAME, true);
        assert_eq!(motion.corona(), Some(corona));
    }

    /// 页面上没有圆盘时只熄灭，位置与弧留在原处。
    #[test]
    fn without_a_disc_only_the_strength_fades() {
        let mut motion = Motion::new();
        let done = target(Mode::Done);
        run(&mut motion, Some(&done), Duration::from_secs(2));
        let lit = motion.corona().expect("有过目标");
        run(&mut motion, None, Duration::from_secs(2));
        let dark = motion.corona().expect("有过目标");
        assert_eq!(dark.strength, 0.);
        assert_eq!(
            Corona {
                strength: 0.,
                ..lit
            },
            dark
        );
    }
}
