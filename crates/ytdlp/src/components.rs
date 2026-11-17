//! 组件目录：随应用带的 Python 与 QuickJS-NG（见 [`Bundle`]）第一次用时解出来，不联网；yt-dlp 与它锁定版本的依赖
//! （yt-dlp 每个版本发布的 `pin` 与 `pin-curl-cffi` 两组，不要 mutagen）的 wheel 由核心下载（经应用的代理，含
//! SOCKS；能续传、有字节进度）、按 PyPI 给的 SHA-256 核对，再交给这个 Python 的 pip 离线装进 yt-dlp 的版本目录。每个
//! 版本自检过了才记成当前的，没装成的不影响在用的。
//!
//! 目录布局（`root` 下）：
//! - `python-<id>/`、`quickjs-<id>/`：解出来的随应用带的运行时（id 见 [`Bundled::id`]）；不是这一版应用带的在应用
//!   启动时删掉
//! - `yt-dlp-<版本>/`：pip 的 `--target` 目录；保留当前的与上一个，更早的在应用启动时（[`Components::sweep`]）删掉：
//!   安装、更新、重装时不删，换版本前起的解析进程可能还在用
//! - `state.json`：当前与上一个 yt-dlp 版本、装它时的 Python 版本；先写临时文件、落盘再改名
//! - `downloads/wheels/`：下载中与下好了的 wheel 与核心的任务目录（续传用），用过即删
//! - `staging/`：解包与 pip 安装的临时目录，每次安装前清空
//! - `trash-<名>-<纳秒>/`：换下来的组件目录（见 [`Components::replace_dir`]），应用启动时删掉
//! - `driver-<摘要>.py`：解析进程的驱动脚本，按内容命名，内容变了另写一个

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use hs_m3u8_core::{CancellationToken, Engine, FileOutputOptions, FileRequest, Proxy, Source, Url};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::Error;
use crate::archive;
use crate::bundle::{Bundle, Bundled};

/// 解析进程的驱动脚本：列依赖、挑 wheel、自检、解析。
pub(crate) const DRIVER: &str = include_str!("driver.py");
/// pip 离线安装、驱动脚本列依赖与挑 wheel 最多多久：不联网，只解包。
const PIP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// 自检最多多久：只导入模块、让 QuickJS-NG 算一个式子。
const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(60);
/// 换下来的组件目录挪到的名字的前缀（见 [`Components::replace_dir`]）。
const TRASH: &str = "trash-";
/// 命令的错误输出最多留多少字节写进错误信息：出错时末尾是原因。
const STDERR_TAIL: usize = 4096;
/// PyPI 的 JSON 接口响应最多多少字节：yt-dlp 的全部发布约 1.2 MB（2026-11）。
const JSON_LIMIT: u64 = 16 << 20;
/// yt-dlp 在 PyPI 上的名字。
const PROJECT: &str = "yt-dlp";

/// 从哪里取 yt-dlp。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    /// PyPI 的 JSON 接口的根（`<根>/<项目>/json` 为项目，`<根>/<项目>/<版本>/json` 为一个版本），以 `/` 结尾
    pub api: Url,
}

impl Sources {
    pub fn official() -> Sources {
        Sources {
            api: Url::parse("https://pypi.org/pypi/").expect("写死的地址"),
        }
    }
}

/// yt-dlp 的发布渠道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// 稳定版：约每月一版，YouTube 改了之后常滞后数周
    Stable,
    /// 每日构建（PyPI 上的 `.devN` 预发布）：修复最先到这里，yt-dlp 推荐用户用它
    Nightly,
}

/// 装好了的组件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub python: PathBuf,
    /// QuickJS-NG：yt-dlp 解 YouTube 的 JavaScript 挑战用
    pub quickjs: PathBuf,
    /// yt-dlp 与依赖所在的目录（pip 的 `--target`），由驱动脚本加进 `sys.path`
    pub site: PathBuf,
    /// yt-dlp 的版本，PEP 440 规范化的写法（与 PyPI 上的一致）
    pub version: String,
    /// 上一个 yt-dlp 版本（更新前的，留着可换回，见 [`Components::rollback`]）；没有时为 None
    pub previous: Option<String>,
    pub driver: PathBuf,
}

/// 随应用带的运行时。
#[derive(Clone, Copy)]
enum Package {
    Python,
    QuickJs,
}

/// 安装进行到哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 开始了、还没到下面任一步（清理临时目录、写驱动脚本）
    Preparing,
    /// 等在解析的解析完（重装前：在用的组件换不下）
    Waiting,
    /// 把随应用带的 Python 解出来
    Python,
    /// 把随应用带的 QuickJS-NG 解出来
    QuickJs,
    /// 查 PyPI 上 yt-dlp 的版本与它锁定的依赖、挑合本机的包
    Resolving,
    /// 下载 yt-dlp 与依赖的 wheel
    YtDlp,
    /// 用 pip 把 wheel 装进临时目录
    Setup,
    /// 自检
    Checking,
}

/// 安装的进度：`bytes`、`total` 为这一步要下载的字节（不知道总数时 `total` 为 None）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    pub stage: Stage,
    pub bytes: u64,
    pub total: Option<u64>,
}

/// `state.json` 的内容。
#[derive(Debug, Serialize, Deserialize)]
struct State {
    /// 当前的 yt-dlp 版本
    yt_dlp: String,
    /// 上一个版本；没有时为 None
    previous: Option<String>,
    /// 装 yt-dlp 时用的 Python（[`Bundled::id`]）：依赖里有按解释器版本编译的扩展模块，换了 Python 要重装
    python: String,
}

/// PyPI 上一个版本的发布：依赖声明与各个文件。
#[derive(Deserialize)]
struct Release {
    info: ReleaseInfo,
    urls: Vec<ReleaseFile>,
}

#[derive(Deserialize)]
struct ReleaseInfo {
    requires_dist: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Clone)]
struct ReleaseFile {
    filename: String,
    url: String,
    size: u64,
    digests: Digests,
}

#[derive(Deserialize, Serialize, Clone)]
struct Digests {
    sha256: String,
}

/// 驱动脚本 `requirements` 列出的一个依赖。
#[derive(Deserialize)]
struct Requirement {
    name: String,
    version: String,
}

pub struct Components {
    root: PathBuf,
    bundle: Bundle,
    sources: Sources,
    engine: Engine,
    /// 安装与更新一次只做一个
    busy: tokio::sync::Mutex<()>,
}

impl Components {
    /// 组件放在 `root` 下（不存在时第一次安装时建）；`engine` 下载组件、请求 PyPI 的接口。
    pub fn new(root: PathBuf, bundle: Bundle, sources: Sources, engine: Engine) -> Components {
        Components {
            root,
            bundle,
            sources,
            engine,
            busy: tokio::sync::Mutex::new(()),
        }
    }

    /// 装全了的组件；缺哪一样（含 yt-dlp 不是用锁定的 Python 装的）时为 None。驱动脚本按内容命名，应用升级后换了内容时
    /// 在这里写出新的。状态文件读不出、内容认不出、驱动脚本写不出时报 [`Error::Disk`]（[`Components::install`] 能修好）。
    pub fn installed(&self) -> Result<Option<Installed>, Error> {
        let Some(state) = self.state()? else {
            return Ok(None);
        };
        if state.python != self.bundle.python.id {
            return Ok(None);
        }
        let installed = self.installed_as(&state.yt_dlp, state.previous.clone());
        let present = [&installed.python, &installed.quickjs, &installed.site]
            .iter()
            .all(|path| path.exists());
        if !present {
            return Ok(None);
        }
        if !installed.driver.exists() {
            write_atomically_blocking(&installed.driver, DRIVER.as_bytes())?;
        }
        Ok(Some(installed))
    }

    /// 装上缺的：随应用带的 Python 与 QuickJS-NG（解出来，不联网）与 yt-dlp（经 `proxy`；状态文件记着的版本，没有或坏了时为
    /// `channel` 上最新的）；已装全时直接返回。状态文件坏了时照没有处理、装好后重写。`cancel` 取消时报
    /// [`Error::Cancelled`]，已下了的留着下次续传。
    pub async fn install(
        &self,
        proxy: &Proxy,
        channel: Channel,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<Installed, Error> {
        let _busy = self.busy.lock().await;
        if let Ok(Some(installed)) = self.installed() {
            return Ok(installed);
        }
        self.install_missing(proxy, channel, progress, cancel).await
    }

    /// 重装：随应用带的 Python 与 QuickJS-NG 重新解出来（不联网），yt-dlp（状态文件记着的版本，没有或坏了时为 `channel`
    /// 上最新的）重新下载、安装、自检，再换下在用的（文件被杀毒软件隔离或坏了、进程起不来）。每一样都是新的就位了才删
    /// 旧的：yt-dlp 下载失败（离线、代理坏了）时在用的 yt-dlp 不动。用着它们的解析进程要先停下（Windows 上挪不动在运行
    /// 的程序）；上一个 yt-dlp 版本照旧记着。
    pub async fn repair(
        &self,
        proxy: &Proxy,
        channel: Channel,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<Installed, Error> {
        let _busy = self.busy.lock().await;
        self.prepare().await?;
        for package in [Package::Python, Package::QuickJs] {
            self.ensure(package, true, progress).await?;
        }
        let version = match self.state() {
            Ok(Some(state)) => state.yt_dlp,
            Ok(None) | Err(_) => {
                progress(InstallProgress {
                    stage: Stage::Resolving,
                    bytes: 0,
                    total: None,
                });
                self.latest(proxy, channel).await?
            }
        };
        self.install_yt_dlp(proxy, &version, progress, cancel).await
    }

    /// 换回上一个 yt-dlp（更新后新版出了问题时）：不联网，自检过了才换，换后上一个是刚才在用的。没有上一个（没更新过，
    /// 或它的目录没了）时报 [`Error::NoPrevious`]，没装时报 [`Error::NotInstalled`]。
    pub async fn rollback(&self, cancel: &CancellationToken) -> Result<Installed, Error> {
        let _busy = self.busy.lock().await;
        self.switch_back(cancel).await
    }

    /// [`Components::rollback`] 拿到锁之后的部分。
    async fn switch_back(&self, cancel: &CancellationToken) -> Result<Installed, Error> {
        let current = self.installed()?.ok_or(Error::NotInstalled)?;
        let previous = current.previous.clone().ok_or(Error::NoPrevious)?;
        let back = self.installed_as(&previous, Some(current.version.clone()));
        if !back.site.is_dir() {
            return Err(Error::NoPrevious);
        }
        let found = self_test(
            &back.python,
            &back.driver,
            &back.site,
            &back.quickjs,
            cancel,
        )
        .await?;
        if found != previous {
            return Err(Error::SelfTest(format!(
                "要换回的是 {previous}，那个目录里装的是 {found}"
            )));
        }
        self.write_state(&previous, Some(current.version)).await?;
        Ok(back)
    }

    /// 删掉不再用的：不是这一版应用带的 Python 与 QuickJS-NG（与以前版本用的 deno）、当前与上一个以外的 yt-dlp、旧的
    /// 驱动脚本、换下来挪开的组件目录（原名不在时最近挪开的那个留着，等 [`Components::recover`] 挪回）；删不掉的留着，
    /// 下次再删。换版本之后用旧版本的解析进程可能还在跑（换版本前开始的任务），在没有任务运行时（应用启动时）调用。
    pub async fn sweep(&self) {
        let _busy = self.busy.lock().await;
        let Ok(mut entries) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        let mut names = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        // 要挪回而没挪成（见 recover）的留着，下次启动再挪
        let pending: Vec<String> = self.unplaced(&names).into_values().collect();
        // 状态文件读不出时只删挪开的，别的不知道哪个在用
        let keep = match self.state() {
            Ok(Some(state)) => Some([
                format!("python-{}", self.bundle.python.id),
                format!("quickjs-{}", self.bundle.quickjs.id),
                format!("yt-dlp-{}", state.yt_dlp),
                state
                    .previous
                    .map_or_else(String::new, |previous| format!("yt-dlp-{previous}")),
            ]),
            Ok(None) | Err(_) => None,
        };
        let driver = self.driver_path();
        for name in &names {
            let path = self.root.join(name);
            let trash = name.starts_with(TRASH) && !pending.contains(name);
            let stale_dir = keep.as_ref().is_some_and(|keep| {
                ["python-", "quickjs-", "deno-", "yt-dlp-"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
                    && !keep.contains(name)
            });
            let stale_driver = keep.is_some() && name.starts_with("driver-") && path != driver;
            // 清理失败不影响已装好的，下次启动时再删
            if trash || stale_dir {
                let _left = tokio::fs::remove_dir_all(&path).await;
            } else if stale_driver {
                let _left = tokio::fs::remove_file(&path).await;
            }
        }
    }

    /// 换组件目录时在两次改名之间中断（断电、崩溃）、原名没有新的放上的：把最近挪开的那个（完好的旧组件）挪回去，等于那次
    /// 换没开始。应用启动时、算组件装没装好之前调（同步，只是几次改名）；挪不回去（文件被占）的留给下次启动。
    pub fn recover(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        for (original, name) in self.unplaced(&names) {
            let _later = std::fs::rename(self.root.join(&name), self.root.join(original));
        }
    }

    /// `names`（组件目录里的）中原名不在的挪开的目录：原名对最近挪开的那个的名字。
    fn unplaced(&self, names: &[String]) -> BTreeMap<String, String> {
        let mut newest: BTreeMap<String, (String, u128)> = BTreeMap::new();
        for name in names {
            let Some((original, nanos)) = name
                .strip_prefix(TRASH)
                .and_then(|rest| rest.rsplit_once('-'))
                .and_then(|(original, nanos)| Some((original, nanos.parse::<u128>().ok()?)))
            else {
                continue;
            };
            if !self.root.join(original).exists()
                && newest.get(original).is_none_or(|(_, at)| *at < nanos)
            {
                newest.insert(original.to_owned(), (name.clone(), nanos));
            }
        }
        newest
            .into_iter()
            .map(|(original, (name, _))| (original, name))
            .collect()
    }

    /// [`Components::install`] 拿到锁、核对过没装全之后的部分。
    async fn install_missing(
        &self,
        proxy: &Proxy,
        channel: Channel,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<Installed, Error> {
        self.prepare().await?;
        for package in [Package::Python, Package::QuickJs] {
            self.ensure(package, false, progress).await?;
        }
        // 坏了的状态文件不挡着修：照没有处理
        let wanted = match self.state() {
            Ok(Some(state)) => Some(state.yt_dlp),
            Ok(None) | Err(_) => None,
        };
        let version = match wanted {
            Some(version) => version,
            None => {
                progress(InstallProgress {
                    stage: Stage::Resolving,
                    bytes: 0,
                    total: None,
                });
                self.latest(proxy, channel).await?
            }
        };
        self.install_yt_dlp(proxy, &version, progress, cancel).await
    }

    /// 装上 yt-dlp 的 `version`（PyPI 上的写法，经 `proxy`）、自检，过了才换成当前的，上一个留着；已是当前的直接返回，
    /// 是上一个（目录还在）时照 [`Components::rollback`] 换回、不重装（运行中的任务可能还在用它的目录），换回的自检
    /// 不过（它坏了）时才重装。Python 与 QuickJS-NG 要已解出来（见 [`Components::install`]），没装时报
    /// [`Error::NotInstalled`]。
    pub async fn update(
        &self,
        proxy: &Proxy,
        version: &str,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<Installed, Error> {
        let _busy = self.busy.lock().await;
        let current = self.installed()?.ok_or(Error::NotInstalled)?;
        if current.version == version {
            return Ok(current);
        }
        // 自检不过（导入失败、版本对不上：目录里的文件被隔离、删了）时照常重装，replace_dir 把坏的挪开；起不来、超时等
        // 一时的失败照样报出
        if current.previous.as_deref() == Some(version)
            && self.root.join(format!("yt-dlp-{version}")).is_dir()
        {
            match self.switch_back(cancel).await {
                Err(Error::SelfTest(broken)) => {
                    self.prepare().await?;
                    return self
                        .install_yt_dlp(proxy, version, progress, cancel)
                        .await
                        .map_err(|error| {
                            Error::SelfTest(format!(
                                "本机的 {version} 坏了（{broken}），重装也没成：{error}"
                            ))
                        });
                }
                switched => return switched,
            }
        }
        self.prepare().await?;
        self.install_yt_dlp(proxy, version, progress, cancel).await
    }

    /// `channel` 上最新的 yt-dlp 版本（经 `proxy` 查 PyPI）。
    pub async fn latest(&self, proxy: &Proxy, channel: Channel) -> Result<String, Error> {
        let body = self.get(proxy, &format!("{PROJECT}/json")).await?;
        latest_of(&body, channel)
    }

    fn installed_as(&self, version: &str, previous: Option<String>) -> Installed {
        Installed {
            python: executable(&self.root, "python", &self.bundle.python),
            quickjs: executable(&self.root, "quickjs", &self.bundle.quickjs),
            site: self.root.join(format!("yt-dlp-{version}")),
            version: version.to_owned(),
            previous,
            driver: self.driver_path(),
        }
    }

    /// 把装好的 `staging` 放到 `target`：`target` 在时先把它整个改名挪开（`trash-<名>-<纳秒>`，由
    /// [`Components::sweep`] 删），再放新的，放不上时挪回来。Windows 上目录里有文件被占时改名整体失败，旧的完好；不删
    /// 一半。
    async fn replace_dir(&self, staging: &Path, target: &Path) -> Result<(), Error> {
        let name = target
            .file_name()
            .expect("组件目录以名字结尾")
            .to_string_lossy();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let trash = self.root.join(format!("{TRASH}{name}-{nanos}"));
        let moved = match tokio::fs::rename(target, &trash).await {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(Error::disk(target, &error)),
        };
        if let Err(error) = tokio::fs::rename(staging, target).await {
            if moved && let Err(back) = tokio::fs::rename(&trash, target).await {
                // 旧的留在 trash 里：之后照没装处理
                return Err(Error::Disk(format!(
                    "{}：{error}；旧的挪不回来（在 {}）：{back}",
                    target.display(),
                    trash.display()
                )));
            }
            return Err(Error::disk(target, &error));
        }
        Ok(())
    }

    /// 记下当前（`yt_dlp`）与上一个 yt-dlp 版本：先写临时文件、落盘再改名。
    async fn write_state(&self, yt_dlp: &str, previous: Option<String>) -> Result<(), Error> {
        let state = State {
            yt_dlp: yt_dlp.to_owned(),
            previous,
            python: self.bundle.python.id.clone(),
        };
        write_atomically(
            &self.root.join("state.json"),
            &serde_json::to_vec(&state).expect("状态总能写成 JSON"),
        )
        .await
    }

    fn driver_path(&self) -> PathBuf {
        let digest = archive::sha256_bytes(DRIVER.as_bytes());
        self.root.join(format!("driver-{}.py", &digest[..12]))
    }

    fn state(&self) -> Result<Option<State>, Error> {
        let path = self.root.join("state.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| Error::Disk(format!("{} 认不出：{error}", path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::disk(&path, &error)),
        }
    }

    /// 经 `proxy` 取 PyPI 的 JSON 接口 `<根>/<path>`。
    async fn get(&self, proxy: &Proxy, path: &str) -> Result<Vec<u8>, Error> {
        let url = self
            .sources
            .api
            .join(path)
            .map_err(|error| Error::Index(format!("{path}：{error}")))?;
        let mut source = Source::new(url);
        source.http.proxy = proxy.clone();
        let call = hs_m3u8_core::Call {
            method: hs_m3u8_core::Method::Get,
            body: None,
            limit: JSON_LIMIT,
        };
        let reply = self
            .engine
            .call(&source, &call)
            .await
            .map_err(Error::Download)?;
        match reply.status {
            200 => Ok(reply.body),
            404 => Err(Error::Index(format!("PyPI 上没有 {path}"))),
            status => Err(Error::Index(format!("PyPI 回了 HTTP {status}（{path}）"))),
        }
    }

    /// 建好目录、清空上次没装完留下的临时目录、删掉 `downloads/` 里 wheel 以外的（以前版本下载的组件包与续传目录）、
    /// 写出驱动脚本。
    async fn prepare(&self) -> Result<(), Error> {
        let staging = self.root.join("staging");
        remove_dir(&staging).await?;
        let downloads = self.root.join("downloads");
        let listed = match tokio::fs::read_dir(&downloads).await {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error::disk(&downloads, &error)),
        };
        if let Some(mut entries) = listed {
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|error| Error::disk(&downloads, &error))?
            {
                if entry.file_name() == "wheels" {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    remove_dir(&path).await?;
                } else {
                    remove(&path).await?;
                }
            }
        }
        for dir in [&staging, &downloads.join("wheels")] {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|error| Error::disk(dir, &error))?;
        }
        let driver = self.driver_path();
        if !driver.exists() {
            write_atomically(&driver, DRIVER.as_bytes()).await?;
        }
        Ok(())
    }

    /// 经 `proxy` 下载 `url` 到 `path`，内容的 SHA-256 须为 `sha256`；进度经 `report`（这次下了的、总的字节）。`path` 上
    /// 已有（上次下好了没用完）时先核对：对得上直接用，对不上删掉重下。下到的对不上时删掉、报 [`Error::Mismatch`]。
    async fn fetch(
        &self,
        url: &str,
        sha256: &str,
        path: &Path,
        proxy: &Proxy,
        report: &(dyn Fn(u64, Option<u64>) + Sync),
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        if path.exists() {
            if checked(path, sha256).await?.is_ok() {
                return Ok(());
            }
            remove(path).await?;
        }
        let url = Url::parse(url).map_err(|error| Error::Index(format!("地址 {url}：{error}")))?;
        let mut source = Source::new(url);
        source.http.proxy = proxy.clone();
        let mut output = FileOutputOptions::new(path.to_path_buf());
        // 整个名字后加 `.hsdl`：`with_extension` 会截掉版本号里最后一个点之后的部分，换了版本还撞上同一个目录
        let mut work_dir = path.as_os_str().to_owned();
        work_dir.push(".hsdl");
        output.work_dir = Some(PathBuf::from(work_dir));
        let mut request = FileRequest::new(source, output);
        // GitHub、PyPI 的下载经重定向到每次签名不同的地址：以内容的哈希担保，换了地址也续传
        request.identity = Some(format!("sha256:{sha256}"));
        let job = self.engine.start_file(request).map_err(Error::Download)?;
        let control = job.control();
        let mut watching = control.progress();
        let waiting = job.wait();
        tokio::pin!(waiting);
        let result = loop {
            tokio::select! {
                result = &mut waiting => break result,
                changed = watching.changed() => {
                    if changed.is_ok() {
                        let now = watching.borrow_and_update().clone();
                        report(now.bytes, now.total);
                    }
                }
                () = cancel.cancelled() => {
                    control.cancel();
                    break waiting.await;
                }
            }
        };
        match result {
            Ok(_) => {}
            Err(hs_m3u8_core::Error::Cancelled) => return Err(Error::Cancelled),
            Err(error) => return Err(Error::Download(error)),
        }
        if let Err(actual) = checked(path, sha256).await? {
            remove(path).await?;
            return Err(Error::Mismatch {
                name: path.display().to_string(),
                expected: sha256.to_owned(),
                actual,
            });
        }
        Ok(())
    }

    /// 随应用带的 `package` 在 `<名>-<id>/` 下就位：没有、或里面缺了可执行文件（被杀毒软件隔离、被删）时，或 `again`
    /// （重装）时，从应用里解出来（不联网），新的就位了才删旧的。
    async fn ensure(
        &self,
        package: Package,
        again: bool,
        progress: &(dyn Fn(InstallProgress) + Sync),
    ) -> Result<(), Error> {
        let (bundled, name, stage) = match package {
            Package::Python => (&self.bundle.python, "python", Stage::Python),
            Package::QuickJs => (&self.bundle.quickjs, "quickjs", Stage::QuickJs),
        };
        if !again && executable(&self.root, name, bundled).is_file() {
            return Ok(());
        }
        progress(InstallProgress {
            stage,
            bytes: 0,
            total: None,
        });
        let staging = self.root.join("staging").join(name);
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|error| Error::disk(&staging, &error))?;
        let (bytes, into, file) = (
            bundled.bytes.clone(),
            staging.clone(),
            relative(&staging, &bundled.executable),
        );
        tokio::task::spawn_blocking(move || match package {
            Package::Python => archive::unpack_tar_zst(&bytes, &into),
            Package::QuickJs => write_executable(&file, &bytes),
        })
        .await
        .expect("解包不会 panic，运行时关闭时本任务也一起丢弃")?;
        if !relative(&staging, &bundled.executable).is_file() {
            return Err(Error::Archive(format!(
                "随应用带的 {name} 里没有 {}",
                bundled.executable
            )));
        }
        let target = self.root.join(format!("{name}-{}", bundled.id));
        self.replace_dir(&staging, &target).await
    }

    /// 下载 yt-dlp `version` 与它锁定的依赖的 wheel（各按 PyPI 给的 SHA-256 核对），用装好的 Python 的 pip 离线装进
    /// 临时目录，自检，改名成 `yt-dlp-<版本>/` 再记成当前的。
    async fn install_yt_dlp(
        &self,
        proxy: &Proxy,
        version: &str,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<Installed, Error> {
        if version.is_empty()
            || !version
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        {
            return Err(Error::Index(format!("版本号 {version:?} 认不出")));
        }
        let at = |stage| {
            progress(InstallProgress {
                stage,
                bytes: 0,
                total: None,
            });
        };
        let report = |bytes, total| {
            progress(InstallProgress {
                stage: Stage::YtDlp,
                bytes,
                total,
            });
        };
        at(Stage::Resolving);
        let python = executable(&self.root, "python", &self.bundle.python);
        let driver = self.driver_path();
        // yt-dlp 这个版本与它锁定的依赖，各自在 PyPI 上的文件
        let release = self.release(proxy, PROJECT, version).await?;
        let requires = release.info.requires_dist.clone().unwrap_or_default();
        let listed = run_driver(&python, &driver, "requirements", &requires, cancel).await?;
        let requirements: Vec<Requirement> = serde_json::from_slice(&listed)
            .map_err(|error| Error::Command(format!("驱动脚本列出的依赖认不出：{error}")))?;
        let mut files: BTreeMap<String, Vec<ReleaseFile>> = BTreeMap::new();
        files.insert(PROJECT.to_owned(), release.urls);
        for requirement in &requirements {
            let release = self
                .release(proxy, &requirement.name, &requirement.version)
                .await?;
            files.insert(requirement.name.clone(), release.urls);
        }
        let picked = run_driver(&python, &driver, "pick", &files, cancel)
            .await
            .map_err(|error| match error {
                Error::Exited(reason) => Error::Unsupported(reason),
                error => error,
            })?;
        let picked: BTreeMap<String, String> = serde_json::from_slice(&picked)
            .map_err(|error| Error::Command(format!("驱动脚本挑的包认不出：{error}")))?;
        let chosen: Vec<ReleaseFile> = picked
            .iter()
            .map(|(name, filename)| {
                files
                    .get(name)
                    .and_then(|listed| listed.iter().find(|file| file.filename == *filename))
                    .cloned()
                    .ok_or_else(|| Error::Command(format!("驱动脚本挑了不在列的 {filename}")))
            })
            .collect::<Result<_, _>>()?;
        // 逐个下载，进度按字节合计
        let total: u64 = chosen.iter().map(|file| file.size).sum();
        report(0, Some(total));
        let wheels = self.root.join("downloads").join("wheels");
        let mut done = 0;
        let mut paths = Vec::new();
        for file in &chosen {
            if file.filename.contains(['/', '\\']) || !file.filename.ends_with(".whl") {
                return Err(Error::Index(format!(
                    "PyPI 给的文件名 {} 认不出",
                    file.filename
                )));
            }
            let path = wheels.join(&file.filename);
            self.fetch(
                &file.url,
                &file.digests.sha256.to_ascii_lowercase(),
                &path,
                proxy,
                &|bytes, _| report(done + bytes, Some(total)),
                cancel,
            )
            .await?;
            done += file.size;
            paths.push(path);
        }
        at(Stage::Setup);
        let staging = self.root.join("staging").join("yt-dlp");
        let mut pip = command(&python);
        pip.args(pip_args(&staging)).args(&paths);
        run(pip, PIP_TIMEOUT, cancel)
            .await
            .map_err(|error| match error {
                Error::Command(reason) | Error::Exited(reason) => Error::Pip(reason),
                error => error,
            })?;
        progress(InstallProgress {
            stage: Stage::Checking,
            bytes: 0,
            total: None,
        });
        let quickjs = executable(&self.root, "quickjs", &self.bundle.quickjs);
        let found = self_test(&python, &driver, &staging, &quickjs, cancel).await?;
        if found != version {
            return Err(Error::SelfTest(format!(
                "要装的是 {version}，装上的是 {found}"
            )));
        }
        let target = self.root.join(format!("yt-dlp-{version}"));
        self.replace_dir(&staging, &target).await?;
        // 重装同一版本时上一个照旧；换了 Python 时上一个是用旧 Python 装的（带按解释器版本编译的扩展模块），不再算
        let previous = match self.state() {
            Ok(Some(state)) if state.python != self.bundle.python.id => None,
            Ok(Some(state)) if state.yt_dlp == version => state.previous,
            Ok(Some(state)) => Some(state.yt_dlp),
            Ok(None) | Err(_) => None,
        };
        self.write_state(version, previous.clone()).await?;
        for path in &paths {
            remove(path).await?;
        }
        Ok(self.installed_as(version, previous))
    }

    /// PyPI 上 `name` 的 `version` 这一版。
    async fn release(&self, proxy: &Proxy, name: &str, version: &str) -> Result<Release, Error> {
        let body = self.get(proxy, &format!("{name}/{version}/json")).await?;
        serde_json::from_slice(&body)
            .map_err(|error| Error::Index(format!("PyPI 回的 {name} {version} 认不出：{error}")))
    }
}

/// 随应用带的 `bundled` 解出来后的可执行文件。
fn executable(root: &Path, name: &str, bundled: &Bundled) -> PathBuf {
    relative(
        &root.join(format!("{name}-{}", bundled.id)),
        &bundled.executable,
    )
}

/// `dir` 下以 `/` 分隔的相对路径 `path`。
fn relative(dir: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(dir.to_path_buf(), |dir, part| dir.join(part))
}

/// 把 `bytes` 写成可执行文件 `path`（Unix 上权限 0o755）。阻塞，调用方放在阻塞线程上。
fn write_executable(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    std::fs::write(path, bytes).map_err(|error| Error::disk(path, &error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| Error::disk(path, &error))?;
    }
    Ok(())
}

/// `path` 的内容与 `sha256` 对得上为 Ok；对不上为实际的哈希。
async fn checked(path: &Path, sha256: &str) -> Result<Result<(), String>, Error> {
    let (path, expected) = (path.to_path_buf(), sha256.to_owned());
    tokio::task::spawn_blocking(move || {
        let actual = archive::sha256_of(&path)?;
        Ok(if actual == expected {
            Ok(())
        } else {
            Err(actual)
        })
    })
    .await
    .expect("算哈希不会 panic，运行时关闭时本任务也一起丢弃")
}

/// 删掉目录 `path`（不在时什么也不做）。
async fn remove_dir(path: &Path) -> Result<(), Error> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::disk(path, &error)),
    }
}

async fn remove(path: &Path) -> Result<(), Error> {
    tokio::fs::remove_file(path)
        .await
        .map_err(|error| Error::disk(path, &error))
}

/// 访问 `url` 时经的代理；直连时为 None。系统代理读不了时报 [`Error::Proxy`]。
pub(crate) fn proxy_for(proxy: &Proxy, url: &Url) -> Result<Option<Url>, Error> {
    match proxy {
        Proxy::Direct => Ok(None),
        Proxy::Url(proxy) => Ok(Some(proxy.clone())),
        Proxy::System => match hs_m3u8_core::system_proxy() {
            Ok(Some(system)) => Ok(system.proxy_for(url).cloned()),
            Ok(None) => Ok(None),
            Err(error) => Err(Error::Proxy(error.to_string())),
        },
    }
}

/// 起子进程的命令：不读环境变量里的代理（代理只照应用的设置给）；Windows 上不弹控制台窗口。
pub(crate) fn command(program: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"] {
        command.env_remove(name);
        command.env_remove(name.to_ascii_lowercase());
    }
    #[cfg(windows)]
    {
        /// CREATE_NO_WINDOW
        const NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(NO_WINDOW);
    }
    command
}

/// pip 离线装 `target` 的参数（后面接各个 wheel 文件）：隔离模式（不读 PYTHON* 与 PIP_* 环境变量、用户的
/// site-packages 与 pip 配置），不联网、不解依赖（依赖已列全）、不用缓存。
fn pip_args(target: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "-I",
        "-m",
        "pip",
        "--isolated",
        "install",
        "--no-input",
        "--disable-pip-version-check",
        "--no-cache-dir",
        "--no-color",
        "--progress-bar",
        "off",
        "--no-index",
        "--no-deps",
    ]
    .map(str::to_owned)
    .into();
    args.push("--target".to_owned());
    args.push(target.to_string_lossy().into_owned());
    args
}

/// 运行驱动脚本的命令 `name`，标准输入为 `input` 的 JSON，返回标准输出。
async fn run_driver(
    python: &Path,
    driver: &Path,
    name: &str,
    input: &impl Serialize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    let mut script = command(python);
    script.arg("-I").arg("-X").arg("utf8").arg(driver).arg(name);
    let input = serde_json::to_vec(input).expect("驱动脚本的输入总能写成 JSON");
    run_with(script, Some(input), PIP_TIMEOUT, cancel).await
}

/// 自检：驱动脚本导入 `site` 里的 yt-dlp 与解 JavaScript 挑战的 yt-dlp-ejs，`quickjs` 算出一个式子；返回 yt-dlp 规范化的
/// 版本。
async fn self_test(
    python: &Path,
    driver: &Path,
    site: &Path,
    quickjs: &Path,
    cancel: &CancellationToken,
) -> Result<String, Error> {
    let mut selftest = command(python);
    selftest
        .arg("-I")
        .arg("-X")
        .arg("utf8")
        .arg(driver)
        .arg("selftest")
        .arg(site);
    let out = run(selftest, SELF_TEST_TIMEOUT, cancel)
        .await
        .map_err(|error| match error {
            Error::Exited(reason) => Error::SelfTest(reason),
            error => error,
        })?;
    #[derive(Deserialize)]
    struct Report {
        yt_dlp: String,
    }
    let report: Report = serde_json::from_slice(&out)
        .map_err(|error| Error::SelfTest(format!("驱动脚本的输出认不出：{error}")))?;
    let mut sum = command(quickjs);
    sum.args(["-e", "print(6 * 7)"]);
    let out = run(sum, SELF_TEST_TIMEOUT, cancel)
        .await
        .map_err(|error| match error {
            Error::Exited(reason) => Error::SelfTest(format!("QuickJS-NG 运行不了：{reason}")),
            error => error,
        })?;
    if String::from_utf8_lossy(&out).trim() != "42" {
        return Err(Error::SelfTest("QuickJS-NG 算错了式子".to_owned()));
    }
    Ok(report.yt_dlp)
}

/// 运行 `command` 到结束，返回标准输出（见 [`run_with`]）。
pub(crate) async fn run(
    command: tokio::process::Command,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    run_with(command, None, timeout, cancel).await
}

/// 运行 `command` 到结束，标准输入为 `input`（None 时为空），返回标准输出；超过 `timeout` 或 `cancel` 时杀掉进程
/// （后者报 [`Error::Cancelled`]），起不来、超时报 [`Error::Command`]，退出码不为 0 时报 [`Error::Exited`]（带错误
/// 输出的末尾）。
async fn run_with(
    mut command: tokio::process::Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    let mut child = command
        .spawn()
        .map_err(|error| Error::Command(format!("{program} 起不来：{error}")))?;
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take().expect("上面设了 piped");
    let mut stderr = child.stderr.take().expect("上面设了 piped");
    let writing = async {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            stdin.write_all(&input).await?;
            // 关掉标准输入，对方读到结尾
            stdin.shutdown().await?;
        }
        Ok::<(), std::io::Error>(())
    };
    let reading = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (written, read_out, read_err) = tokio::join!(
            writing,
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        );
        written.and(read_out).and(read_err).map(|_| (out, err))
    };
    let finished = async {
        let read = reading.await;
        let status = child.wait().await;
        (read, status)
    };
    let (read, status) = tokio::select! {
        done = tokio::time::timeout(timeout, finished) => match done {
            Ok(done) => done,
            // 超时：丢下 finished 即丢下子进程，kill_on_drop 杀掉它
            Err(_) => return Err(Error::Command(format!("{program} 超过 {} 秒没结束", timeout.as_secs()))),
        },
        () = cancel.cancelled() => return Err(Error::Cancelled),
    };
    let (out, err) = read.map_err(|error| Error::Command(format!("与 {program} 收发：{error}")))?;
    let status = status.map_err(|error| Error::Command(format!("等 {program} 结束：{error}")))?;
    if !status.success() {
        let text = String::from_utf8_lossy(&err);
        let start = text.len().saturating_sub(STDERR_TAIL);
        let start = (start..text.len())
            .find(|index| text.is_char_boundary(*index))
            .unwrap_or(text.len());
        return Err(Error::Exited(format!(
            "{program} 失败（{status}）：{}",
            text[start..].trim()
        )));
    }
    Ok(out)
}

/// 先写 `path` 旁的临时文件、落盘，再改名：读的一方看到的要么是旧的、要么是完整的新内容，断电后也不会是半截。
async fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let (path, bytes) = (path.to_path_buf(), bytes.to_vec());
    tokio::task::spawn_blocking(move || write_atomically_blocking(&path, &bytes))
        .await
        .expect("写文件不会 panic，运行时关闭时本任务也一起丢弃")
}

/// [`write_atomically`] 的阻塞版本。
fn write_atomically_blocking(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write as _;
    let temporary = path.with_extension("tmp");
    let mut file =
        std::fs::File::create(&temporary).map_err(|error| Error::disk(&temporary, &error))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| Error::disk(&temporary, &error))?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(|error| Error::disk(path, &error))
}

/// PyPI 的 JSON 接口回的 `body` 里 `channel` 上最新的版本：稳定版为 `info.version`；每日构建为上传最晚的 `.dev` 版本
/// （没有比稳定版新的时为稳定版）。
fn latest_of(body: &[u8], channel: Channel) -> Result<String, Error> {
    #[derive(Deserialize)]
    struct Project {
        info: Info,
        releases: BTreeMap<String, Vec<File>>,
    }
    #[derive(Deserialize)]
    struct Info {
        version: String,
    }
    #[derive(Deserialize)]
    struct File {
        upload_time_iso_8601: String,
        yanked: bool,
    }
    let project: Project = serde_json::from_slice(body)
        .map_err(|error| Error::Index(format!("PyPI 回的 yt-dlp 发布信息认不出：{error}")))?;
    let stable = project.info.version;
    if channel == Channel::Stable {
        return Ok(stable);
    }
    // 各版本最早的上传时间：ISO 8601 的 UTC 时间按字符串比即按时间比
    let uploaded = |files: &[File]| {
        files
            .iter()
            .filter(|file| !file.yanked)
            .map(|file| file.upload_time_iso_8601.as_str())
            .min()
            .map(str::to_owned)
    };
    let stable_at = project
        .releases
        .get(&stable)
        .and_then(|files| uploaded(files));
    let newest = project
        .releases
        .iter()
        .filter(|(version, _)| version.contains(".dev"))
        .filter_map(|(version, files)| uploaded(files).map(|at| (at, version)))
        .max();
    match (newest, stable_at) {
        (Some((at, version)), Some(stable_at)) if at > stable_at => Ok(version.clone()),
        (Some((_, version)), None) => Ok(version.clone()),
        _ => Ok(stable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pip_installs_offline_into_the_target() {
        let args = pip_args(Path::new("/c/staging/yt-dlp"));
        let joined = args.join(" ");
        assert!(
            joined.starts_with("-I -m pip --isolated install "),
            "{joined}"
        );
        for part in ["--no-index", "--no-deps", "--target /c/staging/yt-dlp"] {
            assert!(joined.contains(part), "缺 {part}：{joined}");
        }
        assert!(!joined.contains("--proxy") && !joined.contains("--index-url"));
    }

    #[test]
    fn the_latest_release_follows_the_channel() {
        let body = serde_json::json!({
            "info": { "version": "2026.10.26" },
            "releases": {
                "2026.10.21": [{ "upload_time_iso_8601": "2026-10-21T10:00:00.000000Z", "yanked": false }],
                "2026.10.26": [{ "upload_time_iso_8601": "2026-10-26T22:25:00.000000Z", "yanked": false }],
                "2026.10.25.1.dev0": [{ "upload_time_iso_8601": "2026-10-25T01:00:00.000000Z", "yanked": false }],
                "2026.11.16.232655.dev0": [{ "upload_time_iso_8601": "2026-11-16T23:30:00.000000Z", "yanked": false }],
                "2026.11.17.1.dev0": [{ "upload_time_iso_8601": "2026-11-17T01:00:00.000000Z", "yanked": true }]
            }
        })
        .to_string();
        assert_eq!(
            latest_of(body.as_bytes(), Channel::Stable).unwrap(),
            "2026.10.26"
        );
        assert_eq!(
            latest_of(body.as_bytes(), Channel::Nightly).unwrap(),
            "2026.11.16.232655.dev0"
        );
        // 每日构建都比稳定版旧时用稳定版
        let older = serde_json::json!({
            "info": { "version": "2026.10.26" },
            "releases": {
                "2026.10.26": [{ "upload_time_iso_8601": "2026-10-26T22:25:00.000000Z", "yanked": false }],
                "2026.10.25.1.dev0": [{ "upload_time_iso_8601": "2026-10-25T01:00:00.000000Z", "yanked": false }]
            }
        })
        .to_string();
        assert_eq!(
            latest_of(older.as_bytes(), Channel::Nightly).unwrap(),
            "2026.10.26"
        );
        assert!(matches!(
            latest_of(b"{}", Channel::Stable),
            Err(Error::Index(_))
        ));
    }
}
