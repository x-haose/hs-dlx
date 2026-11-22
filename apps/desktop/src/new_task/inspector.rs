//! 新建页右边的检查面板：标题与主机；探测中为进行到哪一步与请求经哪里走；认出播放列表为探测结果的读数、清晰度之外
//! 的选择（音轨、输出、保存位置、请求头）与开始；认出文件为大小、类型、能否续传与保存位置、请求头、开始；探测失败为
//! 错误与能做的事（改请求头、重新探测、网页视频要登录或被拦截时到网页里登录、在网页里找视频流）；磁力链接取种子信息时为连着的节点数，取到后（种子
//! 文件读到后）为大小、要下的文件、保存位置与开始。磁力链接在这几步都可以改用 PikPak 云离线。地址是网页时：yt-dlp
//! 解析中只写请求经哪里走，组件没装时为安装与进度，解析出来后为时长、上传者、画质选项、保存位置与开始（见 web.rs）。

use gpui_kit::{
    AnyElement, Context, Div, IntoElement, ParentElement as _, SharedString, Styled as _, div, px,
};
use gpui_kit::{InteractiveElement as _, StatefulInteractiveElement as _};
use hs_dlx_app::{Failure, FailureKind, MetadataProgress, OutputKind};
use hs_m3u8_core::{FileInfo, Probe};

use super::{NewTask, NewTaskEvent, PageCheck, Probing, Route, TorrentChoice, duration_of};

/// 磁力链接取种子信息多久还没有节点时提示去检测网络。
const NO_PEERS_HINT: std::time::Duration = std::time::Duration::from_secs(60);
use crate::disk_access;
use crate::format;
use crate::theme;
use crate::ui::{self, Solid, Step, Tone};

impl NewTask {
    pub(super) fn render_inspector(&self, cx: &mut Context<Self>) -> Div {
        let host = crate::links::source(&self.url);
        let title = self.title.clone();
        match &self.probing {
            Probing::Metadata(progress) => self.metadata_inspector(title, host, progress, cx),
            Probing::Torrent(choice) => self.torrent_inspector(title, host, choice, cx),
            Probing::Failed(failure) if self.is_torrent() => {
                self.torrent_failed_inspector(title, host, failure, cx)
            }
            // 探测是一次请求，中间没有可报的步骤：表盘写探测了多久，这里只写请求经哪里走
            Probing::Running => ui::inspector(
                title,
                host,
                self.reads_with_route(Vec::new()),
                [self.cancel(Tone::Plain, cx)],
            ),
            Probing::Failed(_) if self.page_check == PageCheck::Opening => ui::inspector(
                title,
                host,
                [ui::steps([
                    ("取到的不是播放列表".into(), Step::Done),
                    ("在网页里打开，看是不是网页".into(), Step::Now),
                ])
                .into_any_element()],
                [self.cancel(Tone::Plain, cx)],
            ),
            Probing::Failed(failure) => self.failed_inspector(title, host, failure, cx),
            Probing::Ready(probe) => self.ready_inspector(title, host, probe, cx),
            Probing::File(info) => self.file_inspector(title, host, info, cx),
            Probing::Resolving => ui::inspector(
                title,
                host,
                self.reads_with_route(Vec::new()),
                [self.cancel(Tone::Plain, cx)],
            ),
            Probing::NeedsWeb(state) => self.needs_web_inspector(title, host, state, cx),
            Probing::Web(choice) => self.web_inspector(title, host, choice, cx),
        }
    }

    /// 退出新建页的按钮，独占底栏一行。
    pub(super) fn cancel(&self, tone: Tone, cx: &mut Context<Self>) -> AnyElement {
        ui::button(
            "cancel",
            "取消",
            tone,
            cx.listener(|_, _, _, cx| cx.emit(NewTaskEvent::Dismissed)),
        )
        .w_full()
        .into_any_element()
    }

    /// 「网络」一格：这个地址的请求经哪个代理或为什么直连；代理用不了时为「用不了」与原因。还没读到系统代理时为 None。
    fn route(&self) -> Option<((SharedString, SharedString), Option<String>)> {
        let (value, broken) = match self.route.as_ref()? {
            Route::Proxy(proxy, source) => (format!("经 {proxy}（{source}）"), None),
            Route::Direct(why) => (format!("直连 · {why}"), None),
            Route::Broken(why) => ("用不了".to_owned(), Some(why.clone())),
        };
        Some((("网络".into(), value.into()), broken))
    }

    /// 读数格 `cells`，后面接一格占满一行的「网络」（读到系统代理之后），代理用不了时下面写原因。
    /// 两样都没有时为 None。
    pub(super) fn reads_with_route(
        &self,
        cells: Vec<(SharedString, SharedString)>,
    ) -> Option<AnyElement> {
        let (route, broken) = self.route().unzip();
        if cells.is_empty() && route.is_none() {
            return None;
        }
        let shown = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(ui::reads(cells, route))
            .children(broken.flatten().map(ui::error_text))
            .into_any_element();
        Some(shown)
    }

    /// 探测失败：错误、这类失败怎么办（见 [`format::failure`]；要登录时换成怎么登录）、看过不是网页的说明、请求经哪里
    /// 走、缺请求头时的编辑框；底栏是重新探测与另找。被拒或取到错误页的多半缺请求头，改了再探测；再试也一样的不给重新
    /// 探测。网页视频要登录时主按钮是到网页里登录（嗅探页里登录、回到视频页后点「解析网页视频」，带着登录后的 Cookie
    /// 解析；从嗅探页来的回嗅探页），另给重新探测（在常用的浏览器里登录了、或在设置里选了从浏览器读之后，再解析时现
    /// 读）；被拦截时另给到网页里登录。不是从嗅探页来的还可以在网页里打开它找视频流。
    fn failed_inspector(
        &self,
        title: String,
        host: String,
        failure: &Failure,
        cx: &mut Context<Self>,
    ) -> Div {
        let headers = failure.kind.headers_likely_missing();
        let needs_login = failure.kind == FailureKind::WebLogin;
        let login = needs_login && !self.from_sniff;
        let again = headers || failure.kind.retry_helps();
        let mut body = vec![ui::note(failure.message.clone(), true).into_any_element()];
        if !needs_login {
            body.push(ui::note(format::failure(failure.kind).1, false).into_any_element());
        }
        // 读不了浏览器的数据：失败的说明里已写了怎么开权限，这里给直达的按钮
        if failure.kind == FailureKind::WebCookies && self.tasks.read(cx).cookies_denied().is_some()
        {
            body.extend(disk_access::button().map(IntoElement::into_any_element));
        }
        if let PageCheck::NotPage(note) = &self.page_check {
            body.push(ui::note(note.clone(), false).into_any_element());
        }
        if needs_login {
            let how = if self.from_sniff {
                "回到网页里登录，回到视频页后再点顶上的「解析网页视频」；或在设置的「网页视频 → 登录信息」里选你常用的浏览器（已选的在它里面登录这个网站），再点重新探测"
            } else {
                "在打开的网页里登录，回到视频页后点顶上的「解析网页视频」；或在设置的「网页视频 → 登录信息」里选你常用的浏览器（已选的在它里面登录这个网站），再点重新探测"
            };
            body.push(ui::note(how.to_owned(), false).into_any_element());
        }
        body.extend(self.reads_with_route(Vec::new()));
        if headers {
            body.push(
                group("请求头", [self.headers.clone().into_any_element()]).into_any_element(),
            );
        }
        let mut foot = Vec::new();
        if login {
            let url = self.url.clone();
            foot.push(
                ui::primary(
                    "login",
                    "到网页里登录",
                    None,
                    Solid::Amber,
                    true,
                    cx.listener(move |_, _, _, cx| cx.emit(NewTaskEvent::Sniff(url.clone()))),
                )
                .into_any_element(),
            );
        } else if needs_login {
            // 从嗅探页来的：退出新建页即回到嗅探页
            foot.push(
                ui::primary(
                    "back-to-page",
                    "回到网页里登录",
                    None,
                    Solid::Amber,
                    true,
                    cx.listener(|_, _, _, cx| cx.emit(NewTaskEvent::Dismissed)),
                )
                .into_any_element(),
            );
        } else if again {
            foot.push(
                ui::primary(
                    "reprobe",
                    "重新探测",
                    None,
                    Solid::Amber,
                    true,
                    cx.listener(|this, _, window, cx| this.reprobe(window, cx)),
                )
                .into_any_element(),
            );
        }
        let mut row = Vec::new();
        if needs_login && again {
            row.push(
                ui::button(
                    "reprobe",
                    "重新探测",
                    Tone::Plain,
                    cx.listener(|this, _, window, cx| this.reprobe(window, cx)),
                )
                .flex_1()
                .into_any_element(),
            );
        }
        if !self.from_sniff && !needs_login {
            let url = self.url.clone();
            row.push(
                ui::button(
                    "sniff",
                    if failure.kind == FailureKind::WebBlocked {
                        "到网页里登录"
                    } else {
                        "在网页里找视频流"
                    },
                    Tone::Plain,
                    cx.listener(move |_, _, _, cx| cx.emit(NewTaskEvent::Sniff(url.clone()))),
                )
                .flex_1()
                .into_any_element(),
            );
        }
        // 没有别的按钮时「返回」独占一行，描边
        let alone = row.is_empty();
        let back = ui::button(
            "dismiss",
            "返回",
            if alone { Tone::Plain } else { Tone::Ghost },
            cx.listener(|_, _, _, cx| cx.emit(NewTaskEvent::Dismissed)),
        );
        row.push(if alone { back.flex_1() } else { back }.into_any_element());
        foot.push(div().flex().gap(px(10.)).children(row).into_any_element());
        ui::inspector(title, host, body, foot)
    }

    /// 认出文件时：大小、类型、能否续传与网络的读数，保存位置、请求头，开始。
    fn file_inspector(
        &self,
        title: String,
        host: String,
        info: &FileInfo,
        cx: &mut Context<Self>,
    ) -> Div {
        let cells: Vec<(SharedString, SharedString)> = vec![
            (
                "大小".into(),
                info.length
                    .map_or_else(|| "不知道".to_owned(), format::bytes)
                    .into(),
            ),
            (
                "类型".into(),
                info.content_type
                    .clone()
                    .unwrap_or_else(|| "不知道".to_owned())
                    .into(),
            ),
            (
                "续传".into(),
                if info.accepts_ranges {
                    "支持，可多个连接".into()
                } else {
                    "不支持，中断后从头下".into()
                },
            ),
        ];
        let mut body: Vec<AnyElement> = self.reads_with_route(cells).into_iter().collect();
        body.push(self.folder_group(cx).into_any_element());
        body.push(group("请求头", [self.headers.clone().into_any_element()]).into_any_element());
        let headline = if self.creating {
            "正在开始…".to_owned()
        } else {
            "开始下载".to_owned()
        };
        let detail = match info.length {
            Some(length) => format!("{} · {}", self.title, format::bytes(length)),
            None => self.title.clone(),
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
                Some(detail.into()),
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

    /// 磁力链接取种子信息时：连着的节点数；种子文件读的时候什么也没有。
    fn metadata_inspector(
        &self,
        title: String,
        host: String,
        progress: &MetadataProgress,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut body = Vec::new();
        if self.url.scheme() == "magnet" {
            body.push(
                ui::reads(
                    vec![("节点".into(), progress.peers.to_string().into())],
                    None,
                )
                .into_any_element(),
            );
            body.push(
                ui::note("没有节点有这个种子时会一直找，可随时取消", false).into_any_element(),
            );
            // 连上了节点时 tracker 报错不妨碍取种子信息（多为种子带的几个 tracker 里有连不上的），不显示
            if let Some(error) = &progress.tracker_error
                && progress.peers == 0
            {
                body.push(ui::note(format!("tracker 报错：{error}"), true).into_any_element());
            }
            if let Some(error) = &progress.public_trackers_error {
                body.push(
                    ui::note(
                        format!("公共 tracker 列表没取到，隔一阵再取：{error}"),
                        true,
                    )
                    .into_any_element(),
                );
            }
            if let Some(problem) = &progress.proxy_problem {
                body.push(ui::note(format!("BT 代理：{problem}"), true).into_any_element());
            }
            if progress.peers == 0 && self.since.elapsed() >= NO_PEERS_HINT {
                body.push(
                    ui::note(
                        "一直找不到节点时，可到设置里的「网络检测」看是哪里不通",
                        false,
                    )
                    .into_any_element(),
                );
            }
        }
        let mut foot = self.cloud_offer(cx);
        foot.push(self.cancel(Tone::Plain, cx));
        ui::inspector(title, host, body, foot)
    }

    /// 磁力链接改用 PikPak 云离线：新建的没成（多为没登录）时的原因与按钮；不是磁力链接、正在新建时为空。存到的位置
    /// 同本机下载。
    fn cloud_offer(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.url.scheme() != "magnet" || self.creating {
            return Vec::new();
        }
        let mut offer: Vec<AnyElement> = self
            .error
            .clone()
            .map(|error| ui::error_text(error).into_any_element())
            .into_iter()
            .collect();
        offer.push(
            ui::button(
                "cloud",
                "改用 PikPak 云离线",
                Tone::Plain,
                cx.listener(|this, _, _, cx| this.start_cloud(cx)),
            )
            .into_any_element(),
        );
        offer
    }

    /// 取不到种子信息、读不出种子文件：错误与这类失败怎么办（见 [`format::failure`]），能再试的给再试。
    fn torrent_failed_inspector(
        &self,
        title: String,
        host: String,
        failure: &Failure,
        cx: &mut Context<Self>,
    ) -> Div {
        let body = [
            ui::note(failure.message.clone(), true).into_any_element(),
            ui::note(format::failure(failure.kind).1, false).into_any_element(),
        ];
        let mut foot = self.cloud_offer(cx);
        if failure.kind.retry_helps() {
            foot.push(
                ui::primary(
                    "retry",
                    "再试一次",
                    None,
                    Solid::Amber,
                    true,
                    cx.listener(|this, _, window, cx| this.reprobe(window, cx)),
                )
                .into_any_element(),
            );
        }
        foot.push(self.cancel(Tone::Plain, cx));
        ui::inspector(title, host, body, foot)
    }

    /// 取到种子信息后：选了的大小与文件数（私有种子写明只经种子里的 tracker 找节点），要下的文件，保存位置，开始。
    fn torrent_inspector(
        &self,
        title: String,
        host: String,
        choice: &TorrentChoice,
        cx: &mut Context<Self>,
    ) -> Div {
        let (count, bytes) = choice.selected();
        let total: u64 = choice.info.files.iter().map(|file| file.size).sum();
        let cells: Vec<(SharedString, SharedString)> = vec![
            (
                "大小".into(),
                format!("{} / {}", format::bytes(bytes), format::bytes(total)).into(),
            ),
            (
                "文件".into(),
                format!("{count} / {} 个", choice.info.files.len()).into(),
            ),
        ];
        let mut body = vec![ui::reads(cells, None).into_any_element()];
        if choice.info.private {
            body.push(ui::note("私有种子：只经种子里的 tracker 找节点", false).into_any_element());
        }
        let all = count == choice.info.files.len();
        let toggle_all = ui::chip(
            "select-all",
            if all { "全不选" } else { "全选" },
            false,
            cx.listener(move |this, _, _, cx| this.select_all(!all, cx)),
        )
        .into_any_element();
        // 简化：文件逐个画出，种子里有上千个文件时改用只画看得见的那些的列表
        let files = choice
            .info
            .files
            .iter()
            .zip(&choice.wanted)
            .enumerate()
            .map(|(at, (file, wanted))| {
                file_row(at, &file.path, file.size, *wanted)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(at, cx)))
                    .into_any_element()
            });
        body.push(
            group(
                "要下的文件",
                [
                    chips_row([toggle_all]),
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .children(files)
                        .into_any_element(),
                ],
            )
            .into_any_element(),
        );
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
                Some(format!("{count} 个文件 · {}", format::bytes(bytes)).into()),
                Solid::Glow,
                !self.creating && count > 0,
                cx.listener(|this, _, window, cx| this.start(window, cx)),
            )
            .into_any_element(),
        );
        if !self.creating && self.url.scheme() == "magnet" {
            foot.push(
                ui::button(
                    "cloud",
                    "改用 PikPak 云离线",
                    Tone::Plain,
                    cx.listener(|this, _, _, cx| this.start_cloud(cx)),
                )
                .into_any_element(),
            );
        }
        if !self.creating {
            foot.push(self.cancel(Tone::Ghost, cx));
        }
        ui::inspector(title, host, body, foot)
    }

    /// 「保存到」一组：现在的保存位置与更改。
    pub(super) fn folder_group(&self, cx: &mut Context<Self>) -> Div {
        group(
            "保存到",
            [div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(ui::field(format::path(&self.folder, self.home.as_deref())).flex_1())
                .child(ui::button(
                    "folder",
                    "更改",
                    Tone::Plain,
                    cx.listener(|this, _, _, cx| this.change_folder(cx)),
                ))
                .into_any_element()],
        )
    }

    /// 认出播放列表：读数、选择（多了可以滚动）与开始。
    fn ready_inspector(
        &self,
        title: String,
        host: String,
        probe: &Probe,
        cx: &mut Context<Self>,
    ) -> Div {
        let master = probe.master.as_ref();
        let duration = duration_of(probe);
        // （下标、名字、选项上写的、选中）
        let audio_options: Vec<(usize, String, String, bool)> = master
            .map(|m| {
                let group = m.variants[self.variant].audio_group.clone();
                m.audio
                    .iter()
                    .filter(|a| a.separate && Some(&a.group) == group.as_ref())
                    .map(|a| {
                        let name = a
                            .name
                            .clone()
                            .or_else(|| a.language.clone())
                            .unwrap_or_else(|| format!("音轨 {}", a.index + 1));
                        let on = self.audio == Some(a.index) || (self.audio.is_none() && a.default);
                        let label = if a.default {
                            format!("{name} · 默认")
                        } else {
                            name.clone()
                        };
                        (a.index, name, label, on)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let audio_label = audio_options
            .iter()
            .find(|(_, _, _, on)| *on)
            .map(|(_, name, _, _)| name.clone());

        let mut cells: Vec<(SharedString, SharedString)> = vec![if probe.live {
            ("类型".into(), "直播".into())
        } else {
            ("时长".into(), format::clock(duration).into())
        }];
        cells.push((
            "清晰度".into(),
            master
                .map_or_else(|| "单一".to_owned(), |m| format!("{} 档", m.variants.len()))
                .into(),
        ));
        let separate = master.map_or(0, |m| m.audio.iter().filter(|a| a.separate).count());
        if separate > 0 {
            cells.push(("音轨".into(), format!("{separate} 条").into()));
        }
        let mut body: Vec<AnyElement> = self.reads_with_route(cells).into_iter().collect();

        if !audio_options.is_empty() {
            let chips = audio_options.into_iter().map(|(index, _, label, on)| {
                ui::chip(
                    ("audio", index),
                    label,
                    on,
                    cx.listener(move |this, _, _, cx| {
                        this.audio = Some(index);
                        cx.notify();
                    }),
                )
                .into_any_element()
            });
            body.push(group("音轨", [chips_row(chips)]).into_any_element());
        }
        let outputs = [OutputKind::Mp4, OutputKind::Hls, OutputKind::Both].map(|kind| {
            ui::chip(
                output_label(kind),
                output_label(kind),
                self.output == kind,
                cx.listener(move |this, _, _, cx| {
                    this.output = kind;
                    cx.notify();
                }),
            )
            .into_any_element()
        });
        body.push(group("输出", [chips_row(outputs)]).into_any_element());
        body.push(self.folder_group(cx).into_any_element());
        body.push(group("请求头", [self.headers.clone().into_any_element()]).into_any_element());

        let picked = master.map(|m| &m.variants[self.variant]);
        let verb = if probe.live {
            "开始录制"
        } else {
            "开始下载"
        };
        let headline = if self.creating {
            "正在开始…".to_owned()
        } else {
            picked.map_or_else(
                || verb.to_owned(),
                |v| format!("{verb} · {}", format::variant(v)),
            )
        };
        let detail: Vec<String> = [
            picked
                .and_then(|v| v.bandwidth)
                .filter(|_| !probe.live)
                .map(|bw| format::size_bound(bw, duration)),
            audio_label,
            Some(output_label(self.output).to_owned()),
        ]
        .into_iter()
        .flatten()
        .collect();
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
                Some(detail.join(" · ").into()),
                Solid::Glow,
                !self.creating,
                cx.listener(|this, _, window, cx| this.start(window, cx)),
            )
            .into_any_element(),
        );
        // 开始了就不能取消，任务建成后照常出现在任务栏里
        if !self.creating {
            foot.push(self.cancel(Tone::Ghost, cx));
        }
        ui::inspector(title, host, body, foot)
    }
}

/// 输出选项上写的名字。
fn output_label(kind: OutputKind) -> &'static str {
    match kind {
        OutputKind::Mp4 => "MP4",
        OutputKind::Hls => "本地 HLS",
        OutputKind::Both => "两者",
    }
}

/// 种子里的一个文件：勾选框、相对路径（过长截断）与大小；点整行勾上或去掉。
fn file_row(at: usize, path: &str, size: u64, wanted: bool) -> gpui_kit::Stateful<Div> {
    let mark = div()
        .flex_none()
        .size(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .text_size(px(11.));
    let mark = if wanted {
        mark.bg(theme::accent())
            .border_color(theme::accent())
            .text_color(theme::ground())
            .child("✓")
    } else {
        mark.border_color(theme::edge())
    };
    div()
        .id(("torrent-file", at))
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(8.))
        .py(px(6.))
        .rounded(px(8.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::raised()))
        .child(mark)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(13.))
                .text_color(if wanted {
                    theme::text()
                } else {
                    theme::muted()
                })
                .child(path.to_owned()),
        )
        .child(ui::num(
            div()
                .flex_none()
                .text_size(px(12.))
                .text_color(theme::muted())
                .child(format::bytes(size)),
        ))
}

/// 带小标题的一组。
pub(super) fn group(title: &'static str, children: impl IntoIterator<Item = AnyElement>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(ui::caption(title))
        .children(children)
}

/// 一行可换行的选项。
fn chips_row(chips: impl IntoIterator<Item = AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .gap(px(8.))
        .children(chips)
        .into_any_element()
}
