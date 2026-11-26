//! 网页里的视频流：网页在后台加载、不显示，主体列出找到的视频流，每条在后台探测好（几档清晰度、时长或直播），用户
//! 挑一条下载。要登录、认证或手动点播放时可以打开网页（左边网页、右边列表），弄完收起。
//!
//! 页面的地址、加载、标题与故障由网页引擎报；脚本发回的只有找到的流、顶层文档的类型与正文是否为播放列表，按不可信
//! 数据校验、限量（见 [`script`]、[`inbox`]）。打开的地址是不是网页按第一个顶层文档定论（见 [`verdict`]）。网页要开
//! 的新窗口与要下载的文件都不放行，列出来由用户决定。网页里的视频与音频只在网页显示着时放。找不到流时，打开的地址
//! 可以当作播放列表交回新建页（缺请求头时源站回了网页）。

mod check;
mod found;
mod inbox;
mod list;
mod page;
mod request;
mod script;
mod verdict;

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, IntoElement,
    ParentElement as _, Render, Styled as _, Subscription, Task, Window, div, px,
};
use hs_dlx_app::Service;
use hs_dlx_webview::WebView;
use hs_m3u8_core::Url;

use self::found::{Check, Found};
use self::inbox::{Engine, Inbox};
use self::script::Report;
use self::verdict::Judge;
pub use self::verdict::Verdict;
use crate::dial;
use crate::links::{NOT_HTTP, http_url};
use crate::new_task::Link;
use crate::ui::GAP;
use crate::wait::within;
use crate::web_route::{Lease, RelayFailure, WebRoute};
use hs_dlx_webview::cookies::cookies_for;

/// 一个页面里最多列出多少条流：网页能报任意多条，再多的只计数。
const MAX_FOUND: usize = 200;
/// 加载这么久还没找到流时给出建议：没开始显示的多半打不开，显示了的多半要登录或点了播放才加载视频。
const SUGGEST_AFTER: Duration = Duration::from_secs(12);
/// 打开地址后最多等多久有是不是网页的结论，到时按打不开定论：慢的网页首字节也在十几秒内，网页引擎没给出故障（如
/// macOS 上连不上时）也不让新建页一直等。
const DECIDE_WITHIN: Duration = Duration::from_secs(30);

/// 发给外面的事件。
pub enum SniffEvent {
    /// 打开这条流的新建页（带着后台已探测好的结果，见 [`Link::probe`]）
    Open(Link),
    Dismissed,
    /// 网页显示或收起了：外面据此决定要不要让出任务列表的位置
    PageToggled,
    /// 打开的地址是不是网页有了结论（[`SniffView::verdict`]）
    Decided,
}

/// 当前页面加载到哪一步。
struct Load {
    /// 开始显示的顶层地址，网页引擎报的；还没开始显示时为 None
    shown: Option<String>,
    finished: bool,
    /// 这次打开或换页的时刻
    since: Instant,
}

pub struct SniffView {
    service: Rc<Service>,
    /// 打开的地址：可以当作播放列表交回新建页
    opened: Url,
    /// 建不出网页时为原因
    webview: Result<Entity<WebView>, String>,
    /// 网页显示着（左边网页、右边列表）；否则只有列表，网页在后台
    page_shown: bool,
    /// 网页此刻是整页的人机验证（见 [`script::SCRIPT`]）：要用户在网页里过了验证才找得到流
    challenge: bool,
    /// 嗅探页正显示着（由外面告诉，见 [`SniffView::set_on_screen`]）
    on_screen: bool,
    address: Entity<InputState>,
    /// 网页的标题；还没有时为 None
    title: Option<String>,
    load: Load,
    /// 当前页面的故障（导航没成、网页进程退出）：要刷新才回来
    failure: Option<String>,
    /// 网页的出口没照此刻的设置的原因（见 [`WebRoute::acquire`]）
    proxy_note: Option<String>,
    /// 网页开着期间持有的出口；建不出网页时为 None
    lease: Option<Rc<Lease>>,
    _relay_failures: Subscription,
    judge: Judge,
    found: Vec<Found>,
    /// 列表满了之后又找到的条数
    overflow: usize,
    inbox: Rc<Inbox>,
    /// 最近一次操作没成的原因（地址不是链接、网页没能显示等）
    error: Option<String>,
    /// 网页最近一次要在新窗口里打开的地址
    popup: Option<Url>,
    /// 网页最近一次要下载的文件的地址
    download: Option<Url>,
    /// 网页换了地址、还没写进地址栏的（写地址栏要窗口，下一次画时写）
    navigated: Option<String>,
    /// 正在后台探测的一条（一次一条）；释放即取消
    checking: Option<Task<()>>,
    /// 在读网页的 Cookie、好把网页交给网页视频解析（见 [`SniffView::open_page_as_web`]）；释放即取消
    opening_web: Option<Task<()>>,
    _engine: Task<()>,
    /// 找流中逐帧重画、加载够久时重画一次好给出建议（见 [`SniffView::wait_to_suggest`]）
    _suggest: Task<()>,
    /// 到时还没有结论就按打不开定论
    _deadline: Task<()>,
    _typed: Subscription,
}

impl EventEmitter<SniffEvent> for SniffView {}

impl SniffView {
    /// 在后台打开 `url`、开始找视频流；网页不显示，视频与音频不放。
    pub fn new(
        service: Rc<Service>,
        url: Url,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (inbox, mut events) = Inbox::new();
        let offer = |make: fn(String) -> Engine| {
            let inbox = inbox.clone();
            move |text: String| inbox.offer(make(text))
        };
        let builder = wry::WebViewBuilder::new()
            .with_visible(false)
            // 不放开自动播放（各引擎照默认）；视频与音频另由网页引擎暂停或静音，见 apply_media
            .with_autoplay(false)
            // 播放器常在嵌入的框架里：每一层框架都注入，脚本自己决定在哪层跑（见 script::SCRIPT）
            .with_initialization_script_for_main_only(script::SCRIPT, false)
            .with_ipc_handler({
                let inbox = inbox.clone();
                move |request| {
                    let frame = hs_dlx_webview::ipc_frame(&request);
                    let body = request.into_body();
                    inbox.offer(Engine::Message { body, frame });
                }
            })
            .with_document_title_changed_handler(offer(Engine::Title))
            .with_new_window_req_handler({
                let popup = offer(Engine::Popup);
                move |url, _| {
                    popup(url);
                    wry::NewWindowResponse::Deny
                }
            })
            .with_download_started_handler({
                let download = offer(Engine::Download);
                move |url, _| {
                    download(url);
                    false
                }
            })
            .with_on_page_load_handler({
                let inbox = inbox.clone();
                move |event, url| {
                    inbox.offer(match event {
                        wry::PageLoadEvent::Started => Engine::Started(url),
                        wry::PageLoadEvent::Finished => Engine::Finished(url),
                    });
                }
            });
        // macOS 上报 Safari 的标识（见 hs_dlx_webview::user_agent），网站的人机验证才把它当浏览器
        let builder = match hs_dlx_webview::user_agent() {
            Some(agent) => builder.with_user_agent(&agent),
            None => builder,
        };
        // 网页的出口进程里共用（见 crate::web_route）；中转连不过去的报来时看是不是打开的地址
        let route = WebRoute::get(cx);
        let relay_failures = cx.subscribe(&route, |this: &mut Self, _, failure, cx| {
            this.on_relay_failure(failure, cx)
        });
        let acquired = service
            .settings()
            .map_err(|error| format!("读不出设置：{error}"))
            .and_then(|settings| route.update(cx, |route, _| route.acquire(&settings.proxy)));
        let engine_events = {
            let inbox = inbox.clone();
            move |event| {
                inbox.offer(match event {
                    hs_dlx_webview::Event::LoadFailed(reason) => Engine::LoadFailed(reason),
                    hs_dlx_webview::Event::ProcessGone => Engine::Crashed,
                    hs_dlx_webview::Event::Responded(status) => Engine::Responded(status),
                });
            }
        };
        let (lease, proxy_note, webview) = match acquired {
            Ok((lease, note)) => {
                let webview = WebView::build(
                    builder,
                    url.as_str(),
                    &lease.proxy,
                    engine_events,
                    window,
                    cx,
                )
                .map(|webview| cx.new(|_| webview))
                .map_err(|error| format!("建不出网页：{error}"));
                (Some(lease), note, webview)
            }
            Err(reason) => (None, None, Err(reason)),
        };
        let engine = cx.spawn(async move |this, cx| {
            while let Some(first) = events.recv().await {
                // 网页引擎在同一次回调里报的事件一起到（回调与这里都在界面线程上）：一批处理完再按加载完定论
                let mut batch = vec![first];
                while let Ok(next) = events.try_recv() {
                    batch.push(next);
                }
                let alive = this.update(cx, |this, cx| {
                    for event in batch {
                        this.on_engine(event, cx);
                    }
                    this.settle(cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let address = cx.new(|cx| InputState::new(window, cx));
        address.update(cx, |input, cx| input.set_value(url.to_string(), window, cx));
        let typed = cx.subscribe_in(
            &address,
            window,
            |this: &mut Self, input, event: &InputEvent, _, cx| match event {
                InputEvent::PressEnter { .. } => {
                    let text = input.read(cx).value().to_string();
                    this.open(&text, cx);
                }
                InputEvent::Change | InputEvent::Focus | InputEvent::Blur => {}
            },
        );
        let mut judge = Judge::new();
        if let Err(reason) = &webview {
            judge.fail(reason.clone());
        }
        let deadline = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DECIDE_WITHIN).await;
            // 关了时不再需要结论
            let _closed = this.update(cx, |this, cx| {
                let what = if this.load.shown.is_some() {
                    "加载完"
                } else {
                    "打开"
                };
                let reason = format!("网页 {} 秒还没{what}", DECIDE_WITHIN.as_secs());
                if this.judge.fail(reason) {
                    cx.emit(SniffEvent::Decided);
                }
            });
        });
        let mut view = SniffView {
            service,
            opened: url,
            webview,
            page_shown: false,
            challenge: false,
            on_screen: false,
            address,
            title: None,
            load: Load {
                shown: None,
                finished: false,
                since: Instant::now(),
            },
            failure: None,
            proxy_note,
            lease,
            _relay_failures: relay_failures,
            judge,
            found: Vec::new(),
            overflow: 0,
            inbox,
            error: None,
            popup: None,
            download: None,
            navigated: None,
            checking: None,
            opening_web: None,
            _engine: engine,
            _suggest: Task::ready(()),
            _deadline: deadline,
            _typed: typed,
        };
        view.apply_media(cx);
        view.wait_to_suggest(cx);
        view
    }

    /// 从现在起到 [`SUGGEST_AFTER`]：还没找到流、嗅探页显示着而网页收起时每隔 [`dial::frame`] 重画一次，让找流的
    /// 小表盘转起来（系统要求减少动态效果时不画）；到时再重画一次，那时还没找到流就给出建议。
    fn wait_to_suggest(&mut self, cx: &mut Context<Self>) {
        self.load.since = Instant::now();
        let since = self.load.since;
        self._suggest = cx.spawn(async move |this, cx| {
            while since.elapsed() < SUGGEST_AFTER {
                cx.background_executor()
                    .timer(dial::frame(dial::TURN))
                    .await;
                let closed = this.update(cx, |this, cx| {
                    let searching =
                        this.webview.is_ok() && this.found.is_empty() && this.failure.is_none();
                    if searching && this.on_screen && !this.page_shown && !cx.reduce_motion() {
                        cx.notify();
                    }
                });
                if closed.is_err() {
                    return;
                }
            }
            // 关了时不用重画
            let _closed = this.update(cx, |_, cx| cx.notify());
        });
    }

    fn on_engine(&mut self, event: Engine, cx: &mut Context<Self>) {
        self.inbox.received(&event);
        let decided = match event {
            Engine::Started(url) => {
                self.found.clear();
                self.overflow = 0;
                self.title = None;
                self.error = None;
                self.popup = None;
                self.download = None;
                self.failure = None;
                self.checking = None;
                self.load.shown = Some(url.clone());
                self.load.finished = false;
                self.navigated = Some(url);
                self.wait_to_suggest(cx);
                // 新页面照当前是否显示决定放不放
                self.apply_media(cx);
                false
            }
            // 定论放到这一批处理完（见 settle）
            Engine::Finished(url) => {
                self.load.finished = true;
                self.navigated = Some(url);
                false
            }
            // 正在做的后台探测不依赖网页进程，照常做完
            Engine::LoadFailed(reason) => {
                self.failure = Some(reason.clone());
                self.judge.fail(reason)
            }
            Engine::Responded(status) => self.judge.responded(status),
            Engine::Crashed => {
                let reason = "网页进程意外退出了".to_owned();
                self.failure = Some(reason.clone());
                self.judge.fail(reason)
            }
            Engine::Message { body, frame } => match script::parse(&body) {
                Some(Report::Stream { url, agent }) => {
                    let top = self.load.shown.as_deref().and_then(http_url);
                    let frame = frame.as_deref().and_then(http_url);
                    self.add(Found::new(url, frame, top, agent), cx);
                    false
                }
                Some(Report::Document { content_type }) => {
                    self.challenge = false;
                    self.judge.document(&content_type)
                }
                // 验证页藏在后台时用户不知道要先过验证：是验证页即把网页显示出来
                Some(Report::Content {
                    playlist,
                    challenge,
                }) => {
                    self.challenge = challenge;
                    if challenge && !self.page_shown {
                        self.show_page(true, cx);
                    }
                    self.judge.content(playlist, challenge)
                }
                None => return,
            },
            // 单页应用换页时地址变了而不重新加载，常伴着改标题：顺带读一次地址
            Engine::Title(title) => {
                self.title = (!title.is_empty()).then_some(title);
                if let Some(webview) = self.webview() {
                    match webview.read(cx).raw().url() {
                        Ok(url) => self.navigated = Some(url),
                        Err(error) => self.error = Some(format!("读不出网页的地址：{error}")),
                    }
                }
                false
            }
            // 不是 http(s) 的（about:blank、blob: 之类）没什么可开
            Engine::Popup(url) => {
                self.popup = http_url(&url);
                false
            }
            Engine::Download(url) => {
                self.download = http_url(&url);
                // 还没显示任何页面：打开的地址本身就是要下载的文件
                self.load.shown.is_none() && self.judge.download()
            }
        };
        if decided {
            cx.emit(SniffEvent::Decided);
        }
        cx.notify();
    }

    /// 一批引擎事件处理完：显示过的页面加载完了，就按它报过的类型定论。放到批末是因为 WebView2 对没成的导航也先报
    /// 加载完、再报失败（同一次回调），失败要先定。
    fn settle(&mut self, cx: &mut Context<Self>) {
        if self.load.finished && self.load.shown.is_some() && self.judge.finished() {
            cx.emit(SniffEvent::Decided);
        }
    }

    /// 中转连不过去：打开的地址还没开始显示、连的正是它的主机与端口时，网页打不开（网页引擎拿到中转的 502 不报
    /// 原因）；页面里别的请求连不过去由页面自己处理。
    ///
    /// 简化：中转是开着的网页共用的，分不出连接来自哪个网页；另一个嗅探页恰在这时连同一主机与端口失败，也会算到这里。
    /// 要分开时给每个网页一个只认它的口令（引擎不收代理口令，要换成按网页区分的端口或数据存储）。
    fn on_relay_failure(&mut self, failure: &RelayFailure, cx: &mut Context<Self>) {
        let opened = self.opened.host().map(|host| host.to_owned());
        if self.load.shown.is_some()
            || opened.as_ref() != Some(&failure.target.host)
            || self.opened.port_or_known_default() != Some(failure.target.port)
        {
            return;
        }
        self.failure = Some(failure.reason.clone());
        if self.judge.fail(failure.reason.clone()) {
            cx.emit(SniffEvent::Decided);
        }
        cx.notify();
    }

    /// 加进列表并排队探测；同一个框架又报来的同一条流换成新地址（见 [`Found::renew`]），满了的只计数。
    fn add(&mut self, found: Found, cx: &mut Context<Self>) {
        if let Some(known) = self.found.iter_mut().find(|known| known.same(&found)) {
            known.renew(found);
        } else if self.found.len() < MAX_FOUND {
            self.found.push(found);
        } else {
            self.overflow += 1;
        }
        self.check_next(cx);
    }

    /// 打开的地址是不是网页。
    pub fn verdict(&self) -> &Verdict {
        self.judge.verdict()
    }

    /// 网页的标题；还没有时为 None。
    pub fn page_title(&self) -> Option<String> {
        self.title.clone()
    }

    /// 列出了几条流。
    pub fn found_count(&self) -> usize {
        self.found.len()
    }

    /// 网页显示着：外面这时让出任务列表的位置。
    pub fn page_shown(&self) -> bool {
        self.page_shown
    }

    fn webview(&self) -> Option<&Entity<WebView>> {
        self.webview.as_ref().ok()
    }

    /// 嗅探页显示（`on_screen`）或不显示（换到别的页）了。不显示时原生网页要自己藏起来（不随 GPUI 的画面消失）；显示
    /// 时网页随下一次画摆出来。后台探测照常做。
    pub fn set_on_screen(&mut self, on_screen: bool, cx: &mut Context<Self>) {
        self.on_screen = on_screen;
        if !on_screen {
            self.hide_webview(cx);
        }
        self.apply_media(cx);
        cx.notify();
    }

    /// 显示或收起网页。收起即弄完了（登录、点播放）：没成的流带着新的 Cookie 重新探测。
    fn show_page(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.page_shown = shown;
        if !shown {
            self.hide_webview(cx);
            self.retry(None, cx);
        }
        self.apply_media(cx);
        cx.emit(SniffEvent::PageToggled);
        cx.notify();
    }

    fn hide_webview(&mut self, cx: &mut Context<Self>) {
        if let Some(webview) = self.webview()
            && let Err(error) = webview.read(cx).hide()
        {
            self.error = Some(format!("网页没能收起：{error}"));
        }
    }

    /// 网页看得见（嗅探页显示着、网页没收起）时放视频与音频，否则由网页引擎暂停（macOS）或静音（Windows、Linux）。
    fn apply_media(&mut self, cx: &mut Context<Self>) {
        let silenced = !(self.on_screen && self.page_shown);
        if let Some(webview) = self.webview()
            && let Err(error) = webview.read(cx).silence_media(silenced)
        {
            self.error = Some(format!("网页没能停下或恢复播放：{error}"));
        }
    }

    /// 在网页里打开 `text`（地址栏里输入的，或网页要在新窗口里开、用户点了在这里打开的）。
    fn open(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(url) = http_url(text) else {
            self.error = Some(NOT_HTTP.to_owned());
            cx.notify();
            return;
        };
        if let Some(webview) = self.webview() {
            self.error = webview
                .read(cx)
                .raw()
                .load_url(url.as_str())
                .err()
                .map(|error| format!("打不开：{error}"));
        }
        cx.notify();
    }

    /// 列表里第 `index` 条、地址为 `url` 的流还在时为 `index`：画列表时记下的按钮在列表换了之后（旧帧上的点击）可能
    /// 对不上，不同框架报的流也可能地址相同，两样都核对。
    fn row(&self, index: usize, url: &Url) -> Option<usize> {
        self.found
            .get(index)
            .is_some_and(|found| found.url == *url)
            .then_some(index)
    }

    /// 打开列表里那一条流（见 [`SniffView::row`]）的新建页：探测好的带上结果与当时的请求头，没成的带上请求头，网页的
    /// 标题取现在的。对不上时什么也不做。
    fn open_found(&self, index: usize, url: &Url, cx: &mut Context<Self>) {
        let Some(found) = self.row(index, url).map(|index| &self.found[index]) else {
            return;
        };
        let (headers, probe) = match &found.check {
            Check::Ready { probe, headers, .. } => (headers.clone(), Some(probe.clone())),
            Check::Failed {
                headers: Some(headers),
                ..
            } => (headers.clone(), None),
            Check::Waiting | Check::Running | Check::Failed { headers: None, .. } => return,
        };
        self.open_link(url.clone(), self.title.clone(), headers, probe, cx);
    }

    /// 把网页要下载的文件 `url` 不带请求头交给新建页去探测；任务照网页的标题取名。
    fn open_download(&self, url: Url, cx: &mut Context<Self>) {
        self.open_link(url, self.title.clone(), Vec::new(), None, cx);
    }

    /// 把正显示的网页交给新建页按网页视频解析（yt-dlp），带上网页引擎里发往它的 Cookie：在这里登录过的网站，解析与
    /// 下载都带着登录状态。读不出 Cookie 时写明原因、不交。
    fn open_page_as_web(&mut self, cx: &mut Context<Self>) {
        let Some(webview) = self.webview().cloned() else {
            return;
        };
        let page = self
            .load
            .shown
            .as_deref()
            .and_then(http_url)
            .unwrap_or_else(|| self.opened.clone());
        let cookies = cookies_for(webview.read(cx).raw(), &page);
        self.opening_web = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let cookies = within(
                std::pin::pin!(cookies),
                executor.timer(check::COOKIE_TIMEOUT),
            )
            .await
            .unwrap_or_else(|| {
                Err(format!(
                    "网页引擎 {} 秒没有回出 Cookie",
                    check::COOKIE_TIMEOUT.as_secs()
                ))
            });
            // 关了时不再需要
            let _closed = this.update(cx, |this, cx| {
                this.opening_web = None;
                match cookies {
                    Ok(cookies) => {
                        let headers = request::cookie_header(&cookies).into_iter().collect();
                        this.open_link(page, None, headers, None, cx);
                    }
                    Err(reason) => {
                        this.error = Some(format!("没读出网页的 Cookie：{reason}"));
                        cx.notify();
                    }
                }
            });
        }));
    }

    /// 把打开的地址当作播放列表交给新建页：它取到的是网页（多为缺请求头时源站回的拦截页），网页的标题不是视频的，
    /// 任务由地址取名。
    fn open_opened(&self, cx: &mut Context<Self>) {
        self.open_link(self.opened.clone(), None, Vec::new(), None, cx);
    }

    fn open_link(
        &self,
        url: Url,
        title: Option<String>,
        headers: Vec<(String, String)>,
        probe: Option<Box<hs_m3u8_core::Probe>>,
        cx: &mut Context<Self>,
    ) {
        cx.emit(SniffEvent::Open(Link {
            url,
            title,
            headers,
            dropped: Vec::new(),
            probe,
        }));
    }
}

impl Render for SniffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 网页换了地址：写进地址栏；正在输入时先留着，输完再写
        if !self.address.read(cx).focus_handle(cx).is_focused(window)
            && let Some(text) = self.navigated.take()
        {
            self.address
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        if self.page_shown {
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .gap(px(GAP))
                .child(self.render_page(cx))
                .child(self.render_aside(cx))
        } else {
            self.render_deck(cx)
        }
    }
}
