//! AH-06～AH-10：后台服务进程内生命周期状态机与 --acp-http-service 入口装配。
use super::ipc::{self, ControlOperation};
use super::{process, runtime};
use crate::acp_api::{
    settings, BackendHandle, ServiceConfig, ServiceError, ServiceErrorKind, ServicePhase,
    ServiceStatus,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub(super) struct Running {
    pub(super) backend: BackendHandle,
    pub(super) process: process::ProcessOwner,
    pub(super) http_shutdown: CancellationToken,
    pub(super) http: tokio::task::JoinHandle<Result<(), ServiceError>>,
}
impl Running {
    async fn start(
        config: ServiceConfig,
        state: watch::Sender<ServiceStatus>,
        cancellation: &CancellationToken,
    ) -> Result<Self, ServiceError> {
        crate::acp_api::diagnostics::async_call("acp_runtime", "start", async { settings::validate_config(&config)?;
    let workspace = settings::workspace_dir()?;
    // 先绑定精确端口，失败不启动 Agent，不隐式寻找空闲端口。
    tracing::debug!(event = "acp_http_bind_started", component = "acp_runtime", port = config.port, host = "127.0.0.1", "ACP HTTP 本机端口监听开始");
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, config.port))
        .await
        .map_err(|e| {
            tracing::error!(event = "acp_http_bind_failed", component = "acp_runtime", stage = "bind", port = config.port, error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP HTTP 本机端口监听失败");
            ipc::error(format!(
                "端口 {}：无法监听 127.0.0.1（端口占用或无权限）：{e}",
                config.port
            ))
        })?;
    tracing::info!(event = "acp_http_bind_completed", component = "acp_runtime", port = config.port, "ACP HTTP 本机端口监听完成");
    if cancellation.is_cancelled() {
        return Err(stopping());
    }
    state.send_modify(|state| {
        state.phase = ServicePhase::Starting;
        state.running_config = Some(config.clone());
        state.error = None;
    });
    let process = process::ProcessOwner::start(config, workspace.clone(), state.clone());
    let backend =
        crate::acp_api::acp::start_backend(process.handle.clone(), workspace, state.clone());
    let router = crate::acp_api::http::router(backend.clone());
    let http_shutdown = CancellationToken::new();
    let shutdown = http_shutdown.clone();
    let http = tokio::spawn(async move {
        let result = crate::acp_api::http::serve(listener, router, shutdown).await;
        if let Err(error) = &result {
            state.send_modify(|state| {
                state.phase = ServicePhase::Error;
                state.error = Some(error.message.clone());
            });
        }
        result
    }.instrument(tracing::Span::current()));
    Ok(Self {
        backend,
        process,
        http_shutdown,
        http,
    }) }).await
    }
    async fn finish(self, cancellation: &CancellationToken) -> Result<(), ServiceError> {
        crate::acp_api::diagnostics::async_call("acp_runtime", "finish", async {
            // 排空和取消共用同一所有者；Stop 可抢占等待，但不能丢弃安全收尾。
            let drain = async {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => self.backend.stop().await,
                    result = self.backend.graceful_drain() => {
                        let stop = self.backend.stop().await;
                        result.and(stop)
                    },
                }
            };
            tokio::pin!(drain);
            // 先 poll 后端控制封闭 admission，再关闭 HTTP 新连接，不等 drain ACK。
            let drain = tokio::select! {
                biased;
                result = &mut drain => { self.http_shutdown.cancel(); result },
                () = async { self.http_shutdown.cancel(); } => drain.await,
            };
            // 即使后端回报断裂，仍等待自身进程及全部已接 HTTP 资源实际释放。
            let process = self.process.shutdown().await;
            let http = self.http.await.map_err(|e| {
                ServiceError::new(
                    ServiceErrorKind::Internal,
                    format!("HTTP 后台任务失败：{e}"),
                )
            })?;
            drain.and(process).and(http)
        })
        .await
    }
}
fn failed(state: &watch::Sender<ServiceStatus>, error: &ServiceError) {
    tracing::error!(event = "acp_service_failed", component = "acp_runtime", stage = "lifecycle", error_type = ?error.kind, "ACP 模型服务生命周期失败");
    state.send_modify(|state| {
        state.phase = ServicePhase::Error;
        state.error = Some(error.message.clone());
    });
}
fn stopping() -> ServiceError {
    ServiceError::new(ServiceErrorKind::Stopping, "服务正在停止，应用已取消")
}

async fn transition(
    operation: ControlOperation,
    running: &mut Option<Running>,
    state: &watch::Sender<ServiceStatus>,
    cancellation: &CancellationToken,
) -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::async_call("acp_runtime", "transition", async { if matches!(operation, ControlOperation::Apply) && !cancellation.is_cancelled() {
    let config = settings::load_config()?.ok_or_else(|| {
        ServiceError::new(ServiceErrorKind::InvalidConfig, "尚未配置 ACP Agent")
    })?;
    state.send_modify(|state| {
        state.saved_config = Some(config.clone());
        state.phase = ServicePhase::Draining;
    });
    if let Some(old) = running.take() {
        if let Err(error) = old.finish(cancellation).await {
            failed(state, &error);
            return Err(error);
        }
    }
    if !cancellation.is_cancelled() {
        state.send_modify(|state| {
            state.running_config = None;
            state.agent_pid = None;
            state.executing = 0;
            state.waiting = 0;
        });
        match Running::start(config, state.clone(), cancellation).await {
            Ok(new) => {
                *running = Some(new);
                // AH-09：保留初始化和 HTTP 失败状态；Stop 同样可抢占初始化等待。
                let mut initialized = state.subscribe();
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {},
                    result = initialized.wait_for(|state| state.phase != ServicePhase::Starting) => {
                        return result
                            .map(|state| state.clone())
                            .map_err(|_| ipc::error("应用后模型服务初始化状态已关闭"));
                    },
                }
            }
            Err(error) if error.kind == ServiceErrorKind::Stopping => {}
            Err(error) => {
                failed(state, &error);
                return Err(error);
            }
        }
    }
}
state.send_modify(|state| state.phase = ServicePhase::Stopping);
if let Some(old) = running.take() {
    if let Err(error) = old.finish(cancellation).await {
        failed(state, &error);
        return Err(error);
    }
}
state.send_modify(|state| {
    state.phase = ServicePhase::Stopped;
    state.running_config = None;
    state.service_pid = None;
    state.agent_pid = None;
    state.executing = 0;
    state.waiting = 0;
    state.error = None;
});
Ok(state.borrow().clone()) }).await
}

async fn controller(
    config: Option<ServiceConfig>,
    state: watch::Sender<ServiceStatus>,
    commands: mpsc::UnboundedReceiver<ipc::Control>,
) {
    let running = match config {
        Some(config) => {
            match Running::start(config, state.clone(), &CancellationToken::new()).await {
                Ok(running) => Some(running),
                Err(error) => {
                    failed(&state, &error);
                    None
                }
            }
        }
        None => None,
    };
    control_loop(running, state, commands).await;
}

pub(super) async fn control_loop(
    mut running: Option<Running>,
    state: watch::Sender<ServiceStatus>,
    mut commands: mpsc::UnboundedReceiver<ipc::Control>,
) {
    let mut queued = std::collections::VecDeque::new();
    let mut closed = false;
    while let Some(control) = match queued.pop_front() {
        Some(control) => Some(control),
        None => commands.recv().await,
    } {
        let applying = matches!(control.operation, ControlOperation::Apply);
        let cancellation = CancellationToken::new();
        if !applying {
            cancellation.cancel();
            state.send_modify(|state| state.phase = ServicePhase::Stopping);
        }
        let mut stop_replies = Vec::new();
        let result = {
            // 只在控制器内 poll：生命周期未来持有旧/新实例，退出必须等它完整收尾。
            let transition = transition(control.operation, &mut running, &state, &cancellation)
                .instrument(control.span.clone());
            tokio::pin!(transition);
            loop {
                tokio::select! {
                    biased;
                    command = commands.recv(), if !closed => {
                        if let Some(command) = command {
                            match command.operation {
                                ControlOperation::Stop => {
                                    cancellation.cancel();
                                    state.send_modify(|state| state.phase = ServicePhase::Stopping);
                                    stop_replies.push(command.reply);
                                },
                                ControlOperation::Apply => {
                                    if cancellation.is_cancelled() {
                                        command.span.in_scope(|| tracing::warn!(event = "acp_control_operation_cancelled", component = "acp_runtime", stage = "admission", reason = "stopping", "ACP 服务停止中，拒绝应用配置"));
                                        let _ = command.reply.send(Err(stopping()));
                                    } else {
                                        command.span.in_scope(|| tracing::debug!(event = "acp_control_operation_queued", component = "acp_runtime", "ACP 应用配置操作进入生命周期队列"));
                                        queued.push_back(command);
                                    }
                                },
                            }
                        } else {
                            closed = true;
                            cancellation.cancel();
                            state.send_modify(|state| state.phase = ServicePhase::Stopping);
                        }
                    },
                    result = &mut transition => break result,
                }
            }
        };
        let interrupted = cancellation.is_cancelled();
        let reply = if applying && interrupted && result.is_ok() {
            Err(stopping())
        } else {
            result.clone()
        };
        let _ = control.reply.send(reply);
        for reply in stop_replies {
            let _ = reply.send(result.clone());
        }
        if closed || interrupted {
            // 封口后禁止消费排队的 Apply 并启动新 Agent；每个 Control 明确完成。
            for control in queued {
                let _ = control.reply.send(Err(stopping()));
            }
            while let Some(control) = commands.recv().await {
                let reply = match control.operation {
                    ControlOperation::Apply => Err(stopping()),
                    ControlOperation::Stop => result.clone(),
                };
                let _ = control.reply.send(reply);
            }
            return;
        }
    }
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    if let Some(old) = running {
        let _ = old.finish(&cancellation).await;
    }
}
/// 主程序内部 --acp-http-service 入口；整个后台生命周期独立于 GUI。
pub fn serve() -> Result<(), ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "serve", || {
        let Some(instance) = ipc::lock("service", false)? else {
            return Ok(());
        };
        // 后台专用运行时不在 serve 返回时析构：退休后的认证 pipe 要覆盖 main 刷盘，
        // 直至 OS 关闭进程。局部/UT IPC 仍使用可正常关闭的独立运行时。
        static SERVICE_RUNTIME: std::sync::LazyLock<Result<tokio::runtime::Runtime, ServiceError>> =
            std::sync::LazyLock::new(runtime);
        let rt = SERVICE_RUNTIME.as_ref().map_err(Clone::clone)?;
        // mutex 与后台进程同寿命；不在资源排空或日志刷盘之前暴露新的单实例窗口。
        // Windows 在所有者线程退出时回收，维护仍以认证 process handle 等 OS signaled。
        let _instance = std::mem::ManuallyDrop::new(instance);
        rt.block_on(async {
    let loaded = settings::load_config();
    let mut initial = ServiceStatus {
        service_pid: Some(std::process::id()),
        ..ServiceStatus::default()
    };
    let config = match loaded {
        Ok(config) => {
            initial.saved_config.clone_from(&config);
            initial.phase = if config.is_some() {
                ServicePhase::Starting
            } else {
                ServicePhase::Unconfigured
            };
            config
        }
        Err(error) => {
            initial.phase = ServicePhase::Error;
            initial.error = Some(error.message);
            None
        }
    };
    tracing::info!(event = "acp_service_initialized", component = "acp_runtime", phase = ?initial.phase, configured = initial.saved_config.is_some(), "ACP 模型服务后台初始状态已建立");
    let (state, receiver) = watch::channel(initial);
    let (commands, requests) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let (ready, started) = oneshot::channel();
    let instance_id = uuid::Uuid::new_v4();
    let (finished, drained) = oneshot::channel();
    let mut ipc_task = tokio::spawn(ipc::serve_for_process(receiver, instance_id, commands, shutdown, ready, finished).instrument(tracing::Span::current()));
    started
        .await
        .map_err(|_| ipc::error("模型服务控制管道启动失败"))??;
    let controller = tokio::spawn(controller(config, state, requests).instrument(tracing::Span::current()));
    let ipc_result = tokio::select! {
        result = &mut ipc_task => result
            .map_err(|e| ipc::error(format!("模型服务控制任务失败：{e}")))?,
        result = drained => result
            .map_err(|_| ipc::error("模型服务退出观察入口异常关闭")),
    };
    controller
        .await
        .map_err(|e| ipc::error(format!("模型服务生命周期任务失败：{e}")))?;
    ipc_result
})
    })
}
