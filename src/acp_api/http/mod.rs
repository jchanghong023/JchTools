//! AH：Axum OpenAI HTTP 适配。监听、进程和 ACP 连接全部由注入后台负责。

mod connection;
mod dto;
mod stream;

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::{
    extract::{connect_info::ConnectInfo, rejection::JsonRejection, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
    Extension, Json, Router,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::acp_api::{BackendHandle, RequestEvent, RequestHandle, SessionKey};
use tracing::Instrument;

use connection::{ConnectionClosed, ObservedListener};
use dto::{completion, model_list, parse_chat, HttpError};
use stream::{cancelled, disconnected, CompletionStream};

const SESSION_HEADER: &str = "x-jchtools-session-id";

/// 仅提供 AH-04 两个公开入口，不在路由模块启动或监听任何后台。
pub fn router(backend: BackendHandle) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(axum::middleware::from_fn(observe_request))
        .with_state(backend)
}

async fn observe_request(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let route = match request.uri().path() {
        "/v1/models" => "models",
        "/v1/chat/completions" => "chat_completions",
        _ => "unknown",
    };
    let method = if request.method() == axum::http::Method::GET {
        "GET"
    } else if request.method() == axum::http::Method::POST {
        "POST"
    } else {
        "other"
    };
    async move {
        let started = Instant::now();
        tracing::info!(
            event = "acp_http_request_started",
            component = "acp_http",
            route,
            method,
            "ACP HTTP 请求开始"
        );
        let response = next.run(request).await;
        let status = response.status().as_u16();
        tracing::info!(
            event = "acp_http_response_ready",
            component = "acp_http",
            route,
            method,
            status,
            elapsed_ms = crate::logging::elapsed_ms(started),
            "ACP HTTP 响应头就绪"
        );
        response
    }
    .instrument(crate::logging::operation_span("acp_http", "http_request"))
    .await
}

/// AH-13：复用 Axum HTTP 服务，并把每条真实连接的读端关闭传给该连接的请求。
/// 不启动 Agent；独立后台负责 listener、router 和 shutdown 的生命周期。
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> Result<(), crate::acp_api::ServiceError> {
    crate::acp_api::diagnostics::async_call("acp_http", "serve", async {
        axum::serve(
            ObservedListener(listener),
            router.into_make_service_with_connect_info::<ConnectionClosed>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .map_err(|error| {
            crate::acp_api::ServiceError::new(
                crate::acp_api::ServiceErrorKind::Io,
                format!("模型服务 HTTP 监听失败：{error}"),
            )
        })
    })
    .await
}

async fn not_found() -> Response {
    HttpError::route(
        StatusCode::NOT_FOUND,
        "HTTP 接口路径不存在，仅支持模型列表和 Chat Completions",
        "not_found",
    )
    .into_response()
}

async fn method_not_allowed() -> Response {
    HttpError::route(
        StatusCode::METHOD_NOT_ALLOWED,
        "该 HTTP 接口不支持此请求方法",
        "method_not_allowed",
    )
    .into_response()
}

async fn models(State(backend): State<BackendHandle>) -> Response {
    match backend.models().await {
        Ok(models) => {
            tracing::info!(
                event = "acp_http_models_completed",
                component = "acp_http",
                model_count = models.len(),
                status = 200,
                "ACP HTTP 模型列表读取完成"
            );
            Json(model_list(models)).into_response()
        }
        Err(error) => HttpError::service(&error).into_response(),
    }
}

fn session_from_headers(headers: &HeaderMap) -> Result<Option<SessionKey>, HttpError> {
    let mut values = headers.get_all(SESSION_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(HttpError::invalid(
            "X-JchTools-Session-ID",
            "会话标识请求头只能提供一次",
        ));
    }
    let value = value
        .to_str()
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            HttpError::invalid("X-JchTools-Session-ID", "会话标识请求头必须是非空有效文本")
        })?;
    Ok(Some(SessionKey(value.to_owned())))
}

fn with_session(mut response: Response, session: HeaderValue) -> Response {
    response.headers_mut().insert(SESSION_HEADER, session);
    response
}

async fn chat(
    State(backend): State<BackendHandle>,
    headers: HeaderMap,
    connection: Option<Extension<ConnectInfo<ConnectionClosed>>>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(rejection) => {
            // Axum 的默认错误正文可能带解析上下文；统一转换为不回显输入的 OpenAI JSON。
            let (status, message, code) = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "HTTP 请求正文超过允许大小",
                    "payload_too_large",
                )
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    "HTTP 请求正文必须是有效的 application/json JSON",
                    "invalid_request",
                )
            };
            tracing::error!(
                event = "acp_http_input_failed",
                component = "acp_http",
                stage = "json_decode",
                status = status.as_u16(),
                error_code = code,
                "ACP HTTP JSON 输入解析失败"
            );
            return HttpError::route(status, message, code).into_response();
        }
    };
    let session = match session_from_headers(&headers) {
        Ok(session) => session,
        Err(error) => return error.into_response(),
    };
    let input = match parse_chat(payload, session) {
        Ok(input) => input,
        Err(error) => return error.into_response(),
    };
    // Hyper 可能因未处理的管线 read-ahead 而不再读取 EOF，不能依赖 handler drop。
    // 真实 serve 注入连接级关闭信号；直接调用 router 的测试仍保留请求 dropguard。
    let closed = connection.map_or_else(CancellationToken::new, |Extension(ConnectInfo(info))| {
        info.0
    });
    let mut request = tokio::select! {
        biased;
        () = closed.cancelled() => return HttpError::service(&cancelled()).into_response(),
        submitted = backend.submit(input.prompt) => match submitted {
            Ok(request) => request,
            Err(error) => return HttpError::service(&error).into_response(),
        },
    };
    tracing::info!(event = "acp_http_request_admitted", component = "acp_http", request_id = %request.accepted.request_id.0, streaming = input.stream, "ACP HTTP 请求已准入");
    let Ok(session_header) = HeaderValue::from_str(&request.accepted.session.0) else {
        return HttpError::route(
            StatusCode::BAD_GATEWAY,
            "ACP 后台返回了无法用于 HTTP 的会话标识",
            "invalid_backend_session",
        )
        .into_response();
    };
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    if input.stream {
        let first_started = Instant::now();
        tracing::info!(event = "acp_http_first_event_started", component = "acp_http", request_id = %request.accepted.request_id.0, "ACP HTTP 流式响应等待首事件");
        // 首事件之前保持响应头未提交：后端在此处失败必须返回真实非 2xx JSON。
        let first = tokio::select! {
            biased;
            () = closed.cancelled() => {
                tracing::warn!(event = "acp_http_first_event_cancelled", component = "acp_http", request_id = %request.accepted.request_id.0, stage = "connection_closed", elapsed_ms = crate::logging::elapsed_ms(first_started), "ACP HTTP 首事件前客户端断连");
                return with_session(HttpError::service(&cancelled()).into_response(), session_header);
            },
            event = request.events.recv() => event,
        };
        let first = match first {
            Some(RequestEvent::Failed(error)) => {
                request.cancellation.disarm();
                tracing::error!(event = "acp_http_first_event_failed", component = "acp_http", request_id = %request.accepted.request_id.0, stage = "backend_terminal", error_type = ?error.kind, elapsed_ms = crate::logging::elapsed_ms(first_started), "ACP HTTP 流式响应首事件为失败终态");
                return with_session(HttpError::service(&error).into_response(), session_header);
            }
            Some(RequestEvent::Cancelled) => {
                request.cancellation.disarm();
                tracing::warn!(event = "acp_http_first_event_cancelled", component = "acp_http", request_id = %request.accepted.request_id.0, stage = "backend_terminal", elapsed_ms = crate::logging::elapsed_ms(first_started), "ACP HTTP 流式响应首事件为取消终态");
                return with_session(
                    HttpError::service(&cancelled()).into_response(),
                    session_header,
                );
            }
            None => {
                tracing::error!(event = "acp_http_first_event_failed", component = "acp_http", request_id = %request.accepted.request_id.0, stage = "event_eof", error_type = "agent_disconnected", elapsed_ms = crate::logging::elapsed_ms(first_started), "ACP HTTP 流式响应首事件前事件流关闭");
                return with_session(
                    HttpError::service(&disconnected()).into_response(),
                    session_header,
                );
            }
            Some(event) => event,
        };
        tracing::info!(event = "acp_http_first_event_completed", component = "acp_http", request_id = %request.accepted.request_id.0, elapsed_ms = crate::logging::elapsed_ms(first_started), "ACP HTTP 首事件已收到，可提交 SSE 响应头");
        if matches!(first, RequestEvent::Completed { .. }) {
            request.cancellation.disarm();
        }
        // Stream 自身持有 guard；响应体被丢弃立即取消，即便 backend 一直静默。
        with_session(
            Sse::new(CompletionStream::new(request, first, created).with_connection_closed(closed))
                .into_response(),
            session_header,
        )
    } else {
        with_session(
            collect_completion(request, created, closed).await,
            session_header,
        )
    }
}

async fn collect_completion(
    mut request: RequestHandle,
    created: u64,
    closed: CancellationToken,
) -> Response {
    let started = Instant::now();
    let request_id = request.accepted.request_id.0.clone();
    tracing::info!(event = "acp_http_completion_started", component = "acp_http", %request_id, "ACP HTTP 非流式响应等待开始");
    let mut text = String::new();
    loop {
        let event = tokio::select! {
            biased;
            () = closed.cancelled() => {
                tracing::warn!(event = "acp_http_completion_cancelled", component = "acp_http", %request_id, stage = "connection_closed", elapsed_ms = crate::logging::elapsed_ms(started), "ACP HTTP 非流式客户端断连");
                return HttpError::service(&cancelled()).into_response();
            },
            event = request.events.recv() => event,
        };
        match event {
            Some(RequestEvent::TextDelta(delta)) => text.push_str(&delta),
            Some(RequestEvent::Completed { reason }) => {
                request.cancellation.disarm();
                tracing::info!(event = "acp_http_completion_completed", component = "acp_http", %request_id, result = ?reason, output_bytes = text.len(), elapsed_ms = crate::logging::elapsed_ms(started), "ACP HTTP 非流式响应完成");
                return Json(completion(&request.accepted, created, text, reason)).into_response();
            }
            Some(RequestEvent::Failed(error)) => {
                request.cancellation.disarm();
                tracing::error!(event = "acp_http_completion_failed", component = "acp_http", %request_id, stage = "backend_terminal", error_type = ?error.kind, elapsed_ms = crate::logging::elapsed_ms(started), "ACP HTTP 非流式响应失败");
                return HttpError::service(&error).into_response();
            }
            Some(RequestEvent::Cancelled) => {
                request.cancellation.disarm();
                tracing::warn!(event = "acp_http_completion_cancelled", component = "acp_http", %request_id, stage = "backend_terminal", elapsed_ms = crate::logging::elapsed_ms(started), "ACP HTTP 非流式响应已取消");
                return HttpError::service(&cancelled()).into_response();
            }
            None => {
                tracing::error!(event = "acp_http_completion_failed", component = "acp_http", %request_id, stage = "event_eof", error_type = "agent_disconnected", elapsed_ms = crate::logging::elapsed_ms(started), "ACP HTTP 非流式事件流提前关闭");
                return HttpError::service(&disconnected()).into_response();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 覆盖 AH-04：续会话标识不能为空或歧义；不擅自设定或裁剪模型历史。
    #[test]
    fn session_header_is_explicit_and_preserved() {
        let mut headers = HeaderMap::new();
        assert!(session_from_headers(&headers).unwrap().is_none());
        headers.insert(SESSION_HEADER, HeaderValue::from_static("stable-session"));
        assert_eq!(
            session_from_headers(&headers).unwrap(),
            Some(SessionKey("stable-session".into()))
        );
        headers.append(SESSION_HEADER, HeaderValue::from_static("another"));
        assert!(session_from_headers(&headers).is_err());
        headers.clear();
        headers.insert(SESSION_HEADER, HeaderValue::from_static(" "));
        assert!(session_from_headers(&headers).is_err());
    }

    // 覆盖 AH-13：admission 尚未答复时断连也必须丢弃 submit guard、取消该请求。
    #[tokio::test]
    async fn connection_close_cancels_pending_admission() {
        use crate::acp_api::{backend_channel, BackendCommand, ServiceStatus};
        let (_status, receiver) = tokio::sync::watch::channel(ServiceStatus::default());
        let (backend, mut commands) = backend_channel(receiver);
        let closed = CancellationToken::new();
        let connection = ConnectionClosed(closed.clone());
        let response = tokio::spawn(chat(
            State(backend),
            HeaderMap::new(),
            Some(Extension(ConnectInfo(connection))),
            Ok(Json(json!({
                "model": "m",
                "messages": [{"role": "user", "content": "test"}],
                "stream": false
            }))),
        ));
        let Some(BackendCommand::Submit {
            reply,
            events,
            cancellation,
            ..
        }) = commands.recv().await
        else {
            panic!("应收到 Submit");
        };
        // 持有两端，排除后台自己先关闭事件/准入通道造成的伪断连。
        let _pending = (reply, events);
        closed.cancel();
        assert_eq!(response.await.unwrap().status().as_u16(), 499);
        assert!(cancellation.is_cancelled());
        assert!(matches!(
            commands.recv().await,
            Some(BackendCommand::Cancel { .. })
        ));
    }

    #[derive(Clone)]
    struct LogCapture(std::sync::mpsc::Sender<Vec<u8>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.send(bytes.to_vec()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "日志测试接收器已关闭")
            })?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn local_request(port: u16, body: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let headers = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            body.len(),
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        String::from_utf8(response).unwrap()
    }

    /// Real TCP exercises response-body polling, not just handler wiring.
    #[tokio::test(flavor = "current_thread")]
    async fn local_http_logs_json_sse_failures_and_private_body_boundaries() {
        use crate::acp_api::{
            backend_channel, AcceptedRequest, BackendCommand, FinishReason, ServiceError,
            ServiceErrorKind, ServiceStatus,
        };
        let (records, recorded) = std::sync::mpsc::channel();
        let capture = LogCapture(records);
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _dispatch = tracing::subscriber::set_default(subscriber);
        let (_status, receiver) = tokio::sync::watch::channel(ServiceStatus::default());
        let (backend, mut commands) = backend_channel(receiver);
        let fixture = tokio::spawn(async move {
            let mut ids = Vec::new();
            while let Some(command) = commands.recv().await {
                if let BackendCommand::Submit {
                    request_id,
                    input,
                    events,
                    reply,
                    ..
                } = command
                {
                    ids.push(request_id.0.clone());
                    reply
                        .send(Ok(AcceptedRequest {
                            request_id,
                            session: SessionKey("private-session-marker".into()),
                            model: input.model,
                        }))
                        .unwrap();
                    match ids.len() {
                        1 | 2 => {
                            events
                                .send(RequestEvent::TextDelta("private-output-marker".into()))
                                .unwrap();
                            events
                                .send(RequestEvent::Completed {
                                    reason: FinishReason::Stop,
                                })
                                .unwrap();
                        }
                        3 => {
                            events
                                .send(RequestEvent::TextDelta("private-output-marker".into()))
                                .unwrap();
                            events
                                .send(RequestEvent::Failed(ServiceError::new(
                                    ServiceErrorKind::Internal,
                                    "private-agent-error-marker",
                                )))
                                .unwrap();
                        }
                        4 => {
                            events
                                .send(RequestEvent::TextDelta("private-output-marker".into()))
                                .unwrap();
                            // EOF without a terminal must be diagnosed as failure.
                        }
                        5 => {
                            events
                                .send(RequestEvent::Failed(ServiceError::new(
                                    ServiceErrorKind::Internal,
                                    "private-agent-error-marker",
                                )))
                                .unwrap();
                        }
                        _ => panic!("unexpected fixture request"),
                    }
                    if ids.len() == 5 {
                        return ids;
                    }
                }
            }
            panic!("fixture ended before all requests");
        });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let shutdown = CancellationToken::new();
        let serving = tokio::spawn(serve(listener, router(backend), shutdown.clone()));
        let invalid = local_request(port, "{\"private-json-marker\":").await;
        assert!(invalid.starts_with("HTTP/1.1 400"));
        for (index, stream) in [false, true, true, true, false].into_iter().enumerate() {
            let body = json!({
                "model": "fixture",
                "messages": [{"role": "user", "content": "private-prompt-marker"}],
                "stream": stream,
            })
            .to_string();
            let response = local_request(port, &body).await;
            let expected_status = if index == 4 {
                "HTTP/1.1 502"
            } else {
                "HTTP/1.1 200"
            };
            assert!(response.starts_with(expected_status), "{response}");
            if stream {
                assert_eq!(response.contains("[DONE]"), index == 1);
            } else if index == 0 {
                assert!(response.contains("private-output-marker"));
            }
        }
        let ids = fixture.await.unwrap();
        shutdown.cancel();
        serving.await.unwrap().unwrap();
        let logs = String::from_utf8(recorded.try_iter().flatten().collect()).unwrap();
        for event in [
            "acp_http_request_started",
            "acp_http_input_failed",
            "acp_request_submitted",
            "acp_http_request_admitted",
            "acp_http_completion_completed",
            "acp_http_completion_failed",
            "acp_sse_completed",
            "acp_sse_failed",
        ] {
            assert!(logs.contains(event), "missing {event}: {logs}");
        }
        for (index, id) in ids.iter().enumerate() {
            let terminal = if index == 0 {
                "acp_http_completion_completed"
            } else if index == 1 {
                "acp_sse_completed"
            } else if index == 4 {
                "acp_http_completion_failed"
            } else {
                "acp_sse_failed"
            };
            assert!(
                logs.lines()
                    .any(|line| line.contains(terminal) && line.contains(id)),
                "{logs}"
            );
            assert!(
                logs.lines()
                    .any(|line| line.contains("acp_request_submitted") && line.contains(id)),
                "{logs}"
            );
        }
        assert!(logs.contains("event_eof"));
        assert!(logs.contains("elapsed_ms"));
        for private in [
            "private-json-marker",
            "private-prompt-marker",
            "private-output-marker",
            "private-session-marker",
            "private-agent-error-marker",
        ] {
            assert!(
                !logs.contains(private),
                "private data reached diagnostic log: {private}"
            );
        }
    }
}
