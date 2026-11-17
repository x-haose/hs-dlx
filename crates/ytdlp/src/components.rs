//! 组件目录：按需下载、核对并解开 Python 与 deno（版本随应用锁定，见 [`Pins`]），用这个 Python 的 pip 从 PyPI 装
//! yt-dlp；yt-dlp 的每个版本装进自己的目录，自检过了才记成当前的，没装成的不影响在用的。
//!
//! 目录布局（`root` 下）：
//! - `python-<版本>/`、`deno-<版本>/`：解开的组件包；不是锁定版本的在装好之后删掉
//! - `yt-dlp-<版本>/`：pip 的 `--target` 目录；保留当前的与上一个（新版出问题时还能换回）
//! - `state.json`：当前的 yt-dlp 版本；先写临时文件再改名
//! - `downloads/`：下载中的组件包与核心的任务目录（续传用），解开后删掉
//! - `staging/`：解包与 pip 安装的临时目录，每次安装前清空
//! - `driver-<摘要>.py`：解析进程的驱动脚本，按内容命名，内容变了另写一个

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use hs_m3u8_core::{CancellationToken, Engine, FileOutputOptions, FileRequest, Proxy, Source, Url};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

use crate::Error;
use crate::archive;
use crate::pins::{Pinned, Pins};

/// 解析进程的驱动脚本：自检、解析。
pub(crate) const DRIVER: &str = include_str!("driver.py");
/// pip 装 yt-dlp 最多多久：要下载约 10 MB 的包（curl-cffi 最大），慢的网络与代理也够。
const PIP_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// 自检最多多久：只导入模块、运行 deno 报版本。
const SELF_TEST_TIMEOUT: Duration = Duration::from_secs(60);
/// 命令的错误输出最多留多少字节写进错误信息：pip 出错时末尾是原因。
const STDERR_TAIL: usize = 4096;
/// PyPI 的 JSON 接口响应最多多少字节：yt-dlp 的全部发布约 1.2 MB（2026-11）。
const JSON_LIMIT: u64 = 16 << 20;

/// 从哪里取 yt-dlp。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sources {
    /// pip 用的索引（PEP 503 的 simple 接口）
    pub index: Url,
    /// yt-dlp 的发布信息（PyPI 的 JSON 接口），查最新版本用
    pub releases: Url,
}

impl Sources {
    pub fn official() -> Sources {
        Sources {
            index: Url::parse("https://pypi.org/simple/").expect("写死的地址"),
            releases: Url::parse("https://pypi.org/pypi/yt-dlp/json").expect("写死的地址"),
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
    /// yt-dlp 的版本
    pub version: String,
    pub driver: PathBuf,
}

/// 安装进行到哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Python,
    Deno,
    /// pip 安装 yt-dlp 与依赖：没有字节数
    YtDlp,
    /// 自检
    Checking,
}

/// 安装的进度：`bytes`、`total` 只在下载组件包时有。
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
    /// 上一个版本，新版出问题时还能换回；没有时为 None
    previous: Option<String>,
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
    /// 组件放在 `root` 下（不存在时第一次安装时建）；`engine` 下载组件包、请求 PyPI 的接口。
    pub fn new(root: PathBuf, pins: Pins, sources: Sources, engine: Engine) -> Components {
        Components {
            root,
            pins,
            sources,
            engine,
            busy: tokio::sync::Mutex::new(()),
        }
    }

    /// 装全了的组件；缺哪一样时为 None。驱动脚本按内容命名，应用升级后换了内容时在这里写出新的。状态文件读不出、内容
    /// 认不出、驱动脚本写不出时报 [`Error::Disk`]。
    pub fn installed(&self) -> Result<Option<Installed>, Error> {
        let Some(state) = self.state()? else {
            return Ok(None);
        };
        let installed = self.installed_as(&state.yt_dlp);
        let present = [&installed.python, &installed.deno, &installed.site]
            .iter()
            .all(|path| path.exists());
        if !present {
            return Ok(None);
        }
        if !installed.driver.exists() {
            let temporary = installed.driver.with_extension("tmp");
            std::fs::write(&temporary, DRIVER).map_err(|error| Error::disk(&temporary, &error))?;
            std::fs::rename(&temporary, &installed.driver)
                .map_err(|error| Error::disk(&installed.driver, &error))?;
        }
        Ok(Some(installed))
    }

    /// 装上缺的：Python、deno（各经 `proxy` 下载、核对 SHA-256、解开）与 `channel` 上最新的 yt-dlp；已装全时直接
    /// 返回。`cancel` 取消时报 [`Error::Cancelled`]，已下了的组件包留着下次续传。
    pub async fn install(
        &self,
        proxy: &Proxy,
        channel: Channel,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<Installed, Error> {
        let _busy = self.busy.lock().await;
        if let Some(installed) = self.installed()? {
            return Ok(installed);
        }
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
        let installed = match self.state()? {
            Some(state) if self.installed_as(&state.yt_dlp).site.exists() => {
                self.installed_as(&state.yt_dlp)
            }
            _ => {
                self.install_yt_dlp(proxy, channel, None, progress, cancel)
                    .await?
            }
        };
        self.sweep().await;
        Ok(installed)
    }

    /// 装上 yt-dlp 的 `version`（经 `proxy`）、自检，过了才换成当前的，上一个留着；已是当前的直接返回。Python 与 deno
    /// 要已装好（见 [`Components::install`]），没装时报 [`Error::NotInstalled`]。
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
        let channel = if version.contains(".dev") {
            Channel::Nightly
        } else {
            Channel::Stable
        };
        let installed = self
            .install_yt_dlp(proxy, channel, Some(version), progress, cancel)
            .await?;
        self.sweep().await;
        Ok(installed)
    }

    /// `channel` 上最新的 yt-dlp 版本（经 `proxy` 查 PyPI）。
    pub async fn latest(&self, proxy: &Proxy, channel: Channel) -> Result<String, Error> {
        let mut source = Source::new(self.sources.releases.clone());
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
        if reply.status != 200 {
            return Err(Error::Index(format!(
                "查 yt-dlp 的发布时 PyPI 回了 HTTP {}",
                reply.status
            )));
        }
        latest_of(&reply.body, channel)
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

    /// 建好目录、清空上次没装完留下的临时目录、写出驱动脚本。
    async fn prepare(&self) -> Result<(), Error> {
        let staging = self.root.join("staging");
        match tokio::fs::remove_dir_all(&staging).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::disk(&staging, &error)),
        }
        for dir in [&staging, &self.root.join("downloads")] {
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

    /// 锁定的组件 `pinned` 在 `<name>-<版本>/` 下就位：没有时下载、核对、解开再改名就位。
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
        if target.exists() {
            return Ok(());
        }
        let downloads = self.root.join("downloads");
        let package = downloads.join(format!("{name}-{}", pinned.version));
        let url = Url::parse(&pinned.url)
            .map_err(|error| Error::Index(format!("{name} 的地址 {}：{error}", pinned.url)))?;
        let mut source = Source::new(url);
        source.http.proxy = proxy.clone();
        let mut output = FileOutputOptions::new(package.clone());
        output.work_dir = Some(downloads.join(format!("{name}-{}.hsdl", pinned.version)));
        let mut request = FileRequest::new(source, output);
        // GitHub 的下载经重定向到每次签名不同的地址：以包的哈希担保内容，换了地址也续传
        request.identity = Some(format!("sha256:{}", pinned.sha256));
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
                        progress(InstallProgress { stage, bytes: now.bytes, total: now.total });
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
        let staging = self.root.join("staging").join(name);
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|error| Error::disk(&staging, &error))?;
        let (checked, expected, archive_kind) =
            (package.clone(), pinned.sha256.clone(), pinned.archive);
        let unpack_into = staging.clone();
        tokio::task::spawn_blocking(move || {
            let actual = archive::sha256_of(&checked)?;
            if actual != expected {
                // 内容不对的包不留着续传
                let _gone = std::fs::remove_file(&checked);
                return Err(Error::Mismatch {
                    name: checked.display().to_string(),
                    expected,
                    actual,
                });
            }
            archive::unpack(&checked, archive_kind, &unpack_into)
        })
        .await
        .expect("核对与解包不会 panic，运行时关闭时本任务也一起丢弃")?;
        let executable = staging.join(&pinned.executable);
        if !executable.is_file() {
            return Err(Error::Archive(format!(
                "{name} 的包里没有 {}",
                pinned.executable
            )));
        }
        tokio::fs::rename(&staging, &target)
            .await
            .map_err(|error| Error::disk(&target, &error))?;
        tokio::fs::remove_file(&package)
            .await
            .map_err(|error| Error::disk(&package, &error))?;
        Ok(())
    }

    /// 用装好的 Python 的 pip 装 yt-dlp（`version` 为 None 时装 `channel` 上最新的）进临时目录，自检，改名成
    /// `yt-dlp-<版本>/` 再记成当前的。
    async fn install_yt_dlp(
        &self,
        proxy: &Proxy,
        channel: Channel,
        version: Option<&str>,
        progress: &(dyn Fn(InstallProgress) + Sync),
        cancel: &CancellationToken,
    ) -> Result<Installed, Error> {
        progress(InstallProgress {
            stage: Stage::YtDlp,
            bytes: 0,
            total: None,
        });
        let python = executable(&self.root, "python", &self.pins.python);
        let staging = self.root.join("staging").join("yt-dlp");
        let pip_proxy = proxy_for(proxy, &self.sources.index)?;
        let args = pip_args(
            &staging,
            &self.sources.index,
            channel,
            version,
            pip_proxy.as_ref(),
        );
        let mut command = tokio::process::Command::new(&python);
        command.args(&args);
        clear_proxy_env(&mut command);
        run(command, PIP_TIMEOUT, cancel)
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
        let found = self_test(&python, &self.driver_path(), &staging, &deno, cancel).await?;
        if let Some(version) = version
            && found != version
        {
            return Err(Error::SelfTest(format!(
                "要装的是 {version}，装上的是 {found}"
            )));
        }
        let target = self.root.join(format!("yt-dlp-{found}"));
        if target.exists() {
            tokio::fs::remove_dir_all(&target)
                .await
                .map_err(|error| Error::disk(&target, &error))?;
        }
        tokio::fs::rename(&staging, &target)
            .await
            .map_err(|error| Error::disk(&target, &error))?;
        let previous = self
            .state()?
            .map(|state| state.yt_dlp)
            .filter(|old| *old != found);
        let state = State {
            yt_dlp: found.clone(),
            previous,
        };
        write_atomically(
            &self.root.join("state.json"),
            &serde_json::to_vec(&state).expect("状态总能写成 JSON"),
        )
        .await?;
        Ok(self.installed_as(&found))
    }

    /// 删掉不再用的：不是锁定版本的 Python、deno，不是当前与上一个的 yt-dlp，旧的驱动脚本。删不掉的留着，下次再删。
    async fn sweep(&self) {
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

/// 子进程不读环境变量里的代理：代理只照应用的设置给。
pub(crate) fn clear_proxy_env(command: &mut tokio::process::Command) {
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"] {
        command.env_remove(name);
        command.env_remove(name.to_ascii_lowercase());
    }
}

/// pip 装 yt-dlp 的参数：隔离模式（不读 PYTHON* 与 PIP_* 环境变量、用户的 site-packages 与 pip 配置），只装预编译
/// 的包，不用缓存；默认的依赖组与 curl-cffi（模拟浏览器的 TLS 指纹，部分站点解析要它）。
fn pip_args(
    target: &Path,
    index: &Url,
    channel: Channel,
    version: Option<&str>,
    proxy: Option<&Url>,
) -> Vec<String> {
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
        "--only-binary=:all:",
        "--timeout",
        "30",
    ]
    .map(str::to_owned)
    .into();
    args.push("--target".to_owned());
    args.push(target.to_string_lossy().into_owned());
    args.push("--index-url".to_owned());
    args.push(index.to_string());
    if index.scheme() == "http"
        && let Some(host) = index.host_str()
    {
        args.push("--trusted-host".to_owned());
        args.push(host.to_owned());
    }
    if let Some(proxy) = proxy {
        args.push("--proxy".to_owned());
        args.push(proxy.to_string());
    }
    if channel == Channel::Nightly {
        args.push("--pre".to_owned());
    }
    let mut requirement = "yt-dlp[default,curl-cffi]".to_owned();
    if let Some(version) = version {
        requirement.push_str("==");
        requirement.push_str(version);
    }
    args.push(requirement);
    args
}

/// 自检：驱动脚本导入 `site` 里的 yt-dlp 与解 JavaScript 挑战的 yt-dlp-ejs，`deno` 报出版本；返回 yt-dlp 的版本。
async fn self_test(
    python: &Path,
    driver: &Path,
    site: &Path,
    deno: &Path,
    cancel: &CancellationToken,
) -> Result<String, Error> {
    let mut command = tokio::process::Command::new(python);
    command.arg("-I").arg(driver).arg("selftest").arg(site);
    clear_proxy_env(&mut command);
    let out = run(command, SELF_TEST_TIMEOUT, cancel)
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
    let mut command = tokio::process::Command::new(deno);
    command.arg("--version");
    let out = run(command, SELF_TEST_TIMEOUT, cancel)
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

/// 运行 `command` 到结束，返回标准输出；超过 `timeout` 或 `cancel` 时杀掉进程（后者报 [`Error::Cancelled`]），退出码
/// 不为 0 时报 [`Error::Command`]（带错误输出的末尾）。
pub(crate) async fn run(
    mut command: tokio::process::Command,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, Error> {
    command
        .stdin(Stdio::null())
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
    let mut stdout = child.stdout.take().expect("上面设了 piped");
    let mut stderr = child.stderr.take().expect("上面设了 piped");
    let reading = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (read_out, read_err) =
            tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        read_out.and(read_err).map(|_| (out, err))
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
    let (out, err) =
        read.map_err(|error| Error::Command(format!("读 {program} 的输出：{error}")))?;
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

/// 先写 `path` 旁的临时文件再改名：读的一方看到的要么是旧的、要么是完整的新内容。
async fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let temporary = path.with_extension("tmp");
    tokio::fs::write(&temporary, bytes)
        .await
        .map_err(|error| Error::disk(&temporary, &error))?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|error| Error::disk(path, &error))
}

/// PyPI 的 JSON 接口回的 `body` 里 `channel` 上最新的版本：稳定版为 `info.version`；每日构建为上传最晚的 `.dev` 版本
/// （没有比稳定版新的时为稳定版）。
fn latest_of(body: &[u8], channel: Channel) -> Result<String, Error> {
    #[derive(Deserialize)]
    struct Project {
        info: Info,
        releases: std::collections::BTreeMap<String, Vec<File>>,
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
    fn pip_runs_isolated_from_the_index_with_the_proxy() {
        let index = Url::parse("http://127.0.0.1:9/simple/").unwrap();
        let proxy = Url::parse("http://127.0.0.1:8080").unwrap();
        let args = pip_args(
            Path::new("/c/staging/yt-dlp"),
            &index,
            Channel::Nightly,
            Some("2026.11.16.232655.dev0"),
            Some(&proxy),
        );
        let joined = args.join(" ");
        assert!(
            joined.starts_with("-I -m pip --isolated install "),
            "{joined}"
        );
        for part in [
            "--only-binary=:all:",
            "--target /c/staging/yt-dlp",
            "--index-url http://127.0.0.1:9/simple/",
            "--trusted-host 127.0.0.1",
            "--proxy http://127.0.0.1:8080/",
            "--pre",
        ] {
            assert!(joined.contains(part), "缺 {part}：{joined}");
        }
        assert_eq!(
            args.last().unwrap(),
            "yt-dlp[default,curl-cffi]==2026.11.16.232655.dev0"
        );
        let https = pip_args(
            Path::new("/t"),
            &Sources::official().index,
            Channel::Stable,
            None,
            None,
        );
        assert!(
            !https
                .iter()
                .any(|arg| arg == "--trusted-host" || arg == "--pre" || arg == "--proxy")
        );
        assert_eq!(https.last().unwrap(), "yt-dlp[default,curl-cffi]");
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
