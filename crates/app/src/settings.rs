//! 应用的设置，与任务一起存在任务存储里。

use std::num::NonZeroU32;
use std::path::PathBuf;

use hs_m3u8_core::Proxy;

/// 应用的设置。
#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    /// 新建任务默认保存到的目录（绝对路径）；没设过时为 None，由界面取系统的默认目录
    pub folder: Option<PathBuf>,
    /// 探测与开始任务时请求经的代理；任务开始后不随设置改变，暂停再继续后才换
    pub proxy: Proxy,
    pub theme: Theme,
    /// BT 任务下完之后做不做种
    pub seeding: Seeding,
    /// 整个 BT 会话的上传限速，字节每秒；None 为不限
    pub upload_limit: Option<NonZeroU32>,
    /// 启动时与之后每天查一次有没有新版本
    pub check_updates: bool,
    /// BT 的节点连接、tracker 与 DHT 经的 SOCKS5 代理（`socks5://` 或 `socks5h://`，带端口，可带用户名与密码）；None
    /// 为直连。与 `proxy` 无关：那个只管 HTTP 的请求
    pub torrent_proxy: Option<hs_m3u8_core::Url>,
    /// 网页视频的 yt-dlp 装与更新走的渠道
    pub web_channel: hs_dlx_ytdlp::Channel,
    /// 网页视频的画质选项按什么挑（见 [`crate::web::choices`]）
    pub web_preference: crate::web::Preference,
    /// 解析网页视频时另从这个浏览器的这个用户资料读登录后的 Cookie；None 为不读
    pub web_browser: Option<hs_dlx_ytdlp::CookieSource>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            folder: None,
            proxy: Proxy::System,
            theme: Theme::Eclipse,
            seeding: Seeding::default(),
            upload_limit: None,
            check_updates: true,
            torrent_proxy: None,
            web_channel: hs_dlx_ytdlp::Channel::Stable,
            web_preference: crate::web::Preference::Compatible,
            web_browser: None,
        }
    }
}

/// BT 任务下完之后做不做种、做到什么时候停。分享率为累计上传的与选了的文件的字节数之比，累计上传与做种时长都含之前
/// 各次运行的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seeding {
    /// 下完即停
    Off,
    /// 做种，任一条件到了即停；都为 None 时一直做到手动停
    Until {
        /// 分享率的百分数：100 为 1.0
        ratio_percent: Option<NonZeroU32>,
        /// 累计做种的分钟数
        minutes: Option<NonZeroU32>,
    },
}

impl Default for Seeding {
    /// 分享率 1.0（传回与下的一样多）或做种 24 小时，先到为准。
    fn default() -> Self {
        Seeding::Until {
            ratio_percent: NonZeroU32::new(100),
            minutes: NonZeroU32::new(24 * 60),
        }
    }
}

impl Seeding {
    /// 累计上传 `uploaded` 字节、做种 `seconds` 秒、选了的文件共 `wanted` 字节时，到了停的条件没有；不做种时总是到了。
    pub fn reached(self, uploaded: u64, wanted: u64, seconds: u64) -> bool {
        match self {
            Seeding::Off => true,
            Seeding::Until {
                ratio_percent,
                minutes,
            } => {
                let ratio = ratio_percent.is_some_and(|percent| {
                    u128::from(uploaded) * 100 >= u128::from(wanted) * u128::from(percent.get())
                });
                let time = minutes.is_some_and(|minutes| seconds >= u64::from(minutes.get()) * 60);
                ratio || time
            }
        }
    }
}

/// 外观主题：窗口背景的光与界面的强调色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// 日蚀：青色的光，外缘偏深蓝
    Eclipse,
    /// 血月：火红的光，外缘偏暗红
    BloodMoon,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn until(ratio_percent: u32, minutes: u32) -> Seeding {
        Seeding::Until {
            ratio_percent: NonZeroU32::new(ratio_percent),
            minutes: NonZeroU32::new(minutes),
        }
    }

    #[test]
    fn seeding_stops_at_whichever_comes_first() {
        let wanted = 1_000;
        // 默认：分享率 1.0 或 24 小时
        let default = Seeding::default();
        assert!(!default.reached(999, wanted, 24 * 3600 - 1));
        assert!(default.reached(1_000, wanted, 0));
        assert!(default.reached(0, wanted, 24 * 3600));
        // 只按分享率（150%）：时长再长也不停
        let ratio = until(150, 0);
        assert!(!ratio.reached(1_499, wanted, u64::MAX));
        assert!(ratio.reached(1_500, wanted, 0));
        // 都不限：一直做；不做种：总是到了
        assert!(!until(0, 0).reached(u64::MAX, wanted, u64::MAX));
        assert!(Seeding::Off.reached(0, wanted, 0));
        // 大数不溢出
        assert!(!until(u32::MAX, 0).reached(u64::MAX, u64::MAX, 0));
    }
}
