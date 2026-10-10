//! AH-12：唯一 Agent 子进程所有者；协议断裂后不重放请求。
use crate::acp_api::{
    agent_process_channel, AgentConnection, AgentExit, AgentProcessCommand, AgentProcessHandle,
    ServiceConfig, ServiceError, ServiceErrorKind, ServiceStatus,
};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, watch};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

pub(super) struct ProcessOwner {
    pub(super) handle: AgentProcessHandle,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), ServiceError>>,
}
impl ProcessOwner {
    pub(super) fn start(
        config: ServiceConfig,
        workspace: PathBuf,
        status: watch::Sender<ServiceStatus>,
    ) -> Self {
        let (handle, mut commands) = agent_process_channel();
        let (shutdown, mut stopping) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut deferred = None;
            loop {
                let command = tokio::select! {
                    biased;
                    _ = &mut stopping => return Ok(()),
                    command = async {
                        match deferred.take() {
                            Some(command) => Some(command),
                            None => commands.recv().await,
                        }
                    } => match command { Some(command) => command, None => return Ok(()) },
                };
                let AgentProcessCommand::Connect { reply } = command;
                if reply.is_closed() {
                    continue;
                }
                let mut command = tokio::process::Command::new(&config.executable);
                command
                    .args(&config.arguments)
                    .current_dir(&workspace)
                    // AH-16：只覆盖自有 Agent 的继承环境，避免后端 OMP 再发现本服务。
                    .env("OMP_JCHTOOLS_DISCOVERY", "0")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                #[cfg(windows)]
                command.creation_flags(0x0800_0000);
                let mut child = match command.spawn() {
                    Ok(child) => child,
                    Err(error) => {
                        let _ = reply.send(Err(ServiceError::new(
                            ServiceErrorKind::Io,
                            format!("启动 ACP Agent 失败：{error}"),
                        )));
                        continue;
                    }
                };
                let pid = child.id().ok_or_else(|| {
                    ServiceError::new(ServiceErrorKind::Internal, "Agent 子进程缺少 PID")
                })?;
                status.send_modify(|state| state.agent_pid = Some(pid));
                let stdin = child.stdin.take().ok_or_else(|| {
                    ServiceError::new(ServiceErrorKind::Internal, "Agent 缺少标准输入")
                })?;
                let stdout = child.stdout.take().ok_or_else(|| {
                    ServiceError::new(ServiceErrorKind::Internal, "Agent 缺少标准输出")
                })?;
                let stderr_task = child.stderr.take().map(|mut stderr| tokio::spawn(async move {
                    // 只消费字节防止管道堵塞；Agent stderr 可能包含提示/账号信息，不记录正文。
                    let mut buffer = [0_u8; 4096];
                    loop {
                        match stderr.read(&mut buffer).await {
                            Ok(0) => break,
                            Ok(_) => {},
                            Err(error) => { tracing::warn!(kind = ?error.kind(), "ACP Agent stderr 读取失败"); break; }
                        }
                    }
                }));
                let (exit, receiver) = watch::channel(None);
                let (finished, mut connection_finished) = oneshot::channel();
                let connection = AgentConnection {
                    transport: agent_client_protocol::ByteStreams::new(
                        stdin.compat_write(),
                        stdout.compat(),
                    ),
                    pid,
                    exit: receiver,
                    finished,
                };
                let _ = reply.send(Ok(connection));
                // 仅未承接任务的初始化失败可终止；正常 Stop 不自动强杀。
                let (result, stopped) = tokio::select! {
                    result = child.wait() => (result, false),
                    completion = &mut connection_finished => {
                        match completion {
                            Ok(Ok(())) => (child.wait().await, false),
                            Ok(Err(failure)) if failure.kind == ServiceErrorKind::InvalidConfig => {
                                match child.start_kill() {
                                    Ok(()) => (child.wait().await, false),
                                    Err(error) => {
                                        let failure = ServiceError::new(ServiceErrorKind::Io,
                                            format!("无法退役初始化失败的自有 Agent：{error}"));
                                        wait_retiring(&mut child, &mut commands, &mut stopping, &status, pid, failure, &mut deferred).await
                                    },
                                }
                            },
                            Ok(Err(failure)) => {
                                wait_retiring(&mut child, &mut commands, &mut stopping, &status, pid, failure, &mut deferred).await
                            },
                            Err(_) => {
                                let failure = ServiceError::new(ServiceErrorKind::AgentDisconnected, "SDK 连接所有者异常退出");
                                wait_retiring(&mut child, &mut commands, &mut stopping, &status, pid, failure, &mut deferred).await
                            },
                        }
                    },
                    _ = &mut stopping => (child.wait().await, true),
                };
                publish_exit(&exit, &status, &result);
                if let Some(task) = stderr_task {
                    // Child 已退出；不因其后代继承 stderr 而无限等待日志管道 EOF。
                    task.abort();
                    let _ = task.await;
                }
                let result = result.map_err(|error| {
                    ServiceError::new(
                        ServiceErrorKind::Io,
                        format!("等待 Agent 退出失败：{error}"),
                    )
                })?;
                if stopped {
                    return Ok(());
                }
                tracing::debug!(pid, code = ?result.code(), "ACP Agent 已退役");
            }
        });
        Self {
            handle,
            shutdown: Some(shutdown),
            task,
        }
    }
    pub(super) async fn shutdown(mut self) -> Result<(), ServiceError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.await.map_err(|e| {
            ServiceError::new(
                ServiceErrorKind::Internal,
                format!("Agent 所有者任务失败：{e}"),
            )
        })?
    }
}
/// 无法证明断裂 Agent 的内部工具可安全终止时保留唯一所有权；
/// 新 Connect 明确失败，不挂起 admission，也不冒险启动第二进程。
async fn wait_retiring(
    child: &mut tokio::process::Child,
    commands: &mut tokio::sync::mpsc::UnboundedReceiver<AgentProcessCommand>,
    stopping: &mut oneshot::Receiver<()>,
    status: &watch::Sender<ServiceStatus>,
    pid: u32,
    failure: ServiceError,
    deferred: &mut Option<AgentProcessCommand>,
) -> (std::io::Result<std::process::ExitStatus>, bool) {
    let failure = ServiceError::new(
        ServiceErrorKind::AgentDisconnected,
        format!(
            "{}；自有 Agent（PID {pid}）仍在安全退役，退出前不能重建连接",
            failure.message
        ),
    );
    status.send_modify(|state| {
        state.phase = crate::acp_api::ServicePhase::Error;
        state.error = Some(failure.message.clone());
    });
    let mut stopped = false;
    loop {
        tokio::select! {
            biased;
            result = child.wait() => return (result, stopped),
            command = commands.recv(), if !stopped => {
                match command {
                    Some(command @ AgentProcessCommand::Connect { .. }) => {
                        // OS 已确认退出时，不能因异步 wait 唤醒稍晚而误拒绝这一新请求。
                        // 保留原命令：先发布原进程退出，再由外层唯一所有者启动下一进程。
                        match child.try_wait() {
                            Ok(Some(result)) => {
                                *deferred = Some(command);
                                return (Ok(result), stopped);
                            }
                            Ok(None) => {
                                let AgentProcessCommand::Connect { reply } = command;
                                let _ = reply.send(Err(failure.clone()));
                            }
                            Err(error) => {
                                let AgentProcessCommand::Connect { reply } = command;
                                let _ = reply.send(Err(ServiceError::new(ServiceErrorKind::Io,
                                    format!("确认退役 Agent 退出失败：{error}"))));
                            }
                        }
                    },
                    None => stopped = true,
                }
            },
            _ = &mut *stopping, if !stopped => stopped = true,
        }
    }
}

fn publish_exit(
    exit: &watch::Sender<Option<AgentExit>>,
    status: &watch::Sender<ServiceStatus>,
    result: &std::io::Result<std::process::ExitStatus>,
) {
    exit.send_replace(Some(match result {
        Ok(result) => AgentExit {
            code: result.code(),
            error: None,
        },
        Err(error) => AgentExit {
            code: None,
            error: Some(format!("等待 Agent 退出失败：{error}")),
        },
    }));
    status.send_modify(|state| state.agent_pid = None);
}
