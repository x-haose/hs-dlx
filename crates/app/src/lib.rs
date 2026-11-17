//! 桌面应用的业务层：任务与设置的存储、任务的生命周期与节流后的进度，建在 hs-m3u8-core（HLS 与单个文件）与
//! hs-dlx-torrent（BT）之上，不依赖界面框架。

pub mod curl;
mod diagnosis;
mod error;
mod failure;
mod naming;
mod netcheck;
mod preference;
mod public_trackers;
mod service;
mod settings;
mod socks;
pub mod special_link;
mod store;
mod task;
mod torrent;
mod update;
pub mod web;

pub use diagnosis::{Check, CheckItem, CheckLevel};
pub use error::Error;
pub use failure::{Failure, FailureKind, not_a_playlist};
/// PikPak 的接口（登录、进度阶段、接口地址等界面要用的类型）。
pub use hs_dlx_cloud::pikpak;
pub use hs_dlx_ytdlp as ytdlp;
pub use naming::{file_name, task_title, title_of};
pub use netcheck::{NetCheck, NetItem, NetworkReport};
pub use preference::StoredPreference;
pub use service::{
    CloudPhase, CloudProgress, Downloaded, Event, Events, MetadataFetch, MetadataHandle,
    PikPakLogin, Resolved, Service, UpdateDownload, WebConfig, WebProgress, WebState,
};
pub use settings::{Seeding, Settings, Theme};
pub use store::Store;
pub use task::{
    CloudFile, CloudProvider, CloudSpec, Draft, DraftKind, Expected, FileSpec, Finished, HlsSpec,
    Kind, OutputKind, OutputSpec, Status, Task, TaskId, TaskSpec, TorrentSpec, WebSpec,
};
pub use torrent::{
    MetadataProgress, SavedTorrent, SeedEnd, TorrentFile, TorrentInfo, TorrentNetwork,
    TorrentPhase, TorrentProgress, torrent_from_file,
};
pub use update::{
    INSTALLED_BEFORE_RESTART, Package, Release, Restart, UpdateFeed, Version,
    record_failure as record_update_failure, sweep as sweep_update,
    take_failure as take_update_failure,
};
