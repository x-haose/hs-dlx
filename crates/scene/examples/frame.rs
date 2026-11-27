//! 按设计稿 0.6.0 方向 C 的布局把日冕的各个状态在两套主题下各渲染一帧，存成 PNG，并量连续渲染的耗时：
//! `cargo run -p hs-dlx-scene --example frame -- <输出目录> [每点像素数]`。文件名为 `<主题>-<状态>.png`。用来在不开应用
//! 的情况下对着稿看着色器与各状态的参数；要有 GPU。

use std::path::Path;
use std::time::{Duration, Instant};

use hs_dlx_scene::{Mode, Motion, Params, Renderer, Target, Theme};

/// 稿的画面大小（逻辑点）。
const WIDTH: f32 = 1440.;
const HEIGHT: f32 = 900.;
/// 舞台上的圆盘（稿的主稿）：圆心与半径。
const STAGE: [f32; 2] = [1030., 372.];
const RADIUS: f32 = 150.;
/// 左边的列表一栏（左、上、右、下）与它的圆角。
const LIST: [f32; 4] = [20., 60., 612., 880.];
const LIST_RADIUS: f32 = 16.;
/// 光芒在循环里取的一处：与稿里的帧相同。
const PHASE: f32 = 0.3;

/// 一个状态：文件名里的名字、圆盘的圆心与半径、有没有列表一栏。
struct Shot {
    name: &'static str,
    mode: Mode,
    center: [f32; 2],
    radius: f32,
    list: bool,
}

fn shots() -> Vec<Shot> {
    let stage = |name, mode| Shot {
        name,
        mode,
        center: STAGE,
        radius: RADIUS,
        list: true,
    };
    vec![
        // 稿 C1：没有列表，圆盘在正中
        Shot {
            name: "standby",
            mode: Mode::Standby,
            center: [720., 366.],
            radius: 168.,
            list: false,
        },
        stage("preparing", Mode::Preparing),
        stage("choosing", Mode::Choosing),
        // 稿 C3：圆盘缩成舞台标题左边的小点
        Shot {
            name: "asking",
            mode: Mode::Asking,
            center: [682., 98.],
            radius: 15.,
            list: true,
        },
        // 主稿：42%，4.5 MB/s
        stage(
            "downloading",
            Mode::Downloading {
                progress: 0.42,
                speed: 4.5e6,
            },
        ),
        // 稿 C7：第 42 分，3.4 Mbps
        stage(
            "recording",
            Mode::Recording {
                hour: 0.7,
                rate: 3.4e6 / 8.,
            },
        ),
        stage("paused", Mode::Paused { progress: 0.42 }),
        // 稿 C4：354 / 480 片时被拒
        Shot {
            center: [1030., 352.],
            ..stage(
                "failed",
                Mode::Failed {
                    progress: Some(0.73),
                },
            )
        },
        stage("done", Mode::Done),
        stage("seeding", Mode::Seeding { upload: 1.2e6 }),
    ]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().expect("用法：frame <输出目录> [每点像素数]");
    let out = Path::new(&out);
    let scale: f32 = args
        .next()
        .map_or(1.0, |s| s.parse().expect("每点像素数是数字"));
    std::fs::create_dir_all(out).expect("建输出目录");
    let mut renderer = Renderer::new().expect("建渲染器");
    let mut timed = None;
    for (theme, theme_name, ground) in [
        (Theme::Eclipse, "eclipse", srgb(0x04060A)),
        (Theme::BloodMoon, "blood", srgb(0x090504)),
    ] {
        for shot in shots() {
            let mut motion = Motion::new();
            let target = Target {
                center: shot.center,
                radius: shot.radius,
                mode: shot.mode,
            };
            motion.step(Some(&target), theme, Duration::ZERO, true);
            let mut corona = motion.corona().expect("刚给过目标");
            corona.phase = PHASE;
            let params = Params {
                width: (WIDTH * scale) as u32,
                height: (HEIGHT * scale) as u32,
                scale,
                corona,
                ground,
                panel: shot.list.then_some(LIST),
                panel_radius: LIST_RADIUS,
            };
            let frame = renderer.render(&params).expect("渲染");
            timed.get_or_insert(params);
            let mut rgba = frame.bgra;
            for pixel in rgba.chunks_mut(4) {
                pixel.swap(0, 2);
            }
            image::save_buffer(
                out.join(format!("{theme_name}-{}.png", shot.name)),
                &rgba,
                frame.width,
                frame.height,
                image::ExtendedColorType::Rgba8,
            )
            .expect("写 PNG");
        }
    }
    let params = timed.expect("至少画了一帧");
    let started = Instant::now();
    for i in 0..30 {
        let mut next = params;
        next.corona.phase = i as f32 / 30.0;
        renderer.render(&next).expect("渲染");
    }
    println!(
        "{}×{}：每帧 {:?}",
        params.width,
        params.height,
        started.elapsed() / 30
    );
}

/// `0xRRGGBB` 换成 sRGB 编码的 0 到 1。
fn srgb(hex: u32) -> [f32; 3] {
    [hex >> 16, hex >> 8, hex].map(|channel| (channel & 0xFF) as f32 / 255.)
}
