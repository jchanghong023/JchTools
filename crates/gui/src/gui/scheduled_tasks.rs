//! 定时任务页面：界面线程只管理草稿和消息，数据库及服务访问由观察线程完成。
use super::{mpsc, AppWindow, Cell, ComponentHandle, Duration, Rc, RefCell, VecModel};
use jchtools_scheduled_tasks::{
    self as scheduled, IntervalUnit, RunLog, RunRecord, Snapshot, Task,
};
use jchtools_ui::{ScheduledRunRow, ScheduledTaskRow};
use slint::Model;
use std::{collections::BTreeMap, path::PathBuf};

enum Command {
    Save(Task),
    Delete(i64),
    Toggle(i64, bool),
    Run(i64),
    Stop(i64),
    Observe(Option<i64>, Option<i64>),
    Export(PathBuf, String),
}

enum Message {
    Snapshot(Result<Snapshot, String>),
    History(i64, Result<Vec<RunRecord>, String>),
    Log(i64, Result<RunLog, String>),
    Action {
        save: bool,
        result: Result<(), String>,
    },
}

fn observe(
    sender: &mpsc::SyncSender<Message>,
    receiver: &mpsc::Receiver<Command>,
    #[cfg(feature = "perf-tracing")] parent: tracing::Span,
) {
    {
        // 只计首次后台连接与快照；后续常驻观察不属于启动耗时。
        #[cfg(feature = "perf-tracing")]
        let _parent = parent.enter();
        jchtools_foundation::perf_span!("gui_startup_scheduled_tasks_check");
        let snapshot = scheduled::ensure_started();
        #[cfg(feature = "perf-tracing")]
        tracing::info!(
            target: "perf",
            event = "gui_startup_background_checked",
            component = "scheduled_tasks",
            ok = snapshot.is_ok(),
        );
        if sender.send(Message::Snapshot(snapshot)).is_err() {
            return;
        }
    }
    #[cfg(feature = "perf-tracing")]
    drop(parent);
    let mut selected = None;
    let mut selected_run = None;
    loop {
        match receiver.recv_timeout(Duration::from_millis(600)) {
            Ok(Command::Observe(task, run)) => {
                selected = task;
                selected_run = run;
            }
            Ok(command) => {
                let (save, result) = match command {
                    Command::Save(task) => (true, scheduled::save(&task).map(|_| ())),
                    Command::Delete(id) => (false, scheduled::delete(id)),
                    Command::Toggle(id, enabled) => (false, scheduled::set_enabled(id, enabled)),
                    Command::Run(id) => (
                        false,
                        scheduled::ensure_started().and_then(|_| scheduled::run_now(id)),
                    ),
                    Command::Stop(id) => (false, scheduled::stop_run(id)),
                    Command::Export(path, text) => (
                        false,
                        std::fs::write(path, text)
                            .map_err(|error| format!("导出日志失败：{error}")),
                    ),
                    Command::Observe(_, _) => unreachable!(),
                };
                if sender.send(Message::Action { save, result }).is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if sender.send(Message::Snapshot(scheduled::list())).is_err() {
            return;
        }
        if let Some(id) = selected {
            if sender
                .send(Message::History(id, scheduled::history(id)))
                .is_err()
            {
                return;
            }
        }
        if let Some(id) = selected_run {
            if sender
                .send(Message::Log(id, scheduled::read_log(id)))
                .is_err()
            {
                return;
            }
        }
    }
}

// UTC keeps timestamps unambiguous across platforms.
fn timestamp(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds).map_or_else(
        || format!("{milliseconds} ms UTC"),
        |time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn log_text(log: &RunLog) -> String {
    let record = &log.record;
    format!(
        "命令：{}\n工作目录：{}\n开始时间：{}\n结束时间：{}\n耗时：{}\n触发方式：{}\n状态：{}\n退出码：{}\n{}\n--- 标准输出 stdout ---\n{}\n--- 错误输出 stderr ---\n{}",
        record.command,
        record.working_directory,
        timestamp(record.started_at),
        record.ended_at.map_or_else(|| "尚未结束".into(), timestamp),
        record.duration_ms.map_or_else(|| "尚未结束".into(), |value| format!("{value} 毫秒")),
        record.trigger,
        record.status,
        record.exit_code.map_or_else(|| "不可用".into(), |value| value.to_string()),
        if record.truncated { "输出已截断，仅保留最新尾部。" } else { "" },
        log.stdout,
        log.stderr,
    )
}

fn positive(text: &str, name: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{name}必须是正整数"));
    }
    match text.parse::<u64>() {
        Ok(value) if value > 0 => Ok(value),
        _ => Err(format!("{name}必须是范围内的正整数")),
    }
}

fn draft(ui: &AppWindow, id: i64) -> Result<Task, String> {
    let mut environment = BTreeMap::new();
    let env = ui.get_scheduled_environment();
    for line in env.lines().filter(|line| !line.trim().is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| "环境变量每行须为 KEY=VALUE".to_string())?;
        let key = key.trim();
        if key.is_empty()
            || key.contains('\0')
            || value.contains('\0')
            || environment.contains_key(key)
        {
            return Err("环境变量名称不能为空、重复或包含空字符".into());
        }
        environment.insert(key.to_string(), value.to_string());
    }
    let timeout = ui.get_scheduled_timeout();
    Ok(Task {
        id,
        name: ui.get_scheduled_name().to_string(),
        command: ui.get_scheduled_command().to_string(),
        working_directory: ui.get_scheduled_directory().to_string(),
        interval: positive(ui.get_scheduled_interval().as_str(), "周期")?,
        unit: if ui.get_scheduled_unit() == 1 {
            IntervalUnit::Hours
        } else {
            IntervalUnit::Minutes
        },
        enabled: ui.get_scheduled_enabled(),
        shell: if ui.get_scheduled_shell() == 1 {
            "powershell.exe"
        } else {
            "cmd.exe"
        }
        .into(),
        environment,
        timeout_seconds: if timeout.trim().is_empty() {
            None
        } else {
            Some(positive(timeout.as_str(), "超时")?)
        },
    })
}

fn set_draft(ui: &AppWindow, task: &Task) {
    ui.set_scheduled_name(task.name.clone().into());
    ui.set_scheduled_command(task.command.clone().into());
    ui.set_scheduled_directory(task.working_directory.clone().into());
    ui.set_scheduled_interval(task.interval.to_string().into());
    ui.set_scheduled_unit(i32::from(task.unit == IntervalUnit::Hours));
    ui.set_scheduled_shell(i32::from(task.shell.eq_ignore_ascii_case("powershell.exe")));
    ui.set_scheduled_environment(
        task.environment
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    );
    ui.set_scheduled_timeout(
        task.timeout_seconds
            .map_or_else(String::new, |value| value.to_string())
            .into(),
    );
    ui.set_scheduled_enabled(task.enabled);
    ui.set_scheduled_advanced(false);
    ui.set_scheduled_error("".into());
    ui.set_scheduled_editor_open(true);
}

fn send_action(ui: &AppWindow, sender: &mpsc::Sender<Command>, command: Command) {
    if ui.get_scheduled_pending() {
        return;
    }
    ui.set_scheduled_error("".into());
    if sender.send(command).is_ok() {
        ui.set_scheduled_pending(true);
    } else {
        ui.set_scheduled_error("定时任务观察线程不可用".into());
    }
}

pub(super) fn wire_scheduled_tasks(ui: &AppWindow) -> slint::Timer {
    let (commands, receiver) = mpsc::channel();
    let (sender, messages) = mpsc::sync_channel(3);
    #[cfg(feature = "perf-tracing")]
    let parent = tracing::Span::current();
    {
        jchtools_foundation::perf_span!("gui_startup_scheduled_tasks_dispatch");
        std::thread::spawn(move || {
            observe(
                &sender,
                &receiver,
                #[cfg(feature = "perf-tracing")]
                parent,
            );
        });
    }
    ui.set_scheduled_status("正在连接独立后台…".into());
    let tasks = Rc::new(RefCell::new(Vec::<scheduled::TaskStatus>::new()));
    let editor_id = Rc::new(Cell::new(0_i64));
    let loaded_log = Rc::new(Cell::new(None::<i64>));

    {
        let weak = ui.as_weak();
        let editor_id = editor_id.clone();
        ui.on_scheduled_new(move || {
            if let Some(ui) = weak.upgrade() {
                editor_id.set(0);
                set_draft(
                    &ui,
                    &Task {
                        id: 0,
                        name: String::new(),
                        command: String::new(),
                        working_directory: String::new(),
                        interval: 1,
                        unit: IntervalUnit::Minutes,
                        enabled: false,
                        shell: "cmd.exe".into(),
                        environment: BTreeMap::new(),
                        timeout_seconds: None,
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let tasks = tasks.clone();
        let editor_id = editor_id.clone();
        ui.on_scheduled_edit(move |id| {
            if let Some(ui) = weak.upgrade() {
                if let Ok(id) = id.parse::<i64>() {
                    if let Some(task) = tasks.borrow().iter().find(|entry| entry.task.id == id) {
                        editor_id.set(id);
                        set_draft(&ui, &task.task);
                    } else {
                        ui.set_scheduled_error("任务不存在，请刷新后重试".into());
                    }
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let commands = commands.clone();
        ui.on_scheduled_save(move || {
            if let Some(ui) = weak.upgrade() {
                match draft(&ui, editor_id.get()) {
                    Ok(task) => send_action(&ui, &commands, Command::Save(task)),
                    Err(error) => ui.set_scheduled_error(error.into()),
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_scheduled_browse(move || {
            if let Some(ui) = weak.upgrade() {
                let entered = ui.get_scheduled_directory();
                super::task_flow::pick_directory(
                    &ui,
                    "选择任务工作目录",
                    entered.as_str(),
                    |ui, path| {
                        ui.set_scheduled_directory(path.to_string_lossy().into_owned().into());
                    },
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let commands = commands.clone();
        let tasks = tasks.clone();
        ui.on_scheduled_delete(move |id| {
            if let (Some(ui), Ok(id)) = (weak.upgrade(), id.parse::<i64>()) {
                if tasks
                    .borrow()
                    .iter()
                    .any(|entry| entry.task.id == id && entry.running)
                {
                    ui.set_scheduled_error("任务正在运行，请先停止本次执行".into());
                } else {
                    send_action(&ui, &commands, Command::Delete(id));
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let commands = commands.clone();
        let tasks = tasks.clone();
        ui.on_scheduled_toggle(move |id| {
            if let (Some(ui), Ok(id)) = (weak.upgrade(), id.parse::<i64>()) {
                if let Some(entry) = tasks.borrow().iter().find(|entry| entry.task.id == id) {
                    send_action(&ui, &commands, Command::Toggle(id, !entry.task.enabled));
                } else {
                    ui.set_scheduled_error("任务不存在，请刷新后重试".into());
                }
            }
        });
    }
    {
        let weak = ui.as_weak();
        let commands = commands.clone();
        ui.on_scheduled_run(move |id| {
            if let (Some(ui), Ok(id)) = (weak.upgrade(), id.parse::<i64>()) {
                send_action(&ui, &commands, Command::Run(id));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let commands = commands.clone();
        ui.on_scheduled_stop(move |id| {
            if let (Some(ui), Ok(id)) = (weak.upgrade(), id.parse::<i64>()) {
                send_action(&ui, &commands, Command::Stop(id));
            }
        });
    }
    {
        let weak = ui.as_weak();
        let loaded_log = loaded_log.clone();
        ui.on_scheduled_show_logs(move |id| {
            if let Some(ui) = weak.upgrade() {
                loaded_log.set(None);
                ui.set_scheduled_selected(id);
                ui.set_scheduled_selected_run("".into());
                ui.set_scheduled_runs(Rc::new(VecModel::<ScheduledRunRow>::default()).into());
                ui.set_scheduled_log_text("请选择执行记录".into());
                ui.set_scheduled_error("".into());
                ui.set_scheduled_log_open(true);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let loaded_log = loaded_log.clone();
        ui.on_scheduled_select_run(move |id| {
            if let Some(ui) = weak.upgrade() {
                loaded_log.set(None);
                ui.set_scheduled_selected_run(id);
                ui.set_scheduled_log_text("正在读取日志…".into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let commands = commands.clone();
        let loaded_log = loaded_log.clone();
        ui.on_scheduled_export_log(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_scheduled_selected_run().parse::<i64>().ok() != loaded_log.get()
                    || loaded_log.get().is_none()
                {
                    ui.set_scheduled_error("请先选择并加载执行记录".into());
                    return;
                }
                let mut dialog = rfd::FileDialog::new()
                    .set_title("导出执行日志")
                    .set_file_name("scheduled-task-log.txt")
                    .add_filter("文本文件", &["txt"]);
                if let Some(start) =
                    super::task_flow::directory_dialog_start(ui.get_scheduled_directory().as_str())
                {
                    dialog = dialog.set_directory(start);
                }
                if let Some(path) = dialog.save_file() {
                    send_action(
                        &ui,
                        &commands,
                        Command::Export(path, ui.get_scheduled_log_text().to_string()),
                    );
                }
            }
        });
    }

    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    let mut observing = (None, None);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(150),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let desired = if ui.get_scheduled_log_open() {
                (
                    ui.get_scheduled_selected().parse::<i64>().ok(),
                    ui.get_scheduled_selected_run().parse::<i64>().ok(),
                )
            } else {
                (None, None)
            };
            if desired != observing {
                observing = desired;
                let _ = commands.send(Command::Observe(desired.0, desired.1));
            }
            for message in messages.try_iter() {
                match message {
                    Message::Snapshot(Ok(snapshot)) => {
                        ui.set_scheduled_status(
                            if snapshot.running {
                                format!(
                                    "独立后台运行中 · PID {}",
                                    snapshot
                                        .pid
                                        .map_or_else(|| "正在启动".into(), |pid| pid.to_string())
                                )
                            } else {
                                "独立后台未运行".into()
                            }
                            .into(),
                        );
                        let rows = snapshot
                            .tasks
                            .iter()
                            .map(|entry| ScheduledTaskRow {
                                id: entry.task.id.to_string().into(),
                                name: entry.task.name.clone().into(),
                                period: format!(
                                    "每 {} {}",
                                    entry.task.interval,
                                    if entry.task.unit == IntervalUnit::Hours {
                                        "小时"
                                    } else {
                                        "分钟"
                                    }
                                )
                                .into(),
                                enabled: entry.task.enabled,
                                running: entry.running,
                                state: entry.current_status.clone().into(),
                                result: entry.last_result.clone().into(),
                                next_run: entry
                                    .next_run
                                    .map_or_else(|| "—".into(), timestamp)
                                    .into(),
                            })
                            .collect::<Vec<_>>();
                        if !ui.get_scheduled_rows().iter().eq(rows.iter().cloned()) {
                            ui.set_scheduled_rows(Rc::new(VecModel::from(rows)).into());
                        }
                        *tasks.borrow_mut() = snapshot.tasks;
                    }
                    Message::Snapshot(Err(error)) => {
                        ui.set_scheduled_status("后台状态未知".into());
                        ui.set_scheduled_error(error.into());
                    }
                    Message::History(id, result) if desired.0 == Some(id) => match result {
                        Ok(records) => {
                            let rows = records
                                .iter()
                                .map(|record| ScheduledRunRow {
                                    id: record.id.to_string().into(),
                                    summary: format!(
                                        "{} · {} · {}",
                                        timestamp(record.started_at),
                                        record.trigger,
                                        record.status
                                    )
                                    .into(),
                                })
                                .collect::<Vec<_>>();
                            if !ui.get_scheduled_runs().iter().eq(rows.iter().cloned()) {
                                ui.set_scheduled_runs(Rc::new(VecModel::from(rows)).into());
                            }
                        }
                        Err(error) => ui.set_scheduled_error(error.into()),
                    },
                    Message::Log(id, result) if desired.1 == Some(id) => match result {
                        Ok(log) => {
                            loaded_log.set(Some(id));
                            ui.set_scheduled_log_text(log_text(&log).into());
                        }
                        Err(error) => {
                            loaded_log.set(None);
                            ui.set_scheduled_log_text("日志读取失败".into());
                            ui.set_scheduled_error(error.into());
                        }
                    },
                    Message::Action { save, result } => {
                        ui.set_scheduled_pending(false);
                        match result {
                            Ok(()) => {
                                ui.set_scheduled_error("".into());
                                if save {
                                    ui.set_scheduled_editor_open(false);
                                }
                            }
                            Err(error) => ui.set_scheduled_error(error.into()),
                        }
                    }
                    Message::History(_, _) | Message::Log(_, _) => {}
                }
            }
        },
    );
    timer
}
