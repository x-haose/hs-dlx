//! 准备的一步：在内置网页里找视频流。地址可能是网页（探测被拒、取到的不是播放列表也不是文件）或网页没有解析器
//! 认得时到这里：等到网页的名额，让宿主在后台打开它，看宿主报来的定论与找到的流。
//!
//! - 定论不是网页：是播放列表或文件的，带着网页引擎里发往它的 Cookie 重新取；出错页以状态码失败。
//! - 整页的人机验证：把网页显示出来，停下等用户（[`PageAsk::Challenge`]），过了自己接着找。
//! - 找到的流逐条在后台探测。第一条探测成功的出现后再等一阵（[`PagePace::adopt_after`]）：没有第二条即采用它，有就
//!   停下让用户选。
//! - 到时（[`PagePace::play_after`]）还没有流：把网页显示出来，停下等用户点播放（[`PageAsk::Play`]），找到了自己接着
//!   走；找到的都没探测成时停下，列出各自的原因。
//!
//! 停下等用户在网页里动手时这一轮不结束（见 `prepare` 的模块说明）。宿主关掉了网页（主窗口关了）时：在后台找着的
//! 排回去，等宿主又能开了重新打开；正等着用户的作废可选项（[`Stop::Lapsed`]）。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use hs_m3u8_core::{Resource, Source};
use tokio::time::Instant;

use super::pages::Place;
use super::prepare::{Adopted, Asked, Flow, Next, Offer, Origin, Stage, Step, Stop, hls_next};
use super::{Event, Shared, lock};
use crate::page::{
    COOKIE_TIMEOUT, Check, Cookie, Found, PageEvent, PagePace, PageSession, Stream, Unopened,
    Verdict, check, document_headers,
};
use crate::store;
use crate::{Ask, Error, Failure, FailureKind, PageAsk, Settings, Status, Task, naming};

/// 一个页面里最多记多少条流：网页能报任意多条，再多的不记。
const MAX_FOUND: usize = 200;

/// 网页看到哪一步离开了它。
enum Left {
    /// 宿主关掉了网页；`asking` 为那时正停下等用户
    Away { asking: bool },
    /// 只有一条探测成功的流：用它
    Adopted(Box<Adopted>),
    /// 几条探测成功的流要用户选，或找到的都没探测成；所在网页的标题
    Streams(Vec<Found>, Option<String>),
    /// 打开的地址不是网页
    Document(Document),
}

/// 网页引擎打开任务的地址，它不是网页。
struct Document {
    /// 是播放列表；否则是别的文件
    playlist: bool,
    /// 不是播放列表时网页引擎认出的类型；没认出时为 None
    kind: Option<String>,
    /// 网页引擎里会发往这个地址的 Cookie；没读出时为原因
    cookies: Result<Vec<Cookie>, String>,
}

/// 正在探测的一条流：结果与没带上的 Cookie 个数。
type Probing = Pin<Box<dyn Future<Output = (Check, usize)> + Send>>;

/// 等到的一件事。
enum Woke {
    Event(Option<Box<PageEvent>>),
    Checked((Check, usize)),
    Timer,
}

/// 一个开着的网页找流找到哪了。
struct Watch<'a> {
    shared: &'a Arc<Shared>,
    flow: &'a Flow,
    task: &'a Task,
    settings: &'a Settings,
    pace: PagePace,
    session: PageSession,
    /// 打开的地址是网页已有定论
    decided: bool,
    /// 此刻停下等用户在网页里做什么
    asking: Option<PageAsk>,
    found: Vec<Found>,
    /// 当前页面的标题（整理成文件主名的），已写成任务的标题
    title: Option<String>,
    /// 正在探测的一条（一次一条）
    probing: Option<Probing>,
    /// 到时还没有定论即打不开
    decide_by: Option<Instant>,
    /// 到时还没有探测成功的流：没找到流的把网页显示出来，找到的都没探测成的停下
    play_by: Option<Instant>,
    /// 有了探测成功的流，等到这时看有没有第二条
    adopt_at: Option<Instant>,
}

impl Shared {
    /// 在内置网页里找任务 `task` 的流（见模块说明）；`failure` 为到这里之前的失败（直接取这个地址的结果）：没有
    /// 网页宿主、或网页引擎的说法与它对不上时以它结束。
    pub(super) async fn in_page(
        self: &Arc<Self>,
        flow: &Flow,
        task: &Task,
        folder: PathBuf,
        settings: &Settings,
        failure: Failure,
    ) -> Result<Next, Stop> {
        let Some((host, pace)) = self.pages.host() else {
            return Err(Stop::Failed(Failure {
                kind: failure.kind,
                message: format!(
                    "{}；这里没有内置网页，没法到网页里找视频流",
                    failure.message
                ),
            }));
        };
        let order = self.queue_for_page(flow)?;
        let left = loop {
            let since = self.seated(flow, order).await?;
            let session = match host.open(flow.id, &task.spec.url, &settings.proxy) {
                Ok(session) => session,
                Err(Unopened::Away) => {
                    self.host_away(flow, order, since, false)?;
                    continue;
                }
                Err(Unopened::Failed(reason)) => {
                    return Err(Stop::failed(
                        FailureKind::Network,
                        format!("建不出网页：{reason}"),
                    ));
                }
            };
            let watch = Watch {
                shared: self,
                flow,
                task,
                settings,
                pace,
                session,
                decided: false,
                asking: None,
                found: Vec::new(),
                title: None,
                probing: None,
                decide_by: Some(Instant::now() + pace.decide_within),
                play_by: None,
                adopt_at: None,
            };
            match watch.run().await? {
                Left::Away { asking } => self.host_away(flow, order, since, asking)?,
                left => break left,
            }
        };
        // 网页已关掉：让出名额，之后的探测与取名不占它
        self.leave_page(flow)?;
        match left {
            Left::Away { .. } => unreachable!("宿主关掉网页的在上面排回去了"),
            Left::Adopted(adopted) => {
                let Adopted { probe, origin } = *adopted;
                Ok(hls_next(task, probe, Some(origin), folder, settings))
            }
            Left::Streams(found, title) => Ok(Next::ask(Asked::streams(found, title.as_deref()))),
            Left::Document(document) => {
                self.refetch(flow, task, folder, settings, document, failure)
                    .await
            }
        }
    }

    /// 这一轮要开网页：在名额里排上（有空的随即占上），位子记在运行登记里，返回它的次序。这一轮已被取消或顶掉时为
    /// [`Stop::Cancelled`]（下同）。
    fn queue_for_page(&self, flow: &Flow) -> Result<u64, Stop> {
        let _store = lock(&self.store);
        self.live(flow)?;
        let seat = self.pages.enter();
        let order = seat.order();
        if let Some(running) = lock(&self.running).get_mut(&flow.id) {
            running.seat = Some(seat);
        }
        Ok(order)
    }

    /// 等到次序为 `order` 的位子占上名额；排着时报出前面还有几个（[`Step::PageQueued`]），占上时报
    /// [`Step::Page`]，返回那一刻宿主说过几回「开得了」（见 [`Shared::host_away`]）。在 `store` 锁内看位子并记步骤：
    /// 名额只在这把锁内放行与归还，看到占上了的那一刻它确实占着。
    async fn seated(&self, flow: &Flow, order: u64) -> Result<u64, Stop> {
        let mut changed = self.pages.subscribe();
        loop {
            changed.borrow_and_update();
            {
                let _store = lock(&self.store);
                self.live(flow)?;
                match self.pages.place(order) {
                    Some(Place::Seated) => {
                        self.step_in(flow, Step::Page { found: 0 });
                        return Ok(self.pages.returns());
                    }
                    Some(Place::Queued { ahead, away }) => {
                        self.step_in(flow, Step::PageQueued { ahead, away });
                    }
                    // 位子随登记让出了：这一轮已不是现在的
                    None => return Err(Stop::Cancelled),
                }
            }
            changed.changed().await.expect("持有 Shared 时名额的账还在");
        }
    }

    /// 宿主关掉了这一轮的网页，或说此刻开不了：`since` 为这一轮占上名额时宿主说过几回「开得了」，这之后它没再说过
    /// 时记下宿主开不了（消息经通道晚到、宿主已经回来的不记，不然排着的要等它下一次回来）。在后台找着的把位子排回
    /// 去、步骤改为排着（宿主开得了时重新打开），与让出名额在同一把锁内，别的任务占上这个名额的事件排在它之后；
    /// 正停下等用户的（`asking`）这一轮到此为止（[`Stop::Lapsed`]），位子留到收尾时与作废可选项一起让出。
    fn host_away(&self, flow: &Flow, order: u64, since: u64, asking: bool) -> Result<(), Stop> {
        let _store = lock(&self.store);
        self.live(flow)?;
        if asking {
            self.pages.gone(since);
            return Err(Stop::Lapsed);
        }
        self.pages.away(order, since);
        if let Some(Place::Queued { ahead, away }) = self.pages.place(order) {
            self.step_in(flow, Step::PageQueued { ahead, away });
        }
        Ok(())
    }

    /// 这一轮不再用网页：让出位子，步骤记为在探测（[`Step::Page`] 只在占着名额时报）。
    fn leave_page(&self, flow: &Flow) -> Result<(), Stop> {
        let _store = lock(&self.store);
        self.live(flow)?;
        let seat = lock(&self.running)
            .get_mut(&flow.id)
            .and_then(|running| running.seat.take());
        // 在 `running` 锁外让出（各把锁之间不嵌套）
        drop(seat);
        self.step_in(flow, Step::Probing);
        Ok(())
    }

    /// 这一轮没被取消、没被顶掉；不然为 [`Stop::Cancelled`]。调用方持着 `store` 锁：之后在这把锁内做的改动不会落在
    /// 顶掉它的那一轮之后。
    pub(super) fn live(&self, flow: &Flow) -> Result<(), Stop> {
        if flow.cancel.is_cancelled() || !self.current(flow.id, &flow.guard) {
            return Err(Stop::Cancelled);
        }
        Ok(())
    }

    /// 这一轮停下等用户在网页里做 `ask`：状态记为等用户、记下可选项，发 [`Event::Changed`] 与 [`Event::Stage`]。
    /// 这一轮照旧在跑、占着名额。
    fn ask_in_page(&self, flow: &Flow, ask: PageAsk) -> Result<(), Stop> {
        let offer = Offer::Page(ask);
        let mut store = lock(&self.store);
        self.live(flow)?;
        let task = self.turn(&mut store, flow, &Status::Waiting(Ask::Page(ask)))?;
        if let Some(running) = lock(&self.running).get_mut(&flow.id) {
            running.step = None;
        }
        lock(&self.offers).insert(flow.id, Asked::plain(offer.clone()));
        self.events.send(Event::Changed {
            task: Box::new(task),
        });
        self.events.send(Event::Stage {
            id: flow.id,
            stage: Some(Box::new(Stage::Asking(offer))),
        });
        Ok(())
    }

    /// 用户在网页里弄好了（过了验证、点了播放）：状态回到准备中，步骤为 `step`，发 [`Event::Changed`] 与
    /// [`Event::Stage`]。
    fn answered_in_page(&self, flow: &Flow, step: Step) -> Result<(), Stop> {
        let mut store = lock(&self.store);
        self.live(flow)?;
        let task = self.turn(&mut store, flow, &Status::Preparing)?;
        lock(&self.offers).remove(&flow.id);
        self.events.send(Event::Changed {
            task: Box::new(task),
        });
        self.step_in(flow, step);
        Ok(())
    }

    /// 把这一轮的任务的状态改为 `status`（准备中与等用户之间），返回改后的任务。调用方持着 `store` 锁。
    fn turn(&self, store: &mut store::Store, flow: &Flow, status: &Status) -> Result<Task, Stop> {
        let id = flow.id;
        self.write_in(store, |tx| {
            store::set_status(tx, id, status)?;
            store::get(tx, id)?.ok_or(Error::NotFound(id))
        })
        .map_err(|error| Stop::of(&error))
    }

    /// 网页引擎打开任务的地址不是网页（`document`）：带着它里面会发往这个地址的 Cookie 重新取。是播放列表的照播放
    /// 列表探测；否则重新认一次，是文件、播放列表的照它定，认出的仍是网页时以原来的失败 `failure` 结束。
    async fn refetch(
        self: &Arc<Self>,
        flow: &Flow,
        task: &Task,
        folder: PathBuf,
        settings: &Settings,
        document: Document,
        failure: Failure,
    ) -> Result<Next, Stop> {
        let Document {
            playlist,
            kind,
            cookies,
        } = document;
        let (cookies, unread) = match cookies {
            Ok(cookies) => (cookies, None),
            Err(reason) => (Vec::new(), Some(reason)),
        };
        let headers = document_headers(&task.spec.headers, &cookies);
        let origin = Origin {
            url: task.spec.url.clone(),
            headers: headers.clone(),
            // 取到的不是网页，没有网页的标题：任务由地址取名
            title: None,
        };
        let mut source = Source::new(task.spec.url.clone());
        source.http.headers = headers;
        source.http.proxy = settings.proxy.clone();
        // 没读出 Cookie 时这次取与之前的一样：失败的说明里写上
        let failed = |mut failure: Failure| {
            if let Some(reason) = &unread {
                failure.message = format!("{}（没读出网页的 Cookie：{reason}）", failure.message);
            }
            Stop::Failed(failure)
        };
        if playlist {
            return match self.engine.probe(&source).await {
                Ok(probe) => Ok(hls_next(
                    task,
                    Box::new(probe),
                    Some(origin),
                    folder,
                    settings,
                )),
                Err(error) => Err(failed(Failure::of(&error))),
            };
        }
        match self.engine.identify(&source).await {
            Ok(Resource::Playlist(probe)) => {
                Ok(hls_next(task, probe, Some(origin), folder, settings))
            }
            Ok(Resource::File(info)) => {
                self.file_next(flow, task, info, Some(origin), folder, settings)
            }
            Ok(Resource::Page { .. }) => {
                let seen = match kind {
                    Some(kind) => format!("网页引擎打开它是 {kind}，不是网页"),
                    None => "网页引擎打开它不是网页".to_owned(),
                };
                Err(failed(Failure {
                    kind: failure.kind,
                    message: format!("{}；{seen}，直接取到的却是网页", failure.message),
                }))
            }
            Err(error) => Err(failed(Failure::of(&error))),
        }
    }
}

impl Watch<'_> {
    /// 看着网页，直到离开它（见 [`Left`]）或这一轮没走成。
    async fn run(mut self) -> Result<Left, Stop> {
        loop {
            let now = Instant::now();
            if let Some(left) = self.conclude(now)? {
                return Ok(left);
            }
            self.check_next();
            if self.asking.is_none() {
                self.shared.step(self.flow, self.step());
            }
            // 已过的时限在上面看过了，要等别的事才会变
            let wake = [self.decide_by, self.play_by, self.adopt_at]
                .into_iter()
                .flatten()
                .filter(|at| *at > now)
                .min();
            let woke = {
                let events = &mut self.session.events;
                let probe = self.probing.as_mut();
                let (probing, timed) = (probe.is_some(), wake.is_some());
                tokio::select! {
                    event = events.recv() => Woke::Event(event.map(Box::new)),
                    checked = async { probe.expect("有在探测的才等").await }, if probing => {
                        Woke::Checked(checked)
                    }
                    () = async { tokio::time::sleep_until(wake.expect("有时限才等")).await },
                        if timed => Woke::Timer,
                }
            };
            match woke {
                Woke::Event(Some(event)) => {
                    if let Some(left) = self.on_event(*event).await? {
                        return Ok(left);
                    }
                }
                Woke::Event(None) => {
                    return Err(Stop::failed(
                        FailureKind::Other,
                        "网页宿主没有交代就关掉了网页",
                    ));
                }
                Woke::Checked(checked) => self.checked(checked),
                Woke::Timer => {}
            }
        }
    }

    /// 宿主报来一件事；离开网页时为 Some。
    async fn on_event(&mut self, event: PageEvent) -> Result<Option<Left>, Stop> {
        let now = Instant::now();
        match event {
            PageEvent::Verdict(_) if self.decided => {}
            PageEvent::Verdict(verdict) => match verdict {
                // 没报出状态码的照网页找：是出错页的话到时没有流，网页会显示给用户看
                Verdict::Page | Verdict::Unknown => {
                    self.decided = true;
                    self.decide_by = None;
                    if self.asking.is_none() {
                        self.play_by = Some(now + self.pace.play_after);
                    }
                }
                Verdict::Refused(status) => {
                    return Err(Stop::failed(
                        FailureKind::Status(status),
                        format!("在网页里打开它回 HTTP {status}"),
                    ));
                }
                Verdict::Playlist => return self.document(true, None).await.map(Some),
                Verdict::NotPage(kind) => return self.document(false, kind).await.map(Some),
                Verdict::Download => return self.document(false, None).await.map(Some),
            },
            PageEvent::Challenge(true) => {
                if self.asking != Some(PageAsk::Challenge) {
                    // 整页的人机验证即是网页，不论状态码
                    self.decided = true;
                    self.decide_by = None;
                    self.play_by = None;
                    // 验证页的标题不是视频的
                    if self.title.take().is_some() {
                        let title = super::prepare::pending_title(&self.task.spec.url);
                        self.shared.retitle(self.flow, &title)?;
                    }
                    self.ask(PageAsk::Challenge)?;
                }
            }
            PageEvent::Challenge(false) => {
                if self.asking == Some(PageAsk::Challenge) {
                    self.answered()?;
                    self.play_by = Some(now + self.pace.play_after);
                }
            }
            PageEvent::Stream(stream) => self.add(stream),
            PageEvent::Title(title) => {
                let title = naming::clean_title(&title);
                let fresh = !title.is_empty() && self.title.as_deref() != Some(title.as_str());
                if fresh && self.asking != Some(PageAsk::Challenge) {
                    self.shared.retitle(self.flow, &title)?;
                    self.title = Some(title);
                }
            }
            PageEvent::Navigated(_) => {
                self.found.clear();
                self.probing = None;
                self.adopt_at = None;
                self.title = None;
                if self.decided && self.asking != Some(PageAsk::Challenge) {
                    self.play_by = Some(now + self.pace.play_after);
                }
            }
            PageEvent::Failed(reason) => {
                return Err(Stop::failed(
                    FailureKind::Network,
                    format!("网页打不开：{reason}"),
                ));
            }
            PageEvent::Closed => {
                return Ok(Some(Left::Away {
                    asking: self.asking.is_some(),
                }));
            }
        }
        Ok(None)
    }

    /// 按此刻（`now`）找到的流与到了的时限下结论；离开网页时为 Some。
    fn conclude(&mut self, now: Instant) -> Result<Option<Left>, Stop> {
        let due = |at: Option<Instant>| at.is_some_and(|at| at <= now);
        if due(self.decide_by) {
            return Err(Stop::failed(
                FailureKind::Network,
                format!("网页 {} 秒还没打开", self.pace.decide_within.as_secs()),
            ));
        }
        let ready = self
            .found
            .iter()
            .filter(|found| matches!(found.check, Check::Ready { .. }))
            .count();
        if ready > 0 {
            if !due(self.adopt_at) {
                return Ok(None);
            }
            self.answered()?;
            let found = std::mem::take(&mut self.found);
            if ready > 1 {
                return Ok(Some(Left::Streams(found, self.title.take())));
            }
            let adopted = found
                .into_iter()
                .find_map(|found| match found.check {
                    Check::Ready { probe, headers } => Some(Adopted {
                        probe,
                        origin: Origin {
                            url: found.url,
                            headers,
                            title: self.title.take(),
                        },
                    }),
                    Check::Waiting | Check::Running | Check::Failed { .. } => None,
                })
                .expect("上面数出有一条探测成功的");
            return Ok(Some(Left::Adopted(Box::new(adopted))));
        }
        let pending = self
            .found
            .iter()
            .any(|found| matches!(found.check, Check::Waiting | Check::Running));
        if due(self.play_by) && !pending {
            self.play_by = None;
            if !self.found.is_empty() {
                // 找到的都没探测成
                self.answered()?;
                let found = std::mem::take(&mut self.found);
                return Ok(Some(Left::Streams(found, self.title.take())));
            }
            if self.asking.is_none() {
                self.ask(PageAsk::Play)?;
            }
        }
        Ok(None)
    }

    /// 没停下等用户时报给外面的步骤：网页开着，已找到几条流。
    fn step(&self) -> Step {
        Step::Page {
            found: self.found.len(),
        }
    }

    /// 停下等用户在网页里做 `ask`：记下状态，让宿主把网页显示出来。
    fn ask(&mut self, ask: PageAsk) -> Result<(), Stop> {
        self.shared.ask_in_page(self.flow, ask)?;
        self.asking = Some(ask);
        self.session
            .page
            .show(ask)
            .map_err(|reason| Stop::failed(FailureKind::Other, format!("网页没能显示：{reason}")))
    }

    /// 正停下等用户时：用户弄好了，收起网页、状态回到准备中。
    fn answered(&mut self) -> Result<(), Stop> {
        if self.asking.take().is_none() {
            return Ok(());
        }
        self.session.page.hide().map_err(|reason| {
            Stop::failed(FailureKind::Other, format!("网页没能收起：{reason}"))
        })?;
        self.shared.answered_in_page(self.flow, self.step())
    }

    /// 记下找到的一条流、排队探测；同一个框架又报来的同一条换成新地址（见 [`Found::renew`]），满了的不记。
    fn add(&mut self, stream: Stream) {
        let found = Found::new(stream);
        if let Some(known) = self.found.iter_mut().find(|known| known.same(&found)) {
            known.renew(found);
        } else if self.found.len() < MAX_FOUND {
            self.found.push(found);
        }
    }

    /// 没在探测时，探测下一条等着的流：读出网页引擎里发往它的 Cookie、配好请求头、探测。
    fn check_next(&mut self) {
        if self.probing.is_some() {
            return;
        }
        let Some(found) = self
            .found
            .iter_mut()
            .find(|found| matches!(found.check, Check::Waiting))
        else {
            return;
        };
        found.check = Check::Running;
        let stream = found.stream();
        let cookies = self.session.page.cookies(&stream.url);
        let engine = self.shared.engine.clone();
        let proxy = self.settings.proxy.clone();
        let own = self.task.spec.headers.clone();
        self.probing = Some(Box::pin(async move {
            let cookies = within_cookie_timeout(cookies).await;
            check(&engine, proxy, &stream, cookies, &own).await
        }));
    }

    /// 探测完一条：记下结果。头一条成功的出现时起算采用的时限；网页显示着等用户点播放时探测没成的，过一阵看是不是
    /// 都没成。
    fn checked(&mut self, (check, skipped): (Check, usize)) {
        self.probing = None;
        let now = Instant::now();
        match &check {
            Check::Ready { .. } => {
                self.adopt_at.get_or_insert(now + self.pace.adopt_after);
                // 有能用的流了：不再按「迟迟没有定论」算打不开
                self.decide_by = None;
            }
            Check::Failed { .. } if self.asking == Some(PageAsk::Play) => {
                self.play_by.get_or_insert(now + self.pace.adopt_after);
            }
            Check::Failed { .. } | Check::Waiting | Check::Running => {}
        }
        // 一次只探测一条；换页时列表清空、探测随之取消：探测中的就是这条
        let entry = self
            .found
            .iter_mut()
            .find(|found| matches!(found.check, Check::Running))
            .expect("探测中的流在探测完之前一直在列表里");
        entry.check = check;
        entry.skipped = skipped;
    }

    /// 定论为打开的地址不是网页：读出网页引擎里会发往它的 Cookie，离开网页。
    async fn document(&mut self, playlist: bool, kind: Option<String>) -> Result<Left, Stop> {
        let cookies = within_cookie_timeout(self.session.page.cookies(&self.task.spec.url)).await;
        Ok(Left::Document(Document {
            playlist,
            kind,
            cookies,
        }))
    }
}

/// 等网页引擎回出 Cookie，最多 [`COOKIE_TIMEOUT`]；到时为原因。
async fn within_cookie_timeout(
    cookies: impl Future<Output = Result<Vec<Cookie>, String>>,
) -> Result<Vec<Cookie>, String> {
    tokio::time::timeout(COOKIE_TIMEOUT, cookies)
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "网页引擎 {} 秒没有回出 Cookie",
                COOKIE_TIMEOUT.as_secs()
            ))
        })
}
