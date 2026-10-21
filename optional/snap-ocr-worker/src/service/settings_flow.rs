//! `Service` 的设置保存流程：热键替换回执（S8-01）、开机启动与设置文件落盘、回滚。
//! 从 `service.rs` 按内聚职责原样搬移；方法补 `pub(super)` 以维持原模块内调用关系。

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::autostart::{autostart_update_required, autostart_value, set_autostart, AutostartValue};
use super::settings::save_launcher;
use super::tray;
use super::{ActivityPhase, Command, ModelState, PendingHotkey, Service, Work};

impl Service {
    /// 进入「后台加载模型」状态：置 Loading、清错误并通知推理线程装载；
    /// 线程已退出时降级为 Error（合同文案两处共用，防改一漏一）。
    pub(super) fn begin_model_load(&mut self) {
        self.model = ModelState::Loading;
        self.model_error = None;
        if self
            .work
            .send(Work::Load(tracing::Span::current()))
            .is_err()
        {
            tracing::warn!(
                event = "ocr_load_dispatch_failed",
                stage = "worker_dispatch",
                error_type = "disconnected",
                "截图模型加载请求无法发送"
            );
            self.model = ModelState::Error;
            self.model_error = Some("推理线程已退出，请重新启动截图服务".into());
        }
    }
    pub(super) fn retry(&mut self) {
        if self.exit_pending.is_some()
            || self.model == ModelState::Loading
            || self.model == ModelState::Ready
        {
            return;
        }
        self.begin_model_load();
        self.refresh_settings();
    }
    pub(super) fn rollback_settings(
        &mut self,
        old_hotkey: &str,
        new_hotkey: &str,
        hotkey_changed: bool,
        old_autostart: Option<bool>,
        reason: String,
    ) -> String {
        let mut detail = reason;
        if let Some(enabled) = old_autostart {
            if set_autostart(enabled).is_err() {
                detail.push_str("；开机启动未能恢复，请检查当前开关状态");
            }
        }
        if hotkey_changed {
            match self.tray.replace(old_hotkey.to_owned()) {
                tray::HotkeyReceipt::Done(Ok(())) => {
                    old_hotkey.clone_into(&mut self.settings.hotkey);
                }
                tray::HotkeyReceipt::Done(Err(_)) => {
                    new_hotkey.clone_into(&mut self.settings.hotkey);
                    let _ = write!(
                        detail,
                        "；原热键已无法恢复，当前仍使用 {new_hotkey}；请重新保存设置"
                    );
                }
                tray::HotkeyReceipt::Late(receiver) => {
                    // S8-01：恢复旧键的替换仍在托盘队列中，超时不是失败终态；
                    // 内存先按旧键记，迟到回执到达后再纠正（Restoring）。
                    old_hotkey.clone_into(&mut self.settings.hotkey);
                    self.pending_hotkey = Some(PendingHotkey::Restoring {
                        new_hotkey: new_hotkey.to_owned(),
                    });
                    self.await_hotkey_receipt(receiver, true);
                    detail.push_str("；原热键恢复仍在等待托盘确认");
                }
            }
        } else {
            old_hotkey.clone_into(&mut self.settings.hotkey);
        }
        self.settings.warning = Some(detail.clone());
        self.refresh_settings();
        detail
    }
    /// 首次等待超过 30 秒时通知界面；恢复旧键与同值确认必须继续持有
    /// 回执直到托盘明确返回，不能把等待耗尽裁成注册终态。
    pub(super) fn await_hotkey_receipt(
        &self,
        receiver: mpsc::Receiver<Result<(), String>>,
        wait_after_timeout: bool,
    ) {
        let commands = self.commands.clone();
        let span = tracing::Span::current();
        std::thread::spawn(move || {
            let _entered = span.enter();
            match receiver.recv_timeout(Duration::from_secs(30)) {
                Ok(result) => {
                    let _ = commands.send(Command::HotkeyReplaced(result));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = commands.send(Command::HotkeyReceiptTimedOut);
                    if wait_after_timeout {
                        let result = receiver
                            .recv()
                            .unwrap_or_else(|_| Err("托盘热键确认回执通道已关闭".into()));
                        let _ = commands.send(Command::HotkeyReplaced(result));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = commands.send(Command::HotkeyReplaced(Err(
                        "托盘热键回执通道已关闭".into()
                    )));
                }
            }
        });
    }
    pub(super) fn save_settings(&mut self, hotkey: &str, enabled: bool) -> Result<(), String> {
        if self.phase != ActivityPhase::Idle || self.exit_pending.is_some() {
            return Err("请等待当前截图任务结束后保存设置".into());
        }
        if self.pending_hotkey.is_some() {
            return Err("上一次热键设置仍在等待托盘确认，请稍后再保存".into());
        }
        tray::parse_hotkey(hotkey)?;
        let old_hotkey = self.settings.hotkey.clone();
        let old_autostart = autostart_value()?;
        let hotkey_changed = old_hotkey != hotkey;
        // 每次保存都取得托盘的真实注册回执：同值且已生效由托盘幂等确认，
        // 同值但启动时注册失败则重试；确认前不写文件、不清除现有警告。
        match self.tray.replace(hotkey.to_owned()) {
            tray::HotkeyReceipt::Done(Ok(())) => {
                hotkey.clone_into(&mut self.settings.hotkey);
            }
            tray::HotkeyReceipt::Done(Err(reason)) => {
                tracing::warn!(
                    component = "snap_ocr_service",
                    event = "ocr_hotkey_update_failed",
                    stage = "hotkey_replace",
                    error_type = "rejected",
                    "截图热键更新被拒绝"
                );
                return Err(reason);
            }
            tray::HotkeyReceipt::Late(receiver) => {
                self.pending_hotkey = Some(PendingHotkey::Saving {
                    hotkey: hotkey.to_owned(),
                    old_hotkey,
                    old_autostart,
                    autostart: enabled,
                });
                self.await_hotkey_receipt(receiver, !hotkey_changed);
                tracing::info!(
                    component = "snap_ocr_service",
                    event = "ocr_hotkey_update_pending",
                    result = "awaiting_receipt",
                    "截图热键注册等待托盘确认"
                );
                self.refresh_settings();
                return Ok(());
            }
        }
        self.commit_settings(hotkey, &old_hotkey, hotkey_changed, old_autostart, enabled)
    }
    /// 保存收尾（热键已生效后）：开机启动与设置文件，任一失败走回滚。
    /// S8-01 起同步路径与迟到回执成功路径共用，保持主流程语义不变。
    pub(super) fn commit_settings(
        &mut self,
        hotkey: &str,
        old_hotkey: &str,
        hotkey_changed: bool,
        old_autostart: AutostartValue,
        enabled: bool,
    ) -> Result<(), String> {
        let started = Instant::now();
        tracing::info!(
            component = "snap_ocr_service",
            event = "ocr_settings_save_started",
            "保存截图设置"
        );
        let autostart_changed = autostart_update_required(old_autostart, enabled);
        let old_enabled = matches!(
            old_autostart,
            AutostartValue::Current | AutostartValue::Stale
        );
        if autostart_changed {
            if let Err(reason) = set_autostart(enabled) {
                tracing::error!(
                    component = "snap_ocr_service",
                    event = "ocr_settings_save_failed",
                    stage = "autostart",
                    error_type = "update_failed",
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "截图开机启动设置更新失败"
                );
                return Err(self.rollback_settings(
                    old_hotkey,
                    hotkey,
                    hotkey_changed,
                    Some(old_enabled),
                    reason,
                ));
            }
        }
        if let Err(reason) = self.settings.save(&self.root) {
            tracing::error!(
                component = "snap_ocr_service",
                event = "ocr_settings_save_failed",
                stage = "settings_file",
                error_type = "save_failed",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图设置文件保存失败"
            );
            return Err(self.rollback_settings(
                old_hotkey,
                hotkey,
                hotkey_changed,
                autostart_changed.then_some(old_enabled),
                reason,
            ));
        }
        self.refresh_settings();
        tracing::info!(
            component = "snap_ocr_service",
            event = "ocr_settings_save_completed",
            elapsed_ms = crate::logging::elapsed_ms(started),
            result = "saved",
            "截图设置保存完成"
        );
        Ok(())
    }
    /// 迟到回执的裁决（S8-01）：按在途上下文 reconcile 到真实终态；
    /// `None` 表示等待线程的有界时限也耗尽（托盘长时间未泵消息）。
    pub(super) fn finish_hotkey_receipt(&mut self, outcome: Option<Result<(), String>>) {
        match self.pending_hotkey.take() {
            Some(PendingHotkey::Saving {
                hotkey,
                old_hotkey,
                old_autostart,
                autostart,
            }) => match outcome {
                Some(Ok(())) => {
                    hotkey.clone_into(&mut self.settings.hotkey);
                    // 待确认请求已接受，只有真实注册成功才能提交；同值重试不
                    // 属于新旧组合迁移，后续失败不需要再替换回相同组合。
                    if let Err(reason) = self.commit_settings(
                        &hotkey,
                        &old_hotkey,
                        hotkey != old_hotkey,
                        old_autostart,
                        autostart,
                    ) {
                        self.tray.notice(reason);
                    }
                }
                Some(Err(reason)) => {
                    // 托盘明确回执失败：Replace 失败路径不改托盘状态、旧键仍
                    // 生效；内存值与设置文件本就未动，无需回滚，直接上报。
                    tracing::warn!(
                        component = "snap_ocr_service",
                        event = "ocr_hotkey_update_failed",
                        stage = "hotkey_replace",
                        error_type = "receipt_failed",
                        "截图热键更新回执失败"
                    );
                    self.settings.warning = Some(reason.clone());
                    self.refresh_settings();
                    self.tray.notice(reason);
                }
                None => {
                    if hotkey == old_hotkey {
                        // 同值确认没有可回滚的新组合，继续持有原注册回执，
                        // 防止未确认时开放下一次保存或清除启动冲突警告。
                        self.pending_hotkey = Some(PendingHotkey::Saving {
                            hotkey,
                            old_hotkey,
                            old_autostart,
                            autostart,
                        });
                        self.settings.warning =
                            Some("快捷键仍在等待托盘注册确认，请稍后再保存".into());
                        self.refresh_settings();
                        return;
                    }
                    // 新键替换仍可能执行；把旧键排在其后恢复，并同样等待明确
                    // 回执。恢复也可能因旧键被占用而失败，不能无回执假称已恢复。
                    let detail = format!("快捷键确认超时，正在恢复 {old_hotkey}；请稍后重试保存");
                    let detail = self.rollback_settings(&old_hotkey, &hotkey, true, None, detail);
                    self.tray.notice(detail);
                }
            },
            Some(PendingHotkey::Restoring { new_hotkey, .. }) => {
                if outcome.is_none() {
                    self.pending_hotkey = Some(PendingHotkey::Restoring { new_hotkey });
                    self.settings.warning =
                        Some("原快捷键恢复仍在等待托盘确认，请稍后再保存".into());
                    self.refresh_settings();
                    return;
                }
                if matches!(outcome, Some(Ok(()))) {
                    tracing::info!(
                        component = "snap_ocr_service",
                        event = "ocr_hotkey_restore_completed",
                        result = "restored",
                        "原截图热键已恢复"
                    );
                    // 恢复成功：清除等待期提示（Late 分支写入的「仍在等待托盘
                    // 确认」不能残留误导），内存旧键即为终态。
                    self.settings.warning = Some("原快捷键已恢复".to_string());
                    self.refresh_settings();
                } else {
                    // 托盘明确拒绝恢复：实际仍持新键，内存值纠正为新键。
                    tracing::warn!(
                        component = "snap_ocr_service",
                        event = "ocr_hotkey_restore_failed",
                        stage = "hotkey_restore",
                        error_type = "confirmation_unavailable",
                        result = "new_hotkey_retained",
                        "原截图热键恢复未确认，保留当前热键"
                    );
                    new_hotkey.clone_into(&mut self.settings.hotkey);
                    let detail =
                        format!("原快捷键恢复未完成，当前仍使用 {new_hotkey}；请重新保存设置");
                    self.settings.warning = Some(detail.clone());
                    self.refresh_settings();
                    self.tray.notice(detail);
                }
            }
            None => {
                // 无在途上下文的孤立回执（理论不可达，防御）：失败时按当前
                // 内存键自愈重排，避免未知状态漂移。
                if let Some(Err(reason)) = outcome {
                    tracing::warn!(
                        kind = "hotkey_receipt_orphan",
                        "热键回执无对应上下文：{reason}"
                    );
                    let current = self.settings.hotkey.clone();
                    self.tray.replace_detached(current);
                }
            }
        }
    }
    pub(super) fn attach_main_exe(&mut self, raw: &str) -> Result<(), String> {
        let path = PathBuf::from(raw);
        if !path.is_absolute()
            || !path.is_file()
            || !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("JchTools.exe"))
        {
            return Err("主程序路径无效".into());
        }
        save_launcher(&self.root, &path)?;
        // S9-11：截图期间隐藏主程序窗口按完整路径比对，登记值变化须同步内存
        // 与共享登记——先更新 settings 再登记；只登记旧值会让首次 attach 的
        // 会话内隐藏失效，并让 open_main/open_settings 误报「主程序位置已改变」。
        self.settings.main_exe = Some(path);
        crate::capture_win::register_main_exe(self.settings.main_exe.as_deref());
        Ok(())
    }
}
