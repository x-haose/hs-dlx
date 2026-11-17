//! 常驻的解析进程：`python -I driver.py serve`，标准输入输出逐行收发 JSON（协议见 driver.py）。进程一次只解析一个，
//! 用完放回去给下一次用；同时最多 [`WORKERS`] 个。一次解析超过时限、被取消、进程退出或回的认不出时杀掉这个进程，
//! 下次另起一个（Python 里打断不了正在跑的解析）。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_m3u8_core::{CancellationToken, Proxy, Url};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Semaphore;

use crate::components::{Installed, clear_proxy_env, proxy_for};
use crate::info::{Info, Wire};

/// 同时最多几个解析进程：每个约占几十 MB 内存，一般一次只解析一个网址。
pub const WORKERS: usize = 2;
/// 标准错误留末尾多少字节：进程意外退出时它是原因。
const STDERR_TAIL: usize = 8192;
/// 一行回应最多多少字节：YouTube 一个视频的结果约 100 KB（格式多、请求头长）。
const REPLY_LIMIT: usize = 64 << 20;

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
    /// 其余：多为网站改了、yt-dlp 要更新
    #[error("{0}")]
    Other(String),
    /// 超过时限没解析完（进程已杀掉）
    #[error("解析超过 {0} 秒没完成")]
    TimedOut(u64),
    /// 解析进程起不来、意外退出或回的认不出：原因是它标准错误的末尾
    #[error("解析进程出错：{0}")]
    Crashed(String),
    /// 系统代理设置读不了
    #[error("{0}")]
    Proxy(String),
    #[error("已取消")]
    Cancelled,
}

/// 一个解析进程。
struct Worker {
    /// 丢弃即杀掉进程（kill_on_drop）
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// 标准错误的末尾，由后台任务一直读着（不读满了会让进程卡住）
    stderr: Arc<Mutex<String>>,
    next_id: u64,
}

pub struct Extractor {
    installed: Installed,
    /// yt-dlp 的缓存目录（YouTube 的播放器数据等）
    cache: PathBuf,
    timeout: Duration,
    idle: Mutex<VecDeque<Worker>>,
    slots: Semaphore,
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
            slot = self.slots.acquire() => slot.expect("信号量不会关闭"),
            () = cancel.cancelled() => return Err(ExtractError::Cancelled),
        };
        let idle = self.idle.lock().expect("锁里不会 panic").pop_front();
        let mut worker = match idle {
            Some(worker) => worker,
            None => self.spawn()?,
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
        let exchange = exchange(&mut worker, line.to_string());
        let reply = tokio::select! {
            reply = tokio::time::timeout(self.timeout, exchange) => match reply {
                Ok(reply) => reply,
                // 丢下 worker 即杀掉进程
                Err(_) => return Err(ExtractError::TimedOut(self.timeout.as_secs())),
            },
            () = cancel.cancelled() => return Err(ExtractError::Cancelled),
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(reason) => return Err(ExtractError::Crashed(with_stderr(&reason, &worker))),
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

    /// 起一个解析进程：隔离模式（不读 PYTHON* 环境变量与用户的 site-packages），不加载 yt-dlp 的插件，不读环境变量里的
    /// 代理。
    fn spawn(&self) -> Result<Worker, ExtractError> {
        let installed = &self.installed;
        let mut command = tokio::process::Command::new(&installed.python);
        command
            .arg("-I")
            .arg(&installed.driver)
            .arg("serve")
            .arg(&installed.site)
            .arg(&installed.deno)
            .arg(&self.cache)
            .env("YTDLP_NO_PLUGINS", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        clear_proxy_env(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| ExtractError::Crashed(format!("起不来：{error}")))?;
        let stdin = child.stdin.take().expect("上面设了 piped");
        let stdout = BufReader::new(child.stdout.take().expect("上面设了 piped"));
        let mut stderr_pipe = child.stderr.take().expect("上面设了 piped");
        let stderr = Arc::new(Mutex::new(String::new()));
        let tail = stderr.clone();
        // 进程退出时管道读到结尾，任务随之结束
        tokio::spawn(async move {
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
            _child: child,
            stdin,
            stdout,
            stderr,
            next_id: 1,
        })
    }
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
            _ => ExtractError::Other(message),
        }
    }
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
