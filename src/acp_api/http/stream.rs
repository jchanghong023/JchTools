//! AH-04 / AH-13：响应体直接持有事件接收器和本轮取消句柄，无脱离 HTTP 的转发任务。

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use axum::response::sse::Event;
use futures::{future::BoxFuture, Stream};
use tokio_util::sync::CancellationToken;

use crate::acp_api::{RequestEvent, RequestHandle, ServiceError, ServiceErrorKind};

use super::dto::{chunk, Delta, HttpError};

pub(super) fn disconnected() -> ServiceError {
    ServiceError::new(
        ServiceErrorKind::AgentDisconnected,
        "ACP 请求事件流未收到终态即关闭",
    )
}

pub(super) fn cancelled() -> ServiceError {
    ServiceError::new(ServiceErrorKind::Cancelled, "本次 ACP 请求已取消")
}

fn json_event(value: impl serde::Serialize) -> Result<Event, axum::Error> {
    Event::default().json_data(value)
}

#[derive(Clone, Copy)]
enum Stage {
    Role,
    Events,
    Done,
    End,
}

pub(super) struct CompletionStream {
    request: RequestHandle,
    id: String,
    first: Option<RequestEvent>,
    created: u64,
    stage: Stage,
    connection_closed: Option<BoxFuture<'static, ()>>,
    started: Instant,
    output_bytes: usize,
    terminal_logged: bool,
}

impl CompletionStream {
    pub(super) fn new(request: RequestHandle, first: RequestEvent, created: u64) -> Self {
        let id = format!("chatcmpl-{}", request.accepted.request_id.0);
        tracing::info!(event = "acp_sse_started", component = "acp_http", request_id = %request.accepted.request_id.0, "ACP SSE 响应开始");
        Self {
            request,
            id,
            first: Some(first),
            created,
            stage: Stage::Role,
            connection_closed: None,
            started: Instant::now(),
            output_bytes: 0,
            terminal_logged: false,
        }
    }

    pub(super) fn with_connection_closed(mut self, closed: CancellationToken) -> Self {
        // 该拥有型取消 future 需要固定地址；响应体持有它，不派生后台转发任务。
        self.connection_closed = Some(Box::pin(closed.cancelled_owned()));
        self
    }
}

impl Stream for CompletionStream {
    type Item = Result<Event, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if matches!(this.stage, Stage::Role | Stage::Events)
            && this
                .connection_closed
                .as_mut()
                .is_some_and(|closed| Future::poll(closed.as_mut(), cx).is_ready())
        {
            this.request.cancellation.cancel();
            this.terminal_logged = true;
            tracing::warn!(event = "acp_sse_cancelled", component = "acp_http", request_id = %this.request.accepted.request_id.0, stage = "connection_closed", elapsed_ms = crate::logging::elapsed_ms(this.started), "ACP SSE 客户端断连");
            this.stage = Stage::End;
            return Poll::Ready(None);
        }
        match this.stage {
            Stage::Role => {
                this.stage = Stage::Events;
                return Poll::Ready(Some(json_event(chunk(
                    &this.id,
                    &this.request.accepted,
                    this.created,
                    Delta::Role { role: "assistant" },
                    None,
                ))));
            }
            Stage::Done => {
                this.stage = Stage::End;
                return Poll::Ready(Some(Ok(Event::default().data("[DONE]"))));
            }
            Stage::End => return Poll::Ready(None),
            Stage::Events => {}
        }
        let event = match this.first.take() {
            Some(event) => Some(event),
            None => match this.request.events.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(event) => event,
            },
        };
        let event = match event {
            Some(RequestEvent::TextDelta(text)) => {
                this.output_bytes += text.len();
                json_event(chunk(
                    &this.id,
                    &this.request.accepted,
                    this.created,
                    Delta::Text { content: &text },
                    None,
                ))
            }
            Some(RequestEvent::Completed { reason }) => {
                this.request.cancellation.disarm();
                this.terminal_logged = true;
                tracing::info!(event = "acp_sse_completed", component = "acp_http", request_id = %this.request.accepted.request_id.0, result = ?reason, output_bytes = this.output_bytes, elapsed_ms = crate::logging::elapsed_ms(this.started), "ACP SSE 已收到完成终态");
                this.stage = Stage::Done;
                json_event(chunk(
                    &this.id,
                    &this.request.accepted,
                    this.created,
                    Delta::Empty {},
                    Some(reason),
                ))
            }
            Some(RequestEvent::Failed(error)) => {
                this.request.cancellation.disarm();
                this.terminal_logged = true;
                tracing::error!(event = "acp_sse_failed", component = "acp_http", request_id = %this.request.accepted.request_id.0, stage = "backend_terminal", error_type = ?error.kind, elapsed_ms = crate::logging::elapsed_ms(this.started), "ACP SSE 后台返回失败终态");
                this.stage = Stage::End;
                Ok(Event::default().data(HttpError::service(&error).json().to_string()))
            }
            Some(RequestEvent::Cancelled) => {
                this.request.cancellation.disarm();
                this.terminal_logged = true;
                tracing::warn!(event = "acp_sse_cancelled", component = "acp_http", request_id = %this.request.accepted.request_id.0, stage = "backend_terminal", elapsed_ms = crate::logging::elapsed_ms(this.started), "ACP SSE 后台返回取消终态");
                this.stage = Stage::End;
                Ok(Event::default().data(HttpError::service(&cancelled()).json().to_string()))
            }
            None => {
                // EOF 不是终态：立即取消遗留轮次，不能冒充 stop 或输出 [DONE]。
                this.request.cancellation.cancel();
                this.terminal_logged = true;
                tracing::error!(event = "acp_sse_failed", component = "acp_http", request_id = %this.request.accepted.request_id.0, stage = "event_eof", error_type = "agent_disconnected", elapsed_ms = crate::logging::elapsed_ms(this.started), "ACP SSE 未收到终态即关闭");
                this.stage = Stage::End;
                Ok(Event::default().data(HttpError::service(&disconnected()).json().to_string()))
            }
        };
        Poll::Ready(Some(event))
    }
}

impl Drop for CompletionStream {
    fn drop(&mut self) {
        if !self.terminal_logged {
            tracing::warn!(event = "acp_sse_cancelled", component = "acp_http", request_id = %self.request.accepted.request_id.0, stage = "response_body_dropped", elapsed_ms = crate::logging::elapsed_ms(self.started), "ACP SSE 响应体提前释放");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp_api::{
        backend_channel, AcceptedRequest, BackendCommand, FinishReason, PromptInput, ServiceStatus,
        SessionKey,
    };
    use futures::StreamExt;
    use serde_json::Value;
    use tokio::sync::watch;

    async fn accepted_request() -> (
        RequestHandle,
        tokio::sync::mpsc::UnboundedReceiver<BackendCommand>,
        tokio::sync::mpsc::UnboundedSender<RequestEvent>,
    ) {
        let (_status, receiver) = watch::channel(ServiceStatus::default());
        let (backend, mut commands) = backend_channel(receiver);
        let submission = tokio::spawn(async move {
            backend
                .submit(PromptInput {
                    model: "m".into(),
                    messages: Vec::new(),
                    session: None,
                })
                .await
        });
        let BackendCommand::Submit {
            request_id,
            events,
            reply,
            ..
        } = commands.recv().await.unwrap()
        else {
            panic!("应收到 Submit");
        };
        reply
            .send(Ok(AcceptedRequest {
                request_id,
                session: SessionKey("s".into()),
                model: "m".into(),
            }))
            .unwrap();
        (submission.await.unwrap().unwrap(), commands, events)
    }

    // 覆盖 AH-04：首段可见时后台仍未完成，无需后台完成后切片。
    #[tokio::test]
    async fn yields_delta_without_waiting_for_completion() {
        let (request, _commands, events) = accepted_request().await;
        let mut stream = CompletionStream::new(request, RequestEvent::TextDelta("首段".into()), 0);
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        events
            .send(RequestEvent::Completed {
                reason: FinishReason::Length,
            })
            .unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.is_none());
    }

    // 覆盖 AH-13：静默期间 drop 也会立即请求取消，不依赖下一段文本。
    #[tokio::test]
    async fn drop_cancels_but_terminal_completion_disarms() {
        let (request, mut commands, _events) = accepted_request().await;
        let stream = CompletionStream::new(request, RequestEvent::TextDelta("x".into()), 0);
        drop(stream);
        assert!(matches!(
            commands.recv().await,
            Some(BackendCommand::Cancel { .. })
        ));

        let (request, mut commands, _events) = accepted_request().await;
        let mut stream = CompletionStream::new(
            request,
            RequestEvent::Completed {
                reason: FinishReason::Stop,
            },
            0,
        );
        let _ = stream.next().await.unwrap().unwrap();
        let _ = stream.next().await.unwrap().unwrap();
        drop(stream);
        assert!(matches!(
            commands.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }

    // 覆盖 AH-13：响应体静默等待时连接关闭唤醒它，只取消对应轮次。
    #[tokio::test]
    async fn connection_close_cancels_silent_body() {
        let (request, mut commands, _events) = accepted_request().await;
        let closed = CancellationToken::new();
        let mut stream = CompletionStream::new(request, RequestEvent::TextDelta("x".into()), 0)
            .with_connection_closed(closed.clone());
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.unwrap().is_ok());
        let waiting = stream.next();
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        closed.cancel();
        assert!(waiting.await.is_none());
        assert!(matches!(
            commands.recv().await,
            Some(BackendCommand::Cancel { .. })
        ));
    }

    // 覆盖 AH-04 / AH-13：已提交响应头后的失败只产生 error，不伪造完成 chunk。
    #[tokio::test]
    async fn failure_and_eof_emit_error_without_success_terminator() {
        use axum::{
            body::to_bytes,
            response::{IntoResponse, Sse},
        };

        for terminal in [
            Some(RequestEvent::Failed(ServiceError::new(
                ServiceErrorKind::Internal,
                "不应泄露正文",
            ))),
            Some(RequestEvent::Cancelled),
            None,
        ] {
            let (request, mut commands, events) = accepted_request().await;
            let eof = terminal.is_none();
            if let Some(terminal) = terminal {
                events.send(terminal).unwrap();
            }
            drop(events);
            let stream = CompletionStream::new(request, RequestEvent::TextDelta("片段".into()), 0);
            let response = Sse::new(stream).into_response();
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body = std::str::from_utf8(&bytes).unwrap();
            assert!(body.contains("\"content\":\"片段\""));
            assert!(body.contains("\"error\":"));
            assert!(!body.contains("不应泄露正文"));
            assert!(!body.contains("[DONE]"));
            assert!(!body.contains("\"finish_reason\":\"stop\""));
            if eof {
                assert!(matches!(
                    commands.recv().await,
                    Some(BackendCommand::Cancel { .. })
                ));
            } else {
                assert!(matches!(
                    commands.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
                ));
            }
        }
    }

    // 覆盖 AH-04：正常完成含稳定请求标识、真实停止原因和唯一结束标记，无伪造用量。
    #[tokio::test]
    async fn successful_sse_preserves_text_and_finish_reason() {
        use axum::{
            body::to_bytes,
            response::{IntoResponse, Sse},
        };

        let (request, _commands, events) = accepted_request().await;
        events
            .send(RequestEvent::TextDelta("乙\n\"丙\"".into()))
            .unwrap();
        events
            .send(RequestEvent::Completed {
                reason: FinishReason::Length,
            })
            .unwrap();
        let response = Sse::new(CompletionStream::new(
            request,
            RequestEvent::TextDelta("甲".into()),
            7,
        ))
        .into_response();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        let chunks: Vec<Value> = body
            .split("\n\n")
            .filter_map(|event| {
                let data = event.strip_prefix("data: ")?;
                if data == "[DONE]" {
                    None
                } else {
                    Some(serde_json::from_str(data).unwrap())
                }
            })
            .collect();
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "甲");
        assert_eq!(chunks[2]["choices"][0]["delta"]["content"], "乙\n\"丙\"");
        assert_eq!(chunks[3]["choices"][0]["finish_reason"], "length");
        assert!(chunks.iter().all(|chunk| chunk["id"] == chunks[0]["id"]));
        assert_eq!(body.matches("[DONE]").count(), 1);
        assert!(!body.contains("\"usage\""));
    }
}
