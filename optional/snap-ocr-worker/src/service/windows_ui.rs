//! `Service` 的界面与窗口方法：结果窗暂存/恢复、进度窗、设置窗与状态刷新。
//! 从 `service.rs` 按内聚职责原样搬移；方法补 `pub(super)` 以维持原模块内调用关系。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slint::ComponentHandle;

use super::autostart::{autostart_value, AutostartValue};
use super::selection_cancel_is_current;
use super::{ActivityPhase, Command, ModelState, Service};
use crate::result_window::{ProgressWindow, ResultWindowHandle, SettingsWindow};

impl Service {
    pub(super) fn task_span(&self) -> tracing::Span {
        self.diagnostic_task
            .as_ref()
            .map_or_else(tracing::Span::none, |(span, _)| span.clone())
    }

    pub(super) fn finish_task(&mut self, result: &'static str, stage: &'static str) {
        if let Some((span, started)) = self.diagnostic_task.take() {
            let _entered = span.enter();
            if result == "failed" {
                tracing::warn!(
                    event = "ocr_task_failed",
                    result,
                    stage,
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图任务失败"
                );
            } else {
                tracing::info!(
                    event = "ocr_task_completed",
                    result,
                    stage,
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图任务结束"
                );
            }
        }
    }
    /// 任务终态统一收尾：依次结束任务计时日志、回到空闲、隐藏进度窗并恢复
    /// 旧结果；带托盘提示的调用处在 `end_task` 之后自行 `tray.notice(...)`。
    pub(super) fn end_task(&mut self, result: &'static str, stage: &'static str) {
        self.finish_task(result, stage);
        self.phase = ActivityPhase::Idle;
        self.hide_progress();
        self.restore_old();
    }
    pub(super) fn status(&self) -> Value {
        // S8-05：损坏的 Run 值不得计入已启用（保持 bool 口径兼容主界面），
        // 并以独立字段呈现损坏状态。
        let autostart = autostart_value().unwrap_or(AutostartValue::Absent);
        json!({"ok":true,"exiting":self.exit_pending.is_some(),"shared_xberg_protocol":2,"background_service_protocol":1,"model":self.model.as_str(),"task":if self.phase == ActivityPhase::Idle {"idle"} else {"recognizing"},
            "hotkey":self.settings.hotkey,"autostart":matches!(autostart, AutostartValue::Current | AutostartValue::Stale),
            "autostart_corrupt":autostart == AutostartValue::Corrupt,
            "error":self.model_error.as_ref().or(self.settings.warning.as_ref())})
    }
    pub(super) fn hide_old(&mut self) {
        self.old_result_visible = self
            .result
            .as_ref()
            .is_some_and(ResultWindowHandle::is_visible);
        if self.old_result_visible {
            if let Some(result) = &self.result {
                if let Some(window) = result.as_weak().upgrade() {
                    let _ = window.hide();
                }
            }
        }
    }
    pub(super) fn restore_old(&mut self) {
        if self.old_result_visible {
            if let Some(result) = &self.result {
                if let Some(window) = result.as_weak().upgrade() {
                    let _ = window.show();
                }
            }
            self.old_result_visible = false;
        }
    }
    /// O-22/O-29：关闭组件也释放惰性属性缓存；O-20 暂存的隐藏旧结果除外。
    pub(super) fn release_closed_result(&mut self) {
        if !self.old_result_visible
            && self
                .result
                .as_ref()
                .is_some_and(|result| !result.is_visible())
        {
            drop(self.result.take());
        }
    }
    pub(super) fn show_progress(&mut self, stage: &str) -> Result<(), String> {
        if self.progress.is_none() {
            let window = ProgressWindow::new().map_err(|_| "识别进度窗口创建失败".to_string())?;
            crate::result_window::apply_progress_theme(&window);
            let weak = self.self_weak.clone();
            window.on_cancel_requested(move || {
                if let Some(service) = weak.upgrade() {
                    service.borrow_mut().cancel_recognition();
                }
            });
            let weak = self.self_weak.clone();
            window.window().on_close_requested(move || {
                if let Some(service) = weak.upgrade() {
                    service.borrow_mut().cancel_recognition();
                }
                slint::CloseRequestResponse::KeepWindowShown
            });
            self.progress = Some(window);
        }
        let window = self
            .progress
            .as_ref()
            .ok_or_else(|| "识别进度窗口不可用".to_string())?;
        crate::result_window::apply_progress_theme(window);
        window.set_stage(stage.into());
        window.set_cancelling(false);
        window
            .show()
            .map_err(|_| "识别进度窗口显示失败".to_string())
    }
    pub(super) fn hide_progress(&self) {
        if let Some(window) = self.progress.as_ref() {
            let _ = window.hide();
        }
    }
    pub(super) fn request_selection_cancel(
        cancel: Arc<AtomicBool>,
        generation_token: Arc<AtomicU64>,
        generation: u64,
    ) {
        if crate::capture_win::cancel_selection() {
            return;
        }
        // 退出请求可能与托盘的 Trigger 消息竞态：遮罩尚未创建时，单次投递
        // 会落空。短暂重试只发送 WM_DONE，不读取或保存截图内容。
        let span = tracing::Span::current();
        std::thread::spawn(move || {
            let _entered = span.enter();
            tracing::debug!(
                event = "ocr_selection_cancel_retry_started",
                "等待选区取消窗口就绪"
            );
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(10));
                // 新任务会复位取消标志，旧取消重试不得关闭新任务的选区。
                if !selection_cancel_is_current(&cancel, &generation_token, generation) {
                    tracing::debug!(
                        event = "ocr_selection_cancel_retry_completed",
                        result = "superseded",
                        "取消重试被新任务替代"
                    );
                    break;
                }
                if crate::capture_win::cancel_selection() {
                    tracing::debug!(
                        event = "ocr_selection_cancel_retry_completed",
                        result = "delivered",
                        "选区取消消息已投递"
                    );
                    break;
                }
            }
            tracing::debug!(
                event = "ocr_selection_cancel_retry_stopped",
                "选区取消等待线程结束"
            );
        });
    }
    pub(super) fn cancel_recognition(&mut self) {
        if self.phase == ActivityPhase::Idle {
            return;
        }
        let span = self.task_span();
        let _entered = span.enter();
        tracing::info!(
            event = "ocr_cancel_requested",
            stage = if self.pending_image.is_some() {
                "waiting_model"
            } else {
                "capture_or_infer"
            },
            "请求取消截图任务"
        );
        self.cancel.store(true, Ordering::Release);
        // 框选窗口运行在托盘线程的消息循环中；取消请求必须显式投递到
        // 当前选区，否则退出/取消只能等用户再次操作遮罩窗口。
        let generation = self.cancel_generation.load(Ordering::Acquire);
        Self::request_selection_cancel(
            Arc::clone(&self.cancel),
            Arc::clone(&self.cancel_generation),
            generation,
        );
        if self.pending_image.take().is_some() {
            self.end_task("cancelled", "waiting_model");
            self.tray.notice("已取消识别");
        } else if let Some(window) = self.progress.as_ref() {
            window.set_cancelling(true);
        }
    }
    pub(super) fn open_main(&self, settings: bool) {
        let span = crate::logging::operation_span("ocr_service", "open_main");
        let _entered = span.enter();
        let started = Instant::now();
        tracing::info!(
            event = "ocr_main_spawn_started",
            settings,
            "请求打开工具箱主界面"
        );
        if let Some(exe) = self.settings.main_exe.as_ref().filter(|p| p.is_file()) {
            let mut command = std::process::Command::new(exe);
            if settings {
                command.arg("--settings");
            }
            match command.spawn() {
                Ok(child) => tracing::info!(
                    event = "ocr_main_spawn_completed",
                    peer_pid = child.id(),
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "工具箱主界面进程已启动"
                ),
                Err(error) => {
                    tracing::error!(event = "ocr_main_spawn_failed", stage = "spawn", error_type = ?error.kind(), error_code = ?error.raw_os_error(), elapsed_ms = crate::logging::elapsed_ms(started), "无法启动工具箱主界面");
                    self.tray.notice("无法打开 JchTools 主界面");
                }
            }
        } else {
            tracing::warn!(
                event = "ocr_main_spawn_failed",
                stage = "resolve_executable",
                error_type = "main_executable_unavailable",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "工具箱主程序位置不可用"
            );
            self.tray.notice("主程序位置已改变，请重新打开 JchTools");
        }
    }
    pub(super) fn open_settings(&mut self) {
        let span = crate::logging::operation_span("ocr_service", "open_settings");
        let _entered = span.enter();
        let started = Instant::now();
        tracing::debug!(event = "ocr_settings_open_started", "请求打开截图设置");
        if self.exit_pending.is_none()
            && self.settings.main_exe.as_ref().is_some_and(|p| p.is_file())
        {
            self.open_main(true);
            return;
        }
        if let Some(window) = self.settings_window.as_ref() {
            if !window.window().is_visible() {
                window.set_draft_key("".into());
                window.set_autostart_draft(window.get_autostart_enabled());
            }
            crate::result_window::apply_settings_theme(window);
            if let Ok(()) = window.show() {
                tracing::debug!(
                    event = "ocr_settings_open_completed",
                    reused = true,
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图设置窗口已显示"
                );
            } else {
                tracing::error!(
                    event = "ocr_settings_open_failed",
                    stage = "show_existing_window",
                    error_type = "slint_platform",
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图设置窗口显示失败"
                );
            }
            return;
        }
        let Ok(window) = SettingsWindow::new() else {
            tracing::error!(
                event = "ocr_settings_open_failed",
                stage = "create_window",
                error_type = "slint_platform",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图设置窗口创建失败"
            );
            self.tray.notice("截图设置窗口创建失败");
            return;
        };
        crate::result_window::apply_settings_theme(&window);
        let weak = self.self_weak.clone();
        window.on_save_settings(move |hotkey, enabled| {
            let Some(service) = weak.upgrade() else {
                return false;
            };
            let mut service = service.borrow_mut();
            match service.save_settings(&hotkey, enabled) {
                Ok(()) => true,
                Err(reason) => {
                    service.tray.notice(reason);
                    false
                }
            }
        });
        // 热键录制主键归一化（O-14）：Shift+符号反向映射在 Rust 侧实现并单测，
        // slint 的录制处理经此回调取回原键（与主程序 gui.rs 同表）。
        window.on_normalize_shift_key(|shift, key| {
            crate::result_window::normalize_shift_key(shift, &key).into()
        });
        let sender = self.commands.clone();
        window.on_retry_model(move || {
            let _ = sender.send(Command::RetryModel);
        });
        let sender = self.commands.clone();
        window.on_initialize_assets(move || {
            let _ = sender.send(Command::InitializeAssets);
        });
        let sender = self.commands.clone();
        window.on_cancel_recognition(move || {
            let _ = sender.send(Command::CancelRecognition);
        });
        let sender = self.commands.clone();
        window.on_exit_service(move || {
            let _ = sender.send(Command::ExitRequested);
        });
        let sender = self.commands.clone();
        window.on_force_exit_service(move || {
            let _ = sender.send(Command::ForceExitRequested);
        });
        let sender = self.commands.clone();
        window.on_close_requested(move || {
            let _ = sender.send(Command::CloseSettings);
        });
        let sender = self.commands.clone();
        window.window().on_close_requested(move || {
            let _ = sender.send(Command::CloseSettings);
            slint::CloseRequestResponse::KeepWindowShown
        });
        self.settings_window = Some(window);
        self.refresh_settings();
        if let Some(window) = self.settings_window.as_ref() {
            if let Ok(()) = window.show() {
                tracing::debug!(
                    event = "ocr_settings_open_completed",
                    reused = false,
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图设置窗口已显示"
                );
            } else {
                tracing::error!(
                    event = "ocr_settings_open_failed",
                    stage = "show_new_window",
                    error_type = "slint_platform",
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图设置窗口显示失败"
                );
            }
        }
    }
    pub(super) fn refresh_theme_if_due(&mut self) {
        if self.last_theme_sync.elapsed() < Duration::from_secs(2) {
            return;
        }
        if let Some(window) = self.settings_window.as_ref() {
            crate::result_window::apply_settings_theme(window);
        }
        if let Some(window) = self.progress.as_ref() {
            crate::result_window::apply_progress_theme(window);
        }
        if let Some(window) = self.result.as_ref() {
            window.refresh_theme();
        }
        self.last_theme_sync = Instant::now();
    }
    pub(super) fn refresh_settings(&mut self) {
        self.refresh_theme_if_due();
        if let Some(window) = self.settings_window.as_ref() {
            window.set_hotkey_label(self.settings.hotkey.as_str().into());
            window.set_model_status(
                match self.model {
                    ModelState::Uninitialized => "资产尚未初始化",
                    ModelState::Loading => "正在加载并预热模型…",
                    ModelState::Ready => "模型已就绪",
                    ModelState::Error => "模型加载失败",
                }
                .into(),
            );
            let (enabled, corrupt) = match autostart_value() {
                Ok(AutostartValue::Current | AutostartValue::Stale) => (true, false),
                Ok(AutostartValue::Corrupt) => (false, true),
                Ok(AutostartValue::Absent) | Err(_) => (false, false),
            };
            if window.get_autostart_draft() == window.get_autostart_enabled() {
                window.set_autostart_draft(enabled);
            }
            window.set_autostart_enabled(enabled);
            window.set_recognizing(self.phase != ActivityPhase::Idle);
            window.set_model_loading(self.model == ModelState::Loading);
            window.set_needs_initialization(matches!(
                self.model,
                ModelState::Uninitialized | ModelState::Error
            ));
            window.set_exit_waiting(self.exit_pending.is_some());
            window.set_force_exit_available(
                self.exit_pending
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(10)),
            );
            // 设置警告与模型错误都有时以「；」连接，单一存在时原文上屏；
            // 损坏的开机启动值单独追加说明（S8-05：明确报告，不当已启用）。
            let mut status = [
                self.settings.warning.as_deref(),
                self.model_error.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("；");
            if corrupt {
                if !status.is_empty() {
                    status.push('；');
                }
                status.push_str(
                    "开机启动项已损坏（未当作已启用，也不会自动改写）；重新保存设置可清除",
                );
            }
            window.set_status_message(status.into());
        }
    }
}
