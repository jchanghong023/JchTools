// future_incompatible 只支持 crate 级；作为测试模块导入时继承测试 crate 的配置。
#![cfg_attr(not(test), warn(future_incompatible))]
#![warn(
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
// terminal 在 wait 主命令退出后，另以合成 stdout/stderr 的完整行等待输出快照；
// terminal-pwd 用真实子进程 current_dir 写入 cwd marker，验证省略 cwd 的回调默认目录。
// Plan.action=native-command 仅在 prompt 首行以 /fixture-add 开头时执行两整数求和；
// 否则作为普通文本返回 Plan.text，不能靠查找正文中的命令或回显证明 slash 识别。
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

#[path = "probe.rs"]
mod probe;
#[path = "serve.rs"]
mod serve;

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

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("--terminal") {
        let root = PathBuf::from(&args[1]);
        let tag = &args[2];
        // 先完整写入再原子发布 PID marker；存在即表示 PID 可解析。
        let pid_path = root.join(format!("terminal-{tag}.pid"));
        let pending_pid = root.join(format!("terminal-{tag}.pid.tmp"));
        std::fs::write(&pending_pid, std::process::id().to_string()).unwrap();
        std::fs::rename(&pending_pid, &pid_path).unwrap();
        if args[3].starts_with("terminal-limit-") {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all("A中🙂β\n".repeat(150_000).as_bytes())
                .unwrap();
            stdout
                .write_all(format!("尾部:{tag}:完成🙂\n").as_bytes())
                .unwrap();
            stdout.flush().unwrap();
            std::process::exit(7);
        }
        if args[3] == "terminal-pwd" || args[3] == "terminal-pwd-explicit" {
            let current_dir = std::env::current_dir()
                .unwrap()
                .into_os_string()
                .into_string()
                .expect("terminal current directory must be valid UTF-8");
            std::fs::write(
                root.join(format!("terminal-{tag}.cwd")),
                current_dir.as_bytes(),
            )
            .unwrap();
        }
        std::io::stdout()
            .write_all(format!("stdout:{tag}\n").as_bytes())
            .unwrap();
        std::io::stderr()
            .write_all(format!("stderr:{tag}\n").as_bytes())
            .unwrap();
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        if args[3] != "terminal" && !args[3].starts_with("terminal-pwd") {
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
    let result = runtime.block_on(serve::serve(root.clone(), mode));
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
    use jchtools_acp_api::acp_api::{
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
        let bytes = match std::fs::read(root.join("audit.jsonl")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => panic!("无法读取探针审计: {error}"),
        };
        // 写入和轮询可交错：只暂缓末尾未提交的半行，已换行记录的读取/
        // JSON 错误必须失败，不能漏计 spawn 或把损坏审计当成没有事件。
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| line.last() == Some(&b'\n'))
            .map(|line| serde_json::from_slice(line).expect("探针审计完整行必须是有效 JSON"))
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
    pub(crate) fn body(plan: &Plan, stream: bool, cwd: &Path) -> Value {
        json!({"model":MODEL_A,"messages":[{"role":"user","content":plan.prompt()}],"stream":stream,"cwd":cwd})
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
        // None 是连接关闭定界；Some 必须读足长度或完整结束 chunk，不能把 EOF 当成功。
        left: Option<usize>,
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
                .map(|value| value.parse().unwrap());
            Self {
                status,
                headers,
                reader,
                chunked,
                left: if chunked { Some(0) } else { left },
                ended: false,
            }
        }
        fn byte(&mut self) -> Option<u8> {
            if self.ended {
                return None;
            }
            if self.chunked && self.left == Some(0) {
                let mut line = String::new();
                assert!(
                    self.reader.read_line(&mut line).unwrap() > 0,
                    "HTTP chunk被截断"
                );
                assert!(line.ends_with("\r\n"), "HTTP chunk标头被截断");
                self.left = Some(
                    usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap(),
                );
                if self.left == Some(0) {
                    // 零长度 chunk 之后仍须收到 trailer 区的终止空行，才是完整消息。
                    loop {
                        line.clear();
                        assert!(
                            self.reader.read_line(&mut line).unwrap() > 0 && line.ends_with("\r\n"),
                            "HTTP chunk结束帧被截断"
                        );
                        if line == "\r\n" {
                            break;
                        }
                    }
                    self.ended = true;
                    return None;
                }
            }
            if self.left == Some(0) {
                self.ended = true;
                return None;
            }
            let mut byte = [0_u8; 1];
            match self.reader.read(&mut byte).unwrap() {
                0 => {
                    assert!(
                        !self.chunked && self.left.is_none(),
                        "HTTP响应帧被截断：尚有 {:?} 字节未读取，chunked={}",
                        self.left,
                        self.chunked
                    );
                    self.ended = true;
                    return None;
                }
                1 => {}
                _ => unreachable!(),
            }
            if let Some(left) = &mut self.left {
                *left -= 1;
            }
            if self.chunked && self.left == Some(0) {
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
                        self.assert_stream_end();
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
        /// 已消费 [DONE] 后验证 HTTP 消息完整结束，不能静默丢弃尾部事件。
        pub(crate) fn assert_stream_end(&mut self) {
            while let Some(byte) = self.byte() {
                assert!(byte.is_ascii_whitespace(), "[DONE]后仍有额外SSE数据");
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
                Some(&body(plan, false, &self.workspace)),
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

    #[cfg(test)]
    mod tests {
        use super::Http;
        use std::{
            io::Write,
            net::{TcpListener, TcpStream},
        };

        fn stream(body: &str, chunked: bool) -> Http {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let reader = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut writer, _) = listener.accept().unwrap();
            if chunked {
                write!(writer, "{:x}\r\n{body}\r\n0\r\n\r\n", body.len()).unwrap();
            } else {
                writer.write_all(body.as_bytes()).unwrap();
            }
            Http {
                status: 200,
                headers: Default::default(),
                reader: std::io::BufReader::new(reader),
                chunked,
                left: Some(if chunked { 0 } else { body.len() }),
                ended: false,
            }
        }

        const COMPLETE: &str = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"合成正文\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );

        /// AH-04/AH-A04：正常结束仍须完整消费 Content-Length/chunked 消息边界。
        #[test]
        fn sse_completion_accepts_complete_http_frames() {
            for chunked in [false, true] {
                assert_eq!(stream(COMPLETE, chunked).finish_stream(), "合成正文");
            }
        }

        /// AH-04/AH-12：终态后串入文本或错误不能被消费者丢弃后伪报成功。
        #[test]
        fn sse_completion_rejects_events_after_done() {
            for chunked in [false, true] {
                for trailing in [
                    "data: {\"choices\":[{\"delta\":{\"content\":\"串入其他请求\"}}]}\n\n",
                    "data: {\"error\":{\"message\":\"晚到的失败\"}}\n\n",
                ] {
                    let mut response = stream(&format!("{COMPLETE}{trailing}"), chunked);
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        response.finish_stream()
                    }));
                    assert!(
                        outcome.is_err(),
                        "终态后额外事件必须失败，chunked={chunked}"
                    );
                }
            }
        }
    }
}
