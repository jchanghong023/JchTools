//! AH-06～AH-10：独立后台装配；GUI facade 只在既有 worker 上调用。
mod ipc;
mod process;

use super::{
    settings, BackendHandle, ServiceConfig, ServiceDiscovery, ServiceError, ServiceErrorKind,
    ServicePhase, ServiceStatus,
};
use ipc::{ControlOperation, Operation};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

fn runtime() -> Result<tokio::runtime::Runtime, ServiceError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| ipc::error(format!("创建模型服务运行时失败：{e}")))
}
fn inactive() -> Result<ServiceStatus, ServiceError> {
    let saved_config = settings::load_config()?;
    Ok(ServiceStatus {
        phase: if saved_config.is_some() {
            ServicePhase::Stopped
        } else {
            ServicePhase::Unconfigured
        },
        saved_config,
        ..ServiceStatus::default()
    })
}
/// 查询只使用私有管道，绝不自动拉起服务或探测 HTTP 端口。
pub fn status() -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "status", || {
        runtime()?
            .block_on(ipc::exchange(Operation::Status))?
            .map_or_else(inactive, Ok)
    })
}
/// AH-15：只查询已有后台；缺席时不读取配置，也不启动后台或 Agent。
pub fn discover() -> Result<Option<ServiceDiscovery>, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "discover", || {
        runtime()?.block_on(ipc::exchange(Operation::Discover))
    })
}
pub fn ensure_started() -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "ensure_started", || {
        let rt = runtime()?;
        if let Some(state) = rt.block_on(ipc::exchange(Operation::Status))? {
            return Ok(state);
        }
        let config = settings::load_config()?;
        if config.is_none() {
            return inactive();
        }
        // 同一用户/会话所有 GUI 使用独立启动锁；服务另持 lifetime lock。
        let _startup =
            ipc::lock("startup", true)?.ok_or_else(|| ipc::error("等待模型服务启动锁超时"))?;
        if let Some(state) = rt.block_on(ipc::exchange(Operation::Status))? {
            return Ok(state);
        }
        let mut executable = std::env::current_exe()
            .map_err(|e| ipc::error(format!("定位 JchTools 程序失败：{e}")))?;
        if cfg!(any(test, feature = "test-hooks")) {
            if let Some(path) = std::env::var_os("JCHTOOLS_TEST_ACP_SERVICE_EXE") {
                executable = std::path::PathBuf::from(path);
                if !executable.is_absolute() || !executable.is_file() {
                    return Err(ipc::error("测试服务程序必须为存在的绝对路径"));
                }
            }
        }
        let mut command = std::process::Command::new(executable);
        command
            .arg("--acp-http-service")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let started = Instant::now();
        tracing::info!(
            event = "acp_service_spawn_started",
            component = "acp_runtime",
            executable_role = "jchtools_service",
            timeout_ms = 15000,
            "ACP 模型服务后台启动开始"
        );
        let mut service = command
    .spawn()
    .map_err(|e| {
        tracing::error!(event = "acp_service_spawn_failed", component = "acp_runtime", stage = "spawn", error_type = ?e.kind(), error_code = ?e.raw_os_error(), elapsed_ms = crate::logging::elapsed_ms(started), "ACP 模型服务后台启动失败");
        ipc::error(format!("启动模型服务后台失败：{e}"))
    })?;
        tracing::info!(
            event = "acp_service_spawn_completed",
            component = "acp_runtime",
            peer_pid = service.id(),
            elapsed_ms = crate::logging::elapsed_ms(started),
            "ACP 模型服务后台子进程已启动"
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(state) = rt.block_on(ipc::exchange::<ServiceStatus>(Operation::Status))? {
                tracing::info!(event = "acp_service_control_ready", component = "acp_runtime", peer_pid = service.id(), phase = ?state.phase, elapsed_ms = crate::logging::elapsed_ms(started), "ACP 模型服务后台控制管道已就绪");
                return Ok(state);
            }
            if let Some(exit) = service.try_wait().map_err(|e| ipc::error(e.to_string()))? {
                tracing::error!(event = "acp_service_start_failed", component = "acp_runtime", peer_pid = service.id(), stage = "child_exit", exit_code = ?exit.code(), elapsed_ms = crate::logging::elapsed_ms(started), "ACP 模型服务后台启动期间退出");
                return Err(ipc::error(format!("模型服务后台启动时退出：{exit}")));
            }
            if Instant::now() >= deadline {
                tracing::error!(
                    event = "acp_service_start_failed",
                    component = "acp_runtime",
                    peer_pid = service.id(),
                    stage = "control_ready_timeout",
                    timeout_ms = 15000,
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "ACP 模型服务后台控制管道就绪超时"
                );
                return Err(ipc::error("模型服务后台未及时建立控制管道"));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    })
}
pub fn save_config(config: &ServiceConfig) -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "save_config", || {
        settings::save_config(config)?;
        if let Some(state) = runtime()?.block_on(ipc::exchange(Operation::Status))? {
            return Ok(state);
        }
        ensure_started()
    })
}
pub fn apply_and_restart() -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "apply_and_restart", || {
        // Apply 只作用已存在的后台；退出后的迟到请求不能反向创建新实例。
        runtime()?
            .block_on(ipc::exchange(Operation::Apply))?
            .ok_or_else(|| {
                ServiceError::new(
                    ServiceErrorKind::NotReady,
                    "模型服务后台已退出，无法应用配置",
                )
            })
    })
}
pub fn stop() -> Result<ServiceStatus, ServiceError> {
    crate::acp_api::diagnostics::call("acp_runtime", "stop", || {
        runtime()?
            .block_on(ipc::exchange(Operation::Stop))?
            .map_or_else(inactive, Ok)
    })
}

struct Running {
    backend: BackendHandle,
    process: process::ProcessOwner,
    http_shutdown: CancellationToken,
    http: tokio::task::JoinHandle<Result<(), ServiceError>>,
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
    tracing::info!(event = "acp_http_bind_started", component = "acp_runtime", port = config.port, host = "127.0.0.1", "ACP HTTP 本机端口监听开始");
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

async fn control_loop(
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
                                        command.span.in_scope(|| tracing::info!(event = "acp_control_operation_queued", component = "acp_runtime", "ACP 应用配置操作进入生命周期队列"));
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
        let Some(_instance) = ipc::lock("service", false)? else {
            return Ok(());
        };
        runtime()?.block_on(async {
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
    let ipc_task = tokio::spawn(ipc::serve(receiver, instance_id, commands, shutdown, ready).instrument(tracing::Span::current()));
    started
        .await
        .map_err(|_| ipc::error("模型服务控制管道启动失败"))??;
    let controller = tokio::spawn(controller(config, state, requests).instrument(tracing::Span::current()));
    let ipc_result = ipc_task
        .await
        .map_err(|e| ipc::error(format!("模型服务控制任务失败：{e}")))?;
    controller
        .await
        .map_err(|e| ipc::error(format!("模型服务生命周期任务失败：{e}")))?;
    ipc_result
})
    })
}

#[cfg(all(test, windows))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::acp_api::{backend_channel, BackendCommand};
    use std::ffi::OsString;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, MutexGuard,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct IsolatedRoot {
        _lock: MutexGuard<'static, ()>,
        _temp: tempfile::TempDir,
        previous: Option<OsString>,
    }

    impl IsolatedRoot {
        fn new() -> Self {
            let lock = crate::asset_util::test_env::env_lock()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let temp = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("JCHTOOLS_TEST_STATE_DIR");
            std::env::set_var("JCHTOOLS_TEST_STATE_DIR", temp.path().join("state"));
            Self {
                _lock: lock,
                _temp: temp,
                previous,
            }
        }
    }

    impl Drop for IsolatedRoot {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("JCHTOOLS_TEST_STATE_DIR", value),
                None => std::env::remove_var("JCHTOOLS_TEST_STATE_DIR"),
            }
        }
    }

    fn instance_available_from_other_thread() -> bool {
        // Windows mutexes are recursive on the owning thread; another thread must probe.
        std::thread::spawn(|| ipc::lock("service", false).unwrap().is_some())
            .join()
            .unwrap()
    }

    // Stop must retire the controller/pipe even when its error response is preserved.
    // Only the backend boundary is controlled: Running::finish, the process owner,
    // HTTP listener, controller and authenticated named-pipe transport are real.
    #[test]
    fn stop_backend_error_releases_controller_pipe_and_instance_after_resources_finish() {
        let _isolation = IsolatedRoot::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap();
                let address = listener.local_addr().unwrap();
                let config = ServiceConfig {
                    executable: std::env::var("ComSpec").expect("Windows command interpreter"),
                    arguments: vec!["/D".into(), "/C".into(), "exit 0".into()],
                    port: address.port(),
                };
                let (state, status) = watch::channel(ServiceStatus {
                    phase: ServicePhase::Ready,
                    running_config: Some(config.clone()),
                    service_pid: Some(std::process::id()),
                    ..ServiceStatus::default()
                });
                let process = process::ProcessOwner::start(
                    config,
                    settings::workspace_dir().unwrap(),
                    state.clone(),
                );
                let connection = tokio::time::timeout(
                    Duration::from_secs(3),
                    process.handle.connect(),
                )
                .await
                .expect("test-owned child must connect within the bound")
                .unwrap();
                let crate::acp_api::AgentConnection {
                    transport,
                    mut exit,
                    finished,
                    ..
                } = connection;
                drop(transport);
                // Natural exit may win the owner's select before this acknowledgement.
                let _ = finished.send(Ok(()));
                let exit_status = tokio::time::timeout(
                    Duration::from_secs(3),
                    exit.wait_for(Option::is_some),
                )
                .await
                .expect("test-owned child must exit naturally")
                .unwrap()
                .clone()
                .unwrap();
                assert_eq!(exit_status.code, Some(0));
                assert_eq!(exit_status.error, None);
                assert_eq!(state.borrow().agent_pid, None);

                let (backend, mut backend_commands) = backend_channel(status.clone());
                let backend_boundary = tokio::spawn(async move {
                    match backend_commands.recv().await {
                        Some(BackendCommand::Stop { reply }) => {
                            // The real BackendHandle maps a closed acknowledgement to
                            // AgentDisconnected; no product transition/result is mocked.
                            drop(reply);
                        }
                        command => panic!("Stop must reach the actual backend boundary: {command:?}"),
                    }
                });
                let http_shutdown = CancellationToken::new();
                let http_finished = Arc::new(AtomicBool::new(false));
                let completed = http_finished.clone();
                let shutdown = http_shutdown.clone();
                let http_backend = backend.clone();
                let http = tokio::spawn(async move {
                    let result = crate::acp_api::http::serve(
                        listener,
                        crate::acp_api::http::router(http_backend),
                        shutdown,
                    )
                    .await;
                    completed.store(true, Ordering::SeqCst);
                    result
                });
                // Prove the real HTTP task is serving, without issuing a backend command.
                let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                socket
                    .write_all(b"GET /test-owned-missing-route HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                let mut response = Vec::new();
                tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut response))
                    .await
                    .expect("real HTTP listener must answer")
                    .unwrap();
                assert!(response.starts_with(b"HTTP/1.1 404"));
                drop(socket);

                let running = Running {
                    backend,
                    process,
                    http_shutdown,
                    http,
                };
                let shutdown = CancellationToken::new();
                let controller_finished = Arc::new(AtomicBool::new(false));
                let completed = controller_finished.clone();
                let (done, mut finished) = oneshot::channel();
                let (published, listening) = oneshot::channel();
                let service_shutdown = shutdown.clone();
                let service = async {
                    let instance = ipc::lock("service", false).unwrap().unwrap();
                    let (commands, requests) = mpsc::unbounded_channel();
                    let (ready, started) = oneshot::channel();
                    let ipc_task = tokio::spawn(ipc::serve(
                        status,
                        uuid::Uuid::new_v4(),
                        commands,
                        service_shutdown,
                        ready,
                    ));
                    started.await.unwrap().unwrap();
                    let controller = tokio::spawn(async move {
                        control_loop(Some(running), state.clone(), requests).await;
                        completed.store(true, Ordering::SeqCst);
                    });
                    published.send(()).unwrap();
                    ipc_task.await.unwrap().unwrap();
                    controller.await.unwrap();
                    drop(instance);
                    let _ = done.send(());
                };
                let exercise = async {
                    listening.await.unwrap();
                    let failure = tokio::time::timeout(
                        Duration::from_secs(3),
                        ipc::exchange::<ServiceStatus>(Operation::Stop),
                    )
                    .await
                    .expect("Stop error must be returned after actual resource cleanup")
                    .expect_err("Stop must explicitly report the typed backend failure");
                    assert_eq!(failure.kind, ServiceErrorKind::AgentDisconnected);
                    assert!(http_finished.load(Ordering::SeqCst), "HTTP task must really finish");
                    let reclaimed_listener = tokio::net::TcpListener::bind(address).await.unwrap();
                    drop(reclaimed_listener);
                    backend_boundary.await.unwrap();

                    let natural_completion = matches!(
                        tokio::time::timeout(Duration::from_millis(500), &mut finished).await,
                        Ok(Ok(()))
                    );
                    let controller_retired = controller_finished.load(Ordering::SeqCst);
                    let pipe_absent = tokio::time::timeout(
                        Duration::from_secs(3),
                        ipc::exchange::<crate::acp_api::ServiceDiscovery>(Operation::Discover),
                    )
                    .await
                    .expect("post-Stop pipe probe must be bounded")
                    .unwrap()
                    .is_none();
                    let instance_released = instance_available_from_other_thread();
                    // Always release test-owned IPC/controller before asserting the red result.
                    shutdown.cancel();
                    if !natural_completion {
                        tokio::time::timeout(Duration::from_secs(3), &mut finished)
                            .await
                            .expect("test-owned service cleanup must be bounded")
                            .unwrap();
                    }
                    assert!(instance_available_from_other_thread());
                    assert!(ipc::exchange::<ServiceStatus>(Operation::Status).await.unwrap().is_none());

                    // Recreate the same lifetime lock and first pipe instance after cleanup.
                    let instance = ipc::lock("service", false).unwrap().unwrap();
                    let (new_state, new_status) = watch::channel(ServiceStatus::default());
                    let (commands, requests) = mpsc::unbounded_channel();
                    let (ready, started) = oneshot::channel();
                    let replacement = tokio::spawn(ipc::serve(
                        new_status,
                        uuid::Uuid::new_v4(),
                        commands,
                        CancellationToken::new(),
                        ready,
                    ));
                    started.await.unwrap().unwrap();
                    let controller = tokio::spawn(control_loop(None, new_state, requests));
                    let stopped = tokio::time::timeout(
                        Duration::from_secs(3),
                        ipc::exchange::<ServiceStatus>(Operation::Stop),
                    )
                    .await
                    .expect("replacement Stop must be bounded")
                    .unwrap()
                    .unwrap();
                    assert_eq!(stopped.phase, ServicePhase::Stopped);
                    tokio::time::timeout(Duration::from_secs(3), async {
                        replacement.await.unwrap().unwrap();
                        controller.await.unwrap();
                    })
                    .await
                    .expect("replacement service must naturally retire");
                    drop(instance);
                    assert!(
                        natural_completion && controller_retired && pipe_absent && instance_released,
                        "Stop Err must preserve its report AND retire after resource cleanup: \
                         service_completed={natural_completion}, controller_retired={controller_retired}, \
                         pipe_absent={pipe_absent}, instance_released={instance_released}"
                    );
                };
                tokio::time::timeout(Duration::from_secs(12), async {
                    tokio::join!(service, exercise);
                })
                .await
                .expect("Stop-error regression must have bounded cleanup");
            });
    }
}
