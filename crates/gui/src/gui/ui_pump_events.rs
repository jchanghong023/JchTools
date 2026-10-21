//! 界面事件泵的事件排空与任务收尾方法（drain/finish_task/refresh_runtime，自 gui.rs 拆分）。
use super::*;

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
                        push_event_log(&mut s.logs, error.clone());
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
                        ui.set_status(
                            "任务已取消；已完成的操作不会自动回滚，详情见进度与日志".into(),
                        );
                    } else {
                        ui.set_error_text(error.into());
                        ui.set_status(
                            "任务已停止；已完成的操作不会自动回滚，详情见进度与日志".into(),
                        );
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
                Event::SelectionSaved(path, _saved, error) => {
                    let mut s = self.state.borrow_mut();
                    if s.task.as_ref() != Some(&path) {
                        continue;
                    }
                    s.pending_selection = s.pending_selection.saturating_sub(1);
                    if let Some(error) = error {
                        ui.set_error_text(error.into());
                    }
                    if s.pending_selection == 0 && s.plan_recompute_inflight == 0 {
                        // 部分保存失败也针对数据库实际提交的选择重算依赖。
                        s.plan_page_pending = true;
                        s.plan_load.page.fetch_add(1, Ordering::AcqRel);
                        s.plan_load.state.fetch_add(1, Ordering::AcqRel);
                        s.plan_recompute_inflight = 1;
                        ui.set_ready(false);
                        ui.set_plan_editable(false);
                        let out = self.out.clone();
                        std::thread::spawn(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    organizer_engine::recompute_plan(&path)
                                }));
                            let result = match result {
                                Ok(Ok(cancelled)) => Ok(cancelled),
                                Ok(Err(error)) => Err(format!("重算受影响计划失败：{error:#}")),
                                Err(_) => Err("重算受影响计划时后台操作意外退出".into()),
                            };
                            let _ = out.send(Event::PlanRecomputed(path, result));
                        });
                    }
                }
                Event::PlanRecomputed(path, result) => {
                    let mut s = self.state.borrow_mut();
                    if s.task.as_ref() != Some(&path) {
                        continue;
                    }
                    s.plan_recompute_inflight = s.plan_recompute_inflight.saturating_sub(1);
                    s.plan_page_pending = true;
                    let cancelled = match result {
                        Ok(cancelled) => cancelled,
                        Err(error) => {
                            s.plan_recompute_failed = true;
                            ui.set_ready(false);
                            ui.set_error_text(error.into());
                            continue;
                        }
                    };
                    if s.pending_selection != 0 || s.plan_recompute_inflight != 0 {
                        continue;
                    }
                    if cancelled > 0 && !ui.get_busy() {
                        ui.set_status(
                            format!("已按取消的勾选重算：{cancelled} 个依赖项失效，转为未勾选")
                                .into(),
                        );
                    }
                    // cancelled=0 也重载：连续勾选及失败行均以 DB 真值呈现。
                    load_plan_state(
                        &self.out,
                        &s.plan_load,
                        path,
                        ui.get_directory().to_string(),
                        PlanQuery::page(
                            s.page_starts.get(s.page).copied().unwrap_or(0),
                            s.page,
                            s.plan_filter.clone(),
                            false,
                        ),
                    );
                }
                Event::PlanPage(path, mut actions, page, gen, filter, state_gen, snapshot) => {
                    if self.state.borrow().task.as_ref() != Some(&path) {
                        continue;
                    }
                    let mut s = self.state.borrow_mut();
                    if s.pending_selection != 0 || s.plan_recompute_inflight != 0 {
                        continue;
                    }
                    // 过期事件：页面代际须仍是当前请求（只取快照的请求不动页面代际）。
                    let latest = s.plan_load.page.load(Ordering::Acquire);
                    if !plan_page_event_accepted(
                        gen,
                        latest,
                        filter.as_deref(),
                        s.plan_filter.as_deref(),
                    ) {
                        continue;
                    }
                    let more = actions.len() > 100;
                    actions.truncate(100);
                    if more && s.page_starts.len() <= page + 1 {
                        if let Some(last) = actions.last() {
                            s.page_starts.push(last.id);
                        }
                    }
                    s.page = page;
                    // 只有真的还有上一页/下一页时才让按钮可用，避免点了没有任何反应。
                    ui.set_plan_prev_enabled(page > 0);
                    ui.set_plan_next_enabled(more);
                    ui.set_plan_page_label(format!("第 {} 页 · 每页最多 100 条", page + 1).into());
                    let rows = actions
                        .into_iter()
                        .map(|a| PlanRow {
                            id: a.id.to_string().into(),
                            selected: a.selected,
                            kind: match a.kind {
                                ActionKind::Delete => "删除",
                                ActionKind::Move => "移动/重命名",
                                ActionKind::EmptyDirectory => "空目录复查",
                            }
                            .into(),
                            source: a.source.into(),
                            target: a
                                .target
                                .unwrap_or_else(|| {
                                    a.keeper
                                        .as_ref()
                                        .map(|v| format!("保留 {}", v.0))
                                        .unwrap_or_default()
                                })
                                .into(),
                            // H-05/C-07：空目录清理是整理收尾的强制动作，行内明示不可取消，
                            // 与复选框的禁用口径一致（C-11 如实显示）。
                            reason: if matches!(a.kind, ActionKind::EmptyDirectory) {
                                format!("{}（整理完成后强制清理，不可取消）", a.reason)
                            } else {
                                a.reason
                            }
                            .into(),
                            state: plan_row_state(a.selected, a.state.as_str()).into(),
                        })
                        .collect::<Vec<_>>();
                    ui.set_plans(Rc::new(VecModel::from(rows)).into());
                    s.plan_page_pending = snapshot.is_none();
                    // 失败收尾的请求顺带取回摘要：与页面同一次开库，界面线程不再自己开库。
                    if let Some(summary) = snapshot
                        .as_ref()
                        .and_then(|snapshot| snapshot.summary.as_ref())
                    {
                        apply_summary(ui, &mut s, summary);
                    }
                    // 页面重建后同步一次就绪：勾选保存失败触发重载时，不能让「开始执行」
                    // 因为一次瞬时数据库失败而一直禁用。快照按当前配置/目录纯比较判定。
                    if !ui.get_busy()
                        && s.pending_selection == 0
                        && s.plan_recompute_inflight == 0
                        && !s.plan_recompute_failed
                        && s.task.as_ref() == Some(&path)
                    {
                        apply_plan_readiness(ui, &s, snapshot.as_ref());
                    }
                    // 独立快照若先于新页到达，会被呈现门禁拒绝；显示后补查当前目录。
                    if !ui.get_busy() && s.plan_load.state.load(Ordering::Acquire) != state_gen {
                        load_plan_state(
                            &self.out,
                            &s.plan_load,
                            path,
                            ui.get_directory().to_string(),
                            PlanQuery::readiness(),
                        );
                    }
                }
                Event::PlanState(path, gen, snapshot) => {
                    let s = self.state.borrow();
                    if s.task.as_ref() != Some(&path) {
                        continue;
                    }
                    // 过期快照：更新的请求在途，低代际结果不得改写当前就绪状态。
                    if s.plan_load.state.load(Ordering::Acquire) != gen {
                        continue;
                    }
                    // 运行中与勾选在途：状态栏文案归任务事件与勾选流程，就绪重算不得改写。
                    if ui.get_busy()
                        || s.pending_selection != 0
                        || s.plan_recompute_inflight != 0
                        || s.plan_recompute_failed
                        || s.plan_page_pending
                    {
                        continue;
                    }
                    apply_plan_readiness(ui, &s, snapshot.as_ref());
                }
                Event::Notice(text) => {
                    if let Some(rest) = text.strip_prefix("SETTINGS_LOADED|") {
                        let Some((generation, payload)) = rest.split_once('|') else {
                            continue;
                        };
                        if generation.parse::<u64>().ok()
                            != Some(self.state.borrow().settings_generation)
                        {
                            continue;
                        }
                        // 启动快照不是设置操作的终态，不清运行标志或消费关闭请求。
                        self.apply_settings_snapshot(ui, payload, false);
                        continue;
                    }
                    if let Some(rest) = text.strip_prefix("SETTINGS_READY|") {
                        let Some((generation, payload)) = rest.split_once('|') else {
                            continue;
                        };
                        if generation.parse::<u64>().ok()
                            != Some(self.state.borrow().settings_generation)
                        {
                            continue;
                        }
                        self.apply_settings_snapshot(ui, payload, true);
                        continue;
                    }
                    if let Some(rest) = text.strip_prefix("SETTINGS_FEATURES|") {
                        let Some((generation, payload)) = rest.split_once('|') else {
                            continue;
                        };
                        if generation.parse::<u64>().ok()
                            != Some(self.state.borrow().settings_generation)
                        {
                            continue;
                        }
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                            ui.set_settings_document_status(
                                value["document_status"].as_str().unwrap_or("未下载").into(),
                            );
                            ui.set_settings_snapshot_status(
                                value["snapshot_status"].as_str().unwrap_or("未下载").into(),
                            );
                            ui.set_settings_media_status(
                                value["media_status"].as_str().unwrap_or("未下载").into(),
                            );
                            ui.set_settings_ffmpeg_status(
                                value["ffmpeg_status"].as_str().unwrap_or("未下载").into(),
                            );
                            ui.set_settings_audio_model_status(
                                value["audio_model_status"]
                                    .as_str()
                                    .unwrap_or("未下载")
                                    .into(),
                            );
                        }
                        continue;
                    }

                    ui.set_notice_text(text.into());
                }
                // Error 更新错误文案。
                // 计划筛选曾乐观置「加载中…」：失败必须恢复分页控件，避免永久中间态。
                Event::Error(text) => {
                    if let Some(rest) = text.strip_prefix("SETTINGS_LOAD_ERR|") {
                        let Some((generation, error)) = rest.split_once('|') else {
                            continue;
                        };
                        if generation.parse::<u64>().ok()
                            == Some(self.state.borrow().settings_generation)
                        {
                            ui.set_settings_status(error.into());
                            if !ui.get_settings_loaded() {
                                ui.set_settings_load_failed(true);
                            }
                        }
                        continue;
                    }

                    if let Some(rest) = text.strip_prefix("SETTINGS_ERROR|") {
                        let Some((generation, error)) = rest.split_once('|') else {
                            continue;
                        };
                        if generation.parse::<u64>().ok()
                            != Some(self.state.borrow().settings_generation)
                        {
                            continue;
                        }
                        ui.set_settings_status(error.into());
                        if ui.get_convert_initializing() {
                            ui.set_settings_audio_failed(true);
                        }
                        if !ui.get_settings_loaded() {
                            ui.set_settings_load_failed(true);
                        }
                        ui.set_convert_initializing(false);
                        ui.set_convert_runtime_saving(false);
                        self.state.borrow_mut().convert_init_cancel = None;
                        start_markdown_readiness(ui, &self.state, &self.out);
                        start_snap_readiness(ui, &self.state, &self.out);
                        // U-09/S1-01：任务运行中不得消费 close_after 提前退出，关闭由
                        // 任务终态统一完成（镜像 on_close_requested 的守卫口径）。
                        if self.close_after_if_idle(ui) {
                            let _ = slint::quit_event_loop();
                        }
                        continue;
                    }

                    ui.set_error_text(text.into());
                    if ui.get_has_task() && !ui.get_busy() {
                        let s = self.state.borrow();
                        let page = s.page;
                        ui.set_plan_prev_enabled(page > 0);
                        ui.set_plan_next_enabled(page + 1 < s.page_starts.len());
                        ui.set_plan_page_label(
                            format!("第 {} 页 · 每页最多 100 条", page + 1).into(),
                        );
                    }
                }
                Event::PlanLoadFailed(text, gen, filter) => {
                    // 计划加载失败带代际/筛选归属：请求已过期（用户切走筛选/翻页后
                    // 旧请求才失败）时不得把红条误报到当前正确视图上。
                    let accepted = {
                        let s = self.state.borrow();
                        s.plan_load.page.load(Ordering::Acquire) == gen
                            && s.plan_filter.as_deref() == filter.as_deref()
                    };
                    if accepted {
                        self.state.borrow_mut().plan_page_pending = true;
                        ui.set_ready(false);
                        ui.set_plan_editable(false);
                        ui.set_error_text(text.into());
                    }
                }
                Event::ExtractCount(generation, count) => {
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
                                        &format!("清点到 {n} 个压缩包（按当前扫描范围，已排除「解压失败」目录）。"),
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
                Event::ExtractDone(path, summary) => {
                    pending_status = None;
                    terminal = true;
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
                    ui.set_quarantined_count(
                        i32::try_from(summary.archives_quarantined).unwrap_or(i32::MAX),
                    );
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
                Event::MdNeedsConfirm(text) => {
                    pending_status = None;
                    terminal = true;
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
                        ui.set_status(
                            "任务已结束；检测到输出冲突（详情见运行日志），请先完成关闭确认".into(),
                        );
                    } else {
                        ui.set_confirm_text(text.into());
                        ui.set_acknowledge(false);
                        ui.set_confirm_kind(4);
                    }
                }
                Event::MdDone(text) => {
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
                        ui.set_convert_log_text(
                            log_panel_text(&self.state.borrow().convert_logs).into(),
                        );
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
                        continue;
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
                            push_event_log(
                                &mut s.convert_logs,
                                format!("转 Markdown 失败：{error}"),
                            );
                            self.log_dirty.set(true);
                            elapsed
                        };
                        ui.set_busy(false);
                        ui.set_paused(false);
                        ui.set_convert_progress(-1.0);
                        ui.set_convert_progress_note("".into());
                        // FAIL 代表真实错误；停止意图不是请求已确认取消的证据。
                        // 正常取消已由 Summary.stopped 经 DONE 分类。
                        ui.set_convert_metrics(
                            format!("转换失败 · 总耗时 {total_elapsed:.1}s").into(),
                        );
                        ui.set_convert_status("转换失败，请查看日志".into());
                        ui.set_error_text(error.into());
                        if ui.get_screen() == 5 {
                            ui.set_status("转 Markdown 失败，详情见进度与日志".into());
                        }
                        ui.set_convert_log_text(
                            log_panel_text(&self.state.borrow().convert_logs).into(),
                        );
                        // 收尾日志直接上屏（S1-03 的例外路径），并清掉脏标记。
                        self.convert_log_dirty.set(false);
                        if self.close_after_if_idle(ui) {
                            let _ = slint::quit_event_loop();
                        }
                        continue;
                    }
                    pending_status = None;
                    terminal = true;
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
                Event::GitDone(text) => {
                    pending_status = None;
                    terminal = true;
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
                                i32::try_from(shared.retry.load(Ordering::Relaxed))
                                    .unwrap_or(i32::MAX),
                            );
                            ui.set_git_retry_wait(
                                i32::try_from(shared.retry_wait.load(Ordering::Relaxed))
                                    .unwrap_or(i32::MAX),
                            );
                            if let Ok(state_text) = shared.state.lock() {
                                ui.set_git_state(state_text.as_str().into());
                            }
                            if let Ok(stage) = shared.stage.lock() {
                                ui.set_git_stage(stage.as_str().into());
                            }
                            ui.set_git_done(
                                i32::try_from(shared.done.load(Ordering::Relaxed))
                                    .unwrap_or(i32::MAX),
                            );
                            ui.set_git_total(
                                i32::try_from(shared.total.load(Ordering::Relaxed))
                                    .unwrap_or(i32::MAX),
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
        }
        if let Some(text) = pending_status {
            ui.set_status(text.into());
        }
        terminal
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
    /// 实时指标与进度：只在真正运行时刷新，空闲时不碰 metrics（否则耗时一直涨）。
    pub(super) fn refresh_runtime(&self, ui: &AppWindow) {
        if !ui.get_busy() {
            return;
        }
        let s = self.state.borrow();
        let Some(control) = &s.control else {
            // 转 Markdown 任务无共享 Control（独立取消原子量）：共享进度条不得
            // 固定显示「准备中」——如实指向转 Markdown 页的实时进度（U-03/H-02）。
            if s.runtime == RuntimeMode::MarkdownConverter {
                ui.set_progress(-1.0);
                ui.set_progress_note("转换任务进行中，实时进度见转 Markdown 页".into());
                // T-22：运行期间实时显示当前文件与耗时（busy 门禁保证空闲不刷）；
                // 尚未进入单文件处理（扫描/文件间隙）时只报运行时长。
                let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                let activity = if s.convert_current.is_empty() {
                    format!("已运行 {elapsed:.1}s")
                } else {
                    format!("正在处理 {} · 耗时 {elapsed:.1}s", s.convert_current)
                };
                let counts = &s.convert_counts;
                ui.set_convert_metrics(
                    format!(
                        "{activity} · 成功 {} · 部分提取 {} · 失败 {} · 跳过 {}",
                        counts.success, counts.partial, counts.failed, counts.skipped
                    )
                    .into(),
                );
            } else {
                ui.set_progress(-1.0);
                ui.set_progress_note("准备中".into());
            }
            return;
        };
        // Git：从共享进度读取全部界面字段（G-13；不写整理流程计数器）。
        if s.runtime == RuntimeMode::Git {
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
                if let Ok(stage) = shared.stage.lock() {
                    ui.set_git_stage(stage.as_str().into());
                }
                if let Ok(state_text) = shared.state.lock() {
                    ui.set_git_state(state_text.as_str().into());
                }
                let retry = shared.retry.load(Ordering::Relaxed);
                ui.set_git_retry(i32::try_from(retry).unwrap_or(i32::MAX));
                ui.set_git_retry_wait(
                    i32::try_from(shared.retry_wait.load(Ordering::Relaxed)).unwrap_or(i32::MAX),
                );
                let done = shared.done.load(Ordering::Relaxed);
                let total = shared.total.load(Ordering::Relaxed);
                ui.set_git_done(i32::try_from(done).unwrap_or(i32::MAX));
                ui.set_git_total(i32::try_from(total).unwrap_or(i32::MAX));
                let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
                if total > 0 {
                    // 进度分数为显示用途（Slint progress 即 f32），整数→浮点无受检 API。
                    // [quality-baseline approved 2026-10-03] 同类显示转换，经用户裁定保留
                    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                    let progress = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
                    ui.set_progress(progress);
                    ui.set_progress_note(format!("{done} / {total}").into());
                } else {
                    ui.set_progress(-1.0);
                    ui.set_progress_note("".into());
                }
                ui.set_metrics(
                    format!("已完成 {done} / {total} · 重试 {retry} 次 · 耗时 {elapsed:.1}s")
                        .into(),
                );
            }
            return;
        }
        // MD：worker 经 control 上报已完成/总数（不写整理的读取量指标）。
        if s.runtime == RuntimeMode::Md {
            let done = control.completed.load(Ordering::Relaxed);
            let planned = control.planned().unwrap_or(0);
            let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
            ui.set_metrics(format!("已处理 {done} / {planned} · 耗时 {elapsed:.1}s").into());
            if planned > 0 {
                // 进度分数为显示用途（Slint progress 即 f32），整数→浮点无受检 API。
                // [quality-baseline approved 2026-10-03] 同类显示转换，经用户裁定保留
                #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
                let progress = (done as f64 / planned as f64).clamp(0.0, 1.0) as f32;
                ui.set_progress(progress);
                ui.set_progress_note(format!("{done} / {planned}").into());
            } else {
                ui.set_progress(-1.0);
                ui.set_progress_note("".into());
            }
            return;
        }
        let read = control.read_bytes.load(Ordering::Relaxed);
        let elapsed = s.started.elapsed().as_secs_f64().max(0.001);
        let done = control.completed.load(Ordering::Relaxed);
        let scanned = control.scanned.load(Ordering::Relaxed);
        // 执行分母：worker 数出的「仍勾选且待执行」项数优先（U-03）；尚未数出时沿用摘要里的
        // 全量分母（旧口径是启动前同步数出，这里把这段等待挪到后台，界面先见到反馈）。
        let planned = control.planned().unwrap_or(s.planned);
        // 执行阶段不再扫描，写“扫描 0 个文件”只会让人误以为没扫到东西，这里只报执行进度。
        if s.applying {
            ui.set_metrics(
                format!("执行中：已处理 {done} / {planned} 项 · 耗时 {elapsed:.1}s").into(),
            );
        } else if s.extracting {
            // 解压不写整理流程的计数器（read_bytes/completed 只由哈希与计划执行累加），
            // 沿用整理口径会显示恒为 0 的「读取 / 平均读取 / 计划项」。这里只报解压
            // 自身真实可得的包进度与耗时（U-03：总量未知时不得编造数字）。
            ui.set_metrics(format!("解压中：已处理 {done} 包 · 耗时 {elapsed:.1}s").into());
        } else {
            // 吞吐速率为显示用途，u64→f64 的精度损失无意义。
            // [quality-baseline approved 2026-09-19] 显示用途转换，经用户裁定保留
            #[allow(clippy::cast_precision_loss)]
            let rate_mib_s = read as f64 / elapsed / 1_048_576.0;
            ui.set_metrics(format!("扫描 {} 个文件 · 读取 {} · 已处理 {} 个计划项 · 耗时 {:.1}s · 平均读取 {:.1} MiB/s",scanned,bytes(read),done,elapsed,rate_mib_s).into());
        }
        // 执行阶段按计划项计数；分析阶段总量未知（扫描/哈希/解压包大小不能提前预知）
        if s.applying && planned > 0 {
            // 进度分数为显示用途，整数→浮点的精度损失无意义。
            // [quality-baseline approved 2026-09-19] Slint progress 属性即 f32，整数→浮点无受检 API
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let progress = (done as f64 / planned as f64).clamp(0.0, 1.0) as f32;
            ui.set_progress(progress);
            ui.set_progress_note(format!("{done} / {planned} 项").into());
        } else if s.applying {
            // 执行阶段没有可执行的计划项（全部取消勾选或空计划）时，不要显示“已扫描 0 个文件”
            // 让人误以为没扫到东西。
            ui.set_progress(-1.0);
            ui.set_progress_note("没有待执行的计划项".into());
        } else if s.extracting {
            // 解压期间不显示整理口径的进度说明；已处理包数与耗时由上面的实时指标行给出。
            ui.set_progress(-1.0);
            ui.set_progress_note("".into());
        } else {
            ui.set_progress(-1.0);
            ui.set_progress_note(format!("已扫描 {scanned} 个文件 · 读取 {}", bytes(read)).into());
        }
    }
}
