//! 新建任务：带着请求头认出地址是什么（或用嗅探页已探测好的结果）。是播放列表时，舞台上每档清晰度画成一个同心环
//! （越粗码率越高，引线连到右侧的标签），点环或用方向键选中，再在检查面板里选音轨、输出、保存位置与请求头；是文件时
//! 舞台上是写着大小的表盘，检查面板里选保存位置与请求头。磁力链接先连节点取种子信息（种子文件直接读），之后舞台上是
//! 写着所选大小的表盘，检查面板里勾要下的文件、选保存位置。探测被拒时可改请求头重新探测；取到的是网页或不是播放列表时
//! 外面可以在网页引擎里打开它看是不是网页（见 [`NewTask::opening_page`]）；请求经哪个代理写在检查面板里。舞台底部是
//! 外面给的粘贴栏。

mod figure;
mod inspector;
mod web;

use self::figure::{FIGURE_HEIGHT, FIGURE_WIDTH};

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{
    AppContext as _, Bounds, Context, Entity, EventEmitter, FocusHandle, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, PathPromptOptions, Pixels, Render, Styled as _,
    Task, Window, div, px,
};
use hs_dlx_app::{
    Draft, DraftKind, Expected, Failure, FailureKind, MetadataFetch, MetadataProgress, OutputKind,
    Service, TorrentInfo, file_name, not_a_playlist, task_title, torrent_from_file,
};
use hs_m3u8_core::hls::{AudioChoice, Preference, VariantChoice};
use hs_m3u8_core::{FileInfo, Probe, Resource, Source, Url};

use crate::dial;
use crate::dock::Dock;
use crate::format;
use crate::headers::HeadersEditor;
use crate::route::Route;
use crate::ui::{self, GAP, INSPECTOR_MIN_WIDTH};

/// 新建页的最小宽：整个同心环区域与收到最窄的检查面板（放得下一行「MP4 · 本地 HLS · 两者」三个选项）。
pub const MIN_WIDTH: f32 = FIGURE_WIDTH + GAP + INSPECTOR_MIN_WIDTH;
/// 新建页的最小高：放得下同心环区域、按键说明与粘贴栏的舞台。
pub const MIN_HEIGHT: f32 = ui::stage_height(FIGURE_HEIGHT, ui::CAPTION_LINE);

/// 要探测的链接。
#[derive(Clone)]
pub struct Link {
    pub url: Url,
    /// 视频所在网页的标题，任务照它取名（见 [`hs_dlx_app::task_title`]）；没有时由地址得出
    pub title: Option<String>,
    /// 探测与下载时带的请求头
    pub headers: Vec<(String, String)>,
    /// `headers` 出自的 curl 命令里没带上的请求头名，列在请求头编辑框下面
    pub dropped: Vec<String>,
    /// 已带着这些请求头探测好的结果（嗅探页后台探测的）：新建页不再探测；None 时新建页自己探测
    pub probe: Option<Box<Probe>>,
}

/// 新建任务视图发给外面的事件。
pub enum NewTaskEvent {
    /// 按这些选择新建任务；结果经 [`NewTask::failed`] 告诉视图，成功时视图随之关掉
    Create(Box<Draft>),
    Dismissed,
    /// 换了保存位置：之后新建的任务默认也存到这里
    FolderChanged(PathBuf),
    /// 探测完成，可以开始：由外面决定要不要把键盘焦点给它（[`NewTask::focus`]）
    Ready,
    /// 第一次探测（不是改了请求头重新探测）取到的是网页或不是播放列表：由外面决定要不要看它是不是网页
    NotAPlaylist,
    /// 用户要在网页里找视频流（失败页上的按钮）
    Sniff(Url),
}

/// 探测进行到哪一步，粘贴栏与任务列表据此显示状态。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Probing,
    Ready,
    Failed,
}

impl Phase {
    /// 粘贴栏右侧与任务列表里新任务一项的说明。
    pub fn label(self) -> &'static str {
        match self {
            Phase::Probing => "探测中",
            Phase::Ready => "探测完成",
            Phase::Failed => "探测失败",
        }
    }
}

/// 第一次探测取到的不是播放列表后，外面在网页引擎里看它是不是网页（见 [`NewTask::opening_page`]）。
#[derive(Clone, PartialEq, Eq)]
enum PageCheck {
    /// 没在看：探测没失败、不是这种失败，或用户已改请求头重新探测
    Idle,
    /// 正在看：探测失败的说明先不显示
    Opening,
    /// 看过了、不是网页（或打不开）：写在失败的说明下面的话
    NotPage(String),
}

enum Probing {
    Running,
    Failed(Failure),
    Ready(Box<Probe>),
    /// 地址上是文件
    File(Box<FileInfo>),
    /// 磁力链接在取种子信息（种子文件在读）
    Metadata(MetadataProgress),
    /// 种子信息到手，在选要下的文件
    Torrent(Box<TorrentChoice>),
    /// 地址是网页，yt-dlp 在解析
    Resolving,
    /// 地址是网页，要解析得先装网页视频的组件：组件此刻的状态
    NeedsWeb(hs_dlx_app::WebState),
    /// 网页解析出了视频，在选画质
    Web(Box<web::WebChoice>),
}

/// 种子的来源。
enum TorrentSource {
    Magnet,
    /// 本机的种子文件
    File(PathBuf),
}

impl TorrentSource {
    /// `url` 是磁力链接或种子文件的 `file:` 地址时为它的来源（见 [`crate::links::torrent_url`]）。
    fn of(url: &Url) -> Option<TorrentSource> {
        crate::links::torrent_url(url.as_str())?;
        match url.scheme() {
            "magnet" => Some(TorrentSource::Magnet),
            _ => url.to_file_path().ok().map(TorrentSource::File),
        }
    }
}

/// 种子信息与选了哪些文件。
pub(super) struct TorrentChoice {
    /// 种子的续传数据，新建任务时交给服务
    resume: Vec<u8>,
    info: TorrentInfo,
    /// 与 `info.files` 一一对应：要下
    wanted: Vec<bool>,
}

impl TorrentChoice {
    fn new(resume: Vec<u8>, info: TorrentInfo) -> TorrentChoice {
        let wanted = vec![true; info.files.len()];
        TorrentChoice {
            resume,
            info,
            wanted,
        }
    }

    /// 选了的文件的序号（[`hs_dlx_app::TorrentFile::index`]）。
    fn indices(&self) -> Vec<i32> {
        self.info
            .files
            .iter()
            .zip(&self.wanted)
            .filter(|(_, wanted)| **wanted)
            .map(|(file, _)| file.index)
            .collect()
    }

    /// 选了几个文件与它们的字节数。
    fn selected(&self) -> (usize, u64) {
        self.info
            .files
            .iter()
            .zip(&self.wanted)
            .filter(|(_, wanted)| **wanted)
            .fold((0, 0), |(count, bytes), (file, _)| {
                (count + 1, bytes + file.size)
            })
    }
}

/// 磁力链接或种子文件在取到种子信息之前的标题：磁力链接的 `dn`（显示名），没有时为「磁力链接」；种子文件为去掉
/// `.torrent` 的文件名。
fn torrent_title(url: &Url) -> String {
    match TorrentSource::of(url) {
        Some(TorrentSource::File(path)) => path.file_stem().map_or_else(
            || "种子文件".to_owned(),
            |stem| stem.to_string_lossy().into_owned(),
        ),
        _ => url
            .query_pairs()
            .find(|(name, _)| name == "dn")
            .map(|(_, name)| name.into_owned())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "磁力链接".to_owned()),
    }
}

pub struct NewTask {
    service: Rc<Service>,
    url: Url,
    /// 任务的标题，开始时与已有的重名会加编号
    title: String,
    /// 交给服务的网页标题（见 [`Link::title`]）
    page_title: Option<String>,
    probing: Probing,
    /// 这次探测开始的时刻：探测中的表盘写探测了多久
    since: Instant,
    /// 取到的不是播放列表后，外面看它是不是网页看到哪一步
    page_check: PageCheck,
    /// 从嗅探页来的流：失败页上不再给「在网页里找视频流」
    from_sniff: bool,
    /// 选中的变体在 `MasterInfo::variants` 里的下标；来源是媒体播放列表时不用
    variant: usize,
    /// 选中的音频在 `MasterInfo::audio` 里的下标；None 为来源的默认
    audio: Option<usize>,
    /// 鼠标所在的环（变体下标）
    hovered: Option<usize>,
    output: OutputKind,
    folder: PathBuf,
    home: Option<PathBuf>,
    /// 同心环的绘制区域，鼠标事件据此算点在哪个环上
    rings_bounds: Rc<Cell<Bounds<Pixels>>>,
    error: Option<String>,
    /// 点了开始、服务还在新建：开始按钮不可用
    creating: bool,
    /// 额外的请求头
    headers: Entity<HeadersEditor>,
    /// 探测完成后获得焦点：方向键换档、回车开始、Esc 关闭
    focus: FocusHandle,
    /// 这个地址的请求经哪里走：还没读到系统代理时为 None
    route: Option<Route>,
    /// 舞台底部的粘贴栏（外面的，各页共用）
    dock: Entity<Dock>,
    _route: Task<()>,
    _probe: Task<()>,
    /// 磁力链接取种子信息时，把连着的节点数转进 [`Probing::Metadata`]
    _metadata: Task<()>,
    /// 探测中逐帧重画表盘，探测有了结果即停
    _frames: Task<()>,
    /// 等网页视频的组件装好（见 [`Probing::NeedsWeb`]）
    _web: Task<()>,
}

impl EventEmitter<NewTaskEvent> for NewTask {}

impl NewTask {
    /// 打开 `link` 的新建页；`from_sniff` 为从嗅探页来的，`dock` 为放在舞台底部的粘贴栏。
    pub fn new(
        service: Rc<Service>,
        link: Link,
        from_sniff: bool,
        folder: PathBuf,
        dock: Entity<Dock>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Link {
            url,
            title: page_title,
            headers,
            dropped,
            probe,
        } = link;
        let editor =
            cx.new(|cx| HeadersEditor::new(url.clone(), headers.clone(), dropped, window, cx));
        // 读系统代理要调操作系统的接口，在后台读
        let target = url.clone();
        let proxy = service.settings().map(|settings| settings.proxy);
        let reading = cx
            .background_executor()
            .spawn(async move { Route::of(&target, proxy) });
        let route = cx.spawn(async move |this, cx| {
            let route = reading.await;
            // 视图在读的期间被关掉时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                this.route = Some(route);
                cx.notify();
            });
        });
        let torrent = TorrentSource::of(&url);
        let title = if torrent.is_some() {
            torrent_title(&url)
        } else {
            task_title(&url, page_title.as_deref())
        };
        let mut view = NewTask {
            service,
            title,
            page_title,
            url,
            probing: Probing::Running,
            since: Instant::now(),
            page_check: PageCheck::Idle,
            from_sniff,
            variant: 0,
            audio: None,
            hovered: None,
            output: OutputKind::Mp4,
            folder,
            home: dirs::home_dir(),
            rings_bounds: Rc::new(Cell::new(Bounds::default())),
            error: None,
            creating: false,
            headers: editor,
            focus: cx.focus_handle(),
            route: None,
            dock,
            _route: route,
            _probe: Task::ready(()),
            _metadata: Task::ready(()),
            _frames: Task::ready(()),
            _web: Task::ready(()),
        };
        match (torrent, probe) {
            (Some(source), _) => view.fetch_torrent(source, cx),
            (None, Some(probe)) => view.probed(Probing::Ready(probe)),
            (None, None) => view.probe(headers, true, cx),
        }
        view
    }

    /// 取种子信息：磁力链接连节点去取，种子文件在后台读（之前的结果作废）。取到即可开始，取不到为失败。
    fn fetch_torrent(&mut self, source: TorrentSource, cx: &mut Context<Self>) {
        self.probing = Probing::Metadata(MetadataProgress::default());
        self.since = Instant::now();
        self.animate(cx);
        let failed = |kind, message: String| Probing::Failed(Failure { kind, message });
        let resume = match source {
            TorrentSource::Magnet => {
                let MetadataFetch {
                    mut progress,
                    resume,
                } = match self.service.fetch_metadata(self.url.to_string()) {
                    Ok(fetch) => fetch,
                    Err(error) => {
                        self.probing = failed(FailureKind::Other, error.to_string());
                        cx.notify();
                        return;
                    }
                };
                // 取的那一端随句柄释放而丢弃，这里随之结束
                self._metadata = cx.spawn(async move |this, cx| {
                    while progress.changed().await.is_ok() {
                        let now = progress.borrow_and_update().clone();
                        let shown = this.update(cx, |this, cx| {
                            if let Probing::Metadata(at) = &mut this.probing {
                                *at = now;
                                cx.notify();
                            }
                        });
                        if shown.is_err() {
                            break;
                        }
                    }
                });
                cx.background_executor().spawn(async move {
                    match resume.await {
                        Ok(Ok(resume)) => TorrentInfo::of(&resume)
                            .map(|info| (resume, info))
                            .map_err(|error| failed(FailureKind::Other, error.to_string())),
                        Ok(Err(error)) => Err(failed(FailureKind::Other, error.to_string())),
                        Err(join) => Err(failed(
                            FailureKind::Other,
                            format!("取种子信息中途出错：{join}"),
                        )),
                    }
                })
            }
            TorrentSource::File(path) => cx.background_executor().spawn(async move {
                let bytes = std::fs::read(&path).map_err(|error| {
                    failed(
                        FailureKind::Disk,
                        format!("读不出种子文件 {}：{error}", path.display()),
                    )
                })?;
                let resume = torrent_from_file(&bytes)
                    .map_err(|error| failed(FailureKind::Malformed, error.to_string()))?;
                let info = TorrentInfo::of(&resume)
                    .map_err(|error| failed(FailureKind::Malformed, error.to_string()))?;
                Ok((resume, info))
            }),
        };
        self._probe = cx.spawn(async move |this, cx| {
            let probing = match resume.await {
                Ok((resume, info)) => Probing::Torrent(Box::new(TorrentChoice::new(resume, info))),
                Err(failed) => failed,
            };
            // 视图在取的期间被关掉时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                let ready = matches!(probing, Probing::Torrent(_));
                if let Probing::Torrent(choice) = &probing {
                    this.title = choice.info.name.clone();
                }
                this.probing = probing;
                if ready {
                    cx.emit(NewTaskEvent::Ready);
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// 是磁力链接或种子文件。
    fn is_torrent(&self) -> bool {
        TorrentSource::of(&self.url).is_some()
    }

    /// 勾上或去掉种子里第 `at` 个文件（[`TorrentChoice::info`] 的文件里的位置）。
    fn toggle_file(&mut self, at: usize, cx: &mut Context<Self>) {
        if let Probing::Torrent(choice) = &mut self.probing {
            choice.wanted[at] = !choice.wanted[at];
            self.error = None;
            cx.notify();
        }
    }

    /// 种子里的文件全选（`all`）或全不选。
    fn select_all(&mut self, all: bool, cx: &mut Context<Self>) {
        if let Probing::Torrent(choice) = &mut self.probing {
            choice.wanted.fill(all);
            self.error = None;
            cx.notify();
        }
    }

    /// 带着请求头 `headers` 探测（之前的探测结果作废）；`first` 为打开时的那一次。
    fn probe(&mut self, headers: Vec<(String, String)>, first: bool, cx: &mut Context<Self>) {
        let mut source = Source::new(self.url.clone());
        source.http.headers = headers;
        self.page_check = PageCheck::Idle;
        let handle = match self.service.identify(source) {
            Ok(handle) => handle,
            Err(error) => {
                self.probing = Probing::Failed(Failure {
                    kind: FailureKind::Other,
                    message: error.to_string(),
                });
                cx.notify();
                return;
            }
        };
        self.probing = Probing::Running;
        self.since = Instant::now();
        self.animate(cx);
        self._probe = cx.spawn(async move |this, cx| {
            let (probing, not_page) = match handle.await {
                Ok(Ok(Resource::Playlist(probe))) => (Probing::Ready(probe), false),
                Ok(Ok(Resource::File(info))) => (Probing::File(Box::new(info)), false),
                // 网页：交给 yt-dlp 解析（见 web.rs），没有专门的解析器时再在网页引擎里看
                Ok(Ok(Resource::Page { .. })) => {
                    let _closed = this.update(cx, |this, cx| this.resolve_page(first, cx));
                    return;
                }
                Ok(Err(error)) => (Probing::Failed(Failure::of(&error)), not_a_playlist(&error)),
                Err(join) => (
                    Probing::Failed(Failure {
                        kind: FailureKind::Other,
                        message: format!("探测中途出错：{join}"),
                    }),
                    false,
                ),
            };
            // 视图在探测期间被关掉时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                let ready = matches!(probing, Probing::Ready(_) | Probing::File(_));
                this.probed(probing);
                if ready {
                    cx.emit(NewTaskEvent::Ready);
                } else if first && not_page {
                    cx.emit(NewTaskEvent::NotAPlaylist);
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// 探测有了结果：播放列表选中来源默认的变体与音频，文件的标题换成文件名（服务器给的，或由地址得出）。
    fn probed(&mut self, probing: Probing) {
        if let Probing::File(info) = &probing {
            // 地址经过重定向时，最后的地址里才有真正的文件名（`/download?id=1` 跳到 `/real.zip`）
            self.title = file_name(&info.url, info.file_name.as_deref());
        }
        if let Probing::Ready(probe) = &probing
            && let Some(master) = &probe.master
        {
            self.variant = master.selected.variant.index;
            self.audio = master.selected.audio.as_ref().map(|a| a.index);
        }
        self.probing = probing;
    }

    /// 外面开始在网页引擎里打开地址、看它是不是网页：探测失败的说明先不显示。
    pub fn opening_page(&mut self, cx: &mut Context<Self>) {
        self.page_check = PageCheck::Opening;
        self.animate(cx);
        cx.notify();
    }

    /// 探测中（[`Phase::Probing`]）每隔 [`dial::frame`] 重画一次，让表盘上的彗尾转起来；系统要求减少动态效果时不画。
    fn animate(&mut self, cx: &mut Context<Self>) {
        self._frames = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(dial::frame(dial::TURN))
                    .await;
                let going = this.update(cx, |this, cx| {
                    let going = this.phase() == Phase::Probing;
                    // 减少动态效果可能随时开关：开着时不画，循环照旧，关掉后接着转
                    if going && !cx.reduce_motion() {
                        cx.notify();
                    }
                    going
                });
                // 视图关掉了，或不再探测
                if !matches!(going, Ok(true)) {
                    break;
                }
            }
        });
    }

    /// 外面正在网页引擎里打开地址（[`NewTask::opening_page`] 之后，没有重新探测）。
    pub fn is_opening_page(&self) -> bool {
        self.page_check == PageCheck::Opening
    }

    /// 外面看过了、不是网页（或打不开）：显示探测失败的说明，`note` 写在下面。
    pub fn not_a_page(&mut self, note: String, cx: &mut Context<Self>) {
        self.page_check = PageCheck::NotPage(note);
        cx.notify();
    }

    /// 带上请求头编辑框里的请求头重新探测；输入框里还没按回车的一条加不进去时不探测（原因显示在输入框下面）。磁力
    /// 链接与种子文件没有请求头，重新取种子信息。
    fn reprobe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(source) = TorrentSource::of(&self.url) {
            self.fetch_torrent(source, cx);
            return;
        }
        if let Some(headers) = self
            .headers
            .update(cx, |editor, cx| editor.confirm(window, cx))
        {
            self.probe(headers, false, cx);
        }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn phase(&self) -> Phase {
        match self.probing {
            Probing::Running => Phase::Probing,
            Probing::Failed(_) if self.page_check == PageCheck::Opening => Phase::Probing,
            Probing::Failed(_) => Phase::Failed,
            Probing::Metadata(_) | Probing::Resolving | Probing::NeedsWeb(_) => Phase::Probing,
            Probing::Ready(_) | Probing::File(_) | Probing::Torrent(_) | Probing::Web(_) => {
                Phase::Ready
            }
        }
    }

    /// 让新建页获得键盘焦点：方向键换档、回车开始、Esc 关闭。
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    /// 新建页自己有焦点时的按键：↑ ← 换到码率高一档（外一环），↓ → 换到低一档，回车开始，Esc 关闭。
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 开始了就不能取消，也不再换档（网页视频为换画质）
        if !self.focus.is_focused(window) || self.creating {
            return;
        }
        let web = matches!(self.probing, Probing::Web(_));
        match event.keystroke.key.as_str() {
            "up" | "left" if web => self.step_choice(-1, cx),
            "down" | "right" if web => self.step_choice(1, cx),
            "up" | "left" => self.step_variant(-1, cx),
            "down" | "right" => self.step_variant(1, cx),
            "enter" => self.start(window, cx),
            "escape" => cx.emit(NewTaskEvent::Dismissed),
            _ => {}
        }
    }

    /// 按环由外向内的顺序从选中的一档挪 `step` 档；到头了不动。
    fn step_variant(&mut self, step: isize, cx: &mut Context<Self>) {
        let Probing::Ready(probe) = &self.probing else {
            return;
        };
        let Some(master) = &probe.master else {
            return;
        };
        let order: Vec<usize> = figure::rings(&master.variants)
            .iter()
            .map(|ring| ring.index)
            .collect();
        let Some(at) = order.iter().position(|&index| index == self.variant) else {
            return;
        };
        if let Some(&index) = at.checked_add_signed(step).and_then(|to| order.get(to)) {
            self.select_variant(index, cx);
        }
    }

    fn select_variant(&mut self, index: usize, cx: &mut Context<Self>) {
        self.variant = index;
        // 音频组换了时回到来源的默认音频
        if let Probing::Ready(probe) = &self.probing
            && let Some(master) = &probe.master
        {
            let group = master.variants[index].audio_group.as_deref();
            if let Some(audio) = self.audio
                && Some(master.audio[audio].group.as_str()) != group
            {
                self.audio = None;
            }
        }
        cx.notify();
    }

    fn change_folder(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("存到这里".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = picked.await;
            // 选择期间视图被关掉时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(Some(mut paths))) if !paths.is_empty() => {
                        let folder = paths.swap_remove(0);
                        this.folder = folder.clone();
                        cx.emit(NewTaskEvent::FolderChanged(folder));
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => this.error = Some(format!("打不开目录选择：{error}")),
                    Err(_) => this.error = Some("目录选择没有返回结果".to_owned()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 交出选择，由主窗口新建任务；请求头输入框里还没按回车的一条加不进去时不交，原因显示在输入框下面。还没探测完或
    /// 探测失败时什么也不做。
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        let kind = match &self.probing {
            Probing::Ready(probe) => {
                let preference = match &probe.master {
                    Some(master) => Preference {
                        variant: master.variants[self.variant].choice(),
                        audio: self
                            .audio
                            .map_or(AudioChoice::Default, |index| master.audio[index].choice()),
                    },
                    None => Preference {
                        variant: VariantChoice::Best,
                        audio: AudioChoice::Default,
                    },
                };
                DraftKind::Hls {
                    title: self.page_title.clone(),
                    preference,
                    output: self.output,
                    live: probe.live,
                }
            }
            Probing::File(info) => DraftKind::File {
                name: Some(self.title.clone()),
                expected: Expected::from(&**info),
                chunk_size: None,
            },
            Probing::Torrent(choice) => {
                let wanted = choice.indices();
                if wanted.is_empty() {
                    self.error = Some("至少选一个文件".to_owned());
                    cx.notify();
                    return;
                }
                DraftKind::Torrent {
                    resume: choice.resume.clone(),
                    wanted,
                }
            }
            Probing::Web(choice) => {
                let option = choice.choice();
                DraftKind::Web {
                    title: choice.resolved.info.title.clone(),
                    extractor: choice.resolved.info.extractor.clone(),
                    video_id: choice.resolved.info.id.clone(),
                    video: option.video.clone(),
                    audio: option.audio.clone(),
                    container: option.container,
                    ext: option.ext.clone(),
                }
            }
            Probing::Running
            | Probing::Failed(_)
            | Probing::Metadata(_)
            | Probing::Resolving
            | Probing::NeedsWeb(_) => return,
        };
        // 磁力链接与种子文件没有请求头；网页视频只带 Cookie（别的请求头 yt-dlp 自己定）
        let headers = if self.is_torrent() {
            Vec::new()
        } else {
            match self
                .headers
                .update(cx, |editor, cx| editor.confirm(window, cx))
            {
                Some(headers) if matches!(kind, DraftKind::Web { .. }) => headers
                    .into_iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                    .collect(),
                Some(headers) => headers,
                None => return,
            }
        };
        // 文件的地址也用原来的：重定向到的多为有时效的签名地址，续传时重新跟随
        let draft = Draft {
            url: self.url.clone(),
            headers,
            folder: self.folder.clone(),
            kind,
        };
        self.error = None;
        cx.emit(NewTaskEvent::Create(Box::new(draft)));
    }

    /// 把磁力链接交给 PikPak 云离线（整包离线，不分文件），存到选好的保存位置；没登录 PikPak 时写明先去设置里登录。
    fn start_cloud(&mut self, cx: &mut Context<Self>) {
        if self.creating {
            return;
        }
        match self.service.pikpak_user() {
            Ok(Some(_)) => {}
            Ok(None) => {
                self.error = Some("先在设置里登录 PikPak".to_owned());
                cx.notify();
                return;
            }
            Err(error) => {
                self.error = Some(format!("读不出 PikPak 的账号：{error}"));
                cx.notify();
                return;
            }
        }
        self.error = None;
        cx.emit(NewTaskEvent::Create(Box::new(Draft {
            url: self.url.clone(),
            headers: Vec::new(),
            folder: self.folder.clone(),
            kind: DraftKind::Cloud {
                provider: hs_dlx_app::CloudProvider::PikPak,
            },
        })));
    }

    /// 服务开始新建了。
    pub fn creating(&mut self, cx: &mut Context<Self>) {
        self.creating = true;
        cx.notify();
    }

    /// 新建没成：写在开始按钮上面，可以改了再开始。
    pub fn failed(&mut self, message: String, cx: &mut Context<Self>) {
        self.creating = false;
        self.error = Some(message);
        cx.notify();
    }
}

impl Render for NewTask {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let figure = self.render_figure(cx);
        let inspector = self.render_inspector(cx);
        // 舞台至少放得下整个同心环区域：窗口窄时先收两侧的面板
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .flex_1()
            .min_w(px(MIN_WIDTH))
            .min_h_0()
            .flex()
            .gap(px(GAP))
            .child(ui::stage(
                FIGURE_WIDTH,
                figure,
                self.keys_hint(),
                self.dock.clone(),
            ))
            .child(inspector)
    }
}

/// 探测结果的说明（不含主机）：时长（直播不写）、几档清晰度、几条另外的音轨、是否直播。
pub fn describe(probe: &Probe) -> Vec<String> {
    let mut parts = Vec::new();
    if !probe.live {
        parts.push(format::clock(duration_of(probe)));
    }
    match &probe.master {
        Some(master) => {
            parts.push(format!("{} 档清晰度", master.variants.len()));
            let audio = master.audio.iter().filter(|audio| audio.separate).count();
            if audio > 0 {
                parts.push(format!("{audio} 条音轨"));
            }
        }
        None => parts.push("单一清晰度".to_owned()),
    }
    if probe.live {
        parts.push("直播".to_owned());
    }
    parts
}

/// 第 0 条轨（所选变体或来源本身）的时长；直播为当前窗口的。
fn duration_of(probe: &Probe) -> Duration {
    probe.tracks.first().map_or(Duration::ZERO, |t| t.duration)
}
