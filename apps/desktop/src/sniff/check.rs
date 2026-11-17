//! 后台探测找到的流：一次一条，读出网页引擎里发往它的 Cookie、配好请求头、探测。

use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{BackgroundExecutor, Context};
use hs_dlx_app::{Failure, Service};
use hs_dlx_webview::cookies::{Cookie, cookies_for};
use hs_m3u8_core::{Source, Url};

use super::found::Check;
use super::{SniffView, request};
use crate::wait::within;

/// 等网页引擎回出 Cookie 最多多久：引擎卡住时不让探测一直挂着。
pub(super) const COOKIE_TIMEOUT: Duration = Duration::from_secs(5);
/// 探测一条流最多多久，到时取消（连同核心在途的请求与重试）：源站不回应时不让后面的一直排着。核心默认连接超时 15
/// 秒、第一次重试前等约 0.5 秒，30 秒容得下一次连接超时后再试一次。
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

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
        let target = request::Stream {
            url: found.url.clone(),
            frame: found.frame.clone(),
            top: found.top.clone(),
            agent: found.agent.clone(),
        };
        let service = self.service.clone();
        self.checking = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let (check, skipped) = run(&service, cookies, &target, &executor).await;
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

/// 读出 Cookie、配好请求头、探测；返回结果与没带上的 Cookie 个数。
async fn run(
    service: &Rc<Service>,
    cookies: impl Future<Output = Result<Vec<Cookie>, String>>,
    target: &request::Stream,
    executor: &BackgroundExecutor,
) -> (Check, usize) {
    let cookies = within(std::pin::pin!(cookies), executor.timer(COOKIE_TIMEOUT))
        .await
        .unwrap_or_else(|| {
            Err(format!(
                "网页引擎 {} 秒没有回出 Cookie",
                COOKIE_TIMEOUT.as_secs()
            ))
        });
    let made = cookies.and_then(|cookies| request::headers_for(target, &cookies));
    let made = match made {
        Ok(made) => made,
        Err(reason) => {
            return (
                Check::Failed {
                    reason,
                    headers: None,
                },
                0,
            );
        }
    };
    let mut source = Source::new(target.url.clone());
    source.http.headers.clone_from(&made.headers);
    let failed = |reason| Check::Failed {
        reason,
        headers: Some(made.headers.clone()),
    };
    let probing = match service.probe(source) {
        Ok(probing) => probing,
        Err(error) => return (failed(error.to_string()), made.skipped),
    };
    // 超时即释放句柄、取消探测
    let probing = std::pin::pin!(probing);
    let check = match within(probing, executor.timer(PROBE_TIMEOUT)).await {
        Some(Ok(Ok(probe))) => Check::Ready {
            probe: Box::new(probe),
            headers: made.headers.clone(),
        },
        Some(Ok(Err(error))) => failed(Failure::of(&error).message),
        Some(Err(join)) => failed(format!("探测中途出错：{join}")),
        None => failed(format!(
            "探测 {} 秒没有结果，已停下",
            PROBE_TIMEOUT.as_secs()
        )),
    };
    (check, made.skipped)
}
