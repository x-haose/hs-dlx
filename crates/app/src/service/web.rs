//! 网页视频的组件与解析（yt-dlp）：组件到用户要用时才安装，安装的进度与组件的状态经 watch 通道给界面；解析在常驻的
//! 进程里做（见 [`hs_dlx_ytdlp::Extractor`]）。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_dlx_ytdlp::{
    Channel, Components, ExtractError, Extractor, Info, InstallProgress, Pins, Request, Sources,
};
use hs_m3u8_core::{CancellationToken, Engine, Url};
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

use super::{Service, lock};
use crate::Error;
use crate::store;
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
    /// 装好了（`version` 照常在用），上次更新没成：原因
    UpdateFailed {
        version: String,
        reason: String,
    },
    /// 没装成（可再装，已下的接着用）：原因
    Failed(String),
    /// 装好了而解析进程起不来（文件被杀毒软件隔离或坏了）：原因；要重装
    Broken(String),
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
    pub(super) extractor: Mutex<Option<Arc<Extractor>>>,
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

    /// 装好了在用的 yt-dlp 版本；没装好（含正在装）时为 None。
    fn version(&self) -> Option<String> {
        match &*self.state.borrow() {
            WebState::Ready(version) | WebState::UpdateFailed { version, .. } => {
                Some(version.clone())
            }
            WebState::Unavailable
            | WebState::Missing
            | WebState::Installing(_)
            | WebState::Failed(_)
            | WebState::Broken(_) => None,
        }
    }

    /// `extractor` 的解析进程起不来（组件的文件被隔离或坏了）：它还是在用的那个时不再拿它解析，状态记为坏了（带上
    /// 原因），由用户重装；已换掉（重装、更新过）时什么也不做。在用它的任务照样以组件坏了停下。
    pub(super) fn broken(&self, extractor: &Arc<Extractor>, reason: String) {
        let mut current = lock(&self.extractor);
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, extractor))
        {
            *current = None;
            self.state.send_replace(WebState::Broken(reason));
        }
    }

    /// 删掉不再用的组件（见 [`Components::sweep`]）；本平台没有组件时什么也不做。
    pub(super) async fn sweep(&self) {
        if let Some(components) = &self.components {
            components.sweep().await;
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
    /// 记下网页视频的 yt-dlp 渠道：之后的安装与更新照它。
    pub fn set_web_channel(&self, channel: Channel) -> Result<(), Error> {
        self.shared.write(|tx| store::set_web_channel(tx, channel))
    }

    /// 记下网页视频的画质取向：之后的解析照它挑画质选项。
    pub fn set_web_preference(&self, preference: Preference) -> Result<(), Error> {
        self.shared
            .write(|tx| store::set_web_preference(tx, preference))
    }

    /// 经设置里的代理查设置里的渠道上最新的 yt-dlp：比装着的新（版本不同）时为它，已是最新时为 None。组件没装好时报
    /// [`Error::WebNotReady`]；句柄在任何执行器上都可等待。
    pub fn check_web_update(
        &self,
    ) -> Result<AbortOnDropHandle<Result<Option<String>, hs_dlx_ytdlp::Error>>, Error> {
        let web = &self.shared.web;
        let current = web.version().ok_or(Error::WebNotReady)?;
        let settings = self.settings()?;
        let shared = self.shared.clone();
        Ok(AbortOnDropHandle::new(self.runtime().spawn(async move {
            let components = shared.web.components.as_ref().expect("装好了即有组件");
            let latest = components
                .latest(&settings.proxy, settings.web_channel)
                .await?;
            Ok((latest != current).then_some(latest))
        })))
    }

    /// 把 yt-dlp 换成 `version`（经设置里的代理），进度与结果见 [`Service::web_state`]：装好、自检过了才换上，在用的
    /// 解析进程随之换成新的；没装成时照旧用原来的版本，状态回到它并带上原因。组件没装好、或正在装时报
    /// [`Error::WebNotReady`]。
    pub fn update_web(&self, version: String) -> Result<(), Error> {
        let web = &self.shared.web;
        let current = web.version().ok_or(Error::WebNotReady)?;
        let proxy = self.settings()?.proxy;
        let cancel = {
            let mut installing = lock(&web.installing);
            if installing.is_some() {
                return Err(Error::WebNotReady);
            }
            let cancel = CancellationToken::new();
            *installing = Some(cancel.clone());
            cancel
        };
        let shared = self.shared.clone();
        let _entered = self.runtime().enter();
        self.shared.tracker.spawn(async move {
            let web = &shared.web;
            let components = web.components.as_ref().expect("装好了即有组件");
            let state = web.state.clone();
            let updated = components
                .update(
                    &proxy,
                    &version,
                    &|progress| {
                        state.send_replace(WebState::Installing(progress));
                    },
                    &cancel,
                )
                .await;
            let next = match updated {
                Ok(installed) => {
                    let version = installed.version.clone();
                    *lock(&web.extractor) = Some(Arc::new(Extractor::new(
                        installed,
                        web.cache.clone(),
                        EXTRACT_TIMEOUT,
                    )));
                    WebState::Ready(version)
                }
                // 没换成：原来的照旧在用
                Err(error) => WebState::UpdateFailed {
                    version: current,
                    reason: error.to_string(),
                },
            };
            *lock(&web.installing) = None;
            web.state.send_replace(next);
        });
        Ok(())
    }

    /// 换回上一个 yt-dlp 版本（不联网，见 [`Components::rollback`]），结果见 [`Service::web_state`]：自检过了才换，在用
    /// 的解析进程随之换成它；没换成时照旧用原来的，状态回到它并带上原因。组件没装好、正在装时报
    /// [`Error::WebNotReady`]。
    pub fn rollback_web(&self) -> Result<(), Error> {
        let web = &self.shared.web;
        let current = web.version().ok_or(Error::WebNotReady)?;
        let cancel = {
            let mut installing = lock(&web.installing);
            if installing.is_some() {
                return Err(Error::WebNotReady);
            }
            let cancel = CancellationToken::new();
            *installing = Some(cancel.clone());
            cancel
        };
        let shared = self.shared.clone();
        let _entered = self.runtime().enter();
        self.shared.tracker.spawn(async move {
            let web = &shared.web;
            let components = web.components.as_ref().expect("装好了即有组件");
            let next = match components.rollback(&cancel).await {
                Ok(installed) => {
                    let version = installed.version.clone();
                    *lock(&web.extractor) = Some(Arc::new(Extractor::new(
                        installed,
                        web.cache.clone(),
                        EXTRACT_TIMEOUT,
                    )));
                    WebState::Ready(version)
                }
                Err(error) => WebState::UpdateFailed {
                    version: current,
                    reason: error.to_string(),
                },
            };
            *lock(&web.installing) = None;
            web.state.send_replace(next);
        });
        Ok(())
    }

    /// 可换回的上一个 yt-dlp 版本（见 [`Service::rollback_web`]）；没装好或没有时为 None。
    pub fn web_previous(&self) -> Option<String> {
        lock(&self.shared.web.extractor)
            .as_ref()
            .and_then(|extractor| extractor.installed().previous.clone())
    }

    /// 网页视频的组件的状态，随安装进度与结果变。
    pub fn web_state(&self) -> watch::Receiver<WebState> {
        self.shared.web.state.subscribe()
    }

    /// 安装网页视频的组件（经设置里的代理），进度与结果见 [`Service::web_state`]：没装、没装成时安装（已下的接着用）；
    /// 坏了或装好了时重装（见 [`Components::repair`]），在用的解析进程随之换成新的；正在装时什么也不做。本平台没有组件时报
    /// [`Error::WebUnavailable`]，读不出设置时报错。
    pub fn install_web(&self) -> Result<(), Error> {
        let web = &self.shared.web;
        if web.components.is_none() {
            return Err(Error::WebUnavailable);
        }
        let settings = self.settings()?;
        let (proxy, channel) = (settings.proxy, settings.web_channel);
        let before = web.state.borrow().clone();
        let repair = !matches!(before, WebState::Missing | WebState::Failed(_));
        if repair
            && lock(&self.shared.running)
                .values()
                .any(|running| matches!(running.control, super::Control::Web(_)))
        {
            return Err(Error::WebInUse);
        }
        let cancel = {
            let mut installing = lock(&web.installing);
            if installing.is_some() {
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
            let report = |progress| {
                state.send_replace(WebState::Installing(progress));
            };
            let installed = if repair {
                // 闲着的解析进程用着要换下的组件
                let old = lock(&web.extractor).take();
                if let Some(old) = old {
                    old.shutdown().await;
                }
                let repaired = components.repair(&proxy, channel, &report, &cancel).await;
                if let Err(error) = &repaired {
                    // 新的没就位时旧的照旧在（下载失败、取消），照原来的状态接着用
                    let kept = match components.installed() {
                        Ok(Some(installed)) if !matches!(before, WebState::Broken(_)) => {
                            Some(installed)
                        }
                        Ok(_) | Err(_) => None,
                    };
                    let next = match (kept, &before) {
                        (Some(installed), _) => {
                            let version = installed.version.clone();
                            *lock(&web.extractor) = Some(Arc::new(Extractor::new(
                                installed,
                                web.cache.clone(),
                                EXTRACT_TIMEOUT,
                            )));
                            WebState::UpdateFailed {
                                version,
                                reason: format!("重装没成：{error}"),
                            }
                        }
                        (None, WebState::Broken(_)) => {
                            WebState::Broken(format!("重装没成：{error}"))
                        }
                        (None, _) => WebState::Failed(error.to_string()),
                    };
                    *lock(&web.installing) = None;
                    web.state.send_replace(next);
                    return;
                }
                repaired
            } else {
                components.install(&proxy, channel, &report, &cancel).await
            };
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

    /// 经设置里的代理解析网页地址 `url`（带上请求头 `headers` 里的 Cookie，发往网页所在的可注册域名，与任务运行时
    /// 一样），按设置里的取向给出画质选项。组件没装好时报 [`Error::WebNotReady`]，读不出设置时报错；句柄在任何执行器上都可等待，释放它即取消（解析进程随之杀掉）。只有
    /// 通用解析器认得的地址报 [`ExtractError::Unsupported`]（转嗅探页找）。
    pub fn resolve(
        &self,
        url: Url,
        headers: &[(String, String)],
    ) -> Result<AbortOnDropHandle<Result<Resolved, ExtractError>>, Error> {
        let extractor = lock(&self.shared.web.extractor)
            .clone()
            .ok_or(Error::WebNotReady)?;
        let settings = self.settings()?;
        let (proxy, preference) = (settings.proxy, settings.web_preference);
        let request = Request {
            cookies: super::web_task::cookies(&url, headers),
            url,
            generic: false,
        };
        let shared = self.shared.clone();
        Ok(AbortOnDropHandle::new(self.runtime().spawn(async move {
            // 句柄释放时任务被丢弃，解析进程随 Worker 的释放杀掉
            let info = extractor
                .extract(&request, &proxy, &CancellationToken::new())
                .await
                .inspect_err(|error| {
                    if let ExtractError::Broken(reason) = error {
                        shared.web.broken(&extractor, reason.clone());
                    }
                })?;
            let choices = choices(&info, preference);
            Ok(Resolved { info, choices })
        })))
    }
}
