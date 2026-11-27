//! 退出进行到哪一步：问（窗口里的提示框）之后等任务停下、补写状态再退出；期间出了要用户看的事就不退出。

use gpui_kit::Context;
use hs_dlx_app::{Ask, Status};

use super::Tasks;
use super::row::Row;

/// 要走时该怎么办（见 [`Tasks::departure`]）。
pub enum Departure {
    /// 照办，不用问：已决定退出，或没有要等的任务、状态都已写进存储
    Now,
    /// 已在问或在等任务停下：这次不再问
    Underway,
    /// 没有要等的任务，但有任务的状态写不进存储：问要不要仍然退出
    Unsaved(hs_dlx_app::Error),
    /// 有 `busy` 个任务没停下；`recording` 为其中有在录的直播
    Busy { busy: usize, recording: bool },
}

/// 提示框在问的是什么。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    /// 退出应用（托盘、程序坞、注销，或没有要等的任务时关窗）
    App,
    /// 关窗；`quit_requested` 为问的期间又要退出应用，答完要按退出应用再问
    Window { quit_requested: bool },
}

/// 退出进行到哪一步。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Quit {
    Staying,
    /// 正在问要不要退出（提示框开着）：再来的请求不再问。`reopened` 为期间再次打开过应用（接手了转交），选了退出也
    /// 不退出
    Asking {
        asked: Asked,
        reopened: bool,
    },
    /// 用户选了退出：等运行中的任务都停下就退出；期间不开始新的运行，出了要用户看的事就不退出
    Waiting,
    /// 退出已不可撤回（已向系统发出退出）：不再接手转交来的链接
    Leaving,
}

impl Tasks {
    pub fn quit(&self) -> Quit {
        self.quit
    }

    /// 撤回过几次退出（[`Tasks::stay`]）：比较前后两次的值即知其间有没有撤回。问完接着再问
    /// （[`Tasks::stay_unanswered`]）不算撤回。
    pub fn stays(&self) -> u64 {
        self.stays
    }

    /// 有任务在准备、运行、继续、放弃或从列表移除：退出要等它们。新建与粘贴在途的不算：建成的随即暂停（见
    /// [`Tasks::apply`]），赶不上的由服务关闭时停下。
    pub fn busy(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.task.status, Status::Running | Status::Preparing) || !r.idle())
            .count()
    }

    /// 有在运行的直播录制（能停止并写出的）。
    pub fn recording(&self) -> bool {
        self.rows.iter().any(Row::stoppable)
    }

    /// 在退出（等任务停下或已决定退出）：不开始新的运行。
    pub fn quitting(&self) -> bool {
        matches!(self.quit, Quit::Waiting | Quit::Leaving)
    }

    /// 要走（关窗或退出应用）时该怎么办。没有要等的任务时先把没写进存储的状态补写进去。
    pub fn departure(&self) -> Departure {
        match self.quit {
            Quit::Leaving => Departure::Now,
            Quit::Asking { .. } | Quit::Waiting => Departure::Underway,
            Quit::Staying => match self.busy() {
                0 => match self.service.flush() {
                    Ok(()) => Departure::Now,
                    Err(error) => Departure::Unsaved(error),
                },
                busy => Departure::Busy {
                    busy,
                    recording: self.recording(),
                },
            },
        }
    }

    /// 开始问要不要退出（窗口里的提示框开着），问的是 `asked`。
    pub fn ask(&mut self, asked: Asked) {
        self.quit = Quit::Asking {
            asked,
            reopened: false,
        };
    }

    /// 正在问关窗时又要退出应用：记下，答完按退出应用再问（见 [`Quit::Asking`]）。问的已是退出应用时，这次的答复
    /// 就是答复；没在问时什么也不做。
    pub fn request_quit_while_asking(&mut self) {
        if let Quit::Asking {
            asked: Asked::Window { quit_requested },
            ..
        } = &mut self.quit
        {
            *quit_requested = true;
        }
    }

    /// 问完了、接着要再问一次（退出应用）：回到没在问，但不答复在等的系统退出请求（再问的答案去答复它；
    /// 与 [`Tasks::stay`] 的区别）。
    pub fn stay_unanswered(&mut self) {
        self.quit = Quit::Staying;
    }

    /// 选了退出：暂停在准备、运行的任务与停在网页上等用户的任务（`stop_recordings` 时直播录制改为停止并写出），等都
    /// 停下（继续、放弃也做完）就退出。
    pub fn quit_after_stopping(&mut self, stop_recordings: bool, cx: &mut Context<Self>) {
        self.quit = Quit::Waiting;
        for row in &self.rows {
            let stoppable = matches!(
                row.task.status,
                Status::Running | Status::Preparing | Status::Waiting(Ask::Page(_))
            );
            if !stoppable {
                continue;
            }
            if stop_recordings && row.stoppable() {
                self.service.stop(row.task.id);
            } else {
                self.service.pause(row.task.id);
            }
        }
        self.quit_if_idle(cx);
        cx.notify();
    }

    /// 不退出了：失败与没删干净的东西留给用户看；`why` 写进提示。已决定退出的照常退出。
    pub fn cancel_quit(&mut self, why: String) {
        if self.quit == Quit::Waiting {
            self.stay();
        }
        self.add_notice(why);
    }

    /// 选了退出时，提示框开着期间再次打开过应用（见 [`Tasks::receive`]）：不退出了，提示原因；没再次打开过时什么也
    /// 不做。返回是否再次打开过。
    pub fn reopened_while_asking(&mut self) -> bool {
        let reopened = matches!(self.quit, Quit::Asking { reopened: true, .. });
        if reopened {
            self.stay_after_reopen();
        }
        reopened
    }

    /// 选了退出，但提示框开着期间再次打开过应用：不退出了，提示原因。
    pub fn stay_after_reopen(&mut self) {
        self.stay();
        self.add_notice("提示框开着时再次打开了应用，没有退出".to_owned());
    }

    /// 不退出：在等答复的系统退出请求随之取消（注销、关机不再进行）。
    pub fn stay(&mut self) {
        self.quit = Quit::Staying;
        self.stays += 1;
        #[cfg(target_os = "macos")]
        hs_dlx_macos::reply_to_terminate(false);
    }

    /// 退出，不再撤回：有在等答复的系统退出请求时答复它（注销、关机接着进行），否则发出退出。
    pub fn leave(&mut self, cx: &mut Context<Self>) {
        self.quit = Quit::Leaving;
        #[cfg(target_os = "macos")]
        if hs_dlx_macos::reply_to_terminate(true) {
            return;
        }
        cx.quit();
    }

    /// 选了退出、任务都停下（继续、放弃也做完）时，把没写进存储的状态补写进去后退出；补写不进时不退出。
    pub(super) fn quit_if_idle(&mut self, cx: &mut Context<Self>) {
        if self.quit != Quit::Waiting || self.busy() > 0 {
            return;
        }
        match self.service.flush() {
            Ok(()) => self.leave(cx),
            Err(error) => self.cancel_quit(format!(
                "有任务的状态没能写进存储（{error}），没有退出；再关窗口时可以选择仍然退出"
            )),
        }
    }
}
