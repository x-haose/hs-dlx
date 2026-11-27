//! 还没定下的任务的准备：认地址、解析网页、取种子信息，到要用户决定的一步停下等，定下是哪种任务后接着下载。
//!
//! 准备按「轮」走：每一轮是运行时里的一个任务，与下载一样登记在运行表里（暂停、放弃、关闭经同一个登记找到它）。一轮
//! 以四种方式之一结束：定下（登记换成下载的）、停下等用户（[`Status::Waiting`]，可选项记在内存里）、失败、被取消
//! （暂停）。用户的选择、继续、交给网盘各开新的一轮。
//!
//! 例外：在网页里找流的一轮（见 `in_page`）停下等用户在网页里动手（[`Ask::Page`]）时不结束，照旧登记着、占着网页的
//! 名额，用户弄好了它自己把状态改回准备中接着走。所以等用户的任务里只有这一种在运行表里：暂停它即关掉网页，换请求头
//! （[`Answer::Headers`]）顶掉它。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use hs_dlx_ytdlp::{ExtractError, InstallProgress, Live};
use hs_m3u8_core::hls::{AudioChoice, Preference, VariantChoice};
use hs_m3u8_core::{
    CancellationToken, FileInfo, Probe, Resource, Source, Url, check_option_header,
};
use rusqlite::Transaction;

use super::{Control, Ending, Event, Resolved, Run, RunningGuard, Shared, WebState, lock};
use crate::intake::{self, Intake};
use crate::page::{Check, Found};
use crate::store::{self, NewPending};
use crate::web::no_choice_reason;
use crate::{
    Ask, CloudProvider, Draft, DraftKind, Error, Expected, Failure, FailureKind, MetadataProgress,
    OutputKind, PageAsk, Settings, Status, Task, TaskId, TaskSpec, TorrentInfo, WebSpec,
    maybe_a_page, naming, torrent,
};

/// 准备中的任务此刻在做哪一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// 认地址：发探测请求，看它是播放列表、文件还是网页
    Identifying,
    /// 插件 `plugin`（[`WebSpec::YT_DLP`] 为随带的 yt-dlp）在解析网页
    Resolving { plugin: String },
    /// 在装解析要用的组件
    Installing(InstallProgress),
    /// 磁力链接在取种子信息
    Metadata(MetadataProgress),
    /// 要在内置网页里找流，在等网页的名额：前面还有 `ahead` 个任务；`away` 为宿主此刻开不了网页（桌面应用的主
    /// 窗口关着），要等它又能开了才轮得到
    PageQueued { ahead: usize, away: bool },
    /// 网页开着（占着一个名额），在后台找流、逐条探测：已找到 `found` 条
    Page { found: usize },
    /// 网页已关掉、名额已让出：在照找到的流定下，或在重新取网页引擎打开后发现不是网页的地址
    Probing,
    /// 都定了：在取名、建任务目录，随即开始下载
    Starting,
}

/// 还没定下的任务此刻的样子（[`crate::Handle::stage`]）。
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// 服务在往下走
    Working(Step),
    /// 停下等用户：可选项
    Asking(Offer),
}

/// 停下等用户时给出的可选项；在等什么（[`Ask`]）由它得出。
#[derive(Debug, Clone, PartialEq)]
pub enum Offer {
    /// HLS 有两档以上：各档变体、音轨、时长、是否直播
    Quality(Box<Probe>),
    /// 网页视频解析出的各档画质
    WebQuality(Box<Resolved>),
    /// 种子里有两个以上文件
    Files {
        info: Box<TorrentInfo>,
        /// 登录了 PikPak：来源是磁力链接时可以改交给它（[`crate::Handle::offload`]）
        pikpak: bool,
    },
    /// 没有要选的，开始前确认：认出的是什么
    Confirm(Recognized),
    /// 解析网页要用的组件没装好：它此刻的状态（没装、上次没装成、坏了）
    Components(WebState),
    /// 网页里找到几条探测成功的流，选一条；或找到的都没探测成（各自的原因在里面，可以换请求头再来）
    Streams(Vec<StreamOption>),
    /// 要用户在网页里动手：网页由宿主显示着。过了人机验证、点了播放找到流之后任务自己接着走，不用回答
    Page(PageAsk),
}

/// 网页里找到的一条流。
#[derive(Debug, Clone, PartialEq)]
pub struct StreamOption {
    pub url: Url,
    /// 探测的结果；没成（或采用别的流时还没轮到它）时为原因，这样的不能选
    pub probed: Result<Box<Probe>, String>,
}

/// 没有要选的任务认出的是什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recognized {
    /// 单个文件
    File(Box<FileInfo>),
    /// 只有一档的 HLS（含直播）
    Hls(Box<Probe>),
    /// 只有一个文件的种子
    Torrent(Box<TorrentInfo>),
}

impl Offer {
    /// 给出这些可选项时任务在等什么。
    pub fn ask(&self) -> Ask {
        match self {
            Offer::Quality(_) | Offer::WebQuality(_) => Ask::Quality,
            Offer::Files { .. } => Ask::Files,
            Offer::Confirm(_) => Ask::Confirm,
            Offer::Components(_) => Ask::Components,
            Offer::Streams(_) => Ask::Stream,
            Offer::Page(ask) => Ask::Page(*ask),
        }
    }
}

/// 用户对可选项的选择（[`crate::Handle::choose`]）。保存目录都须是绝对路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// HLS（[`Offer::Quality`] 与认出 HLS 的 [`Offer::Confirm`]）：选轨（变体须是可选项里的或最高画质，不能按
    /// 下标）、输出种类与保存目录
    Hls {
        preference: Preference,
        output: OutputKind,
        folder: PathBuf,
    },
    /// 网页视频（[`Offer::WebQuality`]）：画质在可选项里的下标与保存目录
    Web { choice: usize, folder: PathBuf },
    /// 种子（[`Offer::Files`] 与认出种子的 [`Offer::Confirm`]）：要下的文件（[`crate::TorrentFile::index`]，至少一个）
    /// 与保存目录，由本机 BT 下
    Files { wanted: Vec<i32>, folder: PathBuf },
    /// 单个文件（认出文件的 [`Offer::Confirm`]）：保存目录
    File { folder: PathBuf },
    /// 网页里找到的流（[`Offer::Streams`]）：用可选项里的第几条，须是探测成功的；它有两档以上时接着停下选画质
    Stream(usize),
    /// 安装组件并接着解析（[`Offer::Components`]）。组件是各任务共用的：之后暂停或放弃这个任务不停掉安装
    Install,
    /// 换掉任务的请求头并从头准备；对哪种可选项都可以，来源是磁力链接或种子文件的任务没有请求头
    Headers(Vec<(String, String)>),
}

/// 一轮准备从哪里走起。
pub(super) enum Entry {
    /// 从头：认地址（磁力链接、种子文件为取种子信息）
    Fresh,
    /// 先装解析组件，再从头
    Install,
    /// 用网页里找到的这条流：有两档以上时停下选画质，否则定下
    Stream(Box<Adopted>),
    /// 都定了：照它取名、定下并开始
    Settle(Box<Draft>),
}

/// 停下等用户时记在内存里的：给用户的可选项，与照选择定下任务时要用、不给用户看的来历。
pub(super) struct Asked {
    pub offer: Offer,
    /// 可选项里的 HLS 或文件不是照任务自己的地址与请求头取的（网页里找到的流、带着网页引擎的 Cookie 重新取的地址）
    /// 时：取它的地址与请求头
    origin: Option<Origin>,
    /// [`Offer::Streams`] 各条流的来历，与可选项一一对应；没探测成的为 None
    streams: Vec<Option<Origin>>,
}

/// 一个 HLS 或文件是从哪取的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Origin {
    pub url: Url,
    /// 探测时带的请求头，下载照它带
    pub headers: Vec<(String, String)>,
    /// 所在网页的标题，任务照它取名；None 时由地址得出
    pub title: Option<String>,
}

/// 网页里找到、要用的一条流：探测结果与来历。
pub(super) struct Adopted {
    pub probe: Box<Probe>,
    pub origin: Origin,
}

impl Asked {
    /// 照任务自己的地址与请求头给出的可选项。
    pub fn plain(offer: Offer) -> Asked {
        Asked {
            offer,
            origin: None,
            streams: Vec::new(),
        }
    }

    /// 网页里找到的几条流 `found`，所在网页的标题为 `title`。
    pub fn streams(found: Vec<Found>, title: Option<&str>) -> Asked {
        let mut options = Vec::with_capacity(found.len());
        let mut streams = Vec::with_capacity(found.len());
        for found in found {
            let (probed, origin) = match found.check {
                Check::Ready { probe, headers } => (
                    Ok(probe),
                    Some(Origin {
                        url: found.url.clone(),
                        headers,
                        title: title.map(str::to_owned),
                    }),
                ),
                Check::Failed { reason, .. } => (Err(reason), None),
                Check::Waiting | Check::Running => (Err("还没轮到探测它".to_owned()), None),
            };
            options.push(StreamOption {
                url: found.url,
                probed,
            });
            streams.push(origin);
        }
        Asked {
            offer: Offer::Streams(options),
            origin: None,
            streams,
        }
    }
}

/// 一轮准备往下走到的结论。
pub(super) enum Next {
    Ask(Box<Asked>),
    Settle(Box<Draft>),
}

impl Next {
    pub(super) fn ask(asked: Asked) -> Next {
        Next::Ask(Box::new(asked))
    }
}

/// 一轮准备没走到结论。
pub(super) enum Stop {
    /// 被取消：暂停、放弃、关闭，或被另一轮顶掉
    Cancelled,
    Failed(Failure),
    /// 任务正停在网页上等用户，而网页没了（宿主关掉了它）：状态照旧是等用户，可选项作废，与重开应用后一样要继续、
    /// 重新准备
    Lapsed,
}

impl Stop {
    pub(super) fn failed(kind: FailureKind, message: impl Into<String>) -> Stop {
        Stop::Failed(Failure {
            kind,
            message: message.into(),
        })
    }

    /// 服务自己的错误 `error`（读写存储、取名、建会话等）对应的失败。
    pub(super) fn of(error: &Error) -> Stop {
        let kind = match error {
            Error::Core(error) => return Stop::Failed(Failure::of(error)),
            // 种子已在别的任务里：再试也一样
            Error::InvalidInput(_) | Error::Corrupt(_) | Error::TorrentTaken(_) => {
                FailureKind::Unrecoverable
            }
            Error::Io { .. } | Error::TimedOut { .. } => FailureKind::Disk,
            Error::PathTaken(_) => FailureKind::OutputTaken,
            Error::NotLoggedIn => FailureKind::CloudLogin,
            // 有网页视频任务在跑、组件重装不了：暂停它们之后再试
            Error::WebUnavailable | Error::WebNotReady | Error::WebInstalling | Error::WebInUse => {
                FailureKind::WebComponents
            }
            _ => FailureKind::Other,
        };
        Stop::failed(kind, error.to_string())
    }
}

/// 一轮准备：它在运行表里的登记与取消的信号。
pub(super) struct Flow {
    pub id: TaskId,
    pub cancel: CancellationToken,
    pub guard: RunningGuard,
}

/// 地址回的是网页而没有解析器认得它时的失败。
fn not_media() -> Failure {
    Failure {
        kind: FailureKind::Malformed,
        message: "地址回的是网页，不是视频流或文件".to_owned(),
    }
}

/// 认领网址 `url` 的插件的编号；没有插件认领时为 None。
fn claimed(_url: &hs_m3u8_core::Url) -> Option<String> {
    None
}

/// 还没定下的任务在知道更好的名字之前的标题：磁力链接为显示名（`dn`，整理成文件主名），没有时为「磁力链接」；种子
/// 文件为去掉 `.torrent` 的文件名；其余由地址得出（[`naming::task_title`]）。
pub(super) fn pending_title(url: &Url) -> String {
    match url.scheme() {
        "magnet" => {
            let name = url
                .query_pairs()
                .find(|(key, _)| key == "dn")
                .map(|(_, name)| naming::clean_title(&name))
                .unwrap_or_default();
            if name.is_empty() {
                "磁力链接".to_owned()
            } else {
                name
            }
        }
        "file" => url
            .to_file_path()
            .ok()
            .and_then(|path| {
                path.file_stem()
                    .map(|stem| naming::clean_title(&stem.to_string_lossy()))
            })
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "种子文件".to_owned()),
        _ => naming::task_title(url, None),
    }
}

/// 没让用户选轨时 HLS 的选轨：来源默认选中的变体与音频（按身份）；来源本身是媒体播放列表时为最高画质、默认音频。
fn default_preference(probe: &Probe) -> Preference {
    match &probe.master {
        Some(master) => Preference {
            variant: master.selected.variant.choice(),
            audio: master
                .selected
                .audio
                .as_ref()
                .map_or(AudioChoice::Default, |audio| audio.choice()),
        },
        None => Preference {
            variant: VariantChoice::Best,
            audio: AudioChoice::Default,
        },
    }
}

/// 选轨 `preference` 的变体与音频是不是 `probe` 给出的可选项：变体为最高画质或其中一个的身份，音频为默认、按语言或
/// 其中一个的身份。
fn offered(probe: &Probe, preference: &Preference) -> bool {
    let variants = probe.master.iter().flat_map(|master| &master.variants);
    let audio = probe.master.iter().flat_map(|master| &master.audio);
    let variant = preference.variant == VariantChoice::Best
        || variants
            .into_iter()
            .any(|variant| variant.choice() == preference.variant);
    let audio = matches!(
        preference.audio,
        AudioChoice::Default | AudioChoice::Language(_)
    ) || audio
        .into_iter()
        .any(|audio| audio.choice() == preference.audio);
    variant && audio
}

fn absolute(folder: &Path) -> Result<(), Error> {
    if folder.is_absolute() {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "保存目录要是绝对路径：{}",
            folder.display()
        )))
    }
}

/// 把还没定下的任务 `task` 定下为 HLS 任务的草稿：地址、请求头照 `origin`（网页里找到的流；任务照网页的标题取名），
/// 没有时照任务自己的。
fn hls_draft(
    task: &Task,
    probe: &Probe,
    preference: Preference,
    output: OutputKind,
    folder: PathBuf,
    origin: Option<&Origin>,
) -> Draft {
    Draft {
        url: origin.map_or_else(|| task.spec.url.clone(), |origin| origin.url.clone()),
        headers: origin.map_or_else(
            || task.spec.headers.clone(),
            |origin| origin.headers.clone(),
        ),
        folder,
        kind: DraftKind::Hls {
            title: origin.and_then(|origin| origin.title.clone()),
            preference,
            output,
            live: probe.live,
        },
    }
}

/// 认出 HLS（探测结果 `probe`，来历 `origin` 同 [`hls_draft`]）之后：有两档以上时停下选画质，设置了开始前确认时停下
/// 确认，否则照来源默认的选轨定下，存成 MP4。
pub(super) fn hls_next(
    task: &Task,
    probe: Box<Probe>,
    origin: Option<Origin>,
    folder: PathBuf,
    settings: &Settings,
) -> Next {
    let several = probe
        .master
        .as_ref()
        .is_some_and(|master| master.variants.len() >= 2);
    let offer = if several {
        Offer::Quality(probe)
    } else if settings.confirm_before_start {
        Offer::Confirm(Recognized::Hls(probe))
    } else {
        let preference = default_preference(&probe);
        return Next::Settle(Box::new(hls_draft(
            task,
            &probe,
            preference,
            OutputKind::Mp4,
            folder,
            origin.as_ref(),
        )));
    };
    Next::ask(Asked {
        offer,
        origin,
        streams: Vec::new(),
    })
}

/// 把还没定下的任务 `task` 定下为文件任务的草稿：地址用任务原来的（重定向到的多为有时效的签名地址，续传时重新
/// 跟随），文件名取服务器给的或重定向之后的地址里的；请求头照 `origin`（带着网页引擎的 Cookie 认出的），没有时照任务
/// 自己的。
fn file_draft(task: &Task, info: &FileInfo, folder: PathBuf, origin: Option<&Origin>) -> Draft {
    Draft {
        url: task.spec.url.clone(),
        headers: origin.map_or_else(
            || task.spec.headers.clone(),
            |origin| origin.headers.clone(),
        ),
        folder,
        kind: DraftKind::File {
            name: Some(naming::file_name(&info.url, info.file_name.as_deref())),
            expected: Expected::from(info),
            chunk_size: None,
        },
    }
}

/// 把还没定下的任务 `task` 定下为网页视频任务的草稿，画质为 `resolved` 里的第 `choice` 个（调用方已核对有）：地址用
/// 解析器整理过的网页地址（短链接换成视频页：Cookie 按它的域名带，重新解析也不依赖短链接还在），请求头只带 Cookie。
fn web_draft(task: &Task, resolved: &Resolved, choice: usize, folder: PathBuf) -> Draft {
    let option = &resolved.choices[choice];
    let url = resolved
        .info
        .page
        .as_deref()
        .and_then(|page| hs_m3u8_core::Url::parse(page).ok())
        .filter(|page| matches!(page.scheme(), "http" | "https"))
        .unwrap_or_else(|| task.spec.url.clone());
    Draft {
        url,
        headers: task
            .spec
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("cookie"))
            .cloned()
            .collect(),
        folder,
        kind: DraftKind::Web {
            title: resolved.info.title.clone(),
            extractor: resolved.info.extractor.clone(),
            video_id: resolved.info.id.clone(),
            video: option.video.clone(),
            audio: option.audio.clone(),
            container: option.container,
            ext: option.ext.clone(),
        },
    }
}

/// 把还没定下的任务 `task` 定下为 BT 任务的草稿：下种子（续传数据 `resume`）里的 `wanted`。
fn torrent_draft(task: &Task, resume: Vec<u8>, wanted: Vec<i32>, folder: PathBuf) -> Draft {
    Draft {
        url: task.spec.url.clone(),
        headers: Vec::new(),
        folder,
        kind: DraftKind::Torrent { resume, wanted },
    }
}

impl Shared {
    /// 按拆好的一条 `intake` 建还没定下的任务并开始准备（见 [`crate::Handle::add`]）；调用方须在运行时里。
    pub(super) async fn add(
        self: &Arc<Self>,
        intake: Intake,
        created_at: i64,
    ) -> Result<Task, Error> {
        let Intake {
            url,
            headers,
            folder,
        } = intake;
        for (name, value) in &headers {
            check_option_header(name, value).map_err(Error::InvalidInput)?;
        }
        absolute(&folder)?;
        if !matches!(url.scheme(), "http" | "https") && !intake::torrent_source(&url) {
            return Err(Error::InvalidInput(format!(
                "要 http 或 https 的地址、磁力链接或种子文件的地址：{url}"
            )));
        }
        // 规整成与核心算任务目录时相同的写法（去掉 `.` 与重复的分隔符）
        let folder = std::path::absolute(&folder).map_err(|cause| Error::Io {
            action: "规整",
            path: folder.clone(),
            cause,
        })?;
        let torrent = if intake::torrent_source(&url) {
            if !headers.is_empty() {
                return Err(Error::InvalidInput(
                    "磁力链接与种子文件没有请求头".to_owned(),
                ));
            }
            Some(match url.scheme() {
                "magnet" => {
                    let id = hs_dlx_torrent::magnet_id(url.as_str())
                        .map_err(|error| Error::InvalidInput(format!("磁力链接用不了：{error}")))?;
                    (id.as_str().to_owned(), None)
                }
                _ => {
                    let path = url.to_file_path().map_err(|()| {
                        Error::InvalidInput(format!("种子文件的地址不是本机的路径：{url}"))
                    })?;
                    // 大种子有几 MB，在阻塞线程上读与解析
                    let (resume, info) = tokio::task::spawn_blocking(move || {
                        let bytes = std::fs::read(&path).map_err(|cause| Error::Io {
                            action: "读",
                            path,
                            cause,
                        })?;
                        let resume = torrent::torrent_from_file(&bytes)?;
                        let info = TorrentInfo::of(&resume)?;
                        Ok::<_, Error>((resume, info))
                    })
                    .await
                    .expect("读种子文件不会 panic，运行时关闭时本任务也一起丢弃")?;
                    (info.info_hash, Some(resume))
                }
            })
        } else {
            None
        };
        let title = pending_title(&url);
        let mut store = lock(&self.store);
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        let id = store::insert_pending(
            &tx,
            &NewPending {
                title: &title,
                url: &url,
                headers: &headers,
                folder: &folder,
                torrent: torrent
                    .as_ref()
                    .map(|(info_hash, resume)| (info_hash.as_str(), resume.as_deref())),
            },
            created_at,
        )?;
        let task = store::get(&tx, id)?.expect("同一事务里刚插入的行读得到");
        self.begin_flow(tx, applied, task, Entry::Fresh, true)
    }

    /// 给还没定下的任务 `task` 开一轮准备，从 `entry` 走起：状态记为准备中，提交 `tx`（连同补写了的 `applied`），登记、
    /// 起这一轮，发 [`Event::Added`]（`added` 时）或 [`Event::Changed`]。这个任务在跑的那一轮（交给网盘时顶掉取种子
    /// 信息的，换请求头时顶掉停在网页上等用户的）随之作废，它占着的网页名额随登记换掉而归还。返回准备中的任务；提交不了时什么也没开始。调用方持着 `store` 锁、核对过服务没关闭，且在
    /// 运行时里。
    pub(super) fn begin_flow(
        self: &Arc<Self>,
        tx: Transaction<'_>,
        applied: Vec<(TaskId, Ending)>,
        mut task: Task,
        entry: Entry,
        added: bool,
    ) -> Result<Task, Error> {
        let id = task.id;
        task.status = Status::Preparing;
        store::set_status(&tx, id, &task.status)?;
        self.commit(tx, applied)?;
        lock(&self.offers).remove(&id);
        let step = match &entry {
            Entry::Fresh if intake::torrent_source(&task.spec.url) => {
                Step::Metadata(MetadataProgress::default())
            }
            Entry::Fresh => Step::Identifying,
            Entry::Install => Step::Installing(InstallProgress {
                stage: hs_dlx_ytdlp::Stage::Preparing,
                bytes: 0,
                total: None,
            }),
            Entry::Stream(_) | Entry::Settle(_) => Step::Starting,
        };
        if let Some(superseded) = lock(&self.running).get(&id) {
            superseded.control.cancel();
        }
        let cancel = CancellationToken::new();
        let guard = self.register_with(id, Control::Prepare(cancel.clone()), Some(step.clone()));
        let boxed = Box::new(task.clone());
        self.events.send(if added {
            Event::Added { task: boxed }
        } else {
            Event::Changed { task: boxed }
        });
        self.events.send(Event::Stage {
            id,
            stage: Some(Box::new(Stage::Working(step))),
        });
        let shared = self.clone();
        self.tracker
            .spawn(async move { shared.run_flow(Flow { id, cancel, guard }, entry).await });
        Ok(task)
    }

    /// 见 [`crate::Handle::choose`]；调用方须在运行时里。
    pub(super) fn choose(self: &Arc<Self>, id: TaskId, answer: Answer) -> Result<Task, Error> {
        let mut store = lock(&self.store);
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        if lock(&self.discarding).contains(&id) {
            return Err(Error::Discarding(id));
        }
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        // 等用户的任务里只有停在网页上的那种在运行表里（见模块说明）：新的一轮顶掉它
        let mut task = store::get(&tx, id)?.ok_or(Error::NotFound(id))?;
        if !matches!(task.status, Status::Waiting(_)) {
            return Err(Error::InvalidInput(format!("{id}没有在等选择")));
        }
        let entry = {
            let offers = lock(&self.offers);
            let asked = offers.get(&id).ok_or_else(|| {
                Error::InvalidInput(format!("{id}的可选项已不在，要先继续、重新准备"))
            })?;
            entry_for(&tx, &mut task, asked, answer)?
        };
        self.begin_flow(tx, applied, task, entry, false)
    }

    /// 见 [`crate::Handle::offload`]；调用方须在运行时里。
    pub(super) fn offload(
        self: &Arc<Self>,
        id: TaskId,
        provider: CloudProvider,
    ) -> Result<Task, Error> {
        let mut store = lock(&self.store);
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        if lock(&self.discarding).contains(&id) {
            return Err(Error::Discarding(id));
        }
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        let task = store::get(&tx, id)?.ok_or(Error::NotFound(id))?;
        let Some(pending) = task.spec.pending() else {
            return Err(Error::InvalidInput(format!(
                "{id}已定下是哪种任务，不能再交给网盘"
            )));
        };
        if task.spec.url.scheme() != "magnet" {
            return Err(Error::InvalidInput("云离线只收磁力链接".to_owned()));
        }
        // 没登录时任务照旧
        self.pikpak_session_in(&tx)?;
        let draft = Draft {
            url: task.spec.url.clone(),
            headers: Vec::new(),
            folder: pending.folder.clone(),
            kind: DraftKind::Cloud { provider },
        };
        self.begin_flow(tx, applied, task, Entry::Settle(Box::new(draft)), false)
    }

    /// 见 [`crate::Handle::stage`]。
    pub(super) fn stage(&self, id: TaskId) -> Result<Option<Stage>, Error> {
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        store::get(&tx, id)?.ok_or(Error::NotFound(id))?;
        let step = lock(&self.running)
            .get(&id)
            .and_then(|running| running.step.clone());
        Ok(match step {
            Some(step) => Some(Stage::Working(step)),
            None => lock(&self.offers)
                .get(&id)
                .map(|asked| Stage::Asking(asked.offer.clone())),
        })
    }

    /// 启动时：在等选文件的任务（[`Ask::Files`]）存着种子的续传数据，由它得出可选项，各发一次 [`Event::Stage`]。读
    /// 不出的（存储出错、续传数据认不出）照没有可选项处理：继续时重新准备，那时报出。
    pub(super) fn restore_offers(self: &Arc<Self>) {
        let shared = self.clone();
        let _entered = self.runtime.enter();
        self.tracker.spawn(async move {
            let stored = shared.write(|tx| {
                let pikpak = store::pikpak_account(tx)?.is_some();
                let mut resumes = Vec::new();
                for id in store::waiting_for(tx, Ask::Files)? {
                    if let Some(resume) = store::pending_torrent(tx, id)? {
                        resumes.push((id, resume));
                    }
                }
                Ok((pikpak, resumes))
            });
            let Ok((pikpak, resumes)) = stored else {
                return;
            };
            for (id, resume) in resumes {
                let info = tokio::task::spawn_blocking(move || TorrentInfo::of(&resume))
                    .await
                    .expect("读种子信息不会 panic，运行时关闭时本任务也一起丢弃");
                let Ok(info) = info else {
                    continue;
                };
                let offer = Offer::Files {
                    info: Box::new(info),
                    pikpak,
                };
                {
                    // 这期间任务可能已继续或放弃
                    let mut store = lock(&shared.store);
                    let waiting = store
                        .transaction()
                        .and_then(|tx| store::get(&tx, id))
                        .is_ok_and(|task| {
                            task.is_some_and(|task| task.status == Status::Waiting(Ask::Files))
                        });
                    if !waiting || lock(&shared.running).contains_key(&id) {
                        continue;
                    }
                    lock(&shared.offers).insert(id, Asked::plain(offer.clone()));
                    shared.events.send(Event::Stage {
                        id,
                        stage: Some(Box::new(Stage::Asking(offer))),
                    });
                }
            }
        });
    }

    /// 跑一轮准备到结束：定下并开始（登记换成下载的），或把停下的状态写进存储（见 [`Shared::finish_flow`]）。
    async fn run_flow(self: Arc<Self>, flow: Flow, entry: Entry) {
        let walked = flow
            .cancel
            .run_until_cancelled(self.walk(&flow, entry))
            .await
            .unwrap_or(Err(Stop::Cancelled));
        let ended = match walked {
            Ok(Next::Ask(asked)) => Ok(*asked),
            Ok(Next::Settle(draft)) => {
                let settled = flow
                    .cancel
                    .run_until_cancelled(self.settle(&flow, *draft))
                    .await
                    .unwrap_or(Err(Stop::Cancelled));
                match settled {
                    // 登记已换成下载的：这一轮的登记释放时不动它
                    Ok(()) => return,
                    Err(stop) => Err(stop),
                }
            }
            Err(stop) => Err(stop),
        };
        self.finish_flow(flow, ended);
    }

    /// 一轮准备没定下就结束了：停下等用户（`ended` 为可选项）、失败、被取消，或停在网页上等用户时网页没了。在
    /// `store` 锁内核对这一轮没被顶掉（顶掉了的什么也不做），把状态写进存储（写不进时记为待补写）、换掉记着的可选项、
    /// 发 [`Event::Changed`]、[`Event::Stage`]（有可选项时）与 [`Event::Ended`]，最后去掉登记（它占着的网页名额随之
    /// 归还）：都在锁内，别的操作与下一个轮到名额的任务的事件排不到这之前。结论出来之后才到的取消（暂停）照暂停算。
    ///
    /// 停在网页上等用户的一轮被关闭服务取消、或网页没了（[`Stop::Lapsed`]）时状态不动（照旧等用户，重开应用后与别的
    /// 等用户的任务一样），只作废可选项：发可选项没有了的 [`Event::Stage`] 与 [`Event::Ended`]。
    fn finish_flow(&self, flow: Flow, ended: Result<Asked, Stop>) {
        let Flow { id, cancel, guard } = flow;
        let mut store = lock(&self.store);
        if !self.current(id, &guard) {
            return;
        }
        let ended = match ended {
            _ if cancel.is_cancelled() => Err(Stop::Cancelled),
            ended => ended,
        };
        let lapsed = match &ended {
            Err(Stop::Lapsed) => true,
            Err(Stop::Cancelled) => {
                self.closed.load(Ordering::Relaxed)
                    && self
                        .task_in(&mut store, id)
                        .is_ok_and(|task| matches!(task.status, Status::Waiting(Ask::Page(_))))
            }
            Ok(_) | Err(Stop::Failed(_)) => false,
        };
        lock(&self.offers).remove(&id);
        if lapsed {
            self.events.send(Event::Stage { id, stage: None });
            // 读不出时不报这一轮停下：存储出了错，状态照它里面的
            if let Ok(task) = self.task_in(&mut store, id) {
                self.events.send(Event::Ended {
                    id,
                    status: task.status,
                    result: None,
                    total: None,
                    leftovers: Vec::new(),
                    unsaved: None,
                });
            }
            drop(guard);
            return;
        }
        let (status, asked) = match ended {
            Ok(asked) => (Status::Waiting(asked.offer.ask()), Some(asked)),
            Err(Stop::Failed(failure)) => (Status::Failed(failure), None),
            Err(Stop::Cancelled | Stop::Lapsed) => (Status::Paused, None),
        };
        let ending = Ending::of(status);
        let saved = self.save_in(&mut store, id, &ending);
        let offer = asked.map(|asked| {
            let offer = asked.offer.clone();
            lock(&self.offers).insert(id, asked);
            offer
        });
        self.announce_in(&mut store, id, ending, offer, Vec::new(), saved);
        drop(guard);
    }

    /// 任务 `id` 在运行表里的登记还是 `guard` 的那一次（没被另一轮或下载顶掉）。
    pub(super) fn current(&self, id: TaskId, guard: &RunningGuard) -> bool {
        lock(&self.running)
            .get(&id)
            .is_some_and(|running| running.serial == guard.serial)
    }

    /// 这一轮走到了 `step`：记下并发 [`Event::Stage`]；已被顶掉的不记。在 `store` 锁内做：与顶掉它的那一轮发的
    /// 事件先后分明。
    pub(super) fn step(&self, flow: &Flow, step: Step) {
        let _store = lock(&self.store);
        self.step_in(flow, step);
    }

    /// 同 [`Shared::step`]，调用方已持着 `store` 锁。
    pub(super) fn step_in(&self, flow: &Flow, step: Step) {
        {
            let mut running = lock(&self.running);
            match running.get_mut(&flow.id) {
                Some(running) if running.serial == flow.guard.serial => {
                    if running.step.as_ref() == Some(&step) {
                        return;
                    }
                    running.step = Some(step.clone());
                }
                _ => return,
            }
        }
        self.events.send(Event::Stage {
            id: flow.id,
            stage: Some(Box::new(Stage::Working(step))),
        });
    }

    /// 准备中知道了更好的名字：换掉标题（整理成文件主名，什么都不剩或与 `task` 里的相同时不动），发
    /// [`Event::Changed`]。
    pub(super) fn rename(&self, flow: &Flow, task: &Task, title: &str) -> Result<(), Stop> {
        let title = naming::clean_title(title);
        if title.is_empty() || title == task.spec.title {
            return Ok(());
        }
        self.retitle(flow, &title)
    }

    /// 把这一轮的任务的标题换成 `title`（已整理成文件主名），发 [`Event::Changed`]。
    pub(super) fn retitle(&self, flow: &Flow, title: &str) -> Result<(), Stop> {
        let id = flow.id;
        self.change(|tx| {
            store::set_pending_title(tx, id, title)?;
            store::get(tx, id)?.ok_or(Error::NotFound(id))
        })
        .map(drop)
        .map_err(|error| Stop::of(&error))
    }

    async fn walk(self: &Arc<Self>, flow: &Flow, entry: Entry) -> Result<Next, Stop> {
        match entry {
            Entry::Settle(draft) => Ok(Next::Settle(draft)),
            Entry::Stream(adopted) => {
                let (task, folder, settings) = self.unsettled(flow)?;
                let Adopted { probe, origin } = *adopted;
                Ok(hls_next(&task, probe, Some(origin), folder, &settings))
            }
            Entry::Install => {
                // 这期间别处装好了的不重装
                let missing = matches!(
                    *self.web.state().borrow(),
                    WebState::Missing | WebState::Failed(_) | WebState::Broken(_)
                );
                if missing {
                    self.install_web().map_err(|error| Stop::of(&error))?;
                }
                self.fresh(flow).await
            }
            Entry::Fresh => self.fresh(flow).await,
        }
    }

    /// 从头准备：磁力链接、种子文件取种子信息；http(s) 的地址先看有没有插件认领，没有就自己探测一次，是播放列表、
    /// 文件的照它定，是网页的交给 yt-dlp 解析。
    async fn fresh(self: &Arc<Self>, flow: &Flow) -> Result<Next, Stop> {
        let (task, folder, settings) = self.unsettled(flow)?;
        if intake::torrent_source(&task.spec.url) {
            return self.torrent(flow, &task, folder, &settings).await;
        }
        if let Some(plugin) = claimed(&task.spec.url) {
            return self.page(flow, &task, folder, &settings, plugin).await;
        }
        self.step(flow, Step::Identifying);
        let mut source = Source::new(task.spec.url.clone());
        source.http.headers.clone_from(&task.spec.headers);
        source.http.proxy = settings.proxy.clone();
        match self.engine.identify(&source).await {
            Ok(Resource::Playlist(probe)) => Ok(hls_next(&task, probe, None, folder, &settings)),
            Ok(Resource::File(info)) => self.file_next(flow, &task, info, None, folder, &settings),
            Ok(Resource::Page { .. }) => {
                self.page(flow, &task, folder, &settings, WebSpec::YT_DLP.to_owned())
                    .await
            }
            Err(error) if maybe_a_page(&error) => {
                self.in_page(flow, &task, folder, &settings, Failure::of(&error))
                    .await
            }
            Err(error) => Err(Stop::Failed(Failure::of(&error))),
        }
    }

    /// 这一轮的任务（还没定下）、它的保存目录与此刻的设置。
    fn unsettled(&self, flow: &Flow) -> Result<(Task, PathBuf, Settings), Stop> {
        let task = self.get(flow.id).map_err(|error| Stop::of(&error))?;
        let folder = task
            .spec
            .pending()
            .expect("准备的只会是还没定下的任务")
            .folder
            .clone();
        let settings = self.settings().map_err(|error| Stop::of(&error))?;
        Ok((task, folder, settings))
    }

    /// 认出单个文件 `info`（请求头照 `origin`，同 [`file_draft`]）之后：任务改叫文件名，设置了开始前确认时停下确认，
    /// 否则定下。
    pub(super) fn file_next(
        &self,
        flow: &Flow,
        task: &Task,
        info: FileInfo,
        origin: Option<Origin>,
        folder: PathBuf,
        settings: &Settings,
    ) -> Result<Next, Stop> {
        // 地址经过重定向时，最后的地址里才有真正的文件名
        let name = naming::file_name(&info.url, info.file_name.as_deref());
        self.rename(flow, task, &name)?;
        Ok(if settings.confirm_before_start {
            Next::ask(Asked {
                offer: Offer::Confirm(Recognized::File(Box::new(info))),
                origin,
                streams: Vec::new(),
            })
        } else {
            Next::Settle(Box::new(file_draft(task, &info, folder, origin.as_ref())))
        })
    }

    /// 交给插件 `plugin` 解析网页：组件没装好时停下等用户同意安装（在装的等它装完），解析出格式后选画质（只有一档、
    /// 又不要先确认时直接定下）。没有解析器认得这个网址、或这个平台没有组件时转到网页里找流（[`Shared::in_page`]）。
    async fn page(
        self: &Arc<Self>,
        flow: &Flow,
        task: &Task,
        folder: PathBuf,
        settings: &Settings,
        plugin: String,
    ) -> Result<Next, Stop> {
        let mut states = self.web.state();
        loop {
            let state = states.borrow_and_update().clone();
            match state {
                WebState::Ready(_) | WebState::UpdateFailed { .. } | WebState::Updating { .. } => {
                    break;
                }
                WebState::Unavailable => {
                    return self
                        .in_page(flow, task, folder, settings, not_media())
                        .await;
                }
                WebState::Missing | WebState::Failed(_) | WebState::Broken(_) => {
                    return Ok(Next::ask(Asked::plain(Offer::Components(state))));
                }
                WebState::Installing(progress) => {
                    self.step(flow, Step::Installing(progress));
                    states
                        .changed()
                        .await
                        .expect("持有 Shared 时状态的发送端还在");
                }
            }
        }
        self.step(flow, Step::Resolving { plugin });
        let resolving = self
            .resolving(task.spec.url.clone(), &task.spec.headers)
            .map_err(|error| Stop::of(&error))?;
        let resolved = match resolving.await {
            Ok(resolved) => resolved,
            Err(ExtractError::Unsupported(_)) => {
                return self
                    .in_page(flow, task, folder, settings, not_media())
                    .await;
            }
            Err(error) => return Err(Stop::Failed(Failure::of_extract(&error))),
        };
        if let Some(entries) = &resolved.info.entries {
            return Err(Stop::failed(
                FailureKind::Unsupported,
                format!(
                    "这是播放列表（{} 条），这一版只下单个视频：打开其中一个视频的网页再粘贴",
                    entries.len()
                ),
            ));
        }
        if matches!(resolved.info.live, Live::Now | Live::Processing) {
            return Err(Stop::failed(
                FailureKind::Unsupported,
                "正在直播或直播刚结束、回放还在处理：这一版的网页视频只下点播，回放处理好后再粘贴",
            ));
        }
        // 没有格式时解析已报错（网站说的原因）；有格式而都下不了的原因见 no_choice_reason
        if resolved.choices.is_empty() {
            return Err(Stop::failed(
                FailureKind::Unsupported,
                format!(
                    "没有能下的格式：{}",
                    no_choice_reason(&resolved.info, resolved.freshness.as_ref())
                ),
            ));
        }
        self.rename(flow, task, &resolved.info.title)?;
        Ok(
            if resolved.choices.len() == 1 && !settings.confirm_before_start {
                Next::Settle(Box::new(web_draft(task, &resolved, 0, folder)))
            } else {
                Next::ask(Asked::plain(Offer::WebQuality(Box::new(resolved))))
            },
        )
    }

    /// 种子：续传数据已存着（种子文件转成的，或上次取到的）时用它，没有时磁力链接去取、取到即存下；有两个以上文件时
    /// 停下选文件，只有一个时直接定下（或先确认）。
    async fn torrent(
        self: &Arc<Self>,
        flow: &Flow,
        task: &Task,
        folder: PathBuf,
        settings: &Settings,
    ) -> Result<Next, Stop> {
        let id = flow.id;
        let stored = self
            .write(|tx| store::pending_torrent(tx, id))
            .map_err(|error| Stop::of(&error))?;
        let resume = match stored {
            Some(resume) => resume,
            None => {
                self.step(flow, Step::Metadata(MetadataProgress::default()));
                let mut fetch = self
                    .fetch_metadata(task.spec.url.to_string())
                    .map_err(|error| Stop::of(&error))?;
                let resume = loop {
                    tokio::select! {
                        fetched = &mut fetch.resume => break fetched,
                        Ok(()) = fetch.progress.changed() => {
                            let progress = fetch.progress.borrow_and_update().clone();
                            self.step(flow, Step::Metadata(progress));
                        }
                    }
                };
                let resume = resume
                    .expect("取种子信息不会 panic，运行时关闭时本任务也一起丢弃")
                    .map_err(|error| Stop::of(&error))?;
                self.write(|tx| store::set_pending_torrent(tx, id, &resume))
                    .map_err(|error| Stop::of(&error))?;
                resume
            }
        };
        // 解析整个续传数据（大种子有几 MB），在阻塞线程上做
        let parsed = resume.clone();
        let info = tokio::task::spawn_blocking(move || TorrentInfo::of(&parsed))
            .await
            .expect("读种子信息不会 panic，运行时关闭时本任务也一起丢弃")
            .map_err(|error| Stop::failed(FailureKind::Malformed, error.to_string()))?;
        self.rename(flow, task, &info.name)?;
        if info.files.len() >= 2 {
            let pikpak = self
                .write(store::pikpak_account)
                .map_err(|error| Stop::of(&error))?
                .is_some();
            return Ok(Next::ask(Asked::plain(Offer::Files {
                info: Box::new(info),
                pikpak,
            })));
        }
        Ok(if settings.confirm_before_start {
            Next::ask(Asked::plain(Offer::Confirm(Recognized::Torrent(Box::new(
                info,
            )))))
        } else {
            let all = info.files.iter().map(|file| file.index).collect();
            Next::Settle(Box::new(torrent_draft(task, resume, all, folder)))
        })
    }

    /// 都定了：照 `draft` 取名、定任务目录，把任务定下并开始（见 [`Shared::settle_and_start`]）。这一轮已被取消时为
    /// [`Stop::Cancelled`]。
    async fn settle(self: &Arc<Self>, flow: &Flow, draft: Draft) -> Result<(), Stop> {
        self.step(flow, Step::Starting);
        let settled = self
            .establish(draft, |spec, work_dir, resume| {
                self.settle_and_start(flow, spec, work_dir, resume)
            })
            .await;
        match settled {
            Ok(_) => Ok(()),
            Err(_) if flow.cancel.is_cancelled() => Err(Stop::Cancelled),
            Err(error) => Err(Stop::of(&error)),
        }
    }

    /// 把这一轮的任务定下为 `spec`（任务目录 `work_dir`、BT 的续传数据 `resume`）并开始：在 `store` 锁内核对这一轮
    /// 没被取消、没被顶掉、服务没关闭（都报 [`Error::Closed`]，调用方按取消处理），在一个事务里定下并开始下载，提交后
    /// 把运行登记换成下载的，发 [`Event::Changed`]。提交不了时任务照旧没定下：已开始的下载取消、它建出的任务目录
    /// 删掉（见 [`Shared::abandon`]）。调用方须在运行时里。
    fn settle_and_start(
        self: &Arc<Self>,
        flow: &Flow,
        spec: &TaskSpec,
        work_dir: Option<PathBuf>,
        resume: Option<Vec<u8>>,
    ) -> Result<Task, Error> {
        let id = flow.id;
        let mut store = lock(&self.store);
        if self.closed.load(Ordering::Relaxed)
            || flow.cancel.is_cancelled()
            || !self.current(id, &flow.guard)
        {
            return Err(Error::Closed);
        }
        let tx = store.transaction()?;
        let applied = self.apply_unsaved(&tx)?;
        store::settle(&tx, id, spec, work_dir.as_deref(), resume.as_deref())?;
        let task = store::get(&tx, id)?.expect("同一事务里刚定下的行读得到");
        let job = self.launch(&tx, &task)?;
        if let Err(error) = self.commit(tx, applied) {
            drop(store);
            self.abandon(job, work_dir);
            return Err(error);
        }
        // 先发定下了，再登记下载：它的进度都在这之后
        self.events.send(Event::Changed {
            task: Box::new(task.clone()),
        });
        self.track(&task, job, Run::Resume);
        // 暂停不经 `store` 锁：换登记之前到的取消的是准备的这一轮，下载照样停下
        if flow.cancel.is_cancelled()
            && let Some(running) = lock(&self.running).get(&id)
        {
            running.control.cancel();
        }
        Ok(task)
    }
}

/// 用户对 `asked` 里的可选项的选择 `answer` 让任务 `task` 接着从哪里走：对不上、选的不在可选项里时报
/// [`Error::InvalidInput`]。选择带来的请求头写进 `tx` 与 `task`。
fn entry_for(
    tx: &Transaction<'_>,
    task: &mut Task,
    asked: &Asked,
    answer: Answer,
) -> Result<Entry, Error> {
    let id = task.id;
    let origin = asked.origin.as_ref();
    let draft = match (&asked.offer, answer) {
        (_, Answer::Headers(headers)) => {
            for (name, value) in &headers {
                check_option_header(name, value).map_err(Error::InvalidInput)?;
            }
            if intake::torrent_source(&task.spec.url) {
                return Err(Error::InvalidInput(format!("{id}没有请求头")));
            }
            store::set_headers(tx, id, &headers)?;
            task.spec.headers = headers;
            return Ok(Entry::Fresh);
        }
        (Offer::Components(_), Answer::Install) => return Ok(Entry::Install),
        (
            Offer::Quality(probe) | Offer::Confirm(Recognized::Hls(probe)),
            Answer::Hls {
                preference,
                output,
                folder,
            },
        ) => {
            absolute(&folder)?;
            if !offered(probe, &preference) {
                return Err(Error::InvalidInput("选的变体或音轨不在可选项里".to_owned()));
            }
            hls_draft(task, probe, preference, output, folder, origin)
        }
        (Offer::Streams(options), Answer::Stream(index)) => {
            let picked = options
                .get(index)
                .zip(asked.streams.get(index))
                .ok_or_else(|| {
                    Error::InvalidInput(format!("没有第 {index} 条流：只有 {} 条", options.len()))
                })?;
            let (
                StreamOption {
                    probed: Ok(probe), ..
                },
                Some(origin),
            ) = picked
            else {
                return Err(Error::InvalidInput(format!(
                    "第 {index} 条流没探测成，不能用"
                )));
            };
            return Ok(Entry::Stream(Box::new(Adopted {
                probe: probe.clone(),
                origin: origin.clone(),
            })));
        }
        (Offer::WebQuality(resolved), Answer::Web { choice, folder }) => {
            absolute(&folder)?;
            if choice >= resolved.choices.len() {
                return Err(Error::InvalidInput(format!(
                    "没有第 {choice} 个画质：只有 {} 个",
                    resolved.choices.len()
                )));
            }
            web_draft(task, resolved, choice, folder)
        }
        (
            Offer::Files { info, .. } | Offer::Confirm(Recognized::Torrent(info)),
            Answer::Files { wanted, folder },
        ) => {
            absolute(&folder)?;
            if wanted.is_empty() {
                return Err(Error::InvalidInput("至少要选一个文件".to_owned()));
            }
            if let Some(index) = wanted
                .iter()
                .find(|index| !info.files.iter().any(|file| file.index == **index))
            {
                return Err(Error::InvalidInput(format!(
                    "种子里没有序号为 {index} 的文件"
                )));
            }
            let resume = store::pending_torrent(tx, id)?
                .ok_or_else(|| Error::Corrupt(format!("{id}在等选文件却没有种子信息")))?;
            torrent_draft(task, resume, wanted, folder)
        }
        (Offer::Confirm(Recognized::File(info)), Answer::File { folder }) => {
            absolute(&folder)?;
            file_draft(task, info, folder, origin)
        }
        _ => {
            return Err(Error::InvalidInput(format!("这个选择与{id}在等的对不上")));
        }
    };
    Ok(Entry::Settle(Box::new(draft)))
}
