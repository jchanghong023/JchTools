//! AH：HTTP、官方 ACP 客户端和后台之间的请求级通道契约。

use std::fmt;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub const DEFAULT_PORT: u16 = 8765;
pub const MAX_EXECUTING: usize = 4;
pub const MAX_WAITING: usize = 16;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceConfig {
    pub executable: String,
    pub arguments: Vec<String>,
    pub port: u16,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            executable: String::new(),
            arguments: Vec::new(),
            port: DEFAULT_PORT,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServicePhase {
    #[default]
    Unconfigured,
    Starting,
    Ready,
    Draining,
    Stopping,
    Stopped,
    Error,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceStatus {
    pub phase: ServicePhase,
    pub saved_config: Option<ServiceConfig>,
    pub running_config: Option<ServiceConfig>,
    pub service_pid: Option<u32>,
    pub agent_pid: Option<u32>,
    pub executing: usize,
    pub waiting: usize,
    pub error: Option<String>,
}

impl ServiceStatus {
    pub fn pending_apply(&self) -> bool {
        if self.service_pid.is_none()
            || !matches!(
                self.phase,
                ServicePhase::Ready | ServicePhase::Starting | ServicePhase::Error
            )
        {
            return false;
        }
        let Some(saved) = self.saved_config.as_ref() else {
            return false;
        };
        let applicable = match self.running_config.as_ref() {
            Some(running) => saved != running,
            None => self.phase == ServicePhase::Error,
        };
        applicable && super::settings::validate_config(saved).is_ok()
    }
}

/// AH-15：发现只投影当前运行状态，不携带配置、进程参数或错误详情。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceDiscovery {
    pub protocol_version: u32,
    pub service_id: DiscoveryServiceId,
    pub instance_id: uuid::Uuid,
    pub phase: ServicePhase,
    pub base_url: Option<String>,
    pub execution_mode: DiscoveryExecutionMode,
    pub capabilities: DiscoveryCapabilities,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DiscoveryServiceId {
    #[serde(rename = "jchtools-acp-http")]
    JchToolsAcpHttp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryExecutionMode {
    ServerAgent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiscoveryCapabilities {
    pub text: bool,
    pub streaming: bool,
    pub client_tools: bool,
    pub server_tools: bool,
}

impl ServiceDiscovery {
    pub fn from_status(instance_id: uuid::Uuid, status: &ServiceStatus) -> Self {
        Self {
            protocol_version: 1,
            service_id: DiscoveryServiceId::JchToolsAcpHttp,
            instance_id,
            phase: status.phase,
            base_url: if status.phase == ServicePhase::Ready {
                status
                    .running_config
                    .as_ref()
                    .map(|config| format!("http://127.0.0.1:{}", config.port))
            } else {
                None
            },
            execution_mode: DiscoveryExecutionMode::ServerAgent,
            capabilities: DiscoveryCapabilities {
                text: true,
                streaming: true,
                client_tools: false,
                server_tools: true,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceErrorKind {
    InvalidRequest,
    InvalidConfig,
    ModelsUnavailable,
    ModelUnavailable,
    HistoryMismatch,
    SessionUnavailable,
    QueueFull,
    NotReady,
    Stopping,
    Cancelled,
    AgentDisconnected,
    Io,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceError {
    pub kind: ServiceErrorKind,
    pub message: String,
}

impl ServiceError {
    pub fn new(kind: ServiceErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServiceError {}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RequestId(pub String);

impl RequestId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionKey(pub String);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    Developer,
    User,
    Assistant,
}

impl MessageRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptInput {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub session: Option<SessionKey>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDescriptor {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinishReason {
    Stop,
    Length,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestEvent {
    TextDelta(String),
    Completed { reason: FinishReason },
    Failed(ServiceError),
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedRequest {
    pub request_id: RequestId,
    pub session: SessionKey,
    pub model: String,
}

#[derive(Debug)]
pub struct RequestHandle {
    pub accepted: AcceptedRequest,
    pub events: mpsc::UnboundedReceiver<RequestEvent>,
    pub cancellation: CancellationHandle,
}

/// 请求所有权随 HTTP 响应移动；丢弃响应只取消本轮，不释放共享 Agent。
#[derive(Debug)]
pub struct CancellationHandle {
    request_id: RequestId,
    commands: mpsc::UnboundedSender<BackendCommand>,
    token: CancellationToken,
    armed: bool,
}

impl CancellationHandle {
    pub fn cancel(&self) {
        if self.armed && !self.token.is_cancelled() {
            self.token.cancel();
            // 通道已关闭意味着后台已结束；请求 token 仍记录取消，不伪造完成。
            let _result = self.commands.send(BackendCommand::Cancel {
                request_id: self.request_id.clone(),
            });
        }
    }

    /// 仅在收到真实终态后解除断连取消。
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancellationHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Debug)]
pub enum BackendCommand {
    Models {
        reply: oneshot::Sender<Result<Vec<ModelDescriptor>, ServiceError>>,
    },
    Submit {
        request_id: RequestId,
        input: PromptInput,
        events: mpsc::UnboundedSender<RequestEvent>,
        cancellation: CancellationToken,
        reply: oneshot::Sender<Result<AcceptedRequest, ServiceError>>,
    },
    Cancel {
        request_id: RequestId,
    },
    GracefulDrain {
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
    Stop {
        reply: oneshot::Sender<Result<(), ServiceError>>,
    },
}

#[derive(Clone, Debug)]
pub struct BackendHandle {
    commands: mpsc::UnboundedSender<BackendCommand>,
    status: watch::Receiver<ServiceStatus>,
}

pub fn backend_channel(
    status: watch::Receiver<ServiceStatus>,
) -> (BackendHandle, mpsc::UnboundedReceiver<BackendCommand>) {
    let (commands, receiver) = mpsc::unbounded_channel();
    (BackendHandle { commands, status }, receiver)
}

fn backend_closed() -> ServiceError {
    ServiceError::new(ServiceErrorKind::AgentDisconnected, "ACP 后台连接已关闭")
}

impl BackendHandle {
    pub fn status(&self) -> ServiceStatus {
        self.status.borrow().clone()
    }

    pub fn subscribe_status(&self) -> watch::Receiver<ServiceStatus> {
        self.status.clone()
    }

    pub async fn models(&self) -> Result<Vec<ModelDescriptor>, ServiceError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(BackendCommand::Models { reply })
            .map_err(|_| backend_closed())?;
        response.await.map_err(|_| backend_closed())?
    }

    pub async fn submit(&self, input: PromptInput) -> Result<RequestHandle, ServiceError> {
        let request_id = RequestId::new();
        let (events, receiver) = mpsc::unbounded_channel();
        let token = CancellationToken::new();
        // 在 admission 等待之前安装 guard，HTTP 在首个响应之前断连也可取消。
        let cancellation = CancellationHandle {
            request_id: request_id.clone(),
            commands: self.commands.clone(),
            token: token.clone(),
            armed: true,
        };
        let (reply, response) = oneshot::channel();
        self.commands
            .send(BackendCommand::Submit {
                request_id,
                input,
                events,
                cancellation: token,
                reply,
            })
            .map_err(|_| backend_closed())?;
        let accepted = response.await.map_err(|_| backend_closed())??;
        Ok(RequestHandle {
            accepted,
            events: receiver,
            cancellation,
        })
    }

    pub fn cancel(&self, request_id: RequestId) -> Result<(), ServiceError> {
        self.commands
            .send(BackendCommand::Cancel { request_id })
            .map_err(|_| backend_closed())
    }

    pub async fn graceful_drain(&self) -> Result<(), ServiceError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(BackendCommand::GracefulDrain { reply })
            .map_err(|_| backend_closed())?;
        response.await.map_err(|_| backend_closed())?
    }

    pub async fn stop(&self) -> Result<(), ServiceError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(BackendCommand::Stop { reply })
            .map_err(|_| backend_closed())?;
        response.await.map_err(|_| backend_closed())?
    }
}

/// SDK ByteStreams 使用 futures IO；顺序为写入 Agent stdin、读取 Agent stdout。
pub type AgentTransport = agent_client_protocol::ByteStreams<
    tokio_util::compat::Compat<tokio::process::ChildStdin>,
    tokio_util::compat::Compat<tokio::process::ChildStdout>,
>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentExit {
    pub code: Option<i32>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct AgentConnection {
    pub transport: AgentTransport,
    pub pid: u32,
    pub exit: watch::Receiver<Option<AgentExit>>,
    pub finished: oneshot::Sender<Result<(), ServiceError>>,
}

#[derive(Debug)]
pub enum AgentProcessCommand {
    Connect {
        reply: oneshot::Sender<Result<AgentConnection, ServiceError>>,
    },
}

#[derive(Clone, Debug)]
pub struct AgentProcessHandle {
    commands: mpsc::UnboundedSender<AgentProcessCommand>,
}

pub fn agent_process_channel() -> (
    AgentProcessHandle,
    mpsc::UnboundedReceiver<AgentProcessCommand>,
) {
    let (commands, receiver) = mpsc::unbounded_channel();
    (AgentProcessHandle { commands }, receiver)
}

impl AgentProcessHandle {
    pub async fn connect(&self) -> Result<AgentConnection, ServiceError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(AgentProcessCommand::Connect { reply })
            .map_err(|_| backend_closed())?;
        response.await.map_err(|_| backend_closed())?
    }
}
