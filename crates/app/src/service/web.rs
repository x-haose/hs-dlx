//! 网页视频的组件与解析（yt-dlp）：组件到用户要用时才安装，安装的进度与组件的状态经 watch 通道给界面；解析在常驻的
//! 进程里做（见 [`hs_dlx_ytdlp::Extractor`]）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// 状态 `state` 下装好了在用的 yt-dlp 版本；没装好（含正在装）时为 None。
fn version_of(state: &WebState) -> Option<String> {
    match state {
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

/// 渠道上有没有该换上的 yt-dlp。
enum Newer {
    /// 在用的不比最新的旧
    None,
    /// 最新的是换回时换下的（见 [`hs_dlx_ytdlp::Installed::skipped`]）
    Skipped(String),
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
    /// 后台最近一次查新版没成的原因（见 [`keep_web_current`]）；查成了、换上了（[`Web::settle`] 到 Ready）、还没查过时
    /// 为 None
    auto_check: watch::Sender<Option<String>>,
    /// 这次运行里在本机装不上的版本与原因（见 [`hs_dlx_ytdlp::Error::version_unusable`]，下载等一时的失败不记）：解析时
    /// 换新不再装它；换上了任何版本（[`Web::settle`] 到 Ready）即清掉
    failed_update: Mutex<Option<(String, String)>>,
    /// 在装的期间在用的解析进程被判坏了：原因。装完时在用的还没换上新的就落为 [`WebState::Broken`]（见 [`Web::settle`]）
    broke: Mutex<Option<String>>,
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
            broke: Mutex::new(None),
        }
    }

    /// 装好了在用的 yt-dlp 版本；没装好（含正在装）时为 None。
    fn version(&self) -> Option<String> {
        version_of(&self.state.borrow())
    }

    /// `extractor` 的解析进程起不来（组件的文件被隔离或坏了）：它还是在用的那个时不再拿它解析，状态记为坏了（带上
    /// 原因），由用户重装；正在装时先记下原因，装完时落定（见 [`Web::settle`]）；已换掉（重装、更新过）时什么也不做。
    /// 在用它的任务照样以组件坏了停下。锁的次序：先安装的锁，再解析进程的。
    pub(super) fn broken(&self, extractor: &Arc<Extractor>, reason: String) {
        let installing = lock(&self.installing);
        let mut current = lock(&self.extractor);
        if current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, extractor))
        {
            *current = None;
            if installing.is_some() {
                *lock(&self.broke) = Some(reason);
            } else {
                self.state.send_replace(WebState::Broken(reason));
            }
        }
    }

    /// 开始安装、更新或换回：在安装的锁里核对没在关闭（报 [`Error::Closed`]）、没在装（报 [`Error::WebInstalling`]），按
    /// 此刻的状态由 `first` 定开始时的状态（它报的错照报），占住锁、换上状态；返回取消的信号与开始前的状态。
    fn begin(
        &self,
        closed: &AtomicBool,
        first: impl FnOnce(&WebState) -> Result<WebState, Error>,
    ) -> Result<(CancellationToken, WebState), Error> {
        let mut installing = lock(&self.installing);
        if closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        if installing.is_some() {
            return Err(Error::WebInstalling);
        }
        let before = self.state.borrow().clone();
        let first = first(&before)?;
        let cancel = CancellationToken::new();
        *installing = Some(cancel.clone());
        self.state.send_replace(first);
        Ok((cancel, before))
    }

    /// 安装、更新或换回做完：状态换成 `next`、放开安装的锁，两件在锁里一起做（别处看到状态不在装时即可开始装）。期间
    /// 在用的被判坏了、又没换上新的时落为 [`WebState::Broken`]；落为 Ready 时清掉装不上的版本与后台查新版没成的原因。
    fn settle(&self, next: WebState) {
        let mut installing = lock(&self.installing);
        let broke = lock(&self.broke).take();
        let next = match broke {
            Some(reason) if lock(&self.extractor).is_none() => WebState::Broken(reason),
            _ => next,
        };
        if matches!(next, WebState::Ready(_)) {
            *lock(&self.failed_update) = None;
            self.auto_check.send_replace(None);
        }
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
        let web = &self.web;
        // 一开始就显示在换：到第一个字节之前（查 PyPI）也有好几秒
        let (cancel, before) = web.begin(&self.closed, |state| {
            Ok(WebState::Updating {
                version: version_of(state).ok_or(Error::WebNotReady)?,
                progress: InstallProgress {
                    stage: Stage::Preparing,
                    bytes: 0,
                    total: None,
                },
            })
        })?;
        let current = version_of(&before).expect("begin 核对过装好了");
        let updating = {
            let current = current.clone();
            move |progress| WebState::Updating {
                version: current.clone(),
                progress,
            }
        };
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
                    WebState::Ready(version)
                }
                // 取消的不算失败：照原来的
                Err(hs_dlx_ytdlp::Error::Cancelled) => before,
                // 没换成：原来的照旧在用
                Err(error) => {
                    if error.version_unusable() {
                        *lock(&web.failed_update) = Some((version, error.to_string()));
                    }
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

    /// 经设置里的代理查设置里的渠道上最新的 yt-dlp，与在用的比，另给在用的版本；组件没装好、读不出设置、查不了、版本号
    /// 认不出时为原因。
    async fn newer_web(&self) -> Result<(Newer, String), String> {
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
        let newer = is_newer(&latest, &installed.version).ok_or_else(|| {
            format!(
                "认不出版本号：渠道上最新的是 {latest}，在用的是 {}",
                installed.version
            )
        })?;
        let found = if !newer {
            // 渠道换了、在用的比渠道上最新的还新时也不动：只升不降
            Newer::None
        } else if installed.skipped.as_deref() == Some(latest.as_str()) {
            Newer::Skipped(latest)
        } else {
            Newer::Version(latest)
        };
        Ok((found, installed.version))
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

    /// 解不出 JavaScript 挑战时把 yt-dlp 换成渠道上最新的：先等在做的安装做完，再查，有该换上的（这次运行里装不上的
    /// 除外）就更新（经设置里的代理）、等它做完。要拿来重新解析的为换上的最新版，或这期间别处换上的（不是 `using`）。
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
        let (newer, current) = match self.newer_web().await {
            Ok(found) => found,
            Err(why) => return retry(self, Freshness::Unknown(format!("查新版没成：{why}"))),
        };
        let version = match newer {
            Newer::None => return retry(self, Freshness::Latest(Channel::of(&current))),
            Newer::Skipped(version) => return retry(self, Freshness::Skipped(version)),
            Newer::Version(version) => version,
        };
        let failed = lock(&self.web.failed_update)
            .clone()
            .filter(|(failed, _)| *failed == version);
        if let Some((_, reason)) = failed {
            return retry(
                self,
                Freshness::Unknown(format!("{version} 在本机装不上：{reason}")),
            );
        }
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
            Ok(settled) => match lock(&self.web.extractor).clone() {
                Some(current) if current.installed().version == version => {
                    return Retry {
                        extractor: Some(current),
                        unsolved: Freshness::Latest(Channel::of(&version)),
                    };
                }
                _ => match settled {
                    WebState::UpdateFailed { reason, .. } | WebState::Broken(reason) => reason,
                    _ => format!("没换上 {version}"),
                },
            },
        };
        retry(self, Freshness::Unknown(why))
    }
}

/// 启动后先查一次，之后每隔 `every`（只算醒着的时候）查一次渠道上的 yt-dlp，有更新的（跳过的除外）就换上；组件没装好、
/// 正在装时等状态变了再查。查的结果记在 [`Web::auto_check`]。服务释放时结束。
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
        let checked = if ready {
            match strong.newer_web().await {
                Ok((Newer::Version(version), _)) => strong
                    .settings()
                    .and_then(|settings| strong.start_web_update(version, settings.proxy))
                    .map(|()| true)
                    .or_else(|error| match error {
                        // 别处正好在装：等它装完再看
                        Error::WebInstalling => Ok(false),
                        Error::Closed => Err(None),
                        error => Err(Some(error.to_string())),
                    }),
                Ok(_) => Ok(true),
                Err(why) => Err(Some(why)),
            }
        } else {
            Ok(false)
        };
        match checked {
            // 在关闭
            Err(None) => return,
            Err(Some(why)) => {
                strong.web.auto_check.send_replace(Some(why));
            }
            Ok(true) => {
                strong.web.auto_check.send_replace(None);
            }
            Ok(false) => {
                drop(strong);
                // 发送端在 Shared 里：服务释放时这里随之返回，下一轮 upgrade 不到即结束
                let _changed = state.changed().await;
                continue;
            }
        }
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

    /// 经设置里的代理查设置里的渠道上最新的 yt-dlp：比装着的新时为它，不比装着的新（含切换渠道后在用的反而更新）时为
    /// None，版本号认不出时报 [`hs_dlx_ytdlp::Error::Index`]。组件没装好时报 [`Error::WebNotReady`]；句柄在任何执行器上
    /// 都可等待。
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
            let newer = is_newer(&latest, &current).ok_or_else(|| {
                hs_dlx_ytdlp::Error::Index(format!(
                    "认不出版本号：渠道上最新的是 {latest}，在用的是 {current}"
                ))
            })?;
            Ok(newer.then_some(latest))
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
        let (cancel, before) = web.begin(&self.shared.closed, |state| {
            Ok(WebState::Updating {
                version: version_of(state).ok_or(Error::WebNotReady)?,
                progress: InstallProgress {
                    stage: Stage::Checking,
                    bytes: 0,
                    total: None,
                },
            })
        })?;
        let current = version_of(&before).expect("begin 核对过装好了");
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
        let repairing =
            |state: &WebState| !matches!(state, WebState::Missing | WebState::Failed(_));
        // 一开始就显示在装：到第一个字节之前（查 PyPI、解出随带的）也有好几秒
        let begun = web.begin(&self.shared.closed, |state| {
            if repairing(state)
                && lock(&self.shared.running)
                    .values()
                    .any(|running| matches!(running.control, super::Control::Web(_)))
            {
                return Err(Error::WebInUse);
            }
            Ok(WebState::Installing(InstallProgress {
                stage: Stage::Preparing,
                bytes: 0,
                total: None,
            }))
        });
        let (cancel, before) = match begun {
            Ok(begun) => begun,
            Err(Error::WebInstalling) => return Ok(()),
            Err(error) => return Err(error),
        };
        let repair = repairing(&before);
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use hs_dlx_ytdlp::Installed;

    use super::*;

    /// 没有组件的 [`Web`]，摆上一个在用的 yt-dlp `version`（解析进程不起：建 [`Extractor`] 不起进程）。
    fn web_using(version: &str) -> (Web, Arc<Extractor>) {
        let root = std::env::temp_dir().join("hs-dlx-web-state");
        let web = Web::new(
            WebConfig {
                root: root.clone(),
                bundle: None,
                sources: Sources::official(),
                update_every: None,
            },
            Engine::new(NonZeroUsize::new(1).expect("1 不为 0")),
        );
        let extractor = Arc::new(Extractor::new(
            Installed {
                python: root.join("python"),
                quickjs: root.join("qjs"),
                site: root.join("site"),
                version: version.to_owned(),
                previous: None,
                skipped: None,
                driver: root.join("driver.py"),
            },
            root.join("cache"),
            EXTRACT_TIMEOUT,
        ));
        *lock(&web.extractor) = Some(extractor.clone());
        web.state.send_replace(WebState::Ready(version.to_owned()));
        (web, extractor)
    }

    fn updating(state: &WebState) -> Result<WebState, Error> {
        Ok(WebState::Updating {
            version: version_of(state).ok_or(Error::WebNotReady)?,
            progress: InstallProgress {
                stage: Stage::Preparing,
                bytes: 0,
                total: None,
            },
        })
    }

    /// 在装时与关闭后不能开始；开始时给出开始前的状态。
    #[test]
    fn only_one_change_starts_and_none_after_closing() {
        let (web, _) = web_using("1");
        let open = AtomicBool::new(false);
        let (_, before) = web.begin(&open, updating).unwrap();
        assert_eq!(before, WebState::Ready("1".to_owned()));
        assert!(matches!(
            web.begin(&open, updating),
            Err(Error::WebInstalling)
        ));
        web.settle(before);
        let closed = AtomicBool::new(true);
        assert!(matches!(web.begin(&closed, updating), Err(Error::Closed)));
        assert_eq!(*web.state.borrow(), WebState::Ready("1".to_owned()));
    }

    /// 换版本期间在用的被判坏了：更新没成或取消时落为坏了，不回到照常可用；换上了新的则照新的。
    #[test]
    fn a_component_broken_during_an_update_stays_broken_unless_replaced() {
        let open = AtomicBool::new(false);
        for failed in [
            WebState::UpdateFailed {
                version: "1".to_owned(),
                reason: "更新没成".to_owned(),
            },
            WebState::Ready("1".to_owned()),
        ] {
            let (web, extractor) = web_using("1");
            web.begin(&open, updating).unwrap();
            web.broken(&extractor, "QuickJS 不在了".to_owned());
            assert!(matches!(*web.state.borrow(), WebState::Updating { .. }));
            web.settle(failed);
            assert_eq!(
                *web.state.borrow(),
                WebState::Broken("QuickJS 不在了".to_owned())
            );
        }
        let (web, extractor) = web_using("1");
        web.begin(&open, updating).unwrap();
        web.broken(&extractor, "QuickJS 不在了".to_owned());
        let (_, replaced) = web_using("2");
        *lock(&web.extractor) = Some(replaced);
        web.settle(WebState::Ready("2".to_owned()));
        assert_eq!(*web.state.borrow(), WebState::Ready("2".to_owned()));
    }

    /// 换上了（落为 Ready）即清掉后台查新版没成的原因与装不上的版本；没换上时留着。
    #[test]
    fn settling_ready_clears_stale_failures() {
        let open = AtomicBool::new(false);
        let (web, _) = web_using("1");
        web.auto_check.send_replace(Some("连不上 PyPI".to_owned()));
        *lock(&web.failed_update) = Some(("2".to_owned(), "pip 装不上".to_owned()));
        web.begin(&open, updating).unwrap();
        web.settle(WebState::UpdateFailed {
            version: "1".to_owned(),
            reason: "更新没成".to_owned(),
        });
        assert!(web.auto_check.borrow().is_some());
        assert!(lock(&web.failed_update).is_some());
        web.begin(&open, updating).unwrap();
        web.settle(WebState::Ready("3".to_owned()));
        assert_eq!(*web.auto_check.borrow(), None);
        assert_eq!(*lock(&web.failed_update), None);
    }
}
