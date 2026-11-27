//! 云离线任务：提交磁力链接给网盘，等网盘下完，列出文件、定下内容的根，再逐个取直链交给核心分段下载，按网盘给的 GCID
//! 校验，全部通过才算完成。各文件下完即记进存储，暂停或失败后继续时只补没下完的。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hs_dlx_cloud::pikpak::{self, Client, Entry, Phase, Reason, Session, gcid::Gcid};
use hs_m3u8_core::{
    CancellationToken, FileOutputOptions, FileProgress, FileRequest, HttpErrorKind, HttpOptions,
    Leftover, Proxy, Source, WorkDirProblem,
};
use tokio::task::JoinHandle;

use super::{Event, Shared, lock, throttled};
use crate::naming::{self, CloudPlace};
use crate::store;
use crate::{CloudFile, CloudSpec, Error, Failure, FailureKind, Status, Task, TaskId, TaskSpec};

/// 网盘还在下时多久问一次进度。
const POLL: Duration = Duration::from_secs(5);
/// 一个文件的直链被拒时最多重取几次直链。
const RELINKS: u32 = 3;
/// 等网盘下完时，接口连着暂时失败（网络、5xx、请求太频繁）多久才算失败：几小时的离线下载里偶尔一次不算。
const POLL_TOLERANCE: Duration = Duration::from_secs(5 * 60);
/// 校验时每次读多少字节。
const VERIFY_CHUNK: usize = 1 << 20;
/// 网盘文件夹最多套几层：防网盘回的目录结构打转。
const MAX_DEPTH: usize = 32;

/// 运行中的云离线任务的进度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudProgress {
    /// 网盘在下：阶段与百分比（0 到 100）
    Remote { phase: CloudPhase, percent: u8 },
    /// 网盘下完了，在列文件
    Listing,
    /// 在下第 `index` 个文件（从 0 起），共 `count` 个；`done` 为之前的文件已下完的字节数，`total` 为全部文件的，
    /// `received` 为这次运行从网络读到的字节数（各文件合计，同 [`FileProgress::received`] 按它的变化算速度）
    Fetching {
        index: usize,
        count: usize,
        done: u64,
        total: u64,
        received: u64,
        file: Box<FileProgress>,
    },
    /// 第 `index` 个文件（保存位置 `output`）在网盘上被删了（或删进了回收站），跳过它
    Skipped { index: usize, output: PathBuf },
    /// 在校验第 `index` 个文件，已读 `checked` 字节；其余同 [`CloudProgress::Fetching`]
    Verifying {
        index: usize,
        count: usize,
        done: u64,
        total: u64,
        checked: u64,
    },
}

/// 网盘上的离线下载还没完成时在哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudPhase {
    /// 网盘收下了，在排队
    Queued,
    Running,
    /// 在网页或 App 里被暂停了，恢复后才接着下
    Paused,
}

/// 开始了的云离线任务。
pub(super) struct CloudRun {
    pub cancel: CancellationToken,
    /// 停下的结果，连同各文件收尾时核心没删掉的东西
    pub done: JoinHandle<(Outcome, Vec<Leftover>)>,
}

/// 云离线任务停下的结果。
pub(super) enum Outcome {
    /// 全部文件下完并校验过：字节数
    Finished(u64),
    Paused,
    Failed(Failure),
}

/// 网盘的失败翻译成任务的失败。
pub(super) fn failure(error: &pikpak::Error) -> Failure {
    let kind = match error {
        pikpak::Error::Transport(error) => FailureKind::of(error),
        pikpak::Error::Rejected(rejection) => match rejection.reason() {
            // 换过令牌、重申过 captcha 之后仍被拒
            Reason::LoginExpired | Reason::AccessExpired | Reason::Captcha => {
                FailureKind::CloudLogin
            }
            Reason::Quota => FailureKind::CloudQuota,
            Reason::RateLimited => FailureKind::Status(429),
            // 网盘或途中的网关出错，不是离线下载本身失败
            Reason::Other if rejection.status >= 500 => FailureKind::Status(rejection.status),
            Reason::Other => FailureKind::CloudFailed,
        },
        pikpak::Error::Unrecognized(_) => FailureKind::CloudApi,
        // 简化：要人机验证也按登录失效报出、在消息里给出验证页；真遇到时看验证后怎么接着用，再单列一种失败与操作
        pikpak::Error::Verification(_)
        | pikpak::Error::LoginExpired
        | pikpak::Error::LoginDenied
        | pikpak::Error::LoggedOut => FailureKind::CloudLogin,
        pikpak::Error::Save(_) => FailureKind::Disk,
        pikpak::Error::NoRandom(_) => FailureKind::Other,
    };
    let message = match error {
        pikpak::Error::Verification(url) => {
            format!("PikPak 要求人机验证：在浏览器里打开 {url} 通过后再继续")
        }
        error => error.to_string(),
    };
    Failure { kind, message }
}

impl Shared {
    /// PikPak 的登录状态：还没有时照存储里的账号建；没登录时报 [`Error::NotLoggedIn`]。调用方不能持着 `store` 锁
    /// （持着时用 [`Shared::pikpak_session_in`]）。
    pub(super) fn pikpak_session(self: &Arc<Self>) -> Result<Arc<Session>, Error> {
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        self.pikpak_session_in(&tx)
    }

    /// 同 [`Shared::pikpak_session`]，账号从调用方持着 `store` 锁开的事务 `tx` 读（锁的顺序：先 `store` 再 `pikpak`）。
    pub(super) fn pikpak_session_in(
        self: &Arc<Self>,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<Arc<Session>, Error> {
        let mut slot = lock(&self.pikpak);
        if let Some(session) = &*slot {
            return Ok(session.clone());
        }
        let account = store::pikpak_account(tx)?.ok_or(Error::NotLoggedIn)?;
        let created = self.new_pikpak_session(account);
        *slot = Some(created.clone());
        Ok(created)
    }

    /// 以 `account` 建 PikPak 的登录状态：换了的令牌写进存储；它已作废（退出登录、换了账号）时不写，那次换令牌失败。
    /// 是否作废在 `store` 锁内判，与作废它的 [`Shared::replace_pikpak`] 不会交错。
    pub(super) fn new_pikpak_session(self: &Arc<Self>, account: pikpak::Account) -> Arc<Session> {
        let shared = Arc::downgrade(self);
        let endpoints = self.pikpak_endpoints.clone();
        Arc::new_cyclic(|this: &std::sync::Weak<Session>| {
            let this = this.clone();
            Session::new(
                endpoints,
                account,
                Box::new(move |account| {
                    let shared = shared.upgrade().ok_or("应用正在退出")?;
                    shared
                        .write(|tx| {
                            if this.upgrade().is_none_or(|session| session.is_retired()) {
                                return Err(Error::NotLoggedIn);
                            }
                            store::set_pikpak_account(tx, Some(account))
                        })
                        .map_err(|error| error.to_string())
                }),
            )
        })
    }

    /// 换掉 PikPak 的登录状态（登录了新的为 Some，退出登录为 None），旧的作废、不再发请求与写存储。调用方持着 `store`
    /// 锁，已提交了存储里的账号（见 [`Shared::set_pikpak`]）。
    pub(super) fn replace_pikpak(&self, session: Option<Arc<Session>>) {
        let old = std::mem::replace(&mut *lock(&self.pikpak), session);
        if let Some(old) = old {
            old.retire();
        }
    }

    /// 经代理 `proxy` 调 PikPak 接口的客户端；`session` 为登录状态。
    pub(super) fn pikpak_client(&self, session: Arc<Session>, proxy: Proxy) -> Client {
        let http = HttpOptions {
            proxy,
            ..HttpOptions::default()
        };
        Client::new(self.engine.clone(), http, session)
    }

    /// 开始云离线任务 `task`（经代理 `proxy`，取直链与下载走同一出口），登录状态从调用方持着 `store` 锁开的事务 `tx`
    /// 读；没登录时报 [`Error::NotLoggedIn`]。调用方须在运行时里。
    pub(super) fn launch_cloud(
        self: &Arc<Self>,
        tx: &rusqlite::Transaction<'_>,
        task: &Task,
        proxy: Proxy,
    ) -> Result<CloudRun, Error> {
        let client = self.pikpak_client(self.pikpak_session_in(tx)?, proxy.clone());
        let cancel = CancellationToken::new();
        let shared = self.clone();
        let (task, token) = (task.clone(), cancel.clone());
        let done = tokio::spawn(async move {
            let leftovers = Mutex::new(Vec::new());
            let ctx = Ctx {
                client: &client,
                proxy: &proxy,
                cancel: &token,
                leftovers: &leftovers,
            };
            let outcome = match shared.run_cloud(&task, ctx).await {
                Ok(bytes) => Outcome::Finished(bytes),
                Err(Stop::Paused) => Outcome::Paused,
                Err(Stop::Failed(failure)) => Outcome::Failed(failure),
            };
            (outcome, lock(&leftovers).split_off(0))
        });
        Ok(CloudRun { cancel, done })
    }

    async fn run_cloud(self: &Arc<Self>, task: &Task, ctx: Ctx<'_>) -> Result<u64, Stop> {
        let id = task.id;
        let mut spec = task.spec.cloud().expect("只为云离线任务开始").clone();
        let user = ctx.client.session().user_id();
        if user != spec.user {
            return Err(Stop::failed(
                FailureKind::CloudLogin,
                format!(
                    "这个任务是用另一个 PikPak 账号（用户 {}）建的，现在登录的是用户 {user}：登录回那个账号再继续",
                    spec.user
                ),
            ));
        }
        if spec.job.is_none() {
            let job = self
                .submit_cloud(id, &task.spec.url, spec.before.take(), ctx)
                .await?;
            spec.job = Some(job);
        }
        let (root, work_dir, files) = match (&spec.root, &task.work_dir) {
            (Some(root), Some(work_dir)) => {
                let files = self
                    .write(|tx| store::cloud_files(tx, id))
                    .map_err(Stop::store)?;
                (root.clone(), work_dir.clone(), files)
            }
            (None, None) => self.list_cloud(id, &spec, ctx).await?,
            _ => unreachable!("存储的约束保证根与任务目录一起定下"),
        };
        let count = files.len();
        let mut total: u64 = files
            .iter()
            .filter(|file| !file.skipped)
            .map(|file| file.size)
            .sum();
        let mut done: u64 = files
            .iter()
            .filter(|file| file.done)
            .map(|file| file.size)
            .sum();
        let mut received = 0;
        let job = spec.job.as_deref().expect("列文件之前已提交");
        // 没跳过的文件数
        let mut kept = files.iter().filter(|file| !file.skipped).count();
        for (index, file) in files.iter().enumerate() {
            if file.done || file.skipped {
                continue;
            }
            let file_dir = work_dir.join(index.to_string());
            let output = output_of(&root, &file.path);
            let at = Position {
                id,
                index,
                count,
                done,
                total,
                received,
            };
            match (exists(&output).await.map_err(Stop::Failed)?, file.fetched) {
                // 上次取回、装上了输出，还没校验就停了
                (true, true) => {}
                // 没记上取回（装上输出后存储写不进、或正好退出）：内容与网盘的相符即认领、记为下完，不符的不是这个
                // 任务的、不删
                (true, false) => {
                    self.verify_cloud_file(at, file, &output, &file_dir, ctx.cancel, false)
                        .await?;
                    self.write(|tx| {
                        store::set_cloud_file_fetched(tx, id, index, true)?;
                        store::set_cloud_file_done(tx, id, index, true)
                    })
                    .map_err(Stop::store)?;
                    done += file.size;
                    continue;
                }
                (false, _) => match self
                    .fetch_cloud_file(at, file, &output, &file_dir, job, ctx)
                    .await?
                {
                    Some(read) => {
                        received += read;
                        self.write(|tx| store::set_cloud_file_fetched(tx, id, index, true))
                            .map_err(Stop::store)?;
                    }
                    None => {
                        self.reset_file_dir(&file_dir, &output, ctx.cancel).await?;
                        self.write(|tx| store::set_cloud_file_skipped(tx, id, index))
                            .map_err(Stop::store)?;
                        self.cloud_progress(
                            id,
                            CloudProgress::Skipped {
                                index,
                                output: output.clone(),
                            },
                        );
                        total -= file.size;
                        kept -= 1;
                        continue;
                    }
                },
            }
            self.verify_cloud_file(at, file, &output, &file_dir, ctx.cancel, true)
                .await?;
            self.write(|tx| store::set_cloud_file_done(tx, id, index, true))
                .map_err(Stop::store)?;
            done += file.size;
        }
        if kept == 0 {
            return Err(Stop::failed(
                FailureKind::Unrecoverable,
                "网盘上下到的文件都被删了，没有可取回的：只能放弃".to_owned(),
            ));
        }
        // 各文件的任务目录装上输出后已删，剩下空的父目录
        let removed = tokio::fs::remove_dir(&work_dir).await;
        if let Err(error) = removed
            && error.kind() != std::io::ErrorKind::NotFound
        {
            // 输出都已校验过，留下的空目录不影响结果
            self.events.send(Event::Orphaned {
                paths: vec![work_dir],
                reason: format!("删不掉：{error}"),
            });
        }
        Ok(total)
    }

    /// 等网盘下完，列出文件，定下内容的根与任务目录并记进存储；返回根、任务目录与文件。
    async fn list_cloud(
        self: &Arc<Self>,
        id: TaskId,
        spec: &CloudSpec,
        ctx: Ctx<'_>,
    ) -> Result<(PathBuf, PathBuf, Vec<CloudFile>), Stop> {
        let (client, cancel) = (ctx.client, ctx.cancel);
        let job_id = spec.job.as_deref().expect("列文件之前已提交");
        // 接口连着暂时失败的起点；成功一次即清掉
        let mut failing: Option<tokio::time::Instant> = None;
        let job = loop {
            let job = match until(cancel, client.job(job_id)).await? {
                Ok(job) => {
                    failing = None;
                    job
                }
                Err(error) if transient(&error) => {
                    let since = *failing.get_or_insert_with(tokio::time::Instant::now);
                    if since.elapsed() >= POLL_TOLERANCE {
                        return Err(Stop::cloud(error));
                    }
                    until(cancel, tokio::time::sleep(POLL)).await?;
                    continue;
                }
                Err(error) => return Err(Stop::cloud(error)),
            };
            let Some(job) = job else {
                self.write(|tx| store::clear_cloud_job(tx, id))
                    .map_err(Stop::store)?;
                return Err(Stop::failed(
                    FailureKind::CloudFailed,
                    "网盘上的这个离线任务不在了（可能在网页或 App 里删了）：继续即重新提交（再用一次离线次数）"
                        .to_owned(),
                ));
            };
            let phase = match job.phase {
                Phase::Pending => Some(CloudPhase::Queued),
                Phase::Running => Some(CloudPhase::Running),
                Phase::Paused => Some(CloudPhase::Paused),
                Phase::Complete | Phase::Failed => None,
            };
            if let Some(phase) = phase {
                self.cloud_progress(
                    id,
                    CloudProgress::Remote {
                        phase,
                        percent: job.progress,
                    },
                );
            }
            match job.phase {
                Phase::Complete => break job,
                Phase::Failed => {
                    let reason = if job.message.is_empty() {
                        "网盘没有说原因".to_owned()
                    } else {
                        job.message
                    };
                    self.write(|tx| store::clear_cloud_job(tx, id))
                        .map_err(Stop::store)?;
                    return Err(Stop::failed(
                        FailureKind::CloudFailed,
                        format!(
                            "网盘上的离线下载失败了：{reason}；继续即重新提交（再用一次离线次数）"
                        ),
                    ));
                }
                Phase::Pending | Phase::Running | Phase::Paused => {
                    until(cancel, tokio::time::sleep(POLL)).await?;
                }
            }
        };
        self.cloud_progress(id, CloudProgress::Listing);
        let top_id = job.file.ok_or_else(|| {
            Stop::failed(
                FailureKind::CloudApi,
                "网盘说离线下载完成了，却没给出下到的文件".to_owned(),
            )
        })?;
        let top = match until(cancel, client.entry(&top_id)).await? {
            Ok(top) if !top.trashed => top,
            Err(error) if !maybe_gone(&error) => return Err(Stop::cloud(error)),
            // 在回收站里，或可能删了
            gone => {
                self.write(|tx| store::clear_cloud_job(tx, id))
                    .map_err(Stop::store)?;
                let reason = gone
                    .err()
                    .map_or(String::new(), |error| format!("（{error}）"));
                return Err(Stop::failed(
                    FailureKind::CloudFailed,
                    format!(
                        "网盘上下到的内容不在了，可能在网页或 App 里删了{reason}：继续即重新提交（再用一次离线次数）"
                    ),
                ));
            }
        };
        let mut found = Vec::new();
        if top.folder {
            until(cancel, walk(client, &top, &mut Vec::new(), &mut found))
                .await?
                .map_err(Stop::cloud)?;
        } else {
            found.push((Vec::new(), top.clone()));
        }
        // 用户在网页或 App 里删进回收站的（多为广告文件）不要
        found.retain(|(_, entry)| !entry.trashed);
        if found.is_empty() {
            return Err(Stop::failed(
                FailureKind::CloudFailed,
                "网盘上下到的文件夹是空的（或都在回收站里）：从回收站恢复后继续，或放弃".to_owned(),
            ));
        }
        let missing: Vec<&str> = found
            .iter()
            .filter(|(_, entry)| !entry.complete)
            .map(|(_, entry)| entry.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(Stop::failed(
                FailureKind::CloudFailed,
                format!(
                    "网盘上有 {} 个文件没下完整：{}。在网页或 App 里把它们删进回收站后继续，只取回其余的",
                    missing.len(),
                    missing.join("、")
                ),
            ));
        }
        let places: Vec<CloudPlace> = found
            .iter()
            .map(|(folders, entry)| CloudPlace {
                folders: folders.clone(),
                file: (entry.name.clone(), entry.id.clone()),
            })
            .collect();
        let paths = if top.folder {
            naming::cloud_paths(&places)
        } else {
            vec![String::new()]
        };
        let files: Vec<CloudFile> = found
            .into_iter()
            .zip(paths)
            .map(|((_, entry), path)| CloudFile {
                id: entry.id,
                path,
                size: entry.size,
                gcid: entry.gcid.expect("文件都有 GCID（读出时已核对）"),
                fetched: false,
                done: false,
                skipped: false,
            })
            .collect();
        // 取名与写入之间别的任务可能占了同一个根：写入时报 PathTaken 就重新取名（同新建）
        loop {
            let taken = self.write(store::taken_paths).map_err(Stop::store)?;
            let seen = taken.clone();
            let (folder, name, single, fallback) = (
                spec.folder.clone(),
                top.name.clone(),
                !top.folder,
                job_id.to_owned(),
            );
            let (_, root) = super::named(&spec.folder, move || {
                naming::cloud_root_for(&folder, &name, single, &fallback, &taken)
            })
            .await
            .map_err(Stop::store)?;
            let work_dir = naming::file_work_dir(&root);
            // 标题换成了根名
            let rooted = self.change(|tx| {
                store::set_cloud_root(tx, id, &root, &work_dir, &files)?;
                store::get(tx, id)?.ok_or(Error::NotFound(id))
            });
            match rooted {
                Ok(_) => return Ok((root, work_dir, files)),
                Err(Error::PathTaken(path))
                    if !seen.contains(&naming::path_key(&path.to_string_lossy())) => {}
                Err(error) => return Err(Stop::store(error)),
            }
        }
    }

    /// 取直链、交给核心下载第 `at.index` 个文件到 `output`（任务目录 `file_dir`），返回从网络读到的字节数（各次合计）；
    /// 用户在网盘上删了它（离线任务 `job` 下到的内容里已没有它，或它在回收站里）时为 None。
    /// 以网盘的文件编号加 GCID 担保内容身份，直链每次签名不同也沿用已下的范围（内容对不对由之后的 GCID 校验核对）。
    /// 直链被拒时重取；接连 [`RELINKS`] 个直链一个字节没下到就被拒时以云端失败报出（可再继续）。任务目录续不了（记录
    /// 与这次的对不上）时删掉它从头下一次：目录只属于这个文件，内容有 GCID 把关。
    async fn fetch_cloud_file(
        self: &Arc<Self>,
        at: Position,
        file: &CloudFile,
        output: &Path,
        file_dir: &Path,
        job: &str,
        ctx: Ctx<'_>,
    ) -> Result<Option<u64>, Stop> {
        let (client, proxy, cancel) = (ctx.client, ctx.proxy, ctx.cancel);
        let mut relinked = 0;
        let mut reset = false;
        // 之前几次（直链被拒前）读到的
        let mut received = 0;
        loop {
            let entry = match until(cancel, client.entry(&file.id)).await? {
                Ok(entry) if entry.trashed => return Ok(None),
                Ok(entry) => entry,
                Err(error) => {
                    if maybe_gone(&error) && !self.still_listed(job, &file.id, ctx).await? {
                        return Ok(None);
                    }
                    return Err(Stop::cloud(error));
                }
            };
            let link = entry.link.ok_or_else(|| {
                Stop::failed(
                    FailureKind::CloudApi,
                    format!("网盘没给出 {} 的直链", file.path),
                )
            })?;
            let mut source = Source::new(link);
            source.http.proxy = proxy.clone();
            let mut options = FileOutputOptions::new(output.to_path_buf());
            options.work_dir = Some(file_dir.to_path_buf());
            let mut request = FileRequest::new(source, options);
            request.identity = Some(format!("pikpak:{}:{}", file.id, file.gcid));
            request.expected = Some(hs_m3u8_core::Expected {
                length: Some(file.size),
                ..hs_m3u8_core::Expected::default()
            });
            let job = self.engine.start_file(request).map_err(Stop::core)?;
            let control = job.control();
            let finished = CancellationToken::new();
            let events = self.events.clone();
            let before = at.received + received;
            let forwarding = tokio::spawn(throttled(
                control.progress(),
                finished.clone(),
                move |progress: FileProgress| {
                    events.send(Event::CloudProgress {
                        id: at.id,
                        progress: Box::new(CloudProgress::Fetching {
                            index: at.index,
                            count: at.count,
                            done: at.done,
                            total: at.total,
                            received: before + progress.received,
                            file: Box::new(progress),
                        }),
                    });
                },
            ));
            let waiting = job.wait();
            tokio::pin!(waiting);
            let result = tokio::select! {
                result = &mut waiting => result,
                () = cancel.cancelled() => {
                    control.cancel();
                    waiting.await
                }
            };
            finished.cancel();
            forwarding
                .await
                .expect("转发进度的任务不会 panic，运行时关闭时本任务也一起丢弃");
            let read = control.progress().borrow().received;
            received += read;
            let error = match result {
                Ok(fetched) => {
                    lock(ctx.leftovers).extend(fetched.leftovers);
                    return Ok(Some(received));
                }
                Err(hs_m3u8_core::Error::Cancelled) => return Err(Stop::Paused),
                Err(error) => error,
            };
            if unresumable(&error) {
                if reset {
                    // 同一次里重下后仍核对不上（多为各连接落到了给出不同验证器的节点）：再继续时还会重下
                    return Err(Stop::failed(
                        FailureKind::CloudFailed,
                        format!("{} 的直链重下后仍核对不上：{error}", output.display()),
                    ));
                }
                reset = true;
                self.reset_file_dir(file_dir, output, cancel).await?;
                continue;
            }
            if !link_rejected(&error) {
                return Err(Stop::core(error));
            }
            // 这个直链下到过东西（过期前在正常续传）时重新计数：只有接连被拒才算下不了
            if read > 0 {
                relinked = 0;
            }
            if relinked == RELINKS {
                return Err(Stop::failed(
                    FailureKind::CloudFailed,
                    format!(
                        "{} 的直链接连重取了 {RELINKS} 次仍下不了：{error}",
                        output.display()
                    ),
                ));
            }
            relinked += 1;
        }
    }

    /// 按网盘给的 GCID 校验第 `at.index` 个文件 `output`。不符时：`owned`（这个任务取回的）删掉它、记为没取回，以
    /// [`FailureKind::Integrity`] 失败，删不掉的以磁盘一类的失败报出；不是这个任务取回的不删，以
    /// [`FailureKind::OutputTaken`] 失败。
    async fn verify_cloud_file(
        self: &Arc<Self>,
        at: Position,
        file: &CloudFile,
        output: &Path,
        file_dir: &Path,
        cancel: &CancellationToken,
        owned: bool,
    ) -> Result<(), Stop> {
        let (path, size, cancelled) = (output.to_path_buf(), file.size, cancel.clone());
        let events = self.events.clone();
        let checked: Result<String, u64> = tokio::task::spawn_blocking(move || {
            gcid_of(&path, size, &cancelled, |checked| {
                events.send(Event::CloudProgress {
                    id: at.id,
                    progress: Box::new(CloudProgress::Verifying {
                        index: at.index,
                        count: at.count,
                        done: at.done,
                        total: at.total,
                        checked,
                    }),
                });
            })
        })
        .await
        .expect("校验不会 panic，运行时关闭时本任务也一起丢弃")?;
        let what = match checked {
            Ok(gcid) if gcid == file.gcid => return Ok(()),
            Ok(gcid) => format!("与网盘给的 GCID 不符（网盘 {}，下到的 {gcid}）", file.gcid),
            Err(read) => format!("有 {read} 字节，网盘上是 {} 字节", file.size),
        };
        if !owned {
            return Err(Stop::failed(
                FailureKind::OutputTaken,
                format!(
                    "保存位置上已有 {}，{what}，不是这个任务取回的：挪走后再继续",
                    output.display()
                ),
            ));
        }
        if let Err(error) = tokio::fs::remove_file(output).await {
            return Err(Stop::failed(
                FailureKind::Disk,
                format!(
                    "{} {what}，删不掉（{error}）：先手动删掉它再继续",
                    output.display()
                ),
            ));
        }
        // 留着的任务目录（收尾没删掉的）会让重下时原样装回同一份内容
        self.reset_file_dir(file_dir, output, cancel).await?;
        self.write(|tx| store::set_cloud_file_fetched(tx, at.id, at.index, false))
            .map_err(Stop::store)?;
        Err(Stop::failed(
            FailureKind::Integrity,
            format!("{} {what}，已删掉，继续时重下这个文件", output.display()),
        ))
    }

    /// 把磁力链接 `link` 提交给网盘，记下网盘给的离线任务编号并返回。发出前先在存储里记下提交前网盘上已有的同一链接
    /// 的任务（`before`，上次提交结果不明时为那次记下的）：结果不明（发出后断开、没读完响应、网关出错、退出）时，下次
    /// 按它找之后新建的那个（别的本机任务记着的除外），找到即用它，不重复提交。
    async fn submit_cloud(
        self: &Arc<Self>,
        id: TaskId,
        link: &hs_m3u8_core::Url,
        before: Option<Vec<String>>,
        ctx: Ctx<'_>,
    ) -> Result<String, Stop> {
        let (client, cancel) = (ctx.client, ctx.cancel);
        let link = link.as_str();
        let job = match before {
            // 上次提交结果不明
            Some(before) => match self.find_submitted(id, link, &before, ctx).await? {
                Some(job) => job,
                None => self.submit_once(id, link, &before, ctx).await?,
            },
            None => {
                let before = until(cancel, client.job_ids_for(link))
                    .await?
                    .map_err(Stop::cloud)?;
                self.write(|tx| store::set_cloud_before(tx, id, &before))
                    .map_err(Stop::store)?;
                self.submit_once(id, link, &before, ctx).await?
            }
        };
        self.write(|tx| store::set_cloud_job(tx, id, &job.id))
            .map_err(Stop::store)?;
        Ok(job.id)
    }

    /// 提交一次（提交中的标记已记下）；结果不明时在 `before` 之外找新建的那个，找不到照原样报错（标记留着，下次再找）。
    async fn submit_once(
        &self,
        id: TaskId,
        link: &str,
        before: &[String],
        ctx: Ctx<'_>,
    ) -> Result<pikpak::Job, Stop> {
        let (client, cancel) = (ctx.client, ctx.cancel);
        let error = match until(cancel, client.submit(link)).await? {
            Ok(job) => return Ok(job),
            Err(error) => error,
        };
        let unclear = match &error {
            pikpak::Error::Transport(error) => super::may_have_taken_effect(error),
            // 网关出错时网盘可能已收下
            pikpak::Error::Rejected(rejection) => rejection.status >= 500,
            _ => false,
        };
        if !unclear {
            return Err(Stop::cloud(error));
        }
        match self.find_submitted(id, link, before, ctx).await? {
            Some(job) => Ok(job),
            None => Err(Stop::cloud(error)),
        }
    }

    /// 结果不明的那次提交建的离线任务：提交的是 `link`、不在 `before` 里、也不是别的本机任务记着的；没有时为 None。
    async fn find_submitted(
        &self,
        id: TaskId,
        link: &str,
        before: &[String],
        ctx: Ctx<'_>,
    ) -> Result<Option<pikpak::Job>, Stop> {
        let mut known = self
            .write(|tx| store::cloud_jobs_except(tx, id))
            .map_err(Stop::store)?;
        known.extend_from_slice(before);
        until(ctx.cancel, ctx.client.find_job(link, &known))
            .await?
            .map_err(Stop::cloud)
    }

    /// 删掉文件 `output` 的任务目录 `file_dir`（没有时什么也不做）；删不掉时以磁盘一类的失败报出。
    async fn reset_file_dir(
        &self,
        file_dir: &Path,
        output: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), Stop> {
        let leftovers = self
            .engine
            .discard(file_dir, hs_m3u8_core::Unsettled::Fail, cancel)
            .await
            .map_err(Stop::core)?;
        match leftovers.first() {
            None => Ok(()),
            Some(leftover) => Err(Stop::failed(
                FailureKind::Disk,
                format!("{} 的任务目录删不掉：{leftover}", output.display()),
            )),
        }
    }

    /// 离线任务 `job` 下到的内容里还有不在回收站的文件 `file_id`；查不清（任务或下到的内容本身取不到）时为 true，由
    /// 调用方照原样报错，不当作删了。
    async fn still_listed(&self, job: &str, file_id: &str, ctx: Ctx<'_>) -> Result<bool, Stop> {
        let (client, cancel) = (ctx.client, ctx.cancel);
        let Some(top_id) = until(cancel, client.job(job))
            .await?
            .map_err(Stop::cloud)?
            .and_then(|job| job.file)
        else {
            return Ok(true);
        };
        let top = match until(cancel, client.entry(&top_id)).await? {
            Ok(top) => top,
            // 下到的内容整个删了
            Err(error) if maybe_gone(&error) => return Ok(false),
            Err(_) => return Ok(true),
        };
        if top.trashed {
            return Ok(false);
        }
        if !top.folder {
            return Ok(top.id == file_id);
        }
        let mut found = Vec::new();
        until(cancel, walk(client, &top, &mut Vec::new(), &mut found))
            .await?
            .map_err(Stop::cloud)?;
        Ok(found
            .iter()
            .any(|(_, entry)| entry.id == file_id && !entry.trashed))
    }

    fn cloud_progress(&self, id: TaskId, progress: CloudProgress) {
        self.events.send(Event::CloudProgress {
            id,
            progress: Box::new(progress),
        });
    }

    /// 放弃云离线任务的文件：各文件的核心任务目录、已装上的输出，再删空了的目录（内容的根、子目录与任务目录）；
    /// 删不掉、或里面有不是这个任务的东西时报 [`Error::ContentLeft`]。
    pub(super) async fn discard_cloud(
        self: &Arc<Self>,
        id: TaskId,
        spec: &CloudSpec,
        work_dir: Option<&Path>,
    ) -> Result<(), Error> {
        let (Some(root), Some(work_dir)) = (&spec.root, work_dir) else {
            return Ok(());
        };
        let files = self.write(|tx| store::cloud_files(tx, id))?;
        let left = |reason: String| Error::ContentLeft {
            id,
            cause: Box::new(Error::InvalidInput(reason)),
        };
        for index in 0..files.len() {
            let dir = work_dir.join(index.to_string());
            let leftovers = self
                .engine
                .discard(
                    &dir,
                    hs_m3u8_core::Unsettled::Fail,
                    &CancellationToken::new(),
                )
                .await?;
            if let Some(leftover) = leftovers.first() {
                return Err(left(leftover.to_string()));
            }
        }
        // 只删这个任务取回的；没取回的位置上即使有东西也不是它的
        for file in files.iter().filter(|file| file.fetched) {
            let output = output_of(root, &file.path);
            match tokio::fs::remove_file(&output).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(left(format!("删不掉 {}：{error}", output.display()))),
            }
        }
        // 由深到浅删空了的目录：先各文件所在的子目录，再内容的根（单文件时根就是文件，上面已删）与任务目录
        let mut dirs: Vec<PathBuf> = files
            .iter()
            .filter(|file| !file.path.is_empty())
            .flat_map(|file| {
                let output = output_of(root, &file.path);
                output
                    .ancestors()
                    .skip(1)
                    .take_while(|dir| dir.starts_with(root))
                    .map(Path::to_path_buf)
                    .collect::<Vec<_>>()
            })
            .collect();
        dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
        dirs.dedup();
        dirs.push(work_dir.to_path_buf());
        for dir in dirs {
            match tokio::fs::remove_dir(&dir).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(left(format!(
                        "删不掉 {}（里面可能有不是这个任务的文件）：{error}",
                        dir.display()
                    )));
                }
            }
        }
        Ok(())
    }
}

/// 一次运行里各步共用的：调网盘的客户端、下载经的代理（与取直链同一出口）、暂停的信号。
#[derive(Clone, Copy)]
struct Ctx<'a> {
    client: &'a Client,
    proxy: &'a Proxy,
    cancel: &'a CancellationToken,
    /// 各文件收尾时核心没删掉的东西，任务停下时一并报出
    leftovers: &'a Mutex<Vec<Leftover>>,
}

/// 一个文件在任务里的位置，报进度用。
#[derive(Debug, Clone, Copy)]
struct Position {
    id: TaskId,
    index: usize,
    count: usize,
    done: u64,
    total: u64,
    /// 这次运行之前的文件从网络读到的字节数
    received: u64,
}

/// 云离线任务没跑完的原因。
enum Stop {
    Paused,
    Failed(Failure),
}

impl Stop {
    fn failed(kind: FailureKind, message: String) -> Stop {
        Stop::Failed(Failure { kind, message })
    }

    fn cloud(error: pikpak::Error) -> Stop {
        Stop::Failed(failure(&error))
    }

    /// 核心报的错误；取消（暂停）为 [`Stop::Paused`]。
    fn core(error: hs_m3u8_core::Error) -> Stop {
        match error {
            hs_m3u8_core::Error::Cancelled => Stop::Paused,
            error => Stop::Failed(Failure::of(&error)),
        }
    }

    /// 存储读写或取名出错：任务停下，以磁盘一类的失败报出（处理后可再继续）。
    fn store(error: Error) -> Stop {
        Stop::failed(FailureKind::Disk, error.to_string())
    }
}

/// 等 `future`，`cancel` 先到时为 [`Stop::Paused`]。
async fn until<T>(cancel: &CancellationToken, future: impl Future<Output = T>) -> Result<T, Stop> {
    cancel.run_until_cancelled(future).await.ok_or(Stop::Paused)
}

/// 递归列出网盘文件夹 `folder` 里的文件，连同各自所在的各级文件夹（`path`，名字与编号）放进 `found`。
async fn walk(
    client: &Client,
    folder: &Entry,
    path: &mut Vec<(String, String)>,
    found: &mut Vec<(Vec<(String, String)>, Entry)>,
) -> Result<(), pikpak::Error> {
    if path.len() >= MAX_DEPTH {
        return Err(pikpak::Error::Unrecognized(format!(
            "文件夹套了 {MAX_DEPTH} 层以上"
        )));
    }
    for entry in client.children(&folder.id).await? {
        if entry.folder && entry.trashed {
            // 删进回收站的文件夹：里面的一概不要
        } else if entry.folder {
            path.push((entry.name.clone(), entry.id.clone()));
            Box::pin(walk(client, &entry, path, found)).await?;
            path.pop();
        } else {
            found.push((path.clone(), entry));
        }
    }
    Ok(())
}

/// 文件在内容的根 `root` 下的位置：`path` 为空时就是根。
fn output_of(root: &Path, path: &str) -> PathBuf {
    if path.is_empty() {
        root.to_path_buf()
    } else {
        path.split('/')
            .fold(root.to_path_buf(), |dir, part| dir.join(part))
    }
}

/// `path` 上已有东西（上次下完、装上了输出，还没校验就停了）。
async fn exists(path: &Path) -> Result<bool, Failure> {
    tokio::fs::try_exists(path).await.map_err(|error| Failure {
        kind: FailureKind::Disk,
        message: format!("查不了 {}：{error}", path.display()),
    })
}

/// 网盘自己（带错误名，不是途中的网关、代理回的）拒绝得像是东西不在了：4xx，不是登录、额度、频率一类。
fn maybe_gone(error: &pikpak::Error) -> bool {
    matches!(
        error,
        pikpak::Error::Rejected(rejection)
            if (400..500).contains(&rejection.status)
                && !rejection.name.is_empty()
                && rejection.reason() == Reason::Other
    )
}

/// 文件的任务目录续不了：记录与这次的对不上（同一直链换了验证器、总长度变了、核对不了是同一份）。记录读不了的
/// （坏了、是更新的版本写的）放弃也删不掉，照原样报出。
fn unresumable(error: &hs_m3u8_core::Error) -> bool {
    matches!(
        error,
        hs_m3u8_core::Error::WorkDir {
            problem: WorkDirProblem::FileChanged | WorkDirProblem::SourceUnverified,
            ..
        }
    )
}

/// 直链被拒：过期了或换了出口，重取一个再下。
fn link_rejected(error: &hs_m3u8_core::Error) -> bool {
    match error {
        hs_m3u8_core::Error::Http {
            kind: HttpErrorKind::Status(status),
            ..
        } => matches!(status, 401 | 403 | 410),
        hs_m3u8_core::Error::NotExpected { .. } => true,
        _ => false,
    }
}

/// 读 `path` 算 GCID（总长 `size` 字节），每读一块调 `checked`（已读字节数）：长度是 `size` 时为 Ok(GCID)，不是时为
/// Err(实际字节数)。`cancel` 取消时为 [`Stop::Paused`]，读不了时以磁盘一类的失败报出。
fn gcid_of(
    path: &Path,
    size: u64,
    cancel: &CancellationToken,
    mut checked: impl FnMut(u64),
) -> Result<Result<String, u64>, Stop> {
    use std::io::Read as _;
    let disk = |error: std::io::Error| {
        Stop::failed(
            FailureKind::Disk,
            format!("读不了 {}：{error}", path.display()),
        )
    };
    let mut reader = std::fs::File::open(path).map_err(disk)?;
    let mut gcid = Gcid::new(size);
    let mut buffer = vec![0; VERIFY_CHUNK];
    let mut read: u64 = 0;
    loop {
        if cancel.is_cancelled() {
            return Err(Stop::Paused);
        }
        let n = reader.read(&mut buffer).map_err(disk)?;
        if n == 0 {
            break;
        }
        read += n as u64;
        gcid.update(&buffer[..n]);
        checked(read);
    }
    if read != size {
        return Ok(Err(read));
    }
    Ok(Ok(gcid.finish()))
}

/// 暂时的失败：网络（核心判为可重试的）、网盘或网关出错（5xx）、请求太频繁；过一阵再试可能就好。
fn transient(error: &pikpak::Error) -> bool {
    match error {
        pikpak::Error::Transport(error) => error.retryable(),
        pikpak::Error::Rejected(rejection) => rejection.transient(),
        _ => false,
    }
}

/// 新建云离线任务时的规格：标题取磁力链接的显示名（`dn`，整理成文件主名），没有或什么都不剩时为「磁力链接」；列出
/// 文件、定下根后换成根名。
pub(super) fn draft_spec(spec: CloudSpec, url: hs_m3u8_core::Url) -> TaskSpec {
    let name = url
        .query_pairs()
        .find(|(key, _)| key == "dn")
        .map(|(_, name)| naming::clean_title(&name))
        .unwrap_or_default();
    let title = if name.is_empty() {
        "磁力链接".to_owned()
    } else {
        name
    };
    TaskSpec {
        title,
        url,
        headers: Vec::new(),
        kind: crate::Kind::Cloud(spec),
    }
}

/// 云离线任务的结束状态。
pub(super) fn ending(outcome: Outcome) -> (Status, Option<crate::Finished>) {
    match outcome {
        Outcome::Finished(bytes) => (
            Status::Done,
            Some(crate::Finished {
                duration: None,
                bytes,
                resolution: None,
            }),
        ),
        Outcome::Paused => (Status::Paused, None),
        Outcome::Failed(failure) => (Status::Failed(failure), None),
    }
}
