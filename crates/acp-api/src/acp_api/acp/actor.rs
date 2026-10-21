use super::{
    error, mpsc, oneshot, round, validate_history, watch, AcceptedRequest, Agent, Arc,
    BackendCommand, BoxFuture, Callbacks, CancellationToken, ChatMessage, ConnectionTo, FutureExt,
    FuturesUnordered, HashMap, HashSet, Instrument, Mutex, PathBuf, PoisonError, PromptInput,
    RequestEvent, RequestId, ServiceError, ServiceErrorKind, ServicePhase, ServiceStatus,
    SessionConfigId, SessionId, SessionKey, SessionModel, VecDeque, MAX_EXECUTING, MAX_WAITING,
};

pub(super) struct SessionState {
    pub(super) id: SessionId,
    pub(super) cwd: PathBuf,
    pub(super) model: SessionModel,
    pub(super) history: Vec<ChatMessage>,
}
pub(super) struct SessionSlot {
    pub(super) cwd: PathBuf,
    pub(super) state: Option<SessionState>,
    pub(super) invalid: bool,
}
pub(super) struct Pending {
    pub(super) key: SessionKey,
    pub(super) input: Option<PromptInput>,
    pub(super) events: mpsc::UnboundedSender<RequestEvent>,
    pub(super) started: std::time::Instant,
    pub(super) cancellation: CancellationToken,
}
pub(super) struct RoundContext {
    pub(super) id: RequestId,
    pub(super) key: SessionKey,
    pub(super) input: PromptInput,
    pub(super) state: Option<SessionState>,
    pub(super) negotiated_model_id: Option<SessionConfigId>,
    pub(super) events: mpsc::UnboundedSender<RequestEvent>,
    pub(super) cancellation: CancellationToken,
}
pub(super) struct RoundResult {
    pub(super) id: RequestId,
    pub(super) key: SessionKey,
    pub(super) state: Option<SessionState>,
    pub(super) terminal: RequestEvent,
}
pub(super) struct Actor {
    pub(super) status: watch::Sender<ServiceStatus>,
    pub(super) sessions: HashMap<SessionKey, SessionSlot>,
    pub(super) requests: HashMap<RequestId, Pending>,
    pub(super) queue: VecDeque<RequestId>,
    pub(super) active: HashSet<SessionKey>,
    // 未消费的探测会话是 catalog 真源；分配给首轮时才与会话状态分离。
    pub(super) models: SessionModel,
    pub(super) sealed: bool,
    pub(super) stopping: bool,
    pub(super) acknowledgements: Vec<oneshot::Sender<Result<(), ServiceError>>>,
}
impl Actor {
    fn counts(&self) {
        self.status.send_modify(|s| {
            s.executing = self.active.len();
            s.waiting = self.queue.len();
        });
    }
    pub(super) fn phase(&self, phase: ServicePhase, failure: Option<&ServiceError>) {
        self.status.send_modify(|s| {
            // 外层生命周期还要等待 Agent 和 HTTP 资源；Actor 不能降级排空/停止阶段。
            let phase = match s.phase {
                ServicePhase::Stopping => ServicePhase::Stopping,
                ServicePhase::Draining if phase != ServicePhase::Stopping => ServicePhase::Draining,
                _ => phase,
            };
            tracing::info!(event = "acp_backend_phase_changed", component = "acp_agent", previous = ?s.phase, current = ?phase, error_type = ?failure.map(|error| error.kind), "ACP 后台阶段变更");
            s.phase = phase;
            s.error = failure.map(|e| e.message.clone());
        });
    }
    pub(super) fn command(&mut self, command: BackendCommand) {
        match command {
            BackendCommand::Models { reply } => {
                let _ = reply.send(if self.sealed {
                    Err(error(ServiceErrorKind::Stopping, "服务已停止接收请求"))
                } else {
                    self.models
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                        .map(|selector| selector.values.clone())
                        .map_err(Clone::clone)
                });
            }
            BackendCommand::Submit {
                request_id,
                input,
                events,
                cancellation,
                reply,
            } => {
                let span = jchtools_foundation::logging::operation_span_with_id(
                    "acp_agent",
                    "admission",
                    &request_id.0,
                );
                let _entered = span.enter();
                let result = self.admit(request_id.clone(), input, events, cancellation);
                let _ = reply.send(result);
            }
            BackendCommand::Cancel { request_id } => {
                tracing::info!(event = "acp_request_cancel_received", component = "acp_agent", request_id = %request_id.0, known = self.requests.contains_key(&request_id), "ACP 后台收到请求取消");
                if let Some(request) = self.requests.get(&request_id) {
                    request.cancellation.cancel();
                }
            }
            BackendCommand::GracefulDrain { reply } => {
                self.sealed = true;
                self.acknowledgements.push(reply);
                if !self.stopping {
                    self.phase(ServicePhase::Draining, None);
                }
            }
            BackendCommand::Stop { reply } => {
                self.sealed = true;
                self.stopping = true;
                self.acknowledgements.push(reply);
                for request in self.requests.values() {
                    request.cancellation.cancel();
                }
                self.phase(ServicePhase::Stopping, None);
            }
        }
    }
    pub(super) fn admit(
        &mut self,
        id: RequestId,
        input: PromptInput,
        events: mpsc::UnboundedSender<RequestEvent>,
        cancellation: CancellationToken,
    ) -> Result<AcceptedRequest, ServiceError> {
        crate::acp_api::diagnostics::call("acp_agent", "admit", || {
            if self.sealed {
                return Err(error(ServiceErrorKind::Stopping, "服务已停止接收请求"));
            }
            if cancellation.is_cancelled() {
                return Err(error(ServiceErrorKind::Cancelled, "请求已取消"));
            }
            if input.messages.is_empty() {
                return Err(error(ServiceErrorKind::InvalidRequest, "messages 不得为空"));
            }
            let key = input
                .session
                .clone()
                .unwrap_or_else(|| SessionKey(uuid::Uuid::new_v4().to_string()));
            if let Some(slot) = self.sessions.get(&key) {
                if slot.cwd != input.cwd {
                    return Err(error(
                        ServiceErrorKind::SessionUnavailable,
                        "此会话已绑定其他工作目录",
                    ));
                }
                if slot.invalid {
                    return Err(error(
                        ServiceErrorKind::SessionUnavailable,
                        "此会话已失效，须使用新会话",
                    ));
                }
                if !self.active.contains(&key) {
                    if let Some(state) = &slot.state {
                        validate_history(&state.history, &input.messages)?;
                    }
                }
            }
            // 续会话以它当前的选择列表准入；在途会话的状态由上一轮完成后再检查。
            let available = if let Some(slot) = self.sessions.get(&key) {
                slot.state.as_ref().map(|state| {
                    state
                        .model
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                        .map(|selector| selector.values.iter().any(|model| model.id == input.model))
                        .map_err(Clone::clone)
                })
            } else {
                Some(
                    self.models
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                        .map(|selector| selector.values.iter().any(|model| model.id == input.model))
                        .map_err(Clone::clone),
                )
            };
            if let Some(available) = available {
                if !available? {
                    return Err(error(
                        ServiceErrorKind::ModelUnavailable,
                        "请求 model 不在 Agent 可选择列表中",
                    ));
                }
            }
            let immediately_running = self.active.len() < MAX_EXECUTING
                && !self.active.contains(&key)
                && !self
                    .queue
                    .iter()
                    .any(|queued| self.requests.get(queued).is_some_and(|r| r.key == key));
            if !immediately_running && self.queue.len() >= MAX_WAITING {
                return Err(error(
                    ServiceErrorKind::QueueFull,
                    "请求等待队列已满（最多 16 个）",
                ));
            }
            let accepted = AcceptedRequest {
                request_id: id.clone(),
                session: key.clone(),
                model: input.model.clone(),
            };
            self.sessions.entry(key.clone()).or_insert(SessionSlot {
                cwd: input.cwd.clone(),
                state: None,
                invalid: false,
            });
            self.requests.insert(
                id.clone(),
                Pending {
                    key,
                    input: Some(input),
                    events,
                    cancellation,
                    started: std::time::Instant::now(),
                },
            );
            self.queue.push_back(id);
            tracing::debug!(event = "acp_request_queued", component = "acp_agent", request_id = %accepted.request_id.0, executing = self.active.len(), waiting = self.queue.len(), "ACP 请求进入调度队列");
            Ok(accepted)
        })
    }
    pub(super) fn fail_all(&mut self, failure: &ServiceError) {
        for (id, request) in self.requests.drain() {
            tracing::error!(event = "acp_request_failed", component = "acp_agent", request_id = %id.0, stage = "connection", error_type = ?failure.kind, elapsed_ms = jchtools_foundation::logging::elapsed_ms(request.started), "ACP 连接故障交付请求失败");
            let _ = request.events.send(RequestEvent::Failed(failure.clone()));
        }
        self.queue.clear();
        self.active.clear();
        for slot in self.sessions.values_mut() {
            slot.state = None;
            slot.invalid = true;
        }
        *self.models.lock().unwrap_or_else(PoisonError::into_inner) = Err(failure.clone());
        self.phase(ServicePhase::Error, Some(failure));
        self.counts();
    }
    pub(super) fn finish(&mut self, round: RoundResult) {
        self.active.remove(&round.key);
        if let Some(request) = self.requests.remove(&round.id) {
            match &round.terminal {
                RequestEvent::Completed { reason } => {
                    tracing::info!(event = "acp_request_completed", component = "acp_agent", request_id = %round.id.0, result = ?reason, elapsed_ms = jchtools_foundation::logging::elapsed_ms(request.started), "ACP 请求轮次完成");
                }
                RequestEvent::Failed(error) => {
                    tracing::error!(event = "acp_request_failed", component = "acp_agent", request_id = %round.id.0, stage = "round", error_type = ?error.kind, elapsed_ms = jchtools_foundation::logging::elapsed_ms(request.started), "ACP 请求轮次失败");
                }
                RequestEvent::Cancelled => {
                    tracing::warn!(event = "acp_request_cancelled", component = "acp_agent", request_id = %round.id.0, stage = "round", elapsed_ms = jchtools_foundation::logging::elapsed_ms(request.started), "ACP 请求轮次取消");
                }
                RequestEvent::TextDelta(_) => {}
            }
            let _ = request.events.send(round.terminal);
        }
        if let Some(slot) = self.sessions.get_mut(&round.key) {
            slot.invalid = round.state.is_none();
            slot.state = round.state;
        }
        self.counts();
    }
    pub(super) fn schedule(
        &mut self,
        cx: &ConnectionTo<Agent>,
        callbacks: &Callbacks,
        spare: &mut Option<SessionState>,
        rounds: &mut FuturesUnordered<BoxFuture<'static, RoundResult>>,
    ) {
        let mut index = 0;
        while index < self.queue.len() {
            let Some(id) = self.queue.get(index).cloned() else {
                break;
            };
            let Some(request) = self.requests.get(&id) else {
                self.queue.remove(index);
                continue;
            };
            if request.cancellation.is_cancelled() || request.events.is_closed() {
                self.queue.remove(index);
                if let Some(request) = self.requests.remove(&id) {
                    tracing::warn!(event = "acp_request_cancelled", component = "acp_agent", request_id = %id.0, stage = "queued", elapsed_ms = jchtools_foundation::logging::elapsed_ms(request.started), "ACP 排队请求已取消");
                    let _ = request.events.send(RequestEvent::Cancelled);
                }
                continue;
            }
            if self.active.len() >= MAX_EXECUTING || self.active.contains(&request.key) {
                index += 1;
                continue;
            }
            let key = request.key.clone();
            if self.sessions.get(&key).is_some_and(|slot| slot.invalid) {
                self.queue.remove(index);
                if let Some(request) = self.requests.remove(&id) {
                    tracing::error!(event = "acp_request_failed", component = "acp_agent", request_id = %id.0, stage = "queued_session", error_type = "session_unavailable", elapsed_ms = jchtools_foundation::logging::elapsed_ms(request.started), "ACP 排队请求会话已失效");
                    let _ = request.events.send(RequestEvent::Failed(error(
                        ServiceErrorKind::SessionUnavailable,
                        "此会话已失效，未重放等待请求",
                    )));
                }
                continue;
            }
            self.queue.remove(index);
            let Some(request) = self.requests.get_mut(&id) else {
                continue;
            };
            let Some(input) = request.input.take() else {
                continue;
            };
            let mut state = self
                .sessions
                .get_mut(&key)
                .and_then(|slot| slot.state.take());
            if state.is_none() && spare.as_ref().is_some_and(|probe| probe.cwd == input.cwd) {
                if let Some(probe) = spare.take() {
                    // 仅同工作目录的首轮可接管探测会话；其余会话从 catalog 重新协商。
                    let catalog = probe
                        .model
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    self.models = Arc::new(Mutex::new(catalog));
                    state = Some(probe);
                }
            }
            let negotiated_model_id = if state.is_none() {
                self.models
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .ok()
                    .map(|selector| selector.id.clone())
            } else {
                None
            };
            self.active.insert(key.clone());
            let span =
                jchtools_foundation::logging::operation_span_with_id("acp_agent", "round", &id.0);
            tracing::debug!(event = "acp_request_execution_started", component = "acp_agent", request_id = %id.0, reused_session = state.is_some(), queue_elapsed_ms = jchtools_foundation::logging::elapsed_ms(request.started), "ACP 排队请求开始执行");
            rounds.push(
                round(
                    cx.clone(),
                    callbacks.clone(),
                    RoundContext {
                        id,
                        key,
                        input,
                        state,
                        negotiated_model_id,
                        events: request.events.clone(),
                        cancellation: request.cancellation.clone(),
                    },
                )
                .instrument(span)
                .boxed(),
            );
        }
        self.counts();
    }
}
