//! 新建页的网页视频：粘贴的地址回的是网页时交给 yt-dlp 解析（组件没装时先装），解析出来后选画质、开始。只有通用
//! 解析器认得的网址照原来的走：在网页引擎里看、去嗅探页找视频流。

use gpui_kit::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};
use hs_dlx_app::web::{AudioCodec, Choice, Container, VideoCodec, no_choice_reason};
use hs_dlx_app::ytdlp::{ExtractError, Live};
use hs_dlx_app::{Failure, FailureKind, Resolved, WebState};

use super::{NewTask, NewTaskEvent, Probing};
use crate::dial;
use crate::format;
use crate::ring::{self, Ticks};
use crate::theme;
use crate::ui::{self, Solid, Tone};

/// 解析出的视频与选中的画质。
pub(super) struct WebChoice {
    pub resolved: Resolved,
    /// 选中的在 `resolved.choices` 里的下标
    pub selected: usize,
}

impl WebChoice {
    pub(super) fn choice(&self) -> &Choice {
        &self.resolved.choices[self.selected]
    }
}

impl NewTask {
    /// 地址回的是网页：组件装好了就解析，没装（或没装成）时等用户装，平台没有组件时照原来的去网页引擎里看。
    pub(super) fn resolve_page(&mut self, first: bool, cx: &mut Context<Self>) {
        let state = self.service.web_state().borrow().clone();
        match state {
            WebState::Ready(_) | WebState::UpdateFailed { .. } => self.resolve(first, cx),
            WebState::Unavailable => self.not_resolvable(first, cx),
            WebState::Missing
            | WebState::Installing(_)
            | WebState::Failed(_)
            | WebState::Broken(_) => {
                self.probing = Probing::NeedsWeb(state);
                self.watch_web(first, cx);
                cx.notify();
            }
        }
    }

    /// 看着组件的状态：装着时更新进度，装好即解析。
    fn watch_web(&mut self, first: bool, cx: &mut Context<Self>) {
        let mut states = self.service.web_state();
        self._web = cx.spawn(async move |this, cx| {
            loop {
                if states.changed().await.is_err() {
                    return;
                }
                let state = states.borrow_and_update().clone();
                let going = this.update(cx, |this, cx| {
                    if !matches!(this.probing, Probing::NeedsWeb(_)) {
                        return false;
                    }
                    if matches!(state, WebState::Ready(_) | WebState::UpdateFailed { .. }) {
                        this.resolve(first, cx);
                        return false;
                    }
                    this.probing = Probing::NeedsWeb(state);
                    cx.notify();
                    true
                });
                if !matches!(going, Ok(true)) {
                    return;
                }
            }
        });
    }

    /// 装网页视频的组件（进度见 [`Probing::NeedsWeb`]）。
    fn install_web(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.service.install_web() {
            self.probing = Probing::Failed(Failure {
                kind: FailureKind::WebComponents,
                message: error.to_string(),
            });
        }
        cx.notify();
    }

    /// 交给 yt-dlp 解析（之前的结果作废），带上请求头里的 Cookie（要登录才看得到的视频）；`first` 为第一次探测（见
    /// [`NewTask::not_resolvable`]）。
    fn resolve(&mut self, first: bool, cx: &mut Context<Self>) {
        let headers = self.headers.read(cx).headers().to_vec();
        let handle = match self.service.resolve(self.url.clone(), &headers) {
            Ok(handle) => handle,
            Err(error) => {
                // Cookie 的值不合的（有制表符等）改了再试；其余是组件没装好
                let kind = if matches!(error, hs_dlx_app::Error::InvalidInput(_)) {
                    FailureKind::Unrecoverable
                } else {
                    FailureKind::WebComponents
                };
                self.probing = Probing::Failed(Failure {
                    kind,
                    message: error.to_string(),
                });
                cx.notify();
                return;
            }
        };
        self.probing = Probing::Resolving;
        self.animate(cx);
        self._probe = cx.spawn(async move |this, cx| {
            let result = handle.await;
            // 视图在解析期间被关掉时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(resolved)) => this.resolved(resolved, cx),
                    Ok(Err(ExtractError::Unsupported(_))) => this.not_resolvable(first, cx),
                    Ok(Err(error)) => {
                        this.probing = Probing::Failed(Failure::of_extract(&error));
                    }
                    Err(join) => {
                        this.probing = Probing::Failed(Failure {
                            kind: FailureKind::Other,
                            message: format!("解析中途出错：{join}"),
                        });
                    }
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// 没有专门的解析器认得：照原来的，以「回的是网页」失败，第一次打开时让外面在网页引擎里看、去嗅探页。
    fn not_resolvable(&mut self, first: bool, cx: &mut Context<Self>) {
        self.probing = Probing::Failed(Failure {
            kind: FailureKind::Malformed,
            message: "地址回的是网页，不是视频流或文件".to_owned(),
        });
        if first {
            cx.emit(NewTaskEvent::NotAPlaylist);
        }
        cx.notify();
    }

    fn resolved(&mut self, resolved: Resolved, cx: &mut Context<Self>) {
        if let Some(entries) = &resolved.info.entries {
            self.probing = Probing::Failed(Failure {
                kind: FailureKind::Unsupported,
                message: format!(
                    "这是播放列表（{} 条），这一版只下单个视频：打开其中一个视频的网页再粘贴",
                    entries.len()
                ),
            });
            return;
        }
        // 网页直播要按直播录（续录、按时间切片），这一版的网页视频任务只下点播
        if matches!(resolved.info.live, Live::Now | Live::Processing) {
            self.probing = Probing::Failed(Failure {
                kind: FailureKind::Unsupported,
                message: "正在直播或直播刚结束、回放还在处理：这一版的网页视频只下点播，回放处理好后再粘贴".to_owned(),
            });
            return;
        }
        // 没有格式时解析已报错（网站说的原因）；有格式而都下不了的原因见 no_choice_reason
        if resolved.choices.is_empty() {
            self.probing = Probing::Failed(Failure {
                kind: FailureKind::Unsupported,
                message: format!(
                    "没有能下的格式：{}",
                    no_choice_reason(&resolved.info, resolved.freshness.as_ref())
                ),
            });
            return;
        }
        self.title = resolved.info.title.clone();
        self.probing = Probing::Web(Box::new(WebChoice {
            resolved,
            selected: 0,
        }));
        cx.emit(NewTaskEvent::Ready);
    }

    /// 选第 `index` 个画质。
    fn select_choice(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Probing::Web(choice) = &mut self.probing
            && index < choice.resolved.choices.len()
        {
            choice.selected = index;
            cx.notify();
        }
    }

    /// 选中的画质往上（`step` 为负）或往下挪。
    pub(super) fn step_choice(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Probing::Web(choice) = &self.probing {
            let last = choice.resolved.choices.len().saturating_sub(1);
            let next = choice.selected.saturating_add_signed(step).min(last);
            self.select_choice(next, cx);
        }
    }

    /// 组件没装好：说明、安装（或重装）的按钮与进度，另可改去嗅探页。
    pub(super) fn needs_web_inspector(
        &self,
        title: String,
        host: String,
        state: &WebState,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut body: Vec<AnyElement> = vec![
            ui::note(
                "解析网页里的视频（YouTube、B 站等）要先装网页视频的组件：下载 yt-dlp，约 11 MB，只装一次"
                    .to_owned(),
                false,
            )
            .into_any_element(),
        ];
        let mut foot = Vec::new();
        match state {
            WebState::Installing(progress) => {
                body.push(ui::steps(format::web_install_steps(progress)).into_any_element());
                body.push(ui::note(format::web_install(progress), false).into_any_element());
            }
            WebState::Failed(reason) => {
                body.push(ui::note(format!("上次没装成：{reason}"), true).into_any_element());
            }
            WebState::Broken(reason) => {
                body.push(
                    ui::note(format!("组件运行不了，要重装：{reason}"), true).into_any_element(),
                );
            }
            WebState::Missing
            | WebState::Ready(_)
            | WebState::UpdateFailed { .. }
            | WebState::Unavailable => {}
        }
        if !matches!(state, WebState::Installing(_)) {
            foot.push(
                ui::primary(
                    "install-web",
                    if matches!(state, WebState::Broken(_)) {
                        "重装网页视频组件"
                    } else {
                        "安装网页视频组件"
                    },
                    None,
                    Solid::Glow,
                    true,
                    cx.listener(|this, _, _, cx| this.install_web(cx)),
                )
                .into_any_element(),
            );
        }
        if !self.from_sniff {
            let url = self.url.clone();
            foot.push(
                ui::button(
                    "sniff",
                    "改用嗅探页找视频流",
                    Tone::Plain,
                    cx.listener(move |_, _, _, cx| cx.emit(NewTaskEvent::Sniff(url.clone()))),
                )
                .w_full()
                .into_any_element(),
            );
        }
        foot.push(self.cancel(Tone::Ghost, cx));
        ui::inspector(title, host, body, foot)
    }

    /// 解析出来了：时长、上传者、网站，画质选项（点选），保存位置与开始。
    pub(super) fn web_inspector(
        &self,
        title: String,
        host: String,
        choice: &WebChoice,
        cx: &mut Context<Self>,
    ) -> Div {
        let info = &choice.resolved.info;
        let mut cells: Vec<(SharedString, SharedString)> = Vec::new();
        if let Some(duration) = info.duration {
            cells.push((
                "时长".into(),
                format::clock(std::time::Duration::from_secs_f64(duration.max(0.0))).into(),
            ));
        }
        if let Some(uploader) = &info.uploader {
            cells.push(("上传者".into(), uploader.clone().into()));
        }
        let mut body: Vec<AnyElement> = self.reads_with_route(cells).into_iter().collect();
        let rows = choice
            .resolved
            .choices
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let selected = index == choice.selected;
                div()
                    .id(("web-choice", index))
                    .flex()
                    .justify_between()
                    .px(px(12.))
                    .py(px(8.))
                    .rounded(px(10.))
                    .cursor_pointer()
                    .when_selected(selected)
                    .on_click(cx.listener(move |this, _, _, cx| this.select_choice(index, cx)))
                    .child(div().child(label(option)))
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child(option.size.map_or_else(String::new, |size| {
                                let approx = if size.exact { "" } else { "约 " };
                                format!("{approx}{}", format::bytes(size.bytes))
                            })),
                    )
                    .into_any_element()
            });
        body.push(super::inspector::group("画质", rows).into_any_element());
        body.push(self.folder_group(cx).into_any_element());
        let headline = if self.creating {
            "正在开始…".to_owned()
        } else {
            "开始下载".to_owned()
        };
        let mut foot: Vec<AnyElement> = self
            .error
            .clone()
            .map(|error| ui::error_text(error).into_any_element())
            .into_iter()
            .collect();
        foot.push(
            ui::primary(
                "start",
                headline,
                Some(format!("{} · {}", self.title, label(choice.choice())).into()),
                Solid::Glow,
                !self.creating,
                cx.listener(|this, _, window, cx| this.start(window, cx)),
            )
            .into_any_element(),
        );
        if !self.creating {
            foot.push(self.cancel(Tone::Ghost, cx));
        }
        ui::inspector(title, host, body, foot)
    }
}

/// 选中的画质的表盘：大字为清晰度（只要音频时为「音频」），下面是编码、容器与大小。
pub(super) fn web_dial(choice: &WebChoice) -> Div {
    let ticks = Ticks::full(
        ring::COMET_TICKS,
        theme::alpha(theme::accent(), 0.45),
        false,
    );
    let option = choice.choice();
    let big = match option.height {
        Some(height) if option.video.is_some() => format!("{height}p"),
        _ if option.video.is_none() => "音频".to_owned(),
        _ => "原画".to_owned(),
    };
    let size = option.size.map_or_else(String::new, |size| {
        format!(
            " · {}{}",
            if size.exact { "" } else { "约 " },
            format::bytes(size.bytes)
        )
    });
    let line = format!("{}{size}", codecs(option));
    dial::dial(
        theme::accent(),
        dial::figure(move |bounds, window, _| {
            ring::paint_ticks(bounds, ring::RADIUS, &ticks, window)
        }),
        dial::readout("网页视频", theme::accent(), dial::Big::Phrase(big), line),
    )
}

/// 画质的写法：`1080p60 · H.264 · MP4`、`音频 · AAC · M4A`。
fn label(option: &Choice) -> String {
    let resolution = match (option.video.is_some(), option.height) {
        (false, _) => "音频".to_owned(),
        (true, Some(height)) => {
            let fps = option
                .fps
                .filter(|fps| *fps > 30.5)
                .map_or_else(String::new, |fps| format!("{}", fps.round()));
            format!("{height}p{fps}")
        }
        (true, None) => "原画".to_owned(),
    };
    let hdr = option
        .dynamic_range
        .as_ref()
        .map_or_else(String::new, |range| format!(" {range}"));
    format!("{resolution}{hdr} · {}", codecs(option))
}

/// 编码与容器：`H.264 + AAC · MP4`。
fn codecs(option: &Choice) -> String {
    let video = option.video_codec.as_ref().map(|codec| match codec {
        VideoCodec::H264 => "H.264".to_owned(),
        VideoCodec::Hevc => "HEVC".to_owned(),
        VideoCodec::Vp9 => "VP9".to_owned(),
        VideoCodec::Av1 => "AV1".to_owned(),
        VideoCodec::Other(name) => name.clone(),
        VideoCodec::Unknown => "视频".to_owned(),
    });
    let audio = option.audio_codec.as_ref().map(|codec| match codec {
        AudioCodec::Aac => "AAC".to_owned(),
        AudioCodec::Opus => "Opus".to_owned(),
        AudioCodec::Mp3 => "MP3".to_owned(),
        AudioCodec::Ac3 => "AC-3".to_owned(),
        AudioCodec::Eac3 => "E-AC-3".to_owned(),
        AudioCodec::Flac => "FLAC".to_owned(),
        AudioCodec::Alac => "ALAC".to_owned(),
        AudioCodec::Vorbis => "Vorbis".to_owned(),
        AudioCodec::Other(name) => name.clone(),
        AudioCodec::Unknown => "音频".to_owned(),
    });
    let streams = [video, audio]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" + ");
    let container = match option.container {
        Container::Mp4 => "MP4".to_owned(),
        Container::Matroska => "MKV".to_owned(),
        Container::Original => option.ext.to_uppercase(),
    };
    format!("{streams} · {container}")
}

/// 选中的行的底色。
trait Selected {
    fn when_selected(self, selected: bool) -> Self;
}

impl Selected for gpui_kit::Stateful<Div> {
    fn when_selected(self, selected: bool) -> Self {
        if selected {
            self.bg(theme::alpha(theme::accent(), 0.16))
                .text_color(theme::text())
        } else {
            self.text_color(theme::muted())
                .hover(|style| style.text_color(theme::text()))
        }
    }
}
