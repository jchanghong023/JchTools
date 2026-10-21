//! 当前 Windows 用户及登录会话专用控制通道；不以 HTTP 端口猜测服务归属。
#[cfg(windows)]
use crate::acp_api::ServiceDiscovery;
use crate::acp_api::{ServiceError, ServiceErrorKind, ServiceStatus};
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
const PROTOCOL: u32 = 1;
#[cfg(windows)]
const MAX_FRAME: usize = crate::acp_api::settings::CONTROL_FRAME_LIMIT;
#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) enum Operation {
    Status,
    Apply,
    Stop,
    Discover,
}

#[cfg(windows)]
#[derive(Serialize, Deserialize)]
struct Request {
    operation: Operation,
    // Typed UUIDs cannot carry arbitrary user or credential text into a span.
    diagnostic_id: uuid::Uuid,
}

// AH-15 public discovery is a distinct external boundary, not a private control frame.
#[cfg(windows)]
#[derive(Deserialize)]
enum PublicDiscovery {
    Discover,
}

#[cfg(windows)]
#[derive(Deserialize)]
#[serde(untagged)]
enum IncomingRequest {
    PrivateControl(Request),
    PublicDiscovery(PublicDiscovery),
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Apply => "apply",
            Self::Stop => "stop",
            Self::Discover => "discover",
        }
    }
}
pub(super) enum ControlOperation {
    Apply,
    Stop,
}
pub(super) struct Control {
    pub(super) operation: ControlOperation,
    pub(super) reply: oneshot::Sender<Result<ServiceStatus, ServiceError>>,
    pub(super) span: tracing::Span,
}
#[cfg(windows)]
#[derive(Serialize, Deserialize)]
struct Response<T> {
    protocol: u32,
    pid: u32,
    result: Result<T, ServiceError>,
}
pub(super) fn error(message: impl Into<String>) -> ServiceError {
    ServiceError::new(ServiceErrorKind::Io, message)
}
#[cfg(windows)]
async fn send<T: Serialize>(
    stream: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<(), ServiceError> {
    crate::acp_api::diagnostics::async_call("acp_control", "frame_send", async { let data = serde_json::to_vec(value).map_err(|e| error(e.to_string()))?;
if data.len() > MAX_FRAME {
    tracing::error!(event = "acp_control_frame_failed", component = "acp_control", stage = "frame_size", direction = "send", size_bytes = data.len(), error_type = "frame_too_large", "ACP 控制消息发送大小超限");
    return Err(error("模型服务控制消息过大"));
}
let length = u32::try_from(data.len()).map_err(|e| error(e.to_string()))?;
stream
    .write_u32_le(length)
    .await
    .inspect_err(|e| tracing::error!(event = "acp_control_frame_failed", component = "acp_control", stage = "write_header", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP 控制帧头写入失败"))
    .map_err(|e| error(format!("写入模型服务控制管道失败：{e}")))?;
stream
    .write_all(&data)
    .await
    .inspect_err(|e| tracing::error!(event = "acp_control_frame_failed", component = "acp_control", stage = "write_body", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP 控制帧写入失败"))
    .map_err(|e| error(format!("写入模型服务控制管道失败：{e}")))?;
stream
    .flush()
    .await
    .map_err(|e| {
        tracing::error!(event = "acp_control_frame_failed", component = "acp_control", stage = "flush", direction = "send", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP 控制帧刷新失败");
        error(e.to_string())
    }) }).await
}
#[cfg(windows)]
async fn receive<T: serde::de::DeserializeOwned>(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<T, ServiceError> {
    crate::acp_api::diagnostics::async_call("acp_control", "frame_receive", async { let length = stream
    .read_u32_le()
    .await
    .inspect_err(|e| tracing::error!(event = "acp_control_frame_failed", component = "acp_control", stage = "read_header", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP 控制帧头读取失败"))
    .map_err(|e| error(format!("读取模型服务控制管道失败：{e}")))? as usize;
if length > MAX_FRAME {
    tracing::error!(event = "acp_control_frame_failed", component = "acp_control", stage = "frame_size", direction = "receive", size_bytes = length, error_type = "frame_too_large", "ACP 控制消息接收大小超限");
    return Err(error("模型服务控制消息过大"));
}
let mut data = vec![0; length];
stream
    .read_exact(&mut data)
    .await
    .inspect_err(|e| tracing::error!(event = "acp_control_frame_failed", component = "acp_control", stage = "read_body", error_type = ?e.kind(), error_code = ?e.raw_os_error(), "ACP 控制帧读取失败"))
    .map_err(|e| error(e.to_string()))?;
serde_json::from_slice(&data)
    .inspect_err(|e| tracing::error!(event = "acp_control_frame_failed", component = "acp_control", stage = "json_decode", error_type = ?e.classify(), "ACP 控制消息 JSON 解析失败"))
    .map_err(|e| error(format!("模型服务控制消息无效：{e}"))) }).await
}

#[cfg(windows)]
mod platform;

#[cfg(all(windows, test))]
pub(super) use platform::serve;
#[cfg(windows)]
pub(super) use platform::{exchange, exchange_with_peer, lock, serve_for_process};

// 产品只支持 Windows；其余构建维度明确拒绝，不提供替代网络控制服务。
#[cfg(not(windows))]
pub(super) fn lock(_kind: &str, _wait: bool) -> Result<Option<()>, ServiceError> {
    Err(error("ACP 后台服务仅支持 Windows"))
}
#[cfg(not(windows))]
pub(super) async fn exchange<T: serde::de::DeserializeOwned>(
    _operation: Operation,
) -> Result<Option<T>, ServiceError> {
    Err(error("ACP 后台服务仅支持 Windows"))
}
#[cfg(not(windows))]
pub(super) async fn serve_for_process(
    _status: watch::Receiver<ServiceStatus>,
    _instance_id: uuid::Uuid,
    _commands: mpsc::UnboundedSender<Control>,
    _shutdown: CancellationToken,
    ready: oneshot::Sender<Result<(), ServiceError>>,
    _finished: oneshot::Sender<()>,
) -> Result<(), ServiceError> {
    let failure = error("ACP 后台服务仅支持 Windows");
    let _ = ready.send(Err(failure.clone()));
    Err(failure)
}
