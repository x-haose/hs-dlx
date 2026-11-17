//! yt-dlp 的组件与解析：第一次用到时下载 Python（python-build-standalone）、deno 与 yt-dlp，装进应用的组件目录（见
//! [`Components`]）；在常驻的 Python 进程里解析网页地址（见 [`Extractor`]）。不知道任务、存储与界面。

mod archive;
mod components;
mod extractor;
mod info;
mod pins;

use std::path::Path;

pub use crate::components::{Channel, Components, InstallProgress, Installed, Sources, Stage};
pub use crate::extractor::{Cookie, ExtractError, Extractor, Request, WORKERS};
pub use crate::info::{Entry, Format, FormatKey, Info, Live, Protocol, Size, Track};
pub use crate::pins::{Archive, Pinned, Pins};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 下载组件包或请求 PyPI 失败（网络、HTTP 状态、本机磁盘），可重试性见核心的错误
    #[error("{0}")]
    Download(hs_m3u8_core::Error),
    /// 下载的组件包与锁定的 SHA-256 不符（已删掉，重试即重新下载）
    #[error("{name} 与应用锁定的不符（应为 {expected}，下到的是 {actual}）")]
    Mismatch {
        name: String,
        expected: String,
        actual: String,
    },
    /// 组件包解不开，或里面没有要用的可执行文件
    #[error("组件包解不开：{0}")]
    Archive(String),
    /// 本机读写失败
    #[error("{0}")]
    Disk(String),
    /// pip 没把下好的包装上（离线安装，与网络无关）：原因是 pip 错误输出的末尾
    #[error("pip 没装上 yt-dlp：{0}")]
    Pip(String),
    /// yt-dlp 这个版本有依赖没有本平台能装的预编译包：换个版本（等下一版）
    #[error("这个版本在本机装不了：{0}")]
    Unsupported(String),
    /// 装上的 yt-dlp 或 deno 运行不了
    #[error("组件自检没过：{0}")]
    SelfTest(String),
    /// PyPI 回的认不出，或给的地址不对
    #[error("{0}")]
    Index(String),
    /// 子进程起不来、超时，或输出认不出
    #[error("{0}")]
    Command(String),
    /// 子进程跑完了、退出码不为 0：原因是它错误输出的末尾
    #[error("{0}")]
    Exited(String),
    /// 系统代理设置读不了
    #[error("{0}")]
    Proxy(String),
    /// 还没装组件
    #[error("还没装 yt-dlp 的组件")]
    NotInstalled,
    /// 没有可换回的上一个 yt-dlp 版本（没更新过，或它的目录没了）
    #[error("没有可换回的上一个 yt-dlp 版本")]
    NoPrevious,
    #[error("已取消")]
    Cancelled,
}

impl Error {
    fn disk(path: &Path, error: &std::io::Error) -> Error {
        Error::Disk(format!("{}：{error}", path.display()))
    }

    /// 原样再试可能就好：网络与服务器的临时故障（见核心的 [`hs_m3u8_core::Error::retryable`]），或下到的包对不上（已
    /// 删掉，再试即重新下载）。其余要改设置、腾空间、等新版本或修应用。
    pub fn retryable(&self) -> bool {
        match self {
            Error::Download(error) => error.retryable(),
            Error::Mismatch { .. } => true,
            Error::Archive(_)
            | Error::Disk(_)
            | Error::Pip(_)
            | Error::Unsupported(_)
            | Error::SelfTest(_)
            | Error::Index(_)
            | Error::Command(_)
            | Error::Exited(_)
            | Error::Proxy(_)
            | Error::NotInstalled
            | Error::NoPrevious
            | Error::Cancelled => false,
        }
    }
}
