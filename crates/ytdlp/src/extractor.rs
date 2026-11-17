//! 常驻的解析进程：`python -I driver.py serve`，标准输入输出逐行收发 JSON（协议见 driver.py）。进程一次只解析一个，
//! 用完放回去给下一次用；同时最多 [`WORKERS`] 个。一次解析超过时限、被取消、进程退出或回的认不出时杀掉这个进程，
//! 下次另起一个（Python 里打断不了正在跑的解析）。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_m3u8_core::{CancellationToken, Proxy, Url};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Semaphore;

use crate::components::{Installed, command, proxy_for};
use crate::info::{FormatKey, Info, Track, Wire};

/// 同时最多几个解析进程：每个约占几十 MB 内存，一般一次只解析一个网址。
pub const WORKERS: usize = 2;
/// 标准错误留末尾多少字节：进程意外退出时它是原因。
const STDERR_TAIL: usize = 8192;
/// 一行回应最多多少字节：YouTube 一个视频的结果约 100 KB（格式多、请求头长）。
const REPLY_LIMIT: usize = 64 << 20;
/// 交回 yt-dlp 下载时多久没有进度算卡住：yt-dlp 自己有连接与读取的超时（20 秒）与重试。
const STALL: Duration = Duration::from_secs(5 * 60);
/// 进程退出后等读标准错误的任务读完最多多久：管道里剩下的很快读完。
const STDERR_WAIT: Duration = Duration::from_secs(1);
/// 杀了进程后等它退出最多多久：SIGKILL 与 taskkill /F 之后进程即时退出，系统忙时也在一两秒内。
const STOP_WAIT: Duration = Duration::from_secs(5);

/// 发往网站的一个 Cookie（Netscape cookies.txt 的各列）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    /// 域名；`include_subdomains` 时子域也带
    pub domain: String,
    pub include_subdomains: bool,
    pub path: String,
    /// 只经 https 发
    pub secure: bool,
    /// 过期时刻（Unix 秒）；None 为会话 Cookie
    pub expires: Option<i64>,
    pub name: String,
    pub value: String,
}

/// 一次解析的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: Url,
    /// 解析时带的 Cookie（如内置浏览器里登录后的）；只经标准输入传进进程，不落盘
    pub cookies: Vec<Cookie>,
    /// 只有通用解析器认得的网址也解析；为假时这样的网址不发请求、直接报 [`ExtractError::Unsupported`]
    pub generic: bool,
}

/// 解析失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    /// 没有专门认这个网址的解析器（只有通用解析器认得、请求没要它），或通用解析器也没找到视频
    #[error("{0}")]
    Unsupported(String),
    /// 地区限制
    #[error("{0}")]
    Geo(String),
    /// 要登录（或 Cookie 过期了）
    #[error("{0}")]
    Login(String),
    /// 请求太频繁（429）：过一阵再试
    #[error("{0}")]
    RateLimited(String),
    /// 连不上网站、超时、TLS 出错
    #[error("{0}")]
    Network(String),
    /// 网站说视频不可用：私密、删了、未开播等
    #[error("{0}")]
    Unavailable(String),
    /// 组件正在换（[`Extractor::wait_idle`] 之后）：换好后用新的再试
    #[error("解析组件正在换，换好后再试")]
    Replacing,
    /// 交回 yt-dlp 下载时，网站这次给的格式里没有要的那个（见 [`crate::FormatKey`]）
    #[error("{0}")]
    Changed(String),
    /// 交回 yt-dlp 下载时写不了文件（磁盘满、没有权限）
    #[error("{0}")]
    Disk(String),
    /// 其余：多为网站改了、yt-dlp 要更新
    #[error("{0}")]
    Other(String),
    /// 超过时限没解析完（进程已杀掉）
    #[error("解析超过 {0} 秒没完成")]
    TimedOut(u64),
    /// 解析进程意外退出或回的认不出：原因是它标准错误的末尾
    #[error("解析进程出错：{0}")]
    Crashed(String),
    /// 解析进程起不来（组件的文件缺了、没有执行权限或不是可执行文件）：重新安装组件
    #[error("解析组件运行不了：{0}")]
    Broken(String),
    /// 本机一时起不了进程（打开的文件或进程太多、内存不够）：过一会再试
    #[error("起不了解析进程：{0}")]
    System(String),
    /// 系统代理设置读不了
    #[error("{0}")]
    Proxy(String),
    #[error("已取消")]
    Cancelled,
}

/// 一个解析进程。释放即连同它起的子进程（yt-dlp 解 JavaScript 挑战时起的 QuickJS-NG）一起杀掉。
struct Worker {
    /// 停下（[`Worker::stop`]）或释放时取走
    child: Option<Child>,
    /// 没经停下（或停到一半）就释放的进程交到这里，由 [`Extractor::stop_workers`] 等它们退出
    dying: Arc<Mutex<Vec<Child>>>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// 标准错误的末尾，由后台任务 `reading` 一直读着（不读满了会让进程卡住），进程退出时它读到结尾结束
    stderr: Arc<Mutex<String>>,
    reading: tokio::task::JoinHandle<()>,
    next_id: u64,
}

impl Worker {
    /// 杀掉进程与它起的子进程，等它退出（至多 [`STOP_WAIT`]）：之后它打开的文件已关上（Windows 上才删得掉）。
    async fn stop(mut self) {
        // 进程留在 Worker 里直到退出：停到一半被丢弃时释放照常把它交给 dying
        let Some(pid) = self.child.as_ref().and_then(Child::id) else {
            return;
        };
        let killed = tokio::process::Command::from(killer(pid)).status().await;
        if let (Err(_), Some(child)) = (killed, self.child.as_mut()) {
            // 杀进程组的命令起不来：至少杀掉它本身
            let _signalled = child.start_kill();
        }
        if let Some(child) = self.child.as_mut() {
            // 等不到退出（系统忙）时下面释放照样交给 dying
            if tokio::time::timeout(STOP_WAIT, child.wait()).await.is_ok() {
                self.child = None;
            }
        }
    }

    /// 进程还活着（没退出、能问到）。
    fn alive(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    }
}

impl Drop for Worker {
    /// 没经 [`Worker::stop`] 释放（解析的句柄被丢弃）时：在运行时里不等结果地起杀进程的命令（tokio 回收它），不在运行时里
    /// 时等它跑完。
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let Some(pid) = child.id() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            let spawned = tokio::process::Command::from(killer(pid)).spawn();
            let mut child = child;
            if spawned.is_err() {
                // 杀进程组的命令起不来（打开的文件或进程太多）：至少杀掉它本身。不先杀：Windows 上父进程先没了，
                // taskkill /T 找不到它的子进程
                let _signalled = child.start_kill();
            }
            let mut dying = self.dying.lock().expect("锁里不会 panic");
            // 已退出的不必再等
            dying.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
            dying.push(child);
        } else {
            // 不在运行时里（应用退出时）：等命令跑完；起不来时本进程随 Child 的释放（kill_on_drop）杀掉
            let _done = killer(pid).status();
        }
    }
}

/// 杀掉进程 `pid` 与它起的子进程的命令：Unix 上它是自己进程组的组长（见 [`Extractor::spawn`]），杀整个组；Windows 上
/// 按进程树杀。进程已经退出时杀不到，不算失败。
fn killer(pid: u32) -> std::process::Command {
    #[cfg(unix)]
    let mut killer = {
        let mut killer = std::process::Command::new("kill");
        killer.args(["-KILL", "--", &format!("-{pid}")]);
        killer
    };
    #[cfg(windows)]
    let mut killer = {
        use std::os::windows::process::CommandExt as _;
        /// CREATE_NO_WINDOW
        const NO_WINDOW: u32 = 0x0800_0000;
        let mut killer = std::process::Command::new("taskkill");
        killer
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(NO_WINDOW);
        killer
    };
    killer.stdout(Stdio::null()).stderr(Stdio::null());
    killer
}

pub struct Extractor {
    installed: Installed,
    /// yt-dlp 的缓存目录（YouTube 的播放器数据等）
    cache: PathBuf,
    timeout: Duration,
    idle: Mutex<VecDeque<Worker>>,
    /// 解析的名额；[`Extractor::wait_idle`] 之后关上
    slots: Semaphore,
    /// 释放了、杀了而还没等到退出的进程
    dying: Arc<Mutex<Vec<Child>>>,
}

impl Extractor {
    /// 用装好的组件 `installed` 解析，缓存放在 `cache`，一次解析最多 `timeout`。进程到第一次解析时才起。
    pub fn new(installed: Installed, cache: PathBuf, timeout: Duration) -> Extractor {
        Extractor {
            installed,
            cache,
            timeout,
            idle: Mutex::new(VecDeque::new()),
            slots: Semaphore::new(WORKERS),
            dying: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 等在解析的、排着队的解析都做完，再关上解析的名额：之后的解析报 [`ExtractError::Replacing`]，不再用这套组件起
    /// 进程。可以取消（丢弃即可）：关上之前取消时这个 Extractor 照常能用。换下组件之前调，接着调
    /// [`Extractor::stop_workers`]。
    pub async fn wait_idle(&self) {
        if let Ok(all) = self.slots.acquire_many(WORKERS as u32).await {
            self.slots.close();
            drop(all);
        }
    }

    /// 停掉闲着的解析进程，等它们与释放时杀了的进程都退出（各至多 5 秒）：Windows 上挪不动在运行的程序。交回 yt-dlp
    /// 下载的进程不在这里：调用方要先停下用它的任务。
    pub async fn stop_workers(&self) {
        let idle: Vec<Worker> = self
            .idle
            .lock()
            .expect("锁里不会 panic")
            .drain(..)
            .collect();
        for worker in idle {
            worker.stop().await;
        }
        let dying: Vec<Child> = self
            .dying
            .lock()
            .expect("锁里不会 panic")
            .drain(..)
            .collect();
        for mut child in dying {
            let _exited = tokio::time::timeout(STOP_WAIT, child.wait()).await;
        }
    }

    pub fn installed(&self) -> &Installed {
        &self.installed
    }

    /// 经 `proxy` 解析 `request`。同时已有 [`WORKERS`] 个在解析时等前面的完成。
    pub async fn extract(
        &self,
        request: &Request,
        proxy: &Proxy,
        cancel: &CancellationToken,
    ) -> Result<Info, ExtractError> {
        let proxy = proxy_for(proxy, &request.url)
            .map_err(|error| ExtractError::Proxy(error.to_string()))?
            .map_or_else(String::new, |proxy| proxy.to_string());
        let _slot = tokio::select! {
            slot = self.slots.acquire() => slot.map_err(|_| ExtractError::Replacing)?,
            () = cancel.cancelled() => return Err(ExtractError::Cancelled),
        };
        let idle = self.idle.lock().expect("锁里不会 panic").pop_front();
        // 闲着的进程可能已被系统或别的程序结束：不拿它解析
        let alive = idle.and_then(|mut worker| worker.alive().then_some(worker));
        let mut worker = match alive {
            Some(worker) => worker,
            None => self.spawn("serve")?,
        };
        let line = serde_json::json!({
            "id": worker.next_id,
            "url": request.url.as_str(),
            "proxy": proxy,
            "cookies": (!request.cookies.is_empty()).then(|| netscape(&request.cookies)),
            "generic": request.generic,
        });
        let id = worker.next_id;
        worker.next_id += 1;
        let replied = {
            let exchange = exchange(&mut worker, line.to_string());
            tokio::select! {
                reply = tokio::time::timeout(self.timeout, exchange) => Some(reply),
                () = cancel.cancelled() => None,
            }
        };
        let reply = match replied {
            Some(Ok(reply)) => reply,
            Some(Err(_elapsed)) => {
                worker.stop().await;
                return Err(ExtractError::TimedOut(self.timeout.as_secs()));
            }
            None => {
                worker.stop().await;
                return Err(ExtractError::Cancelled);
            }
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(reason) => {
                // 进程退出了：等读标准错误的任务读完（一会儿）再取原因
                let _read = tokio::time::timeout(STDERR_WAIT, &mut worker.reading).await;
                return Err(ExtractError::Crashed(with_stderr(&reason, &worker)));
            }
        };
        let parsed: Reply = match serde_json::from_str(&reply) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Err(ExtractError::Crashed(with_stderr(
                    &format!("回应认不出：{error}"),
                    &worker,
                )));
            }
        };
        if parsed.id != id {
            return Err(ExtractError::Crashed(format!(
                "回应的编号 {} 不是请求的 {id}",
                parsed.id
            )));
        }
        self.idle.lock().expect("锁里不会 panic").push_back(worker);
        match (parsed.ok, parsed.error) {
            (Some(wire), None) => Ok(wire.into_info()),
            (None, Some(failure)) => Err(failure.into_error()),
            _ => Err(ExtractError::Crashed(
                "回应里结果与失败不是恰好一个".to_owned(),
            )),
        }
    }

    /// 起一个解析进程：隔离模式（不读 PYTHON* 环境变量与用户的 site-packages）、标准输入输出用 UTF-8（隔离模式不读
    /// PYTHONIOENCODING，Windows 上管道默认按系统的 ANSI 代码页），不加载 yt-dlp 的插件，不读环境变量里的代理；Unix 上
    /// 自成一个进程组，释放时连同子进程一起杀。
    fn spawn(&self, mode: &str) -> Result<Worker, ExtractError> {
        let installed = &self.installed;
        let mut command = command(&installed.python);
        #[cfg(unix)]
        command.process_group(0);
        command
            .arg("-I")
            .arg("-X")
            .arg("utf8")
            .arg(&installed.driver)
            .arg(mode)
            .arg(&installed.site)
            .arg(&installed.quickjs)
            .arg(&self.cache)
            .env("YTDLP_NO_PLUGINS", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            // 不是可执行文件：ENOEXEC（Unix）；ERROR_BAD_EXE_FORMAT、ERROR_EXE_MACHINE_TYPE_MISMATCH（Windows）。被杀毒
            // 软件拦下：ERROR_VIRUS_INFECTED、ERROR_VIRUS_DELETED（Windows）
            let unusable: &[i32] = if cfg!(windows) {
                &[193, 216, 225, 226]
            } else {
                &[8]
            };
            let broken = matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) || error
                .raw_os_error()
                .is_some_and(|code| unusable.contains(&code));
            if broken {
                ExtractError::Broken(format!("起不来：{error}"))
            } else {
                ExtractError::System(error.to_string())
            }
        })?;
        let stdin = child.stdin.take().expect("上面设了 piped");
        let stdout = BufReader::new(child.stdout.take().expect("上面设了 piped"));
        let mut stderr_pipe = child.stderr.take().expect("上面设了 piped");
        let stderr = Arc::new(Mutex::new(String::new()));
        let tail = stderr.clone();
        // 进程退出时管道读到结尾，任务随之结束
        let reading = tokio::spawn(async move {
            let mut buffer = vec![0; 4096];
            while let Ok(read) = stderr_pipe.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                let mut tail = tail.lock().expect("锁里不会 panic");
                tail.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if tail.len() > STDERR_TAIL {
                    let cut = tail.len() - STDERR_TAIL;
                    let cut = (cut..tail.len())
                        .find(|index| tail.is_char_boundary(*index))
                        .unwrap_or(tail.len());
                    tail.drain(..cut);
                }
            }
        });
        Ok(Worker {
            child: Some(child),
            dying: self.dying.clone(),
            stdin,
            stdout,
            stderr,
            reading,
            next_id: 1,
        })
    }
}

impl Extractor {
    /// 交回 yt-dlp 自己下：经 `proxy` 解析 `request` 的网页、用 yt-dlp 的下载器把 `format` 认出的格式下到 `output`
    /// （自己的引擎下不了的：要模拟浏览器 TLS 指纹的、DASH 分片等），进度经 `progress`（已下的、总的或估算的字节）。
    /// 另起一个进程，不占解析的名额；超过 5 分钟没有进度算卡住（报 [`ExtractError::TimedOut`]），取消或出错时连同
    /// 子进程杀掉，留下的 `.part` 下次接着下；缺了的片段报错，不跳过；这次的格式里认不出 `format` 时报
    /// [`ExtractError::Changed`]。
    ///
    /// `output` 的文件名只能由字母、数字、`.`、`-`、`_` 组成（yt-dlp 会展开输出模板里的 `$` 与 `~`），目录不限。
    pub async fn download(
        &self,
        request: &Request,
        format: &FormatKey,
        output: &Path,
        proxy: &Proxy,
        progress: &(dyn Fn(u64, Option<u64>) + Sync),
        cancel: &CancellationToken,
    ) -> Result<(), ExtractError> {
        let proxy = proxy_for(proxy, &request.url)
            .map_err(|error| ExtractError::Proxy(error.to_string()))?
            .map_or_else(String::new, |proxy| proxy.to_string());
        let (Some(dir), Some(name)) = (output.parent(), output.file_name()) else {
            panic!("输出 {} 要有目录与文件名", output.display());
        };
        let mut worker = self.spawn("download")?;
        let line = serde_json::json!({
            "url": request.url.as_str(),
            "format": key_wire(format),
            "dir": dir.to_string_lossy(),
            "name": name.to_string_lossy(),
            "proxy": proxy,
            "cookies": (!request.cookies.is_empty()).then(|| netscape(&request.cookies)),
        });
        let sent = async {
            worker
                .stdin
                .write_all(format!("{line}\n").as_bytes())
                .await?;
            worker.stdin.flush().await
        };
        if let Err(error) = sent.await {
            return Err(ExtractError::Crashed(with_stderr(
                &format!("写不进请求：{error}"),
                &worker,
            )));
        }
        let result = loop {
            let mut reply = String::new();
            let mut limited = (&mut worker.stdout).take(REPLY_LIMIT as u64);
            let read = limited.read_line(&mut reply);
            let read = tokio::select! {
                read = tokio::time::timeout(STALL, read) => match read {
                    Ok(read) => read,
                    Err(_) => break Err(ExtractError::TimedOut(STALL.as_secs())),
                },
                () = cancel.cancelled() => break Err(ExtractError::Cancelled),
            };
            match read {
                Ok(0) | Err(_) => {
                    let _read = tokio::time::timeout(STDERR_WAIT, &mut worker.reading).await;
                    break Err(ExtractError::Crashed(with_stderr("进程退出了", &worker)));
                }
                Ok(_) => {}
            }
            match serde_json::from_str::<DownloadReply>(&reply) {
                Ok(DownloadReply::Progress { progress: now }) => {
                    progress(
                        now.bytes.unwrap_or(0.0).max(0.0) as u64,
                        now.total.map(|t| t.max(0.0) as u64),
                    );
                }
                Ok(DownloadReply::Ok { .. }) => break Ok(()),
                Ok(DownloadReply::Error { error }) => break Err(error.into_error()),
                Err(error) => {
                    break Err(ExtractError::Crashed(with_stderr(
                        &format!("回应认不出：{error}"),
                        &worker,
                    )));
                }
            }
        };
        // 下完、出错、取消都等进程退出：之后 `.part` 已关上，接着下或删任务目录都不撞上它
        worker.stop().await;
        result
    }
}

/// 下载时进程回的一行。
#[derive(Deserialize)]
#[serde(untagged)]
enum DownloadReply {
    Progress {
        progress: DownloadProgress,
    },
    /// 下完了（内容为空的对象）
    Ok {
        #[serde(rename = "ok")]
        _done: serde::de::IgnoredAny,
    },
    Error {
        error: Failure,
    },
}

#[derive(Deserialize)]
struct DownloadProgress {
    bytes: Option<f64>,
    total: Option<f64>,
}

/// 写一行请求、读一行回应；进程退出、管道断了时为原因。
async fn exchange(worker: &mut Worker, line: String) -> Result<String, String> {
    worker
        .stdin
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|error| format!("写不进请求：{error}"))?;
    worker
        .stdin
        .flush()
        .await
        .map_err(|error| format!("写不进请求：{error}"))?;
    let mut reply = String::new();
    let read = (&mut worker.stdout)
        .take(REPLY_LIMIT as u64)
        .read_line(&mut reply)
        .await
        .map_err(|error| format!("读不出回应：{error}"))?;
    if read == 0 {
        return Err("进程退出了".to_owned());
    }
    if !reply.ends_with('\n') {
        return Err(format!("回应超过 {REPLY_LIMIT} 字节"));
    }
    Ok(reply)
}

/// `reason` 后接进程标准错误的末尾。
fn with_stderr(reason: &str, worker: &Worker) -> String {
    let tail = worker.stderr.lock().expect("锁里不会 panic").clone();
    let tail = tail.trim();
    if tail.is_empty() {
        reason.to_owned()
    } else {
        format!("{reason}；{tail}")
    }
}

/// Netscape cookies.txt 的写法（yt-dlp 的 cookiefile 读它）。
fn netscape(cookies: &[Cookie]) -> String {
    let mut text = String::from("# Netscape HTTP Cookie File\n");
    for cookie in cookies {
        let flag = |on: bool| if on { "TRUE" } else { "FALSE" };
        // 带子域的域名以点开头，不带的不以点开头（cookies.txt 的约定）
        let domain = match (cookie.include_subdomains, cookie.domain.strip_prefix('.')) {
            (true, None) => format!(".{}", cookie.domain),
            (false, Some(bare)) => bare.to_owned(),
            _ => cookie.domain.clone(),
        };
        text.push_str(&format!(
            "{domain}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            flag(cookie.include_subdomains),
            cookie.path,
            flag(cookie.secure),
            cookie.expires.unwrap_or(0),
            cookie.name,
            cookie.value,
        ));
    }
    text
}

#[derive(Deserialize)]
struct Reply {
    id: u64,
    ok: Option<Wire>,
    error: Option<Failure>,
}

#[derive(Deserialize)]
struct Failure {
    kind: String,
    message: String,
}

impl Failure {
    fn into_error(self) -> ExtractError {
        let message = self.message;
        match self.kind.as_str() {
            "unsupported" => ExtractError::Unsupported(message),
            "geo" => ExtractError::Geo(message),
            "login" => ExtractError::Login(message),
            "rate_limited" => ExtractError::RateLimited(message),
            "network" => ExtractError::Network(message),
            "unavailable" => ExtractError::Unavailable(message),
            "changed" => ExtractError::Changed(message),
            "disk" => ExtractError::Disk(message),
            _ => ExtractError::Other(message),
        }
    }
}

/// `key` 写成驱动脚本 `key_of` 的样子。
fn key_wire(key: &FormatKey) -> serde_json::Value {
    fn track(track: &Track) -> Option<&str> {
        match track {
            Track::Absent => Some("none"),
            Track::Unknown => None,
            Track::Codec(codec) => Some(codec),
        }
    }
    serde_json::json!({
        "format_id": key.id,
        "ext": key.ext,
        "vcodec": track(&key.video),
        "acodec": track(&key.audio),
        "height": key.height,
        "language": key.language,
        "filesize": key.size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_are_written_in_the_netscape_layout() {
        let cookies = [
            Cookie {
                domain: "youtube.com".to_owned(),
                include_subdomains: true,
                path: "/".to_owned(),
                secure: true,
                expires: Some(1_800_000_000),
                name: "SID".to_owned(),
                value: "abc".to_owned(),
            },
            Cookie {
                domain: ".www.bilibili.com".to_owned(),
                include_subdomains: false,
                path: "/video".to_owned(),
                secure: false,
                expires: None,
                name: "b".to_owned(),
                value: "1".to_owned(),
            },
        ];
        assert_eq!(
            netscape(&cookies),
            "# Netscape HTTP Cookie File\n\
             .youtube.com\tTRUE\t/\tTRUE\t1800000000\tSID\tabc\n\
             www.bilibili.com\tFALSE\t/video\tFALSE\t0\tb\t1\n"
        );
    }
}
