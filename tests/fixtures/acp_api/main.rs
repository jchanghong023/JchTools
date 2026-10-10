//! AH-02/AH-04/AH-12/AH-13：官方 SDK Agent 探针；不是 OpenCode 或真实模型验收。
// 此文件同时供消费者测试导入 Plan；各目标使用不同辅助函数。
// Agent 参数 --agent <audit-root> <mode>：new-category-{missing,unknown} 仅第二个
// session/new 移除/改变 category；model-options-expand 的 set 回包发布 A/C；
// model-options-update 的 set 回包仅 B、prompt 随后通知 A/B；exit-immediately
// 在 owner handoff 前真实退出。Plan.action=permission-after-cancel 在收到
// session/cancel 后请求权限并等待 release-<tag> 才回 prompt 终态；
// extension-interaction 用 SDK 发送未知且带 sessionId 的交互，明确拒绝/超时均等待
// release-<tag> 后记录 extension-terminal 并返回 EndTurn，保持安全终结先于 HTTP 交付。
// mode=barrier-held 保留正常行为，但 Plan.barrier 不自动超时，仅 cancel/release 放行。
// mode=cancel-exit 只在真实 session/cancel 到达后以75异常退出，不回 prompt 终态。
// mode=new-response-config-update：初始探测回 A/B 后通知 A/C，后续会话报告当前 A/C；
// stdout writer 原样聚合 response/update 为一次 pipe write，consumer 保持真实 ChildStdout。
// model-set-held 仅在 hold-model-set 存在时延迟 set 回包，release-model-set 一次放行后持续有效；
// initialize-held 等待 release-initialize；两种屏障独立于 dispatch、持锁区间，有10秒看门狗。
// Plan.action=tool-events 发送真正的 ToolCall/ToolCallUpdate 通知后输出正常文本。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ConfigOptionUpdate, ContentBlock, ContentChunk,
    CreateElicitationRequest, CreateTerminalRequest, ElicitationFormMode, ElicitationSchema,
    ElicitationSessionScope, Error, InitializeRequest, InitializeResponse, KillTerminalRequest,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind, PromptRequest,
    PromptResponse, ReadTextFileRequest, ReleaseTerminalRequest, RequestPermissionRequest,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, TerminalOutputRequest, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind, WaitForTerminalExitRequest,
    WriteTextFileRequest,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder, Stdio, UntypedMessage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
fn locked(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) const MODEL_A: &str = "fixture-model-a";
pub(crate) const MODEL_B: &str = "fixture-model-b";
pub(crate) const MODEL_C: &str = "fixture-model-c";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Plan {
    pub tag: String,
    pub action: String,
    pub barrier: bool,
    pub chunks: Vec<String>,
}
impl Plan {
    pub(crate) fn new(action: &str) -> Self {
        Self {
            tag: uuid::Uuid::new_v4().to_string(),
            action: action.into(),
            barrier: false,
            chunks: vec!["第一段 α".into(), "第二段 β".into()],
        }
    }
    pub(crate) fn prompt(&self) -> String {
        format!("AH_FIXTURE {}", serde_json::to_string(self).unwrap())
    }
    pub(crate) fn text(&self) -> String {
        self.chunks.concat()
    }
}

struct Session {
    model: String,
    cwd: PathBuf,
    cancelled: bool,
}
struct State {
    root: PathBuf,
    mode: String,
    sessions: HashMap<String, Session>,
    next: u64,
}
impl State {
    fn audit(&self, event: &Value) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.root.join("audit.jsonl"))
            .unwrap();
        writeln!(file, "{event}").unwrap();
        file.flush().unwrap();
    }
}
/// 仅释放 fixture 自有 marker；future 被取消/异常退出也给后续请求留下放行机会。
struct FixtureRelease(PathBuf);
impl FixtureRelease {
    async fn wait(&self, timeout_message: &'static str) -> agent_client_protocol::Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !self.0.exists() {
            if std::time::Instant::now() >= deadline {
                return Err(Error::new(-32043, timeout_message));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }
}
impl Drop for FixtureRelease {
    fn drop(&mut self) {
        if !self.0.exists() {
            let _released = std::fs::write(&self.0, b"cleanup");
        }
    }
}
fn model_config_id(mode: &str) -> &'static str {
    match mode {
        "model-category-missing"
        | "model-category-unknown"
        | "new-category-missing"
        | "new-category-unknown" => "chosen-model",
        _ => "model",
    }
}
fn options(model: &str, mode: &str) -> Vec<SessionConfigOption> {
    vec![SessionConfigOption::select(
        model_config_id(mode),
        "模型",
        model.to_owned(),
        vec![
            SessionConfigSelectOption::new(MODEL_A, "A"),
            SessionConfigSelectOption::new(MODEL_B, "B"),
        ],
    )
    .category(SessionConfigOptionCategory::Model)]
}
fn options_for(model: &str, mode: &str, values: &[&'static str]) -> Vec<SessionConfigOption> {
    vec![SessionConfigOption::select(
        model_config_id(mode),
        "模型",
        model.to_owned(),
        values
            .iter()
            .map(|value| SessionConfigSelectOption::new(*value, *value))
            .collect::<Vec<_>>(),
    )
    .category(SessionConfigOptionCategory::Model)]
}
fn delta(
    cx: &ConnectionTo<Client>,
    id: &SessionId,
    text: String,
) -> agent_client_protocol::Result<()> {
    cx.send_notification(SessionNotification::new(
        id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(text.into())),
    ))
}
async fn callbacks(
    plan: &Plan,
    id: &SessionId,
    cwd: &Path,
    cx: &ConnectionTo<Client>,
) -> agent_client_protocol::Result<String> {
    match plan.action.as_str() {
        "permission" | "permission-denied" => {
            let mut choices = vec![PermissionOption::new(
                "reject",
                "拒绝",
                PermissionOptionKind::RejectOnce,
            )];
            if plan.action == "permission" {
                choices.push(PermissionOption::new(
                    "once",
                    "一次",
                    PermissionOptionKind::AllowOnce,
                ));
                choices.push(PermissionOption::new(
                    "always",
                    "始终",
                    PermissionOptionKind::AllowAlways,
                ));
            }
            let response = cx
                .send_request(RequestPermissionRequest::new(
                    id.clone(),
                    ToolCallUpdate::new(
                        "fixture-tool",
                        ToolCallUpdateFields::new()
                            .title("权限探针")
                            .kind(ToolKind::Execute),
                    ),
                    choices,
                ))
                .block_task()
                .await?;
            Ok(serde_json::to_string(&response).unwrap())
        }
        "files" => {
            let path = cwd.join(format!("{}.txt", plan.tag));
            let content = format!("first\n{}\nlast\n", plan.tag);
            cx.send_request(WriteTextFileRequest::new(id.clone(), path.clone(), content))
                .block_task()
                .await?;
            let response = cx
                .send_request(ReadTextFileRequest::new(id.clone(), path).line(2).limit(1))
                .block_task()
                .await?;
            Ok(response.content)
        }
        "file-missing" => {
            let response = cx
                .send_request(ReadTextFileRequest::new(
                    id.clone(),
                    cwd.join(format!("missing-{}", plan.tag)),
                ))
                .block_task()
                .await?;
            Ok(response.content)
        }
        "elicitation" => {
            let response = cx
                .send_request(CreateElicitationRequest::new(
                    ElicitationFormMode::new(
                        ElicitationSessionScope::new(id.clone()),
                        ElicitationSchema::new().string("answer", true),
                    ),
                    "不要编造回答",
                ))
                .block_task()
                .await?;
            Ok(serde_json::to_string(&response).unwrap())
        }
        "terminal" | "terminal-kill" | "terminal-held" => {
            let child = cx
                .send_request(
                    CreateTerminalRequest::new(
                        id.clone(),
                        std::env::current_exe()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    )
                    .args(vec![
                        "--terminal".into(),
                        cwd.to_string_lossy().into_owned(),
                        plan.tag.clone(),
                        plan.action.clone(),
                    ])
                    .cwd(cwd.to_path_buf())
                    .output_byte_limit(4096),
                )
                .block_task()
                .await?;
            let terminal = child.terminal_id;
            if plan.action == "terminal-held" {
                return Ok(serde_json::to_string(&terminal).unwrap());
            }
            if plan.action == "terminal-kill" {
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !cwd.join(format!("terminal-{}.pid", plan.tag)).exists() {
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::new(-32001, "终端未启动"));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                cx.send_request(KillTerminalRequest::new(id.clone(), terminal.clone()))
                    .block_task()
                    .await?;
            }
            let exited = cx
                .send_request(WaitForTerminalExitRequest::new(
                    id.clone(),
                    terminal.clone(),
                ))
                .block_task()
                .await?;
            let first = cx
                .send_request(TerminalOutputRequest::new(id.clone(), terminal.clone()))
                .block_task()
                .await?;
            let second = cx
                .send_request(TerminalOutputRequest::new(id.clone(), terminal.clone()))
                .block_task()
                .await?;
            cx.send_request(ReleaseTerminalRequest::new(id.clone(), terminal.clone()))
                .block_task()
                .await?;
            let released = cx
                .send_request(TerminalOutputRequest::new(id.clone(), terminal))
                .block_task()
                .await;
            Ok(json!({"wait":exited,"first":first,"second":second,"released_error":released.is_err()}).to_string())
        }
        _ => Ok(plan.text()),
    }
}
async fn turn(
    state: &Mutex<State>,
    request: &PromptRequest,
    responder: Responder<PromptResponse>,
    cx: &ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let input = request
        .prompt
        .iter()
        .filter_map(|block| {
            if let ContentBlock::Text(text) = block {
                Some(text.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let plan: Plan = input
        .rsplit("AH_FIXTURE ")
        .next()
        .and_then(|text| text.lines().next())
        .and_then(|text| serde_json::from_str(text).ok())
        .ok_or_else(|| Error::new(-32602, "缺少有效探针命令"))?;
    let cancellation = responder.cancellation();
    let (root, cwd, model) = {
        let mut state = locked(state);
        let session = state
            .sessions
            .get_mut(&request.session_id.to_string())
            .unwrap();
        session.cancelled = false;
        let cwd = session.cwd.clone();
        let model = session.model.clone();
        state.audit(&json!({"event":"started","tag":plan.tag,"session":request.session_id,"pid":std::process::id(),"model":model,"prompt":input}));
        (state.root.clone(), cwd, model)
    };
    if plan.action == "crash" {
        std::process::exit(73);
    }
    if plan.action == "disconnect" {
        return Err(Error::new(-32045, "fixture connection retired"));
    }
    if plan.action == "error" {
        return responder
            .respond_with_error(Error::new(-32042, format!("fixture failure {}", plan.tag)));
    }
    if plan.action == "permission-after-cancel" {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !locked(state).sessions[&request.session_id.to_string()].cancelled {
            if std::time::Instant::now() >= deadline {
                return responder.respond_with_error(Error::new(-32043, "cancel timeout"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let outcome = cx
            .send_request(RequestPermissionRequest::new(
                request.session_id.clone(),
                ToolCallUpdate::new("cancelled-tool", ToolCallUpdateFields::new()),
                vec![PermissionOption::new(
                    "always",
                    "始终",
                    PermissionOptionKind::AllowAlways,
                )],
            ))
            .block_task()
            .await;
        let outcome = match outcome {
            Ok(response) => json!({"response":response}),
            Err(error) => json!({"error":error}),
        };
        locked(state)
            .audit(&json!({"event":"permission-after-cancel","tag":plan.tag,"outcome":outcome}));
        while !root.join(format!("release-{}", plan.tag)).exists() {
            if std::time::Instant::now() >= deadline {
                return responder
                    .respond_with_error(Error::new(-32043, "permission release timeout"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        return responder.respond(PromptResponse::new(StopReason::Cancelled));
    }
    if plan.action == "extension-interaction" {
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            cx.send_request(UntypedMessage::new(
                "_fixture/ask_user",
                json!({"sessionId":request.session_id,"question":"合成交互"}),
            )?)
            .block_task(),
        )
        .await;
        let outcome = match outcome {
            Ok(Ok(answer)) => json!({"answer":answer}),
            Ok(Err(error)) => json!({"error":error}),
            Err(_) => json!({"timed_out":true}),
        };
        locked(state).audit(&json!({"event":"extension-result","tag":plan.tag,"outcome":outcome}));
        let release = FixtureRelease(root.join(format!("release-{}", plan.tag)));
        if let Err(error) = release.wait("extension release timeout").await {
            return responder.respond_with_error(error);
        }
        locked(state).audit(&json!({"event":"extension-terminal","tag":plan.tag,"session":request.session_id,"pid":std::process::id()}));
        // Agent 忽略交互失败仍返回 EndTurn：适配层必须保留回调失败，不能伪报 HTTP 成功。
        return responder.respond(PromptResponse::new(StopReason::EndTurn));
    }
    if locked(state).mode == "model-options-update" {
        let config_options = options(&model, "model-options-update");
        cx.send_notification(SessionNotification::new(
            request.session_id.clone(),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(config_options.clone())),
        ))?;
        locked(state).audit(
            &json!({"event":"config-update","tag":plan.tag,"config_options":config_options}),
        );
    }
    if plan.action == "tool-events" {
        let tool_id = format!("fixture-tool-{}", plan.tag);
        cx.send_notification(SessionNotification::new(
            request.session_id.clone(),
            SessionUpdate::ToolCall(
                ToolCall::new(tool_id.clone(), "工具事件探针")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::InProgress),
            ),
        ))?;
        locked(state).audit(&json!({"event":"tool-event","tag":plan.tag,"session":request.session_id,"kind":"tool_call","tool_call_id":tool_id}));
        cx.send_notification(SessionNotification::new(
            request.session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_id.clone(),
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            )),
        ))?;
        locked(state).audit(&json!({"event":"tool-event","tag":plan.tag,"session":request.session_id,"kind":"tool_call_update","tool_call_id":tool_id}));
    }
    // 首段在外部 barrier 释放前发送；消费者必须实际收到它才能释放 barrier。
    if plan.action == "text" {
        if let Some(first) = plan.chunks.first() {
            delta(cx, &request.session_id, first.clone())?;
        }
    }
    if plan.barrier {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !root.join(format!("release-{}", plan.tag)).exists() {
            let cancelled = cancellation.is_cancelled()
                || locked(state).sessions[&request.session_id.to_string()].cancelled;
            if cancelled {
                locked(state)
                    .audit(&json!({"event":"cancelled","tag":plan.tag,"pid":std::process::id()}));
                return responder.respond(PromptResponse::new(StopReason::Cancelled));
            }
            if locked(state).mode != "barrier-held" && std::time::Instant::now() >= deadline {
                return responder.respond_with_error(Error::new(-32043, "barrier timeout"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let result = callbacks(&plan, &request.session_id, &cwd, cx).await;
    match result {
        Ok(text) => {
            if plan.action == "text" || plan.action == "tool-events" {
                let first = usize::from(plan.action == "text");
                for chunk in plan.chunks.iter().skip(first) {
                    delta(cx, &request.session_id, chunk.clone())?;
                }
            } else {
                delta(cx, &request.session_id, text)?;
            }
            locked(state).audit(
                &json!({"event":"completed","tag":plan.tag,"model":model,"pid":std::process::id()}),
            );
            responder.respond(PromptResponse::new(StopReason::EndTurn))
        }
        Err(error) => responder.respond_with_error(error),
    }
}
/// fixture 独占的 stdout；不经 Stdout 的行缓冲，burst 只有一次底层 pipe write。
struct PipeStdout;
impl std::io::Write for PipeStdout {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        let mut written = 0;
        // SAFETY: stdout 是进程自有的同步 pipe，bytes 在调用期间有效。
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::WriteFile(
                std::io::stdout().as_raw_handle(),
                bytes.as_ptr(),
                bytes.len().try_into().unwrap(),
                &raw mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 只识别边界和 sessionId；转发 SDK 原始字节，不重新编码 JSON-RPC。
/// SDK 每帧 flush；response 的 flush 必须成功，否则后续通知被同一发送队列卡住。
/// 缓存上限和 watchdog 使缺失通知/错误帧失败，而不是无限等待或增长。
struct NewResponseBurstWriter<W> {
    writer: W,
    root: PathBuf,
    pending: Vec<u8>,
    response_len: usize,
    session: Option<String>,
    deadline: tokio::sync::watch::Sender<Option<tokio::time::Instant>>,
}
impl<W: Write> NewResponseBurstWriter<W> {
    fn accept(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        use std::io::{Error, ErrorKind};
        const LIMIT: usize = 4096;
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if self.pending.len().saturating_add(count) > LIMIT {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "fixture frame/burst exceeds 4096 bytes",
            ));
        }
        if self.pending.is_empty() {
            self.deadline
                .send_replace(Some(tokio::time::Instant::now() + Duration::from_secs(5)));
        }
        self.pending.extend_from_slice(&bytes[..count]);
        if self.pending.last() != Some(&b'\n') {
            return Ok(count);
        }
        let frame: Value = serde_json::from_slice(&self.pending[self.response_len..])?;
        if let Some(session) = &self.session {
            if frame["method"] != "session/update"
                || frame["params"]["sessionId"].as_str() != Some(session.as_str())
                || frame["params"]["update"]["sessionUpdate"] != "config_option_update"
            {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "session/new must be followed by its config update",
                ));
            }
            let written = self.writer.write(&self.pending)?;
            if written != self.pending.len() {
                // 不补写并伪称一次 burst；短写必须明确失败且不生成成功 marker。
                return Err(Error::new(
                    ErrorKind::WriteZero,
                    "fixture burst was not written in one pipe write",
                ));
            }
            std::fs::write(
                self.root.join(format!("new-response-burst-{session}")),
                b"response-and-config-update-in-one-pipe-write",
            )?;
            self.session = None;
            self.response_len = 0;
        } else if let Some(session) = frame["result"]["sessionId"].as_str() {
            self.session = Some(session.to_owned());
            self.response_len = self.pending.len();
            return Ok(count);
        } else {
            self.writer.write_all(&self.pending)?;
        }
        self.pending.clear();
        self.deadline.send_replace(None);
        Ok(count)
    }
}
impl<W: Write + Unpin> futures::io::AsyncWrite for NewResponseBurstWriter<W> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(self.get_mut().accept(bytes))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // 缓存中的分片/response 已被接受；不能等待尚未送进此 writer 的下一帧。
        std::task::Poll::Ready(self.get_mut().writer.flush())
    }
    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        std::task::Poll::Ready(if this.pending.is_empty() {
            this.writer.flush()
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "incomplete fixture frame/response-update pair",
            ))
        })
    }
}

/// stdin 阻塞读取只在独立线程；容量 1 的队列避免阻塞 SDK 的发送/dispatch actor。
struct FixtureStdin {
    receiver: tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    offset: usize,
}
impl FixtureStdin {
    fn new() -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin().lock();
            let mut bytes = [0; 4096];
            loop {
                match stdin.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => {
                        if sender.blocking_send(Ok(bytes[..count].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        let _ = sender.blocking_send(Err(error));
                        break;
                    }
                }
            }
        });
        Self {
            receiver,
            pending: Vec::new(),
            offset: 0,
        }
    }
}
impl futures::io::AsyncRead for FixtureStdin {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::task::Poll;
        let this = self.get_mut();
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.offset == this.pending.len() {
            match futures::ready!(this.receiver.poll_recv(cx)) {
                Some(Ok(pending)) => {
                    this.pending = pending;
                    this.offset = 0;
                }
                Some(Err(error)) => return Poll::Ready(Err(error)),
                None => return Poll::Ready(Ok(0)),
            }
        }
        let count = bytes.len().min(this.pending.len() - this.offset);
        bytes[..count].copy_from_slice(&this.pending[this.offset..this.offset + count]);
        this.offset += count;
        Poll::Ready(Ok(count))
    }
}

async fn serve(root: PathBuf, mode: String) -> agent_client_protocol::Result<()> {
    std::fs::create_dir_all(&root).unwrap();
    let burst_root = (mode == "new-response-config-update").then(|| root.clone());
    let state = Arc::new(Mutex::new(State {
        root,
        mode,
        sessions: HashMap::new(),
        next: 0,
    }));
    locked(&state).audit(&json!({"event":"spawn","pid":std::process::id(),"cwd":std::env::current_dir().unwrap(),"argv":std::env::args().collect::<Vec<_>>(),"omp_jchtools_discovery":std::env::var("OMP_JCHTOOLS_DISCOVERY").ok()}));
    if locked(&state).mode == "exit-immediately" {
        std::process::exit(74);
    }
    let agent = Agent
        .builder()
        .name("JchTools SDK fixture")
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: InitializeRequest, responder, cx| {
                    let release = {
                        let state = locked(&state);
                        state.audit(
                            &json!({"event":"initialize","capabilities":request.client_capabilities}),
                        );
                        if state.mode.starts_with("handshake-error") {
                            return responder
                                .respond_with_error(Error::new(-32044, "fixture initialize failed"));
                        }
                        if state.mode == "initialize-held" {
                            let release = FixtureRelease(state.root.join("release-initialize"));
                            state.audit(&json!({"event":"initialize-held","pid":std::process::id()}));
                            Some(release)
                        } else {
                            None
                        }
                    };
                    let response = InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(AgentCapabilities::new());
                    if let Some(release) = release {
                        // 不阻塞 SDK dispatch；cancel/其他会话仍可到达。锁已在上方词法块释放。
                        cx.spawn(async move {
                            let result = release.wait("initialize release timeout").await;
                            responder.respond_with_result(result.map(|()| response))
                        })
                    } else {
                        responder.respond(response)
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: NewSessionRequest, responder, cx| {
                    let mut state = locked(&state);
                    state.next += 1;
                    let id = SessionId::new(format!("fixture-session-{}", state.next));
                    state.audit(&json!({"event":"session","session":id,"cwd":request.cwd,"config_id":model_config_id(&state.mode)}));
                    state.sessions.insert(
                        id.to_string(),
                        Session {
                            model: MODEL_A.into(),
                            cwd: request.cwd,
                            cancelled: false,
                        },
                    );
                    let response = NewSessionResponse::new(id.clone());
                    let mut config_options = options(MODEL_A, &state.mode);
                    if state.next > 1 {
                        match state.mode.as_str() {
                            "new-category-missing" => config_options[0].category = None,
                            "new-category-unknown" => {
                                config_options[0].category = Some(
                                    SessionConfigOptionCategory::Other("_vendor_unknown".into()),
                                );
                            }
                            "new-response-config-update" => {
                                // 初始通知已更新提供方目录；新会话直接报告当前 A/C。
                                // 不能假设客户端在返回 new 响应时已处理下一条独立通知。
                                config_options =
                                    options_for(MODEL_A, &state.mode, &[MODEL_A, MODEL_C]);
                            }
                            _ => {}
                        }
                    }
                    state.audit(&json!({"event":"session-options","session":response.session_id,"config_options":config_options}));
                    responder.respond(if state.mode == "no-models" {
                        response
                    } else {
                        response.config_options(config_options)
                    })?;
                    if state.mode == "new-response-config-update" {
                        let config_options = options_for(MODEL_A, &state.mode, &[MODEL_A, MODEL_C]);
                        // 两次 SDK 发布之间无 await；stdout writer 收齐两帧后原样一次写入。
                        cx.send_notification(SessionNotification::new(
                            id.clone(),
                            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                                config_options.clone(),
                            )),
                        ))?;
                        state.audit(&json!({"event":"new-config-update","session":id,"config_options":config_options}));
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: SetSessionConfigOptionRequest, responder, cx| {
                    let value = serde_json::to_value(&request.value).unwrap();
                    let model = value
                        .as_str()
                        .or_else(|| value.get("value").and_then(Value::as_str))
                        .unwrap_or("");
                    let (config_options, release) = {
                        let mut state = locked(&state);
                        let allowed = if state.mode == "model-options-expand" {
                            vec![MODEL_A, MODEL_B, MODEL_C]
                        } else if state.mode == "new-response-config-update" {
                            vec![MODEL_A, MODEL_C]
                        } else {
                            vec![MODEL_A, MODEL_B]
                        };
                        if request.config_id.to_string() != model_config_id(&state.mode)
                            || !allowed.contains(&model)
                        {
                            return responder
                                .respond_with_error(Error::new(-32602, "unknown fixture model"));
                        }
                        state
                            .sessions
                            .get_mut(&request.session_id.to_string())
                            .unwrap()
                            .model = model.into();
                        let mut config_options = options(model, &state.mode);
                        match state.mode.as_str() {
                            "model-category-missing" => config_options[0].category = None,
                            "model-category-unknown" => {
                                config_options[0].category = Some(
                                    SessionConfigOptionCategory::Other("_vendor_unknown".into()),
                                );
                            }
                            "model-options-expand" | "new-response-config-update" => {
                                config_options = options_for(model, &state.mode, &[MODEL_A, MODEL_C]);
                            }
                            "model-options-update" if model == MODEL_B => {
                                config_options = options_for(model, &state.mode, &[MODEL_B]);
                            }
                            _ => {}
                        }
                        state.audit(&json!({"event":"model","session":request.session_id,"model":model,
                            "config_id":request.config_id,"config_options":config_options}));
                        let release_path = state.root.join("release-model-set");
                        let release = if state.mode == "model-set-held"
                            && state.root.join("hold-model-set").exists()
                            && !release_path.exists()
                        {
                            let release = FixtureRelease(release_path);
                            state.audit(&json!({"event":"model-set-held","session":request.session_id,"pid":std::process::id()}));
                            Some(release)
                        } else {
                            None
                        };
                        (config_options, release)
                    };
                    let response = SetSessionConfigOptionResponse::new(config_options);
                    if let Some(release) = release {
                        // 不持 MutexGuard 跨 await，也不把其它会话/cancel 卡在 SDK dispatch。
                        cx.spawn(async move {
                            let result = release.wait("model set release timeout").await;
                            responder.respond_with_result(result.map(|()| response))
                        })
                    } else {
                        responder.respond(response)
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |request: PromptRequest, responder, cx| {
                    let state = state.clone();
                    let task_cx = cx.clone();
                    cx.spawn(async move { turn(&state, &request, responder, &task_cx).await })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = state.clone();
                async move |request: CancelNotification, _cx| {
                    let mut state = locked(&state);
                    if let Some(session) = state.sessions.get_mut(&request.session_id.to_string()) {
                        session.cancelled = true;
                    }
                    state.audit(&json!({"event":"cancel-received","session":request.session_id,"pid":std::process::id()}));
                    if state.mode == "cancel-exit" {
                        // 锁仍持有，barrier轮次不能先观察cancelled并回正常终态。
                        state.audit(&json!({"event":"cancel-exit","session":request.session_id,"pid":std::process::id(),"code":75}));
                        std::process::exit(75);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        );
    if let Some(root) = burst_root {
        let (deadline, mut deadlines) = tokio::sync::watch::channel(None);
        let writer = NewResponseBurstWriter {
            writer: PipeStdout,
            root,
            pending: Vec::new(),
            response_len: 0,
            session: None,
            deadline,
        };
        let watchdog = async move {
            loop {
                let deadline = *deadlines.borrow_and_update();
                if let Some(deadline) = deadline {
                    tokio::select! {
                        () = tokio::time::sleep_until(deadline) => {
                            return Err(Error::internal_error().data("fixture response/update pair timed out"));
                        }
                        changed = deadlines.changed() => {
                            if changed.is_err() { return std::future::pending().await; }
                        }
                    }
                } else if deadlines.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        };
        tokio::select! {
            result = agent.connect_to(agent_client_protocol::ByteStreams::new(writer, FixtureStdin::new())) => result,
            result = watchdog => result,
        }
    } else {
        agent.connect_to(Stdio::new()).await
    }
}
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("--terminal") {
        let root = PathBuf::from(&args[1]);
        let tag = &args[2];
        std::fs::write(
            root.join(format!("terminal-{tag}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();
        // 每个 marker 一次写入，避免格式化宏多段写让两条管道的 prefix/tag 交错。
        // 断言仍校验完整 stdout/stderr 字节与实际退出/释放，不依赖两条管道先后顺序。
        std::io::stdout()
            .write_all(format!("stdout:{tag}\n").as_bytes())
            .unwrap();
        std::io::stderr()
            .write_all(format!("stderr:{tag}\n").as_bytes())
            .unwrap();
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        if args[3] != "terminal" {
            while !root.join(format!("terminal-release-{tag}")).exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        std::process::exit(7);
    }
    assert_eq!(
        args.first().map(String::as_str),
        Some("--agent"),
        "仅供测试：需要 --agent <audit-root> [mode]"
    );
    let root = PathBuf::from(args.get(1).expect("缺少探针工作区"));
    let mode = args.get(2).cloned().unwrap_or_else(|| "normal".into());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let held = mode == "handshake-error-held" || mode == "disconnect-held";
    let result = runtime.block_on(serve(root.clone(), mode));
    if held {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: SDK连接已返回，stdout协议transport永久退役；本进程之后不再访问stdout。
        // 关闭本探针自有pipe写端，模拟通信EOF而非结束Agent进程；不编造任何JSON-RPC字节。
        unsafe { windows_sys::Win32::Foundation::CloseHandle(std::io::stdout().as_raw_handle()) };
        while !root.join("release-exit").exists() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if let Err(error) = result {
        eprintln!("SDK fixture: {error}");
        std::process::exit(2);
    }
}

/// 只在测试 crate 内编译：真实 socket 消费者与官方 SDK 探针连接装配。
#[cfg(test)]
pub(crate) mod consumer {
    use super::{Plan, MODEL_A};
    use jchtools::acp_api::{
        self, agent_process_channel, AgentConnection, AgentExit, AgentProcessCommand,
        BackendHandle, ServiceConfig, ServicePhase, ServiceStatus,
    };
    use serde_json::{json, Value};
    use std::{
        collections::BTreeMap,
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        path::{Path, PathBuf},
        process::Stdio,
        time::{Duration, Instant},
    };
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    pub(crate) fn audit(root: &Path) -> Vec<Value> {
        std::fs::read_to_string(root.join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
    pub(crate) fn wait_event(root: &Path, event: &str, tag: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(found) = audit(root)
                .into_iter()
                .find(|entry| entry["event"] == event && entry["tag"] == tag)
            {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "未观察到 {event}/{tag}: {:?}",
                audit(root)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub(crate) fn release(root: &Path, plan: &Plan) {
        std::fs::write(root.join(format!("release-{}", plan.tag)), b"release").unwrap();
    }
    pub(crate) fn unused_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
    pub(crate) fn config(executable: &str, root: &Path, port: u16, mode: &str) -> ServiceConfig {
        ServiceConfig {
            executable: executable.into(),
            arguments: vec![
                "--agent".into(),
                root.to_string_lossy().into_owned(),
                mode.into(),
            ],
            port,
        }
    }
    pub(crate) fn body(plan: &Plan, stream: bool) -> Value {
        json!({"model":MODEL_A,"messages":[{"role":"user","content":plan.prompt()}],"stream":stream})
    }
    pub(crate) fn completion(value: &Value) -> &str {
        assert_eq!(value["object"], "chat.completion");
        assert_eq!(value["choices"][0]["message"]["role"], "assistant");
        assert_eq!(value["choices"][0]["finish_reason"], "stop");
        assert!(value.get("error").is_none(), "{value}");
        value["choices"][0]["message"]["content"].as_str().unwrap()
    }
    pub(crate) struct Http {
        pub status: u16,
        pub headers: BTreeMap<String, String>,
        reader: BufReader<TcpStream>,
        chunked: bool,
        left: usize,
        ended: bool,
    }
    impl Http {
        pub(crate) fn open(
            port: u16,
            method: &str,
            route: &str,
            body: Option<&Value>,
            session: Option<&str>,
        ) -> Self {
            let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            let body = body.map(Value::to_string).unwrap_or_default();
            write!(socket,"{method} {route} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",body.len()).unwrap();
            if let Some(session) = session {
                write!(socket, "X-JchTools-Session-ID: {session}\r\n").unwrap();
            }
            write!(socket, "\r\n{body}").unwrap();
            socket.flush().unwrap();
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
            let mut headers = BTreeMap::<String, String>::new();
            loop {
                line.clear();
                assert!(reader.read_line(&mut line).unwrap() > 0, "HTTP标头被截断");
                if line == "\r\n" {
                    break;
                }
                let (key, value) = line.split_once(':').unwrap();
                headers.insert(key.to_ascii_lowercase(), value.trim().into());
            }
            let chunked = headers
                .get("transfer-encoding")
                .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
            let left = headers
                .get("content-length")
                .map_or(usize::MAX, |value| value.parse().unwrap());
            Self {
                status,
                headers,
                reader,
                chunked,
                left: if chunked { 0 } else { left },
                ended: false,
            }
        }
        fn byte(&mut self) -> Option<u8> {
            if self.ended {
                return None;
            }
            if self.chunked && self.left == 0 {
                let mut line = String::new();
                assert!(
                    self.reader.read_line(&mut line).unwrap() > 0,
                    "HTTP chunk被截断"
                );
                self.left =
                    usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
                if self.left == 0 {
                    self.ended = true;
                    return None;
                }
            }
            if self.left == 0 {
                self.ended = true;
                return None;
            }
            let mut byte = [0_u8; 1];
            match self.reader.read(&mut byte).unwrap() {
                0 => {
                    self.ended = true;
                    return None;
                }
                1 => {}
                _ => unreachable!(),
            }
            if self.left != usize::MAX {
                self.left -= 1;
            }
            if self.chunked && self.left == 0 {
                let mut crlf = [0_u8; 2];
                self.reader.read_exact(&mut crlf).unwrap();
                assert_eq!(&crlf, b"\r\n");
            }
            Some(byte[0])
        }
        pub(crate) fn json(mut self) -> Value {
            let mut bytes = Vec::new();
            while let Some(byte) = self.byte() {
                bytes.push(byte);
            }
            serde_json::from_slice(&bytes).unwrap()
        }
        pub(crate) fn sse(&mut self) -> String {
            let mut event = Vec::new();
            loop {
                let byte = self.byte().expect("SSE缺少终态");
                event.push(byte);
                if event.ends_with(b"\n\n") || event.ends_with(b"\r\n\r\n") {
                    return String::from_utf8(event).unwrap();
                }
            }
        }
        pub(crate) fn delta(&mut self) -> String {
            loop {
                let event = self.sse();
                let data = event
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap();
                assert_ne!(data, "[DONE]", "首段前不得完成");
                let value: Value = serde_json::from_str(data).unwrap();
                assert!(value.get("error").is_none(), "{event}");
                if let Some(text) = value["choices"][0]["delta"]["content"].as_str() {
                    return text.into();
                }
            }
        }
        pub(crate) fn finish_stream(&mut self) -> String {
            let mut text = String::new();
            let mut stop = false;
            loop {
                let event = self.sse();
                if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) {
                    if data == "[DONE]" {
                        assert!(stop, "缺少finish_reason");
                        return text;
                    }
                    let value: Value = serde_json::from_str(data).unwrap();
                    assert!(value.get("error").is_none(), "{event}");
                    if let Some(delta) = value["choices"][0]["delta"]["content"].as_str() {
                        text.push_str(delta);
                    }
                    stop |= value["choices"][0]["finish_reason"] == "stop";
                }
            }
        }
    }
    pub(crate) struct Server {
        pub temp: tempfile::TempDir,
        pub root: PathBuf,
        pub workspace: PathBuf,
        pub port: u16,
        pub backend: BackendHandle,
        runtime: tokio::runtime::Runtime,
        listener: tokio::task::JoinHandle<()>,
    }
    impl Server {
        pub(crate) fn start(executable: &str, mode: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("探针 空格");
            std::fs::create_dir(&root).unwrap();
            let workspace = temp.path().join("acp-workspace");
            std::fs::create_dir(&workspace).unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .unwrap();
            let (backend,port,listener)=runtime.block_on(async {
                let (status,_receiver)=tokio::sync::watch::channel(ServiceStatus{phase:ServicePhase::Starting,..ServiceStatus::default()});
                let owner_status = status.clone();
                let (processes,mut commands)=agent_process_channel();
                let root=root.clone(); let agent_workspace=workspace.clone(); let executable=executable.to_owned(); let mode=mode.to_owned();
                tokio::spawn(async move {
                    while let Some(AgentProcessCommand::Connect{reply})=commands.recv().await {
                        let mut child=tokio::process::Command::new(&executable).args(["--agent",root.to_str().unwrap(),&mode])
                            .current_dir(&agent_workspace).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
                        let pid=child.id().unwrap();
                        // 与真实 owner 一致：进程所有者在 handoff 前发布，退出时清除自己的 PID。
                        owner_status.send_modify(|status| status.agent_pid = Some(pid));
                        let transport=agent_client_protocol::ByteStreams::new(child.stdin.take().unwrap().compat_write(),child.stdout.take().unwrap().compat());
                        let (exit,exit_rx)=tokio::sync::watch::channel(None); let (finished,finished_rx)=tokio::sync::oneshot::channel();
                        if mode == "exit-immediately" {
                            let exited = child.wait().await.unwrap();
                            owner_status.send_modify(|status| {
                                if status.agent_pid == Some(pid) { status.agent_pid = None; }
                            });
                            std::fs::write(root.join("owner-exited"), pid.to_string()).unwrap();
                            exit.send(Some(AgentExit { code: exited.code(), error: None })).unwrap();
                            reply.send(Ok(AgentConnection{transport,pid,exit:exit_rx,finished})).unwrap();
                            continue;
                        }
                        reply.send(Ok(AgentConnection{transport,pid,exit:exit_rx,finished})).unwrap();
                        let owner_status = owner_status.clone();
                        tokio::spawn(async move {
                            let status=tokio::select! {status=child.wait()=>status,_result=finished_rx=>child.wait().await};
                            owner_status.send_modify(|status| {
                                if status.agent_pid == Some(pid) { status.agent_pid = None; }
                            });
                            let _sent=exit.send(Some(AgentExit{code:status.as_ref().ok().and_then(std::process::ExitStatus::code),error:status.err().map(|error|error.to_string())}));
                        });
                    }
                });
                let backend=acp_api::acp::start_backend(processes,workspace.clone(),status);
                let listener=tokio::net::TcpListener::bind(("127.0.0.1",0)).await.unwrap(); let port=listener.local_addr().unwrap().port();
                let router=acp_api::http::router(backend.clone());
                let serving=tokio::spawn(async move {acp_api::http::serve(listener,router,tokio_util::sync::CancellationToken::new()).await.unwrap();});
                (backend,port,serving)
            });
            let server = Self {
                temp,
                root,
                workspace,
                port,
                backend,
                runtime,
                listener,
            };
            server.wait_ready();
            server
        }
        pub(crate) fn wait_ready(&self) {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let status = self.backend.status();
                if matches!(status.phase, ServicePhase::Ready | ServicePhase::Error) {
                    return;
                }
                assert!(Instant::now() < deadline, "初始化超时: {status:?}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        pub(crate) fn post(&self, plan: &Plan) -> Http {
            Http::open(
                self.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(plan, false)),
                None,
            )
        }
        pub(crate) fn shutdown(&self) {
            self.runtime.block_on(self.backend.stop()).unwrap();
            self.listener.abort();
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            // 测试失败也关闭自有连接；tokio child 的 kill_on_drop 只用于探针异常收尾。
            let _stopped = self.runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(15), self.backend.stop()).await
            });
            self.listener.abort();
        }
    }
}
