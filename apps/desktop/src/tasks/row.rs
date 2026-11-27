//! 任务列表里的一行：存储里的任务，加上这次运行收到的进度、结果、信号与速度。界面上的样子见 `crate::shell` 的 look。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hs_dlx_app::{
    CloudFile, CloudProgress, Finished, Kind, OutputSpec, SavedTorrent, Status, Task, TaskSpec,
    TorrentProgress, WebProgress,
};
use hs_m3u8_core::{FileCount, FileProgress, Progress, Saved, Stage};

use crate::format;
use crate::live::Timeline;
use crate::rate::Rate;
use crate::signals::{self, Level, Signal};

/// 写出的一种输出。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Mp4,
    Hls,
}

/// 任务目录里已完成的内容（BT 任务为存着的续传数据）读到了哪一步。
pub enum Disk {
    Unread,
    /// 读不出：原因
    Unreadable(String),
    /// HLS 与文件任务读到了；目录或其中的记录不在时为 None
    Read(Option<Saved>),
    /// BT 任务读到了
    Torrent(SavedTorrent),
    /// 云离线任务读到了：存储里记着的文件（列出文件之前为空）
    Cloud(Vec<CloudFile>),
    /// 网页视频任务读到了：任务目录里已下的字节（各路合计；总量要重新解析才知道）
    Web(u64),
}

/// 完成任务的输出此刻在不在：完成时与打开应用时在后台查一次，渲染时不碰文件系统。
pub struct Present {
    /// 还在的 MP4，可以播放
    pub mp4: Option<PathBuf>,
    /// 在访达里显示哪个：MP4，没有时为本地 HLS 目录
    pub shown: Option<PathBuf>,
}

pub struct Row {
    pub task: Task,
    /// HLS 任务这次运行最近的进度；继续时清掉
    pub progress: Option<Progress>,
    /// 文件任务这次运行最近的进度；继续时清掉
    pub file_progress: Option<FileProgress>,
    /// BT 任务这次运行最近的进度；继续时清掉
    pub torrent_progress: Option<TorrentProgress>,
    /// 云离线任务这次运行最近的进度；继续时清掉
    pub cloud_progress: Option<CloudProgress>,
    /// 网页视频任务这次运行最近的进度；继续时清掉
    pub web_progress: Option<WebProgress>,
    /// 任务不在运行时任务目录里已完成的内容；停下时作废重读，运行中留着继续前读到的
    pub disk: Disk,
    /// 读任务目录的代次：每次作废 `disk` 时加一，只收发起时代次相同的结果
    pub reading: u64,
    /// 完成任务的输出在不在；还没查时为 None
    pub present: Option<Present>,
    pub rate: Rate,
    /// 按时间先后，最多 [`signals::KEEP`] 条
    pub signals: Vec<Signal>,
    /// 输出的文件名（同时要 MP4 与本地 HLS 时为 MP4 的）
    pub file_name: String,
    /// 点了继续、还在读任务目录：操作都不可用
    pub resuming: bool,
    /// 正在放弃：操作都不可用
    pub discarding: bool,
    /// 正在从列表移除（做种的先停下）：操作都不可用
    pub forgetting: bool,
    /// 直播录制这次打开应用以来的时间线（钟面用）
    pub timeline: Timeline,
    /// 开始只写出已录的那一刻的已录时长：那次运行在核心报出进度之前按它显示
    pub recorded_before_writing: Duration,
}

impl Row {
    pub fn new(task: Task, now: Instant) -> Row {
        // 存储里的时间是本应用写的 Unix 秒；换不成本地时刻（超出能表示的范围）时不写时刻
        let at =
            crate::clock::local(task.created_at, "%m-%d %H:%M").unwrap_or_else(|| "—".to_owned());
        let file_name = file_name(&task.spec);
        let mut signals = Vec::new();
        signals::push(
            &mut signals,
            at,
            Level::Info,
            format!("新建 · 保存为 {file_name}"),
        );
        // 打开应用时就已失败的：完整的错误只在存储里，补进信号
        if let Status::Failed(failure) = &task.status {
            signals::push(
                &mut signals,
                "上次".to_owned(),
                Level::Error,
                failure.message.clone(),
            );
        }
        Row {
            task,
            progress: None,
            file_progress: None,
            torrent_progress: None,
            cloud_progress: None,
            web_progress: None,
            disk: Disk::Unread,
            reading: 0,
            present: None,
            rate: Rate::new(now, Some(0)),
            signals,
            file_name,
            resuming: false,
            discarding: false,
            forgetting: false,
            timeline: Timeline::default(),
            recorded_before_writing: Duration::ZERO,
        }
    }

    /// 换掉任务的内容（如改存本地 HLS 之后），显示的文件名随之更新。
    pub fn set_spec(&mut self, spec: TaskSpec) {
        self.file_name = file_name(&spec);
        self.task.spec = spec;
    }

    pub fn signal(&mut self, level: Level, text: String) {
        let at = crate::clock::now("%H:%M:%S");
        signals::push(&mut self.signals, at, level, text);
    }

    /// 能不能操作：正在继续、放弃或从列表移除时不能。
    pub fn idle(&self) -> bool {
        !self.resuming && !self.discarding && !self.forgetting
    }

    /// 已录（直播）或已下（点播）的第 0 条轨时长：运行中按进度，停下的按任务目录里读到的；都没有时为 0。
    pub fn recorded(&self) -> Duration {
        Amount::of(self).map_or(Duration::ZERO, |a| a.duration)
    }

    /// 已完成的片数与字节数，来源同 [`Row::recorded`]；都没有时为 None。
    pub fn completed(&self) -> Option<(usize, u64)> {
        Amount::of(self).map(|a| (a.done, a.bytes))
    }

    /// 能停止并写出：在运行的直播录制（还在准备的也算：会话定下之前停止即结束，写出已录的）。点播停止不起作用，要暂停。
    pub fn stoppable(&self) -> bool {
        self.task.status == Status::Running && self.recording()
    }

    /// 作废读到的任务目录内容（任务停下时：内容变了），之后发起的读才算数。
    pub fn forget_disk(&mut self) {
        self.disk = Disk::Unread;
        self.reading += 1;
    }

    /// 进度里报的写出计数：写的是哪种输出与写了几个文件；同时写 MP4 与本地 HLS 时为 MP4 的。还没报时为 None。
    pub fn writing(&self) -> Option<(Output, FileCount)> {
        let writing = self.progress.as_ref()?.writing?;
        writing
            .mp4
            .map(|count| (Output::Mp4, count))
            .or(writing.hls.map(|count| (Output::Hls, count)))
    }

    /// 正在写出时写出的比例（0 到 1，按 [`Row::writing`] 的文件数，还没报时为 0）；不在写出阶段时为 None。
    pub fn written_share(&self) -> Option<f32> {
        if self.progress.as_ref()?.stage != Stage::Writing {
            return None;
        }
        Some(self.writing().map_or(0.0, |(_, c)| {
            (c.done as f32 / c.total.max(1) as f32).clamp(0.0, 1.0)
        }))
    }

    /// 在运行的点播或文件此刻的进度（0 到 1）：点播写出时为写出的比例，下载时为已下的片数比例；文件为已下的字节比例；
    /// 还不知道合计时为 0。不是在运行的点播或文件（停着的、录直播的、只写出已录的）时为 None。
    pub fn progress_share(&self) -> Option<f32> {
        if self.task.status != Status::Running || self.task.write_only || self.recording() {
            return None;
        }
        if let Some(amount) = FileAmount::of(self) {
            return Some(amount.fraction().unwrap_or(0.0));
        }
        Some(
            self.written_share()
                .unwrap_or_else(|| Amount::of(self).map_or(0.0, |amount| amount.fraction())),
        )
    }

    /// 是直播的录制：运行中过了准备阶段的按阶段；其余（停下的、还在准备的）按任务目录里的记录（继续前读到的），
    /// 没读到时按新建时的选择。按直播新建、实际是点播的，进度到了下载阶段或读到任务目录后就不算。
    pub fn recording(&self) -> bool {
        if self.task.status == Status::Running && self.task.write_only {
            return false;
        }
        if let Some(p) = &self.progress
            && self.task.status == Status::Running
        {
            match p.stage {
                Stage::Recording => return true,
                Stage::Downloading | Stage::Writing | Stage::Done => return false,
                Stage::Preparing => {}
            }
        }
        match &self.disk {
            Disk::Read(Some(Saved::Hls(saved))) => saved.live,
            Disk::Read(Some(Saved::File(_))) | Disk::Torrent(_) | Disk::Cloud(_) => false,
            _ => self.task.spec.hls().is_some_and(|hls| hls.live),
        }
    }
}

/// 已完成、合计（不知道时为 0）、字节数与第 0 条轨的时长。
#[derive(Default)]
pub struct Amount {
    pub done: usize,
    /// 不知道时为 0
    pub total: usize,
    pub bytes: u64,
    pub duration: Duration,
}

impl Amount {
    /// HLS 任务的：运行中取这次运行的进度；停下的取任务目录里读到的（与存储里的总片数），还没读到时取这次运行留下的
    /// 进度。文件任务为 None（见 [`FileAmount`]）。
    pub fn of(row: &Row) -> Option<Amount> {
        row.task.spec.hls()?;
        let from_progress = |p: &Progress| Amount {
            done: p.segments.done,
            total: p.segments.total,
            bytes: p.bytes,
            duration: p.duration,
        };
        if row.task.status == Status::Running {
            return row.progress.as_ref().map(from_progress);
        }
        match &row.disk {
            Disk::Read(Some(Saved::Hls(saved))) => Some(Amount {
                done: saved.segments,
                total: row.task.total.unwrap_or(0),
                bytes: saved.bytes,
                duration: saved.duration,
            }),
            // 存储里是 HLS 任务、目录里却是文件任务的：服务按种类开始任务，读到时核心已报目录种类不符
            Disk::Read(Some(Saved::File(_)) | None)
            | Disk::Torrent(_)
            | Disk::Cloud(_)
            | Disk::Web(_) => None,
            Disk::Unread | Disk::Unreadable(_) => row.progress.as_ref().map(from_progress),
        }
    }

    /// 已完成的比例（0 到 1）；不知道合计时为 0。
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32).min(1.0)
        }
    }
}

/// 文件、BT 与云离线任务已下的与总字节数（BT 只算选了的文件）。
pub struct FileAmount {
    pub bytes: u64,
    /// 服务器没给出时为 None
    pub total: Option<u64>,
}

impl FileAmount {
    /// 文件、BT 与云离线任务的：运行中取这次运行的进度；停下的取任务目录里（BT 为续传数据里，云离线为存储里记着的
    /// 文件）读到的，还没读到时取这次运行留下的进度。BT 任务上次没正常停下、已有的块待校验时为 None；云离线任务还没
    /// 列出文件时（网盘还在下）为 None。HLS 任务为 None（见 [`Amount`]）。
    pub fn of(row: &Row) -> Option<FileAmount> {
        match &row.task.spec.kind {
            Kind::Pending(_) | Kind::Hls(_) => None,
            Kind::File(_) => Self::of_file(row),
            Kind::Torrent(_) => Self::of_torrent(row),
            Kind::Cloud(_) => Self::of_cloud(row),
            Kind::Web(_) => Self::of_web(row),
        }
    }

    /// 网页视频任务的：运行中按这次运行的进度（下载时为各路合计，合并时为写了的）；停着的按读到的任务目录（总量要重新
    /// 解析才知道）。
    fn of_web(row: &Row) -> Option<FileAmount> {
        if row.task.status != Status::Running {
            return match row.disk {
                Disk::Web(bytes) => Some(FileAmount { bytes, total: None }),
                Disk::Unread
                | Disk::Unreadable(_)
                | Disk::Read(_)
                | Disk::Torrent(_)
                | Disk::Cloud(_) => None,
            };
        }
        match row.web_progress.as_ref()? {
            WebProgress::Fetching { bytes, total, .. } => Some(FileAmount {
                bytes: *bytes,
                total: *total,
            }),
            WebProgress::Merging { written, total } => Some(FileAmount {
                bytes: *written,
                total: Some(*total),
            }),
            WebProgress::Resolving | WebProgress::Waiting { .. } => None,
        }
    }

    fn of_cloud(row: &Row) -> Option<FileAmount> {
        let from_progress = |p: &CloudProgress| match p {
            CloudProgress::Fetching {
                done, total, file, ..
            } => Some(FileAmount {
                bytes: done + file.bytes,
                total: Some(*total),
            }),
            CloudProgress::Verifying {
                done,
                total,
                checked,
                ..
            } => Some(FileAmount {
                bytes: done + checked,
                total: Some(*total),
            }),
            CloudProgress::Remote { .. }
            | CloudProgress::Listing
            | CloudProgress::Skipped { .. } => None,
        };
        if row.task.status == Status::Running {
            return row.cloud_progress.as_ref().and_then(from_progress);
        }
        match &row.disk {
            Disk::Cloud(files) if !files.is_empty() => Some(FileAmount {
                bytes: files.iter().filter(|f| f.done).map(|f| f.size).sum(),
                total: Some(files.iter().filter(|f| !f.skipped).map(|f| f.size).sum()),
            }),
            Disk::Cloud(_) | Disk::Read(_) | Disk::Torrent(_) | Disk::Web(_) => None,
            Disk::Unread | Disk::Unreadable(_) => {
                row.cloud_progress.as_ref().and_then(from_progress)
            }
        }
    }

    fn of_file(row: &Row) -> Option<FileAmount> {
        let from_progress = |p: &FileProgress| FileAmount {
            bytes: p.bytes,
            total: p.total,
        };
        if row.task.status == Status::Running {
            return row.file_progress.as_ref().map(from_progress);
        }
        match &row.disk {
            Disk::Read(Some(Saved::File(saved))) => Some(FileAmount {
                bytes: saved.bytes,
                total: saved.total,
            }),
            Disk::Read(Some(Saved::Hls(_)) | None)
            | Disk::Torrent(_)
            | Disk::Cloud(_)
            | Disk::Web(_) => None,
            Disk::Unread | Disk::Unreadable(_) => row.file_progress.as_ref().map(from_progress),
        }
    }

    fn of_torrent(row: &Row) -> Option<FileAmount> {
        let from_progress = |p: &TorrentProgress| FileAmount {
            bytes: p.done,
            total: Some(p.wanted),
        };
        if row.task.status == Status::Running {
            return row.torrent_progress.as_ref().map(from_progress);
        }
        match &row.disk {
            Disk::Torrent(saved) => saved.trusted.then(|| FileAmount {
                bytes: saved.info.done,
                total: Some(saved.info.wanted()),
            }),
            Disk::Read(_) | Disk::Cloud(_) | Disk::Web(_) => None,
            Disk::Unread | Disk::Unreadable(_) => row.torrent_progress.as_ref().map(from_progress),
        }
    }

    /// 已下的比例（0 到 1）；不知道总字节数时为 None。
    pub fn fraction(&self) -> Option<f32> {
        self.total
            .filter(|&total| total > 0)
            .map(|total| (self.bytes as f64 / total as f64).min(1.0) as f32)
    }
}

/// 主要输出（[`TaskSpec::outputs`] 的首项）的文件名；还没定下输出的云离线任务为任务的标题。
pub(super) fn file_name(spec: &TaskSpec) -> String {
    main_output(spec).map_or_else(
        || spec.title.clone(),
        |output| {
            output
                .file_name()
                .expect("存储读回任务时已校验输出路径以文件名结尾")
                .to_string_lossy()
                .into_owned()
        },
    )
}

/// 主要输出（[`TaskSpec::outputs`] 的首项）；还没定下内容的根的云离线任务没有。
pub fn main_output(spec: &TaskSpec) -> Option<&Path> {
    spec.outputs().first().copied()
}

/// 存成了什么：HLS 为输出的种类，文件任务为「文件」，BT 任务为「BT」，云离线任务为「PikPak 云离线」，网页视频为
/// 「网页视频」。
pub fn saved_as(spec: &TaskSpec) -> &'static str {
    match &spec.kind {
        Kind::Hls(hls) => match hls.output {
            OutputSpec::Mp4(_) => "MP4",
            OutputSpec::Hls(_) => "本地 HLS",
            OutputSpec::Both { .. } => "MP4 与本地 HLS",
        },
        Kind::Pending(_) => "还没定下",
        Kind::File(_) => "文件",
        Kind::Torrent(_) => "BT",
        Kind::Cloud(_) => "PikPak 云离线",
        Kind::Web(_) => "网页视频",
    }
}

/// 完成时的说明：存成了什么、视频的宽高与大小（`MP4 · 1920×1080 · 1.4 GB`）。
pub fn finished_text(spec: &TaskSpec, result: &Finished) -> String {
    let mut parts = vec![saved_as(spec).to_owned()];
    if let Some((width, height)) = result.resolution {
        parts.push(format!("{width}×{height}"));
    }
    parts.push(format::bytes(result.bytes));
    parts.join(" · ")
}

/// 测试用的行与任务目录里的内容。
#[cfg(test)]
pub mod sample {
    use hs_dlx_app::{HlsSpec, Kind, OutputSpec, Status, StoredPreference, Task, TaskId, TaskSpec};
    use hs_m3u8_core::hls::Preference;
    use hs_m3u8_core::{HlsSaved, Saved};
    use std::time::{Duration, Instant};

    use super::Row;

    pub fn row(status: Status, live: bool, total: Option<usize>) -> Row {
        let task = Task {
            id: TaskId(1),
            spec: TaskSpec {
                title: "talk".into(),
                url: "https://a.example/talk.m3u8".parse().unwrap(),
                headers: Vec::new(),
                kind: Kind::Hls(HlsSpec {
                    preference: StoredPreference::try_from(&Preference::default()).unwrap(),
                    output: OutputSpec::Mp4("/m/talk.mp4".into()),
                    live,
                }),
            },
            status,
            work_dir: Some("/m/talk.hsdl".into()),
            created_at: 0,
            total,
            result: None,
            write_only: false,
            seeding: false,
            finished_at: None,
            active: std::time::Duration::ZERO,
        };
        Row::new(task, Instant::now())
    }

    pub fn saved(segments: usize, live: bool) -> Saved {
        Saved::Hls(HlsSaved {
            live,
            segments,
            bytes: 4_000_000,
            duration: Duration::from_secs(40),
        })
    }
}

#[cfg(test)]
mod tests {
    use hs_dlx_app::Status;
    use hs_m3u8_core::{Progress, Stage};

    use super::sample::row;

    /// 按直播新建、实际走了点播的：进度到了下载阶段就按点播显示（可暂停），不再当作录制。
    #[test]
    fn a_live_task_that_turned_out_to_be_vod_is_not_recording() {
        let mut task = row(Status::Running, true, None);
        assert!(task.recording());
        task.progress = Some(Progress {
            stage: Stage::Downloading,
            ..Progress::default()
        });
        assert!(!task.recording());
    }
}
