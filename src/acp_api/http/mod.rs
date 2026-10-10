//! AH：Axum OpenAI HTTP 适配。监听、进程和 ACP 连接全部由注入后台负责。

mod connection;
mod dto;
mod stream;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{connect_info::ConnectInfo, rejection::JsonRejection, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
    Extension, Json, Router,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::acp_api::{BackendHandle, RequestEvent, RequestHandle, SessionKey};

use connection::{ConnectionClosed, ObservedListener};
use dto::{completion, parse_chat, HttpError};
use stream::{cancelled, disconnected, CompletionStream};

const SESSION_HEADER: &str = "x-jchtools-session-id";

/// 仅提供 AH-04 两个公开入口，不在路由模块启动或监听任何后台。
pub fn router(backend: BackendHandle) -> Router {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(backend)
}

/// AH-13：复用 Axum HTTP 服务，并把每条真实连接的读端关闭传给该连接的请求。
/// 不启动 Agent；独立后台负责 listener、router 和 shutdown 的生命周期。
pub async fn serve(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> Result<(), crate::acp_api::ServiceError> {
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
        Ok(models) => Json(
            json!({"object":"list", "data":models.into_iter().map(|model|
            json!({"id":model.id, "object":"model", "name":model.name, "owned_by":"acp-agent"}))
            .collect::<Vec<_>>()}),
        )
        .into_response(),
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
        // 首事件之前保持响应头未提交：后端在此处失败必须返回真实非 2xx JSON。
        let first = tokio::select! {
            biased;
            () = closed.cancelled() => return with_session(
                HttpError::service(&cancelled()).into_response(), session_header,
            ),
            event = request.events.recv() => event,
        };
        let first = match first {
            Some(RequestEvent::Failed(error)) => {
                request.cancellation.disarm();
                return with_session(HttpError::service(&error).into_response(), session_header);
            }
            Some(RequestEvent::Cancelled) => {
                request.cancellation.disarm();
                return with_session(
                    HttpError::service(&cancelled()).into_response(),
                    session_header,
                );
            }
            None => {
                return with_session(
                    HttpError::service(&disconnected()).into_response(),
                    session_header,
                )
            }
            Some(event) => event,
        };
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
    let mut text = String::new();
    loop {
        let event = tokio::select! {
            biased;
            () = closed.cancelled() => return HttpError::service(&cancelled()).into_response(),
            event = request.events.recv() => event,
        };
        match event {
            Some(RequestEvent::TextDelta(delta)) => text.push_str(&delta),
            Some(RequestEvent::Completed { reason }) => {
                request.cancellation.disarm();
                return Json(completion(&request.accepted, created, text, reason)).into_response();
            }
            Some(RequestEvent::Failed(error)) => {
                request.cancellation.disarm();
                return HttpError::service(&error).into_response();
            }
            Some(RequestEvent::Cancelled) => {
                request.cancellation.disarm();
                return HttpError::service(&cancelled()).into_response();
            }
            None => return HttpError::service(&disconnected()).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
