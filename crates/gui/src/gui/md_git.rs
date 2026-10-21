//! MD 整理与 Git 工具的运行流程与回调接线（自 gui.rs 拆分）。
use super::*;

/// MD 合并任务的公共运行段：扫描（排除输出自身）→（按 overwrite）合并（M-02~M-07）。
/// 供首次启动（overwrite=false）与覆盖确认后的重跑共用；文件集合在执行时重新扫描。
/// MD 合并/拆分共用的进度回调工厂：FileStarted 更新分母并上屏状态文案，
/// FileCompleted 更新分母与完成计数（U-03/U-12：完成计数只由 FileCompleted 驱动，
/// 单文件任务开局不得显示 1/1）。两处的计数口径只有这一份实现。
fn md_progress_sink<'a>(
    control: &'a Arc<Control>,
    out: &'a EventSender,
    status_text: impl Fn(usize, usize) -> String + 'a,
) -> impl Fn(md_tools::MdProgress) -> anyhow::Result<()> + 'a {
    let out = out.clone();
    move |event| {
        match event {
            md_tools::MdProgress::FileStarted(index, total) => {
                control.set_planned(u64::try_from(total).unwrap_or(0));
                let _ = out.send(Event::Status(status_text(index, total)));
            }
            md_tools::MdProgress::FileCompleted(done, total) => {
                control.set_planned(u64::try_from(total).unwrap_or(0));
                control
                    .completed
                    .store(u64::try_from(done).unwrap_or(0), Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

pub(super) fn md_merge_run(
    root: &Path,
    recursive: bool,
    output: &Path,
    overwrite: bool,
    control: &Arc<Control>,
    out: &EventSender,
) -> Result<String, String> {
    let entries = md_tools::scan_markdown(root, recursive, Some(output), control)
        .map_err(|error| format!("{error:#}"))?;
    let _ = out.send(Event::Log(format!(
        "扫描完成：找到 {} 个 .md 文件（{}），输出：{}",
        entries.len(),
        if recursive { "递归" } else { "不递归" },
        output.display()
    )));
    if entries.is_empty() {
        return Ok(format!(
            "所选范围内没有 .md 文件，未生成输出：{}",
            root.display()
        ));
    }
    // M-07：输出已存在 → 交由界面确认（MD_CONFLICT 前缀触发确认框），确认后重跑覆盖
    if !overwrite && md_tools::output_entry_exists(output) {
        return Err(format!(
            "MD_CONFLICT 输出文件已存在：{}；确认后将覆盖原内容（不可恢复），返回检查则不做任何写入",
            output.display()
        ));
    }
    let stats = md_tools::merge_markdown_with_events(
        &entries,
        output,
        overwrite,
        control,
        &md_progress_sink(control, out, |index, total| {
            format!("合并中：{index} / {total} 个文件")
        }),
    )
    .map_err(|error| format!("{error:#}"))?;
    Ok(format!(
        "合并完成：{} 个文件按创建时间顺序写入 {}",
        stats.files,
        output.display()
    ))
}

/// MD 拆分任务的公共运行段：规划（UTF-8 安全边界）→ 冲突检测 →（按 overwrite）写出
/// （M-08~M-11）。供首次启动与覆盖确认后的重跑共用。
pub(super) fn md_split_run(
    input: &Path,
    limit: u64,
    out_dir: &Path,
    overwrite: bool,
    control: &Arc<Control>,
    out: &EventSender,
) -> Result<String, String> {
    let plan =
        md_tools::plan_splits(input, limit, control).map_err(|error| format!("{error:#}"))?;
    if plan.bounds.is_empty() {
        return Ok(format!(
            "输入文件为空（0 字节），没有可拆分的内容：{}",
            input.display()
        ));
    }
    let names = md_tools::split_names(
        &input
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        plan.bounds.len(),
    );
    // M-11：仅在未确认覆盖时前置检测冲突；确认覆盖（overwrite=true）后直接写出，
    // 否则旧分片仍存在会让确认框无限复弹、覆盖永远无法完成（回归见
    // md_split_existing_output_requires_confirmation_then_overwrites）。
    if !overwrite {
        let conflicts = md_tools::conflicting_outputs(out_dir, &names);
        if !conflicts.is_empty() {
            let sample: Vec<String> = conflicts
                .iter()
                .take(3)
                .map(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            return Err(format!(
                "MD_CONFLICT {} 个同名分片已存在于输出目录（如 {}）",
                conflicts.len(),
                sample.join("、")
            ));
        }
    }
    md_tools::run_split_with_events(
        input,
        &plan,
        out_dir,
        overwrite,
        control,
        &md_progress_sink(control, out, |index, total| {
            format!("拆分中：{index} / {total} 片")
        }),
    )
    .map_err(|error| format!("{error:#}"))?;
    Ok(format!(
        "拆分完成：{} 片写入 {}（每片不超过 {} 字节）",
        plan.bounds.len(),
        out_dir.display(),
        limit
    ))
}

/// 后台线程统一收尾：Err 前缀 MD_CONFLICT 表示需要覆盖确认（M-07/M-11），
/// 由事件泵转为确认框；其余 Err 按失败收尾。
fn spawn_md_worker(
    out: EventSender,
    body: impl FnOnce() -> Result<String, String> + Send + 'static,
) {
    let parent = tracing::Span::current();
    std::thread::spawn(move || {
        let _parent = parent.enter();
        let span = jchtools_foundation::logging::operation_span("gui_worker", "md_task");
        let _entered = span.enter();
        let started = Instant::now();
        tracing::debug!(
            component = "gui_worker",
            event = "gui_md_worker_started",
            "MD 后台任务开始"
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body))
            .unwrap_or_else(|_| Err("MD 整理后台操作意外退出；未完成的输出请手动检查".to_string()));
        let event = match result {
            Ok(text) => Event::MdDone(text),
            Err(text) if text.starts_with("MD_CONFLICT ") => {
                Event::MdNeedsConfirm(text.trim_start_matches("MD_CONFLICT ").to_string())
            }
            Err(text) => Event::Failed(text),
        };
        match &event {
            Event::Failed(_) => tracing::error!(
                component = "gui_worker",
                event = "gui_worker_failed",
                operation = "md_task",
                stage = "task",
                error_type = "worker_result",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "MD 后台任务失败"
            ),
            Event::MdNeedsConfirm(_) => tracing::debug!(
                component = "gui_worker",
                event = "gui_md_worker_completed",
                outcome = "confirmation_required",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "MD 后台任务等待覆盖确认"
            ),
            _ => tracing::debug!(
                component = "gui_worker",
                event = "gui_md_worker_completed",
                outcome = "completed",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "MD 后台任务结果已生成"
            ),
        }
        if out.send(event).is_err() {
            tracing::warn!(
                component = "gui_worker",
                event = "gui_worker_delivery_failed",
                operation = "md_task",
                stage = "event_send",
                error_type = "gui_closed",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "MD 后台结果无法投递到已关闭界面"
            );
        }
    });
}

/// 启动 MD 合并任务（M-02~M-07）：纯参数校验后转后台扫描与合并。
fn start_md_merge(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    let root = PathBuf::from(ui.get_md_input_dir().to_string());
    if root.as_os_str().is_empty() {
        show_error(ui, "请先选择或输入要合并的目录");
        return;
    }
    let name = match normalize_output_name(ui.get_md_output_name().as_str()) {
        Ok(name) => name,
        Err(text) => {
            show_error(ui, text);
            return;
        }
    };
    let out_dir = PathBuf::from(ui.get_md_output_dir().to_string());
    if out_dir.as_os_str().is_empty() {
        show_error(ui, "请填写输出目录（可用「选择目录…」指定）");
        return;
    }
    let recursive = ui.get_md_recursive();
    let output = out_dir.join(&name);
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        s.logs.clear();
        // U-09：一次「停止并关闭」只作用于当次任务收尾，启动新任务必须重置；
        // U-03：实时耗时从本次任务起算。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = RuntimeMode::Md;
        s.md_pending = Some(MdPending::Merge {
            root: root.clone(),
            recursive,
            output: output.clone(),
        });
    }
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_progress(-1.0);
    ui.set_progress_note("".into());
    ui.set_log_text("".into());
    ui.set_status("正在扫描并合并 Markdown 文件；原文件不会被改动".into());
    let worker_out = out.clone();
    #[cfg(test)]
    let path_preflight = state.borrow().review_remediation.path_preflight.clone();
    spawn_md_worker(worker_out.clone(), move || {
        #[cfg(test)]
        if let Some(probe) = path_preflight {
            probe();
        }
        if control.is_cancelled() {
            return Ok("MD 合并已取消；未写入输出".into());
        }
        md_merge_run(&root, recursive, &output, false, &control, &worker_out)
    });
}

/// 启动 MD 拆分任务（M-08~M-11）。
fn start_md_split(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    let input = PathBuf::from(ui.get_md_split_file().to_string());
    if input.as_os_str().is_empty() {
        show_error(ui, "请先选择要拆分的 .md 文件");
        return;
    }
    if !input
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
    {
        show_error(ui, "MD 拆分只接受 .md 文件");
        return;
    }
    let limit = match parse_size_bytes(ui.get_md_split_size().as_str(), ui.get_md_split_unit()) {
        Ok(limit) => limit,
        Err(text) => {
            show_error(ui, text);
            return;
        }
    };
    let out_dir = PathBuf::from(ui.get_md_split_dir().to_string());
    if out_dir.as_os_str().is_empty() {
        show_error(ui, "请填写输出目录（可用「选择目录…」指定）");
        return;
    }
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        s.logs.clear();
        // U-09：一次「停止并关闭」只作用于当次任务收尾，启动新任务必须重置；
        // U-03：实时耗时从本次任务起算。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = RuntimeMode::Md;
        s.md_pending = Some(MdPending::Split {
            input: input.clone(),
            limit,
            out_dir: out_dir.clone(),
        });
    }
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_progress(-1.0);
    ui.set_progress_note("".into());
    ui.set_log_text("".into());
    ui.set_status(format!("正在规划拆分边界（单文件上限 {limit} 字节）").into());
    let worker_out = out.clone();
    #[cfg(test)]
    let path_preflight = state.borrow().review_remediation.path_preflight.clone();
    spawn_md_worker(worker_out.clone(), move || {
        #[cfg(test)]
        if let Some(probe) = path_preflight {
            probe();
        }
        if control.is_cancelled() {
            return Ok("MD 拆分已取消；未写入输出".into());
        }
        md_split_run(&input, limit, &out_dir, false, &control, &worker_out)
    });
}

/// 确认框 kind=4 的处理：消费挂起的 MD 操作并以覆盖模式重跑（M-07/M-11）。
/// 生产由 on_confirmed(4) 调用；无头测试直接调用同一函数走完全相同的路径。
pub(super) fn confirm_md_override(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    let pending = state.borrow_mut().md_pending.take();
    if let Some(pending) = pending {
        ui.set_busy(true);
        ui.set_paused(false);
        ui.set_acknowledge(false);
        ui.set_status("已确认覆盖：正在重新扫描并写入输出".into());
        resume_md_after_confirm(pending, state, out);
    }
}

/// 覆盖确认后的 MD 重跑（M-07/M-11）：重新扫描/规划并写入（overwrite=true）。
fn resume_md_after_confirm(pending: MdPending, state: &Rc<RefCell<State>>, out: &EventSender) {
    let control = Arc::new(Control::default());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        // 覆盖确认重跑是一次新的执行段：耗时从重跑起算（U-03）；close_after 理论上
        // 必为 false（为 true 时 MdNeedsConfirm 已直接退出），此处一并重置保持不变量。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = RuntimeMode::Md;
    }
    let worker_out = out.clone();
    spawn_md_worker(worker_out.clone(), move || match pending {
        MdPending::Merge {
            root,
            recursive,
            output,
        } => md_merge_run(&root, recursive, &output, true, &control, &worker_out),
        MdPending::Split {
            input,
            limit,
            out_dir,
        } => md_split_run(&input, limit, &out_dir, true, &control, &worker_out),
    });
}

/// 启动 Git 逐文件提交并推送任务（G-02~G-16）。
fn start_git(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    if ui.get_busy() {
        return;
    }
    let repo = PathBuf::from(ui.get_git_repo().to_string());
    if repo.as_os_str().is_empty() {
        show_error(ui, "请先选择或输入 Git 项目目录");
        return;
    }
    let control = Arc::new(Control::default());
    let shared = Arc::new(GitShared::new());
    {
        let mut s = state.borrow_mut();
        s.control = Some(Arc::clone(&control));
        s.logs.clear();
        // U-09：一次「停止并关闭」只作用于当次任务收尾，启动新任务必须重置；
        // U-03：实时耗时从本次任务起算。
        s.close_after = false;
        s.started = Instant::now();
        s.runtime = RuntimeMode::Git;
        s.git_shared = Some(Arc::clone(&shared));
        s.md_pending = None;
    }
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
    ui.set_progress(-1.0);
    ui.set_progress_note("".into());
    ui.set_log_text("".into());
    ui.set_git_state("检查仓库".into());
    // 新任务预检期间不显示上一仓库的分支、文件与重试进度。
    ui.set_git_branch("".into());
    ui.set_git_upstream("".into());
    ui.set_git_current("".into());
    ui.set_git_stage("扫描".into());
    ui.set_git_retry(0);
    ui.set_git_retry_wait(0);
    ui.set_git_total(0);
    ui.set_git_done(0);
    ui.set_status("正在验证仓库（分支、upstream 与仓库状态）…".into());
    let worker_out = out.clone();
    let parent = tracing::Span::current();
    #[cfg(test)]
    let execution_hook = state.borrow().review_remediation.git.clone();
    #[cfg(test)]
    let path_preflight = state.borrow().review_remediation.path_preflight.clone();
    std::thread::spawn(move || {
        let _parent = parent.enter();
        let span = jchtools_foundation::logging::operation_span("gui_worker", "git_task");
        let _entered = span.enter();
        let mut preflight_failed = false;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            if let Some(probe) = path_preflight {
                probe();
            }
            if control.is_cancelled() {
                return "Git 任务已取消".into();
            }
            if !repo.is_dir() {
                preflight_failed = true;
                for field in [&shared.state, &shared.stage] {
                    let mut value = field
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *value = "失败".into();
                }
                return format!("Git 目录不存在或无法访问：{}", repo.display());
            }
            #[cfg(test)]
            if let Some(hook) = execution_hook {
                hook();
            }
            let git = match git_tools::find_git() {
                Ok(git) => git,
                Err(error) => {
                    tracing::error!(
                        event = "git_task_start_failed",
                        stage = "find_git",
                        error_type = "git_unavailable",
                        "Git 程序不可用"
                    );
                    if let Ok(mut state) = shared.state.lock() {
                        state.clear();
                        state.push_str("失败");
                    }
                    if let Ok(mut stage) = shared.stage.lock() {
                        stage.clear();
                        stage.push_str("失败");
                    }
                    return format!("无法启动 Git 任务：{error:#}");
                }
            };
            let log_out = worker_out.clone();
            let status_out = worker_out.clone();
            git_tools::run(
                &git,
                &repo,
                &control,
                &shared,
                &|text: &str| {
                    let _ = log_out.send(Event::Log(text.to_string()));
                },
                &|text: &str| {
                    let _ = status_out.send(Event::Status(text.to_string()));
                },
                BACKOFF_UNIT,
            )
        }));
        let final_text = result.unwrap_or_else(|_| {
            // panic payload 可能含凭据；只交付固定文案与本任务的专属终态。
            for field in [&shared.state, &shared.stage] {
                let mut value = field
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                value.clear();
                value.push_str("失败");
                drop(value);
                field.clear_poison();
            }
            "Git 后台任务意外退出；请检查仓库后重试".to_string()
        });
        let event = if preflight_failed {
            Event::Failed(final_text)
        } else {
            Event::GitDone(final_text)
        };
        let _ = worker_out.send(event);
    });
}

/// 允许测试注入状态目录；生产 GUI 走 `run()` / `run_with_pre_loop_hook`。
/// MD 整理与 Git 工具的回调装配（M/G 分区；R-01 参数直接在页面提供）。
/// 生产 `run_with_engine_overrides` 与无头测试 `with_gui` 共用：测试直接以真实回调驱动。
pub(super) fn wire_md_git(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    {
        let weak = ui.as_weak();
        ui.on_md_select_subpage(move |page| {
            if let Some(ui) = weak.upgrade() {
                if page == 0 || page == 1 {
                    ui.set_md_subpage(page);
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_choose_input(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择要合并的目录",
                    ui.get_md_input_dir().as_str(),
                    |ui, path| {
                        ui.set_md_input_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_choose_output(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择合并输出目录",
                    ui.get_md_output_dir().as_str(),
                    |ui, path| {
                        ui.set_md_output_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_split_choose_dir(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择拆分输出目录",
                    ui.get_md_split_dir().as_str(),
                    |ui, path| {
                        ui.set_md_split_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_md_split_choose_file(move || {
            if let Some(ui) = weak.upgrade() {
                // U-07：文件选择从最近输入目录开始。
                let mut dialog = rfd::FileDialog::new()
                    .set_title("选择要拆分的 .md 文件")
                    .add_filter("Markdown", &["md"]);
                let entered = PathBuf::from(ui.get_md_split_file().to_string());
                let start = if entered.is_file() {
                    entered.parent().map(Path::to_path_buf)
                } else if entered.is_dir() {
                    Some(entered.clone())
                } else {
                    let fallback = PathBuf::from(ui.get_md_input_dir().to_string());
                    fallback.is_dir().then_some(fallback)
                };
                if let Some(start) = start {
                    dialog = dialog.set_directory(start);
                }
                if let Some(path) = dialog.pick_file() {
                    ui.set_md_split_file(path.display().to_string().into());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_git_choose_repo(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择 Git 项目目录",
                    ui.get_git_repo().as_str(),
                    |ui, path| {
                        ui.set_git_repo(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_md_merge_start(move || {
            if let Some(ui) = weak.upgrade() {
                start_md_merge(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_md_split_start(move || {
            if let Some(ui) = weak.upgrade() {
                start_md_split(&ui, &state, &out);
            }
        });
    }
    // M-08：字段错误和启动校验复用同一精确字节换算，编辑大小/单位时立即更新。
    ui.on_md_split_size_error(|value, unit| {
        parse_size_bytes(value.as_str(), unit)
            .err()
            .unwrap_or_default()
            .into()
    });
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_git_start(move || {
            if let Some(ui) = weak.upgrade() {
                start_git(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_git_stop(move || {
            // G-15：停止 = 不再处理新文件、不启动下一次 retry；当前 git 命令自然结束。
            // 其他任务运行中跨页点击时同样必须真正取消：control 为 None 时回落
            // 转换/初始化的取消原子量（与 on_confirmed(3)、on_cancel_task 同构，H-02）。
            let control = state.borrow().control.clone();
            let converter_cancel = state.borrow().convert_cancel.clone();
            let converter_init_cancel = state.borrow().convert_init_cancel.clone();
            let stopping_converter = control.is_none();
            let stopping_shared_non_git =
                control.is_some() && state.borrow().runtime != RuntimeMode::Git;
            if let Some(control) = control {
                control.cancel();
            } else if let Some(cancel) = converter_cancel {
                cancel.store(true, Ordering::Release);
            } else if let Some(cancel) = converter_init_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_status(if stopping_converter {
                    // 回落取消的是转 Markdown 任务：文案如实，不写 Git 专属描述。
                    "正在停止转 Markdown；正在取消当前文件".into()
                } else if stopping_shared_non_git {
                    // 取消的是其他工具的共享任务（整理/解压/MD）：通用取消口径。
                    "正在取消；当前操作完成后停止，不会继续后续操作".into()
                } else {
                    "正在停止：等待当前 git 命令结束后不再继续；已成功推送的文件保持成功".into()
                });
            }
        });
    }
}
