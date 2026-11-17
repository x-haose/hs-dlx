//! 组件目录：按需下载、核对并解开 Python 与 deno（版本随应用锁定，见 [`Pins`]）；yt-dlp 与它锁定版本的依赖（yt-dlp
//! 每个版本发布的 `pin` 与 `pin-curl-cffi` 两组，不要 mutagen）的 wheel 也由核心下载、按 PyPI 给的 SHA-256 核对，
//! 再交给这个 Python 的 pip 离线装进 yt-dlp 的版本目录：下载与组件包一样经应用的代理（含 SOCKS）、能续传、有字节
//! 进度，pip 不联网。每个版本自检过了才记成当前的，没装成的不影响在用的。
//!
//! 目录布局（`root` 下）：
//! - `python-<版本>/`、`deno-<版本>/`：解开的组件包；不是锁定版本的在装好之后删掉
//! - `yt-dlp-<版本>/`：pip 的 `--target` 目录；保留当前的与上一个，更早的在安装后与应用启动时（[`Components::sweep`]）
//!   删掉：更新时不删，换版本前起的解析进程可能还在用
//! - `state.json`：当前与上一个 yt-dlp 版本、装它时的 Python 版本；先写临时文件、落盘再改名
//! - `downloads/`：下载中与下好了的组件包、wheel 与核心的任务目录（续传用），用过即删
//! - `staging/`：解包与 pip 安装的临时目录，每次安装前清空
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
use crate::pins::{Pinned, Pins};

/// 解析进程的驱动脚本：列依赖、挑 wheel、自检、解析。
pub(crate) const DRIVER: &str = include_str!("driver.py");
/// pip 离线安装、驱动脚本列依赖与挑 wheel 最多多久：不联网，只解包。
const PIP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// 自检最多多久：只导入模块、运行 deno 报版本。
const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(60);
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
    pub deno: PathBuf,
    /// yt-dlp 与依赖所在的目录（pip 的 `--target`），由驱动脚本加进 `sys.path`
    pub site: PathBuf,
    /// yt-dlp 的版本，PEP 440 规范化的写法（与 PyPI 上的一致）
    pub version: String,
    pub driver: PathBuf,
}

/// 安装进行到哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Python,
    Deno,
    /// 下载 yt-dlp 与依赖的 wheel 并装上
    YtDlp,
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
    /// 装 yt-dlp 时用的 Python（[`Pinned::version`]）：依赖里有按解释器版本编译的扩展模块，换了 Python 要重装
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
    pins: Pins,
    sources: Sources,
    engine: Engine,
    /// 安装与更新一次只做一个
    busy: tokio::sync::Mutex<()>,
}

impl Components {
    /// 组件放在 `root` 下（不存在时第一次安装时建）；`engine` 下载组件、请求 PyPI 的接口。
    pub fn new(root: PathBuf, pins: Pins, sources: Sources, engine: Engine) -> Components {
        Components {
            root,
            pins,
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
        if state.python != self.pins.python.version {
            return Ok(None);
        }
        let installed = self.installed_as(&state.yt_dlp);
        let present = [&installed.python, &installed.deno, &installed.site]
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

    /// 装上缺的：Python、deno（各经 `proxy` 下载、核对 SHA-256、解开）与 yt-dlp（状态文件记着的版本，没有或坏了时为
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

    /// 重装：删掉在用的 Python、deno 与 yt-dlp（文件被杀毒软件隔离或坏了、进程起不来），再照 [`Components::install`]
    /// 装上（yt-dlp 照状态文件记着的版本）。用着它们的解析进程之后起不来，重装完换新的。
    pub async fn repair(
        &self,
        proxy: &Proxy,
        channel: Channel,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<Installed, Error> {
        let _busy = self.busy.lock().await;
        let mut dirs = vec![
            self.root
                .join(format!("python-{}", self.pins.python.version)),
            self.root.join(format!("deno-{}", self.pins.deno.version)),
        ];
        // 坏了的状态文件由安装重写
        if let Ok(Some(state)) = self.state() {
            dirs.push(self.root.join(format!("yt-dlp-{}", state.yt_dlp)));
        }
        for dir in &dirs {
            remove_dir(dir).await?;
        }
        self.install_missing(proxy, channel, progress, cancel).await
    }

    /// 删掉不再用的：锁定版本以外的 Python 与 deno、当前与上一个以外的 yt-dlp、旧的驱动脚本。更新之后用旧版本的
    /// 解析进程可能还在跑（换版本前开始的任务），在没有任务运行时（应用启动时）调用。
    pub async fn sweep(&self) {
        let _busy = self.busy.lock().await;
        self.sweep_unlocked().await;
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
        self.ensure(
            &self.pins.python,
            "python",
            Stage::Python,
            proxy,
            progress,
            cancel,
        )
        .await?;
        self.ensure(
            &self.pins.deno,
            "deno",
            Stage::Deno,
            proxy,
            progress,
            cancel,
        )
        .await?;
        // 坏了的状态文件不挡着修：照没有处理
        let wanted = match self.state() {
            Ok(Some(state)) => Some(state.yt_dlp),
            Ok(None) | Err(_) => None,
        };
        let version = match wanted {
            Some(version) => version,
            None => self.latest(proxy, channel).await?,
        };
        let installed = self
            .install_yt_dlp(proxy, &version, progress, cancel)
            .await?;
        self.sweep_unlocked().await;
        Ok(installed)
    }

    /// 装上 yt-dlp 的 `version`（PyPI 上的写法，经 `proxy`）、自检，过了才换成当前的，上一个留着；已是当前的直接返回。
    /// Python 与 deno 要已装好（见 [`Components::install`]），没装时报 [`Error::NotInstalled`]。
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
        self.prepare().await?;
        self.install_yt_dlp(proxy, version, progress, cancel).await
    }

    /// `channel` 上最新的 yt-dlp 版本（经 `proxy` 查 PyPI）。
    pub async fn latest(&self, proxy: &Proxy, channel: Channel) -> Result<String, Error> {
        let body = self.get(proxy, &format!("{PROJECT}/json")).await?;
        latest_of(&body, channel)
    }

    fn installed_as(&self, version: &str) -> Installed {
        Installed {
            python: executable(&self.root, "python", &self.pins.python),
            deno: executable(&self.root, "deno", &self.pins.deno),
            site: self.root.join(format!("yt-dlp-{version}")),
            version: version.to_owned(),
            driver: self.driver_path(),
        }
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

    /// 建好目录、清空上次没装完留下的临时目录、删掉 `downloads/` 里不是这次锁定版本的组件包（与续传目录：应用升级换了
    /// 锁定版本，旧的不再用得上）、写出驱动脚本。
    async fn prepare(&self) -> Result<(), Error> {
        let staging = self.root.join("staging");
        remove_dir(&staging).await?;
        let downloads = self.root.join("downloads");
        let current: Vec<String> = [("python", &self.pins.python), ("deno", &self.pins.deno)]
            .iter()
            .map(|(name, pinned)| format!("{name}-{}", pinned.version))
            .collect();
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
                let name = entry.file_name().to_string_lossy().into_owned();
                let kept = name == "wheels"
                    || current
                        .iter()
                        .any(|package| name == *package || name == format!("{package}.hsdl"));
                if !kept {
                    let path = entry.path();
                    if path.is_dir() {
                        remove_dir(&path).await?;
                    } else {
                        remove(&path).await?;
                    }
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

    /// 锁定的组件 `pinned` 在 `<name>-<版本>/` 下就位：没有、或里面缺了可执行文件（被杀毒软件隔离、被删）时下载、
    /// 核对、解开再改名就位。
    async fn ensure(
        &self,
        pinned: &Pinned,
        name: &str,
        stage: Stage,
        proxy: &Proxy,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let target = self.root.join(format!("{name}-{}", pinned.version));
        if executable(&self.root, name, pinned).is_file() {
            return Ok(());
        }
        if target.exists() {
            tokio::fs::remove_dir_all(&target)
                .await
                .map_err(|error| Error::disk(&target, &error))?;
        }
        let package = self
            .root
            .join("downloads")
            .join(format!("{name}-{}", pinned.version));
        self.fetch(
            &pinned.url,
            &pinned.sha256,
            &package,
            proxy,
            &|bytes, total| {
                progress(InstallProgress {
                    stage,
                    bytes,
                    total,
                })
            },
            cancel,
        )
        .await?;
        let staging = self.root.join("staging").join(name);
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|error| Error::disk(&staging, &error))?;
        let (unpacked, kind, into) = (package.clone(), pinned.archive, staging.clone());
        tokio::task::spawn_blocking(move || archive::unpack(&unpacked, kind, &into))
            .await
            .expect("解包不会 panic，运行时关闭时本任务也一起丢弃")?;
        if !staging.join(&pinned.executable).is_file() {
            return Err(Error::Archive(format!(
                "{name} 的包里没有 {}",
                pinned.executable
            )));
        }
        tokio::fs::rename(&staging, &target)
            .await
            .map_err(|error| Error::disk(&target, &error))?;
        remove(&package).await
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
        let report = |bytes, total| {
            progress(InstallProgress {
                stage: Stage::YtDlp,
                bytes,
                total,
            });
        };
        report(0, None);
        let python = executable(&self.root, "python", &self.pins.python);
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
                Error::Command(reason) => Error::Unsupported(reason),
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
        report(total, Some(total));
        let staging = self.root.join("staging").join("yt-dlp");
        let mut pip = command(&python);
        pip.args(pip_args(&staging)).args(&paths);
        run(pip, PIP_TIMEOUT, cancel)
            .await
            .map_err(|error| match error {
                Error::Command(reason) => Error::Pip(reason),
                error => error,
            })?;
        progress(InstallProgress {
            stage: Stage::Checking,
            bytes: 0,
            total: None,
        });
        let deno = executable(&self.root, "deno", &self.pins.deno);
        let found = self_test(&python, &driver, &staging, &deno, cancel).await?;
        if found != version {
            return Err(Error::SelfTest(format!(
                "要装的是 {version}，装上的是 {found}"
            )));
        }
        let target = self.root.join(format!("yt-dlp-{version}"));
        if target.exists() {
            tokio::fs::remove_dir_all(&target)
                .await
                .map_err(|error| Error::disk(&target, &error))?;
        }
        tokio::fs::rename(&staging, &target)
            .await
            .map_err(|error| Error::disk(&target, &error))?;
        let previous = match self.state() {
            Ok(Some(state)) => Some(state.yt_dlp).filter(|old| old != version),
            Ok(None) | Err(_) => None,
        };
        let state = State {
            yt_dlp: version.to_owned(),
            previous,
            python: self.pins.python.version.clone(),
        };
        write_atomically(
            &self.root.join("state.json"),
            &serde_json::to_vec(&state).expect("状态总能写成 JSON"),
        )
        .await?;
        for path in &paths {
            remove(path).await?;
        }
        Ok(self.installed_as(version))
    }

    /// PyPI 上 `name` 的 `version` 这一版。
    async fn release(&self, proxy: &Proxy, name: &str, version: &str) -> Result<Release, Error> {
        let body = self.get(proxy, &format!("{name}/{version}/json")).await?;
        serde_json::from_slice(&body)
            .map_err(|error| Error::Index(format!("PyPI 回的 {name} {version} 认不出：{error}")))
    }

    /// 删掉不再用的：不是锁定版本的 Python、deno，不是当前与上一个的 yt-dlp，旧的驱动脚本。删不掉的留着，下次再删。
    async fn sweep_unlocked(&self) {
        let Ok(Some(state)) = self.state() else {
            return;
        };
        let keep = [
            format!("python-{}", self.pins.python.version),
            format!("deno-{}", self.pins.deno.version),
            format!("yt-dlp-{}", state.yt_dlp),
            state
                .previous
                .map_or_else(String::new, |previous| format!("yt-dlp-{previous}")),
        ];
        let driver = self.driver_path();
        let Ok(mut entries) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let stale_dir = ["python-", "deno-", "yt-dlp-"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
                && !keep.contains(&name);
            let stale_driver = name.starts_with("driver-") && path != driver;
            // 清理失败不影响已装好的，下次安装时再删
            if stale_dir {
                let _left = tokio::fs::remove_dir_all(&path).await;
            } else if stale_driver {
                let _left = tokio::fs::remove_file(&path).await;
            }
        }
    }
}

/// 组件 `pinned` 解开后的可执行文件。
fn executable(root: &Path, name: &str, pinned: &Pinned) -> PathBuf {
    pinned.executable.split('/').fold(
        root.join(format!("{name}-{}", pinned.version)),
        |path, part| path.join(part),
    )
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

/// 自检：驱动脚本导入 `site` 里的 yt-dlp 与解 JavaScript 挑战的 yt-dlp-ejs，`deno` 报出版本；返回 yt-dlp 规范化的版本。
async fn self_test(
    python: &Path,
    driver: &Path,
    site: &Path,
    deno: &Path,
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
            Error::Command(reason) => Error::SelfTest(reason),
            error => error,
        })?;
    #[derive(Deserialize)]
    struct Report {
        yt_dlp: String,
    }
    let report: Report = serde_json::from_slice(&out)
        .map_err(|error| Error::SelfTest(format!("驱动脚本的输出认不出：{error}")))?;
    let mut version = command(deno);
    version.arg("--version");
    let out = run(version, SELF_TEST_TIMEOUT, cancel)
        .await
        .map_err(|error| match error {
            Error::Command(reason) => Error::SelfTest(format!("deno 运行不了：{reason}")),
            error => error,
        })?;
    if !String::from_utf8_lossy(&out).starts_with("deno ") {
        return Err(Error::SelfTest("deno 报的版本认不出".to_owned()));
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
/// （后者报 [`Error::Cancelled`]），退出码不为 0 时报 [`Error::Command`]（带错误输出的末尾）。
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
        return Err(Error::Command(format!(
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
