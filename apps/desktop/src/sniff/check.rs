//! 后台探测找到的流：一次一条，读出网页引擎里发往它的 Cookie，交给服务配请求头、探测。

use gpui_kit::Context;
use hs_dlx_app::page::{COOKIE_TIMEOUT, Check, Cookie};
use hs_dlx_webview::cookies::cookies_for;
use hs_m3u8_core::Url;

use super::SniffView;
use crate::wait::within;

/// 网页引擎给的 Cookie 换成服务用的写法。
pub(super) fn for_service(cookies: Vec<hs_dlx_webview::cookies::Cookie>) -> Vec<Cookie> {
    cookies
        .into_iter()
        .map(|cookie| Cookie {
            name: cookie.name,
            value: cookie.value,
            cross_site: cookie.cross_site,
        })
        .collect()
}

/// 等网页引擎回出 Cookie 最多 [`COOKIE_TIMEOUT`]，到时为原因。
pub(super) fn cookie_timeout() -> String {
    format!("网页引擎 {} 秒没有回出 Cookie", COOKIE_TIMEOUT.as_secs())
}

impl SniffView {
    /// 没在探测时，探测下一条等着的流。网页建不出来时不探测（列表里写着原因）。
    pub(super) fn check_next(&mut self, cx: &mut Context<Self>) {
        if self.checking.is_some() {
            return;
        }
        let Some(webview) = self.webview().cloned() else {
            return;
        };
        let Some(found) = self
            .found
            .iter_mut()
            .find(|found| matches!(found.check, Check::Waiting))
        else {
            return;
        };
        found.check = Check::Running;
        let cookies = cookies_for(webview.read(cx).raw(), &found.url);
        let stream = found.stream();
        let service = self.service.clone();
        self.checking = Some(cx.spawn(async move |this, cx| {
            let timer = cx.background_executor().timer(COOKIE_TIMEOUT);
            let cookies = within(std::pin::pin!(cookies), timer)
                .await
                .unwrap_or_else(|| Err(cookie_timeout()))
                .map(for_service);
            let (check, skipped) = match service.check_stream(stream, cookies).await {
                Ok(checked) => checked,
                Err(join) => (
                    Check::Failed {
                        reason: format!("探测中途出错：{join}"),
                        headers: None,
                    },
                    0,
                ),
            };
            // 关了时不再需要结果
            let _closed = this.update(cx, |this, cx| {
                this.checking = None;
                // 一次只探测一条；换页时列表清空、探测随之取消：探测中的就是这条
                let entry = this
                    .found
                    .iter_mut()
                    .find(|entry| matches!(entry.check, Check::Running))
                    .expect("探测中的流在探测完之前一直在列表里");
                entry.check = check;
                entry.skipped = skipped;
                this.check_next(cx);
                cx.notify();
            });
        }));
    }

    /// 没成的流重新排队探测（重新读 Cookie）：`row` 为列表里那一条（见 [`SniffView::row`]），None 为全部没成的。
    pub(super) fn retry(&mut self, row: Option<(usize, &Url)>, cx: &mut Context<Self>) {
        let one = row.and_then(|(index, url)| self.row(index, url));
        for (index, found) in self.found.iter_mut().enumerate() {
            if matches!(found.check, Check::Failed { .. }) && (row.is_none() || one == Some(index))
            {
                found.check = Check::Waiting;
            }
        }
        self.check_next(cx);
        cx.notify();
    }
}
