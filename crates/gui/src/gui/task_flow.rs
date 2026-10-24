//! 目录整理/递归解压任务启动与输入解析、目录选择对话框（自 gui.rs 拆分）。
use super::{
    count_selected_pending, extract_engine, invalid_number_error, organizer_engine,
    reset_fail_list, watch_extract_task, AppWindow, Arc, Context, Control, EngineTestOverrides,
    Event, EventSender, Instant, PathBuf, Rc, RefCell, Result, RuntimeMode, State, Tool,
    EXTRACT_TASK_WATCH_TIMEOUT,
};

/// 引擎侧后台线程公共骨架：body 产出终态事件；panic 以固定文案转
/// [`Event::Failed`]，引擎错误转 `{error:#}` 文本。发送失败静默放弃
/// （界面事件循环已退出时无处投递）。清点等事件形状不同的站点不适用本骨架。
fn spawn_event_worker(
    out: &EventSender,
    panic_text: &'static str,
    body: impl FnOnce() -> anyhow::Result<Event> + Send + 'static,
) {
    let out = out.clone();
    let parent = tracing::Span::current();
    std::thread::spawn(move || {
        let _parent = parent.enter();
        let span = jchtools_foundation::logging::operation_span("gui_worker", "engine_task");
        let _entered = span.enter();
        let started = Instant::now();
        tracing::debug!(event = "gui_worker_started", "后台任务开始");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        let event = match result {
            Ok(Ok(event)) => event,
            Ok(Err(error)) => Event::Failed(format!("{error:#}")),
            Err(_) => Event::Failed(panic_text.to_string()),
        };
        if matches!(&event, Event::Failed(_)) {
            tracing::error!(
                event = "gui_worker_failed",
                stage = "task",
                error_type = "worker_result",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "后台任务失败"
            );
        } else {
            tracing::debug!(
                event = "gui_worker_completed",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "后台任务结果已生成"
            );
        }
        if out.send(event).is_err() {
            tracing::warn!(
                event = "gui_worker_delivery_failed",
                stage = "event_send",
                error_type = "gui_closed",
                "后台结果无法投递到已关闭界面"
            );
        }
    });
}

/// 任务启动样板（五个 start_* 站点共用）：新建 Control、复位共享日志与关闭标记、
/// 记录起算时间与运行口径，并把忙碌态上屏（busy/paused/错误/日志四项清态）。
/// progress/note 复位与 notice 清理等并非五处共有，仍由各站点自留。
pub(super) fn begin_task_run(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    runtime: RuntimeMode,
) -> Arc<Control> {
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        s.logs.clear();
        // U-09：一次「停止并关闭」只作用于当次任务收尾，启动新任务必须重置；
        // U-03：实时耗时从本次任务起算。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = runtime;
    }
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_log_text("".into());
    control
}

/// 同一次 GUI 会话里可能先分析再执行：覆盖对象必须可重复使用，不能 take。
/// start_task 与 start_extract 的引擎注入克隆同源复用此处。
fn clone_engine_overrides(state: &Rc<RefCell<State>>) -> Option<EngineTestOverrides> {
    state
        .borrow()
        .engine_overrides
        .as_ref()
        .map(|o| EngineTestOverrides {
            state_dir: o.state_dir.clone(),
        })
}

pub(super) fn start_task(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    out: &EventSender,
    apply: bool,
) {
    if ui.get_busy() {
        return;
    }
    if state.borrow().pending_selection != 0 || state.borrow().plan_recompute_inflight != 0 {
        ui.set_error_text("计划勾选或依赖重算仍在进行，请稍后再试".into());
        return;
    }
    if apply && state.borrow().plan_recompute_failed {
        ui.set_error_text("计划依赖重算失败，请重新分析后再执行".into());
        return;
    }
    if apply && (state.borrow().plan_page_pending || !ui.get_ready()) {
        ui.set_error_text("请等待更新后的计划显示完成，再确认执行".into());
        return;
    }
    if let Some(error) = invalid_number_error(&state.borrow(), state.borrow().tool) {
        show_error(ui, error);
        return;
    }
    let (configuration, task, tool) = {
        let s = state.borrow();
        (s.config.clone(), s.task.clone(), s.tool)
    };
    let validation = match tool {
        Tool::Organizer => configuration.validate_organizer(),
        Tool::Extract => configuration.validate_extract(),
        Tool::Md
        | Tool::Git
        | Tool::MarkdownConverter
        | Tool::SnapOcr
        | Tool::AcpHttp
        | Tool::CodeAgent => Ok(()),
    };
    if let Err(error) = validation {
        ui.set_error_text(format!("{error:#}").into());
        return;
    }
    let directory = PathBuf::from(ui.get_directory().as_str());
    if apply && task.is_none() {
        ui.set_error_text("还没有可以执行的计划".into());
        return;
    }
    let control = begin_task_run(ui, state, RuntimeMode::Organizer);
    {
        let mut s = state.borrow_mut();
        s.plan_recompute_inflight = 0;
        s.plan_recompute_failed = false;
        s.plan_page_pending = true;
        s.readiness_status_pending.set(false);
        s.page = 0;
        s.page_starts = vec![0];
        s.applying = apply;
        s.extracting = false;
    }
    // 先把运行态上屏，再让 worker 做磁盘工作：busy/状态文案必须早于任何等待 I/O 的操作
    // 画出来（H-02：点确认后界面立即有反馈，项目计数不再拖住首帧）。
    ui.set_ready(false);
    // 分析（新任务）清掉上一任务的提示；执行是同一任务的收尾阶段，必须保留分析期产生的
    // 提示（H-06 Git 目录处置结果等），否则用户在整个执行阶段都看不到该说明。
    if !apply {
        ui.set_notice_text("".into());
    }
    ui.set_panel(2);
    ui.set_status(
        if apply {
            "正在执行已确认的整理计划"
        } else {
            "准备扫描与分析；分析阶段只读，不会改动任何文件"
        }
        .into(),
    );
    // 目录整理不调用解压、不询问解压冲突（C-01/H-03）：引擎只需任务控制与事件通道。
    // 引擎侧只接受裸通道（接口不变）；结果事件由本 worker 用 `out` 回传并唤醒事件循环。
    let context = Context {
        control: control.clone(),
        events: Some(out.channel()),
    };
    let overrides = clone_engine_overrides(state);
    #[cfg(test)]
    let path_preflight = state.borrow().review_remediation.path_preflight.clone();
    spawn_event_worker(
        out,
        "整理线程意外退出；未执行的步骤不会继续",
        move || {
            if apply {
                // apply 分支在函数入口已校验任务库存在；工作线程内不使用 unwrap，
                // 若状态被并发改动则按错误返回，交给事件循环统一呈现。
                let Some(task_path) = task.as_deref() else {
                    anyhow::bail!("内部错误：执行阶段任务库缺失");
                };
                // 执行分母按将实际执行的勾选数修正（U-03）：规划期 planned 是全量，用户可能已
                // 取消部分勾选。计数在 worker 线程完成并写入本次运行的 control——界面线程不为此
                // 打开任务库（H-02），计数失败则保留全量分母，不阻断执行（分母可能偏大但仍收敛）。
                if let Ok(n) = count_selected_pending(task_path) {
                    control.set_planned(n);
                }
                organizer_engine::apply(task_path, context)
                    .map(|result| Event::Done(result.directory, result.summary))
            } else {
                #[cfg(test)]
                if let Some(probe) = path_preflight {
                    probe();
                }
                if control.is_cancelled() {
                    anyhow::bail!("用户取消任务");
                }
                let prepare = match &overrides {
                    // 测试注入只需隔离任务库状态目录（回收站注入随 S-02 一并移除）。
                    Some(o) => organizer_engine::prepare_at(
                        &directory,
                        configuration,
                        context,
                        &o.state_dir,
                    ),
                    None => organizer_engine::prepare(&directory, configuration, context),
                };
                prepare.map(|result| Event::Ready(result.directory, result.summary))
            }
        },
    );
}

/// 「递归解压」一段式启动（X-02）：一段确认后连续执行到结束；无计划审核环节，
/// 结束事件 ExtractDone 只收尾摘要，不改 ready/has_task（那是目录整理两段式的状态）。
pub(super) fn start_extract(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    if let Some(error) = invalid_number_error(&state.borrow(), Tool::Extract) {
        show_error(ui, error);
        return;
    }
    let configuration = {
        let s = state.borrow();
        s.config.clone()
    };
    if let Err(error) = configuration.validate_extract() {
        ui.set_error_text(format!("{error:#}").into());
        return;
    }
    let directory = PathBuf::from(ui.get_directory().as_str());
    let control = begin_task_run(ui, state, RuntimeMode::Extract);
    {
        let mut s = state.borrow_mut();
        s.applying = false;
        s.extracting = true;
        s.planned = 0;
    }
    ui.set_ready(false);
    ui.set_notice_text("".into());
    ui.set_panel(1);
    ui.set_quarantined_count(0);
    // U-10：新解压任务开始——清空上一任务的失败列表与分页游标（在途分页结果一并作废）。
    reset_fail_list(ui, &mut state.borrow_mut());
    // X-05/S-02：成功删原包是破坏性行为，运行期状态栏必须如实说明删除条件与保留条件。
    ui.set_status(
        "正在解压；完整成功解出的原压缩包及分卷会永久删除（不经回收站，不可恢复），失败、未完全解开或取消的包保留，无法完全解开的包移入「解压失败」并记录原因".into(),
    );
    // X-05/X-06/H-07：解压只按成功条件删原包，不删除无关文件、不覆盖、不询问冲突；
    // 引擎侧只需任务控制与事件通道（裸通道接口不变，结果事件由本 worker 用 `out` 回传）。
    let context = Context {
        control: control.clone(),
        events: Some(out.channel()),
    };
    let overrides = clone_engine_overrides(state);
    let generation = state.borrow().extract_generation;
    let fail_sender = state.borrow().fail_sender.clone();
    spawn_event_worker(
        out,
        "解压线程意外退出；未执行的步骤不会继续",
        move || {
            // U-10/H-02：先在工作线程记录旧任务，再启动引擎；不能在引擎建库后
            // 才取快照，否则当前任务会被当成旧任务，运行中失败列表无法关联。
            let state_root = overrides.as_ref().map_or_else(
                || jchtools_file_core::config::state_dir().ok(),
                |overrides| Some(overrides.state_dir.clone()),
            );
            if let Some(state_root) = state_root {
                let tasks_root = state_root.join("tasks");
                let known = std::fs::read_dir(&tasks_root)
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|entry| entry.path())
                            .collect::<Vec<PathBuf>>()
                    })
                    .unwrap_or_default();
                std::thread::spawn(move || {
                    watch_extract_task(
                        &tasks_root,
                        &known,
                        generation,
                        &fail_sender,
                        EXTRACT_TASK_WATCH_TIMEOUT,
                    );
                });
            }
            let result = match overrides {
                Some(overrides) => extract_engine::extract_run_at(
                    &directory,
                    configuration,
                    context,
                    &overrides.state_dir,
                    None,
                ),
                None => extract_engine::extract_run(&directory, configuration, context),
            };
            result.map(|result| Event::ExtractDone(result.directory, result.summary))
        },
    );
}

/// 「开始解压」确认框文案（X-02，S-02 的破坏性告知）：数量由后台清点（大目录不阻塞界面），
/// 清点完成前先给占位文案，事件到达后原位更新。文案固定说明去向、完整成功即永久删除原包及
/// 分卷（不可恢复）、失败/未完全解开/取消时保留、已有文件保留、冲突只为新文件自动改名且保持
/// 扩展名、失败包去向与原因可查；不提供删除方式或冲突策略选项。
pub(super) fn extract_confirm_text(count_text: &str, directory: &str) -> String {
    format!(
        "目标目录：{directory}

将递归解压压缩包（含本次解出的嵌套压缩包），就地解到各包所在位置。
{count_text}
每个包完整解开（全部成员落盘）后，原压缩包及该包的分卷会永久删除，不经回收站、不可恢复；失败、未完全解开或取消的包一律保留。
已有文件一律保留；目标同名时只为新解出的文件自动改文件名（如 资料 (1).txt），扩展名与复合扩展名保持不变。
无法完全解开的包保留并移入「解压失败」子目录，原因记录在日志中；处理期间可随时取消。"
    )
}

pub(super) fn show_error(ui: &AppWindow, error: impl std::fmt::Display) {
    ui.set_error_text(error.to_string().into());
}

/// 目录选择起点策略；各页面传入自己的当前字段，不共享输入状态。
pub(super) fn directory_dialog_start(entered: &str) -> Option<PathBuf> {
    let entered = PathBuf::from(entered);
    entered.is_dir().then_some(entered)
}

pub(super) fn pick_directory(
    ui: &AppWindow,
    title: &str,
    entered: &str,
    apply: impl FnOnce(&AppWindow, PathBuf),
) {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(start) = directory_dialog_start(entered) {
        dialog = dialog.set_directory(start);
    }
    if let Some(path) = dialog.pick_folder() {
        apply(ui, path);
    }
}

/// M-08：解析拆分大小输入——正数十进制（可含小数），按 KB（1024）/ MB（1024×1024）
/// 换算后必须为整数字节；非法输入报字段错误，不自动截断或钳制。
pub(super) fn parse_size_bytes(text: &str, unit_index: i32) -> Result<u64, String> {
    let multiplier: u64 = if unit_index == 1 { 1024 * 1024 } else { 1024 };
    let trimmed = text.trim();
    let (whole, frac) = trimmed
        .split_once('.')
        .map_or((trimmed, ""), |(w, f)| (w, f));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() && frac.is_empty() {
        return Err("请输入大于 0 的数字（可用小数，换算后须为整数字节）".into());
    }
    if !digits(whole) || !digits(frac) {
        return Err("大小只接受非负十进制数字".into());
    }
    let whole_value: u64 = if whole.is_empty() {
        0
    } else {
        whole
            .parse()
            .map_err(|_| "数值超出允许的范围".to_string())?
    };
    let base = whole_value
        .checked_mul(multiplier)
        .ok_or_else(|| "数值超出允许的范围".to_string())?;
    if frac.is_empty() {
        if base == 0 {
            return Err("拆分大小必须大于 0".into());
        }
        return Ok(base);
    }
    // 去掉不影响数值的尾零；KB/MB 的整数字节小数最多分别需要 10/20 位。
    // 使用 u128 计算中间量，避免合法的 1 字节 MB 小数被 u64 的 10^20 拒绝。
    let frac = frac.trim_end_matches('0');
    if frac.len() > if unit_index == 1 { 20 } else { 10 } {
        return Err("换算后不是整数字节".into());
    }
    let frac_value: u128 = if frac.is_empty() {
        0
    } else {
        frac.parse().map_err(|_| "数值超出允许的范围".to_string())?
    };
    let digits_count = u32::try_from(frac.len()).unwrap_or(u32::MAX);
    let denom = 10u128
        .checked_pow(digits_count)
        .ok_or_else(|| "数值超出允许的范围".to_string())?;
    let scaled = frac_value * u128::from(multiplier);
    if !scaled.is_multiple_of(denom) {
        return Err("换算后不是整数字节（如 0.1 KB = 102.4 字节）".into());
    }
    let fractional_bytes =
        u64::try_from(scaled / denom).map_err(|_| "数值超出允许的范围".to_string())?;
    // 合法性判定看换算后的总量：整数部分为 0 的小数（如 0.5 MB）在汇总后才见真值，
    // 提前判 base==0 会把合法小数输入误拒（回归见 split_size_accepts_integral_fractions）。
    let total = base
        .checked_add(fractional_bytes)
        .ok_or_else(|| "数值超出允许的范围".to_string())?;
    if total == 0 {
        return Err("拆分大小必须大于 0".into());
    }
    Ok(total)
}

/// MD 整理合并输出文件名（M-07）：去掉首尾空白，不含路径分隔符，自动补 `.md` 后缀。
pub(super) fn normalize_output_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("请填写输出文件名".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return Err("输出文件名不能包含路径分隔符".into());
    }
    if jchtools_file_core::fsutil::validate_component(name).is_err() {
        return Err(format!("输出文件名「{name}」不是合法的 Windows 文件名"));
    }
    if name.to_ascii_lowercase().ends_with(".md") {
        Ok(name.to_owned())
    } else {
        Ok(format!("{name}.md"))
    }
}
