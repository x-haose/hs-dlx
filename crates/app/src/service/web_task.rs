//! 网页视频任务：每次开始（含继续）都重新解析网页、按任务记着的格式（[`FormatKey`]）认出这次的地址（地址会过期、与
//! 出口绑定），各路（视频、另配的音频）分别下到任务目录里，再合并（两路时）或改名（一路时）成输出。
//!
//! 任务目录（输出旁的 `<输出>.hsdl`）里：每一路下完的文件 `<路>.<扩展名>`（在即这一路下完了）与它的核心任务目录
//! `<路>.hsdl`（续传用；交回 yt-dlp 下的那一路是 yt-dlp 的 `.part`），合并出的 `merged.<扩展名>`（core 先写临时文件、写完
//! 才换上）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hs_dlx_ytdlp::{Cookie, ExtractError, Extractor, Format, FormatKey, Info, Protocol, Request};
use hs_m3u8_core::remux::{FileInput, Streams, WriteProgress};
use hs_m3u8_core::{
    CancellationToken, FileOutputOptions, FileRequest, JobRequest, Leftover, OutputOptions, Proxy,
    Source, Target, Url,
};
use tokio::task::JoinHandle;

use super::{Event, Shared, lock, throttled};
use crate::web::{Container, Route, route};
use crate::{Failure, FailureKind, Finished, Task, TaskId, WebSpec};

/// 运行中的网页视频任务的进度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebProgress {
    /// 在解析网页
    Resolving,
    /// 网站要求到这个时刻（Unix 秒）才能开始下（YouTube 的片头广告时长）
    Waiting { until: i64 },
    /// 在下第 `part` 路（从 0 起），共 `parts` 路；`bytes` 为各路已下的合计、`total` 为各路大小之和（有一路不知道时为
    /// None），`received` 为这次运行从网络读到的字节数（按它的变化算速度）
    Fetching {
        part: usize,
        parts: usize,
        bytes: u64,
        total: Option<u64>,
        received: u64,
    },
    /// 在合并：已写 `written`、共 `total` 字节（各路文件之和）
    Merging { written: u64, total: u64 },
}

/// 开始了的网页视频任务。
pub(super) struct WebRun {
    pub cancel: CancellationToken,
    /// 停下的结果，连同收尾时核心没删掉的东西
    pub done: JoinHandle<(Outcome, Vec<Leftover>)>,
}

/// 网页视频任务停下的结果。
pub(super) enum Outcome {
    Finished(Finished),
    Paused,
    Failed(Failure),
}

/// 一次运行停下的原因。
enum Stop {
    Paused,
    Failed(Failure),
}

impl Stop {
    fn failed(kind: FailureKind, message: impl Into<String>) -> Stop {
        Stop::Failed(Failure {
            kind,
            message: message.into(),
        })
    }

    fn core(error: &hs_m3u8_core::Error) -> Stop {
        match error {
            hs_m3u8_core::Error::Cancelled => Stop::Paused,
            error => Stop::Failed(Failure::of(error)),
        }
    }

    fn extract(error: &ExtractError) -> Stop {
        match error {
            ExtractError::Cancelled => Stop::Paused,
            error => Stop::Failed(Failure::of_extract(error)),
        }
    }

    fn disk(path: &Path, error: &std::io::Error) -> Stop {
        Stop::failed(FailureKind::Disk, format!("{}：{error}", path.display()))
    }
}

/// 一路：任务记着的格式、这次解析里认出的它与它在任务目录里的名字。
struct Part<'a> {
    key: &'a FormatKey,
    format: &'a Format,
    /// `video`、`audio`；只有一路时为 `media`
    role: &'static str,
    streams: Streams,
}

/// 一次运行里各步共用的。
#[derive(Clone, Copy)]
struct Ctx<'a> {
    id: TaskId,
    spec: &'a WebSpec,
    /// 重新解析用的（网页地址、Cookie）；交回 yt-dlp 下时它再解析一次
    request: &'a Request,
    work_dir: &'a Path,
    extractor: &'a Extractor,
    proxy: &'a Proxy,
    cancel: &'a CancellationToken,
    leftovers: &'a Mutex<Vec<Leftover>>,
}

/// 一路在任务里的位置，报进度用。
#[derive(Clone, Copy)]
struct Position {
    part: usize,
    parts: usize,
    /// 之前各路已下的字节
    done: u64,
    total: Option<u64>,
    /// 之前各路这次运行从网络读到的字节
    received: u64,
}

impl Shared {
    /// 开始网页视频任务 `task`（经代理 `proxy`）；解析组件没装好时报 [`crate::Error::WebNotReady`]。调用方须在运行时里。
    pub(super) fn launch_web(
        self: &Arc<Self>,
        task: &Task,
        proxy: Proxy,
    ) -> Result<WebRun, crate::Error> {
        let extractor = lock(&self.web.extractor)
            .clone()
            .ok_or(crate::Error::WebNotReady)?;
        let cancel = CancellationToken::new();
        let shared = self.clone();
        let (task, token) = (task.clone(), cancel.clone());
        let done = tokio::spawn(async move {
            let leftovers = Mutex::new(Vec::new());
            let outcome = match shared
                .run_web(&task, &extractor, &proxy, &token, &leftovers)
                .await
            {
                Ok(finished) => Outcome::Finished(finished),
                Err(Stop::Paused) => Outcome::Paused,
                Err(Stop::Failed(failure)) => Outcome::Failed(failure),
            };
            (outcome, lock(&leftovers).split_off(0))
        });
        Ok(WebRun { cancel, done })
    }

    async fn run_web(
        self: &Arc<Self>,
        task: &Task,
        extractor: &Extractor,
        proxy: &Proxy,
        cancel: &CancellationToken,
        leftovers: &Mutex<Vec<Leftover>>,
    ) -> Result<Finished, Stop> {
        let id = task.id;
        let spec = task.spec.web().expect("只为网页视频任务开始");
        let work_dir = task
            .work_dir
            .clone()
            .expect("网页视频任务新建时定下任务目录");
        self.web_progress(id, WebProgress::Resolving);
        // 任务是照专门的解析器建的；网址仍归它时 yt-dlp 照样先用它，归不了了（新版的 yt-dlp 去掉了它）时通用解析器
        // 找到的也接得上：是不是同一个视频由视频编号核对
        let request = Request {
            url: task.spec.url.clone(),
            cookies: cookies(&task.spec.url, &task.spec.headers),
            generic: true,
        };
        let info = extractor
            .extract(&request, proxy, cancel)
            .await
            .map_err(|error| Stop::extract(&error))?;
        let parts = parts(&info, spec)?;
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|error| Stop::disk(&work_dir, &error))?;
        if let Some(until) = parts
            .iter()
            .filter_map(|part| part.format.available_at)
            .max()
        {
            self.web_progress(id, WebProgress::Waiting { until });
            wait_until(until, cancel).await?;
        }
        let ctx = Ctx {
            id,
            spec,
            request: &request,
            work_dir: &work_dir,
            extractor,
            proxy,
            cancel,
            leftovers,
        };
        let total = parts
            .iter()
            .map(|part| part.format.size.map(|size| size.bytes))
            .sum::<Option<u64>>();
        let (mut done, mut received) = (0, 0);
        let mut files = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            let file = part_file(&work_dir, part);
            if !exists(&file).await? {
                let at = Position {
                    part: index,
                    parts: parts.len(),
                    done,
                    total,
                    received,
                };
                received += self.fetch_part(ctx, at, part, &file).await?;
            }
            done += size_of(&file).await?;
            files.push(file);
        }
        let finished = self.assemble(ctx, &parts, &files).await?;
        // 各路下完的文件已合并或改名，剩下的是空了的任务目录
        let removed = tokio::fs::remove_dir_all(&work_dir).await;
        if let Err(error) = removed
            && error.kind() != std::io::ErrorKind::NotFound
        {
            // 输出已写好，留下的目录不影响结果
            let _closed = self.events.send(Event::Orphaned {
                paths: vec![work_dir],
                reason: format!("删不掉：{error}"),
            });
        }
        Ok(finished)
    }

    /// 下一路 `part` 到 `file`（这一路下完时由核心或 yt-dlp 装上），返回这次从网络读到的字节数（交回 yt-dlp 下时按它报
    /// 的已下字节算）。
    async fn fetch_part(
        &self,
        ctx: Ctx<'_>,
        at: Position,
        part: &Part<'_>,
        file: &Path,
    ) -> Result<u64, Stop> {
        let format = part.format;
        let reporter = Reporter {
            events: self.events.clone(),
            id: ctx.id,
            at,
        };
        match route(format) {
            Route::Unsupported(reason) => Err(Stop::failed(
                FailureKind::Unsupported,
                format!("格式 {} {reason}", format.id),
            )),
            Route::YtDlp => {
                let read = Mutex::new(0u64);
                ctx.extractor
                    .download(
                        ctx.request,
                        part.key,
                        file,
                        ctx.proxy,
                        &|bytes, _total| {
                            *lock(&read) = bytes;
                            reporter.send(bytes, bytes);
                        },
                        ctx.cancel,
                    )
                    .await
                    .map_err(|error| Stop::extract(&error))?;
                let read = *lock(&read);
                Ok(read)
            }
            Route::Own => {
                let url = Url::parse(&format.url).map_err(|error| {
                    Stop::failed(
                        FailureKind::WebExtract,
                        format!("格式 {} 的地址认不出：{error}", format.id),
                    )
                })?;
                let mut source = Source::new(url);
                source.http.headers.clone_from(&format.headers);
                if let Some(cookie) = &format.cookie {
                    source
                        .http
                        .headers
                        .push(("Cookie".to_owned(), cookie.clone()));
                }
                source.http.proxy = ctx.proxy.clone();
                let part_dir = ctx.work_dir.join(format!("{}.hsdl", part.role));
                if format.protocol == Protocol::Hls {
                    self.fetch_hls(source, file, part_dir, ctx, reporter).await
                } else {
                    self.fetch_file(source, format, file, part_dir, ctx, reporter)
                        .await
                }
            }
        }
    }

    /// 文件引擎下一路：每个请求至多取格式给的字节（YouTube 的 10 MiB），以「解析器:视频编号:格式编号」担保内容身份
    /// （地址每次解析都不同），按格式给的确切大小核对。
    async fn fetch_file(
        &self,
        source: Source,
        format: &Format,
        file: &Path,
        part_dir: PathBuf,
        ctx: Ctx<'_>,
        reporter: Reporter,
    ) -> Result<u64, Stop> {
        let mut output = FileOutputOptions::new(file.to_path_buf());
        output.work_dir = Some(part_dir);
        let mut request = FileRequest::new(source, output);
        request.chunk_size = format.chunk_size.and_then(std::num::NonZeroU64::new);
        request.identity = Some(format!(
            "web:{}:{}:{}",
            ctx.spec.extractor, ctx.spec.video_id, format.id
        ));
        request.expected =
            format
                .size
                .filter(|size| size.exact)
                .map(|size| hs_m3u8_core::Expected {
                    length: Some(size.bytes),
                    ..hs_m3u8_core::Expected::default()
                });
        let job = self
            .engine
            .start_file(request)
            .map_err(|error| Stop::core(&error))?;
        let control = job.control();
        let finished = CancellationToken::new();
        let forwarding = tokio::spawn(throttled(
            control.progress(),
            finished.clone(),
            move |progress: hs_m3u8_core::FileProgress| {
                reporter.send(progress.bytes, progress.received);
            },
        ));
        let waiting = job.wait();
        tokio::pin!(waiting);
        let result = tokio::select! {
            result = &mut waiting => result,
            () = ctx.cancel.cancelled() => {
                control.cancel();
                waiting.await
            }
        };
        finished.cancel();
        forwarding
            .await
            .expect("转发进度的任务不会 panic，运行时关闭时本任务也一起丢弃");
        let received = control.progress().borrow().received;
        match result {
            Ok(output) => {
                lock(ctx.leftovers).extend(output.leftovers);
                Ok(received)
            }
            Err(error) => Err(Stop::core(&error)),
        }
    }

    /// HLS 引擎下一路（点播），写成 MP4（`file`）。
    async fn fetch_hls(
        &self,
        source: Source,
        file: &Path,
        part_dir: PathBuf,
        ctx: Ctx<'_>,
        reporter: Reporter,
    ) -> Result<u64, Stop> {
        let mut output = OutputOptions::new(Target::Mp4(file.to_path_buf()));
        output.work_dir = Some(part_dir);
        let mut request = JobRequest::new(source, output);
        request.live = None;
        let job = self
            .engine
            .start(request)
            .map_err(|error| Stop::core(&error))?;
        let control = job.control();
        let finished = CancellationToken::new();
        let forwarding = tokio::spawn(throttled(
            control.progress(),
            finished.clone(),
            move |progress: hs_m3u8_core::Progress| {
                reporter.send(progress.bytes, progress.received);
            },
        ));
        let waiting = job.wait();
        tokio::pin!(waiting);
        let result = tokio::select! {
            result = &mut waiting => result,
            () = ctx.cancel.cancelled() => {
                control.cancel();
                waiting.await
            }
        };
        finished.cancel();
        forwarding
            .await
            .expect("转发进度的任务不会 panic，运行时关闭时本任务也一起丢弃");
        let received = control.progress().borrow().received;
        match result {
            Ok(output) => {
                lock(ctx.leftovers).extend(output.leftovers);
                Ok(received)
            }
            Err(error) => Err(Stop::core(&error)),
        }
    }

    /// 各路下完的 `files` 变成输出：两路时合并进任务目录里的 `merged`、再改名，一路时直接改名。输出位置上已有
    /// 东西时不覆盖，以保存位置被占报出。
    async fn assemble(
        &self,
        ctx: Ctx<'_>,
        parts: &[Part<'_>],
        files: &[PathBuf],
    ) -> Result<Finished, Stop> {
        let output = &ctx.spec.path;
        if exists(output).await? {
            return Err(Stop::failed(
                FailureKind::OutputTaken,
                format!("保存位置上已有 {}：挪走后再继续", output.display()),
            ));
        }
        let mut resolution = None;
        let source = if files.len() == 1 {
            files[0].clone()
        } else {
            let container = match ctx.spec.container {
                Container::Matroska => hs_m3u8_core::remux::Container::Matroska,
                Container::Mp4 | Container::Original => hs_m3u8_core::remux::Container::Mp4,
            };
            let ext = output
                .extension()
                .map_or_else(String::new, |ext| ext.to_string_lossy().into_owned());
            let merged = ctx.work_dir.join(format!("merged.{ext}"));
            let total = files
                .iter()
                .map(|file| file.metadata().map_or(0, |m| m.len()))
                .sum();
            let (events, id) = (self.events.clone(), ctx.id);
            let written = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let progress = WriteProgress::new(move |amount| {
                let now = written.fetch_add(amount, std::sync::atomic::Ordering::Relaxed) + amount;
                // 接收端随界面关闭而丢弃时没有谁要
                let _closed = events.send(Event::WebProgress {
                    id,
                    progress: Box::new(WebProgress::Merging {
                        written: now,
                        total,
                    }),
                });
            });
            let inputs = parts
                .iter()
                .zip(files)
                .map(|(part, file)| FileInput {
                    path: file.clone(),
                    streams: part.streams,
                })
                .collect();
            let merging = hs_m3u8_core::merge_files(
                inputs,
                merged.clone(),
                container,
                ctx.cancel.clone(),
                progress,
            )
            .await;
            // 失败或取消时 core 删掉它写的临时文件，`merged` 要么是完整的、要么不在
            let report = merging.map_err(|error| Stop::core(&error))?;
            resolution = report.streams.iter().find_map(|stream| match stream.shape {
                hs_m3u8_core::remux::Shape::Video { width, height, .. } => Some((width, height)),
                hs_m3u8_core::remux::Shape::Audio { .. } => None,
            });
            merged
        };
        tokio::fs::rename(&source, output)
            .await
            .map_err(|error| Stop::disk(output, &error))?;
        let bytes = size_of(output).await?;
        Ok(Finished {
            duration: None,
            bytes,
            resolution,
        })
    }

    fn web_progress(&self, id: TaskId, progress: WebProgress) {
        // 接收端随界面关闭而丢弃时没有谁要
        let _closed = self.events.send(Event::WebProgress {
            id,
            progress: Box::new(progress),
        });
    }

    /// 放弃网页视频任务的文件：任务目录（各路的核心任务目录、下完的各路、合并的半成品）。输出要么不在（没做完），要么
    /// 已是完整的（做完了的任务不放弃）。删不掉时报 [`crate::Error::ContentLeft`]。
    pub(super) async fn discard_web(
        &self,
        id: TaskId,
        work_dir: Option<&Path>,
    ) -> Result<(), crate::Error> {
        let Some(work_dir) = work_dir else {
            return Ok(());
        };
        remove_work_dir(work_dir)
            .await
            .map_err(|cause| crate::Error::ContentLeft {
                id,
                cause: Box::new(cause),
            })
    }
}

/// 删掉网页视频任务的任务目录 `work_dir`（没有时什么也不做）。
pub(super) async fn remove_work_dir(work_dir: &Path) -> Result<(), crate::Error> {
    match tokio::fs::remove_dir_all(work_dir).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(cause) => Err(crate::Error::Io {
            action: "删除",
            path: work_dir.to_path_buf(),
            cause,
        }),
    }
}

/// 发进度：一路的字节加上之前各路的。
#[derive(Clone)]
struct Reporter {
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    id: TaskId,
    at: Position,
}

impl Reporter {
    fn send(&self, bytes: u64, received: u64) {
        // 接收端随界面关闭而丢弃时没有谁要
        let _closed = self.events.send(Event::WebProgress {
            id: self.id,
            progress: Box::new(WebProgress::Fetching {
                part: self.at.part,
                parts: self.at.parts,
                bytes: self.at.done + bytes,
                total: self.at.total,
                received: self.at.received + received,
            }),
        });
    }
}

/// 照任务记着的格式在这次解析的 `info` 里认出各路；视频编号对不上（网页现在是别的视频）时以视频没了报出，认不出
/// 选中的格式时以格式变了报出（不拿编号相同、属性不同的顶上）。
fn parts<'a>(info: &'a Info, spec: &'a WebSpec) -> Result<Vec<Part<'a>>, Stop> {
    if info.id != spec.video_id {
        return Err(Stop::failed(
            FailureKind::WebGone,
            format!(
                "网页现在是另一个视频（{}，原来是 {}）",
                info.id, spec.video_id
            ),
        ));
    }
    let find = |key: &FormatKey| {
        info.find(key).ok_or_else(|| {
            Stop::failed(
                FailureKind::WebChanged,
                format!(
                    "网站这次给的格式里认不出选中的画质（格式 {}）：再试一次；一直这样就放弃后重新新建",
                    key.id
                ),
            )
        })
    };
    Ok(match (&spec.video, &spec.audio) {
        (Some(video), Some(audio)) => vec![
            Part {
                key: video,
                format: find(video)?,
                role: "video",
                streams: Streams::Video,
            },
            Part {
                key: audio,
                format: find(audio)?,
                role: "audio",
                streams: Streams::Audio,
            },
        ],
        (Some(only), None) | (None, Some(only)) => vec![Part {
            key: only,
            format: find(only)?,
            role: "media",
            streams: Streams::All,
        }],
        (None, None) => unreachable!("存储的约束保证视频与音频至少有一个"),
    })
}

/// 一路下完的文件在任务目录里的位置：`<路>.<扩展名>`（扩展名新建时校验过，只有字母与数字；没有时为 `bin`）。
fn part_file(work_dir: &Path, part: &Part<'_>) -> PathBuf {
    let ext = part.key.ext.as_deref().unwrap_or("bin");
    work_dir.join(format!("{}.{ext}", part.role))
}

/// 等到 `until`（Unix 秒）；取消时报暂停。
async fn wait_until(until: i64, cancel: &CancellationToken) -> Result<(), Stop> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let wait = u64::try_from(until).unwrap_or(0).saturating_sub(now);
    if wait == 0 {
        return Ok(());
    }
    cancel
        .run_until_cancelled(tokio::time::sleep(Duration::from_secs(wait)))
        .await
        .ok_or(Stop::Paused)
}

async fn exists(path: &Path) -> Result<bool, Stop> {
    tokio::fs::try_exists(path)
        .await
        .map_err(|error| Stop::disk(path, &error))
}

async fn size_of(path: &Path) -> Result<u64, Stop> {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.len())
        .map_err(|error| Stop::disk(path, &error))
}

/// 任务请求头里的 Cookie（网页视频只存它）给解析用：发往网页所在的可注册域名（含子域）。网站的接口常在另一个子域
/// （B 站的 api.bilibili.com），只给网页的主机不够。
pub(super) fn cookies(page: &Url, headers: &[(String, String)]) -> Vec<Cookie> {
    let Some(host) = page.host_str() else {
        return Vec::new();
    };
    let domain = psl::domain_str(host).unwrap_or(host).to_owned();
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
        .flat_map(|(_, value)| value.split(';'))
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (!name.is_empty()).then(|| Cookie {
                domain: domain.clone(),
                include_subdomains: true,
                path: "/".to_owned(),
                secure: false,
                expires: None,
                name: name.to_owned(),
                value: value.to_owned(),
            })
        })
        .collect()
}

/// 网页视频任务停下的结果对应的状态与完成时的结果。
pub(super) fn ending(outcome: Outcome) -> (crate::Status, Option<Finished>) {
    match outcome {
        Outcome::Finished(finished) => (crate::Status::Done, Some(finished)),
        Outcome::Paused => (crate::Status::Paused, None),
        Outcome::Failed(failure) => (crate::Status::Failed(failure), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_go_to_the_registrable_domain_of_the_page() {
        let page = Url::parse("https://www.bilibili.com/video/BV1").unwrap();
        let found = cookies(
            &page,
            &[
                (
                    "Cookie".to_owned(),
                    "SESSDATA=a; bili_jct=b; =x; bad".to_owned(),
                ),
                ("Referer".to_owned(), "https://x".to_owned()),
            ],
        );
        let pairs: Vec<(&str, &str, &str)> = found
            .iter()
            .map(|c| (c.domain.as_str(), c.name.as_str(), c.value.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("bilibili.com", "SESSDATA", "a"),
                ("bilibili.com", "bili_jct", "b")
            ]
        );
        assert!(found.iter().all(|c| c.include_subdomains));
    }
}
