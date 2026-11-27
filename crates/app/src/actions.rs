//! 任务此刻能做什么、先做什么：按状态、失败的种类、是否直播、是否选了只写出已录的决定。界面、托盘与控制协议都照它
//! 给出操作（[`crate::Handle::actions`]）。

use crate::web::Container;
use crate::{FailureKind, Kind, OutputSpec, Status, Task, intake};

/// 对任务能做的一件事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 粘贴一条 curl 命令换掉请求头并重试：被拒、取到错误页的，多半缺浏览器里那次请求带的头
    PasteCurl,
    /// 手动改请求头（保存后要不要接着跑见 [`Plan::resume_after_headers`]）。停着的任务经
    /// [`crate::Handle::set_headers`]，要接着跑的再 [`crate::Handle::resume`]；等用户而手里有可选项的任务
    /// （[`Situation::offered`]，含停在网页上的）经 [`crate::Handle::choose`] 的 [`crate::Answer::Headers`]，换了即从头
    /// 准备
    EditHeaders,
    /// 不再续录，只把已录的写出来（不联网）
    WriteRecorded,
    /// 改存本地 HLS 再跑（点播续传，直播只写出）
    SwitchToHls,
    /// 网页视频合并改用 MKV 再跑（已下的各路照样用）
    SwitchToMatroska,
    /// 再跑一次：暂停的继续，失败的重试，选了只写出的改回续录；还没定下的从头准备
    Resume,
    /// 打开保存目录：占着输出路径的东西就在里面
    OpenFolder,
    /// 重新登录网盘（云离线的登录失效了）
    LogIn,
    /// 不用网盘了，把磁力链接交给本机的 BT（云离线的种种失败之后）：用同一条磁力另建一个任务
    LocalTorrent,
    /// 暂停
    Pause,
    /// 停止录制并写出已录的
    Stop,
    /// 放弃：连同已下的内容一起删；还没定下的任务没有本机数据，直接删记录
    Discard,
    /// 从列表移除，输出不动
    Forget,
    /// 让下完的 BT 任务做种
    Seed,
    /// 停止做种
    StopSeeding,
    /// 播放写出的视频
    Play,
    /// 在文件管理器里显示输出
    Reveal,
}

/// 任务此刻的操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// 最该先做的排第一，各不相同；任务正在放弃时为空
    pub actions: Vec<Action>,
    /// 改请求头保存后接着再跑。只保存的：只写出已录的（不联网），失败与请求头无关的（见
    /// [`FailureKind::headers_can_change_outcome`]），不是停着的任务
    pub resume_after_headers: bool,
}

/// 任务在存储之外的情况，操作要按它定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Situation {
    /// 是直播的录制：在运行的按此刻的阶段，停着的按任务目录里的内容
    pub recording: bool,
    /// 停下等用户的任务手里有可选项（或网页开着）；没有时要先再跑一次、重新准备
    pub offered: bool,
}

/// 任务 `task` 此刻的操作。
///
/// - 准备中：暂停、放弃。在下载：暂停、放弃；在录的直播先是停止并写出。
/// - 等用户：选择本身不在这里（经 [`crate::Handle::choose`]）；能加请求头、放弃。可选项不在了（重开应用后）先再跑
///   一次。
/// - 暂停或失败：
///   - 再跑一次：选了只写出的写出，其余续传续录；没有可写出的改为续录。
///   - 被拒或取到错误页的，先粘贴 curl 或改请求头；放不进 MP4 的，先改存本地 HLS（网页视频改用 MKV），此前再跑只会
///     同样失败。
///   - 直播接不上之前录的、来源已没有（404/410）或停滞的，先写出已录的：续录多半不成。
///   - 再试也一样的不给再跑；改了请求头可能不一样的（[`FailureKind::headers_can_change_outcome`]），保存后接着跑。
///   - 直播停着时都能写出已录的，选了只写出的也能改回续录；有请求头的任务都能改请求头。
///   - 云离线登录失效的先重新登录；网盘那边的失败（登录、额度、云端下载、接口变了）都能改用本机 BT。
///   - 没有自己的任务目录的（见 [`Task::work_dir`]）只能从列表移除；其余最后都是放弃。
/// - 已完成：播放（有视频的）、在文件管理器里显示、从列表移除；BT 另有做种或停止做种。
/// - 放弃没做完：再放弃。
pub fn plan(task: &Task, situation: Situation) -> Plan {
    let only = |actions: &[Action]| Plan {
        actions: actions.to_vec(),
        resume_after_headers: false,
    };
    let headers = has_headers(task);
    match &task.status {
        Status::Preparing => only(&[Action::Pause, Action::Discard]),
        Status::Waiting(_) => {
            let mut actions = Vec::new();
            if !situation.offered {
                actions.push(Action::Resume);
            }
            if headers {
                actions.push(Action::EditHeaders);
            }
            actions.push(Action::Discard);
            Plan {
                actions,
                // 换请求头即从头准备
                resume_after_headers: true,
            }
        }
        Status::Running if situation.recording => {
            only(&[Action::Stop, Action::Pause, Action::Discard])
        }
        Status::Running => only(&[Action::Pause, Action::Discard]),
        Status::Paused | Status::Failed(_) if unresumable(task) => only(&[Action::Forget]),
        Status::Paused => stopped(task, None, situation.recording, headers),
        Status::Failed(failure) => stopped(task, Some(failure.kind), situation.recording, headers),
        Status::Done => {
            let mut actions = Vec::new();
            if task.seeding {
                actions.push(Action::StopSeeding);
            }
            if playable(task) {
                actions.push(Action::Play);
            }
            actions.push(Action::Reveal);
            if task.spec.torrent().is_some() && !task.seeding {
                actions.push(Action::Seed);
            }
            actions.push(Action::Forget);
            only(&actions)
        }
        Status::DiscardUnfinished => only(&[Action::Discard]),
    }
}

/// 任务有请求头可改：BT、云离线与来源是磁力链接或种子文件的没有。
fn has_headers(task: &Task) -> bool {
    task.spec.torrent().is_none()
        && task.spec.cloud().is_none()
        && !intake::torrent_source(&task.spec.url)
}

/// 定下了的任务没有自己的任务目录（见 [`Task::work_dir`]；BT 本就没有，云离线列出文件之前也没有，不算）：不能继续，
/// 只能从列表移除。
fn unresumable(task: &Task) -> bool {
    match task.spec.kind {
        Kind::Hls(_) | Kind::File(_) | Kind::Web(_) => task.work_dir.is_none(),
        Kind::Pending(_) | Kind::Torrent(_) | Kind::Cloud(_) => false,
    }
}

/// 输出里有能播放的视频：HLS 任务写出的 MP4、网页视频。
fn playable(task: &Task) -> bool {
    match &task.spec.kind {
        Kind::Hls(hls) => matches!(hls.output, OutputSpec::Mp4(_) | OutputSpec::Both { .. }),
        Kind::Web(_) => true,
        Kind::Pending(_) | Kind::File(_) | Kind::Torrent(_) | Kind::Cloud(_) => false,
    }
}

/// 内容放不进 MP4 时能改成什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Instead {
    /// HLS 任务：改存本地 HLS
    Hls,
    /// 合并或转封装成 MP4 的网页视频任务：改用 MKV
    Matroska,
    /// 别的任务（保持原样的网页视频是自己的 HLS 引擎写的 MP4，换不了）：没有
    Nothing,
}

/// 暂停（`failure` 为 None）或失败的任务 `task` 的操作；`live` 为直播录制，`headers` 为任务有请求头。
fn stopped(task: &Task, failure: Option<FailureKind>, live: bool, headers: bool) -> Plan {
    let instead = match &task.spec.kind {
        Kind::Hls(_) => Instead::Hls,
        Kind::Web(web) if web.container == Container::Mp4 => Instead::Matroska,
        Kind::Pending(_) | Kind::File(_) | Kind::Torrent(_) | Kind::Cloud(_) | Kind::Web(_) => {
            Instead::Nothing
        }
    };
    let mut plan = stopped_plan(failure, live, task.write_only, headers, instead);
    plan.actions.push(Action::Discard);
    plan
}

fn stopped_plan(
    failure: Option<FailureKind>,
    live: bool,
    write_only: bool,
    headers: bool,
    instead: Instead,
) -> Plan {
    let mp4 = failure == Some(FailureKind::Mp4Unsupported);
    let rerun = if write_only && failure != Some(FailureKind::NothingToWrite) {
        Action::WriteRecorded
    } else {
        Action::Resume
    };
    let headers_first = failure.is_some_and(FailureKind::headers_likely_missing);
    let live_over = live
        && matches!(
            failure,
            Some(
                FailureKind::SourceChanged | FailureKind::Status(404 | 410) | FailureKind::Stalled
            )
        );
    let mut actions = Vec::new();
    let mut add = |action| {
        if !actions.contains(&action) {
            actions.push(action);
        }
    };
    if headers_first && headers {
        add(Action::PasteCurl);
        add(Action::EditHeaders);
    }
    if failure == Some(FailureKind::CloudLogin) {
        add(Action::LogIn);
    }
    if mp4 {
        match instead {
            Instead::Hls => add(Action::SwitchToHls),
            Instead::Matroska => add(Action::SwitchToMatroska),
            Instead::Nothing => {}
        }
    }
    if live_over {
        add(Action::WriteRecorded);
    }
    if !mp4
        && (failure.is_none_or(FailureKind::retry_helps)
            || failure == Some(FailureKind::NothingToWrite))
    {
        add(rerun);
    }
    if live && !mp4 && failure != Some(FailureKind::NothingToWrite) {
        add(Action::WriteRecorded);
        if write_only {
            add(Action::Resume);
        }
    }
    if headers {
        add(Action::EditHeaders);
    }
    if failure == Some(FailureKind::OutputTaken) {
        add(Action::OpenFolder);
    }
    if matches!(
        failure,
        Some(
            FailureKind::CloudLogin
                | FailureKind::CloudQuota
                | FailureKind::CloudFailed
                | FailureKind::CloudApi
        )
    ) {
        add(Action::LocalTorrent);
    }
    Plan {
        actions,
        resume_after_headers: failure.is_none_or(FailureKind::headers_can_change_outcome)
            && rerun == Action::Resume,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use hs_m3u8_core::hls::Preference;

    use super::Action::*;
    use super::*;
    use crate::{
        Ask, CloudProvider, CloudSpec, Failure, HlsSpec, PendingSpec, StoredPreference, TaskId,
        TaskSpec, TorrentSpec,
    };

    fn actions(failure: Option<FailureKind>, live: bool, write_only: bool) -> Vec<Action> {
        stopped_plan(failure, live, write_only, true, Instead::Hls).actions
    }

    #[test]
    fn the_first_action_fits_the_failure() {
        // 暂停的点播：继续，旁边能改请求头
        assert_eq!(actions(None, false, false), [Resume, EditHeaders]);
        // 403：先粘贴 curl，其次手动改请求头，保存后重试
        let forbidden = stopped_plan(
            Some(FailureKind::Status(403)),
            false,
            false,
            true,
            Instead::Hls,
        );
        assert_eq!(forbidden.actions, [PasteCurl, EditHeaders]);
        assert!(forbidden.resume_after_headers);
        // 放不进 MP4：先改存本地 HLS，此前写出、重试都会同样失败
        let mp4 = stopped_plan(
            Some(FailureKind::Mp4Unsupported),
            true,
            false,
            true,
            Instead::Hls,
        );
        assert_eq!(mp4.actions, [SwitchToHls, EditHeaders]);
        assert!(!mp4.resume_after_headers);
        // 网页视频合并放不进 MP4：改用 MKV；只有一路的没有可改的
        let web = |instead| {
            stopped_plan(
                Some(FailureKind::Mp4Unsupported),
                false,
                false,
                true,
                instead,
            )
            .actions
        };
        assert_eq!(web(Instead::Matroska), [SwitchToMatroska, EditHeaders]);
        assert_eq!(web(Instead::Nothing), [EditHeaders]);
        // 直播来源没了：先写出已录的，不给重试
        assert_eq!(
            actions(Some(FailureKind::Status(404)), true, false),
            [WriteRecorded, EditHeaders]
        );
        // 输出路径被占：重试，并能打开保存目录
        assert_eq!(
            actions(Some(FailureKind::OutputTaken), false, false),
            [Resume, EditHeaders, OpenFolder]
        );
        // 再试也一样的点播：只剩改请求头（放弃另给），改了请求头也只保存
        let stuck = stopped_plan(
            Some(FailureKind::Unrecoverable),
            false,
            false,
            true,
            Instead::Hls,
        );
        assert_eq!(stuck.actions, [EditHeaders]);
        assert!(!stuck.resume_after_headers);
        // 没有请求头的（BT）：只有继续，写盘出错时也是
        let torrent =
            |failure| stopped_plan(failure, false, false, false, Instead::Nothing).actions;
        assert_eq!(torrent(None), [Resume]);
        assert_eq!(torrent(Some(FailureKind::Disk)), [Resume]);
        // 云离线（也没有请求头）：登录失效先登录；网盘那边的失败能改用本机 BT；接口变了不给重试；校验不符只重试
        let cloud = torrent;
        assert_eq!(
            cloud(Some(FailureKind::CloudLogin)),
            [LogIn, Resume, LocalTorrent]
        );
        assert_eq!(cloud(Some(FailureKind::CloudQuota)), [Resume, LocalTorrent]);
        assert_eq!(cloud(Some(FailureKind::CloudApi)), [LocalTorrent]);
        assert_eq!(cloud(Some(FailureKind::Integrity)), [Resume]);
    }

    #[test]
    fn writing_only_reruns_as_writing_until_there_is_nothing_to_write() {
        // 只写出时盘满：腾出空间后再跑也只写出，也能改回续录；改了请求头只保存
        let disk = stopped_plan(Some(FailureKind::Disk), true, true, true, Instead::Hls);
        assert_eq!(disk.actions, [WriteRecorded, Resume, EditHeaders]);
        assert!(!disk.resume_after_headers);
        // 证书不受信任：改请求头也没用，只保存
        let certificate = stopped_plan(
            Some(FailureKind::Certificate),
            false,
            false,
            true,
            Instead::Hls,
        );
        assert!(!certificate.resume_after_headers);
        // 没有可写出的：改为续录，不再给写出
        let nothing = stopped_plan(
            Some(FailureKind::NothingToWrite),
            true,
            true,
            true,
            Instead::Hls,
        );
        assert_eq!(nothing.actions, [Resume, EditHeaders]);
        assert!(nothing.resume_after_headers);
    }

    fn task(kind: Kind, url: &str, status: Status) -> Task {
        let settled = !matches!(kind, Kind::Pending(_));
        Task {
            id: TaskId(1),
            spec: TaskSpec {
                title: "talk".into(),
                url: url.parse().unwrap(),
                headers: Vec::new(),
                kind,
            },
            status,
            work_dir: settled.then(|| PathBuf::from("/m/talk.hsdl")),
            created_at: 0,
            total: None,
            result: None,
            write_only: false,
            seeding: false,
            finished_at: None,
            active: Duration::ZERO,
        }
    }

    fn hls(status: Status) -> Task {
        let kind = Kind::Hls(HlsSpec {
            preference: StoredPreference::try_from(&Preference::default()).unwrap(),
            output: OutputSpec::Mp4("/m/talk.mp4".into()),
            live: true,
        });
        task(kind, "https://a.example/talk.m3u8", status)
    }

    fn pending(url: &str, status: Status) -> Task {
        let kind = Kind::Pending(PendingSpec {
            folder: "/m".into(),
        });
        task(kind, url, status)
    }

    fn of(task: &Task, recording: bool, offered: bool) -> Vec<Action> {
        plan(task, Situation { recording, offered }).actions
    }

    #[test]
    fn every_status_has_its_actions() {
        // 在录的直播先是停止并写出；点播只能暂停
        assert_eq!(
            of(&hls(Status::Running), true, false),
            [Stop, Pause, Discard]
        );
        assert_eq!(of(&hls(Status::Running), false, false), [Pause, Discard]);
        // 停着的最后都是放弃
        assert_eq!(
            of(&hls(Status::Paused), false, false),
            [Resume, EditHeaders, Discard]
        );
        // 没有自己的任务目录的只能移除
        let mut bare = hls(Status::Paused);
        bare.work_dir = None;
        assert_eq!(of(&bare, false, false), [Forget]);
        // 完成：有视频的先播放
        assert_eq!(of(&hls(Status::Done), false, false), [Play, Reveal, Forget]);
        assert_eq!(of(&hls(Status::DiscardUnfinished), false, false), [Discard]);
        // BT 下完：做种或停止做种
        let torrent = |seeding| {
            let kind = Kind::Torrent(TorrentSpec {
                info_hash: "0123456789abcdef0123456789abcdef01234567".into(),
                root: "/m/iso".into(),
            });
            let mut task = task(kind, "magnet:?xt=urn:btih:0123", Status::Done);
            task.work_dir = None;
            task.seeding = seeding;
            task
        };
        assert_eq!(of(&torrent(false), false, false), [Reveal, Seed, Forget]);
        assert_eq!(
            of(&torrent(true), false, false),
            [StopSeeding, Reveal, Forget]
        );
        // 云离线还没定下内容的根时没有任务目录，照样能继续
        let cloud = Kind::Cloud(CloudSpec {
            provider: CloudProvider::PikPak,
            user: "u".into(),
            job: None,
            before: None,
            folder: "/m".into(),
            root: None,
        });
        let mut cloud = task(cloud, "magnet:?xt=urn:btih:0123", Status::Paused);
        cloud.work_dir = None;
        assert_eq!(of(&cloud, false, false), [Resume, Discard]);
    }

    #[test]
    fn unsettled_tasks_resume_from_scratch_or_wait_for_a_choice() {
        let page = "https://a.example/watch";
        assert_eq!(
            of(&pending(page, Status::Preparing), false, false),
            [Pause, Discard]
        );
        // 等用户：有可选项时选择不在这里；可选项不在了先再跑一次
        let waiting = pending(page, Status::Waiting(Ask::Quality));
        assert_eq!(of(&waiting, false, true), [EditHeaders, Discard]);
        assert_eq!(of(&waiting, false, false), [Resume, EditHeaders, Discard]);
        // 来源是磁力链接的没有请求头
        let magnet = pending("magnet:?xt=urn:btih:0123", Status::Waiting(Ask::Files));
        assert_eq!(of(&magnet, false, true), [Discard]);
        // 失败的照种类：被拒的先粘贴 curl，不支持的只能放弃
        let failed = |kind| {
            pending(
                page,
                Status::Failed(Failure {
                    kind,
                    message: String::new(),
                }),
            )
        };
        assert_eq!(
            of(&failed(FailureKind::Status(403)), false, false),
            [PasteCurl, EditHeaders, Discard]
        );
        assert_eq!(
            of(&failed(FailureKind::Unsupported), false, false),
            [EditHeaders, Discard]
        );
        assert_eq!(
            of(&failed(FailureKind::Network), false, false),
            [Resume, EditHeaders, Discard]
        );
    }
}
