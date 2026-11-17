//! 网页视频的组件与解析（yt-dlp）：组件到用户要用时才安装，安装的进度与组件的状态经 watch 通道给界面；解析在常驻的
//! 进程里做（见 [`hs_dlx_ytdlp::Extractor`]）。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_dlx_ytdlp::{
    Channel, Components, Cookie, ExtractError, Extractor, Info, InstallProgress, Pins, Request,
    Sources,
};
use hs_m3u8_core::{CancellationToken, Engine, Url};
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

use super::{Service, lock};
use crate::Error;
use crate::web::{Choice, Preference, choices};

/// 一次解析最多多久：YouTube 第一次要下播放器、解 JavaScript 挑战，慢的网络与代理下约半分钟。
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);

/// 网页视频的组件放在哪、从哪取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    /// 组件目录（不存在时安装时建）；yt-dlp 的缓存放在它下面的 `cache`
    pub root: PathBuf,
    /// 本平台锁定的 Python 与 deno；应用不发布的平台为 None，网页视频用不了
    pub pins: Option<Pins>,
    pub sources: Sources,
}

/// 网页视频的组件现在怎样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebState {
    /// 这个平台没有组件
    Unavailable,
    /// 还没装
    Missing,
    Installing(InstallProgress),
    /// 装好了：yt-dlp 的版本
    Ready(String),
    /// 没装成（可再装）：原因
    Failed(String),
}

/// 一个网页地址解析出的视频与它的画质选项。
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub info: Info,
    /// 见 [`crate::web::choices`]；没有能下的格式时为空
    pub choices: Vec<Choice>,
}

pub(super) struct Web {
    /// 本平台没有组件时为 None
    components: Option<Components>,
    cache: PathBuf,
    state: watch::Sender<WebState>,
    /// 装好了才有；换了 yt-dlp 的版本时换掉（旧的进程随最后一个引用释放而结束）
    extractor: Mutex<Option<Arc<Extractor>>>,
    /// 在安装：取消它的信号
    installing: Mutex<Option<CancellationToken>>,
}

impl Web {
    /// 照 `config` 建；组件已装好时即可解析。
    pub(super) fn new(config: WebConfig, engine: Engine) -> Web {
        let cache = config.root.join("cache");
        let components = config
            .pins
            .map(|pins| Components::new(config.root, pins, config.sources, engine));
        let (state, extractor) = match components.as_ref().map(Components::installed) {
            None => (WebState::Unavailable, None),
            Some(Ok(None)) => (WebState::Missing, None),
            Some(Ok(Some(installed))) => (
                WebState::Ready(installed.version.clone()),
                Some(Arc::new(Extractor::new(
                    installed,
                    cache.clone(),
                    EXTRACT_TIMEOUT,
                ))),
            ),
            Some(Err(error)) => (WebState::Failed(error.to_string()), None),
        };
        Web {
            components,
            cache,
            state: watch::Sender::new(state),
            extractor: Mutex::new(extractor),
            installing: Mutex::new(None),
        }
    }

    /// 取消在做的安装（退出时）。
    pub(super) fn cancel_install(&self) {
        if let Some(cancel) = lock(&self.installing).as_ref() {
            cancel.cancel();
        }
    }
}

impl Service {
    /// 网页视频的组件的状态，随安装进度与结果变。
    pub fn web_state(&self) -> watch::Receiver<WebState> {
        self.shared.web.state.subscribe()
    }

    /// 安装网页视频的组件（经设置里的代理），进度与结果见 [`Service::web_state`]；已装好或正在装时什么也不做。本平台没有
    /// 组件时报 [`Error::WebUnavailable`]，读不出设置时报错。
    pub fn install_web(&self) -> Result<(), Error> {
        let web = &self.shared.web;
        if web.components.is_none() {
            return Err(Error::WebUnavailable);
        }
        let proxy = self.settings()?.proxy;
        let cancel = {
            let mut installing = lock(&web.installing);
            if installing.is_some() || matches!(*web.state.borrow(), WebState::Ready(_)) {
                return Ok(());
            }
            let cancel = CancellationToken::new();
            *installing = Some(cancel.clone());
            cancel
        };
        let shared = self.shared.clone();
        let _entered = self.runtime().enter();
        self.shared.tracker.spawn(async move {
            let web = &shared.web;
            let components = web.components.as_ref().expect("上面核对过有组件");
            let state = web.state.clone();
            let installed = components
                .install(
                    &proxy,
                    Channel::Stable,
                    &|progress| {
                        state.send_replace(WebState::Installing(progress));
                    },
                    &cancel,
                )
                .await;
            let next = match installed {
                Ok(installed) => {
                    let version = installed.version.clone();
                    *lock(&web.extractor) = Some(Arc::new(Extractor::new(
                        installed,
                        web.cache.clone(),
                        EXTRACT_TIMEOUT,
                    )));
                    WebState::Ready(version)
                }
                Err(hs_dlx_ytdlp::Error::Cancelled) => WebState::Missing,
                Err(error) => WebState::Failed(error.to_string()),
            };
            *lock(&web.installing) = None;
            web.state.send_replace(next);
        });
        Ok(())
    }

    /// 取消在做的安装；没在装时什么也不做。
    pub fn cancel_web_install(&self) {
        self.shared.web.cancel_install();
    }

    /// 经设置里的代理解析网页地址 `url`（带 `cookies`），给出画质选项（取向 `preference`）。组件没装好时报
    /// [`Error::WebNotReady`]，读不出设置时报错；句柄在任何执行器上都可等待，释放它即取消（解析进程随之杀掉）。只有
    /// 通用解析器认得的地址报 [`ExtractError::Unsupported`]（转嗅探页找）。
    pub fn resolve(
        &self,
        url: Url,
        cookies: Vec<Cookie>,
        preference: Preference,
    ) -> Result<AbortOnDropHandle<Result<Resolved, ExtractError>>, Error> {
        let extractor = lock(&self.shared.web.extractor)
            .clone()
            .ok_or(Error::WebNotReady)?;
        let proxy = self.settings()?.proxy;
        let request = Request {
            url,
            cookies,
            generic: false,
        };
        Ok(AbortOnDropHandle::new(self.runtime().spawn(async move {
            // 句柄释放时任务被丢弃，解析进程随 Worker 的释放杀掉
            let info = extractor
                .extract(&request, &proxy, &CancellationToken::new())
                .await?;
            let choices = choices(&info, preference);
            Ok(Resolved { info, choices })
        })))
    }
}
