//! 任务存储：一个 SQLite 文件。表结构的版本记在 `PRAGMA user_version`，打开时按版本迁移；读写函数接受调用方
//! 开好的事务，事务边界由服务层定。

use std::collections::HashSet;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use hs_m3u8_core::{Proxy, Url};

use crate::preference::StoredPreference;
use crate::torrent::{Resume, SessionState};
use crate::web::Container;
use crate::{
    CloudFile, CloudProvider, CloudSpec, Error, Expected, Failure, FailureKind, FileSpec, Finished,
    HlsSpec, Kind, OutputSpec, Seeding, Settings, Status, Task, TaskId, TaskSpec, Theme,
    TorrentSpec, WebSpec, naming,
};
use hs_dlx_cloud::pikpak::Account;
use hs_dlx_ytdlp::{FormatKey, Track};
use std::num::NonZeroU32;

/// 一次迁移：在给定的事务里把表结构升一个版本。
type Migration = fn(&Transaction<'_>) -> Result<(), Error>;

/// 各版本的迁移：第 i 项把版本 i 升到 i + 1。已发布的迁移不再改动，新的表结构只追加一项。
const MIGRATIONS: [Migration; 12] = [
    create_task_table,
    rebuild_with_work_dir_and_result,
    create_settings_table,
    rebuild_with_kinds,
    add_theme,
    rebuild_with_ranges,
    rebuild_with_torrents,
    rebuild_with_seeding,
    add_check_updates,
    add_torrent_proxy,
    rebuild_with_cloud,
    rebuild_with_web,
];

fn create_task_table(tx: &Transaction<'_>) -> Result<(), Error> {
    tx.execute_batch(
        "CREATE TABLE task (
            id INTEGER PRIMARY KEY,
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            preference TEXT NOT NULL,
            headers TEXT NOT NULL,
            output_kind TEXT NOT NULL CHECK (output_kind IN ('mp4', 'hls', 'both')),
            mp4_path TEXT,
            hls_path TEXT,
            live INTEGER NOT NULL CHECK (live IN (0, 1)),
            status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done')),
            error TEXT,
            created_at INTEGER NOT NULL
        ) STRICT;",
    )?;
    Ok(())
}

/// 第 2 版，重建表：
/// - 编号不再重用（AUTOINCREMENT）：界面按编号认领异步回来的结果，删掉的任务的编号给了新任务就会认错。
/// - 任务目录单列（绝对路径；NULL 时任务没有自己的任务目录）。第 1 版的行按第 1 版的规则补算：没完成的取与输出同级的
///   `<主名>.hsdl`，已完成的为 NULL（任务目录已在完成时删掉）。补算出的任务目录相同（按 [`v2_path_key`] 比较）的一组
///   里，编号最小的留着它，其余的改为失败、任务目录为 NULL：放弃它们会删掉留着的那个已下的内容。
/// - 失败的种类（[`crate::FailureKind`] 在存储里的写法，CHECK 列出全部写法），只在失败时有；第 1 版失败的行记为
///   其余。
/// - 点播的总片数，与完成时写出的结果（时长、字节数、视频的宽高）。
/// - 继续时只写出已录的（[`Task::write_only`]），第 1 版的行为否。
fn rebuild_with_work_dir_and_result(tx: &Transaction<'_>) -> Result<(), Error> {
    tx.execute_batch(
        "CREATE TABLE task_v2 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            preference TEXT NOT NULL,
            headers TEXT NOT NULL,
            output_kind TEXT NOT NULL CHECK (output_kind IN ('mp4', 'hls', 'both')),
            mp4_path TEXT,
            hls_path TEXT,
            work_dir TEXT,
            live INTEGER NOT NULL CHECK (live IN (0, 1)),
            status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done', 'discard-unfinished')),
            error TEXT,
            failure TEXT CHECK (
                (status = 'failed') = (failure IS NOT NULL)
                AND (failure IS NULL
                     OR failure IN ('network', 'certificate', 'disk', 'output-taken', 'source-changed', 'malformed',
                                    'stalled', 'nothing-to-write', 'mp4-unsupported', 'unsupported', 'unrecoverable',
                                    'other')
                     OR failure GLOB 'status:[0-9][0-9][0-9]')
            ),
            created_at INTEGER NOT NULL,
            total INTEGER CHECK (total >= 0),
            duration_ms INTEGER CHECK (duration_ms >= 0),
            bytes INTEGER CHECK (bytes >= 0),
            width INTEGER CHECK (width > 0),
            height INTEGER CHECK (height > 0),
            write_only INTEGER NOT NULL DEFAULT 0 CHECK (write_only IN (0, 1))
        ) STRICT;
        INSERT INTO task_v2 (id, title, url, preference, headers, output_kind, mp4_path, hls_path, live, status,
                             error, failure, created_at)
            SELECT id, title, url, preference, headers, output_kind, mp4_path, hls_path, live, status, error,
                   CASE WHEN status = 'failed' THEN 'other' END, created_at
            FROM task;
        DROP TABLE task;
        ALTER TABLE task_v2 RENAME TO task;",
    )?;
    /// 第 1 版的一行里迁移要用的列。
    struct V1Row {
        id: i64,
        title: String,
        status: String,
        kind: String,
        mp4: Option<String>,
        hls: Option<String>,
    }
    let rows: Vec<V1Row> = tx
        .prepare("SELECT id, title, status, output_kind, mp4_path, hls_path FROM task ORDER BY id")?
        .query_map([], |row| {
            Ok(V1Row {
                id: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                kind: row.get(3)?,
                mp4: row.get(4)?,
                hls: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // 任务目录的比较键 → 留着它的任务的标题
    let mut owners: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for row in rows {
        let work_dir = match (row.kind.as_str(), row.mp4, row.hls) {
            ("mp4" | "both", Some(mp4), _) => v1_work_dir(&mp4, true),
            ("hls", None, Some(hls)) => v1_work_dir(&hls, false),
            _ => None,
        }
        .ok_or_else(|| Error::Corrupt(format!("任务 {} 的输出", row.id)))?;
        if row.status == "done" {
            continue;
        }
        match owners.entry(v2_path_key(&work_dir)) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(row.title);
                tx.execute(
                    "UPDATE task SET work_dir = ?2 WHERE id = ?1",
                    params![row.id, work_dir],
                )?;
            }
            std::collections::hash_map::Entry::Occupied(owner) => {
                tx.execute(
                    "UPDATE task SET status = 'failed', error = ?2, failure = 'other' WHERE id = ?1",
                    params![
                        row.id,
                        format!(
                            "与「{}」共用任务目录 {work_dir}，放弃这个任务会删掉那个任务已下的内容；只能从列表移除",
                            owner.get()
                        )
                    ],
                )?;
            }
        }
    }
    Ok(())
}

/// 第 3 版：设置（[`crate::Settings`]）只有一行，编号固定为 1；保存目录为 NULL 时没设过，代理的写法见
/// [`proxy_code`]。
fn create_settings_table(tx: &Transaction<'_>) -> Result<(), Error> {
    tx.execute_batch(
        "CREATE TABLE settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            folder TEXT,
            proxy TEXT NOT NULL DEFAULT 'system' CHECK (proxy IN ('system', 'direct') OR proxy GLOB 'url:?*')
        ) STRICT;
        INSERT INTO settings (id) VALUES (1);",
    )?;
    Ok(())
}

/// 第 4 版，重建表，任务分种类（`kind`）：
/// - `hls`：选轨偏好、输出（种类与 MP4、本地 HLS 的路径）、是否录直播必有，总片数与只写出已录只用于它。
/// - `file`：单个文件，输出路径必有；认出地址时服务器报的总字节数、ETag、Last-Modified、Content-Type（第一次下载按它们
///   核对，都可无），与每个请求最多取的字节数（可无）只用于它。
///
/// 第 3 版的行都是 `hls`。失败种类的写法不变。删掉旧表会连同删掉它的编号计数，新表的计数接着旧表的，编号照样不重用。
fn rebuild_with_kinds(tx: &Transaction<'_>) -> Result<(), Error> {
    let counter = task_counter(tx)?;
    tx.execute_batch(
        "CREATE TABLE task_v4 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('hls', 'file')),
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            preference TEXT,
            output_kind TEXT CHECK (output_kind IN ('mp4', 'hls', 'both')),
            mp4_path TEXT,
            hls_path TEXT,
            live INTEGER CHECK (live IN (0, 1)),
            file_path TEXT,
            file_length INTEGER CHECK (file_length >= 0),
            file_etag TEXT,
            file_last_modified TEXT,
            file_content_type TEXT,
            chunk_size INTEGER CHECK (chunk_size > 0),
            work_dir TEXT,
            status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done', 'discard-unfinished')),
            error TEXT,
            failure TEXT CHECK (
                (status = 'failed') = (failure IS NOT NULL)
                AND (failure IS NULL
                     OR failure IN ('network', 'certificate', 'disk', 'output-taken', 'source-changed', 'malformed',
                                    'stalled', 'nothing-to-write', 'mp4-unsupported', 'unsupported', 'unrecoverable',
                                    'other')
                     OR failure GLOB 'status:[0-9][0-9][0-9]')
            ),
            created_at INTEGER NOT NULL,
            total INTEGER CHECK (total >= 0),
            duration_ms INTEGER CHECK (duration_ms >= 0),
            bytes INTEGER CHECK (bytes >= 0),
            width INTEGER CHECK (width > 0),
            height INTEGER CHECK (height > 0),
            write_only INTEGER NOT NULL DEFAULT 0 CHECK (write_only IN (0, 1)),
            CHECK (kind = 'file' OR (preference IS NOT NULL AND output_kind IS NOT NULL AND live IS NOT NULL)),
            CHECK (kind = 'hls' OR (preference IS NULL AND output_kind IS NULL AND live IS NULL AND mp4_path IS NULL
                                    AND hls_path IS NULL AND total IS NULL AND write_only = 0)),
            CHECK ((kind = 'file') = (file_path IS NOT NULL)),
            CHECK (kind = 'file' OR (file_length IS NULL AND file_etag IS NULL AND file_last_modified IS NULL
                                     AND file_content_type IS NULL AND chunk_size IS NULL))
        ) STRICT;
        INSERT INTO task_v4 (id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, work_dir,
                             status, error, failure, created_at, total, duration_ms, bytes, width, height, write_only)
            SELECT id, 'hls', title, url, headers, preference, output_kind, mp4_path, hls_path, live, work_dir, status,
                   error, failure, created_at, total, duration_ms, bytes, width, height, write_only
            FROM task;
        DROP TABLE task;
        ALTER TABLE task_v4 RENAME TO task;",
    )?;
    keep_task_counter(tx, counter)
}

/// 任务表的编号计数（AUTOINCREMENT 记在 sqlite_sequence 里）；还没分配过编号时为 None。重建任务表前读出，重建后由
/// [`keep_task_counter`] 接上：删掉旧表会连同删掉它的计数。
fn task_counter(tx: &Transaction<'_>) -> Result<Option<i64>, Error> {
    Ok(tx
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'task'",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

/// 重建后的任务表的编号计数接着重建前的 `counter`（[`task_counter`]）。
fn keep_task_counter(tx: &Transaction<'_>, counter: Option<i64>) -> Result<(), Error> {
    if let Some(counter) = counter {
        let kept = tx.execute(
            "UPDATE sqlite_sequence SET seq = max(seq, ?1) WHERE name = 'task'",
            params![counter],
        )?;
        if kept == 0 {
            tx.execute(
                "INSERT INTO sqlite_sequence (name, seq) VALUES ('task', ?1)",
                params![counter],
            )?;
        }
    }
    Ok(())
}

/// 第 6 版，重建任务表：
/// - 文件任务加 `file_accepts_ranges`（认出地址时服务器支持范围请求，第一次下载按它决定带不带 If-Range 核对），只用于
///   文件任务且必有。第 5 版没有文件任务（服务拒绝新建）；有的话照下面只拷了两种任务共有的列，文件的各列都补不出，
///   迁移按约束失败、存储留在第 5 版。
/// - 失败的种类加 `not-expected`（[`crate::FailureKind::NotExpected`]）。
fn rebuild_with_ranges(tx: &Transaction<'_>) -> Result<(), Error> {
    let counter = task_counter(tx)?;
    tx.execute_batch(
        "CREATE TABLE task_v6 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('hls', 'file')),
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            preference TEXT,
            output_kind TEXT CHECK (output_kind IN ('mp4', 'hls', 'both')),
            mp4_path TEXT,
            hls_path TEXT,
            live INTEGER CHECK (live IN (0, 1)),
            file_path TEXT,
            file_length INTEGER CHECK (file_length >= 0),
            file_accepts_ranges INTEGER CHECK (file_accepts_ranges IN (0, 1)),
            file_etag TEXT,
            file_last_modified TEXT,
            file_content_type TEXT,
            chunk_size INTEGER CHECK (chunk_size > 0),
            work_dir TEXT,
            status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done', 'discard-unfinished')),
            error TEXT,
            failure TEXT CHECK (
                (status = 'failed') = (failure IS NOT NULL)
                AND (failure IS NULL
                     OR failure IN ('network', 'certificate', 'disk', 'output-taken', 'source-changed', 'not-expected',
                                    'malformed', 'stalled', 'nothing-to-write', 'mp4-unsupported', 'unsupported',
                                    'unrecoverable', 'other')
                     OR failure GLOB 'status:[0-9][0-9][0-9]')
            ),
            created_at INTEGER NOT NULL,
            total INTEGER CHECK (total >= 0),
            duration_ms INTEGER CHECK (duration_ms >= 0),
            bytes INTEGER CHECK (bytes >= 0),
            width INTEGER CHECK (width > 0),
            height INTEGER CHECK (height > 0),
            write_only INTEGER NOT NULL DEFAULT 0 CHECK (write_only IN (0, 1)),
            CHECK (kind = 'file' OR (preference IS NOT NULL AND output_kind IS NOT NULL AND live IS NOT NULL)),
            CHECK (kind = 'hls' OR (preference IS NULL AND output_kind IS NULL AND live IS NULL AND mp4_path IS NULL
                                    AND hls_path IS NULL AND total IS NULL AND write_only = 0)),
            CHECK ((kind = 'file') = (file_path IS NOT NULL AND file_accepts_ranges IS NOT NULL)),
            CHECK (kind = 'file' OR (file_path IS NULL AND file_accepts_ranges IS NULL AND file_length IS NULL
                                     AND file_etag IS NULL AND file_last_modified IS NULL AND file_content_type IS NULL
                                     AND chunk_size IS NULL))
        ) STRICT;
        INSERT INTO task_v6 (id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, work_dir,
                             status, error, failure, created_at, total, duration_ms, bytes, width, height, write_only)
            SELECT id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, work_dir, status,
                   error, failure, created_at, total, duration_ms, bytes, width, height, write_only
            FROM task;
        DROP TABLE task;
        ALTER TABLE task_v6 RENAME TO task;",
    )?;
    keep_task_counter(tx, counter)
}

/// 第 7 版：
/// - 重建任务表，加种类 `torrent`（BT）：种子的编号（信息哈希的十六进制，40 个小写字母与数字，不同任务不重复）、内容的
///   根（绝对路径）、续传数据（libtorrent 的格式，含种子信息）、续传数据记的已有块是否可信（见
///   [`crate::torrent::Resume::trusted`]）四列必有且只用于它；BT 任务没有任务目录与请求头（`[]`）。HLS 与文件的约束
///   不变。
/// - 设置加 BT 会话的监听端口（1 到 65535，没监听过为 NULL）与 DHT 状态（libtorrent 的格式，没存过为 NULL）。
fn rebuild_with_torrents(tx: &Transaction<'_>) -> Result<(), Error> {
    let counter = task_counter(tx)?;
    tx.execute_batch(
        "CREATE TABLE task_v7 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('hls', 'file', 'torrent')),
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            preference TEXT,
            output_kind TEXT CHECK (output_kind IN ('mp4', 'hls', 'both')),
            mp4_path TEXT,
            hls_path TEXT,
            live INTEGER CHECK (live IN (0, 1)),
            file_path TEXT,
            file_length INTEGER CHECK (file_length >= 0),
            file_accepts_ranges INTEGER CHECK (file_accepts_ranges IN (0, 1)),
            file_etag TEXT,
            file_last_modified TEXT,
            file_content_type TEXT,
            chunk_size INTEGER CHECK (chunk_size > 0),
            torrent_info_hash TEXT UNIQUE CHECK (length(torrent_info_hash) = 40
                                                 AND torrent_info_hash NOT GLOB '*[^0-9a-f]*'),
            torrent_root TEXT,
            torrent_resume BLOB,
            torrent_trusted INTEGER CHECK (torrent_trusted IN (0, 1)),
            work_dir TEXT,
            status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done', 'discard-unfinished')),
            error TEXT,
            failure TEXT CHECK (
                (status = 'failed') = (failure IS NOT NULL)
                AND (failure IS NULL
                     OR failure IN ('network', 'certificate', 'disk', 'output-taken', 'source-changed', 'not-expected',
                                    'malformed', 'stalled', 'nothing-to-write', 'mp4-unsupported', 'unsupported',
                                    'unrecoverable', 'other')
                     OR failure GLOB 'status:[0-9][0-9][0-9]')
            ),
            created_at INTEGER NOT NULL,
            total INTEGER CHECK (total >= 0),
            duration_ms INTEGER CHECK (duration_ms >= 0),
            bytes INTEGER CHECK (bytes >= 0),
            width INTEGER CHECK (width > 0),
            height INTEGER CHECK (height > 0),
            write_only INTEGER NOT NULL DEFAULT 0 CHECK (write_only IN (0, 1)),
            CHECK (kind != 'hls' OR (preference IS NOT NULL AND output_kind IS NOT NULL AND live IS NOT NULL)),
            CHECK (kind = 'hls' OR (preference IS NULL AND output_kind IS NULL AND live IS NULL AND mp4_path IS NULL
                                    AND hls_path IS NULL AND total IS NULL AND write_only = 0)),
            CHECK ((kind = 'file') = (file_path IS NOT NULL AND file_accepts_ranges IS NOT NULL)),
            CHECK (kind = 'file' OR (file_path IS NULL AND file_accepts_ranges IS NULL AND file_length IS NULL
                                     AND file_etag IS NULL AND file_last_modified IS NULL AND file_content_type IS NULL
                                     AND chunk_size IS NULL)),
            CHECK ((kind = 'torrent') = (torrent_info_hash IS NOT NULL AND torrent_root IS NOT NULL
                                         AND torrent_resume IS NOT NULL AND torrent_trusted IS NOT NULL)),
            CHECK (kind = 'torrent' OR (torrent_info_hash IS NULL AND torrent_root IS NULL AND torrent_resume IS NULL
                                        AND torrent_trusted IS NULL)),
            CHECK (kind != 'torrent' OR (work_dir IS NULL AND headers = '[]'))
        ) STRICT;
        INSERT INTO task_v7 (id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, file_path,
                             file_length, file_accepts_ranges, file_etag, file_last_modified, file_content_type,
                             chunk_size, work_dir, status, error, failure, created_at, total, duration_ms, bytes, width,
                             height, write_only)
            SELECT id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, file_path,
                   file_length, file_accepts_ranges, file_etag, file_last_modified, file_content_type, chunk_size,
                   work_dir, status, error, failure, created_at, total, duration_ms, bytes, width, height, write_only
            FROM task;
        DROP TABLE task;
        ALTER TABLE task_v7 RENAME TO task;
        ALTER TABLE settings ADD COLUMN torrent_port INTEGER CHECK (torrent_port BETWEEN 1 AND 65535);
        ALTER TABLE settings ADD COLUMN torrent_dht BLOB;",
    )?;
    keep_task_counter(tx, counter)
}

/// 第 8 版：
/// - 重建任务表，BT 任务加做种标记（只用于它且必有）：1 为在做种、或下次打开要接着做，只在已完成时；第 7 版的 BT 任务都
///   为 0（那时下完即停）。其余约束不变。
/// - 设置加做种与上传限速（见 [`crate::Seeding`]）：下完做不做种、到哪个分享率（百分数）或累计做种多少分钟停（NULL 为
///   不按它停），与整个 BT 会话的上传限速（字节每秒，NULL 为不限）。没设过为做种、分享率 1.0 或 24 小时先到为准、不限速。
fn rebuild_with_seeding(tx: &Transaction<'_>) -> Result<(), Error> {
    let counter = task_counter(tx)?;
    tx.execute_batch(
        "CREATE TABLE task_v8 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('hls', 'file', 'torrent')),
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            preference TEXT,
            output_kind TEXT CHECK (output_kind IN ('mp4', 'hls', 'both')),
            mp4_path TEXT,
            hls_path TEXT,
            live INTEGER CHECK (live IN (0, 1)),
            file_path TEXT,
            file_length INTEGER CHECK (file_length >= 0),
            file_accepts_ranges INTEGER CHECK (file_accepts_ranges IN (0, 1)),
            file_etag TEXT,
            file_last_modified TEXT,
            file_content_type TEXT,
            chunk_size INTEGER CHECK (chunk_size > 0),
            torrent_info_hash TEXT UNIQUE CHECK (length(torrent_info_hash) = 40
                                                 AND torrent_info_hash NOT GLOB '*[^0-9a-f]*'),
            torrent_root TEXT,
            torrent_resume BLOB,
            torrent_trusted INTEGER CHECK (torrent_trusted IN (0, 1)),
            torrent_seeding INTEGER CHECK (torrent_seeding IN (0, 1)),
            work_dir TEXT,
            status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done', 'discard-unfinished')),
            error TEXT,
            failure TEXT CHECK (
                (status = 'failed') = (failure IS NOT NULL)
                AND (failure IS NULL
                     OR failure IN ('network', 'certificate', 'disk', 'output-taken', 'source-changed', 'not-expected',
                                    'malformed', 'stalled', 'nothing-to-write', 'mp4-unsupported', 'unsupported',
                                    'unrecoverable', 'other')
                     OR failure GLOB 'status:[0-9][0-9][0-9]')
            ),
            created_at INTEGER NOT NULL,
            total INTEGER CHECK (total >= 0),
            duration_ms INTEGER CHECK (duration_ms >= 0),
            bytes INTEGER CHECK (bytes >= 0),
            width INTEGER CHECK (width > 0),
            height INTEGER CHECK (height > 0),
            write_only INTEGER NOT NULL DEFAULT 0 CHECK (write_only IN (0, 1)),
            CHECK (kind != 'hls' OR (preference IS NOT NULL AND output_kind IS NOT NULL AND live IS NOT NULL)),
            CHECK (kind = 'hls' OR (preference IS NULL AND output_kind IS NULL AND live IS NULL AND mp4_path IS NULL
                                    AND hls_path IS NULL AND total IS NULL AND write_only = 0)),
            CHECK ((kind = 'file') = (file_path IS NOT NULL AND file_accepts_ranges IS NOT NULL)),
            CHECK (kind = 'file' OR (file_path IS NULL AND file_accepts_ranges IS NULL AND file_length IS NULL
                                     AND file_etag IS NULL AND file_last_modified IS NULL AND file_content_type IS NULL
                                     AND chunk_size IS NULL)),
            CHECK ((kind = 'torrent') = (torrent_info_hash IS NOT NULL AND torrent_root IS NOT NULL
                                         AND torrent_resume IS NOT NULL AND torrent_trusted IS NOT NULL
                                         AND torrent_seeding IS NOT NULL)),
            CHECK (kind = 'torrent' OR (torrent_info_hash IS NULL AND torrent_root IS NULL AND torrent_resume IS NULL
                                        AND torrent_trusted IS NULL AND torrent_seeding IS NULL)),
            CHECK (torrent_seeding IS NOT 1 OR status = 'done'),
            CHECK (kind != 'torrent' OR (work_dir IS NULL AND headers = '[]'))
        ) STRICT;
        INSERT INTO task_v8 (id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, file_path,
                             file_length, file_accepts_ranges, file_etag, file_last_modified, file_content_type,
                             chunk_size, torrent_info_hash, torrent_root, torrent_resume, torrent_trusted, torrent_seeding,
                             work_dir, status, error, failure, created_at, total, duration_ms, bytes, width, height,
                             write_only)
            SELECT id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, file_path,
                   file_length, file_accepts_ranges, file_etag, file_last_modified, file_content_type, chunk_size,
                   torrent_info_hash, torrent_root, torrent_resume, torrent_trusted,
                   CASE WHEN kind = 'torrent' THEN 0 END,
                   work_dir, status, error, failure, created_at, total, duration_ms, bytes, width, height, write_only
            FROM task;
        DROP TABLE task;
        ALTER TABLE task_v8 RENAME TO task;
        ALTER TABLE settings ADD COLUMN seed INTEGER NOT NULL DEFAULT 1 CHECK (seed IN (0, 1));
        ALTER TABLE settings ADD COLUMN seed_ratio_percent INTEGER DEFAULT 100 CHECK (seed_ratio_percent > 0);
        ALTER TABLE settings ADD COLUMN seed_minutes INTEGER DEFAULT 1440 CHECK (seed_minutes > 0);
        ALTER TABLE settings ADD COLUMN upload_limit INTEGER CHECK (upload_limit > 0);",
    )?;
    keep_task_counter(tx, counter)
}

/// 第 9 版：设置加「自动检查更新」（1 为启动时与之后每天查一次有没有新版本），没设过为查。
fn add_check_updates(tx: &Transaction<'_>) -> Result<(), Error> {
    tx.execute_batch(
        "ALTER TABLE settings ADD COLUMN check_updates INTEGER NOT NULL DEFAULT 1 CHECK (check_updates IN (0, 1));",
    )?;
    Ok(())
}

/// 第 10 版：设置加 BT 经的 SOCKS5 代理（见 [`crate::Settings::torrent_proxy`]，`socks5://` 或 `socks5h://` 开头的地址），
/// NULL 为直连；没设过为直连。
fn add_torrent_proxy(tx: &Transaction<'_>) -> Result<(), Error> {
    tx.execute_batch("ALTER TABLE settings ADD COLUMN torrent_proxy TEXT;")?;
    Ok(())
}

/// 第 11 版：
/// - 重建任务表，加云离线任务（种类 `cloud`，见 [`crate::CloudSpec`]）：网盘（`cloud_provider`）、建任务的账号
///   （`cloud_user`）与保存目录（`cloud_folder`）必有；网盘上离线任务的编号（`cloud_job`）提交成了才有；内容的根
///   （`cloud_root`）与任务目录一起在列出文件时定下，那时一定已提交；提交发出前记下提交前网盘上已有的同链接任务
///   （`cloud_before`，JSON 字符串数组），拿到编号即清掉；没有请求头。失败的种类加 `cloud-login`、
///   `cloud-quota`、`cloud-failed`、`cloud-api`、`integrity`。其余约束不变。
/// - 云离线任务的文件（`cloud_file`，见 [`crate::CloudFile`]）：按任务与序号（列出时的次序，从 0 起），路径不重复；
///   下完（`done`）的一定已取回到输出位置（`fetched`）。不用外键：迁移在事务里跑，关不了外键检查，之后重建任务表时
///   DROP TABLE 会连带删掉这里的行；删任务时由 [`delete`] 一起删。
/// - 网盘账号（`cloud_account`，见 [`hs_dlx_cloud::pikpak::Account`]）：每个网盘至多一个。
fn rebuild_with_cloud(tx: &Transaction<'_>) -> Result<(), Error> {
    let counter = task_counter(tx)?;
    tx.execute_batch(
        "CREATE TABLE task_v11 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('hls', 'file', 'torrent', 'cloud')),
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            preference TEXT,
            output_kind TEXT CHECK (output_kind IN ('mp4', 'hls', 'both')),
            mp4_path TEXT,
            hls_path TEXT,
            live INTEGER CHECK (live IN (0, 1)),
            file_path TEXT,
            file_length INTEGER CHECK (file_length >= 0),
            file_accepts_ranges INTEGER CHECK (file_accepts_ranges IN (0, 1)),
            file_etag TEXT,
            file_last_modified TEXT,
            file_content_type TEXT,
            chunk_size INTEGER CHECK (chunk_size > 0),
            torrent_info_hash TEXT UNIQUE CHECK (length(torrent_info_hash) = 40
                                                 AND torrent_info_hash NOT GLOB '*[^0-9a-f]*'),
            torrent_root TEXT,
            torrent_resume BLOB,
            torrent_trusted INTEGER CHECK (torrent_trusted IN (0, 1)),
            torrent_seeding INTEGER CHECK (torrent_seeding IN (0, 1)),
            cloud_provider TEXT CHECK (cloud_provider IN ('pikpak')),
            cloud_user TEXT CHECK (cloud_user != ''),
            cloud_job TEXT CHECK (cloud_job != ''),
            cloud_before TEXT CHECK (cloud_before IS NULL OR cloud_job IS NULL),
            cloud_folder TEXT,
            cloud_root TEXT,
            work_dir TEXT,
            status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done', 'discard-unfinished')),
            error TEXT,
            failure TEXT CHECK (
                (status = 'failed') = (failure IS NOT NULL)
                AND (failure IS NULL
                     OR failure IN ('network', 'certificate', 'disk', 'output-taken', 'source-changed', 'not-expected',
                                    'malformed', 'stalled', 'nothing-to-write', 'mp4-unsupported', 'unsupported',
                                    'unrecoverable', 'cloud-login', 'cloud-quota', 'cloud-failed', 'cloud-api',
                                    'integrity', 'other')
                     OR failure GLOB 'status:[0-9][0-9][0-9]')
            ),
            created_at INTEGER NOT NULL,
            total INTEGER CHECK (total >= 0),
            duration_ms INTEGER CHECK (duration_ms >= 0),
            bytes INTEGER CHECK (bytes >= 0),
            width INTEGER CHECK (width > 0),
            height INTEGER CHECK (height > 0),
            write_only INTEGER NOT NULL DEFAULT 0 CHECK (write_only IN (0, 1)),
            CHECK (kind != 'hls' OR (preference IS NOT NULL AND output_kind IS NOT NULL AND live IS NOT NULL)),
            CHECK (kind = 'hls' OR (preference IS NULL AND output_kind IS NULL AND live IS NULL AND mp4_path IS NULL
                                    AND hls_path IS NULL AND total IS NULL AND write_only = 0)),
            CHECK ((kind = 'file') = (file_path IS NOT NULL AND file_accepts_ranges IS NOT NULL)),
            CHECK (kind = 'file' OR (file_path IS NULL AND file_accepts_ranges IS NULL AND file_length IS NULL
                                     AND file_etag IS NULL AND file_last_modified IS NULL AND file_content_type IS NULL
                                     AND chunk_size IS NULL)),
            CHECK ((kind = 'torrent') = (torrent_info_hash IS NOT NULL AND torrent_root IS NOT NULL
                                         AND torrent_resume IS NOT NULL AND torrent_trusted IS NOT NULL
                                         AND torrent_seeding IS NOT NULL)),
            CHECK (kind = 'torrent' OR (torrent_info_hash IS NULL AND torrent_root IS NULL AND torrent_resume IS NULL
                                        AND torrent_trusted IS NULL AND torrent_seeding IS NULL)),
            CHECK (torrent_seeding IS NOT 1 OR status = 'done'),
            CHECK (kind != 'torrent' OR (work_dir IS NULL AND headers = '[]')),
            CHECK ((kind = 'cloud') = (cloud_provider IS NOT NULL AND cloud_user IS NOT NULL
                                       AND cloud_folder IS NOT NULL)),
            CHECK (kind = 'cloud' OR (cloud_provider IS NULL AND cloud_user IS NULL AND cloud_job IS NULL
                                      AND cloud_before IS NULL AND cloud_folder IS NULL AND cloud_root IS NULL)),
            CHECK (kind != 'cloud' OR (headers = '[]' AND (cloud_root IS NULL) = (work_dir IS NULL)
                                       AND (cloud_root IS NULL OR cloud_job IS NOT NULL)))
        ) STRICT;
        INSERT INTO task_v11 (id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live,
                              file_path, file_length, file_accepts_ranges, file_etag, file_last_modified,
                              file_content_type, chunk_size, torrent_info_hash, torrent_root, torrent_resume,
                              torrent_trusted, torrent_seeding, work_dir, status, error, failure, created_at, total,
                              duration_ms, bytes, width, height, write_only)
            SELECT id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, file_path,
                   file_length, file_accepts_ranges, file_etag, file_last_modified, file_content_type, chunk_size,
                   torrent_info_hash, torrent_root, torrent_resume, torrent_trusted, torrent_seeding, work_dir, status,
                   error, failure, created_at, total, duration_ms, bytes, width, height, write_only
            FROM task;
        DROP TABLE task;
        ALTER TABLE task_v11 RENAME TO task;
        CREATE TABLE cloud_file (
            task_id INTEGER NOT NULL,
            seq INTEGER NOT NULL CHECK (seq >= 0),
            file_id TEXT NOT NULL CHECK (file_id != ''),
            path TEXT NOT NULL,
            size INTEGER NOT NULL CHECK (size >= 0),
            gcid TEXT NOT NULL CHECK (length(gcid) = 40 AND gcid NOT GLOB '*[^0-9a-f]*'),
            fetched INTEGER NOT NULL DEFAULT 0 CHECK (fetched IN (0, 1)),
            done INTEGER NOT NULL DEFAULT 0 CHECK (done IN (0, 1)),
            skipped INTEGER NOT NULL DEFAULT 0 CHECK (skipped IN (0, 1)),
            CHECK (done = 0 OR fetched = 1),
            CHECK (skipped = 0 OR fetched = 0),
            PRIMARY KEY (task_id, seq),
            UNIQUE (task_id, path)
        ) STRICT;
        CREATE TABLE cloud_account (
            provider TEXT PRIMARY KEY CHECK (provider IN ('pikpak')),
            user_id TEXT NOT NULL CHECK (user_id != ''),
            device_id TEXT NOT NULL CHECK (device_id != ''),
            refresh_token TEXT NOT NULL CHECK (refresh_token != ''),
            access_token TEXT NOT NULL CHECK (access_token != ''),
            access_expires_at INTEGER NOT NULL
        ) STRICT;",
    )?;
    keep_task_counter(tx, counter)
}

/// 第 12 版：网页视频任务（`kind` 为 `web`，yt-dlp 解析）。重建任务表（SQLite 改不了 CHECK）：
/// - 网页视频的各列：解析器（`web_extractor`）、视频编号（`web_video_id`）、视频与另配的音频的格式（`web_video`、
///   `web_audio`，至少一个；认出格式用的各项，写法见 `StoredFormat`）、合并后的容器（`web_container`：`mp4`、`matroska`，只有一路不合并时为 `original`）、输出
///   （`web_path`）；有任务目录（各路各一个核心的任务目录在里面）。只属于网页视频的列在别的任务为 NULL。请求头只放
///   Cookie（解析与下载时带上）。
/// - 失败的种类加 `web-login`、`web-geo`、`web-extract`、`web-gone`、`web-changed`、`web-denied`、`web-blocked`、
///   `web-cookies`、`web-components`。其余约束不变，已有的行原样搬过来。
/// - 设置加网页视频的 yt-dlp 渠道（`web_channel`：`stable`、`nightly`）、画质取向（`web_preference`：`compatible`、
///   `quality`）与读 Cookie 的浏览器（`web_browser`：yt-dlp 的浏览器名，NULL 为不读）及它的用户资料
///   （`web_browser_profile`：目录的绝对路径，浏览器没有用户资料之分时为 NULL），没设过为稳定版、兼容优先、不读。
fn rebuild_with_web(tx: &Transaction<'_>) -> Result<(), Error> {
    let counter = task_counter(tx)?;
    tx.execute_batch(
        "CREATE TABLE task_v12 (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            kind TEXT NOT NULL CHECK (kind IN ('hls', 'file', 'torrent', 'cloud', 'web')),
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            preference TEXT,
            output_kind TEXT CHECK (output_kind IN ('mp4', 'hls', 'both')),
            mp4_path TEXT,
            hls_path TEXT,
            live INTEGER CHECK (live IN (0, 1)),
            file_path TEXT,
            file_length INTEGER CHECK (file_length >= 0),
            file_accepts_ranges INTEGER CHECK (file_accepts_ranges IN (0, 1)),
            file_etag TEXT,
            file_last_modified TEXT,
            file_content_type TEXT,
            chunk_size INTEGER CHECK (chunk_size > 0),
            torrent_info_hash TEXT UNIQUE CHECK (length(torrent_info_hash) = 40
                                                 AND torrent_info_hash NOT GLOB '*[^0-9a-f]*'),
            torrent_root TEXT,
            torrent_resume BLOB,
            torrent_trusted INTEGER CHECK (torrent_trusted IN (0, 1)),
            torrent_seeding INTEGER CHECK (torrent_seeding IN (0, 1)),
            cloud_provider TEXT CHECK (cloud_provider IN ('pikpak')),
            cloud_user TEXT CHECK (cloud_user != ''),
            cloud_job TEXT CHECK (cloud_job != ''),
            cloud_before TEXT CHECK (cloud_before IS NULL OR cloud_job IS NULL),
            cloud_folder TEXT,
            cloud_root TEXT,
            web_extractor TEXT CHECK (web_extractor != ''),
            web_video_id TEXT CHECK (web_video_id != ''),
            web_video TEXT CHECK (web_video != ''),
            web_audio TEXT CHECK (web_audio != ''),
            web_container TEXT CHECK (web_container IN ('mp4', 'matroska', 'original')),
            web_path TEXT,
            work_dir TEXT,
            status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done', 'discard-unfinished')),
            error TEXT,
            failure TEXT CHECK (
                (status = 'failed') = (failure IS NOT NULL)
                AND (failure IS NULL
                     OR failure IN ('network', 'certificate', 'disk', 'output-taken', 'source-changed', 'not-expected',
                                    'malformed', 'stalled', 'nothing-to-write', 'mp4-unsupported', 'unsupported',
                                    'unrecoverable', 'cloud-login', 'cloud-quota', 'cloud-failed', 'cloud-api',
                                    'integrity', 'web-login', 'web-geo', 'web-extract', 'web-gone',
                                    'web-changed', 'web-denied', 'web-blocked', 'web-cookies', 'web-components',
                                    'other')
                     OR failure GLOB 'status:[0-9][0-9][0-9]')
            ),
            created_at INTEGER NOT NULL,
            total INTEGER CHECK (total >= 0),
            duration_ms INTEGER CHECK (duration_ms >= 0),
            bytes INTEGER CHECK (bytes >= 0),
            width INTEGER CHECK (width > 0),
            height INTEGER CHECK (height > 0),
            write_only INTEGER NOT NULL DEFAULT 0 CHECK (write_only IN (0, 1)),
            CHECK (kind != 'hls' OR (preference IS NOT NULL AND output_kind IS NOT NULL AND live IS NOT NULL)),
            CHECK (kind = 'hls' OR (preference IS NULL AND output_kind IS NULL AND live IS NULL AND mp4_path IS NULL
                                    AND hls_path IS NULL AND total IS NULL AND write_only = 0)),
            CHECK ((kind = 'file') = (file_path IS NOT NULL AND file_accepts_ranges IS NOT NULL)),
            CHECK (kind = 'file' OR (file_path IS NULL AND file_accepts_ranges IS NULL AND file_length IS NULL
                                     AND file_etag IS NULL AND file_last_modified IS NULL AND file_content_type IS NULL
                                     AND chunk_size IS NULL)),
            CHECK ((kind = 'torrent') = (torrent_info_hash IS NOT NULL AND torrent_root IS NOT NULL
                                         AND torrent_resume IS NOT NULL AND torrent_trusted IS NOT NULL
                                         AND torrent_seeding IS NOT NULL)),
            CHECK (kind = 'torrent' OR (torrent_info_hash IS NULL AND torrent_root IS NULL AND torrent_resume IS NULL
                                        AND torrent_trusted IS NULL AND torrent_seeding IS NULL)),
            CHECK (torrent_seeding IS NOT 1 OR status = 'done'),
            CHECK (kind != 'torrent' OR (work_dir IS NULL AND headers = '[]')),
            CHECK ((kind = 'cloud') = (cloud_provider IS NOT NULL AND cloud_user IS NOT NULL
                                       AND cloud_folder IS NOT NULL)),
            CHECK (kind = 'cloud' OR (cloud_provider IS NULL AND cloud_user IS NULL AND cloud_job IS NULL
                                      AND cloud_before IS NULL AND cloud_folder IS NULL AND cloud_root IS NULL)),
            CHECK (kind != 'cloud' OR (headers = '[]' AND (cloud_root IS NULL) = (work_dir IS NULL)
                                       AND (cloud_root IS NULL OR cloud_job IS NOT NULL))),
            CHECK ((kind = 'web') = (web_extractor IS NOT NULL AND web_video_id IS NOT NULL
                                     AND web_container IS NOT NULL AND web_path IS NOT NULL)),
            CHECK (kind = 'web' OR (web_extractor IS NULL AND web_video_id IS NULL AND web_video IS NULL
                                    AND web_audio IS NULL AND web_container IS NULL AND web_path IS NULL)),
            CHECK (kind != 'web' OR ((web_video IS NOT NULL OR web_audio IS NOT NULL) AND work_dir IS NOT NULL))
        ) STRICT;
        INSERT INTO task_v12 (id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live,
                              file_path, file_length, file_accepts_ranges, file_etag, file_last_modified,
                              file_content_type, chunk_size, torrent_info_hash, torrent_root, torrent_resume,
                              torrent_trusted, torrent_seeding, cloud_provider, cloud_user, cloud_job, cloud_before,
                              cloud_folder, cloud_root, work_dir, status, error, failure, created_at, total,
                              duration_ms, bytes, width, height, write_only)
            SELECT id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live,
                              file_path, file_length, file_accepts_ranges, file_etag, file_last_modified,
                              file_content_type, chunk_size, torrent_info_hash, torrent_root, torrent_resume,
                              torrent_trusted, torrent_seeding, cloud_provider, cloud_user, cloud_job, cloud_before,
                              cloud_folder, cloud_root, work_dir, status, error, failure, created_at, total,
                              duration_ms, bytes, width, height, write_only
            FROM task;
        DROP TABLE task;
        ALTER TABLE task_v12 RENAME TO task;
        ALTER TABLE settings ADD COLUMN web_channel TEXT NOT NULL DEFAULT 'stable'
            CHECK (web_channel IN ('stable', 'nightly'));
        ALTER TABLE settings ADD COLUMN web_preference TEXT NOT NULL DEFAULT 'compatible'
            CHECK (web_preference IN ('compatible', 'quality'));
        ALTER TABLE settings ADD COLUMN web_browser TEXT
            CHECK (web_browser IN ('chrome', 'edge', 'firefox', 'safari', 'brave', 'chromium', 'opera', 'vivaldi',
                                   'whale'));
        ALTER TABLE settings ADD COLUMN web_browser_profile TEXT
            CHECK (web_browser_profile IS NULL OR (web_browser_profile != '' AND web_browser IS NOT NULL));",
    )?;
    keep_task_counter(tx, counter)
}

/// 第 2 版迁移比较路径用的键：转小写再做 NFC，与写这一版时的 [`naming::path_key`] 相同；那边之后改规则不影响这次
/// 迁移的结果。
fn v2_path_key(path: &str) -> String {
    icu_normalizer::ComposingNormalizerBorrowed::new_nfc()
        .normalize(&path.to_lowercase())
        .into_owned()
}

/// 第 1 版任务目录的位置：输出 `path`（绝对路径）同级的 `<主名>.hsdl`，MP4 的主名去掉扩展名、本地 HLS 的为目录名；
/// 路径不以文件名结尾时为 None。只给第 2 版的迁移用，与当前的规则无关。
fn v1_work_dir(path: &str, strip_extension: bool) -> Option<String> {
    let path = Path::new(path);
    let name = if strip_extension {
        path.file_stem()?
    } else {
        path.file_name()?
    };
    let mut dir = name.to_os_string();
    dir.push(".hsdl");
    path.parent()?.join(dir).into_os_string().into_string().ok()
}

/// 存储被别的连接占着时等多久再报错：有单实例锁，占着它的只会是外部工具（如 sqlite3 命令行），而写存储常在界面线程上。
const BUSY_TIMEOUT: Duration = Duration::from_secs(1);

/// 打开的任务存储。
pub struct Store {
    conn: Connection,
    /// 文件存储旁的锁文件，持有期间别的进程打不开同一份存储；内存里的存储为 None
    _lock: Option<File>,
}

impl Store {
    /// 打开（不存在时新建）`path` 处的存储并迁移到当前版本；所在目录不存在时创建。Unix 上文件只有本人可读写：
    /// 里面明文存着请求头。版本比本应用认识的新时报 [`Error::Corrupt`]（用新版本的应用打开过）。同一份存储同时只能
    /// 被一个进程打开：先对同目录的 `<文件名>.lock` 加排他锁，已被别的进程锁着时报 [`Error::InUse`]；锁随存储释放，
    /// 进程退出时由系统释放。
    pub fn open(path: &Path) -> Result<Store, Error> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io("创建", dir))?;
        }
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        let lock = File::create(&lock_path).map_err(io("创建", &lock_path))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::InUse(path.to_path_buf())),
            Err(TryLockError::Error(cause)) => return Err(io("加锁", &lock_path)(cause)),
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // 请求头里有明文的 Cookie、Authorization：删掉或换下来的内容随即覆盖，不留在空闲页里
        conn.pragma_update(None, "secure_delete", true)?;
        restrict_to_owner(path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Store {
            conn,
            _lock: Some(lock),
        };
        store.migrate()?;
        Ok(store)
    }

    /// 内存里的存储，用于测试。
    pub fn in_memory() -> Result<Store, Error> {
        let mut store = Store {
            conn: Connection::open_in_memory()?,
            _lock: None,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        Ok(self.conn.transaction()?)
    }

    fn migrate(&mut self) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let version = usize::try_from(version)
            .map_err(|_| Error::Corrupt(format!("表结构版本 {version} 不是非负数")))?;
        if version > MIGRATIONS.len() {
            return Err(Error::Corrupt(format!(
                "表结构版本 {version} 比本应用认识的 {} 新",
                MIGRATIONS.len()
            )));
        }
        for migration in &MIGRATIONS[version..] {
            migration(&tx)?;
        }
        tx.pragma_update(None, "user_version", schema_version(MIGRATIONS.len()))?;
        tx.commit()?;
        Ok(())
    }
}

/// 读出一个任务的各列，顺序与 [`Row::read`] 一致。
const COLUMNS: &str = "id, kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, file_path, \
                       file_length, file_etag, file_last_modified, file_content_type, chunk_size, work_dir, status, \
                       error, failure, created_at, total, duration_ms, bytes, width, height, write_only, \
                       file_accepts_ranges, torrent_info_hash, torrent_root, torrent_seeding, cloud_provider, \
                       cloud_job, cloud_folder, cloud_root, cloud_user, cloud_before, web_extractor, web_video_id, \
                       web_video, web_audio, web_container, web_path";

/// 新建任务，状态为运行中，任务目录为 `work_dir`（绝对路径；BT 任务没有，为 None），BT 任务的续传数据为 `resume`
/// （别的任务为 None）；返回分配的编号。输出或任务目录与已有任务的输出或任务目录相同（按 [`naming::path_key`] 比较）
/// 时报 [`Error::PathTaken`]：共用任务目录时，放弃其中一个会删掉另一个已下的内容；共用输出时，后写出的那个要到写出时
/// 才失败。BT 任务的种子已在别的任务里时报 [`Error::TorrentTaken`]。
pub fn insert(
    tx: &Transaction<'_>,
    spec: &TaskSpec,
    work_dir: Option<&Path>,
    resume: Option<&[u8]>,
    created_at: i64,
) -> Result<TaskId, Error> {
    let work_dir_text = work_dir.map(utf8).transpose()?;
    if let Some(dir) = work_dir
        && !dir.is_absolute()
    {
        return Err(Error::InvalidInput(format!(
            "任务目录不是绝对路径：{}",
            dir.display()
        )));
    }
    // 只属于别的种类的列为 NULL（表上的 CHECK 按种类核对）
    let (preference, output_kind, mp4, hls, live) = match &spec.kind {
        Kind::Hls(spec) => {
            let (kind, mp4, hls) = output_columns(&spec.output)?;
            (
                Some(to_json(&spec.preference)),
                Some(kind),
                mp4,
                hls,
                Some(spec.live),
            )
        }
        Kind::File(_) | Kind::Torrent(_) | Kind::Cloud(_) | Kind::Web(_) => {
            (None, None, None, None, None)
        }
    };
    let (file_path, length, accepts_ranges, etag, last_modified, content_type, chunk) = match &spec
        .kind
    {
        Kind::Hls(_) | Kind::Torrent(_) | Kind::Cloud(_) | Kind::Web(_) => {
            (None, None, None, None, None, None, None)
        }
        Kind::File(file) => (
            Some(named_utf8(&file.path)?),
            // 服务器报的，可能大得离谱
            file.expected
                .length
                .map(|length| {
                    i64::try_from(length).map_err(|_| {
                        Error::InvalidInput(format!("服务器报的文件大小 {length} 字节超出范围"))
                    })
                })
                .transpose()?,
            Some(file.expected.accepts_ranges),
            file.expected.etag.as_deref(),
            file.expected.last_modified.as_deref(),
            file.expected.content_type.as_deref(),
            file.chunk_size
                .map(|size| i64::try_from(size.get()).expect("每个请求的字节数远小于 i64 的上限")),
        ),
    };
    let (info_hash, root, trusted, seeding) = match &spec.kind {
        Kind::Hls(_) | Kind::File(_) | Kind::Cloud(_) | Kind::Web(_) => (None, None, None, None),
        Kind::Torrent(torrent) => {
            if let Some(existing) = tx
                .query_row(
                    "SELECT id FROM task WHERE torrent_info_hash = ?1",
                    [&torrent.info_hash],
                    |row| row.get(0),
                )
                .optional()?
            {
                return Err(Error::TorrentTaken(TaskId(existing)));
            }
            // 新建时的续传数据还没有已有的块；还在下，不做种
            (
                Some(torrent.info_hash.as_str()),
                Some(named_utf8(&torrent.root)?),
                Some(true),
                Some(false),
            )
        }
    };
    let (provider, user, job, before, folder, cloud_root) = match &spec.kind {
        Kind::Hls(_) | Kind::File(_) | Kind::Torrent(_) | Kind::Web(_) => {
            (None, None, None, None, None, None)
        }
        Kind::Cloud(cloud) => {
            if !cloud.folder.is_absolute() {
                return Err(Error::InvalidInput(format!(
                    "保存目录不是绝对路径：{}",
                    cloud.folder.display()
                )));
            }
            (
                Some(provider_code(cloud.provider)),
                Some(cloud.user.as_str()),
                cloud.job.as_deref(),
                cloud.before.as_ref().map(to_json),
                Some(utf8(&cloud.folder)?),
                cloud.root.as_deref().map(named_utf8).transpose()?,
            )
        }
    };
    let (extractor, video_id, video, audio, container, web_path) = match &spec.kind {
        Kind::Hls(_) | Kind::File(_) | Kind::Torrent(_) | Kind::Cloud(_) => {
            (None, None, None, None, None, None)
        }
        Kind::Web(web) => {
            if web.video.is_none() && web.audio.is_none() {
                return Err(Error::InvalidInput("网页视频没选视频也没选音频".to_owned()));
            }
            if work_dir.is_none() {
                return Err(Error::InvalidInput("网页视频要有任务目录".to_owned()));
            }
            (
                Some(web.extractor.as_str()),
                Some(web.video_id.as_str()),
                web.video
                    .as_ref()
                    .map(|key| to_json(&StoredFormat::from(key))),
                web.audio
                    .as_ref()
                    .map(|key| to_json(&StoredFormat::from(key))),
                Some(container_code(web.container)),
                Some(named_utf8(&web.path)?),
            )
        }
    };
    let taken = taken_paths(tx)?;
    for path in [
        &mp4,
        &hls,
        &file_path,
        &root,
        &cloud_root,
        &web_path,
        &work_dir_text,
    ]
    .into_iter()
    .flatten()
    {
        if taken.contains(&naming::path_key(path)) {
            return Err(Error::PathTaken(path.into()));
        }
    }
    tx.execute(
        "INSERT INTO task (kind, title, url, headers, preference, output_kind, mp4_path, hls_path, live, file_path,
                           file_length, file_etag, file_last_modified, file_content_type, chunk_size, work_dir,
                           status, created_at, file_accepts_ranges, torrent_info_hash, torrent_root, torrent_resume,
                           torrent_trusted, torrent_seeding, cloud_provider, cloud_job, cloud_folder, cloud_root,
                           cloud_user, cloud_before, web_extractor, web_video_id, web_video, web_audio,
                           web_container, web_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 'running', ?17, ?18, ?19, ?20,
                 ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35)",
        params![
            kind_code(&spec.kind),
            spec.title,
            spec.url.as_str(),
            to_json(&spec.headers),
            preference,
            output_kind,
            mp4,
            hls,
            live,
            file_path,
            length,
            etag,
            last_modified,
            content_type,
            chunk,
            work_dir_text,
            created_at,
            accepts_ranges,
            info_hash,
            root,
            resume,
            trusted,
            seeding,
            provider,
            job,
            folder,
            cloud_root,
            user,
            before,
            extractor,
            video_id,
            video,
            audio,
            container,
            web_path,
        ],
    )?;
    Ok(TaskId(tx.last_insert_rowid()))
}

/// 全部任务，按新建先后。
pub fn list(tx: &Transaction<'_>) -> Result<Vec<Task>, Error> {
    let mut statement = tx.prepare(&format!("SELECT {COLUMNS} FROM task ORDER BY id"))?;
    let rows = statement.query_map([], Row::read)?;
    rows.map(|row| row?.into_task()).collect()
}

/// 一个任务；没有时为 None。
pub fn get(tx: &Transaction<'_>, id: TaskId) -> Result<Option<Task>, Error> {
    tx.query_row(
        &format!("SELECT {COLUMNS} FROM task WHERE id = ?1"),
        [id.0],
        Row::read,
    )
    .optional()?
    .map(Row::into_task)
    .transpose()
}

/// 已有任务占用的路径的比较键（[`naming::path_key`]）：各任务的 MP4、本地 HLS 目录、文件、BT 与云离线内容的根与任务
/// 目录。新建任务选主名时避开它们。
pub fn taken_paths(tx: &Transaction<'_>) -> Result<HashSet<String>, Error> {
    taken_except(tx, None)
}

/// 除任务 `except` 之外的任务占用的路径的比较键；`except` 为 None 时为全部任务的。
fn taken_except(tx: &Transaction<'_>, except: Option<TaskId>) -> Result<HashSet<String>, Error> {
    let mut statement = tx.prepare(
        "SELECT mp4_path, hls_path, file_path, torrent_root, cloud_root, web_path, work_dir FROM task
         WHERE ?1 IS NULL OR id != ?1",
    )?;
    let rows = statement.query_map([except.map(|id| id.0)], |row| {
        Ok([
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
        ])
    })?;
    let mut taken = HashSet::new();
    for row in rows {
        taken.extend(
            row?.into_iter()
                .flatten()
                .map(|path| naming::path_key(&path)),
        );
    }
    Ok(taken)
}

/// 换掉 HLS 任务的输出；新的输出路径与别的任务的输出或任务目录相同时报 [`Error::PathTaken`]，没有这个 HLS 任务时报
/// [`Error::NotFound`]。任务目录不变。
pub fn set_output(tx: &Transaction<'_>, id: TaskId, output: &OutputSpec) -> Result<(), Error> {
    let (kind, mp4, hls) = output_columns(output)?;
    let others = taken_except(tx, Some(id))?;
    for path in [mp4.as_deref(), hls.as_deref()].into_iter().flatten() {
        if others.contains(&naming::path_key(path)) {
            return Err(Error::PathTaken(path.into()));
        }
    }
    if tx.execute(
        "UPDATE task SET output_kind = ?2, mp4_path = ?3, hls_path = ?4 WHERE id = ?1 AND kind = 'hls'",
        params![id.0, kind, mp4, hls],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 换掉网页视频任务合并后的容器与输出；新的输出路径与别的任务的输出或任务目录相同时报 [`Error::PathTaken`]，没有这个
/// 网页视频任务时报 [`Error::NotFound`]。任务目录不变。
pub fn set_web_output(
    tx: &Transaction<'_>,
    id: TaskId,
    container: Container,
    path: &Path,
) -> Result<(), Error> {
    let path_text = named_utf8(path)?;
    if taken_except(tx, Some(id))?.contains(&naming::path_key(&path_text)) {
        return Err(Error::PathTaken(path.into()));
    }
    if tx.execute(
        "UPDATE task SET web_container = ?2, web_path = ?3 WHERE id = ?1 AND kind = 'web'",
        params![id.0, container_code(container), path_text],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 换掉任务的请求头；没有这个任务时报 [`Error::NotFound`]。
pub fn set_headers(
    tx: &Transaction<'_>,
    id: TaskId,
    headers: &[(String, String)],
) -> Result<(), Error> {
    if tx.execute(
        "UPDATE task SET headers = ?2 WHERE id = ?1",
        params![id.0, to_json(&headers)],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 记下 HLS 点播的总片数；没有这个 HLS 任务时报 [`Error::NotFound`]。
pub fn set_total(tx: &Transaction<'_>, id: TaskId, total: usize) -> Result<(), Error> {
    let total = i64::try_from(total).expect("分片数远小于 i64 的上限");
    if tx.execute(
        "UPDATE task SET total = ?2 WHERE id = ?1 AND kind = 'hls'",
        params![id.0, total],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 记下完成时写出的结果；没有这个任务时报 [`Error::NotFound`]。
pub fn set_result(tx: &Transaction<'_>, id: TaskId, result: &Finished) -> Result<(), Error> {
    let millis = result
        .duration
        .map(|d| i64::try_from(d.as_millis()).expect("时长的毫秒数远小于 i64 的上限"));
    let bytes = i64::try_from(result.bytes).expect("字节数远小于 i64 的上限");
    let (width, height) = result.resolution.unzip();
    if tx.execute(
        "UPDATE task SET duration_ms = ?2, bytes = ?3, width = ?4, height = ?5 WHERE id = ?1",
        params![id.0, millis, bytes, width, height],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 记下继续时是否只写出已录的（见 [`Task::write_only`]）；没有这个任务时报 [`Error::NotFound`]。文件任务只能记为
/// false（表上有 CHECK）。
pub fn set_write_only(tx: &Transaction<'_>, id: TaskId, write_only: bool) -> Result<(), Error> {
    if tx.execute(
        "UPDATE task SET write_only = ?2 WHERE id = ?1",
        params![id.0, write_only],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 改状态；没有这个任务时报 [`Error::NotFound`]。
pub fn set_status(tx: &Transaction<'_>, id: TaskId, status: &Status) -> Result<(), Error> {
    let (name, error, failure) = match status {
        Status::Running => ("running", None, None),
        Status::Paused => ("paused", None, None),
        Status::Failed(failure) => (
            "failed",
            Some(failure.message.as_str()),
            Some(failure.kind.code()),
        ),
        Status::Done => ("done", None, None),
        Status::DiscardUnfinished => ("discard-unfinished", None, None),
    };
    let changed = tx.execute(
        "UPDATE task SET status = ?2, error = ?3, failure = ?4 WHERE id = ?1",
        params![id.0, name, error, failure],
    )?;
    if changed == 0 {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 把还标着运行中的任务改为暂停：应用上次异常退出时没来得及改；返回改了几个。其中 BT 任务的续传数据改为不可信：
/// 运行中下了的块没记进去，而存的续传数据可能比磁盘上的多（写盘与改文件在它之后）。
pub fn pause_interrupted(tx: &Transaction<'_>) -> Result<usize, Error> {
    tx.execute(
        "UPDATE task SET torrent_trusted = 0 WHERE status = 'running' AND kind = 'torrent'",
        [],
    )?;
    Ok(tx.execute(
        "UPDATE task SET status = 'paused' WHERE status = 'running'",
        [],
    )?)
}

/// BT 任务的续传数据；没有这个 BT 任务时报 [`Error::NotFound`]。
pub fn torrent_resume(tx: &Transaction<'_>, id: TaskId) -> Result<Resume, Error> {
    tx.query_row(
        "SELECT torrent_resume, torrent_trusted FROM task WHERE id = ?1 AND kind = 'torrent'",
        [id.0],
        |row| {
            Ok(Resume {
                data: row.get(0)?,
                trusted: row.get(1)?,
            })
        },
    )
    .optional()?
    .ok_or(Error::NotFound(id))
}

/// 换掉 BT 任务的续传数据；没有这个 BT 任务时报 [`Error::NotFound`]。
pub fn set_torrent_resume(tx: &Transaction<'_>, id: TaskId, resume: &Resume) -> Result<(), Error> {
    if tx.execute(
        "UPDATE task SET torrent_resume = ?2, torrent_trusted = ?3 WHERE id = ?1 AND kind = 'torrent'",
        params![id.0, resume.data, resume.trusted],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 上次关闭时存下的 BT 会话状态；没存过的为空（端口为 None、DHT 状态为空）。
pub fn torrent_session(tx: &Transaction<'_>) -> Result<SessionState, Error> {
    let (port, dht): (Option<u16>, Option<Vec<u8>>) = tx.query_row(
        "SELECT torrent_port, torrent_dht FROM settings WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(SessionState {
        port,
        dht: dht.unwrap_or_default(),
    })
}

/// 记下关闭时的 BT 会话状态；没有端口（[`SessionState::port`] 为 None）时存着的端口照旧。
pub fn set_torrent_session(tx: &Transaction<'_>, state: &SessionState) -> Result<(), Error> {
    tx.execute(
        "UPDATE settings SET torrent_port = COALESCE(?1, torrent_port), torrent_dht = ?2 WHERE id = 1",
        params![state.port, state.dht],
    )?;
    Ok(())
}

/// 删除任务（云离线任务连同它的文件）；没有时报 [`Error::NotFound`]。
pub fn delete(tx: &Transaction<'_>, id: TaskId) -> Result<(), Error> {
    tx.execute("DELETE FROM cloud_file WHERE task_id = ?1", [id.0])?;
    if tx.execute("DELETE FROM task WHERE id = ?1", [id.0])? == 0 {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 定下云离线任务的内容的根 `root`、任务目录 `work_dir`（都是以文件名结尾的绝对路径）与要下的文件 `files`（次序即
/// 序号），标题改为根名。根或任务目录与别的任务占用的相同时报 [`Error::PathTaken`]；`files` 为空、路径重复、GCID 不是
/// 40 位小写十六进制时报 [`Error::InvalidInput`]；没有这个云离线任务、或它已定下了根时报 [`Error::NotFound`]。
pub fn set_cloud_root(
    tx: &Transaction<'_>,
    id: TaskId,
    root: &Path,
    work_dir: &Path,
    files: &[CloudFile],
) -> Result<(), Error> {
    let (root_text, work_dir_text) = (named_utf8(root)?, named_utf8(work_dir)?);
    if files.is_empty() {
        return Err(Error::InvalidInput(format!("{id}没有要下的文件")));
    }
    let others = taken_except(tx, Some(id))?;
    for path in [&root_text, &work_dir_text] {
        if others.contains(&naming::path_key(path)) {
            return Err(Error::PathTaken(path.into()));
        }
    }
    let title = root
        .file_name()
        .expect("上面已核对以文件名结尾")
        .to_string_lossy()
        .into_owned();
    if tx.execute(
        "UPDATE task SET cloud_root = ?2, work_dir = ?3, title = ?4
         WHERE id = ?1 AND kind = 'cloud' AND cloud_root IS NULL",
        params![id.0, root_text, work_dir_text, title],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    for (seq, file) in files.iter().enumerate() {
        let size = i64::try_from(file.size).map_err(|_| {
            Error::InvalidInput(format!("网盘报的文件大小 {} 字节超出范围", file.size))
        })?;
        let inserted = tx.execute(
            "INSERT INTO cloud_file (task_id, seq, file_id, path, size, gcid, fetched, done)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id.0,
                i64::try_from(seq).expect("文件数远小于 i64 的上限"),
                file.id,
                file.path,
                size,
                file.gcid,
                file.fetched,
                file.done
            ],
        );
        match inserted {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(Error::InvalidInput(format!(
                    "{id}的文件不合要求（路径重复、GCID 不对或没取回就记为下完）：{}",
                    file.path
                )));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// 云离线任务的文件，按序号；还没定下根的为空。路径不是相对内容的根、以 `/` 分隔的文件名时（有空段、`.`、`..`、
/// 反斜杠、冒号）报 [`Error::Corrupt`]：本应用写入的都合要求，拼出的路径不会越出内容的根。
pub fn cloud_files(tx: &Transaction<'_>, id: TaskId) -> Result<Vec<CloudFile>, Error> {
    let mut statement = tx.prepare(
        "SELECT file_id, path, size, gcid, fetched, done, skipped FROM cloud_file WHERE task_id = ?1
         ORDER BY seq",
    )?;
    let rows = statement.query_map([id.0], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, bool>(4)?,
            row.get::<_, bool>(5)?,
            row.get::<_, bool>(6)?,
        ))
    })?;
    rows.map(|row| {
        let (file_id, path, size, gcid, fetched, done, skipped) = row?;
        // 反斜杠与冒号在 Windows 上会换掉路径的根（`C:x`、`\\x`）
        let safe = path.is_empty()
            || path
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..") && !part.contains(['\\', ':']));
        if !safe {
            return Err(Error::Corrupt(format!("{id}的文件路径 {path:?}")));
        }
        Ok(CloudFile {
            id: file_id,
            path,
            size: u64::try_from(size).map_err(|_| Error::Corrupt(format!("{id}的文件大小")))?,
            gcid,
            fetched,
            done,
            skipped,
        })
    })
    .collect()
}

/// 记下云离线任务提交到网盘后网盘给的离线任务编号 `job`（不能为空），清掉提交中的标记；没有这个还没提交过的云离线
/// 任务时报 [`Error::NotFound`]。
pub fn set_cloud_job(tx: &Transaction<'_>, id: TaskId, job: &str) -> Result<(), Error> {
    if job.is_empty() {
        return Err(Error::InvalidInput("网盘给的离线任务编号是空的".to_owned()));
    }
    if tx.execute(
        "UPDATE task SET cloud_job = ?2, cloud_before = NULL
         WHERE id = ?1 AND kind = 'cloud' AND cloud_job IS NULL",
        params![id.0, job],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 记下云离线任务正要提交：`before` 为提交前网盘上已有的同一链接的离线任务编号，提交结果不明（发出后断开、退出）时
/// 下次按它找这之后新建的那个。没有这个还没提交过的云离线任务时报 [`Error::NotFound`]。
pub fn set_cloud_before(tx: &Transaction<'_>, id: TaskId, before: &[String]) -> Result<(), Error> {
    if tx.execute(
        "UPDATE task SET cloud_before = ?2 WHERE id = ?1 AND kind = 'cloud' AND cloud_job IS NULL",
        params![id.0, to_json(&before)],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 除 `id` 外各云离线任务记着的离线任务编号：查回结果不明的提交时排除它们，不认领别的任务的。
pub fn cloud_jobs_except(tx: &Transaction<'_>, id: TaskId) -> Result<Vec<String>, Error> {
    let mut statement = tx.prepare(
        "SELECT cloud_job FROM task WHERE kind = 'cloud' AND cloud_job IS NOT NULL AND id != ?1",
    )?;
    let jobs = statement
        .query_map([id.0], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(jobs)
}

/// 网盘上的离线任务不在了（用户在网页或 App 里删了）：清掉记着的编号，继续时重新提交。只用于还没列出文件的；没有
/// 这样的云离线任务时报 [`Error::NotFound`]。
pub fn clear_cloud_job(tx: &Transaction<'_>, id: TaskId) -> Result<(), Error> {
    if tx.execute(
        "UPDATE task SET cloud_job = NULL, cloud_before = NULL
         WHERE id = ?1 AND kind = 'cloud' AND cloud_root IS NULL",
        [id.0],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 记下云离线任务第 `seq` 个文件取回到输出位置没有（核心装上了输出、还没校验）；没有这个文件时报
/// [`Error::NotFound`]。记为没取回时一并记为没下完。
pub fn set_cloud_file_fetched(
    tx: &Transaction<'_>,
    id: TaskId,
    seq: usize,
    fetched: bool,
) -> Result<(), Error> {
    let seq = i64::try_from(seq).expect("文件数远小于 i64 的上限");
    if tx.execute(
        "UPDATE cloud_file SET fetched = ?3, done = done AND ?3 WHERE task_id = ?1 AND seq = ?2",
        params![id.0, seq, fetched],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 记下云离线任务第 `seq` 个文件下完（校验过）没有；没有这个文件时报 [`Error::NotFound`]，还没取回的记为下完违反
/// 表的约束。
pub fn set_cloud_file_done(
    tx: &Transaction<'_>,
    id: TaskId,
    seq: usize,
    done: bool,
) -> Result<(), Error> {
    let seq = i64::try_from(seq).expect("文件数远小于 i64 的上限");
    if tx.execute(
        "UPDATE cloud_file SET done = ?3 WHERE task_id = ?1 AND seq = ?2",
        params![id.0, seq, done],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 记下云离线任务第 `seq` 个文件跳过（定下根之后用户在网盘上删了它），一并记为没取回、没下完：调用方已确认输出
/// 位置上没有它。没有这个文件时报 [`Error::NotFound`]。
pub fn set_cloud_file_skipped(tx: &Transaction<'_>, id: TaskId, seq: usize) -> Result<(), Error> {
    let seq = i64::try_from(seq).expect("文件数远小于 i64 的上限");
    if tx.execute(
        "UPDATE cloud_file SET skipped = 1, fetched = 0, done = 0 WHERE task_id = ?1 AND seq = ?2",
        params![id.0, seq],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 登录了的 PikPak 账号；没登录时为 None。
pub fn pikpak_account(tx: &Transaction<'_>) -> Result<Option<Account>, Error> {
    Ok(tx
        .query_row(
            "SELECT user_id, device_id, refresh_token, access_token, access_expires_at
             FROM cloud_account WHERE provider = 'pikpak'",
            [],
            |row| {
                Ok(Account {
                    user_id: row.get(0)?,
                    device_id: row.get(1)?,
                    refresh_token: row.get(2)?,
                    access_token: row.get(3)?,
                    access_expires_at: row.get(4)?,
                })
            },
        )
        .optional()?)
}

/// 记下 PikPak 账号（替换已有的）；None 为退出登录。
pub fn set_pikpak_account(tx: &Transaction<'_>, account: Option<&Account>) -> Result<(), Error> {
    match account {
        Some(account) => tx.execute(
            "INSERT OR REPLACE INTO cloud_account
                 (provider, user_id, device_id, refresh_token, access_token, access_expires_at)
             VALUES ('pikpak', ?1, ?2, ?3, ?4, ?5)",
            params![
                account.user_id,
                account.device_id,
                account.refresh_token,
                account.access_token,
                account.access_expires_at
            ],
        )?,
        None => tx.execute("DELETE FROM cloud_account WHERE provider = 'pikpak'", [])?,
    };
    Ok(())
}

/// 表里的一行，原样读出、未校验。
struct Row {
    id: i64,
    kind: String,
    title: String,
    url: String,
    headers: String,
    preference: Option<String>,
    output_kind: Option<String>,
    mp4_path: Option<String>,
    hls_path: Option<String>,
    live: Option<bool>,
    file_path: Option<String>,
    file_length: Option<i64>,
    file_etag: Option<String>,
    file_last_modified: Option<String>,
    file_content_type: Option<String>,
    chunk_size: Option<i64>,
    work_dir: Option<String>,
    status: String,
    error: Option<String>,
    failure: Option<String>,
    created_at: i64,
    total: Option<i64>,
    duration_ms: Option<i64>,
    bytes: Option<i64>,
    width: Option<u32>,
    height: Option<u32>,
    write_only: bool,
    file_accepts_ranges: Option<bool>,
    torrent_info_hash: Option<String>,
    torrent_root: Option<String>,
    torrent_seeding: Option<bool>,
    cloud_provider: Option<String>,
    cloud_job: Option<String>,
    cloud_folder: Option<String>,
    cloud_root: Option<String>,
    cloud_user: Option<String>,
    cloud_before: Option<String>,
    web_extractor: Option<String>,
    web_video_id: Option<String>,
    web_video: Option<String>,
    web_audio: Option<String>,
    web_container: Option<String>,
    web_path: Option<String>,
}

impl Row {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
        Ok(Row {
            id: row.get(0)?,
            kind: row.get(1)?,
            title: row.get(2)?,
            url: row.get(3)?,
            headers: row.get(4)?,
            preference: row.get(5)?,
            output_kind: row.get(6)?,
            mp4_path: row.get(7)?,
            hls_path: row.get(8)?,
            live: row.get(9)?,
            file_path: row.get(10)?,
            file_length: row.get(11)?,
            file_etag: row.get(12)?,
            file_last_modified: row.get(13)?,
            file_content_type: row.get(14)?,
            chunk_size: row.get(15)?,
            work_dir: row.get(16)?,
            status: row.get(17)?,
            error: row.get(18)?,
            failure: row.get(19)?,
            created_at: row.get(20)?,
            total: row.get(21)?,
            duration_ms: row.get(22)?,
            bytes: row.get(23)?,
            width: row.get(24)?,
            height: row.get(25)?,
            write_only: row.get(26)?,
            file_accepts_ranges: row.get(27)?,
            torrent_info_hash: row.get(28)?,
            torrent_root: row.get(29)?,
            torrent_seeding: row.get(30)?,
            cloud_provider: row.get(31)?,
            cloud_job: row.get(32)?,
            cloud_folder: row.get(33)?,
            cloud_root: row.get(34)?,
            cloud_user: row.get(35)?,
            cloud_before: row.get(36)?,
            web_extractor: row.get(37)?,
            web_video_id: row.get(38)?,
            web_video: row.get(39)?,
            web_audio: row.get(40)?,
            web_container: row.get(41)?,
            web_path: row.get(42)?,
        })
    }

    fn into_task(self) -> Result<Task, Error> {
        let id = TaskId(self.id);
        let corrupt = |what: &str| Error::Corrupt(format!("任务 {} 的{what}", self.id));
        let url = self.url.parse().map_err(|_| corrupt("地址"))?;
        let headers: Vec<(String, String)> =
            serde_json::from_str(&self.headers).map_err(|_| corrupt("请求头"))?;
        let cloud = match (
            self.cloud_provider,
            self.cloud_user,
            self.cloud_job,
            self.cloud_folder,
            self.cloud_root,
        ) {
            (None, None, None, None, None) => None,
            (Some(provider), Some(user), job, Some(folder), root)
                if job.is_some() || root.is_none() =>
            {
                let folder = PathBuf::from(folder);
                let root = root.map(PathBuf::from);
                if !folder.is_absolute()
                    || root.as_deref().is_some_and(|root| !named_absolute(root))
                {
                    return Err(corrupt("云离线的保存目录或内容的根"));
                }
                let before = self
                    .cloud_before
                    .map(|text| serde_json::from_str::<Vec<String>>(&text))
                    .transpose()
                    .map_err(|_| corrupt("提交前已有的离线任务"))?;
                Some(CloudSpec {
                    provider: parse_provider(&provider).ok_or_else(|| corrupt("网盘"))?,
                    user,
                    job,
                    before,
                    folder,
                    root,
                })
            }
            _ => return Err(corrupt("云离线的各列")),
        };
        let web = match (
            self.web_extractor,
            self.web_video_id,
            self.web_container,
            self.web_path,
        ) {
            (None, None, None, None) if self.web_video.is_none() && self.web_audio.is_none() => {
                None
            }
            (Some(extractor), Some(video_id), Some(container), Some(path))
                if self.web_video.is_some() || self.web_audio.is_some() =>
            {
                let path = PathBuf::from(path);
                if !named_absolute(&path) {
                    return Err(corrupt("网页视频的输出"));
                }
                let format = |text: Option<String>| {
                    text.map(|text| {
                        serde_json::from_str::<StoredFormat>(&text)
                            .map(FormatKey::from)
                            .map_err(|_| corrupt("网页视频的格式"))
                    })
                    .transpose()
                };
                Some(WebSpec {
                    extractor,
                    video_id,
                    video: format(self.web_video)?,
                    audio: format(self.web_audio)?,
                    container: parse_container(&container).ok_or_else(|| corrupt("容器"))?,
                    path,
                })
            }
            _ => return Err(corrupt("网页视频的各列")),
        };
        let kind = match (
            self.kind.as_str(),
            self.preference,
            self.output_kind,
            self.live,
            self.file_path,
            self.file_accepts_ranges,
            self.torrent_info_hash,
            self.torrent_root,
        ) {
            ("hls", Some(preference), Some(output_kind), Some(live), None, None, None, None)
                if cloud.is_none() && web.is_none() =>
            {
                let preference: StoredPreference =
                    serde_json::from_str(&preference).map_err(|_| corrupt("选轨偏好"))?;
                let output = output_spec(&output_kind, self.mp4_path, self.hls_path)
                    .ok_or_else(|| corrupt("输出"))?;
                Kind::Hls(HlsSpec {
                    preference,
                    output,
                    live,
                })
            }
            ("file", None, None, None, Some(path), Some(accepts_ranges), None, None)
                if cloud.is_none() && web.is_none() =>
            {
                let path = PathBuf::from(path);
                if !named_absolute(&path) {
                    return Err(corrupt("输出"));
                }
                let length = self
                    .file_length
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| corrupt("文件的字节数"))?;
                let chunk_size = self
                    .chunk_size
                    .map(|size| u64::try_from(size).ok().and_then(std::num::NonZeroU64::new))
                    .map(|size| size.ok_or_else(|| corrupt("每个请求的字节数")))
                    .transpose()?;
                Kind::File(FileSpec {
                    path,
                    expected: Expected {
                        length,
                        accepts_ranges,
                        etag: self.file_etag,
                        last_modified: self.file_last_modified,
                        content_type: self.file_content_type,
                    },
                    chunk_size,
                })
            }
            ("torrent", None, None, None, None, None, Some(info_hash), Some(root))
                if cloud.is_none() && web.is_none() =>
            {
                let root = PathBuf::from(root);
                if !named_absolute(&root) {
                    return Err(corrupt("内容的根"));
                }
                Kind::Torrent(TorrentSpec { info_hash, root })
            }
            ("cloud", None, None, None, None, None, None, None) if web.is_none() => {
                Kind::Cloud(cloud.ok_or_else(|| corrupt("云离线的各列"))?)
            }
            ("web", None, None, None, None, None, None, None) if cloud.is_none() => {
                Kind::Web(web.ok_or_else(|| corrupt("网页视频的各列"))?)
            }
            _ => return Err(corrupt("种类")),
        };
        let work_dir = match self.work_dir.map(PathBuf::from) {
            Some(dir) if !named_absolute(&dir) => return Err(corrupt("任务目录")),
            dir => dir,
        };
        let total = self
            .total
            .map(usize::try_from)
            .transpose()
            .map_err(|_| corrupt("总片数"))?;
        let result = match (self.bytes, self.width, self.height) {
            (None, None, None) if self.duration_ms.is_none() => None,
            (Some(bytes), width, height) if width.is_some() == height.is_some() => Some(Finished {
                duration: self
                    .duration_ms
                    .map(|ms| u64::try_from(ms).map(Duration::from_millis))
                    .transpose()
                    .map_err(|_| corrupt("时长"))?,
                bytes: u64::try_from(bytes).map_err(|_| corrupt("字节数"))?,
                resolution: width.zip(height),
            }),
            _ => return Err(corrupt("写出的结果")),
        };
        let status = match (self.status.as_str(), self.error, self.failure) {
            ("running", None, None) => Status::Running,
            ("paused", None, None) => Status::Paused,
            ("failed", Some(message), Some(code)) => Status::Failed(Failure {
                kind: FailureKind::parse(&code).ok_or_else(|| corrupt("失败的种类"))?,
                message,
            }),
            ("done", None, None) => Status::Done,
            ("discard-unfinished", None, None) => Status::DiscardUnfinished,
            _ => return Err(corrupt("状态")),
        };
        Ok(Task {
            id,
            spec: TaskSpec {
                title: self.title,
                url,
                headers,
                kind,
            },
            status,
            work_dir,
            created_at: self.created_at,
            total,
            result,
            write_only: self.write_only,
            seeding: self.torrent_seeding.unwrap_or(false),
        })
    }
}

/// 表里的输出三列读回成输出；种类与路径对不上、或路径不是以文件名结尾的绝对路径时为 None。
fn output_spec(kind: &str, mp4: Option<String>, hls: Option<String>) -> Option<OutputSpec> {
    let output = match (kind, mp4, hls) {
        ("mp4", Some(mp4), None) => OutputSpec::Mp4(mp4.into()),
        ("hls", None, Some(hls)) => OutputSpec::Hls(hls.into()),
        ("both", Some(mp4), Some(hls)) => OutputSpec::Both {
            mp4: mp4.into(),
            hls: hls.into(),
        },
        _ => return None,
    };
    let valid = match &output {
        OutputSpec::Mp4(path) | OutputSpec::Hls(path) => named_absolute(path),
        OutputSpec::Both { mp4, hls } => named_absolute(mp4) && named_absolute(hls),
    };
    valid.then_some(output)
}

/// 是以文件名结尾的绝对路径（不以 `..` 或根结尾）。
fn named_absolute(path: &Path) -> bool {
    path.is_absolute() && path.file_name().is_some()
}

/// 种类在表里的写法（表上有 CHECK）。
fn kind_code(kind: &Kind) -> &'static str {
    match kind {
        Kind::Hls(_) => "hls",
        Kind::File(_) => "file",
        Kind::Torrent(_) => "torrent",
        Kind::Cloud(_) => "cloud",
        Kind::Web(_) => "web",
    }
}

/// 网页视频合并后的容器在表里的写法（表上有 CHECK）。
fn container_code(container: Container) -> &'static str {
    match container {
        Container::Mp4 => "mp4",
        Container::Matroska => "matroska",
        Container::Original => "original",
    }
}

/// [`container_code`] 的反过来；认不出时为 None。
fn parse_container(code: &str) -> Option<Container> {
    match code {
        "mp4" => Some(Container::Mp4),
        "matroska" => Some(Container::Matroska),
        "original" => Some(Container::Original),
        _ => None,
    }
}

/// 网盘在表里的写法（表上有 CHECK）。
fn provider_code(provider: CloudProvider) -> &'static str {
    match provider {
        CloudProvider::PikPak => "pikpak",
    }
}

/// [`provider_code`] 的反过来；认不出时为 None。
fn parse_provider(code: &str) -> Option<CloudProvider> {
    match code {
        "pikpak" => Some(CloudProvider::PikPak),
        _ => None,
    }
}

/// 输出路径在表里的写法：须是以文件名结尾的绝对路径（读回时同样核对），且是 UTF-8。
fn named_utf8(path: &Path) -> Result<String, Error> {
    if !named_absolute(path) {
        return Err(Error::InvalidInput(format!(
            "输出不是以文件名结尾的绝对路径：{}",
            path.display()
        )));
    }
    utf8(path)
}

/// 第 5 版：设置加外观主题，写法见 [`theme_code`]，没设过为日蚀。
fn add_theme(tx: &Transaction<'_>) -> Result<(), Error> {
    tx.execute_batch(
        "ALTER TABLE settings ADD COLUMN
            theme TEXT NOT NULL DEFAULT 'eclipse' CHECK (theme IN ('eclipse', 'blood-moon'));",
    )?;
    Ok(())
}

/// 路径在表里的写法：须是 UTF-8（核心对输出路径的要求相同）。
fn utf8(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidInput(format!("路径不是 UTF-8：{}", path.display())))
}

/// 读出设置；保存目录不是绝对路径、代理（含 BT 的）或主题认不出、做种的数不是正数时报 [`Error::Corrupt`]。
pub fn settings(tx: &Transaction<'_>) -> Result<Settings, Error> {
    type Columns = (
        Option<String>,
        String,
        String,
        bool,
        Option<u32>,
        Option<u32>,
        Option<u32>,
        bool,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<String>,
    );
    let (
        folder,
        proxy,
        theme,
        seed,
        ratio,
        minutes,
        upload,
        check_updates,
        torrent_proxy,
        web_channel,
        web_preference,
        web_browser,
        web_browser_profile,
    ): Columns = tx.query_row(
            "SELECT folder, proxy, theme, seed, seed_ratio_percent, seed_minutes, upload_limit, check_updates,
                    torrent_proxy, web_channel, web_preference, web_browser, web_browser_profile
             FROM settings WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            },
        )?;
    let torrent_proxy = torrent_proxy
        .map(|text| {
            Url::parse(&text)
                .ok()
                .filter(|url| crate::socks::check(url).is_ok())
                .ok_or_else(|| Error::Corrupt("设置里的 BT 代理".to_owned()))
        })
        .transpose()?;
    let positive = |value: Option<u32>, what: &str| {
        value
            .map(|value| {
                NonZeroU32::new(value).ok_or_else(|| Error::Corrupt(format!("设置里的{what}")))
            })
            .transpose()
    };
    let seeding = if seed {
        Seeding::Until {
            ratio_percent: positive(ratio, "做种分享率")?,
            minutes: positive(minutes, "做种时长")?,
        }
    } else {
        Seeding::Off
    };
    let upload_limit = positive(upload, "上传限速")?;
    let folder = folder
        .map(PathBuf::from)
        .map(|folder| {
            folder
                .is_absolute()
                .then_some(folder)
                .ok_or_else(|| Error::Corrupt("设置里的保存目录".to_owned()))
        })
        .transpose()?;
    Ok(Settings {
        folder,
        proxy: parse_proxy(&proxy)?,
        theme: parse_theme(&theme)?,
        seeding,
        upload_limit,
        check_updates,
        torrent_proxy,
        web_channel: match web_channel.as_str() {
            "stable" => hs_dlx_ytdlp::Channel::Stable,
            "nightly" => hs_dlx_ytdlp::Channel::Nightly,
            _ => return Err(Error::Corrupt("设置里的 yt-dlp 渠道".to_owned())),
        },
        web_preference: match web_preference.as_str() {
            "compatible" => crate::web::Preference::Compatible,
            "quality" => crate::web::Preference::Quality,
            _ => return Err(Error::Corrupt("设置里的画质取向".to_owned())),
        },
        web_browser: web_browser
            .map(|code| {
                let browser = hs_dlx_ytdlp::Browser::from_code(&code)
                    .ok_or_else(|| Error::Corrupt("设置里读 Cookie 的浏览器".to_owned()))?;
                Ok::<_, Error>(hs_dlx_ytdlp::CookieSource {
                    browser,
                    profile: web_browser_profile.map(PathBuf::from),
                })
            })
            .transpose()?,
    })
}

/// 记下解析网页视频时从哪个浏览器的哪个用户资料读 Cookie（None 为不读）。用户资料的目录不是 UTF-8 时报
/// [`Error::InvalidInput`]。
pub fn set_web_browser(
    tx: &Transaction<'_>,
    source: Option<&hs_dlx_ytdlp::CookieSource>,
) -> Result<(), Error> {
    let profile = source
        .and_then(|source| source.profile.as_ref())
        .map(|dir| {
            dir.to_str().map(str::to_owned).ok_or_else(|| {
                Error::InvalidInput(format!("用户资料的目录不是 UTF-8：{}", dir.display()))
            })
        })
        .transpose()?;
    tx.execute(
        "UPDATE settings SET web_browser = ?1, web_browser_profile = ?2 WHERE id = 1",
        rusqlite::params![source.map(|source| source.browser.code()), profile],
    )?;
    Ok(())
}

/// 记下网页视频的 yt-dlp 渠道。
pub fn set_web_channel(tx: &Transaction<'_>, channel: hs_dlx_ytdlp::Channel) -> Result<(), Error> {
    let code = match channel {
        hs_dlx_ytdlp::Channel::Stable => "stable",
        hs_dlx_ytdlp::Channel::Nightly => "nightly",
    };
    tx.execute("UPDATE settings SET web_channel = ?1 WHERE id = 1", [code])?;
    Ok(())
}

/// 记下网页视频的画质取向。
pub fn set_web_preference(
    tx: &Transaction<'_>,
    preference: crate::web::Preference,
) -> Result<(), Error> {
    let code = match preference {
        crate::web::Preference::Compatible => "compatible",
        crate::web::Preference::Quality => "quality",
    };
    tx.execute(
        "UPDATE settings SET web_preference = ?1 WHERE id = 1",
        [code],
    )?;
    Ok(())
}

/// 记下 BT 经的 SOCKS5 代理（None 为直连）；调用方已校验（见 [`crate::socks::check`]）。
pub fn set_torrent_proxy(tx: &Transaction<'_>, proxy: Option<&Url>) -> Result<(), Error> {
    tx.execute(
        "UPDATE settings SET torrent_proxy = ?1 WHERE id = 1",
        params![proxy.map(Url::as_str)],
    )?;
    Ok(())
}

/// 记下要不要自动检查更新。
pub fn set_check_updates(tx: &Transaction<'_>, check: bool) -> Result<(), Error> {
    tx.execute(
        "UPDATE settings SET check_updates = ?1 WHERE id = 1",
        params![check],
    )?;
    Ok(())
}

/// 记下做种的设置。
pub fn set_seeding(tx: &Transaction<'_>, seeding: Seeding) -> Result<(), Error> {
    let (seed, ratio, minutes) = match seeding {
        Seeding::Off => (false, None, None),
        Seeding::Until {
            ratio_percent,
            minutes,
        } => (
            true,
            ratio_percent.map(NonZeroU32::get),
            minutes.map(NonZeroU32::get),
        ),
    };
    // 不做种时分享率与时长照旧留着，改回做种时还是原来的
    if seed {
        tx.execute(
            "UPDATE settings SET seed = 1, seed_ratio_percent = ?1, seed_minutes = ?2 WHERE id = 1",
            params![ratio, minutes],
        )?;
    } else {
        tx.execute("UPDATE settings SET seed = 0 WHERE id = 1", [])?;
    }
    Ok(())
}

/// 记下 BT 的上传限速（字节每秒；None 为不限）。
pub fn set_upload_limit(tx: &Transaction<'_>, limit: Option<NonZeroU32>) -> Result<(), Error> {
    tx.execute(
        "UPDATE settings SET upload_limit = ?1 WHERE id = 1",
        params![limit.map(NonZeroU32::get)],
    )?;
    Ok(())
}

/// 记下 BT 任务做不做种（[`Task::seeding`]）；没有这个 BT 任务时报 [`Error::NotFound`]，任务没完成时由表上的 CHECK
/// 拦下。
pub fn set_seeding_task(tx: &Transaction<'_>, id: TaskId, seeding: bool) -> Result<(), Error> {
    if tx.execute(
        "UPDATE task SET torrent_seeding = ?2 WHERE id = ?1 AND kind = 'torrent'",
        params![id.0, seeding],
    )? == 0
    {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 记下新建任务默认保存到的目录 `folder`（绝对路径，调用方已核对）；不是 UTF-8 时报 [`Error::InvalidInput`]。
pub fn set_folder(tx: &Transaction<'_>, folder: &Path) -> Result<(), Error> {
    tx.execute(
        "UPDATE settings SET folder = ?1 WHERE id = 1",
        params![utf8(folder)?],
    )?;
    Ok(())
}

/// 记下代理（调用方已核对能用）。
pub fn set_proxy(tx: &Transaction<'_>, proxy: &Proxy) -> Result<(), Error> {
    tx.execute(
        "UPDATE settings SET proxy = ?1 WHERE id = 1",
        params![proxy_code(proxy)],
    )?;
    Ok(())
}

/// 记下外观主题。
pub fn set_theme(tx: &Transaction<'_>, theme: Theme) -> Result<(), Error> {
    tx.execute(
        "UPDATE settings SET theme = ?1 WHERE id = 1",
        params![theme_code(theme)],
    )?;
    Ok(())
}

/// 主题在存储里的写法：`eclipse`、`blood-moon`。存储格式的一部分，表上有 CHECK。
fn theme_code(theme: Theme) -> &'static str {
    match theme {
        Theme::Eclipse => "eclipse",
        Theme::BloodMoon => "blood-moon",
    }
}

/// 读回 [`theme_code`] 的写法；认不出时报 [`Error::Corrupt`]。
fn parse_theme(code: &str) -> Result<Theme, Error> {
    match code {
        "eclipse" => Ok(Theme::Eclipse),
        "blood-moon" => Ok(Theme::BloodMoon),
        _ => Err(Error::Corrupt("设置里的外观主题".to_owned())),
    }
}

/// 代理在存储里的写法：`system`、`direct`，或 `url:<代理地址>`（明文，可带口令，与请求头一样只有本人可读写）。
/// 存储格式的一部分，表上有 CHECK。
fn proxy_code(proxy: &Proxy) -> String {
    match proxy {
        Proxy::System => "system".to_owned(),
        Proxy::Direct => "direct".to_owned(),
        Proxy::Url(url) => format!("url:{url}"),
    }
}

/// 读回 [`proxy_code`] 的写法；认不出时报 [`Error::Corrupt`]。
fn parse_proxy(code: &str) -> Result<Proxy, Error> {
    match code {
        "system" => Ok(Proxy::System),
        "direct" => Ok(Proxy::Direct),
        _ => code
            .strip_prefix("url:")
            .and_then(|url| Url::parse(url).ok())
            .map(Proxy::Url)
            .ok_or_else(|| Error::Corrupt("设置里的代理".to_owned())),
    }
}

/// 输出在表里的三列：种类与两个路径。
fn output_columns(
    output: &OutputSpec,
) -> Result<(&'static str, Option<String>, Option<String>), Error> {
    Ok(match output {
        OutputSpec::Mp4(mp4) => ("mp4", Some(utf8(mp4)?), None),
        OutputSpec::Hls(hls) => ("hls", None, Some(utf8(hls)?)),
        OutputSpec::Both { mp4, hls } => ("both", Some(utf8(mp4)?), Some(utf8(hls)?)),
    })
}

/// 表结构版本在 `user_version` 里的写法。
fn schema_version(version: usize) -> i64 {
    i64::try_from(version).expect("迁移的项数远小于 i64 的上限")
}

/// 网页视频一路的格式在 `web_video`、`web_audio` 列里的写法（JSON）：认出它用的各项（见 [`FormatKey`]）。
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredFormat {
    id: String,
    ext: Option<String>,
    video: StoredTrack,
    audio: StoredTrack,
    height: Option<u32>,
    language: Option<String>,
    /// 确切的字节数
    size: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredTrack {
    Absent,
    Unknown,
    Codec(String),
}

impl From<&FormatKey> for StoredFormat {
    fn from(key: &FormatKey) -> StoredFormat {
        let track = |track: &Track| match track {
            Track::Absent => StoredTrack::Absent,
            Track::Unknown => StoredTrack::Unknown,
            Track::Codec(codec) => StoredTrack::Codec(codec.clone()),
        };
        StoredFormat {
            id: key.id.clone(),
            ext: key.ext.clone(),
            video: track(&key.video),
            audio: track(&key.audio),
            height: key.height,
            language: key.language.clone(),
            size: key.size,
        }
    }
}

impl From<StoredFormat> for FormatKey {
    fn from(stored: StoredFormat) -> FormatKey {
        let track = |track: StoredTrack| match track {
            StoredTrack::Absent => Track::Absent,
            StoredTrack::Unknown => Track::Unknown,
            StoredTrack::Codec(codec) => Track::Codec(codec),
        };
        FormatKey {
            id: stored.id,
            ext: stored.ext,
            video: track(stored.video),
            audio: track(stored.audio),
            height: stored.height,
            language: stored.language,
            size: stored.size,
        }
    }
}

fn to_json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("只含字符串、整数、数组与枚举，序列化不会失败")
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.to_path_buf();
    move |cause| Error::Io {
        action,
        path,
        cause,
    }
}

/// 存储文件只有本人可读写。Windows 上用户目录本就只有本人能访问，不另设。
fn restrict_to_owner(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(io("设置权限", path))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use hs_m3u8_core::hls::Preference;

    use super::*;

    #[test]
    fn proxies_read_back_as_written() {
        for proxy in [
            Proxy::System,
            Proxy::Direct,
            Proxy::Url(Url::parse("socks5h://user:pass@127.0.0.1:1080").unwrap()),
        ] {
            assert!(parse_proxy(&proxy_code(&proxy)).unwrap() == proxy);
        }
        assert!(matches!(parse_proxy("pac:x"), Err(Error::Corrupt(_))));
    }

    /// 测试用的绝对路径 `/m/<name>`；Windows 上没有盘符的不算绝对路径，为 `C:\m\<name>`。只用作存储里的值，不访问
    /// 文件系统。
    fn m(name: &str) -> PathBuf {
        let root = if cfg!(windows) { r"C:\m" } else { "/m" };
        Path::new(root).join(name)
    }

    /// [`m`] 在表里的写法。
    fn text(name: &str) -> String {
        utf8(&m(name)).unwrap()
    }

    fn spec(output: OutputSpec) -> TaskSpec {
        TaskSpec {
            title: "主题演讲".into(),
            url: "https://media.example.com/keynote/master.m3u8?token=1"
                .parse()
                .unwrap(),
            headers: vec![("Referer".into(), "https://example.com/".into())],
            kind: Kind::Hls(HlsSpec {
                preference: StoredPreference::try_from(&Preference::default()).unwrap(),
                output,
                live: true,
            }),
        }
    }

    fn file_spec(path: PathBuf) -> TaskSpec {
        TaskSpec {
            title: "数据.zip".into(),
            url: "https://files.example.com/d/1".parse().unwrap(),
            headers: Vec::new(),
            kind: Kind::File(FileSpec {
                path,
                expected: Expected {
                    length: Some(1 << 40),
                    accepts_ranges: true,
                    etag: Some("\"v1\"".into()),
                    last_modified: None,
                    content_type: Some("application/zip".into()),
                },
                chunk_size: std::num::NonZeroU64::new(10 << 20),
            }),
        }
    }

    /// 文件任务读回写入的内容；它的输出与任务目录同样算占用。
    #[test]
    fn file_tasks_round_trip() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let spec = file_spec(m("数据.zip"));
        let id = insert(&tx, &spec, Some(&m("数据.hsdl")), None, 1).unwrap();
        let task = get(&tx, id).unwrap().unwrap();
        assert_eq!(
            (task.spec, task.total, task.write_only),
            (spec, None, false)
        );
        let taken = taken_paths(&tx).unwrap();
        for name in ["数据.zip", "数据.hsdl"] {
            assert!(taken.contains(&naming::path_key(&text(name))), "{name}");
        }
        assert!(matches!(
            insert(&tx, &file_spec(m("数据.ZIP")), Some(&m("x.hsdl")), None, 2),
            Err(Error::PathTaken(_))
        ));
        // 只属于 HLS 的不记到文件任务上
        assert!(matches!(set_total(&tx, id, 3), Err(Error::NotFound(_))));
        assert!(set_write_only(&tx, id, true).is_err());
        let output = OutputSpec::Mp4(m("y.mp4"));
        assert!(matches!(
            set_output(&tx, id, &output),
            Err(Error::NotFound(_))
        ));
    }

    /// 种类与各列对不上的行由表拦下；对得上的两行能插入（说明拦下的正是多出或缺少的那一列）。
    #[test]
    fn kinds_and_columns_must_agree() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        // 种类、选轨偏好、输出种类、是否录直播、文件路径、是否支持范围请求
        let hls = "'hls', '{}', 'mp4', 0, NULL, NULL";
        let file = "'file', NULL, NULL, NULL, '/m/a', 1";
        let insert = |kind_columns: &str, extra_columns: &str, extra_values: &str| {
            tx.execute(
                &format!(
                    "INSERT INTO task (kind, preference, output_kind, live, file_path, file_accepts_ranges, title,
                                       url, headers, status, created_at{extra_columns})
                     VALUES ({kind_columns}, 'x', 'https://a/b', '[]', 'paused', 1{extra_values})"
                ),
                [],
            )
        };
        insert(hls, "", "").unwrap();
        insert(file, "", "").unwrap();
        for (kind_columns, extra_columns, extra_values) in [
            // HLS 缺选轨偏好；文件缺路径、缺是否支持范围请求
            ("'hls', NULL, 'mp4', 0, NULL, NULL", "", ""),
            ("'file', NULL, NULL, NULL, NULL, 1", "", ""),
            ("'file', NULL, NULL, NULL, '/m/a', NULL", "", ""),
            // 文件带了 HLS 的列
            ("'file', NULL, NULL, 0, '/m/a', 1", "", ""),
            (file, ", total", ", 3"),
            // HLS 带了文件的列
            ("'hls', '{}', 'mp4', 0, '/m/a', NULL", "", ""),
            ("'hls', '{}', 'mp4', 0, NULL, 0", "", ""),
            (hls, ", chunk_size", ", 1"),
            (hls, ", file_etag", ", 'e'"),
        ] {
            assert!(
                insert(kind_columns, extra_columns, extra_values).is_err(),
                "{kind_columns}{extra_columns}"
            );
        }
    }

    /// 第 3 版的存储升上来时编号计数照旧：删掉的最大编号不再分配。
    #[test]
    fn rebuilding_for_kinds_keeps_the_id_counter() {
        let dir = scratch("v3-counter");
        let path = dir.join("tasks.sqlite");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for migration in &MIGRATIONS[..3] {
                migration(&tx).unwrap();
            }
            for title in ["甲", "乙"] {
                tx.execute(
                    "INSERT INTO task (title, url, preference, headers, output_kind, mp4_path, live, status, created_at)
                     VALUES (?1, 'https://a/b.m3u8', '{}', '[]', 'mp4', ?2, 0, 'paused', 1)",
                    params![title, text(&format!("{title}.mp4"))],
                )
                .unwrap();
            }
            tx.execute("DELETE FROM task WHERE title = '乙'", [])
                .unwrap();
            tx.pragma_update(None, "user_version", 3).unwrap();
            tx.commit().unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let tx = store.transaction().unwrap();
        let next = add(&tx, OutputSpec::Mp4(m("丙.mp4")), 2).unwrap();
        assert_eq!(next.0, 3);
        drop(tx);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 第 6 版的 HLS 与文件任务升到第 7 版后原样读回，编号计数照旧。
    #[test]
    fn rebuilding_for_torrents_keeps_rows_and_the_id_counter() {
        let dir = scratch("v6-rows");
        let path = dir.join("tasks.sqlite");
        let (hls, file) = (spec(OutputSpec::Mp4(m("甲.mp4"))), file_spec(m("数据.zip")));
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for migration in &MIGRATIONS[..6] {
                migration(&tx).unwrap();
            }
            tx.execute(
                "INSERT INTO task (kind, title, url, headers, preference, output_kind, mp4_path, live, work_dir,
                                   status, failure, error, created_at, total)
                 VALUES ('hls', ?1, ?2, ?3, ?4, 'mp4', ?5, 1, ?6, 'failed', 'status:403', 'HTTP 403', 7, 12)",
                params![
                    hls.title,
                    hls.url.as_str(),
                    to_json(&hls.headers),
                    to_json(&hls.hls().unwrap().preference),
                    text("甲.mp4"),
                    text("甲.hsdl"),
                ],
            )
            .unwrap();
            let expected = &file.file().unwrap().expected;
            tx.execute(
                "INSERT INTO task (kind, title, url, headers, file_path, file_length, file_accepts_ranges, file_etag,
                                   file_content_type, chunk_size, work_dir, status, created_at, bytes)
                 VALUES ('file', ?1, ?2, '[]', ?3, ?4, 1, ?5, ?6, ?7, ?8, 'done', 8, 5)",
                params![
                    file.title,
                    file.url.as_str(),
                    text("数据.zip"),
                    i64::try_from(expected.length.unwrap()).unwrap(),
                    expected.etag,
                    expected.content_type,
                    10 << 20,
                    text("数据.zip.hsdl"),
                ],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO task (kind, title, url, headers, file_path, file_accepts_ranges, status, created_at)
                 VALUES ('file', 'x', 'https://a/x', '[]', ?1, 0, 'paused', 9)",
                params![text("x")],
            )
            .unwrap();
            tx.execute("DELETE FROM task WHERE title = 'x'", [])
                .unwrap();
            tx.pragma_update(None, "user_version", 6).unwrap();
            tx.commit().unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let tx = store.transaction().unwrap();
        let tasks = list(&tx).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!((&tasks[0].spec, tasks[0].total), (&hls, Some(12)));
        assert_eq!(
            tasks[0].status,
            Status::Failed(Failure {
                kind: FailureKind::Status(403),
                message: "HTTP 403".into()
            })
        );
        assert_eq!(
            (&tasks[1].spec, tasks[1].result.as_ref().map(|r| r.bytes)),
            (&file, Some(5))
        );
        assert_eq!(
            tasks[1].work_dir.as_deref(),
            Some(m("数据.zip.hsdl").as_path())
        );
        let next = add(&tx, OutputSpec::Mp4(m("乙.mp4")), 10).unwrap();
        assert_eq!(next.0, 4);
        drop(tx);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 第 7 版的 BT 任务升到第 8 版后原样读回，续传数据照旧，不做种；设置补上默认的做种。
    #[test]
    fn rebuilding_for_seeding_keeps_torrent_rows() {
        let dir = scratch("v7-rows");
        let path = dir.join("tasks.sqlite");
        let hash = "0123456789abcdef0123456789abcdef01234567";
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for migration in &MIGRATIONS[..7] {
                migration(&tx).unwrap();
            }
            tx.execute(
                "INSERT INTO task (kind, title, url, headers, torrent_info_hash, torrent_root, torrent_resume,
                                   torrent_trusted, status, created_at, bytes)
                 VALUES ('torrent', '专辑', 'magnet:?xt=urn:btih:x', '[]', ?1, ?2, x'0102', 0, 'done', 3, 9)",
                params![hash, text("专辑")],
            )
            .unwrap();
            tx.pragma_update(None, "user_version", 7).unwrap();
            tx.commit().unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let tx = store.transaction().unwrap();
        let task = &list(&tx).unwrap()[0];
        assert_eq!(
            (&task.spec.title, task.status.clone(), task.seeding),
            (&"专辑".to_owned(), Status::Done, false)
        );
        assert_eq!(
            task.spec.torrent().unwrap(),
            &TorrentSpec {
                info_hash: hash.into(),
                root: m("专辑")
            }
        );
        assert_eq!(
            torrent_resume(&tx, task.id).unwrap(),
            Resume {
                data: vec![1, 2],
                trusted: false
            }
        );
        assert_eq!(settings(&tx).unwrap().seeding, Seeding::default());
        drop(tx);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 新建任务，任务目录取核心的默认位置。
    fn add(tx: &Transaction<'_>, output: OutputSpec, created_at: i64) -> Result<TaskId, Error> {
        let work_dir = output.options().resolved_work_dir().unwrap();
        insert(tx, &spec(output), Some(&work_dir), None, created_at)
    }

    #[test]
    fn tasks_round_trip_and_change_status() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let a = add(&tx, OutputSpec::Mp4(m("a.mp4")), 100).unwrap();
        let both = OutputSpec::Both {
            mp4: m("b.mp4"),
            hls: m("b"),
        };
        let b = add(&tx, both, 200).unwrap();
        let forbidden = Failure {
            kind: FailureKind::Status(403),
            message: "HTTP 状态 403".into(),
        };
        set_status(&tx, a, &Status::Failed(forbidden.clone())).unwrap();
        set_status(&tx, b, &Status::Done).unwrap();
        tx.commit().unwrap();

        let tx = store.transaction().unwrap();
        let tasks = list(&tx).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].spec, spec(OutputSpec::Mp4(m("a.mp4"))));
        assert_eq!(tasks[0].work_dir.as_deref(), Some(m("a.hsdl").as_path()));
        assert_eq!(tasks[0].status, Status::Failed(forbidden));
        assert_eq!(tasks[1].status, Status::Done);
        assert_eq!(get(&tx, b).unwrap().unwrap().created_at, 200);
        delete(&tx, a).unwrap();
        assert!(matches!(delete(&tx, a), Err(Error::NotFound(_))));
        assert!(matches!(
            set_status(&tx, a, &Status::Done),
            Err(Error::NotFound(_))
        ));
        assert_eq!(get(&tx, a).unwrap(), None);
    }

    /// 每一种失败都写得进存储、读得回来（存储里的 CHECK 列着全部写法）。
    #[test]
    fn every_failure_kind_is_stored() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let id = add(&tx, OutputSpec::Mp4(m("a.mp4")), 1).unwrap();
        for kind in crate::failure::every() {
            let failed = Status::Failed(Failure {
                kind,
                message: "x".into(),
            });
            set_status(&tx, id, &failed).unwrap();
            assert_eq!(get(&tx, id).unwrap().unwrap().status, failed);
        }
    }

    #[test]
    fn interrupted_tasks_become_paused() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let running = add(&tx, OutputSpec::Hls(m("c")), 1).unwrap();
        let done = add(&tx, OutputSpec::Hls(m("d")), 2).unwrap();
        set_status(&tx, done, &Status::Done).unwrap();
        assert_eq!(pause_interrupted(&tx).unwrap(), 1);
        assert_eq!(get(&tx, running).unwrap().unwrap().status, Status::Paused);
        assert_eq!(get(&tx, done).unwrap().unwrap().status, Status::Done);
    }

    fn torrent_spec(info_hash: &str, root: PathBuf) -> TaskSpec {
        TaskSpec {
            title: "专辑".into(),
            url: format!("magnet:?xt=urn:btih:{info_hash}").parse().unwrap(),
            headers: Vec::new(),
            kind: Kind::Torrent(TorrentSpec {
                info_hash: info_hash.into(),
                root,
            }),
        }
    }

    /// BT 任务读回写入的内容，续传数据另读另写；内容的根算占用，同一个种子不能有两个任务；异常退出时还在运行的，续传
    /// 数据改为不可信。
    #[test]
    fn torrent_tasks_round_trip() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let spec = torrent_spec(hash, m("专辑"));
        let id = insert(&tx, &spec, None, Some(b"resume"), 1).unwrap();
        let task = get(&tx, id).unwrap().unwrap();
        assert_eq!((task.spec, task.work_dir), (spec, None));
        assert_eq!(
            torrent_resume(&tx, id).unwrap(),
            Resume {
                data: b"resume".to_vec(),
                trusted: true
            }
        );
        assert!(
            taken_paths(&tx)
                .unwrap()
                .contains(&naming::path_key(&text("专辑")))
        );
        assert!(matches!(
            insert(&tx, &torrent_spec(hash, m("别处")), None, Some(b"r"), 2),
            Err(Error::TorrentTaken(at)) if at == id
        ));
        let other = "1123456789abcdef0123456789abcdef01234567";
        assert!(matches!(
            insert(&tx, &torrent_spec(other, m("专辑")), None, Some(b"r"), 2),
            Err(Error::PathTaken(_))
        ));
        let paused = Resume {
            data: b"paused".to_vec(),
            trusted: true,
        };
        set_torrent_resume(&tx, id, &paused).unwrap();
        assert_eq!(torrent_resume(&tx, id).unwrap(), paused);
        assert_eq!(pause_interrupted(&tx).unwrap(), 1);
        assert_eq!(get(&tx, id).unwrap().unwrap().status, Status::Paused);
        assert!(!torrent_resume(&tx, id).unwrap().trusted);
        // 别的任务没有续传数据
        let hls = add(&tx, OutputSpec::Mp4(m("a.mp4")), 3).unwrap();
        assert!(matches!(torrent_resume(&tx, hls), Err(Error::NotFound(_))));
        assert!(matches!(
            set_torrent_resume(&tx, hls, &paused),
            Err(Error::NotFound(_))
        ));
    }

    /// BT 的四列必有且只用于它；它没有任务目录与请求头；编号是 40 个小写十六进制数字。
    #[test]
    fn torrent_columns_must_agree() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let hash = "'0123456789abcdef0123456789abcdef01234567'";
        let insert = |kind: &str, columns: &str, values: &str| {
            tx.execute(
                &format!(
                    "INSERT INTO task (kind, title, url, headers, status, created_at{columns})
                     VALUES ('{kind}', 'x', 'magnet:?', '[]', 'paused', 1{values})"
                ),
                [],
            )
        };
        let all =
            ", torrent_info_hash, torrent_root, torrent_resume, torrent_trusted, torrent_seeding";
        insert("torrent", all, &format!(", {hash}, '/m/a', x'00', 1, 0")).unwrap();
        tx.execute("DELETE FROM task", []).unwrap();
        for (kind, columns, values) in [
            ("torrent", all, ", '0123', '/m/a', x'00', 1, 0".to_owned()),
            // 没完成的不做种（插入的状态为暂停）
            ("torrent", all, format!(", {hash}, '/m/a', x'00', 1, 1")),
            (
                "torrent",
                all,
                ", '0123456789ABCDEF0123456789ABCDEF01234567', '/m/a', x'00', 1, 0".to_owned(),
            ),
            (
                "torrent",
                ", torrent_info_hash, torrent_root, torrent_trusted, torrent_seeding",
                format!(", {hash}, '/m/a', 1, 0"),
            ),
            (
                "torrent",
                &format!("{all}, work_dir"),
                format!(", {hash}, '/m/a', x'00', 1, 0, '/m/a.hsdl'"),
            ),
            (
                "hls",
                ", preference, output_kind, live, torrent_root",
                ", '{}', 'mp4', 0, '/m/a'".to_owned(),
            ),
        ] {
            assert!(insert(kind, columns, &values).is_err(), "{kind}{columns}");
        }
    }

    fn cloud_spec(job: Option<&str>, root: Option<PathBuf>) -> TaskSpec {
        TaskSpec {
            title: "Show".into(),
            url: "magnet:?xt=urn:btih:x".parse().unwrap(),
            headers: Vec::new(),
            kind: Kind::Cloud(CloudSpec {
                provider: CloudProvider::PikPak,
                user: "user-1".into(),
                job: job.map(str::to_owned),
                before: None,
                folder: m(""),
                root,
            }),
        }
    }

    fn cloud_file(id: &str, path: &str) -> CloudFile {
        CloudFile {
            id: id.into(),
            path: path.into(),
            size: 7,
            gcid: "6805ed701e6ea96597858628f8999a955d441d9d".into(),
            fetched: false,
            done: false,
            skipped: false,
        }
    }

    /// 云离线任务新建时还没提交、没有根与任务目录；提交了记下编号（只记一次），列出文件时根与任务目录一起定下（标题
    /// 换成根名）、占用路径；各文件先记取回再记下完，记为没取回时一并没下完，跳过与取回互斥；删任务连同文件。
    #[test]
    fn cloud_tasks_round_trip() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let id = insert(&tx, &cloud_spec(None, None), None, None, 1).unwrap();
        let task = get(&tx, id).unwrap().unwrap();
        assert_eq!(
            (task.spec.clone(), task.work_dir),
            (cloud_spec(None, None), None)
        );
        assert!(cloud_files(&tx, id).unwrap().is_empty());
        let files = [cloud_file("VA", "a.bin"), cloud_file("VB", "Sub/b.bin")];
        // 没提交就定不了根
        assert!(set_cloud_root(&tx, id, &m("节目"), &m("节目.hsdl"), &files).is_err());
        // 提交中的标记读得回；拿到编号即清掉
        set_cloud_before(&tx, id, &["VOld".to_owned()]).unwrap();
        let before = get(&tx, id).unwrap().unwrap();
        assert_eq!(
            before.spec.cloud().unwrap().before.as_deref(),
            Some(&["VOld".to_owned()][..])
        );
        set_cloud_job(&tx, id, "VGone").unwrap();
        assert_eq!(
            get(&tx, id).unwrap().unwrap().spec.cloud().unwrap().before,
            None
        );
        assert!(matches!(
            set_cloud_before(&tx, id, &[]),
            Err(Error::NotFound(_))
        ));
        // 网盘上的不在了：清掉编号，重新提交
        clear_cloud_job(&tx, id).unwrap();
        set_cloud_job(&tx, id, "VTask1").unwrap();
        assert!(matches!(
            set_cloud_job(&tx, id, "VTask2"),
            Err(Error::NotFound(_))
        ));
        // 没有文件、路径被占都不收
        assert!(matches!(
            set_cloud_root(&tx, id, &m("节目"), &m("节目.hsdl"), &[]),
            Err(Error::InvalidInput(_))
        ));
        let other = add(&tx, OutputSpec::Mp4(m("节目")), 2).unwrap();
        assert!(matches!(
            set_cloud_root(&tx, id, &m("节目"), &m("节目.hsdl"), &files),
            Err(Error::PathTaken(_))
        ));
        delete(&tx, other).unwrap();
        set_cloud_root(&tx, id, &m("节目"), &m("节目.hsdl"), &files).unwrap();
        let task = get(&tx, id).unwrap().unwrap();
        assert_eq!(
            task.spec,
            TaskSpec {
                title: "节目".into(),
                ..cloud_spec(Some("VTask1"), Some(m("节目")))
            }
        );
        assert_eq!(task.work_dir, Some(m("节目.hsdl")));
        assert!(
            taken_paths(&tx)
                .unwrap()
                .contains(&naming::path_key(&text("节目")))
        );
        // 只能定一次；定了根之后不再清编号
        assert!(matches!(
            set_cloud_root(&tx, id, &m("别处"), &m("别处.hsdl"), &files),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(clear_cloud_job(&tx, id), Err(Error::NotFound(_))));
        // 没取回的不能记为下完
        assert!(set_cloud_file_done(&tx, id, 1, true).is_err());
        set_cloud_file_fetched(&tx, id, 1, true).unwrap();
        set_cloud_file_done(&tx, id, 1, true).unwrap();
        let stored = cloud_files(&tx, id).unwrap();
        assert_eq!(stored[0], files[0]);
        assert_eq!(
            stored[1],
            CloudFile {
                fetched: true,
                done: true,
                ..files[1].clone()
            }
        );
        set_cloud_file_fetched(&tx, id, 1, false).unwrap();
        assert_eq!(cloud_files(&tx, id).unwrap()[1], files[1]);
        // 记了取回、输出却不在的也能跳过，一并记为没取回；跳过了的不能再记为取回
        set_cloud_file_fetched(&tx, id, 1, true).unwrap();
        set_cloud_file_skipped(&tx, id, 1).unwrap();
        assert!(set_cloud_file_fetched(&tx, id, 1, true).is_err());
        assert_eq!(
            cloud_files(&tx, id).unwrap()[1],
            CloudFile {
                skipped: true,
                ..files[1].clone()
            }
        );
        assert!(matches!(
            set_cloud_file_fetched(&tx, id, 2, true),
            Err(Error::NotFound(_))
        ));
        delete(&tx, id).unwrap();
        let left: i64 = tx
            .query_row("SELECT count(*) FROM cloud_file", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn cloud_files_must_be_distinct_and_hashed() {
        let mut store = Store::in_memory().unwrap();
        let id = {
            let tx = store.transaction().unwrap();
            let id = insert(&tx, &cloud_spec(Some("VTask1"), None), None, None, 1).unwrap();
            tx.commit().unwrap();
            id
        };
        let upper = CloudFile {
            gcid: "6805ED701E6EA96597858628F8999A955D441D9D".into(),
            ..cloud_file("VB", "b.bin")
        };
        for files in [
            [cloud_file("VA", "a.bin"), cloud_file("VB", "a.bin")],
            [cloud_file("VA", "a.bin"), upper],
        ] {
            // 出错时调用方不提交，事务回滚（同服务里的写法）
            let tx = store.transaction().unwrap();
            assert!(matches!(
                set_cloud_root(&tx, id, &m("节目"), &m("节目.hsdl"), &files),
                Err(Error::InvalidInput(_))
            ));
        }
    }

    /// 存储里的文件路径被改坏（越出内容的根、空段）时读不回来。
    #[test]
    fn cloud_file_paths_that_escape_the_root_are_refused() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let id = insert(&tx, &cloud_spec(Some("VTask1"), None), None, None, 1).unwrap();
        set_cloud_root(
            &tx,
            id,
            &m("节目"),
            &m("节目.hsdl"),
            &[cloud_file("VA", "a/b")],
        )
        .unwrap();
        for bad in ["../x", "a//b", "/x", "a/..", ".", "a\\b", "C:x", "a/C:x"] {
            tx.execute("UPDATE cloud_file SET path = ?1", [bad])
                .unwrap();
            assert!(
                matches!(cloud_files(&tx, id), Err(Error::Corrupt(_))),
                "{bad}"
            );
        }
    }

    /// 云离线的网盘、账号与保存目录必有且只用于它；离线任务编号可空，有根时必有；根与任务目录一起有；没有请求头。
    #[test]
    fn cloud_columns_must_agree() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let insert = |kind: &str, columns: &str, values: &str| {
            tx.execute(
                &format!(
                    "INSERT INTO task (kind, title, url, headers, status, created_at{columns})
                     VALUES ('{kind}', 'x', 'magnet:?', '[]', 'paused', 1{values})"
                ),
                [],
            )
        };
        let all = ", cloud_provider, cloud_user, cloud_folder";
        insert("cloud", all, ", 'pikpak', 'u', '/m'").unwrap();
        insert(
            "cloud",
            &format!("{all}, cloud_job, cloud_root, work_dir"),
            ", 'pikpak', 'u', '/m', 'VTask2', '/m/a', '/m/a.hsdl'",
        )
        .unwrap();
        tx.execute("DELETE FROM task", []).unwrap();
        for (kind, columns, values) in [
            (
                "cloud",
                ", cloud_provider, cloud_folder",
                ", 'pikpak', '/m'".to_owned(),
            ),
            ("cloud", all, ", 'baidu', 'u', '/m'".to_owned()),
            ("cloud", all, ", 'pikpak', '', '/m'".to_owned()),
            (
                "cloud",
                &format!("{all}, cloud_job"),
                ", 'pikpak', 'u', '/m', ''".to_owned(),
            ),
            // 有根却没提交
            (
                "cloud",
                &format!("{all}, cloud_root, work_dir"),
                ", 'pikpak', 'u', '/m', '/m/a', '/m/a.hsdl'".to_owned(),
            ),
            (
                "cloud",
                &format!("{all}, cloud_job, cloud_root"),
                ", 'pikpak', 'u', '/m', 'V', '/m/a'".to_owned(),
            ),
            (
                "cloud",
                &format!("{all}, work_dir"),
                ", 'pikpak', 'u', '/m', '/m/a.hsdl'".to_owned(),
            ),
            (
                "hls",
                ", preference, output_kind, live, cloud_user",
                ", '{}', 'mp4', 0, 'u'".to_owned(),
            ),
        ] {
            assert!(
                insert(kind, columns, &values).is_err(),
                "{kind}{columns}{values}"
            );
        }
        let headers = tx.execute(
            "INSERT INTO task (kind, title, url, headers, status, created_at, cloud_provider, cloud_user, cloud_folder)
             VALUES ('cloud', 'x', 'magnet:?', '[[\"a\",\"b\"]]', 'paused', 1, 'pikpak', 'u', '/m')",
            [],
        );
        assert!(headers.is_err());
    }

    fn web_spec(video: Option<&str>, audio: Option<&str>) -> TaskSpec {
        TaskSpec {
            title: "节目.mp4".into(),
            url: "https://www.youtube.com/watch?v=x".parse().unwrap(),
            headers: vec![("Cookie".into(), "SID=a".into())],
            kind: Kind::Web(WebSpec {
                extractor: "Youtube".into(),
                video_id: "x".into(),
                video: video.map(|id| FormatKey {
                    id: id.to_owned(),
                    ext: Some("mp4".into()),
                    video: Track::Codec("avc1.640028".into()),
                    audio: Track::Absent,
                    height: Some(1080),
                    language: None,
                    size: Some(123),
                }),
                audio: audio.map(|id| FormatKey {
                    id: id.to_owned(),
                    ext: Some("m4a".into()),
                    video: Track::Absent,
                    audio: Track::Codec("mp4a.40.2".into()),
                    height: None,
                    language: Some("ja".into()),
                    size: None,
                }),
                container: Container::Mp4,
                path: m("节目.mp4"),
            }),
        }
    }

    /// 网页视频任务写进去读得回；视频与音频都没选、没有任务目录的写不进；输出占着路径。
    #[test]
    fn web_tasks_round_trip() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let id = insert(
            &tx,
            &web_spec(Some("137"), Some("140")),
            Some(&m("节目.mp4.hsdl")),
            None,
            1,
        )
        .unwrap();
        let task = get(&tx, id).unwrap().unwrap();
        assert_eq!(task.spec, web_spec(Some("137"), Some("140")));
        assert_eq!(task.work_dir, Some(m("节目.mp4.hsdl")));
        assert!(
            taken_paths(&tx)
                .unwrap()
                .contains(&naming::path_key(&text("节目.mp4")))
        );
        assert!(matches!(
            insert(&tx, &web_spec(None, None), Some(&m("别的.hsdl")), None, 2),
            Err(Error::InvalidInput(_))
        ));
        assert!(matches!(
            insert(&tx, &web_spec(Some("18"), None), None, None, 2),
            Err(Error::InvalidInput(_))
        ));
        // 每种网页视频的失败都写得进、读得回
        for kind in [
            FailureKind::WebLogin,
            FailureKind::WebGeo,
            FailureKind::WebExtract,
            FailureKind::WebGone,
            FailureKind::WebChanged,
            FailureKind::WebDenied,
            FailureKind::WebBlocked,
            FailureKind::WebCookies,
            FailureKind::WebComponents,
        ] {
            let failure = Failure {
                kind,
                message: "m".into(),
            };
            set_status(&tx, id, &Status::Failed(failure.clone())).unwrap();
            assert_eq!(
                get(&tx, id).unwrap().unwrap().status,
                Status::Failed(failure)
            );
        }
    }

    /// 第 11 版的存储（含云离线任务与文件、网盘账号）升到第 12 版：行原样搬过来，编号接着数。
    #[test]
    fn rebuilding_for_web_keeps_rows_and_the_id_counter() {
        let dir = scratch("v11-rows");
        let path = dir.join("tasks.sqlite");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for migration in &MIGRATIONS[..11] {
                migration(&tx).unwrap();
            }
            tx.execute(
                "INSERT INTO task (kind, title, url, headers, cloud_provider, cloud_user, cloud_job, cloud_folder,
                                   status, created_at)
                 VALUES ('cloud', '节目', 'magnet:?xt=urn:btih:x', '[]', 'pikpak', 'u', 'VTask', ?1, 'paused', 3)",
                params![text("")],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO task (kind, title, url, headers, cloud_provider, cloud_user, cloud_folder, status,
                                   created_at)
                 VALUES ('cloud', '删了', 'magnet:?xt=urn:btih:y', '[]', 'pikpak', 'u', ?1, 'paused', 4)",
                params![text("")],
            )
            .unwrap();
            tx.execute("DELETE FROM task WHERE title = '删了'", [])
                .unwrap();
            tx.pragma_update(None, "user_version", 11).unwrap();
            tx.commit().unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let tx = store.transaction().unwrap();
        let tasks = list(&tx).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].spec.cloud().unwrap().job.as_deref(), Some("VTask"));
        let id = insert(
            &tx,
            &web_spec(Some("18"), None),
            Some(&m("节目.mp4.hsdl")),
            None,
            5,
        )
        .unwrap();
        assert_eq!(id, TaskId(3));
        drop(tx);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rebuilding_for_cloud_keeps_rows_and_the_id_counter() {
        let dir = scratch("v10-rows");
        let path = dir.join("tasks.sqlite");
        let hash = "0123456789abcdef0123456789abcdef01234567";
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for migration in &MIGRATIONS[..10] {
                migration(&tx).unwrap();
            }
            tx.execute(
                "INSERT INTO task (kind, title, url, headers, torrent_info_hash, torrent_root, torrent_resume,
                                   torrent_trusted, torrent_seeding, status, created_at, bytes)
                 VALUES ('torrent', '专辑', 'magnet:?xt=urn:btih:x', '[]', ?1, ?2, x'0102', 1, 1, 'done', 3, 9)",
                params![hash, text("专辑")],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO task (kind, title, url, headers, torrent_info_hash, torrent_root, torrent_resume,
                                   torrent_trusted, torrent_seeding, status, created_at)
                 VALUES ('torrent', '删了', 'magnet:?xt=urn:btih:y', '[]', ?1, ?2, x'00', 1, 0, 'paused', 4)",
                params!["1123456789abcdef0123456789abcdef01234567", text("删了")],
            )
            .unwrap();
            tx.execute("DELETE FROM task WHERE title = '删了'", [])
                .unwrap();
            tx.pragma_update(None, "user_version", 10).unwrap();
            tx.commit().unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let tx = store.transaction().unwrap();
        let tasks = list(&tx).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            (
                &tasks[0].spec.title,
                tasks[0].status.clone(),
                tasks[0].seeding
            ),
            (&"专辑".to_owned(), Status::Done, true)
        );
        assert_eq!(pikpak_account(&tx).unwrap(), None);
        // 删掉的任务的编号不再用
        let id = insert(&tx, &cloud_spec(None, None), None, None, 5).unwrap();
        assert_eq!(id, TaskId(3));
        drop(tx);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_pikpak_account_round_trips() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        assert_eq!(pikpak_account(&tx).unwrap(), None);
        let account = Account {
            user_id: "user-1".into(),
            device_id: "0123456789abcdef0123456789abcdef".into(),
            refresh_token: "r1".into(),
            access_token: "a1".into(),
            access_expires_at: 1_794_819_746,
        };
        set_pikpak_account(&tx, Some(&account)).unwrap();
        let renewed = Account {
            refresh_token: "r2".into(),
            access_token: "a2".into(),
            ..account
        };
        set_pikpak_account(&tx, Some(&renewed)).unwrap();
        assert_eq!(pikpak_account(&tx).unwrap(), Some(renewed));
        set_pikpak_account(&tx, None).unwrap();
        assert_eq!(pikpak_account(&tx).unwrap(), None);
    }

    /// 删掉编号最大的任务后，新任务也不拿它的编号。
    #[test]
    fn ids_are_not_reused() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let first = add(&tx, OutputSpec::Mp4(m("a.mp4")), 1).unwrap();
        delete(&tx, first).unwrap();
        let second = add(&tx, OutputSpec::Mp4(m("b.mp4")), 2).unwrap();
        assert!(second.0 > first.0, "{first:?} {second:?}");
    }

    /// 改过的存储里读不回的行报 [`Error::Corrupt`]：选轨偏好不是 JSON、输出是相对路径。
    #[test]
    fn rows_that_cannot_be_read_back_are_reported() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let preference = to_json(&StoredPreference::try_from(&Preference::default()).unwrap());
        for (preference, mp4) in [
            ("{}", text("x.mp4")),
            (preference.as_str(), "m/x.mp4".into()),
        ] {
            tx.execute(
                "INSERT INTO task (kind, title, url, preference, headers, output_kind, mp4_path, live, status,
                                   created_at)
                 VALUES ('hls', 'x', 'https://a/b.m3u8', ?1, '[]', 'mp4', ?2, 0, 'done', 1)",
                params![preference, mp4],
            )
            .unwrap();
            assert!(matches!(list(&tx), Err(Error::Corrupt(_))));
            tx.execute("DELETE FROM task", []).unwrap();
        }
    }

    /// 测试用的空目录，名字带进程号。
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-dlx-app-{name}-{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_store_from_a_newer_version_is_refused() {
        let dir = scratch("newer");
        let path = dir.join("tasks.sqlite");
        drop(Store::open(&path).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", schema_version(MIGRATIONS.len() + 1))
            .unwrap();
        assert!(matches!(Store::open(&path), Err(Error::Corrupt(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 第 1 版的存储：建表、插入 `rows`（标题、状态、MP4 路径），版本记为 1。
    fn version_1(path: &Path, rows: &[(&str, &str, &str)]) {
        let mut conn = Connection::open(path).unwrap();
        let tx = conn.transaction().unwrap();
        create_task_table(&tx).unwrap();
        for (title, status, mp4) in rows {
            tx.execute(
                "INSERT INTO task (title, url, preference, headers, output_kind, mp4_path, live, status, error, created_at)
                 VALUES (?1, 'https://a/b.m3u8', ?2, '[]', 'mp4', ?3, 0, ?4, ?5, 1)",
                params![
                    title,
                    to_json(&StoredPreference::try_from(&Preference::default()).unwrap()),
                    mp4,
                    status,
                    (*status == "failed").then_some("403")
                ],
            )
            .unwrap();
        }
        tx.pragma_update(None, "user_version", 1).unwrap();
        tx.commit().unwrap();
    }

    /// 第 1 版的存储升到当前版本：没完成的任务补上按输出算出的任务目录（之后新建的任务避开它），已完成的没有；已完成的
    /// 与没完成的同名不算冲突。没完成的两个任务的任务目录只差大小写时，编号小的留着它，另一个改为失败、没有任务目录。
    /// 迁移后的编号接着原有的最大编号分配，也不重用。
    #[test]
    fn version_1_rows_get_their_work_dir() {
        let dir = scratch("migrate");
        let path = dir.join("tasks.sqlite");
        version_1(
            &path,
            &[
                ("甲", "paused", &text("a.mp4")),
                ("乙", "done", &text("A.mp4")),
                ("丙", "failed", &text("A.mp4")),
                ("丁", "running", &text("d.mp4")),
            ],
        );
        let mut store = Store::open(&path).unwrap();
        let tx = store.transaction().unwrap();
        let taken = taken_paths(&tx).unwrap();
        assert!(
            taken.contains(&naming::path_key(&text("a.hsdl"))),
            "{taken:?}"
        );
        let tasks = list(&tx).unwrap();
        let work_dirs: Vec<Option<&Path>> =
            tasks.iter().map(|task| task.work_dir.as_deref()).collect();
        assert_eq!(
            work_dirs,
            [
                Some(m("a.hsdl").as_path()),
                None,
                None,
                Some(m("d.hsdl").as_path())
            ]
        );
        assert_eq!(tasks[1].status, Status::Done);
        assert!(
            matches!(&tasks[2].status, Status::Failed(f) if f.message.contains("「甲」")),
            "{:?}",
            tasks[2].status
        );
        assert_eq!((tasks[0].total, &tasks[1].result), (None, &None));
        let last = tasks[3].id;
        delete(&tx, last).unwrap();
        let next = add(&tx, OutputSpec::Mp4(m("e.mp4")), 2).unwrap();
        assert_eq!(next.0, last.0 + 1);
        // 迁移来的存储有一份没设过的设置
        assert!(settings(&tx).unwrap() == Settings::default());
        drop(tx);
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 设置读回写入的值；不是本应用写的代理写法由表拦下。
    #[test]
    fn settings_round_trip() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        assert!(settings(&tx).unwrap() == Settings::default());
        set_folder(&tx, &m("影片")).unwrap();
        let proxy = Proxy::Url(hs_m3u8_core::Url::parse("http://127.0.0.1:7890").unwrap());
        set_proxy(&tx, &proxy).unwrap();
        set_theme(&tx, Theme::BloodMoon).unwrap();
        let read = settings(&tx).unwrap();
        assert_eq!(read.folder.as_deref(), Some(m("影片").as_path()));
        assert!(read.proxy == proxy);
        assert_eq!(read.theme, Theme::BloodMoon);
        assert!(
            tx.execute("UPDATE settings SET proxy = 'pac' WHERE id = 1", [])
                .is_err()
        );
        assert!(
            tx.execute("UPDATE settings SET theme = 'aurora' WHERE id = 1", [])
                .is_err()
        );
        let empty = torrent_session(&tx).unwrap();
        assert_eq!((empty.port, empty.dht.is_empty()), (None, true));
        set_torrent_session(
            &tx,
            &SessionState {
                port: Some(51413),
                dht: b"dht".to_vec(),
            },
        )
        .unwrap();
        let saved = torrent_session(&tx).unwrap();
        assert_eq!((saved.port, saved.dht), (Some(51413), b"dht".to_vec()));
        // 经代理关闭时没有端口：存着的照旧
        set_torrent_session(
            &tx,
            &SessionState {
                port: None,
                dht: b"later".to_vec(),
            },
        )
        .unwrap();
        let kept = torrent_session(&tx).unwrap();
        assert_eq!((kept.port, kept.dht), (Some(51413), b"later".to_vec()));
        // BT 代理：存进去读得回；存储里认不出的报为损坏，不带出原文（可能有密码）
        let socks = Url::parse("socks5://me:secret@127.0.0.1:1080").unwrap();
        set_torrent_proxy(&tx, Some(&socks)).unwrap();
        assert_eq!(settings(&tx).unwrap().torrent_proxy, Some(socks));
        tx.execute(
            "UPDATE settings SET torrent_proxy = 'http://me:secret@127.0.0.1:1' WHERE id = 1",
            [],
        )
        .unwrap();
        let Err(corrupt) = settings(&tx) else {
            panic!("认不出的 BT 代理当作正常读出");
        };
        let corrupt = corrupt.to_string();
        assert!(
            corrupt.contains("BT 代理") && !corrupt.contains("secret"),
            "{corrupt}"
        );
        set_torrent_proxy(&tx, None).unwrap();
        assert_eq!(settings(&tx).unwrap().torrent_proxy, None);
        // 读 Cookie 的浏览器与用户资料一起存、一起清；有用户资料没有浏览器的由表拦下
        let source = hs_dlx_ytdlp::CookieSource {
            browser: hs_dlx_ytdlp::Browser::Chrome,
            profile: Some(m("Chrome/Profile 2")),
        };
        set_web_browser(&tx, Some(&source)).unwrap();
        assert_eq!(settings(&tx).unwrap().web_browser, Some(source));
        set_web_browser(&tx, None).unwrap();
        assert_eq!(settings(&tx).unwrap().web_browser, None);
        assert!(
            tx.execute(
                "UPDATE settings SET web_browser_profile = '/p' WHERE id = 1",
                []
            )
            .is_err()
        );
        // 做种：改成不做种再改回时分享率与时长照旧；只按时长
        assert_eq!(read.seeding, Seeding::default());
        set_seeding(&tx, Seeding::Off).unwrap();
        assert_eq!(settings(&tx).unwrap().seeding, Seeding::Off);
        tx.execute("UPDATE settings SET seed = 1 WHERE id = 1", [])
            .unwrap();
        assert_eq!(settings(&tx).unwrap().seeding, Seeding::default());
        let minutes_only = Seeding::Until {
            ratio_percent: None,
            minutes: NonZeroU32::new(90),
        };
        set_seeding(&tx, minutes_only).unwrap();
        assert_eq!(settings(&tx).unwrap().seeding, minutes_only);
        set_upload_limit(&tx, NonZeroU32::new(1 << 20)).unwrap();
        assert_eq!(
            settings(&tx).unwrap().upload_limit,
            NonZeroU32::new(1 << 20)
        );
        assert!(
            tx.execute("UPDATE settings SET upload_limit = 0 WHERE id = 1", [])
                .is_err()
        );
    }

    /// 同一份存储同时只能被一个进程（这里是同一进程里的第二个连接）打开；先打开的释放后才能再开。
    #[test]
    fn a_store_is_opened_by_one_app_at_a_time() {
        let dir = scratch("in-use");
        let path = dir.join("tasks.sqlite");
        let first = Store::open(&path).unwrap();
        assert!(matches!(Store::open(&path), Err(Error::InUse(_))));
        drop(first);
        Store::open(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 任务目录与已有任务相同的拒收：MP4 `a.mp4` 与本地 HLS `A` 的任务目录都是 `a.hsdl`（只差大小写）。记下总片数后
    /// 读得回。
    #[test]
    fn work_dirs_are_unique_and_totals_are_kept() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let a = add(&tx, OutputSpec::Mp4(m("a.mp4")), 1).unwrap();
        assert!(matches!(
            add(&tx, OutputSpec::Hls(m("A")), 2),
            Err(Error::PathTaken(path)) if path == m("A.hsdl")
        ));
        set_total(&tx, a, 664).unwrap();
        assert_eq!(get(&tx, a).unwrap().unwrap().total, Some(664));
        assert!(matches!(
            set_total(&tx, TaskId(99), 1),
            Err(Error::NotFound(_))
        ));
    }
}
