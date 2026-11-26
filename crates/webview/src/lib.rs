//! 把 wry 的原生网页视图放进 GPUI 窗口：随布局摆位置、在被裁掉时藏起来，挡住的区域不让下面的 GPUI 内容收到点击；
//! 另取网页引擎的 Cookie 存储（[`cookies`]）。嵌入的部分源自 gpui-webview 0.9.1（© Longbridge，Apache-2.0，全文见本
//! crate 的 LICENSE-APACHE），改为依赖官方 wry，只留不用窗口合成时用到的部分。
//!
//! 原生视图直接挂在窗口上：它盖住所在区域里的 GPUI 内容（弹层、提示也盖住），也不能被部分裁剪（容器只裁掉一部分时它
//! 照样占满原来的区域），整个移出可见区域时才藏起来。不画它的那一帧它不会自己消失：换掉所在的视图时由使用方调
//! [`WebView::hide`]。

mod agent;
pub mod cookies;
mod failure;

pub use agent::user_agent;
pub use failure::Failure;
pub use proxy::{PROXY_FIXED_FOR_PROCESS, WebProxy, custom_proxy_supported};
#[cfg(target_os = "linux")]
mod linux;
mod media;
mod proxy;

/// wry 的网页消息（`with_ipc_handler` 收到的）是哪个框架发的：macOS 上为那个框架的地址；Windows 上只有顶层框架能发，
/// 为顶层页面的地址；Linux 上 wry 给的总是顶层页面的地址、认不出是哪个框架发的，为 None。地址由网页引擎给出，页面
/// 伪造不了。
pub fn ipc_frame(request: &wry::http::Request<String>) -> Option<String> {
    (!cfg!(target_os = "linux")).then(|| request.uri().to_string())
}

#[cfg(target_os = "linux")]
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::{
    App, Bounds, ContentMask, Element, ElementId, Entity, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseDownEvent, ParentElement as _, Pixels, Render,
    Size, Style, Styled as _, Window, div,
};
use wry::Rect;

/// 摆放网页时几件事各自最近一次没做成的原因，做成后清掉；各记各的，一件做成了不清掉另一件的原因。
#[derive(Default)]
struct Troubles {
    /// 摆到布局给的位置、显示出来
    place: Option<String>,
    /// 点在网页外时把键盘焦点交还窗口
    focus: Option<String>,
    /// 页面缩放对齐窗口的缩放：只有 Linux 要对齐，别的平台总为 None
    scale: Option<String>,
}

/// 放在 GPUI 窗口里的网页。
pub struct WebView {
    webview: Rc<wry::WebView>,
    /// 画的过程中报不出错误，放在这里由使用方显示
    troubles: RefCell<Troubles>,
    /// GPUI 的缩放倍数，页面的缩放按它对齐（GDK 只支持整数倍）
    #[cfg(target_os = "linux")]
    scale_factor: Cell<f32>,
    /// 页面所在区域的左上角（窗口坐标），把页面上的点击转给 GPUI 时用
    #[cfg(target_os = "linux")]
    origin: Rc<Cell<gpui_kit::Point<Pixels>>>,
    /// 持有期间 GPUI 驱动 GTK 的主循环
    #[cfg(target_os = "linux")]
    _gtk: linux::GtkUser,
}

impl Drop for WebView {
    /// 视图本身随最后一个引用释放；先藏起来，不让它在窗口里多留一帧。
    fn drop(&mut self) {
        // 释放时没有调用方能处理；藏不起来的视图随窗口一起销毁
        let _hidden = self.webview.set_visible(false);
    }
}

impl WebView {
    /// 按 `builder`（不带地址）在 `window` 里建网页，做好各平台 wry 要的准备，设好代理 `proxy` 后加载 `url`；网页出了
    /// wry 不报的故障（见 [`Failure`]）时在界面线程上调 `on_failure`。`proxy` 要是这个平台设得了的
    /// （[`custom_proxy_supported`]），否则是调用方的缺陷，中止。Linux 上窗口要跑在 X11（含 XWayland）上，Wayland 窗口
    /// 报错；GTK 在这里初始化，有网页期间由 GPUI 驱动它的主循环。
    pub fn build(
        builder: wry::WebViewBuilder,
        url: &str,
        proxy: &WebProxy,
        on_failure: impl Fn(Failure) + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> wry::Result<Self> {
        if let Err(reason) = proxy::supported(proxy) {
            panic!("调用方要先确认网页代理受支持：{reason}");
        }
        let report: failure::Report = Rc::new(on_failure);
        let builder = failure::before_build(builder, &report);
        let builder = proxy::before_build(builder, proxy);
        #[cfg(target_os = "linux")]
        let (webview, gtk) = {
            let parent = linux::X11Parent::new(window)?;
            let gtk = linux::GtkUser::new(cx)?;
            (builder.build_as_child(&parent)?, gtk)
        };
        #[cfg(not(target_os = "linux"))]
        let webview = builder.build_as_child(&*window)?;
        failure::after_build(&webview, &report)?;
        proxy::after_build(&webview, proxy);
        webview.load_url(url)?;
        // 第一次布局之前先不占地方
        webview.set_bounds(Rect::default())?;
        #[cfg(target_os = "linux")]
        let origin = Rc::new(Cell::new(gpui_kit::Point::default()));
        #[cfg(target_os = "linux")]
        {
            linux::forward_mouse_down(&webview, origin.clone(), window, cx);
            linux::match_scale_factor(&webview, window.scale_factor())?;
        }
        #[cfg(not(target_os = "linux"))]
        let _ = cx;
        Ok(WebView {
            webview: Rc::new(webview),
            troubles: RefCell::new(Troubles::default()),
            #[cfg(target_os = "linux")]
            scale_factor: Cell::new(window.scale_factor()),
            #[cfg(target_os = "linux")]
            origin,
            #[cfg(target_os = "linux")]
            _gtk: gtk,
        })
    }

    /// 底层的 wry 网页。
    pub fn raw(&self) -> &wry::WebView {
        &self.webview
    }

    /// 摆放、交还焦点与对齐缩放（Linux）最近没做成的原因，几件都没做成时用「；」连起来；都做成了为 None。
    pub fn trouble(&self) -> Option<String> {
        let troubles = self.troubles.borrow();
        let reasons: Vec<&str> = [&troubles.place, &troubles.focus, &troubles.scale]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        (!reasons.is_empty()).then(|| reasons.join("；"))
    }

    /// 让网页里全部的视频与音频不出声（`silenced`）或恢复，由网页引擎做，页面自己的脚本挡不住：macOS 上为暂停，期间
    /// 页面也播不了；Windows、Linux 上为静音，视频照常播放、下载。做不到时为原因。
    pub fn silence_media(&self, silenced: bool) -> Result<(), String> {
        media::silence(&self.webview, silenced)
    }

    /// 藏起网页（页面照旧留着）：所在的视图不再显示时调，下次画出来时重新摆出来。
    pub fn hide(&self) -> wry::Result<()> {
        self.webview.set_visible(false)
    }

    /// 按页面区域 `document` 与继承下来的裁剪区 `clip` 摆放：完全在裁剪区外时藏起来（页面照旧留着）。
    fn place(&self, document: Bounds<Pixels>, clip: Bounds<Pixels>) -> wry::Result<bool> {
        if document.intersect(&clip).is_empty() {
            self.webview.set_visible(false)?;
            return Ok(false);
        }
        #[cfg(target_os = "linux")]
        let rect = linux::device_bounds(document, self.scale_factor.get());
        #[cfg(not(target_os = "linux"))]
        let rect = Rect {
            position: wry::dpi::LogicalPosition::new(
                f64::from(document.origin.x),
                f64::from(document.origin.y),
            )
            .into(),
            size: wry::dpi::LogicalSize::new(
                f64::from(document.size.width),
                f64::from(document.size.height),
            )
            .into(),
        };
        self.webview.set_bounds(rect)?;
        self.webview.set_visible(true)?;
        Ok(true)
    }

    /// 记下一件事（`slot` 取出它在 [`Troubles`] 里的那一项）的结果 `result`，`what` 为没做成时的说法；原因有变化时
    /// 返回 true，要重画才显示得出来。
    fn record(
        &self,
        slot: fn(&mut Troubles) -> &mut Option<String>,
        result: wry::Result<()>,
        what: &str,
    ) -> bool {
        let reason = result.err().map(|error| format!("{what}：{error}"));
        let mut troubles = self.troubles.borrow_mut();
        let slot = slot(&mut troubles);
        let changed = *slot != reason;
        *slot = reason;
        changed
    }
}

impl Render for WebView {
    fn render(&mut self, _: &mut Window, cx: &mut gpui_kit::Context<Self>) -> impl IntoElement {
        div().size_full().child(WebViewElement {
            view: cx.entity(),
            webview: self.webview.clone(),
        })
    }
}

/// 摆放原生网页视图的元素。
struct WebViewElement {
    view: Entity<WebView>,
    webview: Rc<wry::WebView>,
}

impl IntoElement for WebViewElement {
    type Element = WebViewElement;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WebViewElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: Size::full(),
            flex_shrink: 1.,
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let view = self.view.read(cx);
        let mut changed = false;
        #[cfg(target_os = "linux")]
        {
            if view.scale_factor.replace(window.scale_factor()) != window.scale_factor() {
                let matched = linux::match_scale_factor(&view.webview, window.scale_factor());
                changed |= view.record(|t| &mut t.scale, matched, "网页缩放没能对齐窗口");
            }
            view.origin.set(bounds.origin);
        }
        let clip = window.content_mask().bounds;
        let placed = view.place(bounds, clip);
        let hitbox = match &placed {
            // 原生视图盖住这一块，下面的 GPUI 内容不该收到点击
            Ok(true) => {
                Some(window.insert_hitbox(bounds.intersect(&clip), HitboxBehavior::BlockMouse))
            }
            Ok(false) | Err(_) => None,
        };
        changed |= view.record(|t| &mut t.place, placed.map(|_| ()), "网页没能摆到位置上");
        if changed {
            // 原因随下一帧显示出来；原因不再变时不再请求
            window.refresh();
        }
        hitbox
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let bounds = hitbox.as_ref().map_or(bounds, |hitbox| hitbox.bounds);
        let view = self.view.clone();
        let webview = self.webview.clone();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.on_mouse_event(move |event: &MouseDownEvent, _, window, cx| {
                // 点在网页外面时把键盘焦点交还窗口，输入框才收得到字
                if !bounds.contains(&event.position) {
                    let returned = webview.focus_parent();
                    if view
                        .read(cx)
                        .record(|t| &mut t.focus, returned, "键盘焦点没能交还窗口")
                    {
                        window.refresh();
                    }
                }
            });
        });
    }
}
