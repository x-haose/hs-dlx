//! 窗口里对一个任务的操作：交给 [`crate::tasks::Tasks`] 做，这里管窗口自己的那部分（放弃要再点一次确认、
//! 请求头编辑框的开关）。

use gpui_kit::{AppContext as _, Context, Window};
use hs_dlx_app::TaskId;

use super::Shell;
use crate::headers::HeadersEditor;

impl Shell {
    /// 继续：开始了就关掉请求头编辑框（没开始时编辑框里填的留着）。
    pub(super) fn resume(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if self.tasks.update(cx, |tasks, cx| tasks.resume(id, cx)) {
            self.editing_headers = None;
        }
    }

    /// 不再续录，只写出已录的：开始了就关掉请求头编辑框。
    pub(super) fn write_recorded(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if self
            .tasks
            .update(cx, |tasks, cx| tasks.write_recorded(id, cx))
        {
            self.editing_headers = None;
        }
    }

    /// 改存本地 HLS 再跑：开始了就关掉请求头编辑框。
    pub(super) fn switch_to_hls(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if self
            .tasks
            .update(cx, |tasks, cx| tasks.switch_to_hls(id, cx))
        {
            self.editing_headers = None;
        }
    }

    pub(super) fn switch_to_matroska(&mut self, id: TaskId, cx: &mut Context<Self>) {
        if self
            .tasks
            .update(cx, |tasks, cx| tasks.switch_to_matroska(id, cx))
        {
            self.editing_headers = None;
        }
    }

    /// 放弃任务：要删已下载内容的，第一次点只是要求确认。
    pub(super) fn discard(&mut self, id: TaskId, confirmed: bool, cx: &mut Context<Self>) {
        if !confirmed {
            self.confirm_discard = Some(id);
            cx.notify();
            return;
        }
        self.confirm_discard = None;
        self.tasks.update(cx, |tasks, cx| tasks.discard(id, cx));
    }

    /// 重新登录 PikPak：打开设置页的「网盘」一组（登录的进展在那里），在系统浏览器里打开确认页。
    pub(super) fn log_in_pikpak(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings_at(crate::settings::Section::Cloud, window, cx);
        self.tasks.update(cx, |tasks, cx| tasks.login_pikpak(cx));
    }

    /// 打开停着的任务 `id` 的请求头编辑框，填上它现在的请求头。
    pub(super) fn edit_headers(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spec) = self.tasks.read(cx).row(id).map(|row| row.task.spec.clone()) else {
            return;
        };
        let editor =
            cx.new(|cx| HeadersEditor::new(spec.url, spec.headers, Vec::new(), window, cx));
        editor.update(cx, |editor, cx| editor.focus(window, cx));
        self.editing_headers = Some((id, editor));
        cx.notify();
    }

    /// 存下编辑框里的请求头，`resume` 时接着续传续录；编辑框里还没按回车的一条加不进去时什么也不做（原因显示在编辑
    /// 框里），存不进时编辑框留着（原因在提示里）。
    pub(super) fn save_headers(
        &mut self,
        resume: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((id, editor)) = self.editing_headers.clone() else {
            return;
        };
        let Some(headers) = editor.update(cx, |editor, cx| editor.confirm(window, cx)) else {
            return;
        };
        let saved = self
            .tasks
            .update(cx, |tasks, cx| tasks.set_headers(id, headers, resume, cx));
        if saved {
            self.editing_headers = None;
        }
        cx.notify();
    }
}
