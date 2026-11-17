//! 网页视频任务：每次开始（含继续）都重新解析网页、按任务记着的格式（[`FormatKey`]）认出这次的地址（地址会过期、与
//! 出口绑定），各路（视频、另配的音频）分别下到任务目录里，再合并（两路时）或改名（一路时）成输出。
//!
//! 任务目录（输出旁的 `<输出>.hsdl`）里：每一路下完的文件 `<路>.<扩展名>`（在即这一路下完了）与它的核心任务目录
//! `<路>.hsdl`（续传用；交回 yt-dlp 下的那一路是 yt-dlp 的 `.part`），合并出的 `merged.<扩展名>`（core 先写临时文件、写完
//! 才换上）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hs_dlx_ytdlp::{
    Cookie, ExtractError, Extractor, Format, FormatKey, Info, Protocol, Request, Track,
};
use hs_m3u8_core::remux::{FileInput, Streams, WriteProgress};
use hs_m3u8_core::{
    CancellationToken, FileOutputOptions, FileRequest, JobRequest, Leftover, OutputOptions, Proxy,
    Source, Target, Url,
};
use tokio::task::JoinHandle;

use super::{Event, PROGRESS_INTERVAL, Shared, lock, throttled};
use crate::web::{Container, Route, route};
use crate::{Failure, FailureKind, Finished, Task, TaskId, WebSpec};

/// 换上输出前在任务目录里写下的标记（见 [`placed`]）。
const PLACING: &str = "placing";
/// 自己下时直链被拒或没了，重新解析换新的直链再下的次数。
const RELINKS: usize = 3;

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

/// 一路：任务记着的格式、它在任务目录里的名字与合并时取的流。
struct Lane<'a> {
    key: &'a FormatKey,
    /// `video`、`audio`；只有一路时为 `media`
    role: &'static str,
    streams: Streams,
}

impl Lane<'_> {
    /// 这一路下完的文件：`<路>.<扩展名>`（扩展名新建时校验过，只有字母与数字；没有时为 `bin`）。在即这一路下完了：
    /// 核心与 yt-dlp 都是写完才装上。
    fn file(&self, work_dir: &Path) -> PathBuf {
        let ext = self.key.ext.as_deref().unwrap_or("bin");
        work_dir.join(format!("{}.{ext}", self.role))
    }
}

/// 任务的各路：视频与另配的音频，或只有一路。
fn lanes(spec: &WebSpec) -> Vec<Lane<'_>> {
    match (&spec.video, &spec.audio) {
        (Some(video), Some(audio)) => vec![
            Lane {
                key: video,
                role: "video",
                streams: Streams::Video,
            },
            Lane {
                key: audio,
                role: "audio",
                streams: Streams::Audio,
            },
        ],
        (Some(only), None) | (None, Some(only)) => vec![Lane {
            key: only,
            role: "media",
            streams: Streams::All,
        }],
        (None, None) => unreachable!("存储的约束保证视频与音频至少有一个"),
    }
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
                Err(Stop::Failed(failure)) => {
                    if failure.kind == FailureKind::WebComponents {
                        shared.web.broken(failure.message.clone());
                    }
                    Outcome::Failed(failure)
                }
            };
            (outcome, lock(&leftovers).split_off(0))
        });
        Ok(WebRun { cancel, done })
    }

    /// 跑一次：上次已换上输出（换上后、写进存储前中断）的直接算完成；输出位置被占的先报出；有没下完的路时才解析网页、
    /// 下它们；两路时合并；再换上输出。任务目录留到完成写进存储之后再删（见 `Shared::track_web`）。
    async fn run_web(
        self: &Arc<Self>,
        task: &Task,
        extractor: &Extractor,
        proxy: &Proxy,
        cancel: &CancellationToken,
        leftovers: &Mutex<Vec<Leftover>>,
    ) -> Result<Finished, Stop> {
        let spec = task.spec.web().expect("只为网页视频任务开始");
        let work_dir = task
            .work_dir
            .clone()
            .expect("网页视频任务新建时定下任务目录");
        let output = &spec.path;
        let lanes = lanes(spec);
        let files: Vec<PathBuf> = lanes.iter().map(|lane| lane.file(&work_dir)).collect();
        // 一路时直接换上它，两路时合并进任务目录里的 `merged.<输出的扩展名>` 再换上
        let source = match files.as_slice() {
            [only] => only.clone(),
            _ => {
                let ext = output
                    .extension()
                    .expect("输出的名字新建时带扩展名")
                    .to_string_lossy();
                work_dir.join(format!("merged.{ext}"))
            }
        };
        if placed(&work_dir, &source, output).await? {
            return Ok(Finished {
                duration: None,
                bytes: size_of(output).await?,
                resolution: None,
            });
        }
        if exists(output).await? {
            return Err(output_taken(output));
        }
        tokio::fs::create_dir_all(&work_dir)
            .await
            .map_err(|error| Stop::disk(&work_dir, &error))?;
        // 任务是照专门的解析器建的；网址仍归它时 yt-dlp 照样先用它，归不了了（新版的 yt-dlp 去掉了它）时通用解析器
        // 找到的也接得上：是不是同一个视频由视频编号核对
        let request = Request {
            url: task.spec.url.clone(),
            cookies: cookies(&task.spec.url, &task.spec.headers),
            generic: true,
        };
        let ctx = Ctx {
            id: task.id,
            spec,
            request: &request,
            work_dir: &work_dir,
            extractor,
            proxy,
            cancel,
            leftovers,
        };
        let mut resolution = None;
        // 合并出的还在（上次合并完、换上前中断）时不再合并
        if !exists(&source).await? {
            self.fetch_lanes(ctx, &lanes, &files).await?;
            if files.len() > 1 {
                resolution = self.merge(ctx, &lanes, &files, &source).await?;
            }
        }
        place(&work_dir, &source, output).await?;
        Ok(Finished {
            duration: None,
            bytes: size_of(output).await?,
            resolution,
        })
    }

    /// 解析网页，核对还是同一个视频（视频编号）。
    async fn resolve_web(&self, ctx: Ctx<'_>) -> Result<Info, Stop> {
        self.web_progress(ctx.id, WebProgress::Resolving);
        let info = ctx
            .extractor
            .extract(ctx.request, ctx.proxy, ctx.cancel)
            .await
            .map_err(|error| Stop::extract(&error))?;
        if info.id != ctx.spec.video_id {
            return Err(Stop::failed(
                FailureKind::WebGone,
                format!(
                    "网页现在是另一个视频（{}，原来是 {}）",
                    info.id, ctx.spec.video_id
                ),
            ));
        }
        Ok(info)
    }

    /// 下没下完的各路（`files` 与 `lanes` 一一对应）；都下完了时不解析网页。直链被拒或没了（过期、与出口绑定）时
    /// 重新解析、换新的直链再下，至多 [`RELINKS`] 次。
    async fn fetch_lanes(
        &self,
        ctx: Ctx<'_>,
        lanes: &[Lane<'_>],
        files: &[PathBuf],
    ) -> Result<(), Stop> {
        let mut have = Vec::with_capacity(files.len());
        for file in files {
            have.push(if exists(file).await? {
                Some(size_of(file).await?)
            } else {
                None
            });
        }
        if have.iter().all(Option::is_some) {
            return Ok(());
        }
        let mut info = self.resolve_web(ctx).await?;
        let mut received = 0;
        for (index, (lane, file)) in lanes.iter().zip(files).enumerate() {
            if have[index].is_some() {
                continue;
            }
            let mut relinks = 0;
            loop {
                let format = find(&info, lane.key)?;
                if let Some(until) = format.available_at {
                    self.web_progress(ctx.id, WebProgress::Waiting { until });
                    wait_until(until, ctx.cancel).await?;
                }
                // 已下完的按文件、没下完的按这次的格式给的大小
                let total = lanes
                    .iter()
                    .zip(&have)
                    .map(|(lane, size)| {
                        size.or_else(|| {
                            info.find(lane.key)
                                .and_then(|format| format.size)
                                .map(|size| size.bytes)
                        })
                    })
                    .sum::<Option<u64>>();
                let at = Position {
                    part: index,
                    parts: lanes.len(),
                    done: have.iter().flatten().sum(),
                    total,
                    received,
                };
                match self.fetch_part(ctx, at, lane, format, file).await {
                    Ok(read) => {
                        received += read;
                        break;
                    }
                    Err(Stop::Failed(failure)) if relinks < RELINKS && relinkable(failure.kind) => {
                        relinks += 1;
                        info = self.resolve_web(ctx).await?;
                    }
                    Err(stop) => return Err(stop),
                }
            }
            have[index] = Some(size_of(file).await?);
        }
        Ok(())
    }

    /// 下一路 `lane`（这次解析里认出的格式为 `format`）到 `file`（这一路下完时由核心或 yt-dlp 装上），返回这次从网络
    /// 读到的字节数（交回 yt-dlp 下时按它报的已下字节、减去开始时已有的算）。
    async fn fetch_part(
        &self,
        ctx: Ctx<'_>,
        at: Position,
        lane: &Lane<'_>,
        format: &Format,
        file: &Path,
    ) -> Result<u64, Stop> {
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
                // yt-dlp 报的已下字节含 `.part` 里原有的：以第一次报的为起点
                let read = Mutex::new(None::<(u64, u64)>);
                ctx.extractor
                    .download(
                        ctx.request,
                        lane.key,
                        file,
                        ctx.proxy,
                        &|bytes, _total| {
                            let mut read = lock(&read);
                            let (start, _) = *read.get_or_insert((bytes, bytes));
                            *read = Some((start, bytes));
                            reporter.send(bytes, bytes.saturating_sub(start));
                        },
                        ctx.cancel,
                    )
                    .await
                    .map_err(|error| Stop::extract(&error))?;
                let read = *lock(&read);
                Ok(read.map_or(0, |(start, now)| now.saturating_sub(start)))
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
                let part_dir = ctx.work_dir.join(format!("{}.hsdl", lane.role));
                if format.protocol == Protocol::Hls {
                    let mut output = OutputOptions::new(Target::Mp4(file.to_path_buf()));
                    output.work_dir = Some(part_dir);
                    let mut request = JobRequest::new(source, output);
                    request.live = None;
                    self.fetch_hls(request, ctx, reporter).await
                } else {
                    let mut output = FileOutputOptions::new(file.to_path_buf());
                    output.work_dir = Some(part_dir);
                    let mut request = FileRequest::new(source, output);
                    request.chunk_size = format.chunk_size.and_then(std::num::NonZeroU64::new);
                    // 地址每次解析都不同；网站给的大小不一定准，不拿它核对下到的长度
                    request.identity = Some(identity(ctx.spec, lane.key));
                    self.fetch_file(request, ctx, reporter).await
                }
            }
        }
    }

    /// 文件引擎下一路（每个请求至多取格式给的字节，YouTube 的 10 MiB）。
    async fn fetch_file(
        &self,
        request: FileRequest,
        ctx: Ctx<'_>,
        reporter: Reporter,
    ) -> Result<u64, Stop> {
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

    /// HLS 引擎下一路（点播），写成 MP4。
    async fn fetch_hls(
        &self,
        request: JobRequest,
        ctx: Ctx<'_>,
        reporter: Reporter,
    ) -> Result<u64, Stop> {
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

    /// 把各路下完的 `files` 合并成任务目录里的 `merged`，返回画面的宽高（没有视频时为 None）。合并进度至多每
    /// [`PROGRESS_INTERVAL`] 报一次。
    async fn merge(
        &self,
        ctx: Ctx<'_>,
        lanes: &[Lane<'_>],
        files: &[PathBuf],
        merged: &Path,
    ) -> Result<Option<(u32, u32)>, Stop> {
        let container = match ctx.spec.container {
            Container::Matroska => hs_m3u8_core::remux::Container::Matroska,
            Container::Mp4 | Container::Original => hs_m3u8_core::remux::Container::Mp4,
        };
        let mut total = 0;
        for file in files {
            total += size_of(file).await?;
        }
        let (events, id) = (self.events.clone(), ctx.id);
        let written = Mutex::new((0u64, None::<Instant>));
        let progress = WriteProgress::new(move |amount| {
            let mut written = lock(&written);
            written.0 += amount;
            let now = Instant::now();
            if written.1.is_some_and(|last| now - last < PROGRESS_INTERVAL) {
                return;
            }
            written.1 = Some(now);
            // 接收端随界面关闭而丢弃时没有谁要
            let _closed = events.send(Event::WebProgress {
                id,
                progress: Box::new(WebProgress::Merging {
                    written: written.0,
                    total,
                }),
            });
        });
        let inputs = lanes
            .iter()
            .zip(files)
            .map(|(lane, file)| FileInput {
                path: file.clone(),
                streams: lane.streams,
            })
            .collect();
        // 核心合并完会取消传给它的令牌：给它子令牌，任务自己的不受影响
        let report = hs_m3u8_core::merge_files(
            inputs,
            merged.to_path_buf(),
            container,
            ctx.cancel.child_token(),
            progress,
        )
        .await
        // 失败或取消时核心删掉它写的临时文件，`merged` 要么是完整的、要么不在
        .map_err(|error| Stop::core(&error))?;
        Ok(report.streams.iter().find_map(|stream| match stream.shape {
            hs_m3u8_core::remux::Shape::Video { width, height, .. } => Some((width, height)),
            hs_m3u8_core::remux::Shape::Audio { .. } => None,
        }))
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

/// `key` 在这次解析的 `info` 里认出的格式；认不出时以格式变了报出（不拿编号相同、属性不同的顶上）。
fn find<'a>(info: &'a Info, key: &FormatKey) -> Result<&'a Format, Stop> {
    info.find(key).ok_or_else(|| {
        Stop::failed(
            FailureKind::WebChanged,
            format!(
                "网站这次给的格式里认不出选中的画质（格式 {}）：再试一次；一直这样就放弃后重新新建",
                key.id
            ),
        )
    })
}

/// 自己下时这种失败多为直链过期、被拒或与出口绑定（换了代理）：重新解析换新的直链再下。
fn relinkable(kind: FailureKind) -> bool {
    matches!(
        kind,
        FailureKind::Status(401 | 403 | 404 | 410) | FailureKind::NotExpected
    )
}

/// 文件引擎续传时核对的内容身份：解析器、视频编号与格式的各项（[`FormatKey`]）。存进核心的任务目录，写法改了等于
/// 换了内容，已下的不再接着用。
fn identity(spec: &WebSpec, key: &FormatKey) -> String {
    let track = |track: &Track| match track {
        Track::Absent => "none".to_owned(),
        Track::Unknown => "?".to_owned(),
        Track::Codec(codec) => codec.clone(),
    };
    let or = |value: Option<String>| value.unwrap_or_else(|| "?".to_owned());
    format!(
        "web:{}:{}:{}:{}:{}:{}:{}:{}:{}",
        spec.extractor,
        spec.video_id,
        key.id,
        or(key.ext.clone()),
        track(&key.video),
        track(&key.audio),
        or(key.height.map(|height| height.to_string())),
        or(key.language.clone()),
        or(key.size.map(|size| size.to_string())),
    )
}

fn output_taken(output: &Path) -> Stop {
    Stop::failed(
        FailureKind::OutputTaken,
        format!("保存位置上已有 {}：挪走后再继续", output.display()),
    )
}

/// 上次运行是不是已把 `source` 换上成了 `output`（换上后、完成写进存储前中断）。只有任务目录里有换上的标记
/// （[`PLACING`]）时才可能是：`source` 不在而输出在（改名或硬链接后删掉了源），或两者是同一个文件（硬链接后没来得及
/// 删源，这里删掉）。其余情况标记作废、删掉：输出在则是别人的。
async fn placed(work_dir: &Path, source: &Path, output: &Path) -> Result<bool, Stop> {
    let marker = work_dir.join(PLACING);
    if !exists(&marker).await? {
        return Ok(false);
    }
    let ours = match (exists(source).await?, exists(output).await?) {
        (false, true) => true,
        (true, true) => {
            let (a, b) = (source.to_path_buf(), output.to_path_buf());
            let same = tokio::task::spawn_blocking(move || same_file::is_same_file(&a, &b))
                .await
                .expect("比较文件的阻塞任务不会 panic")
                .map_err(|error| Stop::disk(source, &error))?;
            if same {
                remove(source).await?;
            }
            same
        }
        (_, false) => false,
    };
    if !ours {
        remove(&marker).await?;
    }
    Ok(ours)
}

/// 把 `source` 换上成 `output`，不覆盖已有的文件：先在任务目录里写下标记（见 [`placed`]），能硬链接时硬链接（输出在
/// 时失败）再删源；文件系统不支持硬链接时改名前再看一次输出在不在。
async fn place(work_dir: &Path, source: &Path, output: &Path) -> Result<(), Stop> {
    let marker = work_dir.join(PLACING);
    tokio::fs::write(&marker, b"")
        .await
        .map_err(|error| Stop::disk(&marker, &error))?;
    match tokio::fs::hard_link(source, output).await {
        Ok(()) => remove(source).await,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            remove(&marker).await?;
            Err(output_taken(output))
        }
        // 简化：不支持硬链接的文件系统（FAT、exFAT 等）上先看再改名，两步之间别处写进输出会被覆盖；要杜绝时换成各平台
        // 不覆盖的改名（renameat2、renamex_np、MoveFileEx）。
        Err(_) => {
            if exists(output).await? {
                remove(&marker).await?;
                return Err(output_taken(output));
            }
            tokio::fs::rename(source, output)
                .await
                .map_err(|error| Stop::disk(output, &error))
        }
    }
}

async fn remove(path: &Path) -> Result<(), Stop> {
    tokio::fs::remove_file(path)
        .await
        .map_err(|error| Stop::disk(path, &error))
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

/// 网页视频任务的请求头（已过 `check_option_header`）：只带 Cookie（解析与下载时按网页所在的域名带上，别的请求头
/// yt-dlp 自己定），值里没有制表符（交给 yt-dlp 的 Netscape cookies.txt 以它分列）。不合时报
/// [`crate::Error::InvalidInput`]。
pub(super) fn check_headers(headers: &[(String, String)]) -> Result<(), crate::Error> {
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("cookie") {
            return Err(crate::Error::InvalidInput(format!(
                "网页视频只带 Cookie，不带 {name}"
            )));
        }
        if value.contains('\t') {
            return Err(crate::Error::InvalidInput(
                "Cookie 的值里不能有制表符".to_owned(),
            ));
        }
    }
    Ok(())
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

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hs-dlx-app-web-place-{name}-{}",
            std::process::id()
        ));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("work")).unwrap();
        dir
    }

    /// 换上：输出不在时源换成输出、留下标记；输出在（别人的）时报保存位置被占，不动它，源与标记照旧（标记删掉）。
    #[tokio::test]
    async fn placing_never_overwrites_and_leaves_a_marker() {
        let dir = dir("place");
        let (work, source, output) = (
            dir.join("work"),
            dir.join("work/media.ts"),
            dir.join("out.ts"),
        );
        std::fs::write(&source, b"ours").unwrap();
        std::fs::write(&output, b"theirs").unwrap();
        let taken = place(&work, &source, &output).await;
        assert!(matches!(
            taken,
            Err(Stop::Failed(Failure {
                kind: FailureKind::OutputTaken,
                ..
            }))
        ));
        assert_eq!(std::fs::read(&output).unwrap(), b"theirs");
        assert!(source.exists());
        assert!(!work.join(PLACING).exists());
        std::fs::remove_file(&output).unwrap();
        assert!(place(&work, &source, &output).await.is_ok());
        assert_eq!(std::fs::read(&output).unwrap(), b"ours");
        assert!(!source.exists());
        assert!(work.join(PLACING).exists());
    }

    /// 上次换上后中断：有标记、源不在而输出在，或两者是同一个文件（硬链接后没删源），都算换上了；输出是别的文件或
    /// 没有标记时不算，作废的标记删掉。
    #[tokio::test]
    async fn an_interrupted_placing_is_recognized_only_with_the_marker() {
        let dir = dir("placed");
        let (work, source, output) = (
            dir.join("work"),
            dir.join("work/merged.mp4"),
            dir.join("out.mp4"),
        );
        let marker = work.join(PLACING);
        std::fs::write(&output, b"x").unwrap();
        assert!(!placed(&work, &source, &output).await.ok().unwrap());
        std::fs::write(&marker, b"").unwrap();
        assert!(placed(&work, &source, &output).await.ok().unwrap());
        std::fs::hard_link(&output, &source).unwrap();
        assert!(placed(&work, &source, &output).await.ok().unwrap());
        assert!(!source.exists(), "同一个文件时删掉源");
        std::fs::write(&source, b"x").unwrap();
        assert!(!placed(&work, &source, &output).await.ok().unwrap());
        assert!(!marker.exists(), "输出是别的文件：标记作废");
    }

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
