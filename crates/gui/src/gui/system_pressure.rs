//! 系统压力记录：界面仅发送手动启停命令，状态查询及服务访问均在观察线程执行。
use super::{mpsc, AppWindow, ComponentHandle, Duration};
use jchtools_system_pressure::{self as pressure, Snapshot};

enum Command {
    Start,
    Stop,
}

struct Message {
    action: bool,
    result: Result<Snapshot, String>,
}

fn observe(
    sender: &mpsc::SyncSender<Message>,
    receiver: &mpsc::Receiver<Command>,
    #[cfg(feature = "perf-tracing")] parent: tracing::Span,
) {
    {
        // 只观察已有后台；首次状态查询单独计时，不包含常驻轮询。
        #[cfg(feature = "perf-tracing")]
        let _parent = parent.enter();
        jchtools_foundation::perf_span!("gui_startup_system_pressure_check");
        let result = pressure::status();
        #[cfg(feature = "perf-tracing")]
        tracing::info!(
            target: "perf",
            event = "gui_startup_background_checked",
            component = "system_pressure",
            ok = result.is_ok(),
        );
        if sender
            .send(Message {
                action: false,
                result,
            })
            .is_err()
        {
            return;
        }
    }
    #[cfg(feature = "perf-tracing")]
    drop(parent);
    loop {
        let message = match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(command) => Message {
                action: true,
                result: match command {
                    Command::Start => pressure::start(),
                    Command::Stop => pressure::stop(),
                },
            },
            Err(mpsc::RecvTimeoutError::Timeout) => Message {
                action: false,
                result: pressure::status(),
            },
            // GUI 释放通道后仅结束观察，不停止独立采集进程。
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if sender.send(message).is_err() {
            return;
        }
    }
}

fn send_action(ui: &AppWindow, sender: &mpsc::Sender<Command>, command: Command) {
    if ui.get_system_pressure_pending() {
        return;
    }
    ui.set_system_pressure_error("".into());
    if sender.send(command).is_ok() {
        ui.set_system_pressure_pending(true);
    } else {
        ui.set_system_pressure_status("无法确认实际运行状态".into());
        ui.set_system_pressure_error("系统压力记录观察线程不可用".into());
    }
}

pub(super) fn wire_system_pressure(ui: &AppWindow) -> slint::Timer {
    let (commands, receiver) = mpsc::channel();
    let (sender, messages) = mpsc::sync_channel(2);
    #[cfg(feature = "perf-tracing")]
    let parent = tracing::Span::current();
    {
        jchtools_foundation::perf_span!("gui_startup_system_pressure_dispatch");
        std::thread::spawn(move || {
            observe(
                &sender,
                &receiver,
                #[cfg(feature = "perf-tracing")]
                parent,
            );
        });
    }
    let output_path = pressure::output_path();
    ui.set_system_pressure_path(output_path.display().to_string().into());
    ui.set_system_pressure_status("正在查询实际运行状态…".into());
    {
        let weak = ui.as_weak();
        let commands = commands.clone();
        ui.on_system_pressure_start(move || {
            if let Some(ui) = weak.upgrade() {
                send_action(&ui, &commands, Command::Start);
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_system_pressure_stop(move || {
            if let Some(ui) = weak.upgrade() {
                send_action(&ui, &commands, Command::Stop);
            }
        });
    }

    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    let mut action_error = None;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(150),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            for message in messages.try_iter() {
                if message.action {
                    ui.set_system_pressure_pending(false);
                    action_error = message.result.as_ref().err().cloned();
                }
                match message.result {
                    Ok(snapshot) => {
                        ui.set_system_pressure_running(snapshot.running);
                        ui.set_system_pressure_status(
                            match (snapshot.running, snapshot.error.is_some()) {
                                (true, false) => "正在记录 · 独立后台运行中",
                                (true, true) => "记录异常 · 独立后台仍在运行",
                                (false, false) => "未运行 · 记录已停止",
                                (false, true) => "记录已停止 · 请查看错误",
                            }
                            .into(),
                        );
                        ui.set_system_pressure_error(
                            snapshot
                                .error
                                .or_else(|| action_error.clone())
                                .map_or_else(String::new, |error| {
                                    format!("{}：{error}", output_path.display())
                                })
                                .into(),
                        );
                    }
                    Err(error) => {
                        ui.set_system_pressure_status("无法确认实际运行状态".into());
                        ui.set_system_pressure_error(
                            format!("{}：{error}", output_path.display()).into(),
                        );
                    }
                }
            }
        },
    );
    timer
}
