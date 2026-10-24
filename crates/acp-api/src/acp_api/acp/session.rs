use super::{
    disconnected, error, model_selector, model_selector_from_option, mpsc, oneshot, sdk_error,
    Agent, Arc, Callbacks, CancelNotification, CancellationToken, ChatMessage, ConnectionTo,
    ContentBlock, FinishReason, Instrument, MessageRole, Mutex, NewSessionRequest, PathBuf,
    PoisonError, PromptInput, PromptRequest, RequestEvent, RequestId, RoundContext, RoundResult,
    Route, ServiceError, ServiceErrorKind, SessionConfigId, SessionConfigValueId, SessionKey,
    SessionState, SetSessionConfigOptionRequest, StopReason, TextContent,
};

pub(super) fn validate_history(
    history: &[ChatMessage],
    messages: &[ChatMessage],
) -> Result<(), ServiceError> {
    if messages.len() <= history.len() || !messages.starts_with(history) {
        Err(error(
            ServiceErrorKind::HistoryMismatch,
            "messages 必须包含此会话完整历史前缀和新增消息",
        ))
    } else {
        Ok(())
    }
}
pub(super) async fn create_session(
    cx: &ConnectionTo<Agent>,
    cwd: PathBuf,
    callbacks: &Callbacks,
    negotiated_model_id: Option<SessionConfigId>,
) -> Result<SessionState, ServiceError> {
    crate::acp_api::diagnostics::async_call("acp_agent", "create_session", async {
        let (reply, response) = oneshot::channel();
        let routes = callbacks.routes.clone();
        let connection = cx.clone();
        let diagnostic_span = tracing::Span::current();
        let session_cwd = cwd.clone();
        cx.prepare_request(NewSessionRequest::new(cwd))
            .on_receiving_result(move |result| {
                async move {
                    // 发布前注册有序回调，在原始派发内登记，先于紧随响应的配置通知。
                    let result = (|| {
                        let response =
                            result.map_err(|failure| sdk_error(&connection, &failure))?;
                        // SDK id 保留到本连接结束，拒绝复用取消会话 id，防止迟到更新串轮。
                        let mut routes = routes.lock().unwrap_or_else(PoisonError::into_inner);
                        if !routes.known.insert(response.session_id.clone()) {
                            return Err(error(
                                ServiceErrorKind::SessionUnavailable,
                                "Agent 重复返回了已使用的 ACP 会话标识",
                            ));
                        }
                        routes
                            .workspaces
                            .insert(response.session_id.clone(), session_cwd.clone());
                        routes
                            .spans
                            .insert(response.session_id.clone(), tracing::Span::current());
                        let options = response.config_options.as_deref().unwrap_or_default();
                        // ID 可由当前会话重新协商；缺少 UX category 时保留连接已知身份。
                        let selector = model_selector(options, negotiated_model_id.as_ref());
                        if let Ok(selector) = &selector {
                            routes
                                .model_ids
                                .insert(response.session_id.clone(), selector.id.clone());
                        }
                        if let Ok(selector) = &selector {
                            tracing::debug!(
                                event = "acp_session_models_received",
                                component = "acp_agent",
                                model_count = selector.values.len(),
                                "ACP 会话模型选择能力已收到"
                            );
                        }
                        let model = Arc::new(Mutex::new(selector));
                        routes
                            .models
                            .insert(response.session_id.clone(), model.clone());
                        Ok(SessionState {
                            id: response.session_id,
                            cwd: session_cwd,
                            model,
                            history: Vec::new(),
                        })
                    })();
                    let _ = reply.send(result);
                    Ok(())
                }
                .instrument(diagnostic_span)
            })
            .map_err(|failure| sdk_error(cx, &failure))?;
        response.await.map_err(|_| disconnected())?
    })
    .await
}
pub(super) async fn round(
    cx: ConnectionTo<Agent>,
    callbacks: Callbacks,
    context: RoundContext,
) -> RoundResult {
    let RoundContext {
        id,
        key,
        input,
        state,
        negotiated_model_id,
        events,
        cancellation,
    } = context;
    // 与调度事件相同：已有 ACP 状态（包括同目录探测状态）才算复用。
    let reused_session = state.is_some();
    let mut state = match state {
        Some(state) => state,
        None => match create_session(&cx, input.cwd.clone(), &callbacks, negotiated_model_id).await
        {
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
        (&id, input, reused_session),
        events,
        cancellation,
        &mut prompt_published,
    )
    .await;
    finalize_round(&callbacks, id, key, state, result, prompt_published).await
}

// 统一真实轮次的路由收尾与会话归属判定，便于直接观察引用生命周期。
pub(super) async fn finalize_round(
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
        let mut routes = callbacks
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        routes.models.remove(&session);
        routes.model_ids.remove(&session);
        routes.workspaces.remove(&session);
        routes.spans.remove(&session);
    }
    RoundResult {
        id,
        key,
        state,
        terminal,
    }
}
// 模型选择阶段：校验会话可选模型后发布 SetSessionConfigOption，
// 并等待 Agent 确认请求模型已生效；两段 inspect_err 日志与确认语义不变。
async fn select_model(
    cx: &ConnectionTo<Agent>,
    callbacks: &Callbacks,
    state: &SessionState,
    requested: &str,
) -> Result<(), ServiceError> {
    let selector_id = {
        let model = state.model.lock().unwrap_or_else(PoisonError::into_inner);
        let selector = model.as_ref().map_err(Clone::clone)?;
        if !selector.allows(requested) {
            return Err(error(
                ServiceErrorKind::ModelUnavailable,
                "此会话无法选择请求模型",
            ));
        }
        selector.id.clone()
    };
    let (reply, response) = oneshot::channel();
    let model = state.model.clone();
    let requested_model = requested.to_owned();
    let connection = cx.clone();
    let diagnostic_span = tracing::Span::current();
    callbacks
        .routes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .spans
        .insert(state.id.clone(), diagnostic_span.clone());
    let model_started = std::time::Instant::now();
    tracing::debug!(
        event = "acp_model_selection_started",
        component = "acp_agent",
        "ACP 会话模型选择开始"
    );
    cx.prepare_request(SetSessionConfigOptionRequest::new(
        state.id.clone(),
        selector_id.clone(),
        SessionConfigValueId::new(requested.to_owned()),
    ))
    .on_receiving_result(move |result| {
        async move {
            // 发布前注册有序回调，后到的 ConfigOptionUpdate 不会被旧回包覆盖。
            let result = (|| {
                let response = result.map_err(|failure| sdk_error(&connection, &failure))?;
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
        }
        .instrument(diagnostic_span)
    })
    .inspect_err(|_| {
        tracing::error!(
            event = "acp_model_selection_failed",
            component = "acp_agent",
            stage = "model_publish",
            error_type = "acp_protocol_error",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(model_started),
            "ACP 模型选择请求发布失败"
        );
    })
    .map_err(|failure| sdk_error(cx, &failure))?;
    response.await.map_err(|_| disconnected())
    .and_then(|result| result)
    .inspect_err(|failure| tracing::error!(event = "acp_model_selection_failed", component = "acp_agent", stage = "model_confirmation", error_type = ?failure.kind, elapsed_ms = jchtools_foundation::logging::elapsed_ms(model_started), "ACP 模型选择确认失败"))?;
    tracing::debug!(
        event = "acp_model_selection_completed",
        component = "acp_agent",
        status = "confirmed",
        elapsed_ms = jchtools_foundation::logging::elapsed_ms(model_started),
        "ACP 会话模型选择已确认"
    );
    Ok(())
}

// 新增消息统计与 ContentBlock 组装；角色分布与文本字节数供摘要日志逐字保留。
fn build_prompt_content(
    submitted: &[ChatMessage],
    raw_user: bool,
) -> (Vec<ContentBlock>, [usize; 4], usize) {
    let mut role_counts = [0_usize; 4];
    let mut text_bytes = 0_usize;
    let content = submitted
        .iter()
        .map(|message| {
            role_counts[match message.role {
                MessageRole::System => 0,
                MessageRole::Developer => 1,
                MessageRole::User => 2,
                MessageRole::Assistant => 3,
            }] += 1;
            text_bytes += message.text.len();
            ContentBlock::Text(TextContent::new(if raw_user {
                message.text.clone()
            } else {
                format!("[{}]\n{}", message.role.as_str(), message.text)
            }))
        })
        .collect();
    (content, role_counts, text_bytes)
}

// 单轮提示的发布素材与摘要口径；由 execute_round 组装后按值传入，
// 打包只影响参数形式，不改变发布前登记与等待顺序。
struct PromptRound<'a> {
    request_id: &'a RequestId,
    content: Vec<ContentBlock>,
    events: mpsc::UnboundedSender<RequestEvent>,
    cancellation: CancellationToken,
    input: PromptInput,
    role_counts: [usize; 4],
    text_bytes: usize,
    reused_session: bool,
}

// prompt 发布与等待阶段：注册在途路由、发布前登记有序终态回调、发布请求、
// 处理取消通知，并把 stop_reason 映射为 FinishReason、更新会话历史。
async fn submit_and_await_prompt(
    cx: &ConnectionTo<Agent>,
    callbacks: &Callbacks,
    state: &mut SessionState,
    round: PromptRound<'_>,
    prompt_published: &mut bool,
) -> Result<FinishReason, ServiceError> {
    let PromptRound {
        request_id,
        content,
        events,
        cancellation,
        input,
        role_counts,
        text_bytes,
        reused_session,
    } = round;
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
    let prompt_started = std::time::Instant::now();
    tracing::debug!(
        event = "acp_prompt_started",
        component = "acp_agent",
        message_count = input.messages.len() - state.history.len(),
        "ACP 会话提示请求开始"
    );
    let submitted = &input.messages[state.history.len()..];
    tracing::info!(
        event = "acp_prompt_summary",
        component = "acp_agent",
        request_id = %request_id.0,
        reused_session,
        message_count = submitted.len(),
        system_count = role_counts[0],
        developer_count = role_counts[1],
        user_count = role_counts[2],
        assistant_count = role_counts[3],
        text_bytes,
        "ACP 实际提交新增消息摘要"
    );
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
        .inspect_err(|_| {
            tracing::error!(
                event = "acp_prompt_failed",
                component = "acp_agent",
                stage = "prompt_publish",
                error_type = "acp_protocol_error",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(prompt_started),
                "ACP 提示请求发布失败"
            );
        })
        .map_err(|e| sdk_error(cx, &e))?;
    *prompt_published = true;
    let (result, route) = tokio::select! {
        result = &mut response => result.map_err(|_| disconnected())?,
        () = cancellation.cancelled() => {
            // ACP v1 session/cancel，与 SDK 通用 request cancellation 不混淆。
            tracing::warn!(event = "acp_prompt_cancel_started", component = "acp_agent", "ACP 会话取消通知开始");
            cx.send_notification(CancelNotification::new(state.id.clone())).map_err(|e| sdk_error(cx, &e))?;
            response.await.map_err(|_| disconnected())?
        }
    };
    let route = route.ok_or_else(disconnected)?;
    if let Some(failure) = route.failure {
        return Err(failure);
    }
    let response = result.map_err(|e| sdk_error(cx, &e))
    .inspect_err(|failure| tracing::error!(event = "acp_prompt_failed", component = "acp_agent", stage = "prompt_terminal", error_type = ?failure.kind, elapsed_ms = jchtools_foundation::logging::elapsed_ms(prompt_started), "ACP 提示请求收到失败终态"))?;
    tracing::info!(event = "acp_prompt_terminal_received", component = "acp_agent", result = ?response.stop_reason, output_bytes = route.text.len(), elapsed_ms = jchtools_foundation::logging::elapsed_ms(prompt_started), "ACP 提示请求收到 Agent 终态");
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
async fn execute_round(
    cx: &ConnectionTo<Agent>,
    callbacks: &Callbacks,
    state: &mut SessionState,
    request: (&RequestId, PromptInput, bool),
    events: mpsc::UnboundedSender<RequestEvent>,
    cancellation: CancellationToken,
    prompt_published: &mut bool,
) -> Result<FinishReason, ServiceError> {
    let (request_id, input, reused_session) = request;
    crate::acp_api::diagnostics::async_call("acp_agent", "execute_round", async {
        select_model(cx, callbacks, state, &input.model).await?;
        if cancellation.is_cancelled() {
            return Err(error(ServiceErrorKind::Cancelled, "请求已取消"));
        }
        let submitted = &input.messages[state.history.len()..];
        let raw_user = matches!(submitted, [message] if matches!(message.role, MessageRole::User));
        let (content, role_counts, text_bytes) = build_prompt_content(submitted, raw_user);
        let round = PromptRound {
            request_id,
            content,
            events,
            cancellation,
            input,
            role_counts,
            text_bytes,
            reused_session,
        };
        submit_and_await_prompt(cx, callbacks, state, round, prompt_published).await
    })
    .await
}
