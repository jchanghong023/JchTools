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
    let mut prompt_published = false;
    let result = execute_round(
        &cx,
        &callbacks,
        &mut state,
        input,
        events,
        cancellation,
        &mut prompt_published,
    )
    .await;
    finalize_round(&callbacks, id, key, state, result, prompt_published).await
}

// 统一真实轮次的路由收尾与会话归属判定，便于直接观察引用生命周期。
async fn finalize_round(
    callbacks: &Callbacks,
    id: RequestId,
    key: SessionKey,
    state: SessionState,
    mut result: Result<FinishReason, ServiceError>,
    prompt_published: bool,
) -> RoundResult {
    let session = state.id.clone();
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
            // set 已确认且 prompt 尚未发布时，Agent 历史没有变化；等待回调收尾后才归还。
            (
                RequestEvent::Cancelled,
                (!prompt_published).then_some(state),
            )
        }
        Err(failure) => (RequestEvent::Failed(failure), None),
    };
    if state.is_none() {
        callbacks
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .models
            .remove(&session);
    }
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
    prompt_published: &mut bool,
) -> Result<FinishReason, ServiceError> {
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
    *prompt_published = true;
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

// 每次后台被正常命令/轮次唤醒时，仅收割已完成项；不等待活终端，不另分配完成列表。
// retain_mut 保持退休顺序，首个收尾错误只缓存一份，交由最终 Stop 完整 drain 交付。
fn reap_retired(
    retired: &mut Vec<tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>>,
    first_failure: &mut Option<ServiceError>,
) {
    retired.retain_mut(|task| {
        if task.is_finished() {
            if let Some(result) = task.now_or_never() {
                if !matches!(result, Ok(Ok(()))) && first_failure.is_none() {
                    *first_failure =
                        Some(error(ServiceErrorKind::Io, "ACP 旧回调或终端资源收尾失败"));
                }
                return false;
            }
        }
        true
    });
}

async fn finish_retired(
    retired: &mut Vec<tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>>,
    first_failure: &mut Option<ServiceError>,
) -> Result<(), ServiceError> {
    let mut result = first_failure.take().map_or(Ok(()), Err);
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

// 只观察真实后台的退役集合；生产编译没有 observer、额外命令或公开 test-hooks API。
#[cfg(test)]
type RetirementObserver = Box<
    dyn FnMut(
            &[tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>],
            Option<&Callbacks>,
        ) + Send,
>;

async fn run_backend(
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
    )
    .await;
}

async fn run_backend_inner(
    processes: AgentProcessHandle,
    workspace: PathBuf,
    status: watch::Sender<ServiceStatus>,
    mut commands: mpsc::UnboundedReceiver<BackendCommand>,
    #[cfg(test)] mut observer: Option<RetirementObserver>,
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
                let callbacks = Callbacks::new(workspace.clone());
                let mut exit = connection.exit;
                let mut initialized = false;
                let mut startup_failure = None;
                let connected = callbacks::connect(connection.transport, callbacks.clone(), async |cx: ConnectionTo<Agent>| {
                    let startup = tokio::time::timeout(std::time::Duration::from_secs(30), initialize(&cx, workspace.clone(), &callbacks));
                    tokio::pin!(startup);
                    let probe = loop {
                        reap_retired(&mut retired, &mut retired_failure);
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
                        reap_retired(&mut retired, &mut retired_failure);
                        #[cfg(test)]
                        if let Some(observer) = observer.as_mut() {
                            observer(&retired, None);
                        }
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
                        routes.models.clear();
                        routes.known.clear();
                    }
                    actor.fail_all(failure);
                }
                if finish.is_err() && !actor.stopping {
                    let cleanup = callbacks.clone();
                    retired.push(tokio::spawn(async move { cleanup.shutdown().await }));
                    reap_retired(&mut retired, &mut retired_failure);
                    #[cfg(test)]
                    if let Some(observer) = observer.as_mut() {
                        observer(&retired, Some(&callbacks));
                    }
                } else {
                    if callbacks.shutdown().await.is_err() && finish.is_ok() {
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
                    actor.phase(ServicePhase::Stopped, result.as_ref().err());
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

    fn lifecycle_session(id: &str) -> SessionState {
        SessionState {
            id: SessionId::new(id),
            model: Arc::new(Mutex::new(Ok(ModelSelector {
                id: SessionConfigId::new("model"),
                values: vec![ModelDescriptor {
                    id: "可选模型".into(),
                    name: "模型".into(),
                }],
                current: "可选模型".into(),
            }))),
            history: vec![
                ChatMessage {
                    role: MessageRole::User,
                    text: "已提交".into(),
                },
                ChatMessage {
                    role: MessageRole::Assistant,
                    text: "已完成".into(),
                },
            ],
        }
    }

    async fn assert_invalid_round_releases_selector(failure: ServiceError, terminal: RequestEvent) {
        let workspace = tempfile::tempdir().unwrap();
        let callbacks = Callbacks::new(workspace.path().to_path_buf());
        let invalid = lifecycle_session("invalid");
        let invalid_id = invalid.id.clone();
        let invalid_model = Arc::downgrade(&invalid.model);
        let valid = lifecycle_session("valid");
        let valid_id = valid.id.clone();
        let valid_model = Arc::downgrade(&valid.model);
        let valid_history = valid.history.clone();
        let key = SessionKey("invalid-key".into());
        let valid_key = SessionKey("valid-key".into());
        let id = RequestId::new();
        let (events, mut receiver) = mpsc::unbounded_channel();
        {
            let mut routes = callbacks
                .routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for state in [&invalid, &valid] {
                routes.known.insert(state.id.clone());
                routes.models.insert(state.id.clone(), state.model.clone());
                routes.active.insert(
                    state.id.clone(),
                    Route {
                        events: events.clone(),
                        cancellation: CancellationToken::new(),
                        text: String::new(),
                        failure: None,
                    },
                );
            }
        }
        let (status, _) = watch::channel(ServiceStatus::default());
        let mut actor = Actor {
            status,
            sessions: HashMap::from([
                (
                    key.clone(),
                    SessionSlot {
                        state: None,
                        invalid: false,
                    },
                ),
                (
                    valid_key.clone(),
                    SessionSlot {
                        state: Some(valid),
                        invalid: false,
                    },
                ),
            ]),
            requests: HashMap::from([(
                id.clone(),
                Pending {
                    key: key.clone(),
                    input: None,
                    events,
                    cancellation: CancellationToken::new(),
                },
            )]),
            queue: VecDeque::new(),
            active: HashSet::from([key.clone()]),
            models: lifecycle_session("catalog").model,
            sealed: false,
            stopping: false,
            acknowledgements: Vec::new(),
        };
        let result = finalize_round(&callbacks, id, key.clone(), invalid, Err(failure), true).await;
        actor.finish(result);
        assert_eq!(receiver.try_recv().unwrap(), terminal);
        assert!(actor.sessions[&key].invalid);
        assert!(actor.sessions[&key].state.is_none());
        assert!(!actor.active.contains(&key));
        let retained = actor.sessions[&valid_key].state.as_ref().unwrap();
        assert!(!actor.sessions[&valid_key].invalid);
        assert_eq!(retained.id, valid_id);
        assert_eq!(retained.history, valid_history);
        assert!(valid_model.upgrade().is_some());
        {
            let routes = callbacks
                .routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            assert!(
                routes.known.contains(&invalid_id),
                "失效会话仍需拒绝 SDK id 复用"
            );
            assert!(routes.known.contains(&valid_id));
            assert!(!routes.active.contains_key(&invalid_id));
            assert!(routes.active.contains_key(&valid_id));
            assert!(routes.models.contains_key(&valid_id));
        }
        assert!(
            invalid_model.upgrade().is_none(),
            "失效轮次只保留 known/invalid 墓碑，不得让 routes.models 持有完整 selector"
        );
    }

    #[tokio::test]
    async fn failed_round_releases_selector_and_preserves_session_tombstones() {
        let failure = error(ServiceErrorKind::Internal, "合成轮次失败");
        assert_invalid_round_releases_selector(failure.clone(), RequestEvent::Failed(failure))
            .await;
    }

    #[tokio::test]
    async fn cancelled_round_releases_selector_and_preserves_session_tombstones() {
        assert_invalid_round_releases_selector(
            error(ServiceErrorKind::Cancelled, "合成 prompt 取消"),
            RequestEvent::Cancelled,
        )
        .await;
    }

    #[tokio::test]
    async fn completed_round_preserves_selector_and_valid_history() {
        let workspace = tempfile::tempdir().unwrap();
        let callbacks = Callbacks::new(workspace.path().to_path_buf());
        let state = lifecycle_session("completed");
        let session = state.id.clone();
        let history = state.history.clone();
        let model = Arc::downgrade(&state.model);
        {
            let mut routes = callbacks
                .routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            routes.known.insert(session.clone());
            routes.models.insert(session.clone(), state.model.clone());
        }
        let result = finalize_round(
            &callbacks,
            RequestId::new(),
            SessionKey("completed-key".into()),
            state,
            Ok(FinishReason::Stop),
            true,
        )
        .await;
        assert_eq!(
            result.terminal,
            RequestEvent::Completed {
                reason: FinishReason::Stop
            }
        );
        let retained = result.state.unwrap();
        assert_eq!(retained.id, session);
        assert_eq!(retained.history, history);
        assert!(model.upgrade().is_some());
        let routes = callbacks
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(routes.known.contains(&session));
        assert!(Arc::ptr_eq(&routes.models[&session], &retained.model));
    }

    // Stop 是完整 drain：已完成的首错必须保留，但仍安全等待尚未结束的 cleanup。
    #[tokio::test]
    async fn stop_drain_preserves_first_cleanup_error_and_waits_for_live_cleanup() {
        let first =
            tokio::spawn(async { Err(callbacks::io_error("首个 retired cleanup 失败")) });
        let (release, held) = oneshot::channel();
        let (finished, mut observed_finished) = oneshot::channel();
        let live = tokio::spawn(async move {
            // sender panic/drop 同样自然放行，不 abort/kill live cleanup。
            let _released = held.await;
            let _observed = finished.send(());
            Ok(())
        });
        let later = tokio::spawn(async { Ok(()) });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !first.is_finished() || !later.is_finished() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut retired = vec![first, live, later];
        let mut first_failure = None;
        reap_retired(&mut retired, &mut first_failure);
        assert_eq!(retired.len(), 1, "运行期只保留尚未结束的 cleanup");
        assert!(first_failure.is_some(), "运行期回收不能吞掉已完成项的首错");
        let failure = {
            let draining = finish_retired(&mut retired, &mut first_failure);
            tokio::pin!(draining);
            assert!(futures::poll!(draining.as_mut()).is_pending());
            assert!(matches!(
                observed_finished.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            release.send(()).unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), draining)
                .await
                .unwrap()
                .unwrap_err()
        };
        observed_finished.await.unwrap();
        assert!(retired.is_empty());
        assert_eq!(failure.kind, ServiceErrorKind::Io);
        assert_eq!(failure.message, "ACP 旧回调或终端资源收尾失败");
    }
    #[tokio::test]
    async fn pre_prompt_cancel_preserves_confirmed_selector_and_history() {
        let workspace = tempfile::tempdir().unwrap();
        let callbacks = Callbacks::new(workspace.path().to_path_buf());
        let state = lifecycle_session("not-published");
        let history = state.history.clone();
        let model = Arc::downgrade(&state.model);
        {
            let mut routes = callbacks
                .routes
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            routes.known.insert(state.id.clone());
            routes.models.insert(state.id.clone(), state.model.clone());
        }
        let result = finalize_round(
            &callbacks,
            RequestId::new(),
            SessionKey("not-published-key".into()),
            state,
            Err(error(
                ServiceErrorKind::Cancelled,
                "set 已确认；prompt 未发布",
            )),
            false,
        )
        .await;
        assert_eq!(result.terminal, RequestEvent::Cancelled);
        assert_eq!(result.state.unwrap().history, history);
        assert!(model.upgrade().is_some());
    }

    // 真正启动现有 SDK fixture，贯穿 run_backend 异常/恢复/Stop；不是退役算法副本。
    // 要求正常恢复期间回收已完成项，未完成项仍安全托管到自然退出及最终 Stop。
    #[cfg(windows)]
    #[tokio::test]
    async fn backend_reclaims_completed_retirements_during_recovery_without_dropping_live_cleanup()
    {
        use crate::acp_api::{
            agent_process_channel, AgentConnection, AgentExit, AgentProcessCommand,
        };
        use std::{process::Stdio, sync::Weak, time::Duration};
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

        #[derive(Clone)]
        struct Observation {
            retained: usize,
            completed: usize,
            live: usize,
            routes: Vec<Weak<Mutex<callbacks::Routes>>>,
        }
        struct ReleaseOnDrop(PathBuf);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::write(&self.0, b"release");
            }
        }
        fn alive(pid: u32) -> bool {
            use windows_sys::Win32::{
                Foundation::{CloseHandle, WAIT_FAILED, WAIT_TIMEOUT},
                System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
            };
            // SAFETY: 只查询测试自有进程，不修改/终止，随后关闭本次唯一拥有的句柄。
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                return false;
            }
            // SAFETY: handle 是本函数刚取得的有效同步句柄，零超时只查询完成状态。
            let waited = unsafe { WaitForSingleObject(handle, 0) };
            // SAFETY: 本函数独占该有效句柄，查询后仅在这里关闭一次。
            unsafe { CloseHandle(handle) };
            assert_ne!(waited, WAIT_FAILED);
            waited == WAIT_TIMEOUT
        }
        let executable = std::env::current_exe().unwrap();
        let fixture = executable
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("jchtools-acp-fixture.exe");
        assert!(
            fixture.is_file(),
            "先构建真实 SDK fixture：{}",
            fixture.display()
        );
        let root = tempfile::tempdir().unwrap();
        let release = ReleaseOnDrop(root.path().join("terminal-release-retirement-held"));
        let (processes, mut connections) = agent_process_channel();
        let (controls, mut controllers) = mpsc::unbounded_channel();
        let fixture_root = root.path().to_path_buf();
        let provider = tokio::spawn(async move {
            let mut children = Vec::new();
            while let Some(AgentProcessCommand::Connect { reply }) = connections.recv().await {
                let mut child = tokio::process::Command::new(&fixture)
                    .args(["--agent", fixture_root.to_str().unwrap(), "normal"])
                    .current_dir(&fixture_root)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();
                let pid = child.id().unwrap();
                let transport = agent_client_protocol::ByteStreams::new(
                    child.stdin.take().unwrap().compat_write(),
                    child.stdout.take().unwrap().compat(),
                );
                let (exit, exited) = watch::channel(None);
                let (finished, done) = oneshot::channel();
                let (kill, killed) = oneshot::channel();
                controls.send((kill, done)).unwrap();
                reply
                    .send(Ok(AgentConnection {
                        transport,
                        pid,
                        exit: exited,
                        finished,
                    }))
                    .unwrap();
                children.push(tokio::spawn(async move {
                    let result = tokio::select! {
                        result = child.wait() => result,
                        _ = killed => {
                            child.start_kill().unwrap();
                            child.wait().await
                        }
                    };
                    let _ = exit.send(Some(AgentExit {
                        code: result
                            .as_ref()
                            .ok()
                            .and_then(std::process::ExitStatus::code),
                        error: result.err().map(|error| error.to_string()),
                    }));
                }));
            }
            for child in children {
                child.await.unwrap();
            }
        });
        let (status, _) = watch::channel(ServiceStatus::default());
        let (backend, commands) = backend_channel(status.subscribe());
        let mut phases = backend.subscribe_status();
        let (observations, mut snapshots) = mpsc::unbounded_channel();
        let mut routes = Vec::new();
        let observer: RetirementObserver = Box::new(move |retired, callbacks| {
            if let Some(callbacks) = callbacks {
                routes.push(Arc::downgrade(&callbacks.routes));
            }
            let completed = retired.iter().filter(|task| task.is_finished()).count();
            let _ = observations.send(Observation {
                retained: retired.len(),
                completed,
                live: retired.len() - completed,
                routes: routes.clone(),
            });
        });
        let running = tokio::spawn(run_backend_inner(
            processes,
            root.path().to_path_buf(),
            status,
            commands,
            Some(observer),
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            // 首个 Submit 不负责启动握手：等待真实 initialize + 模型探测完成后的 Ready。
            while phases.borrow_and_update().phase != ServicePhase::Ready {
                phases.changed().await.unwrap();
            }
            assert_eq!(backend.models().await.unwrap().len(), 2);
            for index in 0..3 {
                let action = if index == 1 { "terminal-held" } else { "text" };
                let tag = if index == 1 { "retirement-held".into() } else { format!("retirement-{index}") };
                let plan = serde_json::json!({
                    "tag": tag, "action": action, "barrier": false, "chunks": ["真实恢复输出"],
                });
                let mut request = backend.submit(PromptInput {
                    model: "fixture-model-a".into(),
                    messages: vec![ChatMessage {
                        role: MessageRole::User, text: format!("AH_FIXTURE {plan}"),
                    }],
                    session: None,
                }).await.unwrap();
                loop {
                    match request.events.recv().await.unwrap() {
                        RequestEvent::Completed { .. } => { request.cancellation.disarm(); break; }
                        RequestEvent::Failed(failure) => panic!("真实 fixture 轮次失败：{failure:?}"),
                        RequestEvent::Cancelled => panic!("真实 fixture 不应被取消"),
                        RequestEvent::TextDelta(_) => {}
                    }
                }
                let (kill, done) = controllers.recv().await.unwrap();
                kill.send(()).unwrap();
                assert_eq!(done.await.unwrap().unwrap_err().kind, ServiceErrorKind::AgentDisconnected);
            }
            // 第四个真实 Agent 正常恢复并服务 Models；旧 held terminal 不得阻塞。
            let plan = serde_json::json!({
                "tag": "retirement-recovered", "action": "text", "barrier": false, "chunks": ["恢复成功"],
            });
            let mut request = backend.submit(PromptInput {
                model: "fixture-model-a".into(), session: None,
                messages: vec![ChatMessage {
                    role: MessageRole::User, text: format!("AH_FIXTURE {plan}"),
                }],
            }).await.unwrap();
            while let Some(event) = request.events.recv().await {
                if matches!(event, RequestEvent::Completed { .. }) {
                    request.cancellation.disarm();
                    break;
                }
                assert!(!matches!(event, RequestEvent::Failed(_) | RequestEvent::Cancelled));
            }
            let (_keep_alive, mut finished) = controllers.recv().await.unwrap();
            let held = loop {
                assert_eq!(backend.models().await.unwrap().len(), 2);
                let mut latest = None;
                while let Ok(snapshot) = snapshots.try_recv() { latest = Some(snapshot); }
                if let Some(snapshot) = latest {
                    if snapshot.routes.len() == 3
                        && snapshot.live == 1
                        && snapshot.routes[0].upgrade().is_none()
                        && snapshot.routes[1].upgrade().is_some()
                        && snapshot.routes[2].upgrade().is_none()
                    { break snapshot; }
                }
                tokio::task::yield_now().await;
            };
            let terminal_pid = loop {
                if let Ok(value) = std::fs::read_to_string(root.path().join("terminal-retirement-held.pid")) {
                    break value.parse::<u32>().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert!(alive(terminal_pid), "恢复不得终止旧自有终端");
            assert_eq!(held.live, 1);
            std::fs::write(&release.0, b"release").unwrap();
            let reclaimed = loop {
                backend.models().await.unwrap();
                let mut latest = None;
                while let Ok(snapshot) = snapshots.try_recv() { latest = Some(snapshot); }
                if let Some(snapshot) = latest {
                    // Weak 失效早于 Tokio 发布 JoinHandle 完成位；必须另观测全部任务已完成。
                    // 只等 live==0，不等 retained==0：未回收的完成项仍会触发下面的零留存断言。
                    if snapshot.routes.len() == 3
                        && snapshot.live == 0
                        && snapshot.routes.iter().all(|route| route.upgrade().is_none())
                    {
                        break snapshot;
                    }
                }
                tokio::task::yield_now().await;
            };
            assert!(!alive(terminal_pid), "回收必须等待真实自有终端退出");
            // Stop 和当前连接 finished 的真实 oneshot 均必须成功，不能在正常回收后丢失 ACK。
            assert!(matches!(finished.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
            backend.stop().await.unwrap();
            finished.await.unwrap().unwrap();
            running.await.unwrap();
            provider.await.unwrap();
            // 清理已完成后才报告红，断言失败不留下 held terminal 或挂起 Stop。
            assert_eq!(held.retained, 1,
                "仅 live cleanup 可留存；观察到 completed={}、live={}", held.completed, held.live);
            assert_eq!(reclaimed.retained, 0,
                "正常服务/恢复期间应回收 completed={} 个真实 cleanup", reclaimed.completed);
        }).await.expect("10s 仅为回归 watchdog，不修改产品退出等待");
    }
}
