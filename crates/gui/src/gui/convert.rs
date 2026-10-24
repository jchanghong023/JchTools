//! 转 Markdown 页：组件就绪检查、预检与转换启动接线（自 gui.rs 拆分）。
use super::{
    markdown, pick_directory, AppWindow, Arc, ComponentHandle, ConvertCounts, Duration, Event,
    EventSender, FormatGroup, Instant, Ordering, PathBuf, Rc, RefCell, Result, RuntimeMode, State,
};

pub(super) fn wire_markdown_converter(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    out: &EventSender,
) {
    {
        let weak = ui.as_weak();
        ui.on_convert_timeout_edited(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_convert_timeout_error(
                    parse_validate_timeout_secs(ui.get_convert_timeout_secs().as_str())
                        .err()
                        .unwrap_or_default()
                        .into(),
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_convert_choose_input(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择待转换目录",
                    ui.get_convert_input_dir().as_str(),
                    |ui, path| {
                        ui.set_convert_input_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_convert_choose_output(move || {
            if let Some(ui) = weak.upgrade() {
                pick_directory(
                    &ui,
                    "选择 Markdown 输出目录",
                    ui.get_convert_output_dir().as_str(),
                    |ui, path| {
                        ui.set_convert_output_dir(path.display().to_string().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        let out = out.clone();
        ui.on_convert_start(move || {
            if let Some(ui) = weak.upgrade() {
                start_markdown_conversion(&ui, &state, &out);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let state = state.clone();
        ui.on_convert_stop(move || {
            // 与 on_confirmed(3)/on_cancel_task/on_git_stop 同构：其他任务运行中
            // 跨页点击时必须真正取消共享任务，control 命中即取消之（H-02）。
            let control = state.borrow().control.clone();
            let converter_cancel = state.borrow().convert_cancel.clone();
            let stopping_shared_task = control.is_some();
            if let Some(control) = control {
                control.cancel();
            } else if let Some(cancel) = converter_cancel {
                cancel.store(true, Ordering::Release);
            }
            if let Some(ui) = weak.upgrade() {
                if stopping_shared_task {
                    // 取消的是其他工具的共享任务：文案如实按通用取消口径。
                    ui.set_status("正在取消；当前操作完成后停止，不会继续后续操作".into());
                    return;
                }
                ui.set_convert_status(
                    if ui.get_convert_preparing() {
                        "正在取消启动前检查…"
                    } else {
                        "正在停止；正在取消当前文件"
                    }
                    .into(),
                );
                ui.set_status(
                    if ui.get_convert_preparing() {
                        "正在取消转 Markdown 启动前检查"
                    } else {
                        "正在停止转 Markdown；正在取消当前文件"
                    }
                    .into(),
                );
            }
        });
    }
}

fn selected_conversion_groups(ui: &AppWindow) -> Vec<FormatGroup> {
    let mut groups = Vec::new();
    if ui.get_convert_pdf() {
        groups.push(FormatGroup::Pdf);
    }
    if ui.get_convert_office() {
        groups.push(FormatGroup::Office);
    }
    if ui.get_convert_images() {
        groups.push(FormatGroup::Images);
    }
    if ui.get_convert_media() {
        groups.push(FormatGroup::Media);
    }
    if ui.get_convert_other() {
        groups.push(FormatGroup::Other);
    }
    groups
}

/// 异步检查已安装的可选组件；只读校验可能很慢，绝不阻塞 Slint 事件线程。
pub(super) fn start_markdown_readiness(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    out: &EventSender,
) {
    // 刻意收窄：只看转 Markdown 自身的忙态，不含 snap_initializing（截图初始化
    // 不阻塞就绪检查）；与 gui_activity_in_flight 的五 getter 合取不同是故意的。
    if ui.get_busy()
        || ui.get_convert_initializing()
        || ui.get_convert_preparing()
        || ui.get_convert_runtime_saving()
    {
        return;
    }
    // 即使新选择无需后台检查（例如全部不选），也必须使旧结果失效。
    let generation = {
        let mut s = state.borrow_mut();
        s.convert_readiness_missed = false;
        s.convert_readiness_generation = s.convert_readiness_generation.wrapping_add(1);
        s.convert_readiness_generation
    };
    if ui.get_convert_runtime_dir().trim().is_empty() {
        ui.set_convert_runtime_confirmed(false);
        ui.set_convert_ready(false);
        ui.set_convert_status("请前往设置检查随包基础资产；音频转写资产需主动下载".into());
        return;
    }
    if !ui.get_convert_runtime_confirmed() {
        ui.set_convert_ready(false);
        ui.set_convert_status("请前往设置检查已安装组件".into());
        return;
    }
    let groups = selected_conversion_groups(ui);
    if groups.is_empty() {
        ui.set_convert_ready(false);
        ui.set_convert_status("至少选择一种转换类型".into());
        return;
    }
    ui.set_convert_ready(false);
    ui.set_convert_status("正在检查已安装组件…".into());
    let out = out.clone();
    #[cfg(test)]
    let readiness_hook = state.borrow().review_remediation.readiness.clone();
    std::thread::spawn(move || {
        // T-05/T-06：按当前勾选的场景检查必需组件，缺失时开始按钮保持关闭。
        let check = || markdown::readiness_for_groups(&groups);
        #[cfg(test)]
        let check = || match readiness_hook {
            Some(hook) => hook(),
            None => check(),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(check))
            .unwrap_or_else(|_| Err("转 Markdown 组件检查意外退出，请重试".to_string()));
        let (ok, message) = match result {
            Ok(()) => (true, "已安装组件就绪，可离线使用".to_string()),
            Err(error) => (false, error),
        };
        let _ = out.send(Event::Status(format!(
            "CONVERTER_READINESS|{generation}|{}|{message}",
            i32::from(ok)
        )));
    });
}

// S3-01：转换页的「初始化可选组件」入口已移除（XB-19：初始化集中在设置页，
// markdown_assets::initialize 只按文档成员校验，纯媒体用户点击会误报失败）。
// start_markdown_initialize 及其回调接线随之删除；组件状态统一由按场景的
// start_markdown_readiness 呈现，修复入口经页内「前往设置」进入设置页。

fn parse_validate_timeout_secs(value: &str) -> Result<u64, String> {
    let timeout_secs = value
        .parse::<u64>()
        .map_err(|_| "单文件超时必须是正整数秒数".to_string())?;
    if timeout_secs == 0 {
        return Err("单文件超时必须大于 0".into());
    }
    if Instant::now()
        .checked_add(Duration::from_secs(timeout_secs))
        .is_none()
    {
        return Err("单文件超时超出系统可表示范围".into());
    }
    Ok(timeout_secs)
}

fn start_markdown_conversion(ui: &AppWindow, state: &Rc<RefCell<State>>, out: &EventSender) {
    // 刻意收窄：只看转 Markdown 自身的忙态与就绪标志，不含 snap_initializing；
    // 与 gui_activity_in_flight 的五 getter 合取不同是故意的。
    if ui.get_busy()
        || ui.get_convert_preparing()
        || ui.get_convert_initializing()
        || ui.get_convert_runtime_saving()
        || !ui.get_convert_ready()
    {
        return;
    }
    let input_dir = PathBuf::from(ui.get_convert_input_dir().as_str());
    let output_dir = PathBuf::from(ui.get_convert_output_dir().as_str());
    let timeout_secs = match parse_validate_timeout_secs(ui.get_convert_timeout_secs().as_str()) {
        Ok(timeout_secs) => {
            ui.set_convert_timeout_error("".into());
            timeout_secs
        }
        Err(error) => {
            ui.set_convert_timeout_error(error.clone().into());
            ui.set_error_text(error.into());
            return;
        }
    };
    let groups = selected_conversion_groups(ui);
    if groups.is_empty() {
        ui.set_error_text("至少选择一种转换类型".into());
        return;
    }
    let options = markdown::Options {
        input_dir,
        output_dir,
        flat: ui.get_convert_flat(),
        groups,
        timeout_secs,
    };
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let generation = {
        let mut s = state.borrow_mut();
        s.convert_cancel = Some(cancel.clone());
        s.convert_preparing = true;
        s.convert_pending_options = Some(options.clone());
        // S1-05：新任务不继承上一任务错过的就绪补查标记。
        s.convert_readiness_missed = false;
        s.convert_preflight_generation = s.convert_preflight_generation.wrapping_add(1);
        s.convert_preflight_generation
    };
    ui.set_ready(false);
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
    ui.set_convert_preparing(true);
    ui.set_convert_status("正在检查所选组件和输入输出目录…".into());
    let out = out.clone();
    #[cfg(test)]
    let preflight_hook = state.borrow().review_remediation.preflight.clone();
    std::thread::spawn(move || {
        let check = || {
            if cancel.load(Ordering::Acquire) {
                return Err("用户取消检查".to_string());
            }
            let input = std::fs::canonicalize(&options.input_dir)
                .map_err(|error| format!("无法访问输入目录：{error}"))?;
            if !input.is_dir() {
                return Err("输入路径不是目录".to_string());
            }
            let output = std::fs::canonicalize(&options.output_dir)
                .map_err(|error| format!("无法访问输出目录：{error}"))?;
            if !output.is_dir() {
                return Err("输出路径不是目录".to_string());
            }
            if input == output {
                return Err("输入与输出目录不能相同".to_string());
            }
            markdown::readiness_for_groups(&options.groups)?;
            if cancel.load(Ordering::Acquire) {
                return Err("用户取消检查".to_string());
            }
            Ok(())
        };
        #[cfg(test)]
        let check = || match preflight_hook {
            Some(hook) => hook(),
            None => check(),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(check))
            .unwrap_or_else(|_| Err("转 Markdown 开始前检查意外退出，请重试".to_string()));
        let ok = result.is_ok();
        let message = result.err().unwrap_or_default();
        let _ = out.send(Event::Status(format!(
            "CONVERTER_PREFLIGHT|{generation}|{}|{message}",
            i32::from(ok)
        )));
    });
}

pub(super) fn launch_markdown_conversion(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    out: &EventSender,
    options: markdown::Options,
) {
    let cancel = state
        .borrow()
        .convert_cancel
        .clone()
        .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));
    {
        let mut s = state.borrow_mut();
        s.convert_preparing = false;
        s.convert_pending_options = None;
        s.runtime = RuntimeMode::MarkdownConverter;
        s.started = Instant::now();
        s.convert_current.clear();
        s.convert_counts = ConvertCounts::default();
    }
    ui.set_convert_preparing(false);
    ui.set_convert_detail_text("".into());
    ui.set_convert_detail_open(false);
    ui.set_busy(true);
    ui.set_paused(false);
    ui.set_ready(false);
    ui.set_error_text("".into());
    ui.set_notice_text("".into());
    ui.set_convert_metrics("正在扫描…".into());
    ui.set_convert_progress(-1.0);
    ui.set_convert_progress_note("正在扫描".into());
    ui.set_convert_status("正在转换…".into());
    let out = out.clone();
    let parent = tracing::Span::current();
    #[cfg(test)]
    let conversion_hook = state.borrow().review_remediation.conversion.clone();
    std::thread::spawn(move || {
        let _parent = parent.enter();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            #[cfg(test)]
            if let Some(hook) = conversion_hook {
                hook();
            }
            markdown::run(&options, &cancel, |event| {
                let text = match event {
                    markdown::Event::Started { total } => format!("CONVERTER_STARTED|{total}"),
                    markdown::Event::FileStarted {
                        relative,
                        index,
                        total,
                    } => format!(
                        "CONVERTER_FILE_STARTED|{index}|{total}|{}",
                        relative.display()
                    ),
                    markdown::Event::FileFinished {
                        relative,
                        partial,
                        success,
                        message,
                    } => format!(
                        "CONVERTER_FILE_FINISHED|{}|{}|{}|{}",
                        i32::from(partial),
                        i32::from(success),
                        relative.display(),
                        message.replace('|', "／")
                    ),
                    markdown::Event::Log(text) => {
                        let _ = out.send(Event::Status(format!("CONVERTER_LOG|{text}")));
                        return;
                    }
                    markdown::Event::FileCancelled { relative } => {
                        format!("CONVERTER_FILE_CANCELLED|{}", relative.display())
                    }
                };
                let _ = out.send(Event::Status(text));
            })
        }))
        .unwrap_or_else(|_| Err("转 Markdown 后台任务意外退出；已完成的输出保留".to_string()));
        let text = match result {
            Ok(summary) => format!(
                "CONVERTER_DONE|{}|{}|{}|{}|{}|{}",
                summary.success,
                summary.partial,
                summary.failed,
                summary.skipped_existing,
                summary.skipped_duplicate,
                i32::from(summary.stopped)
            ),
            Err(error) => format!("CONVERTER_FAIL|{error}"),
        };
        if out.send(Event::MdDone(text)).is_err() {
            tracing::warn!(
                component = "gui_worker",
                event = "gui_worker_delivery_failed",
                operation = "markdown_conversion",
                stage = "event_send",
                error_type = "gui_closed",
                "转 Markdown 后台结果无法投递到已关闭界面"
            );
        }
    });
}
