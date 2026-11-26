//! 网页视频的组件与解析（yt-dlp）：组件到用户要用时才安装，安装的进度与组件的状态经 watch 通道给界面；解析在常驻的
//! 进程里做（见 [`hs_dlx_ytdlp::Extractor`]）。

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use hs_dlx_ytdlp::{
    Browser, Bundle, Channel, Components, CookieSource, ExtractError, Extractor, Info,
    InstallProgress, Request, Sources, Stage, is_newer,
};
use hs_m3u8_core::{CancellationToken, Engine, Proxy, Url};
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;

use super::{Service, lock};
use crate::Error;
use crate::store;
use crate::web::{Choice, Freshness, Preference, choices};

/// 一次解析最多多久：YouTube 第一次要下播放器、解 JavaScript 挑战，慢的网络与代理下约半分钟。
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);

/// 网页视频的组件放在哪、从哪取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    /// 组件目录（不存在时安装时建）；yt-dlp 的缓存放在它下面的 `cache`
    pub root: PathBuf,
    /// 随应用带的 Python 与 QuickJS-NG；应用不发布的平台为 None，网页视频用不了
    pub bundle: Option<Bundle>,
    pub sources: Sources,
    /// 多久查一次有没有新的 yt-dlp、有就换上（跳过换回时换下的那一版）；服务启动后先查一次，None 为不自动查
    pub update_every: Option<Duration>,
}

impl WebConfig {
    /// 应用用的 [`WebConfig::update_every`]：网站换了播放器、改了页面后旧版很快就解析不了，yt-dlp 跟着几天内发版。
    pub const UPDATE_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
}

/// 网页视频的组件现在怎样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebState {
    /// 这个平台没有组件
    Unavailable,
    /// 还没装
    Missing,
    /// 在装（第一次装、没装成后再装、重装）：这期间不能解析
    Installing(InstallProgress),
    /// 装好了：yt-dlp 的版本
    Ready(String),
    /// 装好了（`version` 照常在用、照常解析），正在换版本（更新或换回）
    Updating {
        version: String,
        progress: InstallProgress,
    },
    /// 装好了（`version` 照常在用），上次更新、换回或重装没成：做的是什么与原因，如「更新没成：…」
    UpdateFailed { version: String, reason: String },
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
    /// 解不出 JavaScript 挑战（[`Info::challenge_failed`]）时换新的 yt-dlp 为什么没换成（换成了的已用新的重新解析过）；
    /// 别的时候为 None
    pub freshness: Option<Freshness>,
}

/// 渠道上有没有该换上的 yt-dlp。
enum Newer {
    /// 在用的不比最新的旧
    None,
    /// 最新的是换回时换下的（见 [`hs_dlx_ytdlp::Installed::skipped`]）
    Skipped(String),
    /// 最新的这次运行里装过、没装成：版本与原因
    Failed(String, String),
    Version(String),
}

/// 解不出 JavaScript 挑战时换新的 yt-dlp 的结果（见 `Shared::bring_web_current`）。
struct Retry {
    /// 要拿它重新解析的：换上的新版本，或这期间别处换上的；没有时为 None
    extractor: Option<Arc<Extractor>>,
    /// 重新解析后仍解不出（或没得重新解析）时的原因
    unsolved: Freshness,
}

/// 解不出挑战时等 yt-dlp 换好最多等多久：下载约 11 MB 加 pip 安装与自检，慢的代理下也在几分钟内。
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(super) struct Web {
    /// 本平台没有组件时为 None
    components: Option<Components>,
    cache: PathBuf,
    state: watch::Sender<WebState>,
    /// 装好了才有；换了 yt-dlp 的版本时换掉（旧的进程随最后一个引用释放而结束）
    pub(super) extractor: Mutex<Option<Arc<Extractor>>>,
    /// 在安装：取消它的信号。与 `state` 的进出 [`WebState::Installing`]、[`WebState::Updating`] 一起在它的锁里改
    installing: Mutex<Option<CancellationToken>>,
    /// 后台最近一次查新版没成的原因（见 [`keep_web_current`]）；查成了、还没查过时为 None
    auto_check: watch::Sender<Option<String>>,
    /// 这次运行里更新没成的版本与原因：自动更新与解析时换新不再装它（用户手动更新照装）；装成了任何版本即清掉
    failed_update: Mutex<Option<(String, String)>>,
}

impl Web {
    /// 照 `config` 建；组件已装好时即可解析。
    pub(super) fn new(config: WebConfig, engine: Engine) -> Web {
        let cache = config.root.join("cache");
        let components = config
            .bundle
            .map(|bundle| Components::new(config.root, bundle, config.sources, engine));
        // 上次换组件时在两次改名之间中断的，先挪回去再看装没装好
        if let Some(components) = &components {
            components.recover();
        }
        // 应用升级换了随带的 Python 与 QuickJS-NG、而装着的 yt-dlp 照用得上（ABI 没变）：启动后在后台解出来（见
        // [`Shared::unpack_bundled_on_start`]），先显示在解
        let unpack = components.as_ref().is_some_and(|components| {
            matches!(components.installed(), Ok(None)) && components.bundled_missing()
        });
        let (state, extractor) = match components.as_ref().map(Components::installed) {
            None => (WebState::Unavailable, None),
            Some(Ok(None)) if unpack => (
                WebState::Installing(InstallProgress {
                    stage: Stage::Python,
                    bytes: 0,
                    total: None,
                }),
                None,
            ),
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
            installing: Mutex::new(unpack.then(CancellationToken::new)),
            auto_check: watch::Sender::new(None),
            failed_update: Mutex::new(None),
        }
    }

    /// 装好了在用的 yt-dlp 版本；没装好（含正在装）时为 None。
    fn version(&self) -> Option<String> {
        match &*self.state.borrow() {
            WebState::Ready(version)
            | WebState::UpdateFailed { version, .. }
            | WebState::Updating { version, .. } => Some(version.clone()),
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

    /// 开始安装、更新或换回：没在装时占住安装的锁、状态换成 `first`（两件在锁里一起做），返回取消它的信号；在装时为
    /// None。
    fn begin(&self, first: WebState) -> Option<CancellationToken> {
        let mut installing = lock(&self.installing);
        if installing.is_some() {
            return None;
        }
        let cancel = CancellationToken::new();
        *installing = Some(cancel.clone());
        self.state.send_replace(first);
        Some(cancel)
    }

    /// 安装、更新或换回做完：状态换成 `next`、放开安装的锁，两件在锁里一起做（别处看到状态不在装时即可开始装）。
    fn settle(&self, next: WebState) {
        let mut installing = lock(&self.installing);
        self.state.send_replace(next);
        *installing = None;
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

impl super::Shared {
    /// 启动时：随带的 Python 与 QuickJS-NG 要解出来时（见 [`Web::new`]）在后台解，解好即可用；取消时为没装，没解成时
    /// 为没装成（带上原因）。两种情形再装都先解出随带的，装着的 yt-dlp 自检过了即不联网（见 `Components::install`）。
    pub(super) fn unpack_bundled_on_start(self: &Arc<Self>) {
        let Some(cancel) = lock(&self.web.installing).clone() else {
            return;
        };
        let shared = self.clone();
        self.tracker.spawn(async move {
            let web = &shared.web;
            let components = web.components.as_ref().expect("要解出来即有组件");
            let next = match components.unpack_bundled(&cancel).await {
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
            web.settle(next);
        });
    }

    /// 见 [`Service::update_web`]，经 `proxy`。
    pub(super) fn start_web_update(
        self: &Arc<Self>,
        version: String,
        proxy: Proxy,
    ) -> Result<(), Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let web = &self.web;
        let current = web.version().ok_or(Error::WebNotReady)?;
        let before = web.state.borrow().clone();
        // 一开始就显示在换：到第一个字节之前（查 PyPI）也有好几秒
        let updating = {
            let current = current.clone();
            move |progress| WebState::Updating {
                version: current.clone(),
                progress,
            }
        };
        let cancel = web
            .begin(updating(InstallProgress {
                stage: Stage::Preparing,
                bytes: 0,
                total: None,
            }))
            .ok_or(Error::WebInstalling)?;
        let shared = self.clone();
        let _entered = self.runtime.enter();
        self.tracker.spawn(async move {
            let web = &shared.web;
            let components = web.components.as_ref().expect("装好了即有组件");
            let state = web.state.clone();
            let updated = components
                .update(
                    &proxy,
                    &version,
                    &|progress| {
                        state.send_replace(updating(progress));
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
                    *lock(&web.failed_update) = None;
                    WebState::Ready(version)
                }
                // 取消的不算失败：照原来的
                Err(hs_dlx_ytdlp::Error::Cancelled) => before,
                // 没换成：原来的照旧在用
                Err(error) => {
                    *lock(&web.failed_update) = Some((version, error.to_string()));
                    WebState::UpdateFailed {
                        version: current.clone(),
                        reason: format!("更新没成：{error}"),
                    }
                }
            };
            web.settle(next);
        });
        Ok(())
    }

    /// 经设置里的代理查设置里的渠道上最新的 yt-dlp，与在用的比，另给渠道；组件没装好、读不出设置、查不了时为原因。
    async fn newer_web(&self) -> Result<(Newer, Channel), String> {
        let installed = lock(&self.web.extractor)
            .as_ref()
            .map(|extractor| extractor.installed().clone())
            .ok_or_else(|| Error::WebNotReady.to_string())?;
        let settings = self.settings().map_err(|error| error.to_string())?;
        let components = self.web.components.as_ref().expect("装好了即有组件");
        let latest = components
            .latest(
                &settings.proxy,
                settings.web_channel,
                &CancellationToken::new(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let failed = lock(&self.web.failed_update).clone();
        let newer = match failed {
            // 渠道换了、在用的比渠道上最新的还新时也不动：只升不降
            _ if !is_newer(&latest, &installed.version) => Newer::None,
            _ if installed.skipped.as_deref() == Some(latest.as_str()) => Newer::Skipped(latest),
            Some((version, reason)) if version == latest => Newer::Failed(version, reason),
            _ => Newer::Version(latest),
        };
        Ok((newer, settings.web_channel))
    }

    /// 等在做的安装、更新或换回做完，最多 [`SETTLE_TIMEOUT`]；超时时为原因。
    async fn web_settled(&self) -> Result<WebState, String> {
        let mut state = self.web.state.subscribe();
        tokio::time::timeout(
            SETTLE_TIMEOUT,
            state.wait_for(|state| {
                !matches!(state, WebState::Installing(_) | WebState::Updating { .. })
            }),
        )
        .await
        .map_err(|_| format!("等 yt-dlp 装好超过 {} 分钟", SETTLE_TIMEOUT.as_secs() / 60))
        .map(|settled| settled.expect("持有 Shared 时状态的发送端还在").clone())
    }

    /// 解不出 JavaScript 挑战时把 yt-dlp 换成渠道上最新的：先等在做的安装做完，再查，有该换上的就更新（经设置里的代理）、
    /// 等它做完。要拿来重新解析的为换上的最新版，或这期间别处换上的（不是 `using` 的那个）。
    async fn bring_web_current(self: &Arc<Self>, using: &Arc<Extractor>) -> Retry {
        let swapped = |shared: &Self| {
            lock(&shared.web.extractor)
                .clone()
                .filter(|current| !Arc::ptr_eq(current, using))
        };
        let retry = |shared: &Self, unsolved| Retry {
            extractor: swapped(shared),
            unsolved,
        };
        if let Err(why) = self.web_settled().await {
            return retry(self, Freshness::Unknown(why));
        }
        let (newer, channel) = match self.newer_web().await {
            Ok(found) => found,
            Err(why) => return retry(self, Freshness::Unknown(format!("查新版没成：{why}"))),
        };
        let version = match newer {
            Newer::None => return retry(self, Freshness::Latest(channel)),
            Newer::Skipped(version) => return retry(self, Freshness::Skipped(version)),
            Newer::Failed(version, reason) => {
                return retry(
                    self,
                    Freshness::Unknown(format!("{version} 这次装不上：{reason}")),
                );
            }
            Newer::Version(version) => version,
        };
        let started = self
            .settings()
            .and_then(|settings| self.start_web_update(version.clone(), settings.proxy));
        match started {
            // 别处刚开始装：等它，装上的是不是这一版下面看
            Ok(()) | Err(Error::WebInstalling) => {}
            Err(error) => return retry(self, Freshness::Unknown(error.to_string())),
        }
        let why = match self.web_settled().await {
            Err(why) => why,
            Ok(_) => match lock(&self.web.extractor).clone() {
                Some(current) if current.installed().version == version => {
                    return Retry {
                        extractor: Some(current),
                        unsolved: Freshness::Latest(channel),
                    };
                }
                _ => lock(&self.web.failed_update)
                    .clone()
                    .filter(|(failed, _)| *failed == version)
                    .map_or_else(|| format!("没换上 {version}"), |(_, reason)| reason),
            },
        };
        retry(self, Freshness::Unknown(why))
    }
}

/// 启动后先查一次，之后每隔 `every`（只算醒着的时候）查一次渠道上的 yt-dlp，有更新的（跳过的、这次运行里装不上的
/// 除外）就换上；组件没装好、正在装时等状态变了再查。查的结果记在 [`Web::auto_check`]。服务释放时结束。
pub(super) async fn keep_web_current(shared: Weak<super::Shared>, every: Duration) {
    loop {
        let Some(strong) = shared.upgrade() else {
            return;
        };
        let mut state = strong.web.state.subscribe();
        let ready = matches!(
            *state.borrow_and_update(),
            WebState::Ready(_) | WebState::UpdateFailed { .. }
        );
        if !ready {
            drop(strong);
            // 发送端在 Shared 里：服务释放时这里随之返回，下一轮 upgrade 不到即结束
            let _changed = state.changed().await;
            continue;
        }
        let checked = match strong.newer_web().await {
            Ok((Newer::Version(version), _)) => strong
                .settings()
                .and_then(|settings| strong.start_web_update(version, settings.proxy))
                .or_else(|error| match error {
                    // 别处正好在装，或在关闭
                    Error::WebInstalling | Error::Closed => Ok(()),
                    error => Err(error.to_string()),
                }),
            Ok(_) => Ok(()),
            Err(why) => Err(why),
        };
        strong.web.auto_check.send_replace(checked.err());
        // 等的时候不持有服务
        drop(strong);
        tokio::time::sleep(every).await;
    }
}

impl Service {
    /// 记下网页视频的 yt-dlp 渠道：之后的安装与更新照它。
    pub fn set_web_channel(&self, channel: Channel) -> Result<(), Error> {
        self.shared.write(|tx| store::set_web_channel(tx, channel))
    }

    /// 记下解析时从哪个浏览器的哪个用户资料读 Cookie（None 为不读）：之后的解析与任务开始、继续照它。用户资料的目录
    /// 不是 UTF-8 时报 [`Error::InvalidInput`]。
    pub fn set_web_browser(&self, browser: Option<CookieSource>) -> Result<(), Error> {
        self.shared
            .write(|tx| store::set_web_browser(tx, browser.as_ref()))
    }

    /// 这台机器上有数据目录、读得了的浏览器（见 [`Browser::installed`]）。
    pub fn web_browsers(&self) -> Vec<Browser> {
        Browser::installed()
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
                // 句柄释放即丢弃这个任务，不另取消
                .latest(
                    &settings.proxy,
                    settings.web_channel,
                    &CancellationToken::new(),
                )
                .await?;
            Ok((latest != current).then_some(latest))
        })))
    }

    /// 把 yt-dlp 换成 `version`（经设置里的代理），进度与结果见 [`Service::web_state`]：装好、自检过了才换上，在用的
    /// 解析进程随之换成新的；没装成时照旧用原来的版本，状态回到它并带上原因。组件没装好时报 [`Error::WebNotReady`]，
    /// 正在装时报 [`Error::WebInstalling`]。
    pub fn update_web(&self, version: String) -> Result<(), Error> {
        let proxy = self.settings()?.proxy;
        self.shared.start_web_update(version, proxy)
    }

    /// 换回上一个 yt-dlp 版本（不联网，见 [`Components::rollback`]），结果见 [`Service::web_state`]：自检过了才换，在用
    /// 的解析进程随之换成它；没换成时照旧用原来的，状态回到它并带上原因。组件没装好时报 [`Error::WebNotReady`]，正在装时
    /// 报 [`Error::WebInstalling`]。
    pub fn rollback_web(&self) -> Result<(), Error> {
        let web = &self.shared.web;
        let current = web.version().ok_or(Error::WebNotReady)?;
        let before = web.state.borrow().clone();
        let cancel = web
            .begin(WebState::Updating {
                version: current.clone(),
                progress: InstallProgress {
                    stage: Stage::Checking,
                    bytes: 0,
                    total: None,
                },
            })
            .ok_or(Error::WebInstalling)?;
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
                // 取消的不算失败：照原来的
                Err(hs_dlx_ytdlp::Error::Cancelled) => before,
                Err(error) => WebState::UpdateFailed {
                    version: current,
                    reason: format!("换回没成：{error}"),
                },
            };
            web.settle(next);
        });
        Ok(())
    }

    /// 可换回的上一个 yt-dlp 版本（见 [`Service::rollback_web`]）；没装好或没有时为 None。
    pub fn web_previous(&self) -> Option<String> {
        lock(&self.shared.web.extractor)
            .as_ref()
            .and_then(|extractor| extractor.installed().previous.clone())
    }

    /// 后台最近一次查新的 yt-dlp 没成的原因（应用启动后先查一次，之后定期查，见 [`WebConfig::update_every`]）；查成了、
    /// 还没查过时为 None。
    pub fn web_auto_check(&self) -> watch::Receiver<Option<String>> {
        self.shared.web.auto_check.subscribe()
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
        // 一开始就显示在装：到第一个字节之前（查 PyPI、解出随带的）也有好几秒
        let Some(cancel) = web.begin(WebState::Installing(InstallProgress {
            stage: Stage::Preparing,
            bytes: 0,
            total: None,
        })) else {
            return Ok(());
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
                    state.send_replace(WebState::Installing(InstallProgress {
                        stage: Stage::Waiting,
                        bytes: 0,
                        total: None,
                    }));
                    // 先看取消：取消时 wait_idle 还没关上名额，这一个照常能用
                    let waited = tokio::select! {
                        biased;
                        () = cancel.cancelled() => false,
                        () = old.wait_idle() => true,
                    };
                    if !waited {
                        *lock(&web.extractor) = Some(old);
                        web.settle(before);
                        return;
                    }
                    old.stop_workers().await;
                }
                let repaired = components.repair(&proxy, channel, &report, &cancel).await;
                if let Err(error) = &repaired {
                    // 新的没就位时旧的照旧在（下载失败、取消），照原来的状态接着用；取消的不算失败
                    let kept = match components.installed() {
                        Ok(Some(installed)) if !matches!(before, WebState::Broken(_)) => {
                            Some(installed)
                        }
                        Ok(_) | Err(_) => None,
                    };
                    let cancelled = matches!(error, hs_dlx_ytdlp::Error::Cancelled);
                    let next = match (kept, &before) {
                        (Some(installed), _) => {
                            let version = installed.version.clone();
                            *lock(&web.extractor) = Some(Arc::new(Extractor::new(
                                installed,
                                web.cache.clone(),
                                EXTRACT_TIMEOUT,
                            )));
                            if cancelled {
                                before
                            } else {
                                WebState::UpdateFailed {
                                    version,
                                    reason: format!("重装没成：{error}"),
                                }
                            }
                        }
                        (None, _) if cancelled => before,
                        (None, WebState::Broken(_)) => {
                            WebState::Broken(format!("重装没成：{error}"))
                        }
                        (None, _) => WebState::Failed(error.to_string()),
                    };
                    web.settle(next);
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
                // 取消的不算失败：照原来的
                Err(hs_dlx_ytdlp::Error::Cancelled) => before,
                Err(error) => WebState::Failed(error.to_string()),
            };
            web.settle(next);
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
        // 只带 Cookie：别的请求头解析不用（只有通用解析器认得的网页要转嗅探页，那里用得上 Referer 等），Cookie 的值照
        // 新建时校验，不在解析时悄悄丢掉
        let cookies: Vec<(String, String)> = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
            .cloned()
            .collect();
        for (name, value) in &cookies {
            hs_m3u8_core::check_option_header(name, value).map_err(Error::InvalidInput)?;
        }
        super::web_task::check_headers(&cookies)?;
        let headers = cookies.as_slice();
        let extractor = lock(&self.shared.web.extractor)
            .clone()
            .ok_or(Error::WebNotReady)?;
        let settings = self.settings()?;
        let (proxy, preference) = (settings.proxy, settings.web_preference);
        let request = Request {
            cookies: super::web_task::cookies(&url, headers),
            url,
            generic: false,
            browser: settings.web_browser,
        };
        let shared = self.shared.clone();
        Ok(AbortOnDropHandle::new(self.runtime().spawn(async move {
            // 句柄释放时任务被丢弃，解析进程随 Worker 的释放杀掉；在做的更新是另起的任务，照常做完
            let extract = |extractor: Arc<Extractor>| {
                let (shared, request, proxy) = (shared.clone(), request.clone(), proxy.clone());
                async move {
                    extractor
                        .extract(&request, &proxy, &CancellationToken::new())
                        .await
                        .inspect_err(|error| {
                            if let ExtractError::Broken(reason) = error {
                                shared.web.broken(&extractor, reason.clone());
                            }
                        })
                }
            };
            let info = extract(extractor.clone()).await?;
            let found = choices(&info, preference);
            // 解不出挑战时列出的格式也多半下不了（要解的参数没解，被限速或拒绝）：先换新的
            if !info.challenge_failed {
                return Ok(Resolved {
                    info,
                    choices: found,
                    freshness: None,
                });
            }
            // QuickJS 不在了（被杀毒软件隔离等）时 yt-dlp 同样报解不出：组件坏了，要重装，换 yt-dlp 没用
            let quickjs = &extractor.installed().quickjs;
            if !quickjs.is_file() {
                let reason = format!(
                    "解 JavaScript 挑战用的 QuickJS 不在了：{}",
                    quickjs.display()
                );
                shared.web.broken(&extractor, reason.clone());
                return Err(ExtractError::Broken(reason));
            }
            // 多为网站换了播放器而这一版 yt-dlp 还不认得：换上新的再解析一次
            let retry = shared.bring_web_current(&extractor).await;
            let (info, found) = match retry.extractor {
                Some(newer) => {
                    let info = extract(newer).await?;
                    let found = choices(&info, preference);
                    (info, found)
                }
                None => (info, found),
            };
            let freshness = info.challenge_failed.then_some(retry.unsolved);
            Ok(Resolved {
                info,
                choices: found,
                freshness,
            })
        })))
    }
}
