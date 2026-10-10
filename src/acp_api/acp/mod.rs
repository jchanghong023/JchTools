//! AH-02 / AH-04 / AH-12 / AH-13：单连接、请求级路由、4+16 调度与安全收尾。
//! 模型 catalog 与未消费的探测会话共享状态；首轮接管 spare 时冻结，避免会话配置污染新会话能力。
mod callbacks;
mod terminals;

use crate::acp_api::{
    backend_channel, AcceptedRequest, AgentProcessHandle, BackendCommand, BackendHandle,
    ChatMessage, FinishReason, MessageRole, ModelDescriptor, PromptInput, RequestEvent, RequestId,
    ServiceError, ServiceErrorKind, ServicePhase, ServiceStatus, SessionKey, MAX_EXECUTING,
    MAX_WAITING,
};
use agent_client_protocol::{
    schema::{
        v1::{
            CancelNotification, ClientCapabilities, ContentBlock, FileSystemCapabilities,
            InitializeRequest, NewSessionRequest, PromptRequest, SessionConfigId,
            SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
            SessionConfigSelectOption, SessionConfigSelectOptions, SessionConfigValueId, SessionId,
            SetSessionConfigOptionRequest, StopReason, TextContent,
        },
        ProtocolVersion,
    },
    Agent, ConnectionTo,
};
use callbacks::{Callbacks, Route};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

fn error(kind: ServiceErrorKind, message: &str) -> ServiceError {
    ServiceError::new(kind, message)
}
fn disconnected() -> ServiceError {
    error(
        ServiceErrorKind::AgentDisconnected,
        "Agent 连接已断开，本轮未重放",
    )
}
fn sdk_error(cx: &ConnectionTo<Agent>, _: agent_client_protocol::Error) -> ServiceError {
    if cx.is_incoming_closed() {
        disconnected()
    } else {
        error(ServiceErrorKind::Internal, "Agent ACP 请求失败")
    }
}

pub fn start_backend(
    processes: AgentProcessHandle,
    workspace: PathBuf,
    status: watch::Sender<ServiceStatus>,
) -> BackendHandle {
    let (handle, commands) = backend_channel(status.subscribe());
    tokio::spawn(run_backend(processes, workspace, status, commands));
    handle
}
#[derive(Clone)]
struct ModelSelector {
    id: SessionConfigId,
    values: Vec<ModelDescriptor>,
    current: String,
}
type SessionModel = Arc<Mutex<Result<ModelSelector, ServiceError>>>;
fn model_selector(options: &[SessionConfigOption]) -> Result<ModelSelector, ServiceError> {
    let option = options
        .iter()
        .find(|option| {
            option.category == Some(SessionConfigOptionCategory::Model)
                && matches!(option.kind, SessionConfigKind::Select(_))
        })
        .ok_or_else(|| {
            error(
                ServiceErrorKind::ModelsUnavailable,
                "Agent 未提供可选择的 Model/Select 配置能力",
            )
        })?;
    model_selector_from_option(option)
}
fn model_selector_from_option(option: &SessionConfigOption) -> Result<ModelSelector, ServiceError> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return Err(error(
            ServiceErrorKind::ModelsUnavailable,
            "Agent 模型配置类型不可用",
        ));
    };
    let mut values = Vec::new();
    let mut append = |value: &SessionConfigSelectOption| {
        let id = value.value.to_string();
        if !id.is_empty() && !values.iter().any(|model: &ModelDescriptor| model.id == id) {
            values.push(ModelDescriptor {
                id,
                name: value.name.clone(),
            });
        }
    };
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            for value in options {
                append(value);
            }
        }
        SessionConfigSelectOptions::Grouped(groups) => {
            for group in groups {
                for value in &group.options {
                    append(value);
                }
            }
        }
        _ => {
            return Err(error(
                ServiceErrorKind::ModelsUnavailable,
                "Agent 模型选项格式不受支持",
            ))
        }
    }
    let current = select.current_value.to_string();
    if values.is_empty() || !values.iter().any(|model| model.id == current) {
        return Err(error(
            ServiceErrorKind::ModelsUnavailable,
            "Agent 没有有效的可选择模型",
        ));
    }
    Ok(ModelSelector {
        id: option.id.clone(),
        values,
        current,
    })
}
struct SessionState {
    id: SessionId,
    model: SessionModel,
    history: Vec<ChatMessage>,
}
struct SessionSlot {
    state: Option<SessionState>,
    invalid: bool,
}
struct Pending {
    key: SessionKey,
    input: Option<PromptInput>,
    events: mpsc::UnboundedSender<RequestEvent>,
    cancellation: CancellationToken,
}
struct RoundContext {
    id: RequestId,
    key: SessionKey,
    input: PromptInput,
    state: Option<SessionState>,
    events: mpsc::UnboundedSender<RequestEvent>,
    cancellation: CancellationToken,
}
struct RoundResult {
    id: RequestId,
    key: SessionKey,
    state: Option<SessionState>,
    terminal: RequestEvent,
}
struct Actor {
    status: watch::Sender<ServiceStatus>,
    sessions: HashMap<SessionKey, SessionSlot>,
    requests: HashMap<RequestId, Pending>,
    queue: VecDeque<RequestId>,
    active: HashSet<SessionKey>,
    // 未消费的探测会话是 catalog 真源；分配给首轮时才与会话状态分离。
    models: SessionModel,
    sealed: bool,
    stopping: bool,
    acknowledgements: Vec<oneshot::Sender<Result<(), ServiceError>>>,
}
impl Actor {
    fn counts(&self) {
        self.status.send_modify(|s| {
            s.executing = self.active.len();
            s.waiting = self.queue.len();
        });
    }
    fn phase(&self, phase: ServicePhase, failure: Option<&ServiceError>) {
        self.status.send_modify(|s| {
            s.phase = phase;
            s.error = failure.map(|e| e.message.clone());
        });
    }
    fn command(&mut self, command: BackendCommand) {
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
                let result = self.admit(request_id.clone(), input, events, cancellation);
                let _ = reply.send(result);
            }
            BackendCommand::Cancel { request_id } => {
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
    fn admit(
        &mut self,
        id: RequestId,
        input: PromptInput,
        events: mpsc::UnboundedSender<RequestEvent>,
        cancellation: CancellationToken,
    ) -> Result<AcceptedRequest, ServiceError> {
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
            },
        );
        self.queue.push_back(id);
        Ok(accepted)
    }
    fn fail_all(&mut self, failure: &ServiceError) {
        for (_, request) in self.requests.drain() {
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
    fn finish(&mut self, round: RoundResult) {
        self.active.remove(&round.key);
        if let Some(request) = self.requests.remove(&round.id) {
            let _ = request.events.send(round.terminal);
        }
        if let Some(slot) = self.sessions.get_mut(&round.key) {
            slot.invalid = round.state.is_none();
            slot.state = round.state;
        }
        self.counts();
    }
    fn schedule(
        &mut self,
        cx: &ConnectionTo<Agent>,
        workspace: &Path,
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
            if state.is_none() {
                if let Some(probe) = spare.take() {
                    // 后续 set 回包/会话通知只更新已消费的会话，不污染新会话准入能力。
                    let catalog = probe
                        .model
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    self.models = Arc::new(Mutex::new(catalog));
                    state = Some(probe);
                }
            }
            self.active.insert(key.clone());
            rounds.push(
                round(
                    cx.clone(),
                    workspace.to_path_buf(),
                    callbacks.clone(),
                    RoundContext {
                        id,
                        key,
                        input,
                        state,
                        events: request.events.clone(),
                        cancellation: request.cancellation.clone(),
                    },
                )
                .boxed(),
            );
        }
        self.counts();
    }
}
fn validate_history(history: &[ChatMessage], messages: &[ChatMessage]) -> Result<(), ServiceError> {
    if messages.len() <= history.len() || !messages.starts_with(history) {
        Err(error(
            ServiceErrorKind::HistoryMismatch,
            "messages 必须包含此会话完整历史前缀和新增消息",
        ))
    } else {
        Ok(())
    }
}
async fn create_session(
    cx: &ConnectionTo<Agent>,
    workspace: PathBuf,
    callbacks: &Callbacks,
) -> Result<SessionState, ServiceError> {
    let (reply, response) = oneshot::channel();
    let routes = callbacks.routes.clone();
    let connection = cx.clone();
    cx.prepare_request(NewSessionRequest::new(workspace))
        .on_receiving_result(move |result| async move {
            // 发布前注册有序回调，在原始派发内登记，先于紧随响应的配置通知。
            let result = (|| {
                let response = result.map_err(|failure| sdk_error(&connection, failure))?;
                // SDK id 保留到本连接结束，拒绝复用取消会话 id，防止迟到更新串轮。
                let mut routes = routes.lock().unwrap_or_else(PoisonError::into_inner);
                if !routes.known.insert(response.session_id.clone()) {
                    return Err(error(
                        ServiceErrorKind::SessionUnavailable,
                        "Agent 重复返回了已使用的 ACP 会话标识",
                    ));
                }
                let options = response.config_options.as_deref().unwrap_or_default();
                let selector = match &routes.model_id {
                    Some(id) => options
                        .iter()
                        .find(|option| &option.id == id)
                        .ok_or_else(|| {
                            error(
                                ServiceErrorKind::ModelsUnavailable,
                                "Agent 未保留已协商模型配置",
                            )
                        })
                        .and_then(model_selector_from_option),
                    None => model_selector(options),
                };
                if routes.model_id.is_none() {
                    if let Ok(selector) = &selector {
                        routes.model_id = Some(selector.id.clone());
                    }
                }
                let model = Arc::new(Mutex::new(selector));
                routes
                    .models
                    .insert(response.session_id.clone(), model.clone());
                Ok(SessionState {
                    id: response.session_id,
                    model,
                    history: Vec::new(),
                })
            })();
            let _ = reply.send(result);
            Ok(())
        })
        .map_err(|failure| sdk_error(cx, failure))?;
    response.await.map_err(|_| disconnected())?
}
async fn round(
    cx: ConnectionTo<Agent>,
    workspace: PathBuf,
    callbacks: Callbacks,
    context: RoundContext,
) -> RoundResult {
    let RoundContext {
        id,
        key,
        input,
        state,
        events,
        cancellation,
    } = context;
    let mut state = match state {
        Some(state) => state,
        None => match create_session(&cx, workspace, &callbacks).await {
            Ok(state) => state,
            Err(failure) => {
                return RoundResult {
                    id,
                    key,
                    state: None,
                    terminal: RequestEvent::Failed(failure),
                }
            }
        },
    };
    // 尚未修改 ACP 的坏历史只拒绝本请求，不能销毁上一轮有效会话。
    if let Err(failure) = validate_history(&state.history, &input.messages) {
        return RoundResult {
            id,
            key,
            state: Some(state),
            terminal: RequestEvent::Failed(failure),
        };
    }
    let session = state.id.clone();
    let mut result = execute_round(&cx, &callbacks, &mut state, input, events, cancellation).await;
    callbacks
        .routes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .active
        .remove(&session);
    if callbacks.complete_round(&session).await.is_err() {
        result = Err(error(ServiceErrorKind::Io, "本轮 ACP 回调收尾失败"));
    }
    let (terminal, state) = match result {
        Ok(reason) => (RequestEvent::Completed { reason }, Some(state)),
        Err(failure) if failure.kind == ServiceErrorKind::Cancelled => {
            (RequestEvent::Cancelled, None)
        }
        Err(failure) => (RequestEvent::Failed(failure), None),
    };
    RoundResult {
        id,
        key,
        state,
        terminal,
    }
}
async fn execute_round(
    cx: &ConnectionTo<Agent>,
    callbacks: &Callbacks,
    state: &mut SessionState,
    input: PromptInput,
    events: mpsc::UnboundedSender<RequestEvent>,
    cancellation: CancellationToken,
) -> Result<FinishReason, ServiceError> {
    if cancellation.is_cancelled() {
        return Err(error(ServiceErrorKind::Cancelled, "请求已取消"));
    }
    let selector_id = {
        let model = state.model.lock().unwrap_or_else(PoisonError::into_inner);
        let selector = model.as_ref().map_err(Clone::clone)?;
        if !selector.values.iter().any(|model| model.id == input.model) {
            return Err(error(
                ServiceErrorKind::ModelUnavailable,
                "此会话无法选择请求模型",
            ));
        }
        selector.id.clone()
    };
    let (reply, response) = oneshot::channel();
    let model = state.model.clone();
    let requested_model = input.model.clone();
    let connection = cx.clone();
    cx.prepare_request(SetSessionConfigOptionRequest::new(
        state.id.clone(),
        selector_id.clone(),
        SessionConfigValueId::new(input.model.clone()),
    ))
    .on_receiving_result(move |result| async move {
        // 发布前注册有序回调，后到的 ConfigOptionUpdate 不会被旧回包覆盖。
        let result = (|| {
            let response = result.map_err(|failure| sdk_error(&connection, failure))?;
            // category 仅用于首次识别；后续完整响应按已协商 id 确认。
            let selected = response
                .config_options
                .iter()
                .find(|option| option.id == selector_id)
                .and_then(|option| model_selector_from_option(option).ok())
                .ok_or_else(|| {
                    error(
                        ServiceErrorKind::ModelUnavailable,
                        "Agent 模型设置响应未保留已协商的有效 Select 配置",
                    )
                })?;
            if selected.current != requested_model {
                return Err(error(
                    ServiceErrorKind::ModelUnavailable,
                    "Agent 未确认请求模型已生效",
                ));
            }
            *model.lock().unwrap_or_else(PoisonError::into_inner) = Ok(selected);
            Ok(())
        })();
        let _ = reply.send(result);
        Ok(())
    })
    .map_err(|failure| sdk_error(cx, failure))?;
    response.await.map_err(|_| disconnected())??;
    if cancellation.is_cancelled() {
        return Err(error(ServiceErrorKind::Cancelled, "请求已取消"));
    }
    let content = input.messages[state.history.len()..]
        .iter()
        .map(|message| {
            ContentBlock::Text(TextContent::new(format!(
                "[{}]\n{}",
                message.role.as_str(),
                message.text
            )))
        })
        .collect();
    callbacks
        .routes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .active
        .insert(
            state.id.clone(),
            Route {
                events,
                cancellation: cancellation.clone(),
                text: String::new(),
                failure: None,
            },
        );
    // 发布前注册有序终态回调，摘除路由后迟到通知不能进入新轮次。
    let (reply, mut response) = oneshot::channel();
    let routes = callbacks.routes.clone();
    let session = state.id.clone();
    cx.prepare_request(PromptRequest::new(session.clone(), content))
        .on_receiving_result(move |result| async move {
            let route = routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .active
                .remove(&session);
            let _ = reply.send((result, route));
            Ok(())
        })
        .map_err(|e| sdk_error(cx, e))?;
    let (result, route) = tokio::select! {
        result = &mut response => result.map_err(|_| disconnected())?,
        () = cancellation.cancelled() => {
            // ACP v1 session/cancel，与 SDK 通用 request cancellation 不混淆。
            cx.send_notification(CancelNotification::new(state.id.clone())).map_err(|e| sdk_error(cx, e))?;
            response.await.map_err(|_| disconnected())?
        }
    };
    let route = route.ok_or_else(disconnected)?;
    if let Some(failure) = route.failure {
        return Err(failure);
    }
    let response = result.map_err(|e| sdk_error(cx, e))?;
    if cancellation.is_cancelled() || response.stop_reason == StopReason::Cancelled {
        return Err(error(
            ServiceErrorKind::Cancelled,
            "请求已取消并收到 Agent 终态",
        ));
    }
    let reason = match response.stop_reason {
        StopReason::MaxTokens | StopReason::MaxTurnRequests => FinishReason::Length,
        StopReason::EndTurn => FinishReason::Stop,
        StopReason::Refusal => {
            return Err(error(
                ServiceErrorKind::InvalidRequest,
                "Agent 拒绝此轮请求，会话已失效",
            ))
        }
        _ => {
            return Err(error(
                ServiceErrorKind::Internal,
                "Agent 返回了不支持的结束原因",
            ))
        }
    };
    state.history = input.messages;
    state.history.push(ChatMessage {
        role: MessageRole::Assistant,
        text: route.text,
    });
    Ok(reason)
}

async fn finish_retired(
    retired: &mut Vec<tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>>,
) -> Result<(), ServiceError> {
    let mut result = Ok(());
    for task in retired.drain(..) {
        if !matches!(task.await, Ok(Ok(()))) && result.is_ok() {
            result = Err(error(ServiceErrorKind::Io, "ACP 旧回调或终端资源收尾失败"));
        }
    }
    result
}

async fn initialize(
    cx: &ConnectionTo<Agent>,
    workspace: PathBuf,
    callbacks: &Callbacks,
) -> Result<SessionState, ServiceError> {
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
        .map_err(|_| {
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
    create_session(cx, workspace, callbacks).await.map_err(|_| {
        error(
            ServiceErrorKind::InvalidConfig,
            "Agent 模型探测会话创建失败",
        )
    })
}

async fn run_backend(
    processes: AgentProcessHandle,
    workspace: PathBuf,
    status: watch::Sender<ServiceStatus>,
    mut commands: mpsc::UnboundedReceiver<BackendCommand>,
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
    loop {
        actor.phase(ServicePhase::Starting, None);
        actor.models = Arc::new(Mutex::new(Err(error(
            ServiceErrorKind::NotReady,
            "Agent 尚未初始化",
        ))));
        let failure = match processes.connect().await {
            Err(failure) => failure,
            Ok(connection) => {
                let callbacks = Callbacks::new(workspace.clone());
                let mut exit = connection.exit;
                let mut initialized = false;
                let mut startup_failure = None;
                let connected = callbacks::connect(connection.transport, callbacks.clone(), async |cx: ConnectionTo<Agent>| {
                    let startup = tokio::time::timeout(std::time::Duration::from_secs(30), initialize(&cx, workspace.clone(), &callbacks));
                    tokio::pin!(startup);
                    let probe = loop {
                        if actor.stopping {
                            return callbacks.shutdown().await;
                        }
                        if actor.sealed && actor.requests.is_empty() {
                            for reply in actor.acknowledgements.drain(..) { let _ = reply.send(Ok(())); }
                        }
                        if exit.borrow().is_some() { return Err(callbacks::io_error("Agent 初始化期间退出")); }
                        tokio::select! {
                            result = &mut startup => match result {
                                Ok(Ok(probe)) => break probe,
                                Ok(Err(failure)) => { startup_failure = Some(failure); return Err(callbacks::io_error("Agent ACP 初始化失败")); }
                                Err(_) => { startup_failure = Some(error(ServiceErrorKind::InvalidConfig, "Agent ACP 初始化及模型探测超时（30 秒）")); return Err(callbacks::io_error("Agent ACP 初始化超时")); }
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
                        actor.schedule(&cx, &workspace, &callbacks, &mut spare, &mut rounds);
                        if actor.sealed && actor.requests.is_empty() {
                            if actor.stopping {
                                // SDK 连接仍处理已知 handle 的管理回调，直到自有终端安全回收。
                                return callbacks.shutdown().await;
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
                let mut finish = connected.map_err(|_| {
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
                });
                // 通信断裂先交付失败；旧终端自然收尾异步托管，不阻塞模型查询或恢复。
                if let Err(failure) = &finish {
                    {
                        let mut routes = callbacks
                            .routes
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        routes.active.clear();
                        routes.known.clear();
                    }
                    actor.fail_all(failure);
                }
                if finish.is_err() && !actor.stopping {
                    let cleanup = callbacks.clone();
                    retired.push(tokio::spawn(async move { cleanup.shutdown().await }));
                } else {
                    if callbacks.shutdown().await.is_err() && finish.is_ok() {
                        finish = Err(error(
                            ServiceErrorKind::Io,
                            "ACP 自有回调或终端资源收尾失败",
                        ));
                    }
                    if let Err(failure) = finish_retired(&mut retired).await {
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
                    actor.phase(ServicePhase::Stopped, finish.as_ref().err());
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
                    let result = finish_retired(&mut retired).await;
                    actor.phase(ServicePhase::Stopped, result.as_ref().err());
                    let _ = reply.send(result);
                    return;
                }
                Some(command) => actor.command(command),
                None => {
                    let _ = finish_retired(&mut retired).await;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::SessionConfigSelectGroup;
    // 覆盖 AH-04 / AH-A08：完整历史含实际 assistant 输出；不重放历史。
    #[test]
    fn history_requires_exact_prefix_and_new_message() {
        let previous = vec![
            ChatMessage {
                role: MessageRole::User,
                text: "一".into(),
            },
            ChatMessage {
                role: MessageRole::Assistant,
                text: "二".into(),
            },
        ];
        let mut valid = previous.clone();
        valid.push(ChatMessage {
            role: MessageRole::User,
            text: "三".into(),
        });
        assert!(validate_history(&previous, &valid).is_ok());
        assert!(validate_history(&previous, &previous).is_err());
        valid[1].text = "不同".into();
        assert!(validate_history(&previous, &valid).is_err());
    }
    // 覆盖 AH-04：只有 Model 分类的 Select，平铺和分组的真实 value id。
    #[test]
    fn model_discovery_uses_flat_and_grouped_select_values() {
        let choice = SessionConfigSelectOption::new("真实-model", "模型");
        let flat = SessionConfigOption::select("model", "模型", "真实-model", vec![choice.clone()])
            .category(SessionConfigOptionCategory::Model);
        assert_eq!(model_selector(&[flat]).unwrap().values[0].id, "真实-model");
        let grouped = SessionConfigOption::select(
            "model",
            "模型",
            "真实-model",
            vec![SessionConfigSelectGroup::new("group", "分组", vec![choice])],
        )
        .category(SessionConfigOptionCategory::Model);
        assert_eq!(
            model_selector(&[grouped]).unwrap().values[0].id,
            "真实-model"
        );
        assert_eq!(
            model_selector(&[]).err().unwrap().kind,
            ServiceErrorKind::ModelsUnavailable
        );
    }

    // 覆盖 AH-12 / AH-A08：4 个执行 lane 饱和后，同会话等待也计入 16 个容量。
    #[test]
    fn admission_counts_same_session_waiters_and_preserves_fifo() {
        let (status, _) = watch::channel(ServiceStatus::default());
        let mut actor = Actor {
            status,
            sessions: HashMap::new(),
            requests: HashMap::new(),
            queue: VecDeque::new(),
            active: HashSet::new(),
            models: Arc::new(Mutex::new(Ok(ModelSelector {
                id: SessionConfigId::new("model"),
                values: vec![ModelDescriptor {
                    id: "可选模型".into(),
                    name: "模型".into(),
                }],
                current: "可选模型".into(),
            }))),
            sealed: false,
            stopping: false,
            acknowledgements: Vec::new(),
        };
        for lane in 0..MAX_EXECUTING {
            actor.active.insert(SessionKey(format!("lane-{lane}")));
        }
        let session = SessionKey("lane-0".into());
        let mut ids = Vec::new();
        for index in 0..MAX_WAITING {
            let id = RequestId::new();
            let (events, _receiver) = mpsc::unbounded_channel();
            actor
                .admit(
                    id.clone(),
                    PromptInput {
                        model: "可选模型".into(),
                        messages: vec![ChatMessage {
                            role: MessageRole::User,
                            text: index.to_string(),
                        }],
                        session: Some(session.clone()),
                    },
                    events,
                    CancellationToken::new(),
                )
                .unwrap();
            ids.push(id);
        }
        assert_eq!(actor.queue.iter().cloned().collect::<Vec<_>>(), ids);
        let (events, _receiver) = mpsc::unbounded_channel();
        let failure = actor
            .admit(
                RequestId::new(),
                PromptInput {
                    model: "可选模型".into(),
                    messages: vec![ChatMessage {
                        role: MessageRole::User,
                        text: "溢出".into(),
                    }],
                    session: Some(session),
                },
                events,
                CancellationToken::new(),
            )
            .unwrap_err();
        assert_eq!(failure.kind, ServiceErrorKind::QueueFull);
        assert_eq!(actor.queue.len(), MAX_WAITING);
    }
}
