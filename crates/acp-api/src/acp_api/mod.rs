//! ACP HTTP 模型服务：独立后台、官方客户端及本机 HTTP 适配。
//! 关键配置存储边界默认记录 INFO 开始/结果，参数解析等高频辅助边界仅记录 DEBUG。

pub mod acp;
pub mod http;
pub mod runtime;
pub mod settings;
pub mod types;

pub use types::*;

/// Boundary observation only: never formats protocol errors, bodies or credentials.
mod diagnostics {
    use std::{future::Future, time::Instant};
    use tracing::Instrument;

    pub(super) trait Failure {
        fn kind(&self) -> &'static str;
        fn code(&self) -> Option<i32> {
            None
        }
    }

    impl Failure for super::ServiceError {
        fn kind(&self) -> &'static str {
            use super::ServiceErrorKind::{
                AgentDisconnected, Cancelled, HistoryMismatch, Internal, InvalidConfig,
                InvalidRequest, Io, ModelUnavailable, ModelsUnavailable, NotReady, QueueFull,
                SessionUnavailable, Stopping,
            };
            match self.kind {
                InvalidRequest => "invalid_request",
                InvalidConfig => "invalid_config",
                ModelsUnavailable => "models_unavailable",
                ModelUnavailable => "model_unavailable",
                HistoryMismatch => "history_mismatch",
                SessionUnavailable => "session_unavailable",
                QueueFull => "queue_full",
                NotReady => "not_ready",
                Stopping => "stopping",
                Cancelled => "cancelled",
                AgentDisconnected => "agent_disconnected",
                Io => "io",
                Internal => "internal",
            }
        }
    }

    impl Failure for agent_client_protocol::Error {
        fn kind(&self) -> &'static str {
            "acp_protocol_error"
        }
        fn code(&self) -> Option<i32> {
            Some(i32::from(self.code))
        }
    }

    struct Boundary {
        component: &'static str,
        operation: &'static str,
        started: Instant,
        finished: bool,
        configuration_storage: bool,
        span: tracing::Span,
    }

    impl Boundary {
        fn new(component: &'static str, operation: &'static str) -> Self {
            let configuration_storage = component == "acp_settings"
                && matches!(operation, "load_config" | "save_config" | "workspace_dir");
            if configuration_storage {
                tracing::info!(
                    component,
                    operation,
                    event = "acp_call_started",
                    "ACP 配置存储调用开始"
                );
            } else {
                tracing::debug!(
                    component,
                    operation,
                    event = "acp_call_started",
                    "ACP 边界调用开始"
                );
            }
            Self {
                component,
                operation,
                started: Instant::now(),
                finished: false,
                configuration_storage,
                span: tracing::Span::current(),
            }
        }

        fn finish<T, E: Failure>(&mut self, result: &Result<T, E>) {
            self.finished = true;
            let elapsed_ms = jchtools_foundation::logging::elapsed_ms(self.started);
            match result {
                Ok(_) if self.configuration_storage => tracing::info!(
                    component = self.component,
                    operation = self.operation,
                    event = "acp_call_completed",
                    status = "success",
                    elapsed_ms,
                    "ACP 配置存储调用完成"
                ),
                Ok(_) => tracing::debug!(
                    component = self.component,
                    operation = self.operation,
                    event = "acp_call_completed",
                    status = "success",
                    elapsed_ms,
                    "ACP 边界调用完成"
                ),
                Err(error) if matches!(error.kind(), "cancelled" | "stopping") => tracing::warn!(
                    component = self.component,
                    operation = self.operation,
                    event = "acp_call_cancelled",
                    stage = self.operation,
                    error_type = error.kind(),
                    status = "cancelled",
                    elapsed_ms,
                    "ACP 边界调用已取消或停止"
                ),
                Err(error) => {
                    tracing::error!(component = self.component, operation = self.operation, event = "acp_call_failed", stage = self.operation, error_type = error.kind(), error_code = ?error.code(), status = "failed", elapsed_ms, "ACP 边界调用失败");
                }
            }
        }
    }

    impl Drop for Boundary {
        fn drop(&mut self) {
            if !self.finished {
                self.span.in_scope(|| {
                    tracing::warn!(
                        component = self.component,
                        operation = self.operation,
                        event = "acp_call_interrupted",
                        stage = self.operation,
                        status = "interrupted",
                        elapsed_ms = jchtools_foundation::logging::elapsed_ms(self.started),
                        "ACP 边界调用被取消或异常中断"
                    );
                });
            }
        }
    }

    pub(super) fn call<T, E: Failure>(
        component: &'static str,
        operation: &'static str,
        work: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let span = jchtools_foundation::logging::operation_span(component, operation);
        let _entered = span.enter();
        let mut boundary = Boundary::new(component, operation);
        let result = work();
        boundary.finish(&result);
        result
    }

    pub(super) async fn async_call<T, E: Failure>(
        component: &'static str,
        operation: &'static str,
        work: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        async {
            let mut boundary = Boundary::new(component, operation);
            let result = work.await;
            boundary.finish(&result);
            result
        }
        .instrument(jchtools_foundation::logging::operation_span(
            component, operation,
        ))
        .await
    }
}
