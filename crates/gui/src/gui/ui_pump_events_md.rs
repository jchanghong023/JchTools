//! 界面事件泵的 MD 与转 Markdown 事件：MdNeedsConfirm 冲突确认（M-07/M-11/U-09）、MdDone 终态与 CONVERTER_DONE/FAIL 子协议收尾（T-22/S1-03）（自 ui_pump_events.rs 拆分）。
use super::{log_panel_text, push_event_log, AppWindow, RuntimeMode, UiPump};

impl UiPump {
    /// Event::MdNeedsConfirm 的处理：输出/分片冲突转确认框（kind=4）；挂起操作保存在
    /// state.md_pending，确认后重跑，返回检查则由下一次启动自然顶替。close_after 与
    /// 关闭确认框打开期间不得被覆盖确认顶替（U-09）。
    pub(super) fn finish_md_confirm(&self, ui: &AppWindow, text: String) {
        let close_after = self.state.borrow().close_after;
        // M-07/M-11：输出/分片冲突转确认框；挂起的操作保存在 state.md_pending，
        // 确认（kind=4）后重跑，返回检查则由下一次启动自然顶替。
        {
            let mut s = self.state.borrow_mut();
            s.control = None;
            push_event_log(&mut s.logs, format!("等待确认：{text}"));
            self.log_dirty.set(true);
        }
        ui.set_busy(false);
        ui.set_paused(false);
        ui.set_progress(-1.0);
        ui.set_progress_note("".into());
        if close_after && self.close_after_if_idle(ui) {
            // U-09：用户已确认「停止并关闭」。冲突结果可能先于取消检查点
            // 生成、稍后才送达；此时直接退出，不得再弹覆盖确认把用户留在
            // 界面里，也不得残留 close_after。
            let _ = slint::quit_event_loop();
        } else if close_after || ui.get_confirm_kind() == 3 {
            // U-09：「停止任务并关闭」确认框打开期间任务自然走到冲突点：
            // busy/control 均已清空，保持关闭确认框不被破坏性覆盖确认顶替，
            // 用户确认后经 on_confirmed(3) 的无任务分支直接退出；冲突详情
            // 已写入运行日志（上面的 push_event_log）。
            ui.set_status("任务已结束；检测到输出冲突（详情见运行日志），请先完成关闭确认".into());
        } else {
            ui.set_confirm_text(text.into());
            ui.set_acknowledge(false);
            ui.set_confirm_kind(4);
        }
    }

    /// Event::MdDone 中 CONVERTER_DONE|（含统计收尾）与 CONVERTER_FAIL|（失败收尾）
    /// 两个子协议的解码与上屏；命中任一前缀返回 true（原臂以 continue 结束且不置
    /// terminal，不参与收尾日志上屏判定），否则返回 false 交回 MD 主分支收尾。
    pub(super) fn apply_converter_terminal(&self, ui: &AppWindow, text: &str) -> bool {
        if let Some(rest) = text.strip_prefix("CONVERTER_DONE|") {
            let mut fields = rest.split('|');
            let success = fields.next().unwrap_or("0");
            let partial = fields.next().unwrap_or("0");
            let failed = fields.next().unwrap_or("0");
            let skipped_existing = fields.next().unwrap_or("0");
            let skipped_duplicate = fields.next().unwrap_or("0");
            let stopped = fields.next() == Some("1");
            let final_status = if stopped && failed != "0" {
                "已停止，转换失败"
            } else if stopped {
                "已停止"
            } else if partial != "0" || failed != "0" {
                "转换部分失败"
            } else {
                "转换完成"
            };
            let total_elapsed = {
                let mut s = self.state.borrow_mut();
                // T-22：收尾统计的总耗时从任务起算到本事件到达时刻。
                let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                s.convert_cancel = None;
                s.runtime = RuntimeMode::Organizer;
                s.convert_current.clear();
                push_event_log(&mut s.convert_logs, format!(
                    "转 Markdown {final_status}：成功 {success}，部分提取 {partial}，失败 {failed}，已有结果跳过 {skipped_existing}，重复结果跳过 {skipped_duplicate}"
                ));
                self.log_dirty.set(true);
                elapsed
            };
            ui.set_busy(false);
            ui.set_paused(false);
            ui.set_convert_progress(if stopped { -1.0 } else { 1.0 });
            ui.set_convert_progress_note(final_status.into());
            ui.set_convert_metrics(format!("成功 {success} · 部分提取 {partial} · 失败 {failed} · 已有结果跳过 {skipped_existing} · 重复结果跳过 {skipped_duplicate} · 总耗时 {total_elapsed:.1}s").into());
            ui.set_convert_status(final_status.into());
            ui.set_convert_log_text(log_panel_text(&self.state.borrow().convert_logs).into());
            // 收尾日志直接上屏（S1-03 的例外路径），并清掉脏标记。
            self.convert_log_dirty.set(false);
            if ui.get_screen() == 5 {
                ui.set_status(
                    if stopped && failed != "0" {
                        "转 Markdown 已停止，但存在转换失败，详情见进度与日志"
                    } else if stopped {
                        "转 Markdown 已停止；已完成的输出保留"
                    } else if partial != "0" || failed != "0" {
                        "转 Markdown 部分失败，详情见进度与日志"
                    } else {
                        "转 Markdown 已完成，详情见进度与日志"
                    }
                    .into(),
                );
            }
            if self.close_after_if_idle(ui) {
                let _ = slint::quit_event_loop();
            }
            return true;
        }
        if let Some(error) = text.strip_prefix("CONVERTER_FAIL|") {
            let total_elapsed = {
                let mut s = self.state.borrow_mut();
                // T-22：失败收尾的总耗时同样从任务起算到本事件到达时刻
                //（与 DONE 分支同口径）。
                let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                s.convert_cancel = None;
                s.runtime = RuntimeMode::Organizer;
                s.convert_current.clear();
                push_event_log(&mut s.convert_logs, format!("转 Markdown 失败：{error}"));
                self.log_dirty.set(true);
                elapsed
            };
            ui.set_busy(false);
            ui.set_paused(false);
            ui.set_convert_progress(-1.0);
            ui.set_convert_progress_note("".into());
            // FAIL 代表真实错误；停止意图不是请求已确认取消的证据。
            // 正常取消已由 Summary.stopped 经 DONE 分类。
            ui.set_convert_metrics(format!("转换失败 · 总耗时 {total_elapsed:.1}s").into());
            ui.set_convert_status("转换失败，请查看日志".into());
            ui.set_error_text(error.into());
            if ui.get_screen() == 5 {
                ui.set_status("转 Markdown 失败，详情见进度与日志".into());
            }
            ui.set_convert_log_text(log_panel_text(&self.state.borrow().convert_logs).into());
            // 收尾日志直接上屏（S1-03 的例外路径），并清掉脏标记。
            self.convert_log_dirty.set(false);
            if self.close_after_if_idle(ui) {
                let _ = slint::quit_event_loop();
            }
            return true;
        }
        false
    }

    /// Event::MdDone 主分支的处理（非 CONVERTER 子协议）：MD 合并/拆分任务终态。
    pub(super) fn finish_md_task(&self, ui: &AppWindow, text: String) {
        // U-03：短 MD 任务可能先于下一次轮询完成；释放 Control 前定格最终计数。
        self.refresh_runtime(ui);
        {
            let mut s = self.state.borrow_mut();
            s.control = None;
            s.runtime = RuntimeMode::Organizer;
            push_event_log(&mut s.logs, text.clone());
            self.log_dirty.set(true);
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
