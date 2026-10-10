//! AH-04 / AH-05：只接受能够忠实传给 ACP 的纯文本 Chat Completions 子集。

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{Map, Value};

use crate::acp_api::{
    AcceptedRequest, ChatMessage, FinishReason, MessageRole, PromptInput, ServiceError,
    ServiceErrorKind, SessionKey,
};

// 499 是非标准取消状态；编译期校验，不在请求处理路径解析状态码。
const CLIENT_CLOSED_REQUEST: StatusCode = match StatusCode::from_u16(499) {
    Ok(status) => status,
    Err(_) => panic!("HTTP 499 必须是有效状态码"),
};

pub(super) struct ChatInput {
    pub prompt: PromptInput,
    pub stream: bool,
}

#[derive(Debug)]
pub(super) struct HttpError {
    pub status: StatusCode,
    pub message: String,
    pub kind: &'static str,
    pub param: Option<String>,
    pub code: &'static str,
}

impl HttpError {
    pub(super) fn invalid(param: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            kind: "invalid_request_error",
            param: Some(param.into()),
            code: "invalid_request",
        }
    }

    pub(super) fn route(status: StatusCode, message: &'static str, code: &'static str) -> Self {
        Self {
            status,
            message: message.into(),
            kind: "invalid_request_error",
            param: None,
            code,
        }
    }

    pub(super) fn service(error: &ServiceError) -> Self {
        use ServiceErrorKind::{
            AgentDisconnected, Cancelled, HistoryMismatch, Internal, InvalidConfig, InvalidRequest,
            Io, ModelUnavailable, ModelsUnavailable, NotReady, QueueFull, SessionUnavailable,
            Stopping,
        };
        // Agent 的错误文本不可信，不能将 RPC 错误、提示正文或 stderr 直接回传。
        let (status, kind, code, param, message) = match error.kind {
            InvalidRequest => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_request",
                None,
                "ACP 请求内容无法执行",
            ),
            InvalidConfig => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_config",
                None,
                "ACP 服务配置无效，请检查模型服务页",
            ),
            ModelsUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "models_unavailable",
                Some("model"),
                "ACP Agent 未提供可协商并选择的模型能力",
            ),
            ModelUnavailable => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "model_unavailable",
                Some("model"),
                "model 不在 ACP Agent 当前可选模型列表中",
            ),
            HistoryMismatch => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "history_mismatch",
                Some("messages"),
                "messages 与指定 ACP 会话的完整历史前缀不一致",
            ),
            SessionUnavailable => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "session_unavailable",
                Some("X-JchTools-Session-ID"),
                "指定的 ACP 会话不可用，请使用新会话",
            ),
            QueueFull => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "queue_full",
                None,
                "ACP 请求等待队列已满，请稍后重试",
            ),
            NotReady => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "not_ready",
                None,
                "ACP 服务尚未就绪，请检查模型服务页",
            ),
            Stopping => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "stopping",
                None,
                "ACP 服务正在停止或应用配置，不接受新请求",
            ),
            Cancelled => (
                CLIENT_CLOSED_REQUEST,
                "server_error",
                "cancelled",
                None,
                "本次 ACP 请求已取消",
            ),
            AgentDisconnected => (
                StatusCode::BAD_GATEWAY,
                "server_error",
                "agent_disconnected",
                None,
                "ACP Agent 连接中断，本次请求未完成",
            ),
            Io => (
                StatusCode::BAD_GATEWAY,
                "server_error",
                "io_error",
                None,
                "ACP 请求的进程通信或文件终端操作失败",
            ),
            Internal => (
                StatusCode::BAD_GATEWAY,
                "server_error",
                "internal_error",
                None,
                "ACP 请求处理失败，请检查模型服务状态和本地去敏诊断",
            ),
        };
        Self {
            status,
            message: message.into(),
            kind,
            param: param.map(str::to_owned),
            code,
        }
    }

    pub(super) fn json(self) -> Value {
        let error = Map::from_iter([
            ("message".into(), Value::String(self.message)),
            ("type".into(), Value::String(self.kind.into())),
            (
                "param".into(),
                self.param.map_or(Value::Null, Value::String),
            ),
            ("code".into(), Value::String(self.code.into())),
        ]);
        Value::Object(Map::from_iter([("error".into(), Value::Object(error))]))
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.json())).into_response()
    }
}

fn object<'a>(value: &'a Value, param: &str) -> Result<&'a Map<String, Value>, HttpError> {
    value
        .as_object()
        .ok_or_else(|| HttpError::invalid(param, format!("{param} 必须是 JSON 对象")))
}

fn owned_object(value: Value, param: &str) -> Result<Map<String, Value>, HttpError> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(HttpError::invalid(
            param,
            format!("{param} 必须是 JSON 对象"),
        )),
    }
}

fn reject_fields(
    object: &Map<String, Value>,
    names: &[&str],
    prefix: &str,
    reject_null: bool,
) -> Result<(), HttpError> {
    for name in names {
        if object
            .get(*name)
            .is_some_and(|value| reject_null || !value.is_null())
        {
            let param = format!("{prefix}{name}");
            return Err(HttpError::invalid(
                &param,
                format!("{param} 不受 ACP 纯文本接口支持"),
            ));
        }
    }
    Ok(())
}

pub(super) fn parse_chat(
    value: Value,
    session: Option<SessionKey>,
) -> Result<ChatInput, HttpError> {
    let mut body = owned_object(value, "body")?;
    reject_fields(
        &body,
        &[
            "tools",
            "tool_choice",
            "functions",
            "function_call",
            "permission",
            "permissions",
            "permission_mode",
            "approval_policy",
        ],
        "",
        true,
    )?;
    reject_fields(
        &body,
        &[
            "temperature",
            "top_p",
            "max_tokens",
            "max_completion_tokens",
            "stop",
            "seed",
            "frequency_penalty",
            "presence_penalty",
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "reasoning_effort",
            "prediction",
            "audio",
            "modalities",
            "parallel_tool_calls",
            "service_tier",
        ],
        "",
        false,
    )?;
    reject_fields(
        &body,
        &["best_of", "min_tokens", "repetition_penalty", "top_k"],
        "",
        false,
    )?;
    if let Some(n) = body.get("n").filter(|value| !value.is_null()) {
        if n.as_u64() != Some(1) {
            return Err(HttpError::invalid(
                "n",
                "n 只支持整数 1，不能生成多个 completion",
            ));
        }
    }
    if let Some(store) = body.get("store").filter(|value| !value.is_null()) {
        if store.as_bool() != Some(false) {
            return Err(HttpError::invalid(
                "store",
                "store 只支持 false，本接口不保存请求正文",
            ));
        }
    }
    if let Some(format) = body.get("response_format").filter(|value| !value.is_null()) {
        let format = object(format, "response_format")?;
        if format.get("type").and_then(Value::as_str) != Some("text") {
            return Err(HttpError::invalid(
                "response_format",
                "response_format 只支持 text",
            ));
        }
        reject_fields(format, &["json_schema"], "response_format.", true)?;
    }
    let stream = match body.get("stream") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(stream)) => *stream,
        Some(_) => return Err(HttpError::invalid("stream", "stream 必须是布尔值")),
    };
    if let Some(options) = body.get("stream_options").filter(|value| !value.is_null()) {
        let options = object(options, "stream_options")?;
        if let Some(include) = options
            .get("include_usage")
            .filter(|value| !value.is_null())
        {
            if include.as_bool() != Some(false) {
                return Err(HttpError::invalid(
                    "stream_options.include_usage",
                    "ACP 不提供可信 token 用量，include_usage 只支持 false",
                ));
            }
        }
    }
    let model = match body.remove("model") {
        Some(Value::String(model)) if !model.trim().is_empty() => model,
        _ => {
            return Err(HttpError::invalid(
                "model",
                "model 必须是非空的 Agent 可选模型标识",
            ))
        }
    };
    let messages = match body.remove("messages") {
        Some(Value::Array(messages)) if !messages.is_empty() => messages,
        _ => return Err(HttpError::invalid("messages", "messages 必须是非空数组")),
    };
    let mut normalized = Vec::with_capacity(messages.len());
    for (index, message) in messages.into_iter().enumerate() {
        let prefix = format!("messages[{index}]");
        let mut message = owned_object(message, &prefix)?;
        reject_fields(
            &message,
            &[
                "tool_calls",
                "tool_call_id",
                "function_call",
                "name",
                "audio",
                "refusal",
            ],
            &format!("{prefix}."),
            false,
        )?;
        let role = match message.get("role").and_then(Value::as_str) {
            Some("system") => MessageRole::System,
            Some("developer") => MessageRole::Developer,
            Some("user") => MessageRole::User,
            Some("assistant") => MessageRole::Assistant,
            _ => {
                return Err(HttpError::invalid(
                    format!("{prefix}.role"),
                    format!("{prefix}.role 只支持 system、developer、user、assistant 文本角色"),
                ))
            }
        };
        let content_param = format!("{prefix}.content");
        let text = match message.remove("content") {
            Some(Value::String(text)) => text,
            Some(Value::Array(parts)) => {
                let mut text = String::new();
                for (part_index, part) in parts.iter().enumerate() {
                    let param = format!("{content_param}[{part_index}]");
                    let part = object(part, &param)?;
                    if part.get("type").and_then(Value::as_str) != Some("text") {
                        return Err(HttpError::invalid(
                            format!("{param}.type"),
                            format!("{param} 只支持 text 内容块，不支持图片、音频或其他媒体"),
                        ));
                    }
                    let part_text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                        HttpError::invalid(
                            format!("{param}.text"),
                            format!("{param}.text 必须是字符串"),
                        )
                    })?;
                    text.push_str(part_text);
                }
                text
            }
            _ => {
                return Err(HttpError::invalid(
                    &content_param,
                    format!("{content_param} 必须是文本字符串或纯 text 内容块数组"),
                ))
            }
        };
        normalized.push(ChatMessage { role, text });
    }
    Ok(ChatInput {
        prompt: PromptInput {
            model,
            messages: normalized,
            session,
        },
        stream,
    })
}

pub(super) fn finish_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
    }
}

#[derive(serde::Serialize)]
pub(super) struct Completion<'a> {
    id: String,
    object: &'static str,
    created: u64,
    model: &'a str,
    choices: [CompletionChoice; 1],
}

#[derive(serde::Serialize)]
struct CompletionChoice {
    index: u8,
    message: AssistantMessage,
    finish_reason: &'static str,
}

#[derive(serde::Serialize)]
struct AssistantMessage {
    role: &'static str,
    content: String,
}

pub(super) fn completion(
    accepted: &AcceptedRequest,
    created: u64,
    text: String,
    reason: FinishReason,
) -> Completion<'_> {
    Completion {
        id: format!("chatcmpl-{}", accepted.request_id.0),
        object: "chat.completion",
        created,
        model: &accepted.model,
        choices: [CompletionChoice {
            index: 0,
            message: AssistantMessage {
                role: "assistant",
                content: text,
            },
            finish_reason: finish_reason(reason),
        }],
    }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub(super) enum Delta<'a> {
    Role { role: &'static str },
    Text { content: &'a str },
    Empty {},
}

#[derive(serde::Serialize)]
pub(super) struct Chunk<'a> {
    id: &'a str,
    object: &'static str,
    created: u64,
    model: &'a str,
    choices: [ChunkChoice<'a>; 1],
}

#[derive(serde::Serialize)]
struct ChunkChoice<'a> {
    index: u8,
    delta: Delta<'a>,
    finish_reason: Option<&'static str>,
}

pub(super) fn chunk<'a>(
    id: &'a str,
    accepted: &'a AcceptedRequest,
    created: u64,
    delta: Delta<'a>,
    reason: Option<FinishReason>,
) -> Chunk<'a> {
    Chunk {
        id,
        object: "chat.completion.chunk",
        created,
        model: &accepted.model,
        choices: [ChunkChoice {
            index: 0,
            delta,
            finish_reason: reason.map(finish_reason),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 覆盖 AH-04：完整角色历史保留，不人为插入内容块分隔符。
    #[test]
    fn text_roles_and_parts_keep_full_history() {
        let input = parse_chat(
            json!({"model":"actual/model", "messages":[
            {"role":"system", "content":"规则"},
            {"role":"developer", "content":[{"type":"text", "text":"甲"},
                {"type":"text", "text":"乙"}]},
            {"role":"user", "content":"问题"}, {"role":"assistant", "content":"答案"}
        ], "ordinary_extension":true}),
            Some(SessionKey("conversation".into())),
        )
        .unwrap();
        assert_eq!(input.prompt.messages.len(), 4);
        assert_eq!(input.prompt.messages[1].role, MessageRole::Developer);
        assert_eq!(input.prompt.messages[1].text, "甲乙");
        assert_eq!(
            input.prompt.session,
            Some(SessionKey("conversation".into()))
        );
    }

    // 覆盖 AH-04 / AH-05：已知不支持的能力不得被静默接受。
    #[test]
    fn unsupported_fields_roles_and_media_are_rejected() {
        let base = json!({"model":"m", "messages":[{"role":"user", "content":"x"}]});
        for (name, value) in [
            ("tools", json!([])),
            ("tool_choice", json!("none")),
            ("temperature", json!(0)),
            ("n", json!(2)),
            ("stream_options", json!({"include_usage":true})),
        ] {
            let mut request = base.clone();
            request[name] = value;
            assert!(parse_chat(request, None).is_err(), "{name}");
        }
        for content in [
            json!(null),
            json!([{"type":"image_url", "image_url": {"url":"x"}}]),
            json!([{"type":"text", "text":7}]),
        ] {
            let mut request = base.clone();
            request["messages"][0]["content"] = content;
            assert!(parse_chat(request, None).is_err());
        }
        let mut request = base;
        request["messages"][0]["role"] = json!("tool");
        assert!(parse_chat(request, None).is_err());
    }

    // 覆盖 AH-12 / AH-13：错误分类固定，不回显 Agent 原文。
    #[test]
    fn service_errors_map_status_and_hide_untrusted_body() {
        for (kind, status) in [
            (ServiceErrorKind::QueueFull, 429),
            (ServiceErrorKind::ModelsUnavailable, 503),
            (ServiceErrorKind::HistoryMismatch, 400),
            (ServiceErrorKind::AgentDisconnected, 502),
            (ServiceErrorKind::Cancelled, 499),
        ] {
            let error = HttpError::service(&ServiceError::new(kind, "不可信正文"));
            assert_eq!(error.status.as_u16(), status);
            assert!(!error.json().to_string().contains("不可信正文"));
        }
    }
}
