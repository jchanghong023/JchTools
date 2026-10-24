use super::{
    callbacks, create_session, disconnected, error, finish_retired, mpsc, reap_retired, watch,
    Actor, Agent, AgentExit, AgentProcessHandle, Arc, BackendCommand, Callbacks,
    ClientCapabilities, ConnectionTo, FileSystemCapabilities, FuturesUnordered, HashMap, HashSet,
    InitializeRequest, Instrument, Mutex, Path, PathBuf, PoisonError, ProtocolVersion,
    ServiceError, ServiceErrorKind, ServicePhase, ServiceStatus, SessionState, StreamExt, VecDeque,
};

async fn initialize(
    cx: &ConnectionTo<Agent>,
    workspace: PathBuf,
    callbacks: &Callbacks,
) -> Result<SessionState, ServiceError> {
    crate::acp_api::diagnostics::async_call("acp_agent", "initialize", async {
        let init = cx
            .send_request(
                InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                    ClientCapabilities::new()
                        .fs(FileSystemCapabilities::new()
                            .read_text_file(true)
                            .write_text_file(true))
                        .terminal(true),
                ),
            )
            .block_task()
            .await
            .map_err(|failure| {
                tracing::error!(
                    event = "acp_protocol_failed",
                    component = "acp_agent",
                    stage = "initialize_response",
                    error_type = "acp_protocol_error",
                    error_code = i32::from(failure.code),
                    "ACP 初始化协议请求失败"
                );
                error(
                    ServiceErrorKind::InvalidConfig,
                    "Agent ACP 协议初始化请求失败",
                )
            })?;
        if init.protocol_version != ProtocolVersion::V1 {
            return Err(error(
                ServiceErrorKind::InvalidConfig,
                "Agent 协商的 ACP 协议版本不受支持",
            ));
        }
        create_session(cx, workspace, callbacks, None)
            .await
            .map_err(|_| {
                error(
                    ServiceErrorKind::InvalidConfig,
                    "Agent 模型探测会话创建失败",
                )
            })
    })
    .await
}

// 只观察真实后台的退役集合；生产编译没有 observer、额外命令或公开 test-hooks API。
#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
pub(super) type RetirementObserver = Box<
    dyn FnMut(
            &[tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>],
            Option<&Callbacks>,
        ) + Send,
>;

// run_backend_inner 跨连接重建循环共享的可变状态；各阶段函数按 &mut 接收，
// 不再让 actor/retired/commands 等横跨连接闭包与等待循环的多个层级。
struct BackendRun {
    actor: Actor,
    commands: mpsc::UnboundedReceiver<BackendCommand>,
    commands_closed: bool,
    retired: Vec<tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>>,
    retired_failure: Option<ServiceError>,
    // 本连接内 foreground 已完成的收尾结果；每次重建连接前重置。
    foreground_shutdown: Option<Result<(), ServiceError>>,
    #[cfg(test)]
    observer: Option<RetirementObserver>,
}

// 探测阶段的出口：已初始化（进入稳态轮次），或停止分支已自行收尾并携带连接终值。
enum ProbeOutcome {
    Initialized(SessionState),
    Stopped(Result<(), agent_client_protocol::Error>),
}

pub(super) async fn run_backend(
    processes: AgentProcessHandle,
    workspace: PathBuf,
    status: watch::Sender<ServiceStatus>,
    commands: mpsc::UnboundedReceiver<BackendCommand>,
) {
    run_backend_inner(
        processes,
        workspace,
        status,
        commands,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    )
    .await;
}

// 初始化探测阶段：30 秒超时内完成 initialize 与模型探测，期间处理停止收尾、
// 封口 ACK 与 Agent 退出；成功返回探测会话，停止分支返回 callbacks.shutdown 的结果。
async fn wait_probe_initialized(
    run: &mut BackendRun,
    cx: &ConnectionTo<Agent>,
    callbacks: &Callbacks,
    workspace: &Path,
    exit: &mut watch::Receiver<Option<AgentExit>>,
    peer_pid: u32,
    startup_failure: &mut Option<ServiceError>,
) -> Result<ProbeOutcome, agent_client_protocol::Error> {
    let startup = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        initialize(cx, workspace.to_path_buf(), callbacks),
    );
    tokio::pin!(startup);
    loop {
        reap_retired(&mut run.retired, &mut run.retired_failure);
        if run.actor.stopping {
            let result = callbacks.shutdown().await;
            run.foreground_shutdown = Some(
                result
                    .as_ref()
                    .copied()
                    .map_err(|_| error(ServiceErrorKind::Io, "ACP 自有回调或终端资源收尾失败")),
            );
            return Ok(ProbeOutcome::Stopped(result));
        }
        if run.actor.sealed && run.actor.requests.is_empty() {
            for reply in run.actor.acknowledgements.drain(..) {
                let _ = reply.send(Ok(()));
            }
        }
        if exit.borrow().is_some() {
            return Err(callbacks::io_error("Agent 初始化期间退出"));
        }
        tokio::select! {
            result = &mut startup => match result {
                Ok(Ok(probe)) => return Ok(ProbeOutcome::Initialized(probe)),
                Ok(Err(failure)) => {
                    tracing::error!(event = "acp_agent_initialize_failed", component = "acp_agent", stage = "initialize_or_probe", error_type = ?failure.kind, peer_pid, "ACP Agent 初始化或模型探测失败");
                    *startup_failure = Some(failure); return Err(callbacks::io_error("Agent ACP 初始化失败"));
                }
                Err(_) => {
                    tracing::error!(event = "acp_agent_initialize_failed", component = "acp_agent", stage = "initialize_timeout", error_type = "timeout", timeout_ms = 30000, peer_pid, "ACP Agent 初始化及模型探测超时");
                    *startup_failure = Some(error(ServiceErrorKind::InvalidConfig, "Agent ACP 初始化及模型探测超时（30 秒）")); return Err(callbacks::io_error("Agent ACP 初始化超时"));
                }
            },
            () = cx.incoming_closed() => return Err(callbacks::io_error("Agent 初始化输出流已关闭")),
            changed = exit.changed() => { if changed.is_err() || exit.borrow().is_some() { return Err(callbacks::io_error("Agent 初始化期间退出")); } },
            command = run.commands.recv(), if !run.commands_closed => {
                if let Some(command) = command {
                    run.actor.command(command);
                } else {
                    run.commands_closed = true;
                    run.actor.sealed = true;
                    run.actor.stopping = true;
                }
            },
        }
    }
}

// 稳态轮次阶段：调度排队请求并处理命令、退出与轮次完成；封口且空闲时收尾返回
//（返回值即连接终值），其余仅在 Agent 退出或输出流关闭时失败返回。
async fn serve_rounds(
    run: &mut BackendRun,
    cx: &ConnectionTo<Agent>,
    callbacks: &Callbacks,
    exit: &mut watch::Receiver<Option<AgentExit>>,
    spare: &mut Option<SessionState>,
) -> Result<(), agent_client_protocol::Error> {
    let mut rounds = FuturesUnordered::new();
    loop {
        run.actor.schedule(cx, callbacks, spare, &mut rounds);
        reap_retired(&mut run.retired, &mut run.retired_failure);
        #[cfg(test)]
        if let Some(observer) = run.observer.as_mut() {
            observer(&run.retired, None);
        }
        if run.actor.sealed && run.actor.requests.is_empty() {
            if run.actor.stopping {
                // SDK 连接仍处理已知 handle 的管理回调，直到自有终端安全回收。
                let result = callbacks.shutdown().await;
                run.foreground_shutdown =
                    Some(result.as_ref().copied().map_err(|_| {
                        error(ServiceErrorKind::Io, "ACP 自有回调或终端资源收尾失败")
                    }));
                return result;
            }
            // 应用新配置先 drain；只 ACK 空闲，不关闭连接，随后 Stop 再退役。
            for reply in run.actor.acknowledgements.drain(..) {
                let _ = reply.send(Ok(()));
            }
        }
        if exit.borrow().is_some() {
            return Err(callbacks::io_error("Agent 已退出"));
        }
        tokio::select! {
            () = cx.incoming_closed() => return Err(callbacks::io_error("Agent 输出流已关闭")),
            changed = exit.changed() => { if changed.is_err() || exit.borrow().is_some() { return Err(callbacks::io_error("Agent 已退出")); } },
            command = run.commands.recv(), if !run.commands_closed => {
                if let Some(command) = command {
                    run.actor.command(command);
                } else {
                    run.commands_closed = true;
                    run.actor.sealed = true;
                    run.actor.stopping = true;
                    for request in run.actor.requests.values() {
                        request.cancellation.cancel();
                    }
                }
            },
            round = rounds.next(), if !rounds.is_empty() => if let Some(round) = round { run.actor.finish(round); },
        }
    }
}

// 连接失败交付：清空本连接的路由登记，并使全部在途请求立即失败。
fn deliver_connection_failure(run: &mut BackendRun, callbacks: &Callbacks, failure: &ServiceError) {
    {
        let mut routes = callbacks
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        routes.active.clear();
        routes.models.clear();
        routes.model_ids.clear();
        routes.workspaces.clear();
        routes.known.clear();
        routes.spans.clear();
    }
    run.actor.fail_all(failure);
}

pub(super) async fn run_backend_inner(
    processes: AgentProcessHandle,
    workspace: PathBuf,
    status: watch::Sender<ServiceStatus>,
    commands: mpsc::UnboundedReceiver<BackendCommand>,
    #[cfg(test)] observer: Option<RetirementObserver>,
    #[cfg(test)] setup: Option<fn(&Callbacks)>,
) {
    let mut run = BackendRun {
        actor: Actor {
            status,
            sessions: HashMap::new(),
            requests: HashMap::new(),
            queue: VecDeque::new(),
            active: HashSet::new(),
            models: Arc::new(Mutex::new(Err(error(
                ServiceErrorKind::NotReady,
                "Agent 尚未初始化",
            )))),
            sealed: false,
            stopping: false,
            acknowledgements: Vec::new(),
        },
        commands,
        commands_closed: false,
        retired: Vec::new(),
        retired_failure: None,
        foreground_shutdown: None,
        #[cfg(test)]
        observer,
    };
    let mut first = None;
    loop {
        reap_retired(&mut run.retired, &mut run.retired_failure);
        run.actor.phase(ServicePhase::Starting, None);
        run.actor.models = Arc::new(Mutex::new(Err(error(
            ServiceErrorKind::NotReady,
            "Agent 尚未初始化",
        ))));
        let failure = match processes.connect().await {
            Err(failure) => failure,
            Ok(connection) => {
                let callbacks = Callbacks::new();
                #[cfg(test)]
                if let Some(setup) = setup {
                    setup(&callbacks);
                }
                let mut exit = connection.exit;
                let mut initialized = false;
                let mut startup_failure = None;
                run.foreground_shutdown = None;
                let connected = callbacks::connect(
                    connection.transport,
                    callbacks.clone(),
                    async |cx: ConnectionTo<Agent>| {
                        let probe = match wait_probe_initialized(
                            &mut run,
                            &cx,
                            &callbacks,
                            &workspace,
                            &mut exit,
                            connection.pid,
                            &mut startup_failure,
                        )
                        .await
                        {
                            Ok(ProbeOutcome::Initialized(probe)) => probe,
                            // 停止分支已自行收尾；其结果就是本连接的最终结果。
                            Ok(ProbeOutcome::Stopped(result)) => return result,
                            Err(failure) => return Err(failure),
                        };
                        initialized = true;
                        run.actor.models = probe.model.clone();
                        let mut spare = Some(probe);
                        if let Some(command) = first.take() {
                            run.actor.command(command);
                        }
                        let externally_stopping = matches!(
                            run.actor.status.borrow().phase,
                            ServicePhase::Draining | ServicePhase::Stopping
                        );
                        if !run.actor.sealed && !externally_stopping {
                            match &*run
                                .actor
                                .models
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                            {
                                Ok(_) => run.actor.phase(ServicePhase::Ready, None),
                                Err(failure) => run.actor.phase(ServicePhase::Error, Some(failure)),
                            }
                        }
                        serve_rounds(&mut run, &cx, &callbacks, &mut exit, &mut spare).await
                    },
                )
                .await;
                let shutdown_completed = run.foreground_shutdown.is_some();
                let mut finish = match run.foreground_shutdown.take() {
                    Some(Err(failure)) => Err(failure),
                    // 本地收尾成功不掩盖 SDK/Agent 的真实连接失败。
                    Some(Ok(())) | None => connected.map_err(|_| {
                        if initialized {
                            disconnected()
                        } else {
                            startup_failure.unwrap_or_else(|| {
                                error(
                                    ServiceErrorKind::InvalidConfig,
                                    "Agent 初始化期间连接断裂或退出",
                                )
                            })
                        }
                    }),
                };
                // 通信断裂先交付失败；旧终端自然收尾异步托管，不阻塞模型查询或恢复。
                if let Err(failure) = &finish {
                    deliver_connection_failure(&mut run, &callbacks, failure);
                }
                if finish.is_err() && !run.actor.stopping && !shutdown_completed {
                    let cleanup = callbacks.clone();
                    run.retired.push(tokio::spawn(
                        async move { cleanup.shutdown().await }
                            .instrument(tracing::Span::current()),
                    ));
                    reap_retired(&mut run.retired, &mut run.retired_failure);
                    #[cfg(test)]
                    if let Some(observer) = run.observer.as_mut() {
                        observer(&run.retired, Some(&callbacks));
                    }
                } else {
                    if !shutdown_completed && callbacks.shutdown().await.is_err() && finish.is_ok()
                    {
                        finish = Err(error(
                            ServiceErrorKind::Io,
                            "ACP 自有回调或终端资源收尾失败",
                        ));
                    }
                    if let Err(failure) =
                        finish_retired(&mut run.retired, &mut run.retired_failure).await
                    {
                        if finish.is_ok() {
                            finish = Err(failure);
                        }
                    }
                }
                let _ = connection.finished.send(finish.clone());
                if run.actor.stopping {
                    if let Some(BackendCommand::Submit { reply, .. }) = first.take() {
                        let _ = reply
                            .send(Err(error(ServiceErrorKind::Stopping, "服务已停止接收请求")));
                    }
                    run.actor.status.send_modify(|s| {
                        s.executing = 0;
                        s.waiting = 0;
                    });
                    // Stop ACK 只代表 ACP 收尾；Stopped 由外层实际释放进程/HTTP 后发布。
                    run.actor
                        .phase(ServicePhase::Stopping, finish.as_ref().err());
                    for reply in run.actor.acknowledgements.drain(..) {
                        let _ = reply.send(finish.clone());
                    }
                    return;
                }
                if run.actor.sealed {
                    for reply in run.actor.acknowledgements.drain(..) {
                        let _ = reply.send(finish.clone());
                    }
                }
                finish.err().unwrap_or_else(disconnected)
            }
        };
        if let Some(BackendCommand::Submit { reply, .. }) = first.take() {
            let _ = reply.send(Err(failure.clone()));
        }
        run.actor.fail_all(&failure);
        // 不自动重放、不主动保活重建；仅下一新请求触发 D 单实例重建。
        if !await_reconnect_command(&mut run, &mut first).await {
            return;
        }
    }
}

// 失败后的等待阶段：不自动重放、不主动保活重建；新请求触发重建时返回 true，
// Stop 收尾或命令封口在等待中终局时返回 false（后台任务结束）。
async fn await_reconnect_command(run: &mut BackendRun, first: &mut Option<BackendCommand>) -> bool {
    loop {
        reap_retired(&mut run.retired, &mut run.retired_failure);
        #[cfg(test)]
        if let Some(observer) = run.observer.as_mut() {
            observer(&run.retired, None);
        }
        match run.commands.recv().await {
            Some(command @ BackendCommand::Submit { .. }) => {
                let old = match &command {
                    BackendCommand::Submit { input, .. } => {
                        input.session.as_ref().is_some_and(|key| {
                            run.actor.sessions.get(key).is_some_and(|slot| slot.invalid)
                        })
                    }
                    _ => false,
                };
                if run.actor.sealed {
                    run.actor.command(command);
                } else if old {
                    if let BackendCommand::Submit { reply, .. } = command {
                        let _ = reply.send(Err(error(
                            ServiceErrorKind::SessionUnavailable,
                            "Agent 重建前的会话已失效，须使用新会话",
                        )));
                    }
                } else {
                    *first = Some(command);
                    return true;
                }
            }
            Some(BackendCommand::GracefulDrain { reply }) => {
                run.actor.sealed = true;
                run.actor.phase(ServicePhase::Draining, None);
                let _ = reply.send(Ok(()));
            }
            Some(BackendCommand::Stop { reply }) => {
                run.actor.phase(ServicePhase::Stopping, None);
                let result = finish_retired(&mut run.retired, &mut run.retired_failure).await;
                run.actor
                    .phase(ServicePhase::Stopping, result.as_ref().err());
                let _ = reply.send(result);
                return false;
            }
            Some(command) => run.actor.command(command),
            None => {
                let _ = finish_retired(&mut run.retired, &mut run.retired_failure).await;
                return false;
            }
        }
    }
}
