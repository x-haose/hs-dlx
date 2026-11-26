//! wry 不报的网页引擎事件：顶层导航失败、网页进程退出、顶层文档的 HTTP 状态码。wry 的页面加载事件不分成败（打不开
//! 时 WebView2 显示自带的错误页，照样报开始与完成），也不给状态码，这里直接接各平台网页引擎的事件。

use std::rc::Rc;

/// 网页引擎的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 顶层导航没成（连不上、证书不对、地址不对之类）：网页引擎给的原因。macOS 上 wry 不给这个事件，打不开时只是
    /// 一直不开始显示
    LoadFailed(String),
    /// 网页进程退出或卡死了，刷新才回来
    ProcessGone,
    /// 顶层收到了 HTTP 响应：地址与状态码（重定向之后的那个），在它成为文档、页面脚本跑之前报。macOS 上对没成为
    /// 文档的响应（转为下载、被新的导航取代）也报，按地址认领。只有 macOS、Linux 报
    Responded { url: String, status: u16 },
    /// 顶层文档加载完：网页引擎给的它的 HTTP 状态码。只有 Windows 报（WebView2 到加载完才给状态码；旧的运行时没有
    /// `ICoreWebView2NavigationCompletedEventArgs2`，不报）
    Completed(u16),
}

/// 收到事件时调的。
pub(crate) type Report = Rc<dyn Fn(Event)>;

/// 接着网页引擎事件的东西，与网页同生命周期（macOS 上为包着 wry 导航代理的那一层，网页只弱引用它）。
#[cfg(target_os = "macos")]
pub(crate) type Hooks = objc2::rc::Retained<mac::ResponseWatch>;
#[cfg(not(target_os = "macos"))]
pub(crate) type Hooks = ();

/// 建网页之前：macOS 上网页进程退出只能在建之前登记。
#[cfg(target_os = "macos")]
pub(crate) fn before_build<'a>(
    builder: wry::WebViewBuilder<'a>,
    report: &Report,
) -> wry::WebViewBuilder<'a> {
    use wry::WebViewBuilderExtDarwin as _;
    let report = report.clone();
    builder.with_on_web_content_process_terminate_handler(move || report(Event::ProcessGone))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn before_build<'a>(
    builder: wry::WebViewBuilder<'a>,
    _: &Report,
) -> wry::WebViewBuilder<'a> {
    builder
}

/// 建好网页之后：WebView2 接导航完成（看是否成功、报状态码）与进程故障。
#[cfg(target_os = "windows")]
pub(crate) fn after_build(webview: &wry::WebView, report: &Report) -> wry::Result<Hooks> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_PROCESS_FAILED_KIND, COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
        COREWEBVIEW2_WEB_ERROR_STATUS, COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED,
        ICoreWebView2NavigationCompletedEventArgs2,
    };
    use webview2_com::{NavigationCompletedEventHandler, ProcessFailedEventHandler};
    use windows::core::BOOL;
    use windows::core::Interface as _;
    use wry::WebViewExtWindows as _;

    let failed = |error| wry::Error::WebView2Error(webview2_com::Error::WindowsError(error));
    let core = webview.webview();
    let navigated = {
        let report = report.clone();
        NavigationCompletedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let mut success = BOOL::default();
            // SAFETY: 引擎在界面线程上回调，参数在回调期间有效；出参指向本地变量。
            unsafe { args.IsSuccess(&mut success) }?;
            if !success.as_bool() {
                let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                // SAFETY: 同上。
                unsafe { args.WebErrorStatus(&mut status) }?;
                // 被别的导航取代（换页、下载）：既不是没打开，它的状态码也不属于显示着的文档
                if status == COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED {
                    return Ok(());
                }
            }
            // 拿到了 HTTP 响应时为状态码，为 0 即没拿到响应。旧的 WebView2 运行时没有这个接口
            let code = match args.cast::<ICoreWebView2NavigationCompletedEventArgs2>() {
                Ok(args) => {
                    let mut code = 0;
                    // SAFETY: 同上。
                    unsafe { args.HttpStatusCode(&mut code) }?;
                    u16::try_from(code).ok().filter(|code| *code > 0)
                }
                Err(_) => None,
            };
            match code {
                Some(code) => report(Event::Completed(code)),
                // 拿到了 HTTP 响应的（4xx、5xx：要登录、人机验证、地区限制的页面照样显示了）不算没打开；没成而报不出
                // 状态码的（没拿到响应，或旧的运行时）照没成报
                None if !success.as_bool() => {
                    let mut status = COREWEBVIEW2_WEB_ERROR_STATUS::default();
                    // SAFETY: 同上。
                    unsafe { args.WebErrorStatus(&mut status) }?;
                    report(Event::LoadFailed(format!(
                        "网页引擎报导航没成（WebErrorStatus {}）",
                        status.0
                    )));
                }
                None => {}
            }
            Ok(())
        }))
    };
    let mut token = 0;
    // SAFETY: 在界面线程上登记（WebView2 只能在建它的线程上用），网页此刻有效；处理器随网页一起释放。
    unsafe { core.add_NavigationCompleted(&navigated, &mut token) }.map_err(failed)?;
    let crashed = {
        let report = report.clone();
        ProcessFailedEventHandler::create(Box::new(move |_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
            // SAFETY: 引擎在界面线程上回调，参数在回调期间有效；出参指向本地变量。
            unsafe { args.ProcessFailedKind(&mut kind) }?;
            // 只有这几种让网页没了；GPU、工具进程与子框架的进程退出后引擎自己恢复
            if [
                COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED,
                COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED,
                COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE,
            ]
            .contains(&kind)
            {
                report(Event::ProcessGone);
            }
            Ok(())
        }))
    };
    // SAFETY: 同 add_NavigationCompleted。
    unsafe { core.add_ProcessFailed(&crashed, &mut token) }.map_err(failed)?;
    Ok(())
}

/// 建好网页之后：WebKitGTK 接主资源加载失败、提交（取状态码）与网页进程退出。
#[cfg(target_os = "linux")]
pub(crate) fn after_build(webview: &wry::WebView, report: &Report) -> wry::Result<Hooks> {
    use webkit2gtk::{
        LoadEvent, NetworkError, PolicyError, URIResponseExt as _, WebResourceExt as _,
        WebViewExt as _,
    };
    use wry::WebViewExtUnix as _;

    let view = webview.webview();
    let responded = report.clone();
    view.connect_load_changed(move |view, event| {
        if event != LoadEvent::Committed {
            return;
        }
        let Some(response) = view
            .main_resource()
            .and_then(|resource| resource.response())
        else {
            return;
        };
        // 不是 HTTP 的（file:、data:）状态码为 0
        if let (Ok(status @ 1..), Some(url)) =
            (u16::try_from(response.status_code()), response.uri())
        {
            responded(Event::Responded {
                url: url.to_string(),
                status,
            });
        }
    });
    let failed = report.clone();
    view.connect_load_failed(move |_, _, _, error| {
        // 被别的导航取代、交给下载的不算没打开
        if !error.matches(NetworkError::Cancelled)
            && !error.matches(PolicyError::FrameLoadInterruptedByPolicyChange)
        {
            failed(Event::LoadFailed(format!("网页加载失败：{error}")));
        }
        // 照常显示引擎自己的错误页
        false
    });
    let crashed = report.clone();
    view.connect_web_process_terminated(move |_, _| crashed(Event::ProcessGone));
    Ok(())
}

/// 建好网页之后：在 wry 的导航代理外面包一层，顶层文档的响应到时报状态码，别的都照转给 wry（见 [`mac::ResponseWatch`]）。
#[cfg(target_os = "macos")]
pub(crate) fn after_build(webview: &wry::WebView, report: &Report) -> wry::Result<Hooks> {
    use objc2::MainThreadMarker;
    use objc2::runtime::ProtocolObject;
    use wry::WebViewExtMacOS as _;

    let mtm = MainThreadMarker::new().expect("网页只在界面线程上建");
    let view = webview.webview();
    // SAFETY: 在界面线程上读网页的属性。
    let inner = unsafe { view.navigationDelegate() }.expect("wry 建网页时总设了导航代理");
    let watch = mac::ResponseWatch::new(inner, report.clone(), mtm);
    // SAFETY: 在界面线程上设；网页只弱引用代理，`watch` 由调用方与网页一起持有，它又持有 wry 的代理（wry 自己也持有）。
    unsafe { view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*watch))) };
    Ok(watch)
}

#[cfg(target_os = "macos")]
pub(crate) mod mac {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
    use objc2::{DefinedClass as _, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_foundation::NSHTTPURLResponse;
    use objc2_web_kit::{
        WKNavigationDelegate, WKNavigationResponse, WKNavigationResponsePolicy, WKWebView,
    };

    use super::{Event, Report};

    pub struct Ivars {
        /// wry 的导航代理：别的方法都转给它
        inner: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
        report: Report,
    }

    // SAFETY（这一块里的各个 unsafe）：父类是 NSObject、只在界面线程上用（MainThreadOnly），与 WKWebView 的要求一致；
    // 各方法的 Rust 签名与选择子在 Objective-C 里的类型一致（respondsToSelector: 收 SEL 回 BOOL，
    // forwardingTargetForSelector: 收 SEL 回 id，响应决定方法的签名同 objc2-web-kit 里 WKNavigationDelegate 的定义）；
    // 声明遵守的两个协议只实现了其中的可选方法。
    define_class!(
        /// 包着 wry 导航代理的一层：自己只接「决定怎么处理响应」，记下顶层文档的状态码后交给 wry 决定；别的方法经
        /// Objective-C 的消息转发交给 wry 的代理。WebKit 在设代理时按 `respondsToSelector:` 记下代理实现了哪些方法，
        /// 所以它答「实现了」的是自己的加上 wry 的。
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = Ivars]
        pub struct ResponseWatch;

        impl ResponseWatch {
            #[unsafe(method(respondsToSelector:))]
            fn responds_to_selector(&self, selector: Sel) -> bool {
                // SAFETY: NSObject 的 respondsToSelector: 收一个选择子、回 BOOL。
                let own: bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
                own || self.ivars().inner.respondsToSelector(selector)
            }

            #[unsafe(method(forwardingTargetForSelector:))]
            fn forwarding_target_for_selector(&self, _selector: Sel) -> *mut AnyObject {
                // 只有自己没实现的方法才走到这里
                Retained::as_ptr(&self.ivars().inner).cast::<AnyObject>().cast_mut()
            }
        }

        unsafe impl NSObjectProtocol for ResponseWatch {}

        unsafe impl WKNavigationDelegate for ResponseWatch {
            #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
            fn decide_response(
                &self,
                webview: &WKWebView,
                response: &WKNavigationResponse,
                handler: &block2::DynBlock<dyn Fn(WKNavigationResponsePolicy)>,
            ) {
                // SAFETY: WebKit 在界面线程上回调，参数在回调期间有效。
                let main = unsafe { response.isForMainFrame() };
                // SAFETY: 同上。
                let answer = unsafe { response.response() };
                if main
                    && let Ok(http) = answer.downcast::<NSHTTPURLResponse>()
                    && let Ok(status) = u16::try_from(http.statusCode())
                    && let Some(url) = http.URL().and_then(|url| url.absoluteString())
                {
                    (self.ivars().report)(Event::Responded {
                        url: url.to_string(),
                        status,
                    });
                }
                let inner = &self.ivars().inner;
                if inner.respondsToSelector(sel!(webView:decidePolicyForNavigationResponse:decisionHandler:)) {
                    // SAFETY: wry 的代理实现了这个可选方法（上面刚问过），参数原样转交。
                    unsafe {
                        inner.webView_decidePolicyForNavigationResponse_decisionHandler(
                            webview, response, handler,
                        );
                    }
                } else {
                    // 同 WebKit 对没实现这个方法的代理的默认做法：能显示的就显示
                    handler.call((WKNavigationResponsePolicy::Allow,));
                }
            }
        }
    );

    impl ResponseWatch {
        pub fn new(
            inner: Retained<ProtocolObject<dyn WKNavigationDelegate>>,
            report: Report,
            mtm: MainThreadMarker,
        ) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars { inner, report });
            // SAFETY: NSObject 的 init。
            unsafe { msg_send![super(this), init] }
        }
    }
}
