//! 对任务的操作：新建、暂停与停止、读任务目录、继续、改请求头、放弃、从列表移除，与任务停下后的查看。要等的操作在
//! 这里收尾，窗口关掉也照常落进各行。

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui_kit::Context;
use hs_dlx_app::{Draft, Event, Kind, Status, TaskId};
use hs_m3u8_core::Saved;
use tokio::sync::oneshot;

use super::Tasks;
use super::row::{Disk, Present, Row};
use crate::format;
use crate::rate::Rate;
use crate::signals::Level;

/// 新建的结果：建成的任务，或失败的原因。
pub type Created = Result<TaskId, String>;

/// 继续一个停着的任务的叫法：选了只写出的改回续录叫继续录制，失败的叫重试，暂停的叫继续。
pub fn resume_label(failed: bool, write_only: bool) -> &'static str {
    match (write_only, failed) {
        (true, _) => "继续录制",
        (false, true) => "重试",
        (false, false) => "继续",
    }
}

impl Tasks {
    /// 按 `draft` 新建任务：退出中不新建，系统时钟早于 1970 年时记不了创建时间，这两种立即为原因。新建要查磁盘，在
    /// 服务的运行时里做；做完时建成的加进列表（选了退出的随即暂停），结果送进返回的接收端，由发起的窗口显示。
    pub fn create(
        &mut self,
        draft: Draft,
        cx: &mut Context<Self>,
    ) -> Result<oneshot::Receiver<Created>, String> {
        if self.quitting() {
            return Err("正在退出，不开始新任务".to_owned());
        }
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok())
            .ok_or_else(|| "系统时钟早于 1970 年，记不了任务的创建时间".to_owned())?;
        let (reply, created) = oneshot::channel();
        self.creating += 1;
        let handle = self.service.create(draft, created_at);
        cx.spawn(async move |this, cx| {
            let result = handle.await;
            // 应用已在退出、Tasks 已释放时，任务照常在服务里开始，下次打开时在列表里
            let _gone = this.update(cx, |this, cx| {
                this.creating -= 1;
                let result = match result {
                    Ok(Ok(task)) => Ok(this.started(task, cx)),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(join) => Err(format!("新建中途出错：{join}")),
                };
                if result.is_err() && this.quitting() {
                    this.cancel_quit("有新建没成，没有退出".to_owned());
                }
                // 没有在途的新建了，留着的事件都不属于将要加进来的行（是已移除的行的）
                if this.creating == 0 {
                    this.early.clear();
                }
                // 接收端在发起的窗口里等，只在应用退出时随之释放
                let _unread = reply.send(result);
                this.quit_if_idle(cx);
                cx.notify();
            });
        })
        .detach();
        Ok(created)
    }

    /// 新建的任务建成了：加进列表，按先后处理新建期间先到的结束与改名事件；选了退出的随即暂停。
    fn started(&mut self, task: hs_dlx_app::Task, cx: &mut Context<Self>) -> TaskId {
        let id = task.id;
        let mut row = Row::new(task, Instant::now());
        row.timeline.begin(Duration::ZERO);
        self.rows.push(row);
        let (early, rest) = std::mem::take(&mut self.early)
            .into_iter()
            .partition(|event| {
                matches!(event, Event::Ended { id: at, .. } | Event::Renamed { id: at } if *at == id)
            });
        self.early = rest;
        for event in early {
            self.apply(event, cx);
        }
        if self.quitting() {
            self.service.pause(id);
        }
        id
    }

    pub fn pause(&self, id: TaskId) {
        self.service.pause(id);
    }

    /// 停止直播录制并写出已录的。
    pub fn stop(&self, id: TaskId) {
        self.service.stop(id);
    }

    /// 任务不在运行时：暂停与失败的读一次任务目录（BT 为存着的续传数据），完成的查一次输出在不在；放弃中的什么也不做
    /// （目录正在删）。
    pub(super) fn after_stop(&mut self, id: TaskId, cx: &mut Context<Self>) {
        let Some(row) = self.row(id) else {
            return;
        };
        if row.discarding {
            return;
        }
        match &row.task.status {
            Status::Paused | Status::Failed(_) => self.inspect(id, false, cx),
            Status::Done => {
                // HLS 为 MP4 与本地 HLS 目录，文件任务为文件，BT 为内容的根
                let (mp4, other) = match &row.task.spec.kind {
                    Kind::Hls(hls) => {
                        let target = hls.output.options().target;
                        (
                            target.mp4().map(PathBuf::from),
                            target.hls().map(PathBuf::from),
                        )
                    }
                    Kind::File(file) => (None, Some(file.path.clone())),
                    Kind::Torrent(torrent) => (None, Some(torrent.root.clone())),
                    Kind::Cloud(cloud) => (None, cloud.root.clone()),
                    Kind::Web(web) => match web.container {
                        hs_dlx_app::web::Container::Mp4 => (Some(web.path.clone()), None),
                        hs_dlx_app::web::Container::Matroska
                        | hs_dlx_app::web::Container::Original => (None, Some(web.path.clone())),
                    },
                };
                let checking = cx.background_executor().spawn(async move {
                    let mp4 = mp4.filter(|p| p.is_file());
                    let shown = mp4.clone().or_else(|| other.filter(|p| p.exists()));
                    Present { mp4, shown }
                });
                cx.spawn(async move |this, cx| {
                    let present = checking.await;
                    // 查的期间应用退出时不再需要结果
                    let _gone = this.update(cx, |this, cx| {
                        if let Some(row) = this.row_mut(id) {
                            row.present = Some(present);
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            // 放弃没做完的任务目录可能已删了一部分，读出来的不说明什么
            Status::Running | Status::DiscardUnfinished => {}
        }
    }

    /// 读任务目录里已完成的内容（BT 任务读存着的续传数据）；`then_resume` 时读完即继续，读到的片数作本次运行的起点
    /// （读不出时不估剩余时间）。读完（及继续）后，选了退出、都已停下的即退出。
    fn inspect(&mut self, id: TaskId, then_resume: bool, cx: &mut Context<Self>) {
        let Some((reading, torrent, cloud, web)) = self.row(id).map(|row| {
            (
                row.reading,
                row.task.spec.torrent().is_some(),
                row.task.spec.cloud().is_some(),
                row.task.spec.web().is_some(),
            )
        }) else {
            return;
        };
        let read = if web {
            let handle = self.service.inspect_web(id);
            cx.background_executor()
                .spawn(async move { handle.await.map(|result| result.map(Disk::Web)) })
        } else if cloud {
            // 只读存储，不碰磁盘
            let files = self.service.cloud_files(id).map(Disk::Cloud);
            cx.background_executor().spawn(async move { Ok(files) })
        } else if torrent {
            let handle = self.service.inspect_torrent(id);
            cx.background_executor()
                .spawn(async move { handle.await.map(|result| result.map(Disk::Torrent)) })
        } else {
            let handle = self.service.inspect(id);
            cx.background_executor()
                .spawn(async move { handle.await.map(|result| result.map(Disk::Read)) })
        };
        cx.spawn(async move |this, cx| {
            let result = read.await;
            // 等待期间应用退出时不再需要结果
            let _gone = this.update(cx, |this, cx| {
                let disk = match result {
                    Ok(Ok(disk)) => disk,
                    Ok(Err(error)) => Disk::Unreadable(error.to_string()),
                    Err(join) => Disk::Unreadable(format!("读的过程中出错：{join}")),
                };
                // 读的期间任务又运行过、停下了的，这次读到的已过时，停下时另读了
                if let Some(row) = this.row_mut(id)
                    && row.reading == reading
                {
                    row.disk = disk;
                }
                if then_resume {
                    this.resume_now(id);
                }
                this.quit_if_idle(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 继续：先读出任务目录里已有的，作本次运行的起点，再开始；读的期间这一行不能再操作。退出中不继续。发起了（开始
    /// 读目录）时为 true；读完后开始不了的原因写进提示。
    pub fn resume(&mut self, id: TaskId, cx: &mut Context<Self>) -> bool {
        if self.quitting() {
            self.tell("正在退出，不再继续任务".to_owned(), cx);
            return false;
        }
        let Some(row) = self.row_mut(id) else {
            return false;
        };
        if !row.idle() {
            return false;
        }
        row.resuming = true;
        self.inspect(id, true, cx);
        cx.notify();
        true
    }

    /// 不再续录，只把直播录制已录的写成输出；退出中不开始。开始写出了时为 true，没开始的原因写进提示。
    pub fn write_recorded(&mut self, id: TaskId, cx: &mut Context<Self>) -> bool {
        let title = self.title(id);
        if self.quitting() {
            self.tell(format!("正在退出，「{title}」没有写出"), cx);
            return false;
        }
        if !self.row(id).is_some_and(Row::idle) {
            return false;
        }
        let started = match self.service.write_recorded(id) {
            Ok(task) => {
                let row = self.row_mut(id).expect("上面刚找到这一行");
                // 核心报出进度之前，已录时长先按停下时知道的显示
                row.recorded_before_writing = row.recorded();
                row.task = task;
                row.progress = None;
                row.forget_disk();
                // 写出不联网，没有下载速度与剩余时间可估
                row.rate = Rate::new(Instant::now(), None);
                row.signal(Level::Info, "不再续录，写出已录的".to_owned());
                true
            }
            Err(error) => {
                self.add_notice(format!("写出「{title}」已录的失败：{error}"));
                false
            }
        };
        cx.notify();
        started
    }

    /// 网页视频合并时内容放不进 MP4，改用 MKV 再跑；退出中不改。改了并发起了再跑时为 true，没成的原因写进提示。
    pub fn switch_to_matroska(&mut self, id: TaskId, cx: &mut Context<Self>) -> bool {
        let title = self.title(id);
        if self.quitting() {
            self.tell(format!("正在退出，「{title}」没有改用 MKV"), cx);
            return false;
        }
        if !self.row(id).is_some_and(Row::idle) {
            return false;
        }
        match self.service.switch_web_to_matroska(id) {
            Ok(task) => {
                let row = self.row_mut(id).expect("上面刚找到这一行");
                row.set_spec(task.spec);
            }
            Err(error) => {
                self.tell(format!("「{title}」没能改用 MKV：{error}"), cx);
                return false;
            }
        }
        self.resume(id, cx)
    }

    /// 内容放不进 MP4 时改存本地 HLS 再跑：直播只写出已录的，点播接着下；退出中不改。改了并发起了再跑时为 true，没成的
    /// 原因写进提示。
    pub fn switch_to_hls(&mut self, id: TaskId, cx: &mut Context<Self>) -> bool {
        let title = self.title(id);
        if self.quitting() {
            self.tell(format!("正在退出，「{title}」没有改存本地 HLS"), cx);
            return false;
        }
        let Some(live) = self.row(id).filter(|row| row.idle()).map(Row::recording) else {
            return false;
        };
        match self.service.switch_to_hls(id) {
            Ok(task) => {
                let row = self.row_mut(id).expect("上面刚找到这一行");
                row.set_spec(task.spec);
            }
            Err(error) => {
                self.tell(format!("「{title}」没能改存本地 HLS：{error}"), cx);
                return false;
            }
        }
        if live {
            self.write_recorded(id, cx)
        } else {
            self.resume(id, cx)
        }
    }

    fn resume_now(&mut self, id: TaskId) {
        let title = self.title(id);
        let quitting = self.quitting();
        let Some(row) = self.row_mut(id) else {
            return;
        };
        row.resuming = false;
        if row.discarding {
            return;
        }
        if quitting {
            self.add_notice(format!("正在退出，「{title}」没有继续"));
            return;
        }
        // 续传的起点：HLS 为已完成的片数与已录时长；文件的剩余时间按字节算，不用片数
        let (baseline, start, already) = match &row.disk {
            Disk::Read(Some(Saved::Hls(saved))) => (
                Some(saved.segments),
                Some(saved.duration),
                (saved.segments > 0).then(|| format!("{} 片", saved.segments)),
            ),
            Disk::Read(Some(Saved::File(saved))) => (
                Some(0),
                None,
                (saved.bytes > 0).then(|| format::bytes(saved.bytes)),
            ),
            Disk::Read(None) => (Some(0), Some(Duration::ZERO), None),
            // BT 的说法另写，见下面
            Disk::Torrent(_) => (Some(0), None, None),
            Disk::Cloud(files) => {
                let done: u64 = files.iter().filter(|f| f.done).map(|f| f.size).sum();
                (Some(0), None, (done > 0).then(|| format::bytes(done)))
            }
            Disk::Web(bytes) => (Some(0), None, (*bytes > 0).then(|| format::bytes(*bytes))),
            Disk::Unread | Disk::Unreadable(_) => (None, None, None),
        };
        let label = resume_label(
            matches!(row.task.status, Status::Failed(_)),
            row.task.write_only,
        );
        match self.service.resume(id) {
            Ok(task) => {
                let row = self.row_mut(id).expect("上面刚找到这一行");
                row.task = task;
                row.progress = None;
                row.file_progress = None;
                row.torrent_progress = None;
                row.cloud_progress = None;
                row.web_progress = None;
                row.rate = Rate::new(Instant::now(), baseline);
                if let Some(start) = start {
                    row.timeline.begin(start);
                }
                let text = match (&row.disk, already) {
                    (Disk::Torrent(saved), _) if !saved.trusted => {
                        format!("{label}：先重新校验已下的文件")
                    }
                    (Disk::Torrent(saved), _) if saved.info.done > 0 => {
                        format!("{label}：已有 {}", format::bytes(saved.info.done))
                    }
                    (Disk::Cloud(_), Some(already)) => format!("{label}：已取回 {already}"),
                    (_, Some(already)) => format!("{label}：任务目录里已有 {already}"),
                    (_, None) => label.to_owned(),
                };
                row.signal(Level::Info, text);
            }
            Err(error) => self.add_notice(format!("继续「{title}」失败：{error}")),
        }
    }

    /// 放弃任务（删掉已下载的内容）；做完时移除这一行，没删干净的提示出来。
    pub fn discard(&mut self, id: TaskId, cx: &mut Context<Self>) {
        let title = self.title(id);
        if let Some(row) = self.row_mut(id) {
            row.discarding = true;
        }
        let handle = self.service.discard(id);
        cx.spawn(async move |this, cx| {
            let result = handle.await;
            // 等待期间应用退出时不再需要结果；放弃本身在服务里照常做完
            let _gone = this.update(cx, |this, cx| {
                let problems = match result {
                    Ok(Ok(leftovers)) => {
                        this.rows.retain(|row| row.task.id != id);
                        leftovers
                            .iter()
                            .map(|leftover| format!("放弃「{title}」时没删掉：{leftover}"))
                            .collect()
                    }
                    Ok(Err(error)) => vec![format!("放弃「{title}」失败：{error}")],
                    Err(join) => vec![format!("放弃「{title}」中途出错：{join}")],
                };
                // 没删干净的、超时的，服务里已记为放弃没做完：照服务里的状态改这一行（行还在时）
                if this.row(id).is_some() {
                    match this.service.task(id) {
                        Ok(task) => {
                            let row = this.row_mut(id).expect("上面刚找到这一行");
                            row.task.status = task.status;
                        }
                        Err(error) => this.add_notice(format!(
                            "读不出「{title}」放弃后的状态（{error}），界面上的状态可能不对"
                        )),
                    }
                }
                if let Some(row) = this.row_mut(id) {
                    row.discarding = false;
                }
                if !problems.is_empty() && this.quitting() {
                    this.cancel_quit("放弃没做干净，没有退出".to_owned());
                }
                for text in problems {
                    this.add_notice(text);
                }
                this.after_stop(id, cx);
                this.quit_if_idle(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 让下完的 BT 任务做种；开始不了时提示原因。
    pub fn seed(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if let Err(error) = self.service.seed(id) {
            let title = self.title(id);
            self.add_notice(format!("「{title}」没能开始做种：{error}"));
        }
        cx.notify();
    }

    /// 停止做种；记不进存储时提示原因（做种照旧）。
    pub fn stop_seeding(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if let Err(error) = self.service.stop_seeding(id) {
            let title = self.title(id);
            self.add_notice(format!("「{title}」没能停止做种：{error}"));
        }
        cx.notify();
    }

    /// 从列表移除已完成的任务（输出归用户）与没有自己的任务目录的任务：只删记录。
    pub fn forget(&mut self, id: TaskId, cx: &mut Context<Self>) {
        match self.service.forget(id) {
            Ok(()) => self.rows.retain(|row| row.task.id != id),
            Err(error) => {
                let title = self.title(id);
                self.add_notice(format!("移除「{title}」失败：{error}"));
            }
        }
        cx.notify();
    }

    /// 存下停着的任务 `id` 的请求头，`resume` 时接着续传续录。存不进时提示原因，返回 false。
    pub fn set_headers(
        &mut self,
        id: TaskId,
        headers: Vec<(String, String)>,
        resume: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let saved = match self.service.set_headers(id, headers.clone()) {
            Ok(()) => {
                if let Some(row) = self.row_mut(id) {
                    row.task.spec.headers = headers;
                }
                if resume {
                    // 开始不了时原因已提示
                    let _started = self.resume(id, cx);
                }
                true
            }
            Err(error) => {
                let title = self.title(id);
                self.add_notice(format!("改「{title}」的请求头失败：{error}"));
                false
            }
        };
        cx.notify();
        saved
    }
}
