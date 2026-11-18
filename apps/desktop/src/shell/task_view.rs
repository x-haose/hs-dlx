//! 选中任务的舞台与检查面板：舞台中间是表盘（直播录制是钟面），检查面板是读数、信号与操作。

use std::time::{Duration, Instant};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Task, div,
    linear_color_stop, linear_gradient, px,
};
use hs_dlx_app::web::Container;
use hs_dlx_app::{Check, FailureKind, TaskId};
use hs_dlx_app::{Status, TorrentPhase};
use hs_m3u8_core::bare_url;

use super::look::{Look, look};
use super::plan::{Action, Instead, Plan, stopped_plan};
use super::{Shell, live_view};
use crate::signals::{Level, Signal};
use crate::tasks::{Amount, Disk, FileAmount, Row, main_output, resume_label, saved_as};
use crate::ui::{self, GAP, INSPECTOR_MIN_WIDTH, Solid, Tone};
use crate::{dial, disk_access, format, motion, ring, theme};

/// 在系统的文件管理器里显示输出：各平台的叫法。
const REVEAL: &str = if cfg!(target_os = "macos") {
    "在访达中显示"
} else if cfg!(target_os = "windows") {
    "在资源管理器中显示"
} else {
    "在文件夹中显示"
};
/// 信号时间线：竖线离左边多远、最新几条不变淡、最新一条的节点呼吸一次多久。
const SIGNAL_RAIL: f32 = 9.5;
const SIGNAL_FRESH: usize = 3;
const SIGNAL_BREATH: Duration = Duration::from_millis(2400);
/// 任务页的最小宽：表盘与收到最窄的检查面板。
pub(super) const MIN_WIDTH: f32 = ring::AREA + GAP + INSPECTOR_MIN_WIDTH;
/// 任务页的最小高：放得下表盘与粘贴栏的舞台。
pub(super) const MIN_HEIGHT: f32 = ui::stage_height(ring::AREA, 0.);

/// 一个 BT 任务的诊断：在诊断中，或诊断的结果（诊断不了时为原因）。只对诊断时那一次运行（任务停过、开始或停止了做种
/// 之后不再显示）。
pub(super) struct Diagnosis {
    id: TaskId,
    /// 诊断时任务的运行：停下的次数（[`Row::reading`]）与在不在做种
    run: (u64, bool),
    result: Option<Result<Vec<Check>, String>>,
    _running: Task<()>,
}

/// 检查面板底栏里的操作：最该按的一个与其余的。
struct Foot {
    primary: Option<Stateful<Div>>,
    rest: Vec<Stateful<Div>>,
}

impl Shell {
    /// 舞台与检查面板。
    pub(super) fn render_task(&self, row: &Row, now: Instant, cx: &Context<Self>) -> (Div, Div) {
        let look = look(row);
        // 直播录制（没完成的）画钟面
        let live = row.recording() && row.task.status != Status::Done;
        let figure = if live {
            live_view::figure(row, &look)
        } else {
            let ticks = look.ticks;
            dial::dial(
                look.color,
                dial::figure(move |bounds, window, _| {
                    ring::paint_ticks(bounds, ring::RADIUS, &ticks, window)
                }),
                dial::readout(
                    look.state.clone(),
                    look.color,
                    dial::Big::Figure(look.big.clone()),
                    look.line.clone(),
                ),
            )
        };
        let stage = ui::stage(
            ring::AREA,
            figure,
            self.clipboard_hint(cx),
            self.dock.clone(),
        );
        (stage, self.inspector(row, &look, live, now, cx))
    }

    /// 剪贴板里有链接、粘贴栏空着时，粘贴栏上面的一行。
    fn clipboard_hint(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let url = self.clipboard.clone()?;
        if !self.dock.read(cx).text(cx).is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .max_w_full()
                .text_size(px(12.))
                .text_color(theme::muted())
                .child(ui::num(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(format!("剪贴板：{}", bare_url(&url))),
                ))
                .child(
                    div()
                        .id("probe-clipboard-hint")
                        .flex_none()
                        .text_color(theme::accent())
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme::accent_pale()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.probe_text(url.as_str(), window, cx)
                        }))
                        .child("探测"),
                )
                .into_any_element(),
        )
    }

    fn inspector(
        &self,
        row: &Row,
        look: &Look,
        live: bool,
        now: Instant,
        cx: &Context<Self>,
    ) -> Div {
        let host = crate::links::source(&row.task.spec.url);
        let mut body: Vec<AnyElement> = Vec::new();
        if let Some(card) = self.headers_card(row, cx) {
            body.push(card.into_any_element());
        }
        let cells = if live {
            live_view::reads(row)
        } else {
            reads(row, now)
        };
        if !cells.is_empty() {
            body.push(ui::reads(cells, None).into_any_element());
        }
        if !live && row.task.status == Status::Running {
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(ui::caption("速度 · 最近 60 秒"))
                    .child(
                        div()
                            .px(px(10.))
                            .py(px(8.))
                            .rounded(px(10.))
                            .bg(theme::ground())
                            .border_1()
                            .border_color(theme::edge())
                            .child(ui::speed_matrix(row.rate.history(now).to_vec())),
                    )
                    .into_any_element(),
            );
        }
        if live {
            body.push(live_view::legend(row).into_any_element());
        }
        if let Some(section) = self.diagnosis_section(row, cx) {
            body.push(section.into_any_element());
        }
        body.push(signals(&row.signals).into_any_element());
        let Foot { primary, rest } = self.foot(row, cx);
        let mut foot: Vec<AnyElement> = Vec::new();
        let denied = self.tasks.read(cx).cookies_denied();
        match (&row.task.status, denied) {
            (Status::Failed(failure), Some(browser)) if failure.kind == FailureKind::WebCookies => {
                foot.push(disk_access::card(browser).into_any_element());
            }
            _ => {
                if let Some(hint) = look.hint {
                    foot.push(ui::note(hint, true).into_any_element());
                }
            }
        }
        foot.extend(primary.map(IntoElement::into_any_element));
        if !rest.is_empty() {
            foot.push(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(10.))
                    .children(rest)
                    .into_any_element(),
            );
        }
        ui::inspector(row.task.spec.title.clone(), host, body, foot)
    }

    /// 在下或做种的 BT 任务的诊断一节：没诊断过时是「诊断」按钮，诊断中写在诊断，诊断过的逐项列出结论（可再诊断）。
    fn diagnosis_section(&self, row: &Row, cx: &Context<Self>) -> Option<Div> {
        let active = row.task.status == Status::Running || row.task.seeding;
        if row.task.spec.torrent().is_none() || !active {
            return None;
        }
        let id = row.task.id;
        let run = (row.reading, row.task.seeding);
        let shown = self
            .diagnosis
            .as_ref()
            .filter(|diagnosis| diagnosis.id == id && diagnosis.run == run);
        let button = |label: &'static str| {
            ui::button(
                "diagnose",
                label,
                Tone::Plain,
                cx.listener(move |this, _, _, cx| this.diagnose(id, run, cx)),
            )
        };
        let body: AnyElement = match shown.map(|diagnosis| &diagnosis.result) {
            None => button("诊断网络与节点").into_any_element(),
            Some(None) => ui::note("在诊断…", false).into_any_element(),
            Some(Some(Err(reason))) => div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(ui::error_text(reason.clone()))
                .child(button("再诊断一次"))
                .into_any_element(),
            Some(Some(Ok(checks))) => div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .children(
                    checks
                        .iter()
                        .map(|check| ui::check_line(check.level, &check.text).into_any_element()),
                )
                .child(button("再诊断一次"))
                .into_any_element(),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(ui::caption("诊断"))
                .child(body),
        )
    }

    /// 诊断 BT 任务 `id`（这时的运行为 `run`），结果写进诊断一节；之前的结果作废。
    fn diagnose(&mut self, id: TaskId, run: (u64, bool), cx: &mut Context<Self>) {
        let handle = self.tasks.read(cx).service().diagnose(id);
        let running = cx.spawn(async move |this, cx| {
            let result = match handle.await {
                Ok(Ok(checks)) => Ok(checks),
                Ok(Err(error)) => Err(error.to_string()),
                Err(join) => Err(format!("诊断中途出错：{join}")),
            };
            // 诊断期间窗口关掉时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                if let Some(diagnosis) = this.diagnosis.as_mut().filter(|d| d.id == id) {
                    diagnosis.result = Some(result);
                    cx.notify();
                }
            });
        });
        self.diagnosis = Some(Diagnosis {
            id,
            run,
            result: None,
            _running: running,
        });
        cx.notify();
    }

    /// 正在改这个任务的请求头时，检查面板最上面的编辑卡片。
    fn headers_card(&self, row: &Row, cx: &Context<Self>) -> Option<Div> {
        let (_, editor) = self
            .editing_headers
            .as_ref()
            .filter(|(id, _)| *id == row.task.id)?;
        let resume = self.plan(row).resume_after_headers;
        let failed = matches!(row.task.status, Status::Failed(_));
        let label = if resume {
            format!("保存并{}", resume_label(failed, row.task.write_only))
        } else {
            "保存".to_owned()
        };
        Some(
            ui::well()
                .flex()
                .flex_col()
                .gap(px(12.))
                .p(px(14.))
                // 正在编辑：描边比别的卡片亮一档
                .border_color(theme::edge())
                .child(ui::caption("请求头"))
                .child(editor.clone())
                .child(
                    div()
                        .flex()
                        .gap(px(10.))
                        .child(
                            ui::button(
                                "save-headers",
                                label,
                                Tone::Glow,
                                cx.listener(move |this, _, window, cx| {
                                    this.save_headers(resume, window, cx)
                                }),
                            )
                            .flex_1(),
                        )
                        .child(ui::button(
                            "cancel-headers",
                            "取消",
                            Tone::Ghost,
                            cx.listener(|this, _, _, cx| {
                                this.editing_headers = None;
                                cx.notify();
                            }),
                        )),
                ),
        )
    }

    /// 停着的任务 `row` 的操作（见 [`stopped_plan`]）。
    fn plan(&self, row: &Row) -> Plan {
        let failure = match &row.task.status {
            Status::Failed(failure) => Some(failure.kind),
            _ => None,
        };
        let editing = self
            .editing_headers
            .as_ref()
            .is_some_and(|(at, _)| *at == row.task.id);
        let headers = row.task.spec.torrent().is_none() && row.task.spec.cloud().is_none();
        let instead = match (row.task.spec.hls(), row.task.spec.web()) {
            (Some(_), _) => Instead::Hls,
            (None, Some(web)) if web.container == Container::Mp4 => Instead::Matroska,
            (None, _) => Instead::Nothing,
        };
        stopped_plan(
            failure,
            row.recording(),
            row.task.write_only,
            headers,
            editing,
            instead,
        )
    }

    /// 暂停或失败的任务的操作（见 [`stopped_plan`]）：第一个是主操作（失败的为警告色），其余在下面一行。
    fn stopped_foot(&self, row: &Row, discard: Stateful<Div>, cx: &Context<Self>) -> Foot {
        let id = row.task.id;
        let failed = matches!(row.task.status, Status::Failed(_));
        let plan = self.plan(row);
        let mut primary = None;
        let mut rest = Vec::new();
        for (index, action) in plan.actions.iter().enumerate() {
            let (key, label, detail): (&'static str, String, Option<SharedString>) = match action {
                Action::EditHeaders => (
                    "edit-headers",
                    "改请求头".to_owned(),
                    plan.resume_after_headers.then(|| "保存后接着重试".into()),
                ),
                Action::WriteRecorded => (
                    "write-recorded",
                    "写出已录的".to_owned(),
                    // 不知道已录多久（任务目录读不出、还没报进度）时不写，不写成 00:00
                    Amount::of(row).map(|amount| {
                        format!("不联网，写出 {}", format::clock(amount.duration)).into()
                    }),
                ),
                Action::SwitchToHls => (
                    "switch-hls",
                    "改存本地 HLS".to_owned(),
                    Some("已下的照样用".into()),
                ),
                Action::SwitchToMatroska => (
                    "switch-mkv",
                    "改用 MKV".to_owned(),
                    Some("已下的照样用".into()),
                ),
                Action::Resume => (
                    "resume",
                    resume_label(failed, row.task.write_only).to_owned(),
                    None,
                ),
                Action::OpenFolder => ("open-folder", "打开保存目录".to_owned(), None),
                Action::LogIn => (
                    "log-in",
                    "重新登录 PikPak".to_owned(),
                    Some("在浏览器里确认，之后再继续".into()),
                ),
                Action::LocalTorrent => ("local-torrent", "改用本机 BT".to_owned(), None),
            };
            let act = *action;
            let folder = folder_of(&row.task.spec);
            let link = row.task.spec.url.clone();
            let on_click = cx.listener(move |this, _, window, cx| match act {
                Action::EditHeaders => this.edit_headers(id, window, cx),
                Action::WriteRecorded => this.write_recorded(id, cx),
                Action::SwitchToHls => this.switch_to_hls(id, cx),
                Action::SwitchToMatroska => this.switch_to_matroska(id, cx),
                Action::Resume => this.resume(id, cx),
                Action::OpenFolder => cx.open_with_system(&folder),
                Action::LogIn => this.log_in_pikpak(window, cx),
                Action::LocalTorrent => this.probe_text(link.as_str(), window, cx),
            });
            if index == 0 {
                let solid = if failed { Solid::Amber } else { Solid::Glow };
                primary = Some(ui::primary(key, label, detail, solid, true, on_click));
            } else {
                rest.push(ui::button(key, label, Tone::Plain, on_click).flex_1());
            }
        }
        rest.push(discard);
        Foot { primary, rest }
    }

    fn foot(&self, row: &Row, cx: &Context<Self>) -> Foot {
        if !row.idle() {
            return Foot {
                primary: None,
                rest: Vec::new(),
            };
        }
        let id = row.task.id;
        let confirming = self.confirm_discard == Some(id);
        let discard = |cx: &Context<Self>| {
            let button = ui::button(
                "discard",
                if confirming {
                    "再点一次：删除已下的内容"
                } else {
                    "放弃任务"
                },
                if confirming { Tone::Plain } else { Tone::Ghost },
                cx.listener(move |this, _, _, cx| this.discard(id, confirming, cx)),
            );
            // 确认时独占一行，写清后果
            if confirming {
                button
                    .w_full()
                    .border_color(theme::warn())
                    .text_color(theme::warn())
            } else {
                button
            }
        };
        let pause = |cx: &Context<Self>| {
            ui::button(
                "pause",
                "暂停",
                Tone::Plain,
                cx.listener(move |this, _, _, cx| this.tasks.read(cx).pause(id)),
            )
            .flex_1()
        };
        let forget = |tone: Tone, cx: &Context<Self>| {
            ui::button(
                "forget",
                "从列表移除",
                tone,
                cx.listener(move |this, _, _, cx| {
                    this.tasks.update(cx, |tasks, cx| tasks.forget(id, cx))
                }),
            )
        };
        match &row.task.status {
            Status::DiscardUnfinished => Foot {
                primary: None,
                rest: vec![discard(cx).flex_1()],
            },
            Status::Running if row.stoppable() => Foot {
                primary: Some(ui::primary(
                    "stop",
                    "停止并写出",
                    Amount::of(row).map(|amount| {
                        format!("写出已录的 {}", format::clock(amount.duration)).into()
                    }),
                    Solid::Rec,
                    true,
                    cx.listener(move |this, _, _, cx| {
                        this.tasks.read(cx).stop(id);
                        cx.notify();
                    }),
                )),
                rest: vec![pause(cx), discard(cx)],
            },
            Status::Running => Foot {
                primary: None,
                rest: vec![pause(cx), discard(cx)],
            },
            // 没有自己的任务目录（与别的任务共用、迁移时让了出去）：不能继续，放弃会删别人的，只能移除记录。BT 任务本就
            // 没有任务目录，云离线任务列出文件之前也没有
            Status::Paused | Status::Failed(_)
                if row.task.work_dir.is_none()
                    && row.task.spec.torrent().is_none()
                    && row.task.spec.cloud().is_none() =>
            {
                Foot {
                    primary: None,
                    rest: vec![forget(Tone::Plain, cx).flex_1()],
                }
            }
            Status::Paused | Status::Failed(_) => self.stopped_foot(row, discard(cx), cx),
            Status::Done => {
                // 输出在不在是完成时与打开应用时在后台查的；还没查完时先不给这两个按钮
                let (mp4, shown) = row
                    .present
                    .as_ref()
                    .map_or((None, None), |p| (p.mp4.clone(), p.shown.clone()));
                let primary = mp4.map(|mp4| {
                    ui::primary(
                        "play",
                        "播放",
                        Some(row.file_name.clone().into()),
                        Solid::Glow,
                        true,
                        move |_, _, cx| cx.open_with_system(&mp4),
                    )
                });
                let mut rest = Vec::new();
                if let Some(shown) = shown {
                    rest.push(
                        ui::button("reveal", REVEAL, Tone::Plain, move |_, _, cx| {
                            cx.reveal_path(&shown)
                        })
                        .flex_1(),
                    );
                }
                // BT：做种时只能停止做种（停了才能从列表移除），没在做种时可以再做
                if row.task.spec.torrent().is_some() {
                    if row.task.seeding {
                        rest.push(ui::button(
                            "stop-seeding",
                            "停止做种",
                            Tone::Ghost,
                            cx.listener(move |this, _, _, cx| {
                                this.tasks
                                    .update(cx, |tasks, cx| tasks.stop_seeding(id, cx));
                                cx.notify();
                            }),
                        ));
                        return Foot { primary, rest };
                    }
                    rest.push(ui::button(
                        "seed",
                        "做种",
                        Tone::Ghost,
                        cx.listener(move |this, _, _, cx| {
                            this.tasks.update(cx, |tasks, cx| tasks.seed(id, cx))
                        }),
                    ));
                }
                rest.push(forget(Tone::Ghost, cx));
                Foot { primary, rest }
            }
        }
    }
}

/// 检查面板的读数（直播录制的见 [`live_view::reads`]）：下载中为速度、剩余、已下与分片，写出中为已下与分片，停着的为
/// 已下的片数与大小，完成的为时长、大小、画面与输出；还没有内容时为空。
fn reads(row: &Row, now: Instant) -> Vec<(SharedString, SharedString)> {
    let cell = |label: &'static str, value: String| (SharedString::from(label), value.into());
    if row.task.status == Status::Done {
        let Some(result) = row.task.result.as_ref() else {
            return Vec::new();
        };
        if row.task.seeding
            && let Some(progress) = row
                .torrent_progress
                .as_ref()
                .filter(|progress| progress.phase == TorrentPhase::Seeding)
        {
            return vec![
                cell("上传", format!("{}/s", format::bytes(progress.upload_rate))),
                cell("已上传", format::bytes(progress.uploaded)),
                cell("分享率", format::ratio(progress.uploaded, progress.wanted)),
                cell(
                    "节点",
                    // 两边都下完了的节点互相断开（libtorrent 的 close_redundant_connections），做种时连着的都是在下的
                    progress.peers.to_string(),
                ),
            ];
        }
        let mut cells = Vec::new();
        if row.task.spec.hls().is_some() {
            cells.push(cell(
                "时长",
                result
                    .duration
                    .map_or_else(|| "—".to_owned(), format::clock),
            ));
        }
        cells.push(cell("大小", format::bytes(result.bytes)));
        if let Some((width, height)) = result.resolution {
            cells.push(cell("画面", format!("{width}×{height}")));
        }
        cells.push(cell("保存为", saved_as(&row.task.spec).to_owned()));
        if let Some(skipped) = cloud_skipped(row) {
            cells.push(skipped);
        }
        return cells;
    }
    if row.task.spec.torrent().is_some() {
        return torrent_reads(row, now);
    }
    if row.task.spec.cloud().is_some() {
        return cloud_reads(row, now);
    }
    if row.task.spec.web().is_some() {
        return web_reads(row, now);
    }
    if row.task.spec.hls().is_none() {
        return file_reads(row, now);
    }
    let Some(amount) = Amount::of(row) else {
        return Vec::new();
    };
    let Amount {
        done, total, bytes, ..
    } = amount;
    let segments = if total > 0 {
        format!("{done} / {total}")
    } else {
        done.to_string()
    };
    match row.task.status {
        Status::Running if row.written_share().is_some() => {
            vec![cell("已下", format::bytes(bytes)), cell("分片", segments)]
        }
        Status::Running if total > 0 && !row.task.write_only => {
            let speed = format!("{}/s", format::bytes(row.rate.bytes_per_second(now) as u64));
            let left = row
                .rate
                .remaining(now, total.saturating_sub(done))
                .map_or_else(|| "—".to_owned(), format::duration);
            vec![
                cell("速度", speed),
                cell("剩余", left),
                cell("已下", format::bytes(bytes)),
                cell("分片", segments),
            ]
        }
        Status::Running => Vec::new(),
        _ if done == 0 => Vec::new(),
        _ => vec![
            cell("已下", format!("{segments} 片")),
            cell("大小", format::bytes(bytes)),
        ],
    }
}

/// 文件任务的 [`reads`]（完成的除外）：在下时为速度、剩余、已下与连接数，停着的为已下与总大小；还没有内容时为空。
fn file_reads(row: &Row, now: Instant) -> Vec<(SharedString, SharedString)> {
    let cell = |label: &'static str, value: String| (SharedString::from(label), value.into());
    let Some(amount) = FileAmount::of(row) else {
        return Vec::new();
    };
    let size = amount
        .total
        .map_or_else(|| "不知道".to_owned(), format::bytes);
    match (&row.task.status, &row.file_progress) {
        (Status::Running, Some(progress)) if progress.limit > 0 => {
            let speed = row.rate.bytes_per_second(now);
            // 按最近几秒的速度估：剩下的字节除以速度
            let left = amount
                .total
                .map(|total| total.saturating_sub(amount.bytes))
                .filter(|_| speed > 0.0)
                .map_or_else(
                    || "—".to_owned(),
                    |left| format::duration(Duration::from_secs_f64(left as f64 / speed)),
                );
            vec![
                cell("速度", format!("{}/s", format::bytes(speed as u64))),
                cell("剩余", left),
                cell("已下", format::bytes(amount.bytes)),
                cell(
                    "连接",
                    format!("{} / {}", progress.connections, progress.limit),
                ),
            ]
        }
        (Status::Running, _) => Vec::new(),
        _ if amount.bytes == 0 => Vec::new(),
        _ => vec![
            cell("已下", format::bytes(amount.bytes)),
            cell("大小", size),
        ],
    }
}

/// 网页视频任务的 [`reads`]（完成的除外）：下载时为速度、剩余、已下；别的时候为空（停着的已下要重新解析才知道）。
fn web_reads(row: &Row, now: Instant) -> Vec<(SharedString, SharedString)> {
    let cell = |label: &'static str, value: String| (SharedString::from(label), value.into());
    let fetching = matches!(
        row.web_progress,
        Some(hs_dlx_app::WebProgress::Fetching { .. })
    );
    let Some(amount) = FileAmount::of(row).filter(|_| fetching) else {
        return Vec::new();
    };
    let speed = row.rate.bytes_per_second(now);
    let left = amount
        .total
        .map(|total| total.saturating_sub(amount.bytes))
        .filter(|_| speed > 0.0)
        .map_or_else(
            || "—".to_owned(),
            |left| format::duration(Duration::from_secs_f64(left as f64 / speed)),
        );
    vec![
        cell("速度", format!("{}/s", format::bytes(speed as u64))),
        cell("剩余", left),
        cell("已下", format::bytes(amount.bytes)),
    ]
}

/// BT 任务的 [`reads`]（完成的除外）：在下时为速度、剩余、已下、要下的大小、节点数（其中做种的）与上传速度，停着的
/// 为已下与要下的大小；还没有内容时为空。
fn torrent_reads(row: &Row, now: Instant) -> Vec<(SharedString, SharedString)> {
    let cell = |label: &'static str, value: String| (SharedString::from(label), value.into());
    let Some(amount) = FileAmount::of(row) else {
        return Vec::new();
    };
    let wanted = amount.total.unwrap_or(0);
    match (&row.task.status, &row.torrent_progress) {
        (Status::Running, Some(progress)) => {
            let speed = row.rate.bytes_per_second(now);
            // 按最近几秒的速度估：剩下的字节除以速度
            let left = if speed > 0.0 {
                let left = wanted.saturating_sub(amount.bytes);
                format::duration(Duration::from_secs_f64(left as f64 / speed))
            } else {
                "—".to_owned()
            };
            vec![
                cell("速度", format!("{}/s", format::bytes(speed as u64))),
                cell("剩余", left),
                cell("已下", format::bytes(amount.bytes)),
                cell("大小", format::bytes(wanted)),
                cell(
                    "节点",
                    format!("{}（做种 {}）", progress.peers, progress.seeds),
                ),
                cell("上传", format!("{}/s", format::bytes(progress.upload_rate))),
            ]
        }
        (Status::Running, None) => Vec::new(),
        _ if amount.bytes == 0 => Vec::new(),
        _ => vec![
            cell("已下", format::bytes(amount.bytes)),
            cell("大小", format::bytes(wanted)),
        ],
    }
}

/// 云离线任务的 [`reads`]（完成的除外）：取回时为速度、剩余、已取回与文件数，网盘在下时为网盘的进度，停着的为已
/// 取回与总大小；还没有内容时为空。
fn cloud_reads(row: &Row, now: Instant) -> Vec<(SharedString, SharedString)> {
    use hs_dlx_app::CloudProgress;
    let cell = |label: &'static str, value: String| (SharedString::from(label), value.into());
    let amount = FileAmount::of(row);
    match (&row.task.status, &row.cloud_progress, &amount) {
        (Status::Running, Some(CloudProgress::Remote { percent, .. }), _) => {
            vec![cell("网盘", format!("{percent}%"))]
        }
        (Status::Running, Some(CloudProgress::Fetching { index, count, .. }), Some(amount)) => {
            let speed = row.rate.bytes_per_second(now);
            let total = amount.total.unwrap_or(0);
            let left = if speed > 0.0 {
                let left = total.saturating_sub(amount.bytes);
                format::duration(Duration::from_secs_f64(left as f64 / speed))
            } else {
                "—".to_owned()
            };
            vec![
                cell("速度", format!("{}/s", format::bytes(speed as u64))),
                cell("剩余", left),
                cell("已取回", format::bytes(amount.bytes)),
                cell("文件", format!("{} / {count}", index + 1)),
            ]
        }
        (Status::Running, _, _) => Vec::new(),
        (_, _, Some(amount)) if amount.bytes > 0 => {
            let mut cells = vec![
                cell("已取回", format::bytes(amount.bytes)),
                cell("大小", format::bytes(amount.total.unwrap_or(0))),
            ];
            cells.extend(cloud_skipped(row));
            cells
        }
        _ => Vec::new(),
    }
}

/// 云离线任务因用户在网盘上删了而跳过的文件数（不算进大小），没有时为 None。
fn cloud_skipped(row: &Row) -> Option<(SharedString, SharedString)> {
    let Disk::Cloud(files) = &row.disk else {
        return None;
    };
    let skipped = files.iter().filter(|file| file.skipped).count();
    (skipped > 0).then(|| {
        (
            "跳过".into(),
            format!("{skipped} 个网盘上删了的文件").into(),
        )
    })
}

/// 输出所在的目录：占着输出路径的东西就在里面；还没定下内容的根的云离线任务为它的保存目录。
fn folder_of(spec: &hs_dlx_app::TaskSpec) -> std::path::PathBuf {
    match (main_output(spec), spec.cloud()) {
        (Some(output), _) => output
            .parent()
            .expect("输出是以文件名结尾的绝对路径")
            .to_path_buf(),
        (None, Some(cloud)) => cloud.folder.clone(),
        (None, None) => unreachable!("只有云离线任务会还没有输出"),
    }
}

/// 信号：带框的时间线，最新的在最上面。左边一条竖线串起各条的节点。要留意与失败级别的各条整行带同色的淡底与描边，
/// 节点是同色的空心圈；其余的节点是暗的空心圈，最新一条是一般级别时它的节点实心发光、缓缓呼吸。较早的（第
/// [`SIGNAL_FRESH`] 条往后）变淡。每条下面一行是时刻与连续发生的次数。
fn signals(signals: &[Signal]) -> Div {
    let accent = theme::accent();
    let last = signals.len().saturating_sub(1);
    let rows = signals.iter().rev().enumerate().map(|(index, signal)| {
        let level = match signal.level {
            Level::Info => None,
            Level::Warn => Some(theme::signal_warn()),
            Level::Error => Some(theme::signal_error()),
        };
        let newest = index == 0;
        let breath = motion::breathe(SIGNAL_BREATH);
        let node = div()
            .absolute()
            .left(px(SIGNAL_RAIL - 4.5))
            .top(px(11.))
            .size(px(9.))
            .rounded_full()
            .border_2()
            .map(|node| match (level, newest) {
                (Some(color), _) => node.bg(theme::ground()).border_color(color),
                (None, true) => node
                    .bg(accent)
                    .border_color(accent)
                    .shadow(ui::glow(accent, 10., breath)),
                (None, false) => node.bg(theme::ground()).border_color(theme::accent_dim()),
            });
        // 竖线：第一条从节点往下，最后一条到节点为止
        let rail = div()
            .absolute()
            .left(px(SIGNAL_RAIL - 0.5))
            .w(px(1.))
            .bg(theme::edge())
            .map(|rail| match (index == 0, index == last) {
                (true, true) => rail.h(px(0.)),
                (true, false) => rail.top(px(14.)).bottom_0(),
                (false, true) => rail.top_0().h(px(14.)),
                (false, false) => rail.top_0().bottom_0(),
            });
        let when = if signal.repeat > 1 {
            format!("{} · ×{}", signal.at, signal.repeat)
        } else {
            signal.at.clone()
        };
        div()
            .relative()
            .pl(px(26.))
            .pr(px(8.))
            .py(px(7.))
            .rounded(px(10.))
            .when_some(level, |row, color| {
                row.bg(theme::alpha(color, 0.07))
                    .border_1()
                    .border_color(theme::alpha(color, 0.22))
            })
            .when(index >= SIGNAL_FRESH, |row| row.opacity(0.55))
            .child(rail)
            .child(node)
            .child(
                div()
                    .text_size(px(12.5))
                    .line_height(px(18.))
                    .text_color(match (level, newest) {
                        (Some(color), _) => color,
                        (None, true) => theme::text(),
                        (None, false) => theme::soft(),
                    })
                    .child(SharedString::from(signal.text.clone())),
            )
            .child(ui::num(
                div()
                    .mt(px(2.))
                    .text_size(px(11.))
                    .text_color(theme::faint())
                    .child(when),
            ))
    });
    div()
        .flex()
        .flex_col()
        .px(px(12.))
        .pt(px(6.))
        .pb(px(8.))
        .rounded(px(14.))
        .border_1()
        .border_color(theme::edge())
        .bg(linear_gradient(
            180.,
            linear_color_stop(theme::alpha(accent, 0.04), 0.),
            linear_color_stop(theme::alpha(accent, 0.), 0.4),
        ))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px(px(2.))
                .pt(px(6.))
                .pb(px(8.))
                .child(ui::caption("信号"))
                .child(ui::num(
                    div()
                        .px(px(7.))
                        .rounded(px(8.))
                        .bg(theme::alpha(gpui_kit::white(), 0.05))
                        .text_size(px(11.))
                        .text_color(theme::muted())
                        .child(signals.len().to_string()),
                )),
        )
        .children(rows)
}
