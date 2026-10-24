//! 界面事件泵的 Git 终态事件：GitDone 按共享进度定格全部界面字段后收尾（G-13）（自 ui_pump_events.rs 拆分）。
use super::{push_event_log, AppWindow, Ordering, RuntimeMode, UiPump};

impl UiPump {
    /// Event::GitDone 的处理：终态可能先于首次周期刷新到达，收尾时从共享进度定格
    /// 全部 Git 界面字段（G-13），随后清运行态并上屏最终文案。
    pub(super) fn finish_git_task(&self, ui: &AppWindow, text: String) {
        {
            let mut s = self.state.borrow_mut();
            s.control = None;
            s.runtime = RuntimeMode::Organizer;
            push_event_log(&mut s.logs, text.clone());
            self.log_dirty.set(true);
            // G-13：收尾定格全部字段；终态可能先于首次周期刷新到达。
            if let Some(shared) = &s.git_shared {
                if let Ok(branch) = shared.branch.lock() {
                    ui.set_git_branch(branch.as_str().into());
                }
                if let Ok(upstream) = shared.upstream.lock() {
                    ui.set_git_upstream(upstream.as_str().into());
                }
                if let Ok(current) = shared.current.lock() {
                    ui.set_git_current(current.as_str().into());
                }
                ui.set_git_retry(
                    i32::try_from(shared.retry.load(Ordering::Relaxed)).unwrap_or(i32::MAX),
                );
                ui.set_git_retry_wait(
                    i32::try_from(shared.retry_wait.load(Ordering::Relaxed)).unwrap_or(i32::MAX),
                );
                if let Ok(state_text) = shared.state.lock() {
                    ui.set_git_state(state_text.as_str().into());
                }
                if let Ok(stage) = shared.stage.lock() {
                    ui.set_git_stage(stage.as_str().into());
                }
                ui.set_git_done(
                    i32::try_from(shared.done.load(Ordering::Relaxed)).unwrap_or(i32::MAX),
                );
                ui.set_git_total(
                    i32::try_from(shared.total.load(Ordering::Relaxed)).unwrap_or(i32::MAX),
                );
            }
            s.git_shared = None;
        }
        ui.set_busy(false);
        ui.set_paused(false);
        ui.set_progress(-1.0);
        ui.set_progress_note("".into());
        ui.set_status(text.into());
        if self.close_after_if_idle(ui) {
            let _ = slint::quit_event_loop();
        }
    }
}
