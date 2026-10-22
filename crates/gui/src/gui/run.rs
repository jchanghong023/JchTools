//! GUI 运行入口的装配主体：initial_state 与 run_gui（自 gui.rs 拆分）。
#[cfg(test)]
use super::ReviewRemediationHooks;
use super::{
    center_window, control, markdown_assets, mpsc, refresh, registry, settings_payload,
    start_acp_observer, sync_ready_after_directory, system_dark, wire_acp_http, wire_codeagent,
    wire_markdown_converter, wire_md_git, wire_settings, wire_snap_ocr, wire_sync,
    wire_task_lifecycle, AppWindow, Arc, AtomicU64, Cell, ComponentHandle, Config, ConvertCounts,
    Duration, EngineTestOverrides, Event, EventSender, HashMap, IdleDirectoryQueue, Instant,
    PathBuf, PlanLoadSync, Rc, RefCell, Result, RuleSpec, RuntimeMode, State, Tool, ToolRow,
    UiPump, VecDeque, VecModel, EVENT_LOOP_DRAIN,
};

/// 初始界面状态：全部规则规格 + 默认配置（仅会话内存，不落盘）。测试与 run() 共用。
pub(super) fn initial_state() -> Result<State> {
    let specs: Vec<RuleSpec> =
        serde_json::from_str(include_str!("../../../../resources/rules.json"))?;
    let (fail_sender, fail_receiver) = mpsc::channel();
    let (snap_sender, snap_receiver) = mpsc::channel();
    let (acp_sender, acp_receiver) = mpsc::channel();
    let (idle_directory_sender, idle_directory_receiver) = mpsc::channel();
    Ok(State {
        config: Config::default(),
        specs,
        invalid_rule_inputs: HashMap::new(),
        section: "去重".into(),
        task: None,
        control: None,
        logs: VecDeque::new(),
        convert_logs: VecDeque::new(),
        page: 0,
        page_starts: vec![0],
        started: Instant::now(),
        close_after: false,
        pending_selection: 0,
        plan_recompute_inflight: 0,
        plan_recompute_failed: false,
        plan_page_pending: false,
        applying: false,
        extracting: false,
        planned: 0,
        plan_filter: None,
        show_advanced: false,
        tool: Tool::Organizer,
        runtime: RuntimeMode::Organizer,
        md_pending: None,
        git_shared: None,
        convert_cancel: None,
        convert_current: String::new(),
        convert_init_cancel: None,
        convert_preparing: false,
        convert_pending_options: None,
        convert_counts: ConvertCounts::default(),
        convert_readiness_generation: 0,
        convert_readiness_missed: false,
        convert_preflight_generation: 0,
        settings_generation: 0,
        #[cfg(test)]
        settings_download: None,
        #[cfg(test)]
        review_remediation: ReviewRemediationHooks::default(),
        engine_overrides: None,
        plan_load: Arc::new(PlanLoadSync::default()),
        readiness_status_pending: Cell::new(false),
        idle_directory_generation: Cell::new(0),
        idle_directory_queue: Arc::new(std::sync::Mutex::new(IdleDirectoryQueue::default())),
        idle_directory_sender,
        idle_directory_receiver: RefCell::new(idle_directory_receiver),
        extract_task: None,
        fail_page: 0,
        fail_page_starts: vec![0],
        fail_gen: 0,
        fail_sender,
        fail_receiver: RefCell::new(fail_receiver),
        extract_generation: 0,
        snap_init_cancel: None,
        snap_generation: 0,
        snap_commands: None,
        snap_foreground_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        snap_sender,
        snap_receiver: RefCell::new(snap_receiver),
        acp_observer: None,
        acp_sender,
        acp_receiver: RefCell::new(acp_receiver),
        acp_observation_generation: Arc::new(AtomicU64::new(0)),
        acp_request_id: 0,
        acp_pending_request: None,
        acp_service_error: None,
        acp_draft_edited: false,
        acp_explicit_stopped: false,
        codeagent_observer: None,
    })
}

pub(super) fn run_gui(
    hook: impl FnOnce(&AppWindow) + 'static,
    overrides: Option<EngineTestOverrides>,
) -> Result<()> {
    let ui = AppWindow::new()?;
    ui.set_system_dark(system_dark());
    let state = Rc::new(RefCell::new(initial_state()?));
    state.borrow_mut().engine_overrides = overrides;
    let (channel, receiver) = control::event_channel();
    let out = EventSender::new(channel);
    let tools = registry::tools()
        .iter()
        .map(|tool| ToolRow {
            id: tool.id.into(),
            name: tool.name.into(),
            summary: tool.summary.into(),
        })
        .collect::<Vec<_>>();
    ui.set_tool_count(i32::try_from(registry::tools().len()).unwrap_or(i32::MAX));
    ui.set_tools(Rc::new(VecModel::from(tools)).into());
    refresh(&ui, &state.borrow());
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_choose_directory(move || {
            if let Some(ui) = weak.upgrade() {
                // 两个工具页共用此回调：标题按当前工具区分，不得把「整理」文案带到解压页。
                let mut dialog =
                    rfd::FileDialog::new().set_title(if state.borrow().tool == Tool::Extract {
                        "选择需要解压的目录"
                    } else {
                        "选择需要整理的目录"
                    });
                // 已经输入过目录时从这里开始，省掉用户重新导航一遍。
                let entered = PathBuf::from(ui.get_directory().as_str());
                if entered.is_dir() {
                    dialog = dialog.set_directory(&entered);
                }
                if let Some(path) = dialog.pick_folder() {
                    ui.set_directory(path.display().to_string().into());
                    // 立即撤销旧执行权，再由后台快照恢复仍匹配的计划。
                    sync_ready_after_directory(&ui, &state.borrow(), &out);
                }
            }
        });
    }
    wire_sync(&ui, &state, &out);
    wire_md_git(&ui, &state, &out);
    wire_markdown_converter(&ui, &state, &out);
    wire_snap_ocr(&ui, &state, &out);
    wire_settings(&ui, &state, &out);
    wire_acp_http(&ui, &state);
    wire_codeagent(&ui, &state);
    // 启动落在注册表第一个工具（P-02 顺序：递归解压在前）。必须在 wire_sync 之后调用：
    // 回调接线前的 invoke 是空调用，窗口会停在目录整理页。
    if std::env::args_os().any(|arg| arg == "--snap-ocr-settings") {
        ui.invoke_select_tool("snap-ocr".into());
    } else {
        ui.invoke_select_tool("recursive-extract".into());
    }
    wire_task_lifecycle(&ui, &state, &out);
    // 事件泵：100ms 轮询负责进度/耗时/主题刷新与兜底排空；结果事件由 worker 唤醒后
    // 立即排空（`EventSender` → `invoke_from_event_loop` → 事件循环线程上的排空钩子），
    // 因此结果上屏不受轮询周期限制（H-02）。绘制频率仍由这里的 100ms 上界约束。
    let pump = Rc::new(UiPump::new(receiver, state.clone(), out.clone()));
    let timer = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let pump = pump.clone();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(100),
            move || {
                if let Some(ui) = weak.upgrade() {
                    pump.run(&ui);
                }
            },
        );
    }
    // 唤醒钩子只在事件循环线程登记：worker 侧 post 的空闭包在这里执行，立即排空事件通道。
    // 事件循环退出后必须清掉钩子，否则本次运行的界面状态会被留在线程局部存储里。
    EVENT_LOOP_DRAIN.with(|slot| {
        let weak = ui.as_weak();
        let pump = pump.clone();
        *slot.borrow_mut() = Some(Rc::new(move || {
            if let Some(ui) = weak.upgrade() {
                pump.run(&ui);
            }
        }));
    });
    // First show the window. Rules stay in memory for this session only.
    ui.show()?;
    center_window(ui.window());
    // AH-06：窗口可交互后才投递自动连接；每次 GUI 打开仅投递一次。
    let acp_startup = state.clone();
    let acp_startup_timer = slint::Timer::default();
    acp_startup_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(1),
        move || {
            start_acp_observer(&acp_startup, true);
        },
    );
    let startup = out.clone();
    let settings_generation = state.borrow().settings_generation;
    std::thread::spawn(move || {
        // S1-01：启动快照用独立前缀，界面侧不把它当设置操作终态（不清标志、
        // 不消费 close_after），避免迟到快照在任务运行中提前退出或抹掉初始化状态。
        let mut migration_status = None;
        let migration = jchtools_xberg::xberg_runtime::resume_background().and_then(|()| {
            markdown_assets::migrate_bundled_runtime(
                &std::sync::atomic::AtomicBool::new(false),
                |message| {
                    let _ = startup.send(Event::Status(format!(
                        "SETTINGS_PROGRESS|{settings_generation}|{message}"
                    )));
                    migration_status = Some(message);
                },
            )
        });
        let result = settings_payload().and_then(|payload| {
            let status = migration.err().or(migration_status);
            if let Some(error) = status {
                let mut value: serde_json::Value = serde_json::from_str(&payload)
                    .map_err(|error| format!("无法读取设置状态：{error}"))?;
                value["status"] = serde_json::Value::String(error);
                return Ok(value.to_string());
            }
            Ok(payload)
        });
        let event = match result {
            Ok(payload) => {
                Event::Notice(format!("SETTINGS_LOADED|{settings_generation}|{payload}"))
            }
            Err(error) => Event::Error(format!("SETTINGS_LOAD_ERR|{settings_generation}|{error}")),
        };
        let _ = startup.send(event);
    });
    // 窗口刚映射时系统还会套用默认位置，稍后再居中一次，保证首屏就是居中的
    let centered = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(120), move || {
        if let Some(ui) = centered.upgrade() {
            center_window(ui.window());
        }
    });
    if std::env::args_os().any(|arg| arg == "--settings") {
        ui.invoke_navigation(7);
    }
    hook(&ui);
    let loop_result = slint::run_event_loop();
    // 退出后清掉唤醒钩子：它持有本次运行的 State 与事件通道，留着会跨运行泄漏。
    EVENT_LOOP_DRAIN.with(|slot| {
        let _ = slot.borrow_mut().take();
    });
    // AH-07：关闭 GUI 仅停止自己的只读观察，后台服务与在途操作继续完成。
    state.borrow_mut().acp_observer.take();
    // P-10：界面正常收场留痕（guard 在本函数结尾 drop 时刷盘）。
    tracing::info!(
        reason = loop_result
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default(),
        "图形界面退出"
    );
    loop_result?;
    Ok(())
}
