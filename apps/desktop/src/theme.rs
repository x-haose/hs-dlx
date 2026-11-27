//! 界面配色：每套主题（[`Theme`]）一组角色，取值照设计稿的 `hs.css`。
//!
//! 角色：窗口底色 [`bg`]；圆盘里面 [`moon`]；文字 [`ink`] 按透明度分三级（[`ink`]、[`ink_2`]、[`ink_3`]）；主色
//! [`accent`]（进度、选中、每屏最该按的那一个）；[`deep`]（光的深处、做种）；[`core`](fn@core)（光最亮处）；警告色 [`warn`]
//! （要处理的问题）；直播色 [`live`]（录制）。线与面都是 [`ink`] 的低透明度（[`rule`]、[`outline`]、[`wash`]），所以
//! 中性色随主题：血月下底色、线、次要字都偏暖。
//!
//! 一个光源：界面上除背景的日冕外没有东西发光，没有外发光与内发光。
//!
//! 主题是整个应用的状态（只有一个窗口），启动时按设置由 [`set`] 定，在设置页换了之后要让窗口重画。

use std::sync::atomic::{AtomicU8, Ordering};

use gpui_kit::{Hsla, rgb};
pub use hs_dlx_app::Theme;

/// 当前主题：0 为日蚀，1 为血月。
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 一套主题的各个角色（sRGB，0xRRGGBB）。
struct Palette {
    bg: u32,
    moon: u32,
    /// 实心主色、直播色上的文字
    on: u32,
    ink: u32,
    accent: u32,
    deep: u32,
    core: u32,
    warn: u32,
    live: u32,
}

const ECLIPSE: Palette = Palette {
    bg: 0x04060A,
    moon: 0x020306,
    on: 0x04141A,
    ink: 0xEEF1F6,
    accent: 0x5BE7F5,
    deep: 0x3A60FF,
    core: 0xE4F9FF,
    warn: 0xFFAC60,
    live: 0xFF626A,
};

const BLOOD_MOON: Palette = Palette {
    bg: 0x090504,
    moon: 0x050302,
    on: 0x200A04,
    ink: 0xF7F0EA,
    accent: 0xFF7C4A,
    deep: 0xC42212,
    core: 0xFFE4B8,
    warn: 0xFFD870,
    live: 0xFF609C,
};

/// 换主题；之后取的颜色都按它。
pub fn set(theme: Theme) {
    let code = match theme {
        Theme::Eclipse => 0,
        Theme::BloodMoon => 1,
    };
    CURRENT.store(code, Ordering::Relaxed);
}

/// 当前主题。
pub fn current() -> Theme {
    match CURRENT.load(Ordering::Relaxed) {
        0 => Theme::Eclipse,
        _ => Theme::BloodMoon,
    }
}

fn palette() -> &'static Palette {
    match current() {
        Theme::Eclipse => &ECLIPSE,
        Theme::BloodMoon => &BLOOD_MOON,
    }
}

fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}

/// 窗口底色；列表一栏与舞台直接铺在它上面
pub fn bg() -> Hsla {
    color(palette().bg)
}

/// 圆盘里面：比窗口底色更暗
pub fn moon() -> Hsla {
    color(palette().moon)
}

/// 实心的主色、直播色上的文字
pub fn on() -> Hsla {
    color(palette().on)
}

/// 正文
pub fn ink() -> Hsla {
    color(palette().ink)
}

/// 次一级的文字：来源、说明、没选中的选项
pub fn ink_2() -> Hsla {
    alpha(ink(), 0.72)
}

/// 最次一级、只作标注的文字：小标题、读数的标签、时刻、占位字
pub fn ink_3() -> Hsla {
    alpha(ink(), 0.56)
}

/// 分隔线与容器的边：读数之间的竖线、分隔线、面板与输入框的描边
pub fn rule() -> Hsla {
    alpha(ink(), 0.1)
}

/// 可点的东西的描边（次按钮、选项、键帽），与进度环没走到的部分
pub fn outline() -> Hsla {
    alpha(ink(), 0.16)
}

/// 抬起的面：次按钮与粘贴栏的底
pub fn wash() -> Hsla {
    alpha(ink(), 0.05)
}

/// 悬停时的底
pub fn hover() -> Hsla {
    alpha(ink(), 0.08)
}

/// 输入框的底：比窗口底色凹下去一层
pub fn sunken() -> Hsla {
    alpha(gpui_kit::black(), 0.3)
}

/// 主色
pub fn accent() -> Hsla {
    color(palette().accent)
}

/// 光的深处；做种的颜色
pub fn deep() -> Hsla {
    color(palette().deep)
}

/// 光最亮处
pub fn core() -> Hsla {
    color(palette().core)
}

/// 警告色：重试、失败、等用户处理；血月下为琥珀黄，与火红的主色分开
pub fn warn() -> Hsla {
    color(palette().warn)
}

/// 直播录制；血月下偏玫红，与火红的主色分开
pub fn live() -> Hsla {
    color(palette().live)
}

/// `color` 换成透明度 `alpha`（0 到 1）。
pub fn alpha(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha, ..color }
}
