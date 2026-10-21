use super::{
    callbacks, create_session, disconnected, error, finish_retired, mpsc, reap_retired, watch,
    Actor, Agent, AgentProcessHandle, Arc, BackendCommand, Callbacks, ClientCapabilities,
    ConnectionTo, FileSystemCapabilities, FuturesUnordered, HashMap, HashSet, InitializeRequest,
    Instrument, Mutex, PathBuf, PoisonError, ProtocolVersion, ServiceError, ServiceErrorKind,
    ServicePhase, ServiceStatus, SessionState, StreamExt, VecDeque,
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

pub(super) async fn run_backend_inner(
    processes: AgentProcessHandle,
    workspace: PathBuf,
    status: watch::Sender<ServiceStatus>,
    mut commands: mpsc::UnboundedReceiver<BackendCommand>,
    #[cfg(test)] mut observer: Option<RetirementObserver>,
    #[cfg(test)] setup: Option<fn(&Callbacks)>,
) {
    let mut actor = Actor {
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
    };
    let mut first = None;
    let mut commands_closed = false;
    let mut retired = Vec::new();
    let mut retired_failure = None;
    loop {
        reap_retired(&mut retired, &mut retired_failure);
        actor.phase(ServicePhase::Starting, None);
        actor.models = Arc::new(Mutex::new(Err(error(
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
                let mut foreground_shutdown: Option<Result<(), ServiceError>> = None;
                let connected = callbacks::connect(connection.transport, callbacks.clone(), async |cx: ConnectionTo<Agent>| {
                    let startup = tokio::time::timeout(std::time::Duration::from_secs(30), initialize(&cx, workspace.clone(), &callbacks));
                    tokio::pin!(startup);
                    let probe = loop {
                        reap_retired(&mut retired, &mut retired_failure);
                        if actor.stopping {
                            let result = callbacks.shutdown().await;
                            foreground_shutdown = Some(result.as_ref().copied().map_err(|_| {
                                error(ServiceErrorKind::Io, "ACP 自有回调或终端资源收尾失败")
                            }));
                            return result;
                        }
                        if actor.sealed && actor.requests.is_empty() {
                            for reply in actor.acknowledgements.drain(..) { let _ = reply.send(Ok(())); }
                        }
                        if exit.borrow().is_some() { return Err(callbacks::io_error("Agent 初始化期间退出")); }
                        tokio::select! {
                            result = &mut startup => match result {
                                Ok(Ok(probe)) => break probe,
                                Ok(Err(failure)) => {
                                    tracing::error!(event = "acp_agent_initialize_failed", component = "acp_agent", stage = "initialize_or_probe", error_type = ?failure.kind, peer_pid = connection.pid, "ACP Agent 初始化或模型探测失败");
                                    startup_failure = Some(failure); return Err(callbacks::io_error("Agent ACP 初始化失败"));
                                }
                                Err(_) => {
                                    tracing::error!(event = "acp_agent_initialize_failed", component = "acp_agent", stage = "initialize_timeout", error_type = "timeout", timeout_ms = 30000, peer_pid = connection.pid, "ACP Agent 初始化及模型探测超时");
                                    startup_failure = Some(error(ServiceErrorKind::InvalidConfig, "Agent ACP 初始化及模型探测超时（30 秒）")); return Err(callbacks::io_error("Agent ACP 初始化超时"));
                                }
                            },
                            () = cx.incoming_closed() => return Err(callbacks::io_error("Agent 初始化输出流已关闭")),
                            changed = exit.changed() => { if changed.is_err() || exit.borrow().is_some() { return Err(callbacks::io_error("Agent 初始化期间退出")); } },
                            command = commands.recv(), if !commands_closed => {
                                if let Some(command) = command {
                                    actor.command(command);
                                } else {
                                    commands_closed = true;
                                    actor.sealed = true;
                                    actor.stopping = true;
                                }
                            },
                        }
                    };
                    initialized = true;
                    actor.models = probe.model.clone();
                    let mut spare = Some(probe);
                    if let Some(command) = first.take() { actor.command(command); }
                    let externally_stopping = matches!(actor.status.borrow().phase, ServicePhase::Draining | ServicePhase::Stopping);
                    if !actor.sealed && !externally_stopping {
                        match &*actor.models.lock().unwrap_or_else(PoisonError::into_inner) { Ok(_) => actor.phase(ServicePhase::Ready, None), Err(failure) => actor.phase(ServicePhase::Error, Some(failure)) }
                    }
                    let mut rounds = FuturesUnordered::new();
                    loop {
                        actor.schedule(&cx, &callbacks, &mut spare, &mut rounds);
                        reap_retired(&mut retired, &mut retired_failure);
                        #[cfg(test)]
                        if let Some(observer) = observer.as_mut() {
                            observer(&retired, None);
                        }
                        if actor.sealed && actor.requests.is_empty() {
                            if actor.stopping {
                                // SDK 连接仍处理已知 handle 的管理回调，直到自有终端安全回收。
                                let result = callbacks.shutdown().await;
                                foreground_shutdown = Some(result.as_ref().copied().map_err(|_| {
                                    error(ServiceErrorKind::Io, "ACP 自有回调或终端资源收尾失败")
                                }));
                                return result;
                            }
                            // 应用新配置先 drain；只 ACK 空闲，不关闭连接，随后 Stop 再退役。
                            for reply in actor.acknowledgements.drain(..) { let _ = reply.send(Ok(())); }
                        }
                        if exit.borrow().is_some() { return Err(callbacks::io_error("Agent 已退出")); }
                        tokio::select! {
                            () = cx.incoming_closed() => return Err(callbacks::io_error("Agent 输出流已关闭")),
                            changed = exit.changed() => { if changed.is_err() || exit.borrow().is_some() { return Err(callbacks::io_error("Agent 已退出")); } },
                            command = commands.recv(), if !commands_closed => {
                                if let Some(command) = command {
                                    actor.command(command);
                                } else {
                                    commands_closed = true;
                                    actor.sealed = true;
                                    actor.stopping = true;
                                    for request in actor.requests.values() {
                                        request.cancellation.cancel();
                                    }
                                }
                            },
                            round = rounds.next(), if !rounds.is_empty() => if let Some(round) = round { actor.finish(round); },
                        }
                    }
                }).await;
                let shutdown_completed = foreground_shutdown.is_some();
                let mut finish = match foreground_shutdown {
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
                    actor.fail_all(failure);
                }
                if finish.is_err() && !actor.stopping && !shutdown_completed {
                    let cleanup = callbacks.clone();
                    retired.push(tokio::spawn(
                        async move { cleanup.shutdown().await }
                            .instrument(tracing::Span::current()),
                    ));
                    reap_retired(&mut retired, &mut retired_failure);
                    #[cfg(test)]
                    if let Some(observer) = observer.as_mut() {
                        observer(&retired, Some(&callbacks));
                    }
                } else {
                    if !shutdown_completed && callbacks.shutdown().await.is_err() && finish.is_ok()
                    {
                        finish = Err(error(
                            ServiceErrorKind::Io,
                            "ACP 自有回调或终端资源收尾失败",
                        ));
                    }
                    if let Err(failure) = finish_retired(&mut retired, &mut retired_failure).await {
                        if finish.is_ok() {
                            finish = Err(failure);
                        }
                    }
                }
                let _ = connection.finished.send(finish.clone());
                if actor.stopping {
                    if let Some(BackendCommand::Submit { reply, .. }) = first.take() {
                        let _ = reply
                            .send(Err(error(ServiceErrorKind::Stopping, "服务已停止接收请求")));
                    }
                    actor.status.send_modify(|s| {
                        s.executing = 0;
                        s.waiting = 0;
                    });
                    // Stop ACK 只代表 ACP 收尾；Stopped 由外层实际释放进程/HTTP 后发布。
                    actor.phase(ServicePhase::Stopping, finish.as_ref().err());
                    for reply in actor.acknowledgements.drain(..) {
                        let _ = reply.send(finish.clone());
                    }
                    return;
                }
                if actor.sealed {
                    for reply in actor.acknowledgements.drain(..) {
                        let _ = reply.send(finish.clone());
                    }
                }
                finish.err().unwrap_or_else(disconnected)
            }
        };
        if let Some(BackendCommand::Submit { reply, .. }) = first.take() {
            let _ = reply.send(Err(failure.clone()));
        }
        actor.fail_all(&failure);
        // 不自动重放、不主动保活重建；仅下一新请求触发 D 单实例重建。
        loop {
            reap_retired(&mut retired, &mut retired_failure);
            #[cfg(test)]
            if let Some(observer) = observer.as_mut() {
                observer(&retired, None);
            }
            match commands.recv().await {
                Some(command @ BackendCommand::Submit { .. }) => {
                    let old = match &command {
                        BackendCommand::Submit { input, .. } => {
                            input.session.as_ref().is_some_and(|key| {
                                actor.sessions.get(key).is_some_and(|slot| slot.invalid)
                            })
                        }
                        _ => false,
                    };
                    if actor.sealed {
                        actor.command(command);
                    } else if old {
                        if let BackendCommand::Submit { reply, .. } = command {
                            let _ = reply.send(Err(error(
                                ServiceErrorKind::SessionUnavailable,
                                "Agent 重建前的会话已失效，须使用新会话",
                            )));
                        }
                    } else {
                        first = Some(command);
                        break;
                    }
                }
                Some(BackendCommand::GracefulDrain { reply }) => {
                    actor.sealed = true;
                    actor.phase(ServicePhase::Draining, None);
                    let _ = reply.send(Ok(()));
                }
                Some(BackendCommand::Stop { reply }) => {
                    actor.phase(ServicePhase::Stopping, None);
                    let result = finish_retired(&mut retired, &mut retired_failure).await;
                    actor.phase(ServicePhase::Stopping, result.as_ref().err());
                    let _ = reply.send(result);
                    return;
                }
                Some(command) => actor.command(command),
                None => {
                    let _ = finish_retired(&mut retired, &mut retired_failure).await;
                    return;
                }
            }
        }
    }
}
