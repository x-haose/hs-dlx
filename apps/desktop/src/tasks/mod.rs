//! 与窗口无关的任务状态：任务服务、列表里的各行与它们这次运行收到的进度、要用户看的提示、退出进行到哪一步、保存目录
//! 所在盘的可用空间、后打开的实例转交来的链接。对任务的操作在这里发起并收尾（读任务目录、放弃、新建的结果都在这里
//! 落进各行），窗口只发起与显示，窗口不在时照常进行。

mod operations;
mod pikpak;
mod quit;
mod row;

use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{Context, EventEmitter, Subscription, Task};
use hs_dlx_app::ytdlp::Browser;
use hs_dlx_app::{Event, Events, Finished, SeedEnd, Service, Status, TaskId};
use hs_dlx_instance::{Handoff, Handoffs, Received};
use hs_m3u8_core::Stage;

pub use self::operations::resume_label;
pub use self::pikpak::PikPak;
pub use self::quit::{Asked, Departure, Quit};
#[cfg(test)]
pub use self::row::sample;
pub use self::row::{Amount, Disk, FileAmount, Output, Row, finished_text, main_output, saved_as};
use crate::signals::Level;
use crate::wait::within;

/// 多久读一次保存目录所在盘的可用空间。
const FREE_SPACE_INTERVAL: Duration = Duration::from_secs(10);

/// 读一次可用空间最多等多久：网络盘掉线时读会卡住。
const FREE_SPACE_TIMEOUT: Duration = Duration::from_secs(3);

/// 保存目录 `folder` 所在盘对本用户可用的字节数；目录不在（被删、所在的盘没接上）或读不出时为原因（顶栏照原样显示）。
fn free_space(folder: &std::path::Path) -> Result<u64, String> {
    match folder.try_exists() {
        Ok(true) => {
            fs4::available_space(folder).map_err(|error| format!("读不出可用空间：{error}"))
        }
        Ok(false) => Err("保存目录不在".to_owned()),
        Err(error) => Err(format!("读不出可用空间：{error}")),
    }
}

/// 要窗口处理的事。
pub enum TasksEvent {
    /// 后打开的实例转交来的（已接手）：窗口到前台，有链接就探测
    HandedOver(Handoff),
}

/// 任务此刻合起来的样子。
pub struct Activity {
    /// 在运行的任务数
    pub running: usize,
    /// 合计下载速度（字节每秒）
    pub speed: f64,
    /// 在运行的点播的平均进度（0 到 1，见 [`Row::progress_share`]）；没有时为 None
    pub progress: Option<f32>,
    /// 有在录的直播
    pub recording: bool,
    /// 失败了、停着等处理的任务数
    pub failed: usize,
}

/// 一条要用户看到、关掉才消失的提示（操作没成、没删干净的东西）。
pub struct Notice {
    pub id: u64,
    pub text: String,
}

pub struct Tasks {
    service: Rc<Service>,
    /// 按新建先后
    rows: Vec<Row>,
    /// 新建任务默认保存到的目录
    folder: PathBuf,
    notices: Vec<Notice>,
    next_notice: u64,
    quit: Quit,
    /// 撤回过几次退出（见 [`Tasks::stays`]）
    stays: u64,
    /// 在服务里进行中的新建：退出要等它们（建成的随即暂停）
    creating: usize,
    /// 新建期间收到的、列表里还没有的任务的结束与改名事件：新建的结果与任务的事件走两条路，它们可能先到；加进那一行
    /// 时按收到的先后再处理
    early: Vec<Event>,
    /// 保存目录所在盘的可用空间（字节）：还没读到时为 None，读不出、目录不在或读超时时为原因
    free_space: Option<Result<u64, String>>,
    pikpak: PikPak,
    /// 设置里选的、本进程没有权限读它的数据的浏览器（见 [`Browser::access`]）；没选或读得了时为 None。打开应用、窗口
    /// 回到前台与改了选的浏览器时重查（见 [`Tasks::check_cookie_access`]）
    cookies_denied: Option<Browser>,
    /// 收得了后打开的实例转交来的（再次打开应用会回到窗口）；macOS 上总能点程序坞回到窗口，用不到
    #[cfg(not(target_os = "macos"))]
    receives_handoffs: bool,
    _events: Task<()>,
    _handoffs: Task<()>,
    _ticker: Task<()>,
    _free_space: Task<()>,
    _quitting: Subscription,
}

impl EventEmitter<TasksEvent> for Tasks {}

impl Tasks {
    /// `handoffs` 为后打开的实例转交来的（见 hs-dlx-instance），收不了转交时为原因（提示一次，之后打开的实例各自
    /// 报错）。
    pub fn new(
        service: Service,
        mut events: Events,
        folder: PathBuf,
        handoffs: Result<Handoffs, String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let service = Rc::new(service);
        let now = Instant::now();
        let mut problems = Vec::new();
        let rows: Vec<Row> = match service.tasks() {
            Ok(tasks) => tasks.into_iter().map(|task| Row::new(task, now)).collect(),
            Err(error) => {
                problems.push(format!("读不出任务列表：{error}"));
                Vec::new()
            }
        };
        // 应用真要退出了（问过之后，或不经询问的退出，如别的平台注销）：停下所有任务、补写状态（最多等 2 秒）
        let quitting = cx.on_app_quit(|this: &mut Self, _| {
            // 没有窗口能显示了；补写不进的下次打开时按暂停处理（见 Service::close）
            let _unsaved = this.service.close();
            async {}
        });
        #[cfg(not(target_os = "macos"))]
        let receives_handoffs = handoffs.is_ok();
        let handing = match handoffs {
            Ok(mut handoffs) => cx.spawn(async move |this, cx| {
                while let Some(received) = handoffs.recv().await {
                    if this
                        .update(cx, |this, cx| this.receive(received, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            }),
            Err(reason) => {
                problems.push(format!(
                    "之后再打开应用（如在浏览器里点链接）时，链接交不到这个窗口：{reason}"
                ));
                Task::ready(())
            }
        };
        let receiving = cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this.update(cx, |this, cx| this.apply(event, cx)).is_err() {
                    break;
                }
            }
        });
        // 速度按时间窗算，没有新进度时也要随时间降下来：有任务在运行时每秒通知一次
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    if this.rows.iter().any(|r| r.task.status == Status::Running) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let mut tasks = Tasks {
            service,
            rows,
            folder,
            notices: Vec::new(),
            next_notice: 0,
            quit: Quit::Staying,
            stays: 0,
            creating: 0,
            early: Vec::new(),
            free_space: None,
            pikpak: PikPak::default(),
            cookies_denied: None,
            #[cfg(not(target_os = "macos"))]
            receives_handoffs,
            _events: receiving,
            _handoffs: handing,
            _ticker: ticker,
            _free_space: Task::ready(()),
            _quitting: quitting,
        };
        for problem in problems {
            tasks.add_notice(problem);
        }
        tasks.reload_pikpak();
        tasks.check_cookie_access(cx);
        let ids: Vec<TaskId> = tasks.rows.iter().map(|r| r.task.id).collect();
        for id in ids {
            tasks.after_stop(id, cx);
        }
        tasks.watch_free_space(cx);
        tasks
    }

    /// 再次打开应用时转交来的：已决定退出时不接手（交来的一方告诉用户正在退出）；否则接手，等着退出的、正在问要不要
    /// 退出的不再退出，交给窗口（[`TasksEvent::HandedOver`]）。一次没收成的转交提示原因。
    fn receive(&mut self, received: Received, cx: &mut Context<Self>) {
        match received {
            Ok(delivery) if self.quit == Quit::Leaving => delivery.refuse_leaving(),
            Ok(delivery) => {
                // 交出的一方已不等（2 秒没接手）时它已把链接告诉用户，这里不再处理
                let Some(handoff) = delivery.accept() else {
                    return;
                };
                if let Quit::Asking { reopened, .. } = &mut self.quit {
                    *reopened = true;
                }
                if self.quitting() {
                    self.cancel_quit("再次打开了应用，没有退出".to_owned());
                }
                cx.emit(TasksEvent::HandedOver(handoff));
            }
            Err(reason) => self.add_notice(format!("再次打开应用时带来的链接没收到：{reason}")),
        }
        cx.notify();
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        if let Event::Ended { id, .. } | Event::Renamed { id } = &event
            && self.creating > 0
            && self.row(*id).is_none()
        {
            self.early.push(event);
            return;
        }
        match event {
            Event::Progress { id, progress } => {
                if let Some(row) = self.row_mut(id) {
                    row.rate
                        .update(Instant::now(), progress.received, progress.segments.done);
                    for (level, text) in crate::signals::between(row.progress.as_ref(), &progress) {
                        row.signal(level, text);
                    }
                    if progress.stage == Stage::Recording
                        && let Some(session) =
                            row.timeline.observe(row.progress.as_ref(), &progress)
                    {
                        row.signal(Level::Warn, format!("来源接不上，另起会话 {}", session + 1));
                    }
                    row.progress = Some(*progress);
                }
            }
            Event::FileProgress { id, progress } => {
                if let Some(row) = self.row_mut(id) {
                    // 文件没有片数：剩余时间另按字节算
                    row.rate.update(Instant::now(), progress.received, 0);
                    for (level, text) in
                        crate::signals::between_file(row.file_progress.as_ref(), &progress)
                    {
                        row.signal(level, text);
                    }
                    row.file_progress = Some(*progress);
                }
            }
            Event::TorrentProgress { id, progress } => {
                if let Some(row) = self.row_mut(id) {
                    // 没有片数：剩余时间另按字节算
                    row.rate.update(Instant::now(), progress.received, 0);
                    for (level, text) in
                        crate::signals::between_torrent(row.torrent_progress.as_ref(), &progress)
                    {
                        row.signal(level, text);
                    }
                    row.torrent_progress = Some(*progress);
                }
            }
            Event::CloudProgress { id, progress } => {
                if let Some(row) = self.row_mut(id) {
                    if let hs_dlx_app::CloudProgress::Fetching { received, .. } = progress.as_ref()
                    {
                        // 没有片数：剩余时间另按字节算
                        row.rate.update(Instant::now(), *received, 0);
                    }
                    for (level, text) in
                        crate::signals::between_cloud(row.cloud_progress.as_ref(), &progress)
                    {
                        row.signal(level, text);
                    }
                    row.cloud_progress = Some(*progress);
                }
            }
            Event::WebProgress { id, progress } => {
                if let Some(row) = self.row_mut(id) {
                    if let hs_dlx_app::WebProgress::Fetching { received, .. } = progress.as_ref() {
                        row.rate.update(Instant::now(), *received, 0);
                    }
                    for (level, text) in
                        crate::signals::between_web(row.web_progress.as_ref(), &progress)
                    {
                        row.signal(level, text);
                    }
                    row.web_progress = Some(*progress);
                }
            }
            Event::Renamed { id } => match self.service.task(id) {
                Ok(task) => {
                    if let Some(row) = self.row_mut(id) {
                        let was = row::file_name(&row.task.spec);
                        row.task.work_dir = task.work_dir;
                        row.set_spec(task.spec);
                        let name = row::file_name(&row.task.spec);
                        if name != was {
                            row.signal(Level::Info, format!("保存为 {name}"));
                        }
                    }
                }
                Err(error) => {
                    let title = self.title(id);
                    self.add_notice(format!("「{title}」的保存位置定了，但读不出来：{error}"));
                }
            },
            Event::SeedingStarted { id } => {
                if let Some(row) = self.row_mut(id) {
                    row.task.seeding = true;
                    // 下载时的进度不再适用
                    row.torrent_progress = None;
                    row.signal(Level::Info, "开始做种".to_owned());
                }
            }
            Event::SeedingNotSaved { id, reason } => {
                if let Some(row) = self.row_mut(id) {
                    row.signal(Level::Warn, format!("做种的进度这次没存成：{reason}"));
                }
            }
            Event::SeedingEnded { id, end, unsaved } => {
                let title = self.title(id);
                if let Some(row) = self.row_mut(id) {
                    row.task.seeding = false;
                    let uploaded = row.torrent_progress.as_ref().map(|progress| {
                        format!(
                            "已上传 {} · 分享率 {}",
                            crate::format::bytes(progress.uploaded),
                            crate::format::ratio(progress.uploaded, progress.wanted)
                        )
                    });
                    let (level, text) = match end {
                        SeedEnd::Reached => (Level::Info, "做种到了设置里的条件，停下".to_owned()),
                        SeedEnd::Stopped => (Level::Info, "停止做种".to_owned()),
                        SeedEnd::Incomplete => (
                            Level::Warn,
                            "磁盘上的文件不全（挪走、删了或大小变了），停止做种，不补下".to_owned(),
                        ),
                        SeedEnd::Failed(message) => (Level::Error, format!("做种出错：{message}")),
                    };
                    let text = match uploaded {
                        Some(uploaded) => format!("{text} · {uploaded}"),
                        None => text,
                    };
                    row.signal(level, text);
                }
                if let Some(error) = unsaved {
                    self.add_notice(format!(
                        "「{title}」的做种状态没能存下（{error}），下次打开可能接着做种"
                    ));
                }
            }
            Event::Ended {
                id,
                status,
                result,
                total,
                leftovers,
                unsaved,
            } => {
                let title = self.title(id);
                let mut problems: Vec<String> = leftovers
                    .iter()
                    .map(|leftover| format!("「{title}」完成时没删掉：{leftover}"))
                    .collect();
                if let Some(error) = unsaved {
                    problems.push(format!(
                        "「{title}」已停下，但状态没能写进存储（{error}），之后每次写存储时会再写"
                    ));
                }
                self.ended(id, status, result, total, problems, cx);
            }
            Event::TotalUnsaved { id, error } => {
                let title = self.title(id);
                self.add_notice(format!(
                    "没能记下「{title}」的总片数：{error}；任务照常下载，停下时再记"
                ));
            }
            Event::Orphaned { paths, reason } => {
                let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                self.add_notice(format!(
                    "新建任务时存储写不进，任务已取消，但留下了不在任何任务名下的 {}：{reason}",
                    paths.join("、")
                ));
                if self.quitting() {
                    self.cancel_quit("有要处理的东西，没有退出".to_owned());
                }
            }
        }
        cx.notify();
    }

    /// 任务停下：改状态、总片数与结果，写信号，作废之前读到的任务目录内容后重读（或查输出）；`problems` 为要用户
    /// 看的提示（没删掉的东西、状态没写进存储）。退出中遇到失败或要提示的就不退出，留给用户看。
    fn ended(
        &mut self,
        id: TaskId,
        status: Status,
        result: Option<Finished>,
        total: Option<usize>,
        problems: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let title = self.title(id);
        let failed = matches!(status, Status::Failed(_));
        if let Some(row) = self.row_mut(id) {
            // 放弃时任务先停下，这时的「暂停」不是用户要的，不写
            if !row.discarding {
                let (level, text) = ended_signal(&status, &row.task.spec, result.as_ref());
                row.signal(level, text);
            }
            row.task.status = status;
            if result.is_some() {
                row.task.result = result;
            }
            if total.is_some() {
                row.task.total = total;
            }
            row.forget_disk();
        }
        if self.quitting() && (failed || !problems.is_empty()) {
            self.cancel_quit(format!("「{title}」停下时有要处理的，没有退出"));
        }
        for text in problems {
            self.add_notice(text);
        }
        self.after_stop(id, cx);
        self.quit_if_idle(cx);
    }

    /// （重新）开始读保存目录所在盘的可用空间：在后台读，每 10 秒一次。一次读超过 3 秒就先报超时，等它读完再接着
    /// 按间隔读，不叠着发起新的；换了保存目录时调一次，立即按新目录读。
    fn watch_free_space(&mut self, cx: &mut Context<Self>) {
        let folder = self.folder.clone();
        self._free_space = cx.spawn(async move |this, cx| {
            loop {
                let target = folder.clone();
                let mut read = std::pin::pin!(
                    cx.background_executor()
                        .spawn(async move { free_space(&target) })
                );
                let early = within(
                    read.as_mut(),
                    cx.background_executor().timer(FREE_SPACE_TIMEOUT),
                )
                .await;
                let value = match early {
                    Some(value) => value,
                    None => {
                        let late = Err(format!(
                            "可用空间读了 {} 秒没有结果",
                            FREE_SPACE_TIMEOUT.as_secs()
                        ));
                        let alive = this.update(cx, |this, cx| {
                            this.free_space = Some(late);
                            cx.notify();
                        });
                        if alive.is_err() {
                            break;
                        }
                        read.await
                    }
                };
                let alive = this.update(cx, |this, cx| {
                    this.free_space = Some(value);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(FREE_SPACE_INTERVAL).await;
            }
        });
    }

    /// 任务此刻合起来的样子；速度按 `now` 时的时间窗算。
    pub fn activity(&self, now: Instant) -> Activity {
        let running: Vec<&Row> = self
            .rows
            .iter()
            .filter(|row| row.task.status == Status::Running)
            .collect();
        let shares: Vec<f32> = running
            .iter()
            .filter_map(|row| row.progress_share())
            .collect();
        Activity {
            running: running.len(),
            speed: running
                .iter()
                .map(|row| row.rate.bytes_per_second(now))
                .sum(),
            progress: (!shares.is_empty())
                .then(|| shares.iter().sum::<f32>() / shares.len() as f32),
            recording: running.iter().any(|row| row.recording()),
            failed: self
                .rows
                .iter()
                .filter(|row| matches!(row.task.status, Status::Failed(_)))
                .count(),
        }
    }

    /// 收得了后打开的实例转交来的：再次打开应用会回到窗口。
    #[cfg(not(target_os = "macos"))]
    pub fn receives_handoffs(&self) -> bool {
        self.receives_handoffs
    }

    pub fn service(&self) -> &Rc<Service> {
        &self.service
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row(&self, id: TaskId) -> Option<&Row> {
        self.rows.iter().find(|row| row.task.id == id)
    }

    fn row_mut(&mut self, id: TaskId) -> Option<&mut Row> {
        self.rows.iter_mut().find(|row| row.task.id == id)
    }

    /// 列表里的名字；行已不在（放弃做完）时写编号。
    pub fn title(&self, id: TaskId) -> String {
        self.row(id)
            .map_or_else(|| id.to_string(), |r| r.task.spec.title.clone())
    }

    pub fn folder(&self) -> &PathBuf {
        &self.folder
    }

    /// 换新建任务默认保存到的目录：存进设置，存进了才换、按新目录读可用空间。存不进时为原因。
    pub fn set_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) -> Result<(), String> {
        self.service
            .set_folder(&folder)
            .map_err(|error| error.to_string())?;
        self.folder = folder;
        self.watch_free_space(cx);
        cx.notify();
        Ok(())
    }

    pub fn free_space(&self) -> Option<&Result<u64, String>> {
        self.free_space.as_ref()
    }

    /// 见 [`Tasks::cookies_denied`] 字段。
    pub fn cookies_denied(&self) -> Option<Browser> {
        self.cookies_denied
    }

    /// 重查设置里选的浏览器的数据读不读得了，变了时通知窗口重画；读不出设置时提示原因。
    pub fn check_cookie_access(&mut self, cx: &mut Context<Self>) {
        let browser = match self.service.settings() {
            Ok(settings) => settings.web_browser,
            Err(error) => {
                self.tell(
                    format!("读不出设置，查不了能不能读浏览器的登录信息：{error}"),
                    cx,
                );
                return;
            }
        };
        let denied = browser.filter(|browser| browser.access().is_err());
        if denied != self.cookies_denied {
            self.cookies_denied = denied;
            cx.notify();
        }
    }

    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }

    /// 加一条提示并通知窗口重画。
    pub fn tell(&mut self, text: String, cx: &mut Context<Self>) {
        self.add_notice(text);
        cx.notify();
    }

    /// 加一条提示；调用方随后通知窗口重画。
    fn add_notice(&mut self, text: String) {
        self.notices.push(Notice {
            id: self.next_notice,
            text,
        });
        self.next_notice += 1;
    }

    pub fn dismiss_notice(&mut self, id: u64, cx: &mut Context<Self>) {
        self.notices.retain(|n| n.id != id);
        cx.notify();
    }
}

/// 任务停下时的信号。
fn ended_signal(
    status: &Status,
    spec: &hs_dlx_app::TaskSpec,
    result: Option<&Finished>,
) -> (Level, String) {
    match status {
        Status::Done => (
            Level::Info,
            match (result, spec.hls()) {
                (None, _) => "完成".to_owned(),
                (Some(r), Some(_)) => format!("写出 {}", finished_text(spec, r)),
                (Some(r), None) => format!("下载完成 · {}", crate::format::bytes(r.bytes)),
            },
        ),
        Status::Paused if spec.torrent().is_some() || spec.cloud().is_some() => {
            (Level::Info, "已暂停，已下的保留".to_owned())
        }
        Status::Paused => (Level::Info, "已暂停，任务目录保留".to_owned()),
        Status::Failed(failure) => (Level::Error, failure.message.clone()),
        Status::Running | Status::DiscardUnfinished => {
            unreachable!("任务服务只在任务停下后发结束事件，停下的状态不会是运行中或放弃没做完")
        }
    }
}
