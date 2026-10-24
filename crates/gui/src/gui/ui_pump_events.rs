//! 界面事件泵的事件排空骨架与核心任务事件（Status/Log/Ready/Done/Failed）及 finish_task 收尾；
//! 计划页/设置协议/解压/MD·转换/Git 终态与实时指标处理器分置 ui_pump_events_* 子模块（自 gui.rs 拆分）。
use super::{
    apply_summary, load_plan_state, push_event_log, reload_after_failed, request_fail_page,
    AppWindow, Event, PathBuf, PlanQuery, PlanRow, Rc, RuntimeMode, Summary, Tool, UiPump,
    VecModel,
};

impl UiPump {
    pub(super) fn drain(&self, ui: &AppWindow) -> bool {
        let mut terminal = false;
        // 一轮回调返回后才会绘制，同一轮里只有最后一条状态文案会被看到：逐条 set_status
        // 只是把同一个标签反复标记重绘。暂停态丢弃的判断仍按事件原位置求值（语义不变）；
        // 自带收尾文案的事件（Ready/Done、Failed、ExtractDone）处理前清空缓冲，
        // 保证「后写的收尾文案覆盖先写的状态」与逐条写入时一致。
        let mut pending_status: Option<String> = None;
        for event in self.receiver.try_iter().take(256) {
            match event {
                Event::Status(text) => {
                    if let Some(rest) = text.strip_prefix("SETTINGS_PROGRESS|") {
                        let Some((generation, message)) = rest.split_once('|') else {
                            continue;
                        };
                        if generation.parse::<u64>().ok()
                            == Some(self.state.borrow().settings_generation)
                            && (ui.get_convert_initializing() || !ui.get_settings_loaded())
                        {
                            ui.set_settings_status(message.into());
                        }
                        continue;
                    }

                    if self.apply_converter_status(ui, &text) {
                        continue;
                    }
                    if !ui.get_paused() {
                        pending_status = Some(text);
                    }
                }
                Event::Log(text) => self.push_log(text),
                Event::Ready(path, summary) => {
                    pending_status = None;
                    terminal = true;
                    self.finish_task(ui, path, &summary, true);
                }
                Event::Done(path, summary) => {
                    pending_status = None;
                    terminal = true;
                    self.finish_task(ui, path, &summary, false);
                }
                Event::Failed(error) => {
                    pending_status = None;
                    terminal = true;
                    self.handle_failed(ui, &error);
                }
                Event::SelectionSaved(path, _saved, error) => {
                    self.handle_selection_saved(ui, path, error);
                }
                Event::PlanRecomputed(path, result) => {
                    self.handle_plan_recomputed(ui, path, result);
                }
                Event::PlanPage(path, actions, page, gen, filter, state_gen, snapshot) => {
                    self.handle_plan_page(
                        ui,
                        path,
                        actions,
                        page,
                        gen,
                        filter.as_deref(),
                        state_gen,
                        snapshot.as_ref(),
                    );
                }
                Event::PlanState(path, gen, snapshot) => {
                    self.handle_plan_state(ui, &path, gen, snapshot.as_ref());
                }
                Event::Notice(text) => {
                    if self.apply_notice_settings_protocol(ui, &text) {
                        continue;
                    }
                    ui.set_notice_text(text.into());
                }
                // Error 更新错误文案。
                // 计划筛选曾乐观置「加载中…」：失败必须恢复分页控件（见
                // restore_plan_controls_after_error），避免永久中间态。
                Event::Error(text) => {
                    if self.apply_error_settings_protocol(ui, &text) {
                        continue;
                    }
                    ui.set_error_text(text.into());
                    self.restore_plan_controls_after_error(ui);
                }
                Event::PlanLoadFailed(text, gen, filter) => {
                    self.handle_plan_load_failed(ui, text, gen, filter.as_deref());
                }
                Event::ExtractCount(generation, count) => {
                    self.handle_extract_count(ui, generation, count);
                }
                Event::ExtractDone(path, summary) => {
                    pending_status = None;
                    terminal = true;
                    self.finish_extract_task(ui, path, &summary);
                }
                Event::MdNeedsConfirm(text) => {
                    pending_status = None;
                    terminal = true;
                    self.finish_md_confirm(ui, text);
                }
                Event::MdDone(text) => {
                    if self.apply_converter_terminal(ui, &text) {
                        continue;
                    }
                    pending_status = None;
                    terminal = true;
                    self.finish_md_task(ui, text);
                }
                Event::GitDone(text) => {
                    pending_status = None;
                    terminal = true;
                    self.finish_git_task(ui, text);
                }
            }
        }
        if let Some(text) = pending_status {
            ui.set_status(text.into());
        }
        terminal
    }
    /// Event::Failed 终态收尾（自 drain 抽出，与 finish_* 家族对齐）：取消识别、
    /// 五字段清态、失败/取消双文案、计划与失败列表刷新及 close_after 补查。
    /// 语句顺序与借用范围（reload_after_failed 前的借用释放）原样保留。
    fn handle_failed(&self, ui: &AppWindow, error: &str) {
        // 用户点了“取消任务”时不要用红色错误条报同一个消息：取消是预期操作，不是故障。
        let cancelled = {
            let s = self.state.borrow();
            s.control
                .as_ref()
                .is_some_and(|control| control.is_cancelled())
        };
        {
            let mut s = self.state.borrow_mut();
            s.control = None;
            s.extracting = false;
            s.runtime = RuntimeMode::Organizer;
            s.md_pending = None;
            s.git_shared = None;
            push_event_log(&mut s.logs, error.to_string());
            self.log_dirty.set(true);
        }
        ui.set_busy(false);
        // 就绪与勾选可编辑性由紧随其后的计划快照按库真值给出（界面线程不开库）；
        // 收尾期间先按不可执行处理，避免凭运行前的陈旧值放行（fail-closed）。
        ui.set_ready(false);
        ui.set_plan_editable(false);
        ui.set_paused(false);
        if cancelled {
            ui.set_notice_text(error.into());
            ui.set_status("任务已取消；已完成的操作不会自动回滚，详情见进度与日志".into());
        } else {
            ui.set_error_text(error.into());
            ui.set_status("任务已停止；已完成的操作不会自动回滚，详情见进度与日志".into());
        }
        ui.set_progress(-1.0);
        ui.set_progress_note("".into());
        // 失败/取消后计划与摘要可能已部分变化：从任务库重载摘要与当前计划页
        // （同一次后台开库），避免界面停留在失败前的旧数据。
        // task 提取的借用安全见 reload_after_failed 注释。
        reload_after_failed(ui, &self.state, &self.out);
        // U-10：解压失败/取消后同样刷新失败列表，保留本次已产生的失败明细
        // 供查看（仅解压任务存在时；其他工具的失败与该列表无关）。
        if self.state.borrow().extract_task.is_some() {
            let page = self.state.borrow().fail_page;
            request_fail_page(&self.state, page);
        }
        if self.close_after_if_idle(ui) {
            let _ = slint::quit_event_loop();
        }
    }
    /// Event::Error 的计划页恢复段（自 drain 抽出）：有任务且空闲时按当前状态
    /// 恢复分页控件与页码标签，避免筛选失败后的永久中间态。
    fn restore_plan_controls_after_error(&self, ui: &AppWindow) {
        if ui.get_has_task() && !ui.get_busy() {
            let s = self.state.borrow();
            let page = s.page;
            ui.set_plan_prev_enabled(page > 0);
            ui.set_plan_next_enabled(page + 1 < s.page_starts.len());
            ui.set_plan_page_label(format!("第 {} 页 · 每页最多 100 条", page + 1).into());
        }
    }
    /// Ready/Done 收尾：清运行态、上屏摘要与收尾文案，再让计划 worker 取回页面与就绪快照。
    /// 就绪与勾选可编辑性等快照落地后按任务库真值给出（界面线程不开库，H-02）；这里先按
    /// 「不可执行」上屏（fail-closed，不凭任务收尾前的陈旧值放行），收尾文案则按事件类型——
    /// 分析结束与整理结束是事件自身的确定性结论，不依赖库读取（C-11 如实展示）。
    pub(super) fn finish_task(
        &self,
        ui: &AppWindow,
        path: PathBuf,
        summary: &Summary,
        analysis: bool,
    ) {
        // 新计划一律回到「全部」筛选：沿用上一任务的筛选可能恰好计数为 0，
        // 造成「空列表 + 高亮禁用胶囊」的死角。
        let (filter, organizer_visible) = {
            let mut s = self.state.borrow_mut();
            s.task = Some(path.clone());
            s.plan_page_pending = true;
            s.page = 0;
            s.page_starts = vec![0];
            s.control = None;
            s.applying = false;
            s.extracting = false;
            s.plan_filter = None;
            apply_summary(ui, &mut s, summary);
            (None, s.tool == Tool::Organizer)
        };
        ui.set_busy(false);
        ui.set_paused(false);
        ui.set_ready(false);
        ui.set_plan_editable(false);
        ui.set_has_task(true);
        if organizer_visible {
            ui.set_panel(1);
        }
        ui.set_plan_filter(0);
        ui.set_progress(-1.0);
        ui.set_progress_note("".into());
        ui.set_plan_prev_enabled(false);
        ui.set_plan_next_enabled(false);
        if organizer_visible {
            ui.set_status(
                if analysis {
                    "分析完成（只读）。请检查计划，然后确认执行整理。".into()
                } else {
                    format!(
                        "整理结束：已永久删除 {} 项（不可恢复）· 错误 {} 项；完整记录见「进度与日志」。",
                        summary.deleted, summary.errors
                    )
                }
                .into(),
            );
        }
        // 新任务加载落地前清空上一任务的旧行：action id 是各任务库各自的 rowid，
        // 旧行在此窗口内仍可交互，会把勾选写进新任务库的同 id 动作。
        ui.set_plans(Rc::new(VecModel::from(Vec::<PlanRow>::new())).into());
        // 页面与就绪快照同一次开库取回：快照落地后按库真值点亮或禁用执行（C-11）。
        load_plan_state(
            &self.out,
            &self.state.borrow().plan_load,
            path,
            ui.get_directory().to_string(),
            PlanQuery::page(0, 0, filter, false),
        );
        if self.close_after_if_idle(ui) {
            let _ = slint::quit_event_loop();
        }
    }
}
