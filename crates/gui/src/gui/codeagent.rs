//! CodeAgent Token 后台维护的界面装配：请求通道与状态回包应用（自 gui.rs 拆分）。
use super::{mpsc, AppWindow, ComponentHandle, Duration, Rc, RefCell, State};

pub(super) enum CodeAgentCommand {
    Ensure,
    Save(String),
}

pub(super) struct CodeAgentMessage {
    pub(super) saved: Option<std::result::Result<(), String>>,
    pub(super) status: std::result::Result<(jchtools_codeagent::codeagent::Status, u64), String>,
}

pub(super) struct CodeAgentObserver {
    pub(super) commands: mpsc::Sender<CodeAgentCommand>,
    pub(super) messages: mpsc::Receiver<CodeAgentMessage>,
}

pub(super) fn request_codeagent(state: &Rc<RefCell<State>>, command: CodeAgentCommand) -> bool {
    let mut state = state.borrow_mut();
    let observer = state.codeagent_observer.get_or_insert_with(|| {
        let (commands, receiver) = mpsc::channel();
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || loop {
            let command = match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            let mut saved = None;
            let action = match command {
                Some(CodeAgentCommand::Ensure) => {
                    jchtools_codeagent::codeagent::ensure_started().map(|_| ())
                }
                Some(CodeAgentCommand::Save(value)) => {
                    let result = jchtools_codeagent::codeagent::save_interval(&value);
                    saved = Some(result);
                    // 保存校验失败不影响后台存活；错误经 saved 单独呈现，状态仍独立刷新。
                    Ok(())
                }
                None => Ok(()),
            };
            let status =
                action.and_then(|()| jchtools_codeagent::codeagent::status_with_saved_interval());
            if sender.send(CodeAgentMessage { saved, status }).is_err() {
                return;
            }
        });
        CodeAgentObserver { commands, messages }
    });
    observer.commands.send(command).is_ok()
}

fn apply_codeagent_status(ui: &AppWindow, status: &jchtools_codeagent::codeagent::Status) {
    ui.set_codeagent_running(status.running);
    ui.set_codeagent_status(
        if status.running {
            format!(
                "独立后台运行中 · PID {}",
                status
                    .pid
                    .map_or_else(|| "正在启动".into(), |pid| pid.to_string())
            )
        } else {
            "独立后台未运行；重新打开页面可启动".into()
        }
        .into(),
    );
    ui.set_codeagent_last_check(status.last_check.as_deref().unwrap_or("尚未检查").into());
    ui.set_codeagent_result(status.result.text().into());
    ui.set_codeagent_check_failed(status.result.failed());
    ui.set_codeagent_effective_interval(
        format!("后台当前检查间隔：{} 秒", status.interval_seconds).into(),
    );
}

pub(super) fn apply_codeagent_messages(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let messages = state
        .borrow()
        .codeagent_observer
        .as_ref()
        .map(|observer| observer.messages.try_iter().collect::<Vec<_>>())
        .unwrap_or_default();
    for message in messages {
        if let Some(saved) = message.saved {
            ui.set_codeagent_pending(false);
            ui.set_codeagent_error(saved.as_ref().err().cloned().unwrap_or_default().into());
            if saved.is_ok() {
                ui.set_codeagent_draft_edited(false);
            }
        }
        match message.status {
            Ok((status, interval)) => {
                apply_codeagent_status(ui, &status);
                if !ui.get_codeagent_draft_edited() {
                    ui.set_codeagent_interval(interval.to_string().into());
                }
            }
            Err(error) => {
                ui.set_codeagent_running(false);
                ui.set_codeagent_status("后台状态未知".into());
                ui.set_codeagent_error(error.into());
            }
        }
    }
}

pub(super) fn wire_codeagent(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let weak = ui.as_weak();
    let state = state.clone();
    ui.on_codeagent_save_interval(move || {
        if let Some(ui) = weak.upgrade() {
            if !ui.get_codeagent_pending() {
                ui.set_codeagent_pending(true);
                ui.set_codeagent_error("".into());
                if !request_codeagent(
                    &state,
                    CodeAgentCommand::Save(ui.get_codeagent_interval().to_string()),
                ) {
                    ui.set_codeagent_pending(false);
                    ui.set_codeagent_error("CodeAgent 操作线程不可用".into());
                }
            }
        }
    });
}
