//! 同步回调总接线：wire_sync 与任务生命周期 wire_task_lifecycle（自 gui.rs 拆分）。
use super::{
    can_request_acp_stop, changed, confirm_md_override, ensure_snap_supervisor,
    extract_confirm_text, extract_engine, hint_for, invalid_number_error,
    invalidate_idle_directory, load_plan_state, log_panel_text, mpsc, parse_capacity,
    patch_plan_row, patch_rule_row, pointer_position, refresh, registry, request_acp_action,
    request_codeagent, request_fail_page, reset_tool_list, retain_invalid_number, show_error,
    start_extract, start_markdown_readiness, start_snap_readiness, start_task,
    sync_ready_after_directory, sync_rules, AcpCommand, AppWindow, CodeAgentCommand,
    ComponentHandle, Database, Duration, Event, EventSender, Ordering, PathBuf, PlanQuery, Rc,
    RefCell, RuntimeMode, State, Tool, ToolRow, VecModel, WindowDrag,
};

/// 同步回调装配：规则表、分区/工具导航、主题与输入校验——纯属性/状态操作，
/// 不依赖事件循环，独立成函数以便无头 GUI 测试直接装配后断言。
pub(super) fn wire_sync(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    {
        let weak = ui.as_weak();
        let state = state.clone();
        // 无任务目录检查由单 worker 合并执行，不在界面线程触碰文件系统；
        // 只有已有整理计划仍沿原来的 300ms 去抖 + 任务库快照路径重算。
        let debounce = std::rc::Rc::new(slint::Timer::default());
        ui.on_root_edited({
            let weak = weak.clone();
            let state = state.clone();
            let debounce = debounce.clone();
            let out = out.clone();
            move || {
                if let Some(ui) = weak.upgrade() {
                    // 编辑目录立即失效执行按钮（旧同步行为的安全属性）：去抖窗口内不得凭
                    // 陈旧 ready 打开确认框；随后去抖重算，目录与计划仍一致时重新点亮。
                    ui.set_ready(false);
                    if ui.get_confirm_kind() == 1 {
                        ui.set_confirm_kind(0);
                        ui.set_confirm_pending(false);
                    }
                    state.borrow_mut().extract_generation += 1;
                    invalidate_idle_directory(&state.borrow());
                    if !ui.get_has_task() || state.borrow().tool != Tool::Organizer {
                        sync_ready_after_directory(&ui, &state.borrow(), &out);
                        return;
                    }
                    let weak = weak.clone();
                    let state = state.clone();
                    let out = out.clone();
                    debounce.start(
                        slint::TimerMode::SingleShot,
                        Duration::from_millis(300),
                        move || {
                            if let Some(ui) = weak.upgrade() {
                                sync_ready_after_directory(&ui, &state.borrow(), &out);
                            }
                        },
                    );
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_select_tool(move |id| {
            if let Some(ui) = weak.upgrade() {
                // 点回工具时恢复全量列表：搜索过滤不得在切页后残留。
                reset_tool_list(&ui);
                if let Some(tool) = Tool::from_id(id.as_str()) {
                    let screen = match tool {
                        Tool::Extract => 2,
                        Tool::Organizer => 0,
                        Tool::Md => 3,
                        Tool::Git => 4,
                        Tool::MarkdownConverter => 5,
                        Tool::SnapOcr => 6,
                        Tool::AcpHttp => 8,
                        Tool::CodeAgent => 9,
                    };
                    ui.set_screen(screen);
                    ui.set_active_tool_id(id.clone());
                    // 全局提示属于当前页面；切换工具时不得把转换组件缺失详情
                    // 残留到截图或旧工具页面。
                    ui.set_error_text("".into());
                    ui.set_notice_text("".into());
                    ui.set_convert_detail_text("".into());
                    ui.set_convert_detail_open(false);
                    {
                        let mut s = state.borrow_mut();
                        // U-11：已结束任务的共享日志和计数不属于另一个工具。
                        // 运行中保留任务上下文，不能因导航清除尚在产生的结果。
                        if s.tool != tool && !ui.get_busy() {
                            s.logs.clear();
                            ui.set_log_text("".into());
                            ui.set_metrics("尚未开始".into());
                            ui.set_progress(-1.0);
                            ui.set_progress_note("".into());
                        }
                        s.tool = tool;
                        // 切工具回到该工具的第一个规则分区；MD/Git 无规则分区（R-01），
                        // 分区名清空，界面不显示分区胶囊。分区集合随工具变化（R-01）。
                        s.section = tool
                            .sections()
                            .first()
                            .map_or_else(String::new, |first| (*first).to_string());
                    }
                    ui.set_section(0);
                    // 面板页签集合同样随工具变化（目录整理 0–2、递归解压 0–1）：
                    // 共享索引不重置会停在另一工具才有的面板，内容区整块空白（U-11）。
                    ui.set_panel(0);
                    // U-11：MD 整理回到默认子页「合并 MD」，不得停留在拆分子页。
                    ui.set_md_subpage(0);
                    // 转换任务运行中切页再切回时保留实时进度：无条件重置会让长任务
                    // 期间离开再回来的进度条到下一个 CONVERTER_FILE_STARTED 前
                    // 保持不确定。运行判定与 refresh_runtime 同口径（busy 且
                    // runtime=MarkdownConverter）；非运行态保持原重置口径，上次
                    // 任务的残留进度不得带到新会话。
                    if !(ui.get_busy() && state.borrow().runtime == RuntimeMode::MarkdownConverter)
                    {
                        ui.set_convert_progress(-1.0);
                        ui.set_convert_progress_note("".into());
                    }
                    if tool == Tool::MarkdownConverter {
                        ui.set_convert_log_text(
                            log_panel_text(&state.borrow().convert_logs).into(),
                        );
                        start_markdown_readiness(&ui, &state, &out);
                    }
                    if tool == Tool::SnapOcr {
                        // 进入页面即做只读资产复检并连上服务监督线程（O-09/O-11）；
                        // 未初始化、服务缺失都不阻塞其他工具（O-03）。
                        start_snap_readiness(&ui, &state, &out);
                        if state.borrow().snap_commands.is_none() {
                            ensure_snap_supervisor(&state, &out);
                        }
                    }
                    if tool == Tool::CodeAgent {
                        ui.set_codeagent_status("正在连接独立后台…".into());
                        ui.set_codeagent_error("".into());
                        request_codeagent(&state, CodeAgentCommand::Ensure);
                    }
                    refresh(&ui, &state.borrow());
                    // 切工具不是规则或目录改动（C-10/R-04）：就绪计划按「任务+配置+目录」
                    // 重算保持可执行，与目录重输同口径，而不是一律失效。
                    sync_ready_after_directory(&ui, &state.borrow(), &out);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_convert_selection_changed(move || {
            if let Some(ui) = weak.upgrade() {
                start_markdown_readiness(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_search_tools(move |query| {
            if let Some(ui) = weak.upgrade() {
                let query = query.to_lowercase();
                let tools = registry::tools()
                    .iter()
                    .filter(|t| {
                        format!("{} {}", t.name, t.summary)
                            .to_lowercase()
                            .contains(&query)
                    })
                    .map(|t| ToolRow {
                        id: t.id.into(),
                        name: t.name.into(),
                        summary: t.summary.into(),
                    })
                    .collect::<Vec<_>>();
                ui.set_tools(Rc::new(VecModel::from(tools)).into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_navigation(move |screen| {
            if let Some(ui) = weak.upgrade() {
                ui.set_screen(screen);
                ui.set_error_text("".into());
                ui.set_notice_text("".into());
                // 「关于」不是工具：清空 active-tool-id，侧栏不高亮任何工具。
                // 返回工具页统一走 select_tool（工具 NavItem 的点击回调），此处不再恢复列表：
                // 该回调在生产中只被「关于」NavItem 以 1 调用，其余分支属不可达路径。
                if screen == 1 || screen == 7 {
                    ui.set_active_tool_id("".into());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_select_section(move |index| {
            {
                // 分区集合随工具变化（R-01）：递归解压=解压/安全与性能，目录整理=去重/归类/清理/安全与性能。
                let sections = state.borrow().tool.sections();
                if let Some(section) = sections.get(usize::try_from(index.max(0)).unwrap_or(0)) {
                    // UI 与状态用同一个钳制值：负 index 不再出现「状态到解压、高亮停在 -1」的分叉。
                    let index = index.max(0);
                    state.borrow_mut().section = section.to_string();
                    if let Some(ui) = weak.upgrade() {
                        {
                            ui.set_section(index);
                            refresh(&ui, &state.borrow());
                        }
                    }
                }
            }
        });
    }
    // 布尔/下拉：不整表重建 rules。重建会在 selected/toggled 回调里销毁 ListView 里的
    // ComboBox，用户点选后的文字会停在旧值；改为配置写入 + 就地更新该行，
    // 联动行的出现/消失交给 sync_rules 增量插入删除。
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_rule_bool(move |key, value| {
            if let Some(ui) = weak.upgrade() {
                if changed(&ui, &state, key.as_str(), &value.into(), false) {
                    patch_rule_row(&ui, key.as_str(), |row| row.checked = value);
                    sync_rules(&ui, &state.borrow());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_rule_choice(move |key, index| {
            if let Some(ui) = weak.upgrade() {
                let spec = state
                    .borrow()
                    .specs
                    .iter()
                    .find(|s| s.key == key.as_str())
                    .cloned();
                let selected = spec
                    .as_ref()
                    .and_then(|s| s.choices.get(usize::try_from(index.max(0)).unwrap_or(0)))
                    .map(|c| c[0].clone());
                if let Some(value) = selected {
                    let hint = spec
                        .as_ref()
                        .map(|s| hint_for(s, &serde_json::Value::from(value.as_str())));
                    if changed(&ui, &state, key.as_str(), &value.into(), false) {
                        patch_rule_row(&ui, key.as_str(), |row| {
                            row.index = index;
                            if let Some(hint) = &hint {
                                row.hint = hint.clone().into();
                            }
                        });
                        sync_rules(&ui, &state.borrow());
                    }
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        // 主题选择在「关于」页（应用级设置，不属于目录整理的处理规则）；
        // changed() 对 theme 键会跳过计划失效并直接套用。
        ui.on_select_theme(move |choice| {
            if let Some(ui) = weak.upgrade() {
                let value = match choice {
                    1 => "light",
                    2 => "dark",
                    _ => "system",
                };
                changed(&ui, &state, "theme", &value.into(), false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_toggle_advanced(move |show| {
            if let Some(ui) = weak.upgrade() {
                state.borrow_mut().show_advanced = show;
                ui.set_show_advanced(show);
                refresh(&ui, &state.borrow());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_rule_text(move |key, value| {
            if let Some(ui) = weak.upgrade() {
                let spec = state
                    .borrow()
                    .specs
                    .iter()
                    .find(|s| s.key == key.as_str())
                    .cloned();
                let numeric = spec.as_ref().is_some_and(|s| s.kind == "number");
                let capacity = spec
                    .as_ref()
                    .is_some_and(|s| s.unit.as_deref() == Some("bytes"));
                let parsed = if numeric {
                    if let Ok(v) = parse_capacity(value.as_str(), capacity) {
                        serde_json::Value::from(v)
                    } else {
                        retain_invalid_number(&ui, &state, key.as_str(), value.as_str());
                        return;
                    }
                } else {
                    serde_json::Value::from(value.to_string())
                };
                if numeric {
                    state.borrow_mut().invalid_rule_inputs.remove(key.as_str());
                }
                if changed(&ui, &state, key.as_str(), &parsed, false) {
                    // 模型行里的 value 是重建列表（切分区、恢复默认规则）时的唯一来源，必须跟着更新，
                    // 否则重建后这一行会拿旧值覆盖刚改好的设置。
                    let canonical = parsed
                        .as_u64()
                        .map_or_else(|| value.to_string(), |v| v.to_string());
                    patch_rule_row(&ui, key.as_str(), |row| {
                        row.value = canonical.clone().into();
                    });
                    // 联动行的可见性也取决于自身的值（如 large_threshold_bytes 改回默认）：
                    // 与 on_rule_choice 同步可见性，增量插删不重建整表。
                    sync_rules(&ui, &state.borrow());
                } else if numeric {
                    retain_invalid_number(&ui, &state, key.as_str(), value.as_str());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_request_start(move || {
            if let Some(ui) = weak.upgrade() {
                // C-01：分析只读不改文件，无需破坏性确认；只有「执行计划」（kind=2）需要确认。
                start_task(&ui, &state, &out, false);
            }
        });
    }
    {
        // X-02：解压是一段确认。先弹确认框（数量先占位），后台清点压缩包后原位更新文案。
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_request_extract_start(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_busy() {
                    return;
                }
                if let Some(error) = invalid_number_error(&state.borrow(), Tool::Extract) {
                    show_error(&ui, error);
                    return;
                }
                if let Err(error) = state.borrow().config.validate_extract() {
                    show_error(&ui, error);
                    return;
                }
                let directory = PathBuf::from(ui.get_directory().as_str());
                if directory.as_os_str().is_empty() {
                    show_error(&ui, "请先选择需要解压的目录");
                    return;
                }
                ui.set_confirm_text(
                    extract_confirm_text("正在清点压缩包…", ui.get_directory().as_str()).into(),
                );
                ui.set_acknowledge(false);
                ui.set_confirm_kind(1);
                // X-02：数量未知（清点进行中）不得允许确认；清点返回后在事件侧解除。
                ui.set_confirm_pending(true);
                let out = out.clone();
                let config = {
                    let s = state.borrow();
                    s.config.clone()
                };
                // 清点请求代际：每次发起清点请求时递增，迟到的低代际事件整体丢弃（X-02）。
                let generation = {
                    let mut s = state.borrow_mut();
                    s.extract_generation += 1;
                    s.extract_generation
                };
                #[cfg(test)]
                let path_preflight = state.borrow().review_remediation.path_preflight.clone();
                let parent = tracing::Span::current();
                std::thread::spawn(move || {
                    let _parent = parent.enter();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        #[cfg(test)]
                        if let Some(probe) = path_preflight {
                            probe();
                        }
                        // 清点引擎统一执行原路径链接检查、规范化和范围校验。
                        extract_engine::count_archives(&directory, &config)
                    }));
                    let event = match result {
                        Ok(Ok(count)) => Event::ExtractCount(generation, Ok(count)),
                        Ok(Err(error)) => {
                            Event::ExtractCount(generation, Err(format!("{error:#}")))
                        }
                        Err(_) => Event::ExtractCount(
                            generation,
                            Err("清点压缩包时后台操作意外退出".into()),
                        ),
                    };
                    let _ = out.send(event);
                });
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_request_apply(move||{if let Some(ui)=weak.upgrade(){
            if !ui.get_ready(){return;}
            ui.set_confirm_text(format!("目标目录：{}\n执行“整理计划”中已勾选的操作；取消勾选的项目不会执行。\n\n{}\n\n{}",ui.get_directory(),state.borrow().config.destructive_warning(),ui.get_summary()).into());
            ui.set_acknowledge(false);ui.set_confirm_kind(2);
        }});
    }
    {
        // U-10：失败列表翻页。页游标存在才发起（翻页按钮只在对应页真实存在时启用）；
        // 采用「先加载、结果落地再提交页码」，失败时按钮保持与列表一致。
        let state = state.clone();
        ui.on_fail_page(move |direction| {
            let next = if direction < 0 {
                state.borrow().fail_page.saturating_sub(1)
            } else {
                state.borrow().fail_page + 1
            };
            let valid = {
                let s = state.borrow();
                s.extract_task.is_some() && next < s.fail_page_starts.len()
            };
            if valid {
                request_fail_page(&state, next);
            }
        });
    }
}

/// 任务生命周期与窗口控制回调（确认、暂停、取消、勾选、计划翻页/筛选、
/// 打开任务目录、自绘标题栏拖动、关窗确认）：生产装配与无头测试装配共用同一
/// 接线，保证经 invoke 层驱动的用例走与产品完全一致的处理器。
pub(super) fn wire_task_lifecycle(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    let (selection_writer, selection_queue) = mpsc::channel::<(PathBuf, i64, bool)>();
    let selection_out = out.clone();
    std::thread::spawn(move || {
        while let Ok((task, id, selected)) = selection_queue.recv() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Database::open_existing(&task).and_then(|db| db.set_selected(id, selected))
            }));
            let result =
                result.unwrap_or_else(|_| Err(anyhow::anyhow!("保存勾选时后台操作意外退出")));
            let saved = result.is_ok().then_some((id, selected));
            let _ = selection_out.send(Event::SelectionSaved(
                task,
                saved,
                result.err().map(|error| format!("{error:#}")),
            ));
        }
    });
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_confirmed(move |kind| {
            if let Some(ui) = weak.upgrade() {
                if kind == 5 {
                    if ui.get_confirm_kind() != 5 || !can_request_acp_stop(&ui, &state.borrow()) {
                        return;
                    }
                    state.borrow_mut().acp_explicit_stopped = true;
                    ui.set_acp_explicit_stopped(true);
                    request_acp_action(
                        &ui,
                        &state,
                        AcpCommand::Stop,
                        "正在退出模型服务：停止接新请求，取消并等待在途任务安全收尾…",
                    );
                    return;
                }
                if kind == 3 {
                    state.borrow_mut().close_after = true;
                    // 各 GUI 操作可并行（例如旧工具任务与字体初始化），关闭必须请求全部可取消阶段停止。
                    let (control, converter_cancel, converter_init_cancel, snap_init_cancel) = {
                        let state = state.borrow();
                        (
                            state.control.clone(),
                            state.convert_cancel.clone(),
                            state.convert_init_cancel.clone(),
                            state.snap_init_cancel.clone(),
                        )
                    };
                    let has_running = control.is_some()
                        || converter_cancel.is_some()
                        || converter_init_cancel.is_some()
                        || snap_init_cancel.is_some()
                        || ui.get_busy()
                        || ui.get_convert_initializing()
                        || ui.get_convert_preparing()
                        || ui.get_convert_runtime_saving()
                        || ui.get_snap_initializing();
                    if let Some(control) = control {
                        control.cancel();
                    }
                    if let Some(cancel) = converter_cancel {
                        cancel.store(true, Ordering::Release);
                    }
                    if let Some(cancel) = converter_init_cancel {
                        cancel.store(true, Ordering::Release);
                    }
                    if let Some(cancel) = snap_init_cancel {
                        cancel.store(true, Ordering::Release);
                    }
                    if !has_running {
                        let _ = slint::quit_event_loop();
                        return;
                    }
                    ui.set_status("取消任务中，完成当前安全操作后关闭".into());
                } else if kind == 4 {
                    // M-07/M-11：覆盖确认后重跑挂起的 MD 操作（写入前重新扫描/规划）。
                    confirm_md_override(&ui, &state, &out);
                } else if kind == 2 {
                    start_task(&ui, &state, &out, true);
                } else if ui.get_screen() == 2 {
                    start_extract(&ui, &state, &out);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_pause_task(move||{if let Some(ui)=weak.upgrade(){
            let s=state.borrow();
            // H-02/G-13：Git 与转 Markdown 任务不接入暂停检查点（Git 逐文件提交、转换
            // 顶层串行），暂停对它们无效；如实提示而不是翻转 paused 冻结状态显示。
            if s.runtime==RuntimeMode::Git||s.runtime==RuntimeMode::MarkdownConverter||s.control.is_none(){
                drop(s);
                ui.set_status("当前工具的任务不支持暂停".into());
                return;
            }
            if let Some(control)=&s.control{let pause=!control.is_paused();control.pause(pause);ui.set_paused(pause);
                ui.set_status(if pause{"已请求暂停；正在运行的压缩包在完成后暂停，Hash 和整理操作在分块/文件边界暂停"}else{"继续处理"}.into());}
        }});
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_cancel_task(move || {
            // 不同 GUI 阶段可并行；独立请求所有取消信号，截图服务本身不受影响。
            let (control, converter_cancel, converter_init_cancel, snap_init_cancel) = {
                let state = state.borrow();
                (
                    state.control.clone(),
                    state.convert_cancel.clone(),
                    state.convert_init_cancel.clone(),
                    state.snap_init_cancel.clone(),
                )
            };
            if let Some(control) = control {
                control.cancel();
            }
            if let Some(cancel) = converter_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(cancel) = converter_init_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(cancel) = snap_init_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_status("正在取消；当前操作完成后停止，不会继续后续操作".into());
            }
        });
    }
    {
        let state = state.clone();
        let out = out.clone();
        let selection_writer = selection_writer.clone();
        let weak = ui.as_weak();
        ui.on_plan_toggle(move |id, selected| {
            let task = state.borrow().task.clone();
            if let (Some(task), Ok(id)) = (task, id.parse::<i64>()) {
                if let Some(ui) = weak.upgrade() {
                    if ui.get_busy() || state.borrow().plan_recompute_inflight != 0 {
                        return;
                    }
                    // 勾选保存中 ready 暂降，避免在途勾选时误点执行；其他项仍可在 Slint 侧继续编辑。
                    ui.set_ready(false);
                    // C-11：勾选后即时显示对应状态（取消勾选→已取消勾选，重新勾选→待执行），
                    // 不等数据库事件回来；保存失败时由 SelectionSaved/重载恢复真值。
                    patch_plan_row(&ui, id, selected);
                }
                // 每次勾选立即作废旧页和旧快照，不能恢复更早的多行选择。
                let s = state.borrow();
                s.plan_load.page.fetch_add(1, Ordering::AcqRel);
                s.plan_load.state.fetch_add(1, Ordering::AcqRel);
                drop(s);
                state.borrow_mut().plan_page_pending = true;
                state.borrow_mut().pending_selection += 1;
                if let Err(error) = selection_writer.send((task, id, selected)) {
                    let (task, _, _) = error.0;
                    let _ = out.send(Event::SelectionSaved(
                        task,
                        None,
                        Some("保存勾选时后台操作意外退出".into()),
                    ));
                }
            }
        });
    }
    {
        let state = state.clone();
        let out = out.clone();
        let weak = ui.as_weak();
        ui.on_plan_page(move |direction| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // 目录原文随请求带走：页面自带的就绪快照同样按当前目录判定（见 classify_plan_snapshot）。
            let directory = ui.get_directory().to_string();
            let mut state = state.borrow_mut();
            let Some(task) = state.task.clone() else {
                return;
            };
            let next = if direction < 0 {
                state.page.saturating_sub(1)
            } else {
                state.page + 1
            };
            if let Some(start) = state.page_starts.get(next).copied() {
                state.plan_page_pending = true;
                ui.set_ready(false);
                // 先发起加载，成功事件再提交 page：失败时 page/按钮保持与列表一致，避免前进软锁。
                load_plan_state(
                    &out,
                    &state.plan_load,
                    task,
                    directory,
                    PlanQuery::page(start, next, state.plan_filter.clone(), false),
                );
            }
        });
    }
    {
        // 计划类型筛选：""=全部，否则 snake_case kind
        let state = state.clone();
        let out = out.clone();
        let weak = ui.as_weak();
        ui.on_filter_plan(move |kind| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // 目录原文随请求带走：页面自带的就绪快照同样按当前目录判定（见 classify_plan_snapshot）。
            let directory = ui.get_directory().to_string();
            let mut s = state.borrow_mut();
            let Some(task) = s.task.clone() else {
                return;
            };
            // Slint 胶囊已先改 plan-filter 显示；这里必须同步 Rust 状态，否则 PlanPage 事件会被拒。
            s.plan_filter = if kind.is_empty() {
                None
            } else {
                Some(kind.to_string())
            };
            s.page = 0;
            s.page_starts = vec![0];
            s.plan_page_pending = true;
            ui.set_ready(false);
            ui.set_plan_prev_enabled(false);
            ui.set_plan_next_enabled(false);
            ui.set_plan_page_label("加载中…".into());
            load_plan_state(
                &out,
                &s.plan_load,
                task,
                directory,
                PlanQuery::page(0, 0, s.plan_filter.clone(), false),
            );
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.on_open_report(move || {
            if let Some(path) = &state.borrow().task {
                if let Err(error) = open::that(path) {
                    if let Some(ui) = weak.upgrade() {
                        show_error(&ui, error);
                    }
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let drag = Rc::new(RefCell::new(None::<WindowDrag>));
        let anchor = drag.clone();
        ui.on_window_drag_start(move |x, y| {
            if let Some(ui) = weak.upgrade() {
                let window = ui.window();
                // 最大化状态下不在按下时立即还原：双击（无移动）要让 double-clicked 读到
                // 仍最大化，从而切换为还原；真正的拖动由 drag-move 的 restoring 分支还原。
                let restoring = window.is_maximized();
                let position = window.position();
                *anchor.borrow_mut() = Some(WindowDrag {
                    origin: (f64::from(position.x), f64::from(position.y)),
                    press: pointer_position().unwrap_or((f64::from(x), f64::from(y))),
                    restoring,
                });
            }
        });
        let weak = ui.as_weak();
        ui.on_window_drag_move(move |x, y| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut state = drag.borrow_mut();
            let Some(drag) = state.as_mut() else {
                return;
            };
            let pointer = pointer_position().unwrap_or((f64::from(x), f64::from(y)));
            if drag.restoring {
                // 按下时未还原（见 drag-start 注释）：真正的拖动在这里触发还原，
                // 等窗口离开最大化后再重新锚定，避免和系统还原位置互相覆盖。
                if ui.window().is_maximized() {
                    ui.window().set_maximized(false);
                    return;
                }
                let position = ui.window().position();
                drag.origin = (f64::from(position.x), f64::from(position.y));
                drag.press = pointer;
                drag.restoring = false;
                return;
            }
            // 拖动坐标为显示用途：饱和换算，越界值夹到 i32 边界。
            // [quality-baseline approved 2026-09-19] clamp 后截断数学上不可能；std 无 f64→i32 受检转换
            #[allow(clippy::cast_possible_truncation)]
            let drag_x = (drag.origin.0 + pointer.0 - drag.press.0)
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX))
                .round() as i32;
            // [quality-baseline approved 2026-09-19] 同 drag_x：clamp 后截断不可能，无受检转换可用
            #[allow(clippy::cast_possible_truncation)]
            let drag_y = (drag.origin.1 + pointer.1 - drag.press.1)
                .clamp(f64::from(i32::MIN), f64::from(i32::MAX))
                .round() as i32;
            ui.window()
                .set_position(slint::PhysicalPosition::new(drag_x, drag_y));
        });
    }
    {
        let state = state.clone();
        let weak = ui.as_weak();
        ui.window().on_close_requested(move||{
                if let Some(ui)=weak.upgrade(){if ui.get_busy() || ui.get_convert_initializing() || ui.get_convert_preparing() || ui.get_convert_runtime_saving() || ui.get_snap_initializing(){
                ui.set_confirm_text("任务或可选组件初始化仍在进行。确认后会请求取消，等待当前操作结束，再关闭窗口。已经完成的操作不会自动回滚。已经启动的截图服务不受影响。".into());
                ui.set_confirm_kind(3);ui.set_acknowledge(false);return slint::CloseRequestResponse::KeepWindowShown;
            }}
            if let Some(control)=&state.borrow().control{control.cancel();}
            if let Some(cancel)=&state.borrow().convert_cancel{cancel.store(true, Ordering::Release);}
            if let Some(cancel)=&state.borrow().convert_init_cancel{cancel.store(true, Ordering::Release);}
            // O-06/O-16：取消进行中的截图 OCR 初始化；已启动的服务独立存活，不随窗口关闭停止。
            if let Some(cancel)=&state.borrow().snap_init_cancel{cancel.store(true, Ordering::Release);}
            // Slint 1.17 的 CloseRequestResponse 只有 HideWindow / KeepWindowShown，
            // HideWindow 仅隐藏窗口、事件循环仍在跑；必须显式 quit 才能让进程真正退出。
            let _=slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        });
    }
}
