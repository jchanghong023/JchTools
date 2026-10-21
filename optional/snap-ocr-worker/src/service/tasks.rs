//! `Service` 的截图任务、退出流程与控制管道命令分派。
//! 从 `service.rs` 按内聚职责原样搬移；方法补 `pub(super)` 以维持原模块内调用关系。

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slint::ComponentHandle;

use super::autostart::{autostart_enabled, set_autostart};
use super::clipboard::{confirm_background_exit, copy_text, gui_process_alive};
use super::protocol;
use super::{ActivityPhase, Command, LoadFailure, ModelState, OcrError, Service, Work};
use crate::result_window::ResultWindowHandle;

impl Service {
    pub(super) fn request_capture(&mut self) {
        if self.exit_pending.is_some() {
            self.tray.notice("截图服务正在退出");
            return;
        }
        if self.phase != ActivityPhase::Idle {
            self.tray.notice("正在识别");
            return;
        }
        let span = crate::logging::operation_span("snap_ocr", "capture_task");
        let started = Instant::now();
        self.diagnostic_task = Some((span.clone(), started));
        let _entered = span.enter();
        tracing::info!(
            event = "ocr_task_started",
            model_state = self.model.as_str(),
            "截图任务开始"
        );
        self.cancel_generation.fetch_add(1, Ordering::AcqRel);
        self.cancel.store(false, Ordering::Release);
        self.phase = ActivityPhase::Selecting;
        self.hide_old();
        if self.tray.trigger(span.clone()).is_err() {
            self.handle(Command::CaptureFailed(
                "托盘服务不可用，截图请求未交付".into(),
            ));
        }
        self.refresh_settings();
    }
    pub(super) fn request_exit(&mut self) {
        if self.exit_pending.is_some() || self.exit_confirming.is_some() {
            return;
        }
        self.exit_confirming = Some(Instant::now());
        let commands = self.commands.clone();
        // 确认框不阻塞服务事件循环：GUI 心跳仍可更新在途任务和取消结果。
        let span = tracing::Span::current();
        std::thread::spawn(move || {
            let _entered = span.enter();
            let _ = commands.send(Command::ExitDecision(confirm_background_exit()));
        });
    }
    pub(super) fn begin_exit(&mut self) {
        self.cancel_recognition();
        tracing::info!(
            event = "ocr_service_stopping",
            busy = self.phase != ActivityPhase::Idle,
            model_state = self.model.as_str(),
            "截图后台开始安全停止"
        );
        self.exit_pending = Some(Instant::now());
        if self.phase != ActivityPhase::Idle || self.model == ModelState::Loading {
            self.tray
                .notice("正在等待当前任务安全结束；退出完成前后台仍保持响应");
            self.open_settings();
        }
    }
    pub(super) fn stop_worker(&mut self) {
        if self.exit_stop_sent
            || self
                .exit_pending
                .is_none_or(|at| at.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        if self.stop_retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        self.stop_retry_at = None;
        self.gui_tasks.retain(|pid, _| gui_process_alive(*pid));
        if self.gui_tasks.values().any(|busy| *busy) {
            return;
        }
        self.exit_stop_sent = true;
        if self.work.send(Work::Stop).is_err() {
            tracing::warn!(
                event = "ocr_stop_dispatch_failed",
                stage = "worker_dispatch",
                error_type = "disconnected",
                "推理线程已退出，结束截图服务"
            );
            self.complete_exit();
        }
    }
    pub(super) fn complete_exit(&mut self) {
        tracing::info!(
            event = "ocr_service_exit_requested",
            "请求结束截图服务事件循环"
        );
        self.tray.stop();
        if let Some(result) = self.result.take() {
            result.hide_and_clear();
        }
        self.hide_progress();
        let _ = slint::quit_event_loop();
    }
    pub(super) fn pipe(&mut self, request: &Value) -> Value {
        let action = request.get("command").and_then(Value::as_str).unwrap_or("");
        let started = Instant::now();
        let command_kind = protocol::diagnostic_command(Some(action));
        tracing::debug!(
            event = "ocr_control_handler_started",
            command = command_kind,
            "截图服务开始处理控制命令"
        );
        let outcome = match action {
            "ping" | "get-state" => {
                if let (Some(pid), Some(busy)) = (
                    request["gui_pid"]
                        .as_u64()
                        .and_then(|id| u32::try_from(id).ok()),
                    request["gui_busy"].as_bool(),
                ) {
                    self.gui_tasks.insert(pid, busy);
                }
                Ok(())
            }
            "open-settings" => {
                self.open_settings();
                Ok(())
            }
            "save-settings" => match (
                request.get("hotkey").and_then(Value::as_str),
                request.get("autostart").and_then(Value::as_bool),
            ) {
                (Some(hotkey), Some(enabled)) => self.save_settings(hotkey, enabled),
                _ => Err("缺少有效快捷键或开机启动开关".into()),
            },
            "attach-main-exe" => request
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| "缺少主程序路径".to_string())
                .and_then(|path| self.attach_main_exe(path)),
            "retry-load" => {
                self.retry();
                Ok(())
            }
            "capture" => {
                self.request_capture();
                Ok(())
            }
            "cancel" => {
                self.cancel_recognition();
                Ok(())
            }
            "shutdown" => {
                self.request_exit();
                Ok(())
            }
            _ => Err("未知的截图服务命令".into()),
        };
        self.refresh_settings();
        match outcome {
            Ok(()) => {
                tracing::debug!(
                    event = "ocr_control_handler_completed",
                    command = command_kind,
                    model_state = self.model.as_str(),
                    busy = self.phase != ActivityPhase::Idle,
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图服务控制命令完成"
                );
                self.status()
            }
            Err(reason) => {
                tracing::warn!(
                    event = "ocr_control_handler_failed",
                    command = command_kind,
                    stage = command_kind,
                    error_type = "command",
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图服务控制命令失败"
                );
                json!({"ok":false,"error":reason})
            }
        }
    }
    pub(super) fn handle(&mut self, command: Command) {
        match command {
            Command::Pipe(request, response, span) => {
                let _entered = span.enter();
                let _ = response.send(self.pipe(&request));
            }
            Command::CaptureRequested => self.request_capture(),
            Command::Image(image, work) => {
                // 托盘销毁后可能仍有框选的迟到回执，不能重新启动已终结的任务。
                if self.phase == ActivityPhase::Idle {
                    return;
                }
                self.phase = ActivityPhase::Processing;
                let span = self.task_span();
                let _entered = span.enter();
                if self.exit_pending.is_some() || self.cancel.load(Ordering::Acquire) {
                    self.phase = ActivityPhase::Idle;
                    self.pending_image = None;
                    self.hide_progress();
                    self.restore_old();
                    self.finish_task("cancelled", "capture");
                } else {
                    match self.model {
                        ModelState::Loading => {
                            if let Err(reason) = self.show_progress("等待模型就绪…") {
                                self.finish_task("failed", "progress_window");
                                self.phase = ActivityPhase::Idle;
                                self.restore_old();
                                self.tray.notice(reason);
                            } else {
                                self.pending_image = Some((image, work));
                            }
                        }
                        ModelState::Ready => {
                            self.cancel.store(false, Ordering::Release);
                            if let Err(reason) = self.show_progress("正在识别…") {
                                self.finish_task("failed", "progress_window");
                                self.phase = ActivityPhase::Idle;
                                self.restore_old();
                                self.tray.notice(reason);
                            } else if self
                                .work
                                .send(Work::Recognize(image, work, span.clone()))
                                .is_err()
                            {
                                self.finish_task("failed", "worker_dispatch");
                                self.phase = ActivityPhase::Idle;
                                self.hide_progress();
                                self.restore_old();
                                self.tray.notice("推理进程已退出");
                            }
                        }
                        ModelState::Uninitialized | ModelState::Error => {
                            self.finish_task("failed", "model_not_ready");
                            self.phase = ActivityPhase::Idle;
                            self.restore_old();
                            self.tray.notice("模型未就绪，请初始化或在设置中重试加载");
                        }
                    }
                }
            }
            Command::CancelledSelection => {
                self.finish_task("cancelled", "selection");
                self.phase = ActivityPhase::Idle;
                self.hide_progress();
                self.restore_old();
            }
            Command::CaptureFailed(reason) => {
                self.finish_task("failed", "capture");
                self.phase = ActivityPhase::Idle;
                self.hide_progress();
                self.restore_old();
                self.tray.notice(reason);
            }
            Command::ModelLoaded(outcome) => {
                let previous_model = self.model;
                match outcome {
                    Ok(()) => {
                        if let Err(reason) =
                            crate::result_window::register_fonts(&self.root.join("fonts"))
                        // 字体错误属于结果界面阶段，不记录正文或字体路径。
                        {
                            self.model = ModelState::Error;
                            tracing::warn!(
                                event = "ocr_font_registration_failed",
                                stage = "result_fonts",
                                error_type = "font_registration",
                                "截图结果字体注册失败"
                            );
                            self.model_error = Some(reason.to_string());
                        } else {
                            self.model = ModelState::Ready;
                            self.model_error = None;
                            if let Some((image, work)) = self.pending_image.take() {
                                if self.cancel.load(Ordering::Acquire) {
                                    self.finish_task("cancelled", "waiting_model");
                                    self.phase = ActivityPhase::Idle;
                                    self.hide_progress();
                                    self.restore_old();
                                } else {
                                    if let Some(window) = self.progress.as_ref() {
                                        window.set_stage("正在识别…".into());
                                    }
                                    if self
                                        .work
                                        .send(Work::Recognize(image, work, self.task_span()))
                                        .is_err()
                                    {
                                        self.finish_task("failed", "worker_dispatch");
                                        self.phase = ActivityPhase::Idle;
                                        self.hide_progress();
                                        self.restore_old();
                                        self.tray.notice("推理进程已退出");
                                    }
                                }
                            }
                        }
                    }
                    Err(failure) => {
                        tracing::warn!(
                            event = "ocr_model_unavailable",
                            unconfigured = matches!(failure, LoadFailure::NotConfigured(_)),
                            "截图模型不可用"
                        );
                        self.model = match failure {
                            LoadFailure::NotConfigured(_) => ModelState::Uninitialized,
                            LoadFailure::Failed(_) => ModelState::Error,
                        };
                        self.model_error = Some(failure.message().to_owned());
                    }
                }
                tracing::info!(
                    event = "ocr_model_state_changed",
                    old_state = previous_model.as_str(),
                    new_state = self.model.as_str(),
                    "截图模型状态更新"
                );
                if self.model != ModelState::Ready && self.pending_image.take().is_some() {
                    self.finish_task("failed", "model_load");
                    self.phase = ActivityPhase::Idle;
                    self.hide_progress();
                    self.restore_old();
                    self.tray.notice(format!(
                        "截图组件加载失败：{}",
                        self.model_error.as_deref().unwrap_or("未知错误")
                    ));
                }
            }
            Command::OcrFinished(outcome, work) => {
                // 推理线程只在原请求真正终态（或确认引擎退出）后投递；
                // 取消 ACK、模型 Ready 和控制管道响应都不能触发此空闲边界。
                let span = self.task_span();
                let _entered = span.enter();
                let terminal_result = match &outcome {
                    Ok(Some(_)) => "success",
                    Ok(None) => "empty",
                    Err(OcrError::Cancelled) => "cancelled",
                    Err(_) => "failed",
                };
                self.phase = ActivityPhase::Idle;
                self.hide_progress();
                if self.exit_pending.is_some() {
                    self.finish_task(terminal_result, "service_stopping");
                    self.stop_worker();
                    return;
                }
                match outcome {
                    Ok(Some(text)) => {
                        match ResultWindowHandle::show_for_monitor(&text, work, |text| {
                            copy_text(text)
                        }) {
                            Ok(window) => {
                                self.finish_task("success", "result_presented");
                                if let Some(old) = self.result.take() {
                                    old.hide_and_clear();
                                }
                                self.result = Some(window);
                                self.old_result_visible = false;
                            }
                            Err(reason) => {
                                self.finish_task("failed", "result_window");
                                self.restore_old();
                                self.tray.notice(reason.to_string());
                            }
                        }
                    }
                    Ok(None) => {
                        self.finish_task("empty", "infer");
                        self.restore_old();
                        self.tray.notice("选区内未识别到文字");
                    }
                    Err(OcrError::Cancelled) => {
                        self.finish_task("cancelled", "infer");
                        // 原请求现已真实结束，才允许恢复旧结果与重建场景客户端。
                        // 后续 snapshot_state 仅决定是否重预热，不用于判断任务终态。
                        self.restore_old();
                        self.tray.notice("已取消识别");
                        // 防御性复位：清掉本请求遗留的取消标志，确保后续
                        // 加载/预热周期不被它影响（当前 warm_up 使用函数内
                        // 局部标志、不读取服务共享的 self.cancel，且产生
                        // pending_image 的路径必先经 request_capture 复位；
                        // 此复位为纵深防御，并非修复已知竞态）。
                        self.cancel.store(false, Ordering::Release);
                        self.begin_model_load();
                    }
                    Err(OcrError::TimedOut(reason)) => {
                        // 请求超时不终止共享引擎；真实终态后保留重试入口，
                        // 不对故障环境自动重放本次截图。
                        self.finish_task("failed", "timeout");
                        self.restore_old();
                        self.tray.notice(reason.as_str());
                        self.model = ModelState::Error;
                        self.model_error = Some(reason);
                    }
                    Err(OcrError::ProcessExited(reason)) => {
                        // 子进程死亡（被误关黑窗、崩溃等）：连接不可复用，模型
                        // 从就绪降级为错误并保留重试入口（O-13/O-30），不得继续
                        // 冒称就绪导致重试按钮失效。
                        self.finish_task("failed", "process_exited");
                        self.restore_old();
                        self.tray.notice("推理子进程已退出；可在设置中重试加载模型");
                        self.model = ModelState::Error;
                        self.model_error = Some(reason);
                    }
                    Err(OcrError::Backend(reason)) => {
                        // 单次推理失败只结束本任务，已预热的模型保持就绪（O-13）。
                        self.finish_task("failed", "backend");
                        self.restore_old();
                        self.tray.notice(format!("OCR 识别失败：{reason}"));
                    }
                    Err(OcrError::ModelFailure(reason)) => {
                        self.finish_task("failed", "asset_invalid");
                        self.restore_old();
                        self.tray.notice("截图模型资产失效；可在设置中重试加载模型");
                        self.model = ModelState::Error;
                        self.model_error = Some(reason);
                    }
                    Err(OcrError::Communication(reason)) => {
                        self.finish_task("failed", "communication");
                        self.restore_old();
                        self.tray.notice("截图推理通信失败；可在设置中重试加载模型");
                        self.model = ModelState::Error;
                        self.model_error = Some(reason);
                    }
                }
            }
            Command::TrayStopped => {
                self.settings.warning = Some("托盘服务已退出，截图暂不可用".into());
                if self.phase == ActivityPhase::Selecting {
                    self.handle(Command::CaptureFailed("托盘服务已退出，截图未完成".into()));
                }
            }
            Command::HotkeyUnavailable(reason) => {
                self.settings.warning = Some(reason);
                tracing::warn!(
                    event = "ocr_hotkey_warning",
                    error_type = "unavailable",
                    "截图热键不可用"
                );
            }
            Command::OpenMain => self.open_main(false),
            Command::OpenSettings => self.open_settings(),
            Command::InitializeAssets => {
                let started = Instant::now();
                tracing::info!(
                    component = "snap_ocr_service",
                    event = "ocr_main_spawn_started",
                    settings = true,
                    "请求打开工具箱主界面以初始化组件"
                );
                if let Some(exe) = self.settings.main_exe.as_ref().filter(|p| p.is_file()) {
                    if std::process::Command::new(exe)
                        .arg("--settings")
                        .spawn()
                        .is_err()
                    {
                        tracing::error!(
                            component = "snap_ocr_service",
                            event = "ocr_main_spawn_failed",
                            stage = "spawn",
                            error_type = "spawn",
                            elapsed_ms = crate::logging::elapsed_ms(started),
                            "无法启动工具箱主界面以初始化组件"
                        );
                        self.tray.notice("无法打开 JchTools 以初始化组件");
                    }
                } else {
                    tracing::error!(
                        component = "snap_ocr_service",
                        event = "ocr_main_spawn_failed",
                        stage = "resolve_executable",
                        error_type = "main_executable_unavailable",
                        elapsed_ms = crate::logging::elapsed_ms(started),
                        "工具箱主程序位置不可用"
                    );
                    self.tray
                        .notice("主程序位置已改变；请先打开 JchTools 再初始化组件");
                }
            }
            Command::RetryModel => self.retry(),
            Command::CancelRecognition => {
                self.cancel_recognition();
            }
            Command::CloseSettings => {
                if let Some(window) = self.settings_window.as_ref() {
                    window.set_draft_key("".into());
                    window.set_autostart_draft(window.get_autostart_enabled());
                    let _ = window.hide();
                }
            }
            Command::ForceExitRequested => {
                if self
                    .exit_pending
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(10))
                {
                    self.gui_tasks.retain(|pid, _| gui_process_alive(*pid));
                    if self.gui_tasks.values().any(|busy| *busy) {
                        self.tray
                            .notice("其他界面任务仍在处理，等待安全停止后才能强制退出截图推理");
                    } else {
                        let commands = self.commands.clone();
                        let span = tracing::Span::current();
                        std::thread::spawn(move || {
                            let _entered = span.enter();
                            let started = Instant::now();
                            tracing::warn!(
                                event = "ocr_force_stop_started",
                                "开始强制停止共享后台"
                            );
                            let result = crate::xberg_runtime::force_background_exit()
                                .and_then(crate::xberg_runtime::checked)
                                .map(|_| ());
                            if result.is_ok() {
                                tracing::info!(
                                    event = "ocr_force_stop_completed",
                                    elapsed_ms = crate::logging::elapsed_ms(started),
                                    "共享后台强制停止完成"
                                );
                            } else {
                                tracing::error!(
                                    event = "ocr_force_stop_failed",
                                    error_type = "runtime",
                                    elapsed_ms = crate::logging::elapsed_ms(started),
                                    "共享后台强制停止失败"
                                );
                            }
                            let _ = commands.send(Command::ForceExitFinished(result));
                        });
                    }
                }
            }
            Command::ForceExitFinished(result) => {
                if let Err(error) = result {
                    self.tray.notice(error);
                }
            }
            Command::ToggleAutostart => {
                let started = Instant::now();
                tracing::info!(
                    component = "snap_ocr_service",
                    event = "ocr_autostart_toggle_started",
                    "请求切换截图服务开机启动"
                );
                // S8-01 复审：pending 回执收尾（commit_settings）会按保存时的
                // 旧意图回写自启开关；pending 期间拒绝切换，避免迟到回执把
                // 用户刚做的开关覆盖回去。
                if self.pending_hotkey.is_some() {
                    tracing::info!(
                        component = "snap_ocr_service",
                        event = "ocr_autostart_toggle_skipped",
                        result = "settings_pending",
                        elapsed_ms = crate::logging::elapsed_ms(started),
                        "热键设置待确认，暂不切换开机启动"
                    );
                    self.tray
                        .notice("快捷键设置等待确认期间暂不能切换开机启动，请稍后再试");
                } else if let Err(reason) = set_autostart(!autostart_enabled()) {
                    tracing::error!(
                        component = "snap_ocr_service",
                        event = "ocr_autostart_toggle_failed",
                        stage = "autostart_update",
                        error_type = "update_failed",
                        elapsed_ms = crate::logging::elapsed_ms(started),
                        "截图开机启动设置更新失败"
                    );
                    self.tray.notice(reason);
                } else {
                    tracing::info!(
                        component = "snap_ocr_service",
                        event = "ocr_autostart_toggle_completed",
                        result = "updated",
                        elapsed_ms = crate::logging::elapsed_ms(started),
                        "截图开机启动设置已更新"
                    );
                }
            }
            Command::HotkeyReplaced(outcome) => self.finish_hotkey_receipt(Some(outcome)),
            Command::HotkeyReceiptTimedOut => self.finish_hotkey_receipt(None),
            Command::ExitRequested => self.request_exit(),
            Command::ExitDecision(confirmed) => {
                self.exit_confirming = None;
                if confirmed {
                    self.begin_exit();
                }
            }
            Command::WorkerStopped(span) => {
                let _entered = span.enter();
                tracing::debug!(event = "ocr_stop_received", "已收到截图推理线程停止回执");
                self.complete_exit();
                return;
            }
            Command::WorkerStopFailed(span) => {
                let _entered = span.enter();
                tracing::warn!(
                    event = "ocr_stop_retry_scheduled",
                    error_type = "stop_failed",
                    retry_delay_ms = 1_000,
                    "截图推理线程停止未完成，安排重试"
                );
                self.exit_stop_sent = false;
                self.stop_retry_at = Some(Instant::now() + Duration::from_secs(1));
                self.tray
                    .notice("后台停止暂未完成，将继续重试；仍可使用强制退出");
            }
        }
        if self.exit_pending.is_some()
            && self.phase == ActivityPhase::Idle
            && self.model != ModelState::Loading
        {
            self.stop_worker();
        }
        self.refresh_settings();
    }
}
