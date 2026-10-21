//! ACP 模型服务装配：观察者线程、命令通道与界面回包应用（自 gui.rs 拆分）。
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AcpOperation {
    Ensure,
    Save,
    Apply,
    Stop,
}

impl AcpOperation {
    pub(super) fn text(self) -> &'static str {
        match self {
            Self::Ensure => "连接",
            Self::Save => "保存",
            Self::Apply => "应用并重启",
            Self::Stop => "退出",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AcpRequest {
    pub(super) id: u64,
    pub(super) operation: AcpOperation,
}

impl AcpRequest {
    pub(super) const CONNECT: Self = Self {
        id: 0,
        operation: AcpOperation::Ensure,
    };
}

pub(super) enum AcpCommand {
    Ensure,
    Save {
        executable: String,
        arguments: String,
        port: String,
        stopped: bool,
    },
    Apply,
    Stop,
}

impl AcpCommand {
    fn operation(&self) -> AcpOperation {
        match self {
            Self::Ensure => AcpOperation::Ensure,
            Self::Save { .. } => AcpOperation::Save,
            Self::Apply => AcpOperation::Apply,
            Self::Stop => AcpOperation::Stop,
        }
    }
}

pub(super) enum AcpMessage {
    Snapshot(u64, std::result::Result<ServiceStatus, String>),
    Completed(AcpRequest, std::result::Result<ServiceStatus, String>),
}

/// AH-06/AH-07：IPC、SQLite、启动和安全收尾均不进入界面线程。
/// 丢弃本句柄只停止观察，不取消后台操作或关闭 Agent。
pub(super) struct AcpObserver {
    pub(super) commands: mpsc::Sender<(AcpRequest, AcpCommand)>,
    stops: mpsc::Sender<(AcpRequest, AcpCommand)>,
    explicitly_stopped: Arc<std::sync::atomic::AtomicBool>,
    pub(super) closed: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    pub(super) threads: Vec<std::thread::JoinHandle<()>>,
}

impl Drop for AcpObserver {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

fn run_acp_commands(
    receiver: &mpsc::Receiver<(AcpRequest, AcpCommand)>,
    sender: &mpsc::Sender<AcpMessage>,
    closed: &std::sync::atomic::AtomicBool,
    explicitly_stopped: &std::sync::atomic::AtomicBool,
    startup: &std::sync::Mutex<()>,
    #[cfg(test)] hooks: &ReviewRemediationHooks,
) {
    loop {
        let (request, command) = match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if closed.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // 只串行化可创建后台的 Ensure/Save 与 Stop 完成边界。
        // Apply 等待在途请求期间不持锁，独立 Stop 仍可送达请求级取消。
        let _startup = matches!(
            command,
            AcpCommand::Ensure | AcpCommand::Save { .. } | AcpCommand::Stop
        )
        .then(|| {
            startup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result = match command {
            AcpCommand::Ensure => {
                if explicitly_stopped.load(Ordering::Acquire) {
                    acp_runtime::status()
                } else {
                    #[cfg(test)]
                    if let Some(hook) = &hooks.acp_before_ensure {
                        hook();
                    }
                    acp_runtime::ensure_started()
                }
            }
            AcpCommand::Save {
                executable,
                arguments,
                port,
                stopped,
            } => (|| {
                let config = ServiceConfig {
                    executable,
                    arguments: acp_settings::parse_arguments(&arguments)?,
                    port: acp_settings::parse_port(&port)?,
                };
                acp_settings::validate_config(&config)?;
                if stopped || explicitly_stopped.load(Ordering::Acquire) {
                    // 当前 GUI 主动退出后只持久保存，不隐式重开服务（AH-10）。
                    acp_settings::save_config(&config)?;
                    acp_runtime::status()
                } else {
                    acp_runtime::save_config(&config)
                }
            })(),
            AcpCommand::Apply => {
                if explicitly_stopped.load(Ordering::Acquire) {
                    acp_runtime::status()
                } else {
                    acp_runtime::apply_and_restart()
                }
            }
            AcpCommand::Stop => acp_runtime::stop(),
        };
        #[cfg(test)]
        if let Some(hook) = &hooks.acp_completed {
            hook(request.operation);
        }
        if sender
            .send(AcpMessage::Completed(
                request,
                result.map_err(|error| error.to_string()),
            ))
            .is_err()
        {
            break;
        }
    }
}

pub(super) fn start_acp_observer(state: &Rc<RefCell<State>>, ensure: bool) {
    let mut state = state.borrow_mut();
    if let Some(observer) = &state.acp_observer {
        if ensure && !state.acp_explicit_stopped {
            let _ = observer
                .commands
                .send((AcpRequest::CONNECT, AcpCommand::Ensure));
        }
        return;
    }
    let (commands, receiver) = mpsc::channel();
    let (stops, stop_receiver) = mpsc::channel();
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let explicitly_stopped = Arc::new(std::sync::atomic::AtomicBool::new(
        state.acp_explicit_stopped,
    ));
    let startup = Arc::new(std::sync::Mutex::new(()));
    #[cfg(test)]
    let mut threads = Vec::with_capacity(3);
    // Stop 拥有独立控制线程和 IPC 连接，不排在等待排空的 Apply 后面。
    for receiver in [receiver, stop_receiver] {
        let worker_closed = closed.clone();
        let worker_stopped = explicitly_stopped.clone();
        let worker_startup = startup.clone();
        let sender = state.acp_sender.clone();
        #[cfg(test)]
        let test_hooks = state.review_remediation.clone();
        let _worker = std::thread::spawn(move || {
            run_acp_commands(
                &receiver,
                &sender,
                &worker_closed,
                &worker_stopped,
                &worker_startup,
                #[cfg(test)]
                &test_hooks,
            );
        });
        #[cfg(test)]
        threads.push(_worker);
    }
    let observation_generation = state.acp_observation_generation.clone();
    let observer_closed = closed.clone();
    let sender = state.acp_sender.clone();
    let _observer = std::thread::spawn(move || {
        while !observer_closed.load(Ordering::Acquire) {
            // 在读取开始时取代际；保存完成之后才送达的旧读取仍属于旧代际。
            let generation = observation_generation.load(Ordering::Acquire);
            let snapshot = acp_runtime::status().map_err(|error| error.to_string());
            if sender
                .send(AcpMessage::Snapshot(generation, snapshot))
                .is_err()
            {
                break;
            }
            // 短等待使关闭观察及时生效；不阻塞 GUI，不触发 ensure/restart。
            for _ in 0..5 {
                if observer_closed.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
    #[cfg(test)]
    threads.push(_observer);
    if ensure && !state.acp_explicit_stopped {
        let _ = commands.send((AcpRequest::CONNECT, AcpCommand::Ensure));
    }
    state.acp_observer = Some(AcpObserver {
        commands,
        stops,
        explicitly_stopped,
        closed,
        #[cfg(test)]
        threads,
    });
}

pub(super) fn request_acp_action(
    ui: &AppWindow,
    state: &Rc<RefCell<State>>,
    command: AcpCommand,
    text: &str,
) {
    start_acp_observer(state, false);
    let operation = command.operation();
    let mut state = state.borrow_mut();
    state.acp_request_id += 1;
    let request = AcpRequest {
        id: state.acp_request_id,
        operation,
    };
    let sent = state.acp_observer.as_ref().is_some_and(|observer| {
        if operation == AcpOperation::Stop {
            observer.explicitly_stopped.store(true, Ordering::Release);
            observer.stops.send((request, command)).is_ok()
        } else {
            observer.commands.send((request, command)).is_ok()
        }
    });
    if sent {
        state.acp_pending_request = Some(request);
        // 抢占时即建立观察边界：Stop 前开始读取的快照不得回退退出状态。
        state
            .acp_observation_generation
            .fetch_add(1, Ordering::AcqRel);
        ui.set_acp_apply_inflight(operation == AcpOperation::Apply);
        ui.set_acp_request_pending(true);
        ui.set_acp_operation(text.into());
        ui.set_acp_action_error("".into());
    } else {
        ui.set_acp_action_error("模型服务操作线程已退出，请重新打开工具箱".into());
    }
}

pub(super) fn can_request_acp_stop(ui: &AppWindow, state: &State) -> bool {
    // 退出意图只禁止自动重启；IPC 失败后确认后台仍在运行时，允许用户重试退出。
    ui.get_acp_status_known()
        && !ui.get_acp_service_pid().is_empty()
        && (!ui.get_acp_request_pending()
            || state
                .acp_pending_request
                .is_some_and(|request| request.operation == AcpOperation::Apply))
}

fn acp_config_text(config: Option<&ServiceConfig>) -> String {
    config.map_or_else(
        || "无".into(),
        |config| {
            format!(
                "程序：{}\n参数：{}\n地址：http://127.0.0.1:{}",
                config.executable,
                acp_settings::format_arguments(&config.arguments),
                config.port
            )
        },
    )
}

pub(super) fn apply_acp_status(ui: &AppWindow, state: &Rc<RefCell<State>>, status: &ServiceStatus) {
    use jchtools_acp_api::acp_api::ServicePhase;
    let (phase, text) = match status.phase {
        ServicePhase::Unconfigured => ("unconfigured", "未配置有效的 Agent 启动程序"),
        ServicePhase::Starting => ("starting", "正在启动模型服务并初始化 ACP"),
        ServicePhase::Ready => ("ready", "模型服务已就绪"),
        ServicePhase::Draining => ("draining", "停止接收新请求；等待所有已有请求完成后应用配置"),
        ServicePhase::Stopping => ("stopping", "停止接收新请求；正在取消并等待在途任务安全收尾"),
        ServicePhase::Stopped => ("stopped", "模型服务已停止"),
        ServicePhase::Error => ("error", "模型服务未就绪，请查看错误详情"),
    };
    ui.set_acp_phase(phase.into());
    ui.set_acp_status(text.into());
    ui.set_acp_ready(status.phase == ServicePhase::Ready);
    ui.set_acp_status_known(true);
    ui.set_acp_pending_apply(status.pending_apply() && !state.borrow().acp_explicit_stopped);
    ui.set_acp_service_pid(
        status
            .service_pid
            .map_or_else(String::new, |pid| pid.to_string())
            .into(),
    );
    ui.set_acp_agent_pid(
        status
            .agent_pid
            .map_or_else(String::new, |pid| pid.to_string())
            .into(),
    );
    ui.set_acp_executing(i32::try_from(status.executing).unwrap_or(i32::MAX));
    ui.set_acp_waiting(i32::try_from(status.waiting).unwrap_or(i32::MAX));
    {
        let mut state = state.borrow_mut();
        if let Some(error) = &status.error {
            state.acp_service_error = Some(error.clone());
        } else if status.phase == ServicePhase::Ready {
            // 无后台的 Stopped 轮询不是恢复，不能抹去自动连接的具体失败原因。
            state.acp_service_error = None;
        }
        ui.set_acp_service_error(
            state
                .acp_service_error
                .as_deref()
                .unwrap_or_default()
                .into(),
        );
    }
    ui.set_acp_saved_config(acp_config_text(status.saved_config.as_ref()).into());
    ui.set_acp_running_config(acp_config_text(status.running_config.as_ref()).into());
    if !state.borrow().acp_draft_edited {
        if let Some(config) = &status.saved_config {
            ui.set_acp_executable(config.executable.clone().into());
            ui.set_acp_arguments(acp_settings::format_arguments(&config.arguments).into());
            ui.set_acp_port(config.port.to_string().into());
        }
    }
}

pub(super) fn apply_acp_messages(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let messages: Vec<_> = state
        .borrow()
        .acp_receiver
        .borrow_mut()
        .try_iter()
        .collect();
    for message in messages {
        let result = match message {
            AcpMessage::Snapshot(generation, result) => {
                if generation
                    != state
                        .borrow()
                        .acp_observation_generation
                        .load(Ordering::Acquire)
                {
                    continue;
                }
                result
            }
            AcpMessage::Completed(request, result) => {
                let mut state = state.borrow_mut();
                if request.operation == AcpOperation::Ensure {
                    // 已退出的 GUI 不接受迟到自动连接的结果。
                    if state.acp_explicit_stopped {
                        continue;
                    }
                } else if state.acp_pending_request != Some(request) {
                    // Apply 被 Stop 抢占后，迟到结果不消费 Stop 门禁或覆盖其终态。
                    continue;
                }
                // 完成回包确立新的观察边界；此前已开始的读取，即使稍后送达也不得上屏。
                state
                    .acp_observation_generation
                    .fetch_add(1, Ordering::AcqRel);
                // 后台自动连接不消费正在保存/应用/退出的 UI 门禁。
                if request.operation != AcpOperation::Ensure {
                    state.acp_pending_request = None;
                    ui.set_acp_apply_inflight(false);
                    let action = request.operation.text();
                    ui.set_acp_request_pending(false);
                    ui.set_acp_operation(
                        if result.is_ok() {
                            format!("{action}已完成")
                        } else {
                            format!("{action}未成功")
                        }
                        .into(),
                    );
                    if request.operation == AcpOperation::Save && result.is_ok() {
                        state.acp_draft_edited = false;
                    }
                    ui.set_acp_action_error(
                        result.as_ref().err().cloned().unwrap_or_default().into(),
                    );
                }
                drop(state);
                result
            }
        };
        match result {
            Ok(status) => apply_acp_status(ui, state, &status),
            Err(error) => {
                state.borrow_mut().acp_service_error = Some(error.clone());
                ui.set_acp_status_known(false);
                ui.set_acp_ready(false);
                ui.set_acp_status("无法读取或连接模型服务".into());
                ui.set_acp_service_error(error.into());
            }
        }
    }
}

pub(super) fn wire_acp_http(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let edited_state = state.clone();
    ui.on_acp_config_edited(move || {
        edited_state.borrow_mut().acp_draft_edited = true;
    });
    let weak = ui.as_weak();
    let save_state = state.clone();
    ui.on_acp_save_config(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_confirm_kind() != 0 || ui.get_acp_request_pending() {
            return;
        }
        let stopped = save_state.borrow().acp_explicit_stopped;
        save_state.borrow_mut().acp_draft_edited = true;
        request_acp_action(
            &ui,
            &save_state,
            AcpCommand::Save {
                executable: ui.get_acp_executable().to_string(),
                arguments: ui.get_acp_arguments().to_string(),
                port: ui.get_acp_port().to_string(),
                stopped,
            },
            "正在校验并保存配置…",
        );
    });
    let weak = ui.as_weak();
    let apply_state = state.clone();
    ui.on_acp_apply_and_restart(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_confirm_kind() != 0
            || ui.get_acp_request_pending()
            || !ui.get_acp_status_known()
            || !ui.get_acp_pending_apply()
            || apply_state.borrow().acp_explicit_stopped
        {
            return;
        }
        request_acp_action(
            &ui,
            &apply_state,
            AcpCommand::Apply,
            "正在停止接新请求，等待全部已有请求完成后应用并重启…",
        );
    });
    let weak = ui.as_weak();
    let stop_state = state.clone();
    ui.on_acp_request_stop(move || {
        let Some(ui) = weak.upgrade() else { return; };
        if ui.get_confirm_kind() != 0 || !can_request_acp_stop(&ui, &stop_state.borrow()) {
            return;
        }
        ui.set_confirm_pending(false);
        ui.set_acknowledge(false);
        ui.set_confirm_text("确认退出独立模型服务？\n将停止接收新请求，取消在途模型请求并等待安全收尾，再释放本应用拥有的 Agent、终端及连接。不自动强杀。\n截图、Xberg 及其他工具不受影响。\n当前工具箱不会自动重启模型服务；重新打开工具箱后按已保存配置自动启动。".into());
        ui.set_confirm_kind(5);
    });
}
