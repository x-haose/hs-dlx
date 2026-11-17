//! 业务层的错误。

use crate::TaskId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 调用方给的值不合要求（如没法存的选轨偏好、不是 UTF-8 的输出路径）
    #[error("参数不合要求：{0}")]
    InvalidInput(String),
    /// 任务存储读写失败（盘满、文件被删、权限）；照原样报出，不可重试与否看包着的原因
    #[error("任务存储出错：{0}")]
    Store(#[from] rusqlite::Error),
    /// 任务存储里的内容认不出：本应用写入的都能读回，读不回即文件被改动过或被别的程序写过
    #[error("任务存储里的内容认不出：{0}")]
    Corrupt(String),
    #[error("没有{0}")]
    NotFound(TaskId),
    /// 任务正在运行，这个操作要等它停下
    #[error("{0}正在运行")]
    Running(TaskId),
    /// 任务正在放弃（停下、删任务目录），不能再继续或移除
    #[error("{0}正在放弃")]
    Discarding(TaskId),
    /// 只从列表移除（不删任务目录）只用于已完成或没有自己的任务目录的任务
    #[error("{0}没有完成，要连同任务目录一起放弃")]
    NotFinished(TaskId),
    /// 已完成的任务不能再继续
    #[error("{0}已完成")]
    AlreadyDone(TaskId),
    /// 只有 HLS 任务有的操作（只写出已录的、改存本地 HLS）用在了别的任务上
    #[error("{0}不是 HLS 任务")]
    NotHls(TaskId),
    /// 任务没有自己的任务目录（见 [`crate::Task::work_dir`]），不能继续
    #[error("{0}没有自己的任务目录，只能从列表移除")]
    NoWorkDir(TaskId),
    /// 放弃时任务目录没删干净（里面有不是本库写的文件，或删除出错）：任务留在存储里，处理后可再放弃
    #[error("{id}的任务目录没删干净，任务留在列表里：{}", join(.leftovers))]
    WorkDirLeft {
        id: TaskId,
        leftovers: Vec<hs_m3u8_core::Leftover>,
    },
    /// 磁盘操作超时（如网络盘没有响应）：`action` 为在做什么；可重试
    #[error("{action}超过 {} 秒没有结果", .after.as_secs())]
    TimedOut {
        action: String,
        after: std::time::Duration,
    },
    /// 放弃时任务目录删了、存储里的记录没删掉：`leftovers` 为核心报出的残留（之后不再报），`cause` 为删记录时的错误；
    /// 再放弃一次即删掉记录
    #[error("{id}的任务目录已删，存储里的记录没删掉（{cause}），再放弃一次即可{}", left(.leftovers))]
    RecordNotDeleted {
        id: TaskId,
        leftovers: Vec<hs_m3u8_core::Leftover>,
        cause: Box<Error>,
    },
    /// 服务已关闭（[`crate::Service::close`]），不再开始任务
    #[error("正在退出，不再开始任务")]
    Closed,
    /// 新任务的输出或任务目录与存储里已有任务的输出或任务目录相同
    #[error("路径已被另一个任务使用：{}", .0.display())]
    PathTaken(std::path::PathBuf),
    /// 放弃没做完（中途出错、超时或应用退出），任务目录可能已删了一部分：不能继续，只能再放弃
    #[error("{0}的放弃没做完，只能再放弃")]
    DiscardUnfinished(TaskId),
    /// 任务存储已被另一个运行中的应用打开；两个应用同时改同一份任务会互相覆盖状态
    #[error("任务存储已被另一个运行中的 hs-dlx 打开：{}", .0.display())]
    InUse(std::path::PathBuf),
    /// 核心库报的错误
    #[error(transparent)]
    Core(#[from] hs_m3u8_core::Error),
    /// BT 引擎（libtorrent）报的错误，或等它一步的结果超时；说明里 libtorrent 的部分为英文
    #[error("BT：{0}")]
    Torrent(String),
    /// 应用更新没成（查不到、下载不了、签名不符、装不上）：说明里写明哪一步与原因；已装的版本不受影响
    #[error("更新：{0}")]
    Update(String),
    /// 应用更新换到一半，新旧两份都没在原来的位置：说明里写明各在哪、怎么处理；不能再试
    #[error("更新没换完：{0}")]
    UpdateStranded(String),
    /// BT 任务在做种：要先停止做种（[`crate::Service::stop_seeding`]）
    #[error("{0}正在做种，先停止做种")]
    Seeding(TaskId),
    /// 新建的 BT 任务的种子已在这个任务里：两个任务会写同一批文件
    #[error("这个种子已在{0}里")]
    TorrentTaken(TaskId),
    /// 放弃 BT 任务时下了的文件没删干净（里面有不是这个种子的文件，或删除出错）：任务留在存储里，处理后可再放弃
    #[error("{id}下了的文件没删干净，任务留在列表里：{cause}")]
    ContentLeft { id: TaskId, cause: Box<Error> },
    /// 用云离线前要先登录网盘
    #[error("没登录 PikPak")]
    NotLoggedIn,
    /// 这个平台没有网页视频（yt-dlp）的组件
    #[error("这个平台上用不了网页视频的解析")]
    WebUnavailable,
    /// 网页视频的组件还没装好
    #[error("还没装网页视频的解析组件")]
    WebNotReady,
    /// 有网页视频任务在跑，重装组件要先暂停它们（在用的组件换不下）
    #[error("有网页视频任务在跑：先暂停它们再重装")]
    WebInUse,
    /// 调网盘的接口失败（提交、登录等不在任务里做的）
    #[error(transparent)]
    PikPak(#[from] hs_dlx_cloud::pikpak::Error),
    /// 文件系统或系统资源的操作失败：建存储所在的目录、设权限、给存储加锁、取名时检查路径是否已存在、启动 tokio
    /// 运行时；照原样报出
    #[error("{action} {path} 失败：{cause}")]
    Io {
        action: &'static str,
        path: std::path::PathBuf,
        cause: std::io::Error,
    },
}

impl Error {
    /// 写存储时违反了表的约束（CHECK、NOT NULL 等）：本应用写的值与表结构不一致，不是外部故障。
    pub(crate) fn violates_constraint(&self) -> bool {
        matches!(
            self,
            Error::Store(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation
        )
    }
}

fn join(leftovers: &[hs_m3u8_core::Leftover]) -> String {
    leftovers
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("；")
}

/// 残留的说明接在句末；没有时为空。
fn left(leftovers: &[hs_m3u8_core::Leftover]) -> String {
    if leftovers.is_empty() {
        String::new()
    } else {
        format!("；没删掉的：{}", join(leftovers))
    }
}
