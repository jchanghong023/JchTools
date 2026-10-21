use super::{Agent, ConnectionTo, ServiceError, ServiceErrorKind};

pub(super) fn error(kind: ServiceErrorKind, message: &str) -> ServiceError {
    ServiceError::new(kind, message)
}
pub(super) fn disconnected() -> ServiceError {
    error(
        ServiceErrorKind::AgentDisconnected,
        "Agent 连接已断开，本轮未重放",
    )
}
pub(super) fn sdk_error(
    cx: &ConnectionTo<Agent>,
    failure: &agent_client_protocol::Error,
) -> ServiceError {
    let connection_closed = cx.is_incoming_closed();
    tracing::error!(
        event = "acp_protocol_failed",
        component = "acp_agent",
        stage = "sdk_result",
        error_type = "acp_protocol_error",
        error_code = i32::from(failure.code),
        connection_closed,
        "ACP 协议调用失败"
    );
    if connection_closed {
        disconnected()
    } else {
        error(ServiceErrorKind::Internal, "Agent ACP 请求失败")
    }
}
