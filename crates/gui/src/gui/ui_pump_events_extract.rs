//! 界面事件泵的解压任务事件：ExtractCount 清点门禁与文案（X-02）、ExtractDone 一段式收尾与失败列表刷新（X-06/U-10）（自 ui_pump_events.rs 拆分）。
use super::{extract_confirm_text, request_fail_page, AppWindow, PathBuf, Summary, UiPump};

impl UiPump {
    /// Event::ExtractCount 的处理：确认框仍开着（kind=1）且事件仍是当前代际才更新
    /// 文案；零发现不解除门禁，清点失败只留红条并关闭确认框（X-02）。
    pub(super) fn handle_extract_count(
        &self,
        ui: &AppWindow,
        generation: u64,
        count: Result<u64, String>,
    ) {
        let current = self.state.borrow().extract_generation;
        // 确认框仍开着（kind=1）且事件仍是当前代际才更新文案；用户已确认/返回、
        // 或已改目录重新发起清点时，过期事件整体丢弃，不打扰运行中状态（X-02）。
        if ui.get_confirm_kind() == 1 && ui.get_screen() == 2 && generation == current {
            match count {
                Ok(0) => {
                    // X-02：发现数为零时提示无可处理包，不启动空解压任务——
                    // 清点门禁保持（确认按钮保持禁用），用户点「返回检查」
                    // 重新选择目录或调整规则。
                    ui.set_confirm_text(
                        "未发现可处理的压缩包（按当前扫描范围，已排除「解压失败」目录），本次不会解压任何文件；不启动空解压任务。点「返回检查」可重新选择目录或调整规则。"
                            .into(),
                    );
                }
                Ok(n) => {
                    ui.set_confirm_text(
                        extract_confirm_text(
                            &format!(
                                "清点到 {n} 个压缩包（按当前扫描范围，已排除「解压失败」目录）。"
                            ),
                            ui.get_directory().as_str(),
                        )
                        .into(),
                    );
                    // 数量已知：解除清点门禁，勾选后即可确认（X-02）。
                    ui.set_confirm_pending(false);
                }
                Err(error) => {
                    // 清点失败：关闭确认框只留红条——占位文案下继续允许确认
                    // 会让用户在数量未知的状态启动任务（X-02）。
                    ui.set_confirm_kind(0);
                    ui.set_confirm_pending(false);
                    ui.set_error_text(error.into());
                }
            }
        }
    }

    /// Event::ExtractDone 的处理：一段式收尾只清运行态并展示摘要，不改 ready/has_task
    /// （目录整理两段式专用）；如实报出删除与保留口径，并刷新失败列表当前页（U-10）。
    pub(super) fn finish_extract_task(&self, ui: &AppWindow, path: PathBuf, summary: &Summary) {
        // X-02 一段式收尾：只清运行态与展示摘要；不改 ready/has_task（目录整理两段式专用）。
        {
            let mut s = self.state.borrow_mut();
            s.control = None;
            s.extracting = false;
        }
        ui.set_busy(false);
        ui.set_paused(false);
        // 未完全解开的包不一定都成功移入「解压失败」（移动本身可能失败或未执行）：
        // 计数只报「未完全解开」，隔离数量另报，二者都不当成完成解压（X-06）。
        ui.set_quarantined_count(i32::try_from(summary.archives_quarantined).unwrap_or(i32::MAX));
        ui.set_progress(-1.0);
        ui.set_progress_note("".into());
        ui.set_metrics(summary.extract_description().into());
        // X-05/S-02：完整成功即永久删除原包及分卷，收尾必须如实报出删除与保留口径
        //（删除失败会让引擎中止整个任务走 Failed，因此这里不可能把失败报成成功）。
        ui.set_status(
            if summary.archives_failed == 0 {
                format!(
                    "解压结束：成功 {} 包；完整成功解出的原压缩包及分卷已永久删除（不经回收站，不可恢复），已有文件与已落盘结果保留；详情见「进度与日志」。",
                    summary.archives_ok
                )
            } else {
                format!(
                    "解压结束：成功 {} 包（原压缩包及分卷已永久删除，不可恢复）；未完全解开 {} 包（其中 {} 个原包或分卷已移入「解压失败」）保留待处理，详情见「进度与日志」。",
                    summary.archives_ok,
                    summary.archives_failed,
                    summary.archives_quarantined
                )
            }
            .into(),
        );
        // U-10：任务结束后刷新失败列表当前页——收尾日志已全部落库，
        // 此刻取数才能看到完整的失败明细（任务结束后仍可查看）。
        if !self.associate_extract_task(ui, path) {
            let page = self.state.borrow().fail_page;
            request_fail_page(&self.state, page);
        }
        if self.close_after_if_idle(ui) {
            let _ = slint::quit_event_loop();
        }
    }
}
