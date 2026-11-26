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

use crate::{Browser, CookieSource};

use crate::components::{Installed, command, proxy_for};
use crate::info::{FormatKey, Info, Track, Wire};

/// 同时最多几个解析进程：每个约占几十 MB 内存，一般一次只解析一个网址。
pub const WORKERS: usize = 2;
/// 标准错误留末尾多少字节：进程意外退出时它是原因。
const STDERR_TAIL: usize = 8192;
/// 一行回应最多多少字节：YouTube 一个视频的结果约 100 KB（格式多、请求头长）。
const REPLY_LIMIT: usize = 64 << 20;
/// 从浏览器读 Cookie 最多等多久（之后才起算解析的时限）：macOS 上读 Chromium 系的要用户点钥匙串的授权，Linux 上
/// 可能在等密钥环解锁。
const COOKIE_WAIT: Duration = Duration::from_secs(3 * 60);
/// 交回 yt-dlp 下载时多久没有进度算卡住：yt-dlp 自己有连接与读取的超时（20 秒）与重试。
const STALL: Duration = Duration::from_secs(5 * 60);
/// 进程退出后等读标准错误的任务读完最多多久：管道里剩下的很快读完。
const STDERR_WAIT: Duration = Duration::from_secs(1);
/// 杀了进程后等它退出最多多久：SIGKILL 与 taskkill /F 之后进程即时退出，系统忙时也在一两秒内。
const STOP_WAIT: Duration = Duration::from_secs(5);

/// 发往网站的一个 Cookie（Netscape cookies.txt 的各列）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
    /// 另从这个浏览器的这个用户资料读 Cookie（与 `cookies` 合并）；读不出或没读全时报 [`ExtractError::Cookies`]，不发
    /// 请求
    pub browser: Option<CookieSource>,
}

impl Request {
    /// 交回 yt-dlp 下载时用的请求：要从浏览器读的，不再读浏览器，改带那次解析时罐里属于这个网站的 Cookie（`info` 的
    /// [`Info::site_cookies`]，含请求里给的与解析时网站设下的；只有通用解析器认得、没读浏览器时没有从浏览器读的）；不读
    /// 浏览器的原样。
    pub fn for_download(&self, info: &Info) -> Request {
        if self.browser.is_none() {
            return self.clone();
        }
        Request {
            url: self.url.clone(),
            cookies: info.site_cookies.clone(),
            generic: self.generic,
            browser: None,
        }
    }
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
    /// 从浏览器读 Cookie 没读出或没读全（为说明，写了怎么办）：还没向网站发请求
    #[error("{0}")]
    Cookies(String),
    /// 请求太频繁（429）：过一阵再试
    #[error("{0}")]
    RateLimited(String),
    /// 网站拦下了这个出口（解析时网页或接口回 403、412：多为出口 IP 被风控，如 B 站对境外代理）：换直连或别的出口、登录
    /// 后再试
    #[error("{0}")]
    Blocked(String),
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
            "browser": request.browser.as_ref().map(|source| source.browser.code()),
            "profile": profile_wire(request.browser.as_ref()),
            "site": site_of(&request.url),
        });
        let id = worker.next_id;
        worker.next_id += 1;
        // 先等开始解析网页的那一行：从浏览器读 Cookie 时可能在等用户点授权，另算时限
        let reading = request.browser.as_ref().map(|source| {
            ExtractError::Cookies(cookie_trouble(
                Some(source),
                Some("stalled"),
                &format!("等了 {} 秒", COOKIE_WAIT.as_secs()),
            ))
        });
        let first_wait = if reading.is_some() {
            COOKIE_WAIT
        } else {
            self.timeout
        };
        let waited = match send(&mut worker, &line.to_string()).await {
            Ok(()) => wait_line(&mut worker, first_wait, cancel).await,
            Err(reason) => Waited::Line(Err(reason)),
        };
        let timed_out = reading.unwrap_or(ExtractError::TimedOut(self.timeout.as_secs()));
        let (mut parsed, mut worker) = reply_of(waited, worker, id, timed_out).await?;
        if parsed.stage.is_some() {
            let waited = wait_line(&mut worker, self.timeout, cancel).await;
            let timed_out = ExtractError::TimedOut(self.timeout.as_secs());
            (parsed, worker) = reply_of(waited, worker, id, timed_out).await?;
        }
        self.idle.lock().expect("锁里不会 panic").push_back(worker);
        match (parsed.ok, parsed.error) {
            // QuickJS 不在了（被杀毒软件隔离等）时 yt-dlp 同样报解不出挑战：是组件坏了，要重装，换 yt-dlp 没用
            (Some(wire), None) => {
                let info = wire.into_info();
                let quickjs = &self.installed.quickjs;
                if info.challenge_failed && !quickjs.is_file() {
                    return Err(ExtractError::Broken(format!(
                        "解 JavaScript 挑战用的 QuickJS 不在了：{}",
                        quickjs.display()
                    )));
                }
                Ok(info)
            }
            (None, Some(failure)) => {
                Err(failure.into_error(request.browser.as_ref(), proxy.is_empty()))
            }
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
            "browser": request.browser.as_ref().map(|source| source.browser.code()),
            "profile": profile_wire(request.browser.as_ref()),
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
                Ok(DownloadReply::Error { error }) => {
                    break Err(error.into_error(request.browser.as_ref(), proxy.is_empty()));
                }
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
/// 把一行请求写给解析进程；写不进为原因。
async fn send(worker: &mut Worker, line: &str) -> Result<(), String> {
    worker
        .stdin
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|error| format!("写不进请求：{error}"))?;
    worker
        .stdin
        .flush()
        .await
        .map_err(|error| format!("写不进请求：{error}"))
}

/// 等解析进程的一行回应的结果。
enum Waited {
    /// 读到的一行，或读不出的原因（进程退出、超长）
    Line(Result<String, String>),
    TimedOut,
    Cancelled,
}

/// 至多等 `limit` 读一行回应，`cancel` 时不再等。
async fn wait_line(worker: &mut Worker, limit: Duration, cancel: &CancellationToken) -> Waited {
    tokio::select! {
        line = tokio::time::timeout(limit, read_line(worker)) => match line {
            Ok(line) => Waited::Line(line),
            Err(_elapsed) => Waited::TimedOut,
        },
        () = cancel.cancelled() => Waited::Cancelled,
    }
}

/// 等到的一行认成编号为 `id` 的回应，连同进程交回；超时（报 `timed_out`）与取消时先停下进程，进程退出、回应认不出
/// 或编号不对时为 [`ExtractError::Crashed`]，带标准错误的末尾。
async fn reply_of(
    waited: Waited,
    mut worker: Worker,
    id: u64,
    timed_out: ExtractError,
) -> Result<(Reply, Worker), ExtractError> {
    let line = match waited {
        Waited::Line(Ok(line)) => line,
        Waited::Line(Err(reason)) => {
            // 进程退出了：等读标准错误的任务读完（一会儿）再取原因
            let _read = tokio::time::timeout(STDERR_WAIT, &mut worker.reading).await;
            return Err(ExtractError::Crashed(with_stderr(&reason, &worker)));
        }
        Waited::TimedOut => {
            worker.stop().await;
            return Err(timed_out);
        }
        Waited::Cancelled => {
            worker.stop().await;
            return Err(ExtractError::Cancelled);
        }
    };
    let parsed: Reply = serde_json::from_str(&line).map_err(|error| {
        ExtractError::Crashed(with_stderr(&format!("回应认不出：{error}"), &worker))
    })?;
    if parsed.id != id {
        return Err(ExtractError::Crashed(format!(
            "回应的编号 {} 不是请求的 {id}",
            parsed.id
        )));
    }
    Ok((parsed, worker))
}

/// 读解析进程的一行回应；进程退出或一行超过 [`REPLY_LIMIT`] 时为原因。
async fn read_line(worker: &mut Worker) -> Result<String, String> {
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
    /// 开始解析网页时的那一行（`resolving`），之后才是结果或失败
    stage: Option<String>,
    ok: Option<Wire>,
    error: Option<Failure>,
}

#[derive(Deserialize)]
struct Failure {
    kind: String,
    message: String,
    /// `cookies` 的原因（见驱动脚本的 `browser_cookies`）
    reason: Option<String>,
}

impl Failure {
    /// `source` 为请求里读 Cookie 的浏览器与用户资料（`cookies` 的说明里写它们），`direct` 为没走代理（`reset`、
    /// `unreachable` 的说明按它写怎么办；直连时的 `proxy` 只能是网站或中间设备回的 407，照原文报）。
    fn into_error(self, source: Option<&CookieSource>, direct: bool) -> ExtractError {
        let browser = source.map(|source| source.browser);
        let message = self.message;
        match self.kind.as_str() {
            "cookies" => {
                // yt-dlp 把没有权限列的数据目录当成空的，报没找到：这时报没有权限
                let denied = match (self.reason.as_deref(), browser) {
                    (Some("missing"), Some(browser)) => browser.access().err(),
                    _ => None,
                };
                ExtractError::Cookies(match denied {
                    Some(denied) => cookie_trouble(source, Some("denied"), &denied.to_string()),
                    None => cookie_trouble(source, self.reason.as_deref(), &message),
                })
            }
            "unsupported" => ExtractError::Unsupported(message),
            "geo" => ExtractError::Geo(message),
            "login" => ExtractError::Login(message),
            "rate_limited" => ExtractError::RateLimited(message),
            "blocked" => ExtractError::Blocked(message),
            "network" => ExtractError::Network(message),
            // 连上后被掐断：直连时多为网站在本地网络被拦，走代理时多为这个出口被网站拦了
            "reset" if direct => ExtractError::Network(format!(
                "直连时连接被掐断，多为在本地网络被拦：改走代理再试（{message}）"
            )),
            "reset" => ExtractError::Network(format!(
                "经代理时连接被掐断：代理的出口可能被拦了，换个节点或改直连再试（{message}）"
            )),
            // 连接没建成（超时）：直连时多为网站在本地网络被拦（DNS 被污染到连不通的地址），走代理时多为节点连不上网站
            "unreachable" if direct => ExtractError::Network(format!(
                "直连连不上（{message}），多为在本地网络被拦：改走代理再试"
            )),
            // 经 SOCKS 代理时，代理本身连不上与节点连不上网站都是建立连接超时，分不出
            "unreachable" => ExtractError::Network(format!(
                "经代理连不上（{message}）：代理的节点可能连不上这个网站，换个节点或改直连再试；代理不在本机时也可能是代理本身连不上"
            )),
            "proxy" if direct => ExtractError::Network(message),
            "proxy" => ExtractError::Network(format!(
                "代理出错（{message}）：先确认代理开着、端口对，要登录的代理在设置里填好用户名与密码；代理没问题时多为它的节点连不上这个网站，换个节点或改直连再试"
            )),
            "unavailable" => ExtractError::Unavailable(message),
            "changed" => ExtractError::Changed(message),
            "disk" => ExtractError::Disk(message),
            _ => ExtractError::Other(message),
        }
    }
}

/// 网址所在的可注册域名（`www.bilibili.com` 的为 `bilibili.com`）；认不出公共后缀时为主机本身，没有主机时为 None。
fn site_of(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(psl::domain_str(host).unwrap_or(host).to_owned())
}

/// 交给驱动脚本的用户资料目录；不指定时为 null。
fn profile_wire(source: Option<&CookieSource>) -> Option<String> {
    source
        .and_then(|source| source.profile.as_ref())
        .map(|dir| dir.to_string_lossy().into_owned())
}

/// 从 `source` 读 Cookie 没成的说明：原因 `reason`（见驱动脚本的 `browser_cookies`）与怎么办，后接 yt-dlp 的
/// `detail`。
fn cookie_trouble(source: Option<&CookieSource>, reason: Option<&str>, detail: &str) -> String {
    let browser = source.map(|source| source.browser);
    let name = browser.map_or("浏览器", Browser::label);
    let profile = source.and_then(|source| source.profile.as_ref());
    let chromium =
        browser.is_some_and(|browser| !matches!(browser, Browser::Firefox | Browser::Safari));
    let what = match (reason, profile) {
        (Some("missing"), Some(dir)) => format!(
            "没找到 {name} 的用户资料 {} 的 Cookie：它可能被删了，要重选用户资料",
            dir.display()
        ),
        (Some("missing"), None) => {
            format!("没找到 {name} 的 Cookie：这台电脑上没装它，或没用它打开过网页")
        }
        (Some("denied"), _) => browser.map_or_else(
            || "系统不让读浏览器的 Cookie".to_owned(),
            Browser::denied_hint,
        ),
        (Some("encrypted"), _) => format!(
            "{name} 把 Cookie 加密成了只有它自己解得开的（Windows 上 Chrome 127 起），读不了：改用 Firefox 登录、读 Firefox 的"
        ),
        (Some("locked"), _) => format!("{name} 开着时它的 Cookie 文件被锁着：关掉 {name} 再试"),
        (Some("keychain"), _) => format!(
            "没拿到解开 {name} 的 Cookie 的钥匙：macOS 弹出钥匙串的授权时点「允许」后再试；不想每次弹，改用 Firefox"
        ),
        // 只有 Chromium 系的 Cookie 要钥匙串或密钥环里的钥匙解开
        (Some("stalled"), _) if chromium && cfg!(target_os = "macos") => format!(
            "一直没读出 {name} 的 Cookie：多为 macOS 弹出的钥匙串授权还没点，点「允许」后再试"
        ),
        (Some("stalled"), _) if chromium && cfg!(target_os = "linux") => format!(
            "一直没读出 {name} 的 Cookie：多为系统的密钥环（GNOME 密钥环或 KWallet）在等解锁，解锁后再试"
        ),
        (Some("stalled"), _) => format!("一直没读完 {name} 的 Cookie：关掉 {name} 再试"),
        (Some("keyring"), _) => format!(
            "没从系统的密钥环拿到解开 {name} 的 Cookie 的钥匙：解开密钥环（GNOME 密钥环或 KWallet）后再试，或改用 Firefox"
        ),
        _ => format!("读不出 {name} 的 Cookie"),
    };
    format!("{what}（{detail}）")
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

    /// 解析时读过浏览器的，交回 yt-dlp 下载时改带罐里属于这个网站的 Cookie、不再读浏览器；没读的原样。
    #[test]
    fn downloads_carry_the_cookies_read_instead_of_reading_the_browser_again() {
        let cookie = |name: &str| Cookie {
            domain: ".youtube.com".to_owned(),
            include_subdomains: true,
            path: "/".to_owned(),
            secure: true,
            expires: None,
            name: name.to_owned(),
            value: "v".to_owned(),
        };
        let info = Info {
            extractor: "Youtube".to_owned(),
            id: "x".to_owned(),
            title: String::new(),
            page: None,
            duration: None,
            thumbnail: None,
            live: crate::Live::No,
            uploader: None,
            formats: Vec::new(),
            has_drm: false,
            entries: None,
            warnings: Vec::new(),
            challenge_failed: false,
            site_cookies: vec![cookie("SID")],
        };
        let request = Request {
            url: Url::parse("https://www.youtube.com/watch?v=x").unwrap(),
            cookies: vec![cookie("given")],
            generic: false,
            browser: Some(CookieSource {
                browser: Browser::Chrome,
                profile: None,
            }),
        };
        let download = request.for_download(&info);
        assert_eq!(download.browser, None);
        assert_eq!(download.cookies, [cookie("SID")]);
        let plain = Request {
            browser: None,
            ..request
        };
        assert_eq!(plain.for_download(&info), plain);
    }

    /// 连上后被掐断、连接没建成：直连时说改走代理，走代理时说换节点。
    #[test]
    fn a_cut_off_or_unreachable_connection_says_what_to_change_by_the_route() {
        for kind in ["reset", "unreachable"] {
            let failure = || Failure {
                kind: kind.to_owned(),
                message: "建立连接超时".to_owned(),
                reason: None,
            };
            let direct = failure().into_error(None, true).to_string();
            assert!(direct.contains("改走代理"), "{direct}");
            let proxied = failure().into_error(None, false).to_string();
            assert!(proxied.contains("换个节点"), "{proxied}");
        }
    }

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
