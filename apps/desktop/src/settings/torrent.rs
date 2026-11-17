//! 「BT」一组：下完后的做种、上传限速、BT 经的代理与网络检测。

use std::num::NonZeroU32;

use gpui_kit::{Context, Div, FontWeight, ParentElement as _, Styled as _, div, px};
use hs_dlx_app::Seeding;

use super::{SettingsView, row};
use crate::ui::{self, Tone};

/// 做种的几种选择：名字与设置。「分享率 1.0 或 24 小时」为默认（见 [`Seeding::default`]）
const SEEDING_CHOICES: [(&str, &str); 3] = [
    ("seed-off", "下完不做种"),
    ("seed-default", "分享率 1.0 或 24 小时"),
    ("seed-forever", "一直做，手动停"),
];

/// 上传限速的几档：名字与字节每秒（None 为不限）。
const UPLOAD_CHOICES: [(&str, &str, Option<u32>); 4] = [
    ("upload-none", "不限", None),
    ("upload-512k", "512 KB/s", Some(512 << 10)),
    ("upload-2m", "2 MB/s", Some(2 << 20)),
    ("upload-10m", "10 MB/s", Some(10 << 20)),
];

impl SettingsView {
    /// 换做种的设置：存进了才选中，在做种的随即照它判停不停。
    fn pick_seeding(&mut self, picked: Seeding, cx: &mut Context<Self>) {
        match self.tasks.read(cx).service().set_seeding(picked) {
            Ok(()) => {
                self.seeding = picked;
                self.seeding_error = None;
            }
            Err(error) => self.seeding_error = Some(error.to_string()),
        }
        cx.notify();
    }

    /// 换上传限速：存进了才选中。
    fn pick_upload(&mut self, picked: Option<NonZeroU32>, cx: &mut Context<Self>) {
        match self.tasks.read(cx).service().set_upload_limit(picked) {
            Ok(()) => {
                self.upload_limit = picked;
                self.seeding_error = None;
            }
            Err(error) => self.seeding_error = Some(error.to_string()),
        }
        cx.notify();
    }

    /// 做种与上传限速两行；存着的不是这里的几种选择之一时都不选中。
    pub(super) fn render_seeding(&self, cx: &mut Context<Self>) -> [Div; 2] {
        let choices = [
            Seeding::Off,
            Seeding::default(),
            Seeding::Until {
                ratio_percent: None,
                minutes: None,
            },
        ];
        let seeding = SEEDING_CHOICES
            .into_iter()
            .zip(choices)
            .map(|((id, label), value)| {
                ui::chip(
                    id,
                    label,
                    self.seeding == value,
                    cx.listener(move |this, _, _, cx| this.pick_seeding(value, cx)),
                )
            });
        let upload = UPLOAD_CHOICES.into_iter().map(|(id, label, limit)| {
            let value = limit.and_then(NonZeroU32::new);
            ui::chip(
                id,
                label,
                self.upload_limit == value,
                cx.listener(move |this, _, _, cx| this.pick_upload(value, cx)),
            )
        });
        [
            row(
                "做种",
                Some(
                    "BT 任务下完后接着上传给别人；改了随即对在做种的生效；退出应用时停下，下次打开接着做",
                ),
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(div().flex().flex_wrap().gap(px(8.)).children(seeding))
                    .children(self.seeding_error.clone().map(ui::error_text)),
            ),
            row(
                "上传限速",
                Some("BT 所有任务合计"),
                div().flex().flex_wrap().gap(px(8.)).children(upload),
            ),
        ]
    }

    /// 改 BT 代理：`via` 为经代理（填了端口时核对后存），否则改回直连；存进了才换。
    fn pick_torrent_proxy(&mut self, via: bool, cx: &mut Context<Self>) {
        self.torrent_proxy_error = None;
        if via {
            self.torrent_proxy_picked = true;
            if self.torrent_address.read(cx).has_port(cx) {
                self.torrent_address
                    .update(cx, |editor, cx| editor.save(cx));
            }
        } else {
            match self.tasks.read(cx).service().set_torrent_proxy(None) {
                Ok(()) => {
                    self.torrent_proxy = None;
                    self.torrent_proxy_picked = false;
                    // 检测结果（与在做的检测）是按旧的设置量的
                    self.network_report = None;
                    self.network_check = None;
                }
                Err(error) => self.torrent_proxy_error = Some(error.to_string()),
            }
        }
        cx.notify();
    }

    /// 存核对过的 BT 代理地址 `url`。
    pub(super) fn save_torrent_proxy(&mut self, url: hs_m3u8_core::Url, cx: &mut Context<Self>) {
        let saved = self
            .tasks
            .read(cx)
            .service()
            .set_torrent_proxy(Some(url.clone()))
            .map_err(|error| error.to_string());
        match saved {
            Ok(()) => {
                self.torrent_proxy = Some(url);
                self.torrent_proxy_error = None;
                // 检测结果（与在做的检测）是按旧的设置量的
                self.network_report = None;
                self.network_check = None;
            }
            Err(reason) => self.torrent_proxy_error = Some(reason),
        }
        cx.notify();
    }

    /// 开始网络检测；在做时什么也不做。
    fn check_network(&mut self, cx: &mut Context<Self>) {
        if self.network_check.is_some() {
            return;
        }
        let handle = self.tasks.read(cx).service().check_network();
        self.network_check = Some(cx.spawn(async move |this, cx| {
            let result = match handle.await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) => Err(format!("检测的任务没做完：{error}")),
            };
            // 关了时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                this.network_report = Some(result);
                this.network_check = None;
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// 网络检测一行。
    pub(super) fn render_network_check(&self, cx: &mut Context<Self>) -> Div {
        let running = self.network_check.is_some();
        let mut content = div().flex().flex_col().gap(px(12.)).child(
            div().flex().items_center().gap(px(12.)).child(ui::button(
                "check-network",
                if running {
                    "在检测…"
                } else {
                    "检测网络"
                },
                Tone::Plain,
                cx.listener(|this, _, _, cx| this.check_network(cx)),
            )),
        );
        match &self.network_report {
            Some(Ok(report)) => {
                content = content
                    .children(
                        report
                            .checks
                            .iter()
                            .map(|check| ui::check_line(check.level, &check.text)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(report.verdict.clone()),
                    );
            }
            Some(Err(reason)) => content = content.child(ui::error_text(reason.clone())),
            None => {}
        }
        row(
            "网络检测",
            Some("约 10 秒；设了 BT 代理时也从本机直连测一次；BT 用过之后才测端口映射与连入"),
            content,
        )
    }

    /// BT 代理一行。
    pub(super) fn render_torrent_proxy(&self, cx: &mut Context<Self>) -> Div {
        let picked = self.torrent_proxy_picked;
        let edited = self.torrent_address.read(cx).edited();
        let unsaved = match &self.torrent_proxy {
            None if picked => Some("保存之前照旧直连".to_owned()),
            Some(url) if edited => Some(format!(
                "改的还没保存，照旧经 {}",
                hs_m3u8_core::bare_url(url)
            )),
            _ => None,
        };
        let chips = div()
            .flex()
            .flex_wrap()
            .gap(px(8.))
            .child(ui::chip(
                "torrent-direct",
                "直连",
                !picked,
                cx.listener(|this, _, _, cx| this.pick_torrent_proxy(false, cx)),
            ))
            .child(ui::chip(
                "torrent-socks",
                "经 SOCKS5 代理",
                picked,
                cx.listener(|this, _, _, cx| this.pick_torrent_proxy(true, cx)),
            ));
        let input = picked.then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(self.torrent_address.clone())
                .child(ui::caption(
                    "主机为 IPv4 地址或主机名；代理要转发 UDP（UDP ASSOCIATE），DHT 才经得了它",
                ))
                .children(unsaved.map(ui::caption))
        });
        row(
            "BT 代理",
            Some("节点、tracker 与 DHT 经哪里出去；与「下载」里的代理分开设"),
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(chips)
                .children(input)
                .children(self.torrent_proxy_error.clone().map(ui::error_text))
                .child(ui::caption(
                    "很多代理服务禁止 BT，可能限速或封号，做种的上传也走代理；经代理时外面连不进来；改了之后的连接经它，已连着的不断",
                )),
        )
    }
}
