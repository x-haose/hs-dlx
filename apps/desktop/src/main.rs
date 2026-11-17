//! 桌面应用的入口：打开任务存储、启动任务服务、建托盘、开主窗口；任务存储已被开着的应用占着时，把启动参数里的链接
//! 转交给它后退出（见 hs-dlx-instance）。有任务在跑时关掉主窗口可以让它们在后台继续，应用只在明确退出时退出（见
//! main_window）。

mod backdrop;
mod clock;
mod dial;
mod dock;
mod format;
mod headers;
mod links;
mod live;
mod main_window;
mod motion;
mod new_task;
mod rate;
mod ring;
mod route;
mod settings;
mod shell;
mod signals;
mod sniff;
mod tasks;
mod theme;
mod tray;
mod ui;
mod updates;
mod wait;
mod web_route;

use std::borrow::Cow;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use gpui_kit::component::TitleBar;
use gpui_kit::{
    AppContext as _, Bounds, FontWeight, ParentElement as _, QuitMode, Render, SharedString,
    Styled as _, WindowBounds, WindowOptions, div, px, size,
};
use hs_dlx_app::{Service, Store, TorrentNetwork};
use hs_dlx_instance::HandOverError;

use crate::tasks::Tasks;
use crate::tray::Tray;
use crate::ui::{PAD, TITLE_HEIGHT};
use crate::updates::Updates;

/// 全部任务合计同时最多多少个请求（见 `Engine::new`）。
const MAX_REQUESTS: NonZeroUsize = NonZeroUsize::new(32).unwrap();

/// 随应用打包的字体：数字用的 Space Grotesk 三个字重（SIL OFL 1.1，许可证见同目录 OFL.txt）。
const FONTS: [&[u8]; 3] = [
    include_bytes!("../assets/fonts/SpaceGrotesk-Regular.ttf"),
    include_bytes!("../assets/fonts/SpaceGrotesk-Medium.ttf"),
    include_bytes!("../assets/fonts/SpaceGrotesk-Bold.ttf"),
];

/// 随应用带的 Python 与 QuickJS-NG（make runtime 备好、build.rs 交来，见 scripts/runtime.py）：第一次用网页视频时解进
/// 组件目录。
#[cfg(bundled_runtime)]
fn bundle() -> Option<hs_dlx_app::ytdlp::Bundle> {
    use std::borrow::Cow;
    Some(hs_dlx_app::ytdlp::Bundle {
        python: hs_dlx_app::ytdlp::Bundled {
            id: env!("HS_DLX_RUNTIME_PYTHON_ID").to_owned(),
            bytes: Cow::Borrowed(include_bytes!(env!("HS_DLX_RUNTIME_PYTHON_PATH"))),
            executable: env!("HS_DLX_RUNTIME_PYTHON_EXECUTABLE").to_owned(),
        },
        python_abi: env!("HS_DLX_RUNTIME_PYTHON_ABI").to_owned(),
        quickjs: hs_dlx_app::ytdlp::Bundled {
            id: env!("HS_DLX_RUNTIME_QUICKJS_ID").to_owned(),
            bytes: Cow::Borrowed(include_bytes!(env!("HS_DLX_RUNTIME_QUICKJS_PATH"))),
            executable: env!("HS_DLX_RUNTIME_QUICKJS_FILE").to_owned(),
        },
    })
}

/// 应用不发布的平台没有随带的运行时：网页视频用不了。
#[cfg(not(bundled_runtime))]
fn bundle() -> Option<hs_dlx_app::ytdlp::Bundle> {
    None
}

/// 任务存储的位置：系统给应用放数据的目录（macOS 为 ~/Library/Application Support）下的 hs-dlx/tasks.sqlite。
fn store_path() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("hs-dlx").join("tasks.sqlite"))
}

/// 设置里没有保存位置时，新建任务默认保存到哪：影片目录，没有时下载目录，再没有时主目录。
fn default_folder() -> Option<PathBuf> {
    dirs::video_dir()
        .or_else(dirs::download_dir)
        .or_else(dirs::home_dir)
}

/// 打开平台。Linux 上有 X11（含 Wayland 会话里的 XWayland）时只用它：嗅探页的网页只能嵌进 X11 窗口；没有时照默认选，
/// 嗅探页打开时说明原因。
fn application() -> gpui_kit::Application {
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_some() {
        return gpui_kit::platform::linux(gpui_kit::WindowingModes::X11);
    }
    gpui_kit::application()
}

fn main() {
    // panic 是不变量被破坏：照常打印后中止进程，不让界面带着坏掉的状态继续，也不让任务服务里的任务悄悄消失；存储里
    // 还标着运行中的任务下次打开时按暂停处理。背景渲染器等 GPU 画完与释放的那一刻除外：wgpu 把设备丢失等外部故障当 panic
    // 报，渲染器收住后只停掉背景（见 hs_dlx_scene::panics_contained）
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        if !hs_dlx_scene::panics_contained() {
            std::process::abort();
        }
    }));
    // 程序坞、注销、关机时先问：在 GPUI 设好应用代理之前给它加上询问的方法（回调见 main_window）
    #[cfg(target_os = "macos")]
    hs_dlx_macos::install();
    let app = application().with_assets(gpui_kit::assets::Assets);
    // 点程序坞里的图标：回到窗口（关着时重新打开）
    app.on_reopen(main_window::reopen);
    app.run(|cx| {
            gpui_kit::init(cx);
            cx.text_system()
                .add_fonts(FONTS.iter().map(|font| Cow::Borrowed(*font)).collect())
                .expect("随应用打包的字体是有效的 TrueType 文件");
            // 输入框等组件跟着窗口的深色底
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            // 第一个参数为要探测的链接：从终端或别的程序把链接交给应用。不是 UTF-8 的照原样换成替换字符，粘贴栏里
            // 会提示它不是链接
            let link = std::env::args_os()
                .nth(1)
                .map(|arg| arg.to_string_lossy().into_owned());
            let started = store_path()
                .ok_or_else(|| Failure::new("启动失败", "找不到放应用数据的目录"))
                .and_then(|path| {
                    let store = match Store::open(&path) {
                        Ok(store) => store,
                        // 已经开着一个：把链接交给它（或请它到前台）就退出
                        Err(hs_dlx_app::Error::InUse(_)) => {
                            return match hs_dlx_instance::hand_over(&path, link.as_deref()) {
                                Ok(()) => Ok(None),
                                Err(HandOverError::Leaving) => Err(Failure::new(
                                    "hs-dlx 正在退出",
                                    match &link {
                                        Some(link) => {
                                            format!("等它退出后再打开这个链接：{link}")
                                        }
                                        None => "等它退出后再打开".to_owned(),
                                    },
                                )),
                                Err(HandOverError::Failed(reason)) => Err(Failure::new(
                                    "hs-dlx 已经在运行",
                                    match &link {
                                        Some(link) => format!(
                                            "同时只能开一个，链接没能交给它（{reason}）：到已经打开的那个窗口里粘贴 {link}"
                                        ),
                                        None => format!(
                                            "同时只能开一个，没能请它到前台（{reason}）：切到已经打开的那个窗口即可"
                                        ),
                                    },
                                )),
                            };
                        }
                        Err(error) => return Err(Failure::new("启动失败", error.to_string())),
                    };
                    let handoffs = hs_dlx_instance::listen(&path);
                    let (service, events) = Service::new(
                        store,
                        MAX_REQUESTS,
                        TorrentNetwork::Internet,
                        hs_dlx_app::pikpak::Endpoints::official(),
                        // 网页视频的组件放在任务存储旁的 components 下
                        hs_dlx_app::WebConfig {
                            root: path.with_file_name("components"),
                            bundle: bundle(),
                            sources: hs_dlx_app::ytdlp::Sources::official(),
                        },
                    )
                        .map_err(|error| Failure::new("启动失败", error.to_string()))?;
                    let settings = service
                        .settings()
                        .map_err(|error| Failure::new("启动失败", error.to_string()))?;
                    theme::set(settings.theme);
                    let folder = settings
                        .folder
                        .or_else(default_folder)
                        .ok_or_else(|| Failure::new("启动失败", "找不到保存视频的目录"))?;
                    Ok(Some((service, events, folder, handoffs, settings.check_updates)))
                });
            match started {
                Ok(None) => cx.quit(),
                Ok(Some((service, events, folder, handoffs, check_updates))) => {
                    // 最后一个窗口关掉不自动退出：在后台继续时窗口关着、应用照跑；退出由 main_window 发出
                    cx.set_quit_mode(QuitMode::Explicit);
                    let tasks = cx.new(|cx| Tasks::new(service, events, folder, handoffs, cx));
                    let updates = cx.new(|cx| {
                        Updates::new(tasks.clone(), updates::feed(), check_updates, cx)
                    });
                    main_window::install(tasks.clone(), updates, cx);
                    let mut problems = Vec::new();
                    let downloads = updates::feed().dir;
                    if let Some(reason) = hs_dlx_app::take_update_failure(&downloads) {
                        problems.push(format!("上次更新没做成：{reason}"));
                    }
                    if let Err(left) = hs_dlx_app::sweep_update(&downloads) {
                        problems.push(left);
                    }
                    #[cfg(target_os = "macos")]
                    if let Err(error) = main_window::ask_before_terminate(cx) {
                        problems.push(format!("从程序坞退出、注销时不会先问：{error}"));
                    }
                    if let Err(reason) =
                        Tray::install(tasks.clone(), main_window::reopen, main_window::quit, cx)
                    {
                        problems.push(format!(
                            "{reason}：关掉窗口让任务在后台继续后，再次打开应用回到窗口"
                        ));
                    }
                    tasks.update(cx, |tasks, cx| {
                        for problem in problems {
                            tasks.tell(problem, cx);
                        }
                    });
                    main_window::show(link, cx)
                        .expect("任务状态刚装上")
                        .expect("开不了窗口");
                }
                Err(failure) => {
                    cx.set_quit_mode(QuitMode::LastWindowClosed);
                    let options = WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                            None,
                            size(px(560.), px(320.)),
                            cx,
                        ))),
                        window_min_size: None,
                        ..main_window::options(cx)
                    };
                    gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| failure))
                        .expect("开不了窗口");
                }
            }
        });
}

/// 启动失败时的窗口：一句标题与原因。
struct Failure {
    title: SharedString,
    detail: SharedString,
}

impl Failure {
    fn new(title: impl Into<SharedString>, detail: impl Into<SharedString>) -> Failure {
        Failure {
            title: title.into(),
            detail: detail.into(),
        }
    }
}

impl Render for Failure {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::ground())
            .text_color(theme::text())
            .child(
                TitleBar::new()
                    .h(px(TITLE_HEIGHT))
                    .bg(theme::ground())
                    .border_color(theme::ground()),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::warn())
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .max_w(px(480.))
                            .px(px(PAD))
                            .text_center()
                            .text_color(theme::muted())
                            .child(self.detail.clone()),
                    ),
            )
    }
}
