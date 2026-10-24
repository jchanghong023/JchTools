//! AH-02 / AH-04 / AH-12 / AH-13：单连接、请求级路由、4+16 调度与安全收尾。
//! 模型 catalog 与未消费的探测会话共享状态；首轮接管 spare 时冻结，避免会话配置污染新会话能力。
//! 模型配置按会话协商；后台排空/停止阶段保持到外层释放 Agent 与 HTTP 资源。
//! foreground 已完成的本地收尾保留 Io 归属，不由 SDK 的连接退出结果覆盖或重复执行。
mod actor;
mod backend;
mod callbacks;
mod errors;
mod models;
mod retired;
mod session;
mod terminals;

use crate::acp_api::{
    backend_channel, AcceptedRequest, AgentExit, AgentProcessHandle, BackendCommand, BackendHandle,
    ChatMessage, FinishReason, MessageRole, ModelDescriptor, PromptInput, RequestEvent, RequestId,
    ServiceError, ServiceErrorKind, ServicePhase, ServiceStatus, SessionKey, MAX_EXECUTING,
    MAX_WAITING,
};
use actor::{Actor, RoundContext, RoundResult, SessionState};
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
use backend::run_backend;
use callbacks::{Callbacks, Route};
use errors::{disconnected, error, sdk_error};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use models::{model_selector, model_selector_from_option, session_model_allows, SessionModel};
use retired::{finish_retired, reap_retired};
use session::{create_session, round, validate_history};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

#[cfg(test)]
use actor::{Pending, SessionSlot};
#[cfg(test)]
use backend::{run_backend_inner, RetirementObserver};
#[cfg(test)]
use models::ModelSelector;
#[cfg(test)]
use session::finalize_round;

pub fn start_backend(
    processes: AgentProcessHandle,
    workspace: PathBuf,
    status: watch::Sender<ServiceStatus>,
) -> BackendHandle {
    let (handle, commands) = backend_channel(status.subscribe());
    tokio::spawn(
        run_backend(processes, workspace, status, commands).instrument(
            jchtools_foundation::logging::operation_span("acp_agent", "backend"),
        ),
    );
    handle
}

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod tests;
