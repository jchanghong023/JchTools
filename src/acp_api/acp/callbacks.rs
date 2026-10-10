//! AH-02 / AH-13：所有回调经官方 SDK；工作目录不是文件访问沙箱。
use super::{model_selector_from_option, terminals::Terminals, SessionModel};
use crate::acp_api::{AgentTransport, RequestEvent, ServiceError, ServiceErrorKind};
use agent_client_protocol::{
    schema::v1::{
        ContentBlock, CreateElicitationRequest, CreateTerminalRequest, ElicitationScope,
        KillTerminalRequest, PermissionOptionKind, ReadTextFileRequest, ReadTextFileResponse,
        ReleaseTerminalRequest, RequestPermissionOutcome, RequestPermissionRequest,
        RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigId, SessionId,
        SessionNotification, SessionUpdate, TerminalOutputRequest, WaitForTerminalExitRequest,
        WriteTextFileRequest, WriteTextFileResponse,
    },
    Agent, ConnectionTo, UntypedMessage,
};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io::{BufRead, BufReader},
    ops::AsyncFnOnce,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) fn invalid(message: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message.to_owned())
}
pub(super) fn io_error(message: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(message.to_owned())
}

pub(super) struct Route {
    pub events: mpsc::UnboundedSender<RequestEvent>,
    pub cancellation: CancellationToken,
    pub text: String,
    pub failure: Option<ServiceError>,
}
#[derive(Default)]
pub(super) struct Routes {
    pub known: HashSet<SessionId>,
    pub model_id: Option<SessionConfigId>,
    pub models: HashMap<SessionId, SessionModel>,
    pub active: HashMap<SessionId, Route>,
}
#[derive(Clone, Copy)]
enum CallbackLifetime {
    Round,
    Session,
}
struct CallbackTask {
    handle: tokio::task::JoinHandle<()>,
    lifetime: CallbackLifetime,
}
#[derive(Clone)]
pub(super) struct Callbacks {
    pub routes: Arc<Mutex<Routes>>,
    pub terminals: Terminals,
    tasks: Arc<Mutex<HashMap<SessionId, Vec<CallbackTask>>>>,
}
impl Callbacks {
    pub(super) fn new(workspace: PathBuf) -> Self {
        Self {
            routes: Arc::new(Mutex::new(Routes::default())),
            terminals: Terminals::new(workspace),
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    fn spawn(
        &self,
        session: SessionId,
        lifetime: CallbackLifetime,
        future: impl Future<Output = Result<(), agent_client_protocol::Error>> + Send + 'static,
    ) {
        let handle = tokio::spawn(async move {
            let _ = future.await;
        });
        self.tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(session)
            .or_default()
            .push(CallbackTask { handle, lifetime });
    }
    // Prompt 完成不等于 terminal/release；未完成的 handle 回调属于会话。
    pub(super) async fn complete_round(
        &self,
        session: &SessionId,
    ) -> Result<(), agent_client_protocol::Error> {
        let mut result = Ok(());
        let tasks = {
            let mut sessions = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
            let tasks = sessions.entry(session.clone()).or_default();
            let mut finished = Vec::new();
            let mut index = 0;
            while index < tasks.len() {
                if matches!(tasks[index].lifetime, CallbackLifetime::Round)
                    || tasks[index].handle.is_finished()
                {
                    finished.push(tasks.swap_remove(index).handle);
                } else {
                    index += 1;
                }
            }
            finished
        };
        for task in tasks {
            if task.await.is_err() && result.is_ok() {
                result = Err(io_error("ACP 回调收尾失败"));
            }
        }
        result
    }
    pub(super) async fn shutdown(&self) -> Result<(), agent_client_protocol::Error> {
        let mut result = self.terminals.shutdown().await;
        let tasks = self
            .tasks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .flat_map(|(_, tasks)| tasks)
            .collect::<Vec<_>>();
        for task in tasks {
            if task.handle.await.is_err() && result.is_ok() {
                result = Err(io_error("ACP 回调收尾失败"));
            }
        }
        result
    }
    pub(super) fn check(&self, session: &SessionId) -> Result<(), agent_client_protocol::Error> {
        if self
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active
            .get(session)
            .is_some_and(|route| !route.cancellation.is_cancelled())
        {
            Ok(())
        } else {
            Err(invalid("ACP 会话没有有效的在途轮次"))
        }
    }
    pub(super) fn fail(&self, session: &SessionId, message: &str) {
        if let Some(route) = self
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .active
            .get_mut(session)
        {
            // Agent/适配回调失败不归责 HTTP 输入；SDK 回调仍保留原协议错误。
            route
                .failure
                .get_or_insert_with(|| ServiceError::new(ServiceErrorKind::Internal, message));
            route.cancellation.cancel();
        }
    }
    fn notification(&self, notification: SessionNotification) {
        if let SessionUpdate::ConfigOptionUpdate(update) = &notification.update {
            let routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
            if let (Some(model), Some(id)) = (
                routes.models.get(&notification.session_id),
                routes.model_id.as_ref(),
            ) {
                let updated = update
                    .config_options
                    .iter()
                    .find(|option| &option.id == id)
                    .ok_or_else(|| {
                        ServiceError::new(
                            ServiceErrorKind::ModelsUnavailable,
                            "Agent 更新未保留会话模型配置",
                        )
                    })
                    .and_then(model_selector_from_option);
                *model.lock().unwrap_or_else(PoisonError::into_inner) = updated;
            }
            return;
        }
        if let SessionUpdate::AgentMessageChunk(chunk) = notification.update {
            let mut routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(route) = routes.active.get_mut(&notification.session_id) {
                if route.cancellation.is_cancelled() {
                    return;
                }
                if let ContentBlock::Text(text) = chunk.content {
                    // 文本立即发往本轮 channel；保留的文本仅用于下一轮完整历史前缀。
                    route.text.push_str(&text.text);
                    if route
                        .events
                        .send(RequestEvent::TextDelta(text.text))
                        .is_err()
                    {
                        route.cancellation.cancel();
                    }
                } else {
                    route.failure = Some(ServiceError::new(
                        ServiceErrorKind::InvalidRequest,
                        "Agent 返回了不支持的非文本内容",
                    ));
                    route.cancellation.cancel();
                }
            }
        }
    }
    async fn read(
        &self,
        request: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, agent_client_protocol::Error> {
        self.check(&request.session_id)?;
        if !request.path.is_absolute() || request.line == Some(0) {
            return Err(invalid("文件路径须为绝对路径，起始行须大于零"));
        }
        let path = request.path;
        let line = request.line;
        let limit = request.limit;
        let content = tokio::task::spawn_blocking(move || {
            let file = std::fs::File::open(path).map_err(|_| io_error("文件读取失败"))?;
            read_range(BufReader::new(file), line, limit)
        })
        .await
        .map_err(|_| io_error("文件读取任务失败"))??;
        Ok(ReadTextFileResponse::new(content))
    }
    async fn write(
        &self,
        request: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse, agent_client_protocol::Error> {
        self.check(&request.session_id)?;
        if !request.path.is_absolute() {
            return Err(invalid("文件路径须为绝对路径"));
        }
        tokio::task::spawn_blocking(move || {
            std::fs::write(request.path, request.content).map_err(|_| io_error("文件写入失败"))
        })
        .await
        .map_err(|_| io_error("文件写入任务失败"))??;
        Ok(WriteTextFileResponse::new())
    }
    fn permission(
        &self,
        request: &RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, agent_client_protocol::Error> {
        {
            let routes = self.routes.lock().unwrap_or_else(PoisonError::into_inner);
            if routes
                .active
                .get(&request.session_id)
                .is_some_and(|route| route.cancellation.is_cancelled())
            {
                return Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ));
            }
        }
        self.check(&request.session_id)?;
        let option = request
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            .or_else(|| {
                request
                    .options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            });
        if let Some(option) = option {
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                )),
            ))
        } else {
            self.fail(&request.session_id, "Agent 权限请求没有允许选项");
            Err(invalid("Agent 权限请求没有允许选项"))
        }
    }
}
fn read_range(
    mut reader: impl BufRead,
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, agent_client_protocol::Error> {
    if limit == Some(0) {
        return Ok(String::new());
    }
    for _ in 1..line.unwrap_or(1) {
        if reader
            .skip_until(b'\n')
            .map_err(|_| io_error("文件读取失败"))?
            == 0
        {
            return Ok(String::new());
        }
    }
    let mut content = String::new();
    if let Some(limit) = limit {
        for _ in 0..limit {
            if reader
                .read_line(&mut content)
                .map_err(|_| io_error("文件读取失败或不是 UTF-8 文本"))?
                == 0
            {
                break;
            }
        }
    } else {
        reader
            .read_to_string(&mut content)
            .map_err(|_| io_error("文件读取失败或不是 UTF-8 文本"))?;
    }
    Ok(content)
}

pub(super) async fn connect<R>(
    transport: AgentTransport,
    callbacks: Callbacks,
    main: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<R, agent_client_protocol::Error>,
) -> Result<R, agent_client_protocol::Error> {
    let notifications = callbacks.clone();
    let permissions = callbacks.clone();
    let reads = callbacks.clone();
    let writes = callbacks.clone();
    let creates = callbacks.clone();
    let outputs = callbacks.clone();
    let waits = callbacks.clone();
    let kills = callbacks.clone();
    let releases = callbacks.clone();
    let elicitations = callbacks.clone();
    let extensions = callbacks.clone();
    agent_client_protocol::Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                notifications.notification(notification);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                responder.respond_with_result(permissions.permission(&request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReadTextFileRequest, responder, _cx| {
                let callbacks = reads.clone();
                reads.spawn(
                    request.session_id.clone(),
                    CallbackLifetime::Round,
                    async move { responder.respond_with_result(callbacks.read(request).await) },
                );
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WriteTextFileRequest, responder, _cx| {
                let callbacks = writes.clone();
                writes.spawn(
                    request.session_id.clone(),
                    CallbackLifetime::Round,
                    async move { responder.respond_with_result(callbacks.write(request).await) },
                );
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateTerminalRequest, responder, _cx| {
                let callbacks = creates.clone();
                creates.spawn(
                    request.session_id.clone(),
                    CallbackLifetime::Round,
                    async move {
                        let result = match callbacks.check(&request.session_id) {
                            Ok(()) => callbacks.terminals.create(request),
                            Err(error) => Err(error),
                        };
                        responder.respond_with_result(result)
                    },
                );
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: TerminalOutputRequest, responder, _cx| {
                responder.respond_with_result(outputs.terminals.output(&request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WaitForTerminalExitRequest, responder, _cx| {
                let callbacks = waits.clone();
                waits.spawn(
                    request.session_id.clone(),
                    CallbackLifetime::Session,
                    async move {
                        responder.respond_with_result(callbacks.terminals.wait(request).await)
                    },
                );
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: KillTerminalRequest, responder, _cx| {
                let callbacks = kills.clone();
                kills.spawn(
                    request.session_id.clone(),
                    CallbackLifetime::Session,
                    async move {
                        responder.respond_with_result(callbacks.terminals.kill(request).await)
                    },
                );
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReleaseTerminalRequest, responder, _cx| {
                let callbacks = releases.clone();
                releases.spawn(
                    request.session_id.clone(),
                    CallbackLifetime::Session,
                    async move {
                        responder.respond_with_result(callbacks.terminals.release(request).await)
                    },
                );
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateElicitationRequest, responder, _cx| {
                if let ElicitationScope::Session(scope) = request.scope() {
                    elicitations.fail(&scope.session_id, "不支持非权限交互请求");
                }
                responder.respond_with_error(invalid("不支持非权限交互请求"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        // 官方公开的 UntypedMessage handler 在 SDK session retry fallback 前明确拒绝。
        .on_receive_request(
            async move |request: UntypedMessage, responder, _cx| {
                if let Some(session) = request
                    .params
                    .get("sessionId")
                    .and_then(serde_json::Value::as_str)
                {
                    extensions.fail(
                        &SessionId::new(session.to_owned()),
                        "不支持未知的非权限交互请求",
                    );
                }
                responder.respond_with_error(agent_client_protocol::Error::method_not_found())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, main)
        .await
}

#[cfg(test)]
mod tests {
    use super::read_range;
    // 覆盖 AH-13 / AH-A11：1-based 行号和限制保持实际文件换行。
    #[test]
    fn read_line_range_preserves_actual_newlines() {
        assert_eq!(
            read_range(std::io::Cursor::new("一\r\n二\n三"), Some(2), Some(1)).unwrap(),
            "二\n"
        );
        assert_eq!(
            read_range(std::io::Cursor::new("a\nb"), Some(4), None).unwrap(),
            ""
        );
        assert_eq!(
            read_range(std::io::Cursor::new("a\nb"), None, Some(0)).unwrap(),
            ""
        );
    }
}
