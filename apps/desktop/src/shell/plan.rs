//! 停着（暂停或失败）的任务能做什么、先做什么：按失败的种类、是否直播、是否选了只写出已录的决定。放弃总在最后，
//! 不在这里。

use hs_dlx_app::FailureKind;

/// 停着的任务能做的一件事。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    /// 改请求头（编辑卡片里保存，见 [`Plan::resume_after_headers`]）
    EditHeaders,
    /// 不再续录，只把已录的写出来（不联网）
    WriteRecorded,
    /// 改存本地 HLS 再跑（点播续传，直播只写出）
    SwitchToHls,
    /// 网页视频合并改用 MKV 再跑（已下的各路照样用）
    SwitchToMatroska,
    /// 照新建时的请求续传、续录（叫法见 [`crate::tasks::resume_label`]）
    Resume,
    /// 打开保存目录：占着输出路径的东西就在里面
    OpenFolder,
    /// 重新登录网盘（云离线的登录失效了）
    LogIn,
    /// 不用网盘了，把磁力链接交给本机的 BT（云离线的种种失败之后）
    LocalTorrent,
}

/// 内容放不进 MP4 时能改成什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Instead {
    /// HLS 任务：改存本地 HLS
    Hls,
    /// 合并或转封装成 MP4 的网页视频任务：改用 MKV
    Matroska,
    /// 别的任务（保持原样的网页视频是自己的 HLS 引擎写的 MP4，换不了）：没有
    Nothing,
}

/// 停着的任务的操作。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Plan {
    /// 最该先做的排第一，各不相同
    pub actions: Vec<Action>,
    /// 改请求头保存后接着续传续录。只保存的：只写出已录的（不联网），失败与请求头无关的（见
    /// [`FailureKind::headers_can_change_outcome`]）
    pub resume_after_headers: bool,
}

/// 停着的任务的操作。`failure` 为失败的种类（暂停的为 None），`live` 为直播录制，`write_only` 为选了只写出已录的
/// （[`hs_dlx_app::Task::write_only`]），`headers` 为任务有请求头（BT 任务没有），`editing` 为正在改请求头（那时编辑
/// 卡片里有保存的按钮，这里不再给改请求头），`instead` 为放不进 MP4 时能改成什么。
///
/// - 再跑一次：选了只写出的写出，其余续传续录；没有可写出的改为续录。
/// - 被拒或取到错误页的，先改请求头；放不进 MP4 的，先改存本地 HLS（网页视频改用 MKV），此前再跑只会同样失败。
/// - 直播接不上之前录的、来源已没有（404/410）或停滞的，先写出已录的：续录多半不成。
/// - 再试也一样的不给再跑；改了请求头可能不一样的（[`FailureKind::headers_can_change_outcome`]），保存后接着跑。
/// - 直播停着时都能写出已录的，选了只写出的也能改回续录；有请求头的停着的任务都能改请求头。
/// - 云离线登录失效的先重新登录；网盘那边的失败（登录、额度、云端下载、接口变了）都能改用本机 BT。
pub(super) fn stopped_plan(
    failure: Option<FailureKind>,
    live: bool,
    write_only: bool,
    headers: bool,
    editing: bool,
    instead: Instead,
) -> Plan {
    let offer_headers = headers && !editing;
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
    if headers_first && offer_headers {
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
    if offer_headers {
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
    use super::Action::*;
    use super::*;

    fn actions(failure: Option<FailureKind>, live: bool, write_only: bool) -> Vec<Action> {
        stopped_plan(failure, live, write_only, true, false, Instead::Hls).actions
    }

    #[test]
    fn the_first_action_fits_the_failure() {
        // 暂停的点播：继续，旁边能改请求头
        assert_eq!(actions(None, false, false), [Resume, EditHeaders]);
        // 403：先改请求头，保存后重试；正在改时不再给
        let forbidden = Some(FailureKind::Status(403));
        assert_eq!(actions(forbidden, false, false), [EditHeaders]);
        let editing = stopped_plan(forbidden, false, false, true, true, Instead::Hls);
        assert_eq!(editing.actions, []);
        assert!(editing.resume_after_headers);
        // 放不进 MP4：先改存本地 HLS，此前写出、重试都会同样失败
        let mp4 = stopped_plan(
            Some(FailureKind::Mp4Unsupported),
            true,
            false,
            true,
            false,
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
                false,
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
            false,
            Instead::Hls,
        );
        assert_eq!(stuck.actions, [EditHeaders]);
        assert!(!stuck.resume_after_headers);
        // 没有请求头的（BT）：只有继续，写盘出错时也是
        let torrent =
            |failure| stopped_plan(failure, false, false, false, false, Instead::Nothing).actions;
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
        let disk = stopped_plan(
            Some(FailureKind::Disk),
            true,
            true,
            true,
            false,
            Instead::Hls,
        );
        assert_eq!(disk.actions, [WriteRecorded, Resume, EditHeaders]);
        assert!(!disk.resume_after_headers);
        // 证书不受信任：改请求头也没用，只保存
        let certificate = stopped_plan(
            Some(FailureKind::Certificate),
            false,
            false,
            true,
            false,
            Instead::Hls,
        );
        assert!(!certificate.resume_after_headers);
        // 没有可写出的：改为续录，不再给写出
        let nothing = stopped_plan(
            Some(FailureKind::NothingToWrite),
            true,
            true,
            true,
            false,
            Instead::Hls,
        );
        assert_eq!(nothing.actions, [Resume, EditHeaders]);
        assert!(nothing.resume_after_headers);
    }
}
