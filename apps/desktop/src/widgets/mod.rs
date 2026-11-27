//! 基础部件：按钮、选项与「选中」的画法、输入框与键帽、步骤条、读数与速度折线、信号时间线、文字。
//!
//! 圆角两档：控件 [`CONTROL_RADIUS`]（按钮、输入框、列表行、选项），容器 [`CONTAINER_RADIUS`]（面板、网页框、弹出
//! 面板）；键帽 [`KEY_RADIUS`]。颜色只取 [`crate::theme`] 的角色，部件不发光。

mod button;
mod choice;
mod field;
mod record;
mod stats;
mod steps;
mod text;

pub use button::{Button, Kind, Size};
pub use choice::{Toward, chip, lit};
pub use field::{caption, field, input_frame, kbd, pastes_over};
pub use record::{Line, record};
pub use stats::{Stat, stats};
pub use steps::{Step, steps};
pub use text::{check_line, copy_link, dot, error_note, error_text, mono, note, typeset};

/// 控件的圆角：按钮、输入框、列表行、选项。
pub const CONTROL_RADIUS: f32 = 10.;
/// 容器的圆角：面板、网页框、弹出面板。
pub const CONTAINER_RADIUS: f32 = 16.;
/// 键帽的圆角。
pub const KEY_RADIUS: f32 = 6.;
/// 小标题（11 号字）的行高。
pub const CAPTION_LINE: f32 = 16.;
/// 正文（13 号字）的行高。
pub const TEXT_LINE: f32 = 20.;
/// 界面的字体（随应用打包，见 assets/fonts）；中文落到系统字体。
pub const SANS: &str = "Geist";
/// 等宽的字体：地址、路径、时刻、键帽。
pub const MONO: &str = "Geist Mono";
