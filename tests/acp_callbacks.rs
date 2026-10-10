//! AH-02/AH-05/AH-13/AH-A10/AH-A11：经HTTP和官方SDK触发真实文件及终端回调。
//! SDK夹具补充验证客户端能力，不等同OpenCode实际工具路径或真实模型验收。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_api/main.rs"]
mod fixture;
use fixture::{
    consumer::{audit, body, completion, wait_event, Http, Server},
    Plan, MODEL_A,
};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn server() -> Server {
    Server::start(env!("CARGO_BIN_EXE_jchtools-acp-fixture"), "normal")
}

/// 覆盖 AH-13/AH-A10：allow-always优先于allow-once，且无允许选项必须明确失败。
#[test]
fn permission_uses_broadest_allow_and_never_fabricates_approval() {
    let server = server();
    let permission = Plan::new("permission");
    let response = server.post(&permission);
    assert_eq!(response.status, 200);
    let value = response.json();
    let outcome: Value = serde_json::from_str(completion(&value)).unwrap();
    assert_eq!(outcome["outcome"]["outcome"], "selected");
    assert_eq!(outcome["outcome"]["optionId"], "always");
    let denied = server.post(&Plan::new("permission-denied"));
    assert_eq!(denied.status, 502);
    let value = denied.json();
    assert!(value.get("error").is_some());
    assert!(value.get("choices").is_none());
}

/// 覆盖 AH-02/AH-05/AH-13/AH-A11：被宣告的文件能力确实写盘、按行读取并明确返回不存在失败。
#[test]
fn file_callbacks_write_actual_bytes_and_read_only_requested_line() {
    let server = server();
    let plan = Plan::new("files");
    let response = server.post(&plan);
    assert_eq!(response.status, 200);
    let value = response.json();
    assert_eq!(completion(&value).trim_end_matches('\n'), plan.tag);
    assert_eq!(
        std::fs::read_to_string(server.workspace.join(format!("{}.txt", plan.tag))).unwrap(),
        format!("first\n{}\nlast\n", plan.tag)
    );
    let initialization = audit(&server.root)
        .into_iter()
        .find(|event| event["event"] == "initialize")
        .unwrap();
    assert_eq!(initialization["capabilities"]["fs"]["readTextFile"], true);
    assert_eq!(initialization["capabilities"]["fs"]["writeTextFile"], true);
    let missing = server.post(&Plan::new("file-missing"));
    assert_eq!(missing.status, 502);
    assert!(missing.json().get("error").is_some());
}

/// 覆盖 AH-13：非权限交互不得伪造答案、无限等待或被转换成HTTP工具循环。
#[test]
fn non_permission_interaction_fails_explicitly() {
    let server = server();
    let response = server.post(&Plan::new("elicitation"));
    assert_eq!(response.status, 502);
    let value = response.json();
    assert!(value.get("error").is_some());
    assert!(value.get("choices").is_none());
}

/// 覆盖 AH-13/AH-A11：同一个实际终端的stdout/stderr、非零退出、重复累计查询及release。
#[test]
fn terminal_callbacks_capture_wait_and_release_the_same_child() {
    let server = server();
    let plan = Plan::new("terminal");
    let response = server.post(&plan);
    assert_eq!(response.status, 200);
    let value = response.json();
    let terminal: Value = serde_json::from_str(completion(&value)).unwrap();
    assert_eq!(terminal["wait"]["exitCode"], 7);
    let output = terminal["first"]["output"].as_str().unwrap();
    assert!(
        output.contains(&format!("stdout:{}", plan.tag)),
        "{terminal}"
    );
    assert!(
        output.contains(&format!("stderr:{}", plan.tag)),
        "{terminal}"
    );
    assert_eq!(
        terminal["first"], terminal["second"],
        "output不得重启命令或消耗累计输出"
    );
    assert_eq!(terminal["released_error"], true);
    let pid = std::fs::read_to_string(server.workspace.join(format!("terminal-{}.pid", plan.tag)))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(!process_alive(pid), "已wait/release的终端进程仍在运行");
    let initialization = audit(&server.root)
        .into_iter()
        .find(|event| event["event"] == "initialize")
        .unwrap();
    assert_eq!(initialization["capabilities"]["terminal"], true);
}

/// 覆盖 AH-13/AH-A11：kill只结束目标终端，后续模型请求仍使用唯一Agent。
#[test]
fn terminal_kill_reaps_child_without_terminating_agent() {
    let server = server();
    let plan = Plan::new("terminal-kill");
    let response = server.post(&plan);
    assert_eq!(response.status, 200);
    let value = response.json();
    let terminal: Value = serde_json::from_str(completion(&value)).unwrap();
    assert_eq!(terminal["released_error"], true);
    let pid = std::fs::read_to_string(server.workspace.join(format!("terminal-{}.pid", plan.tag)))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(!process_alive(pid));
    let next = Plan::new("text");
    assert_eq!(completion(&server.post(&next).json()), next.text());
    assert_eq!(
        wait_event(&server.root, "started", &plan.tag)["pid"],
        wait_event(&server.root, "started", &next.tag)["pid"]
    );
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
}

fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_FAILED, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    // SAFETY: 只以同步权限打开测试自己记录的子进程 PID；不修改或终止进程。
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: 有效进程句柄，零时等待只查询进程对象是否已完成退出。
    let waited = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: 查询完成，释放本次唯一拥有的Windows句柄。
    unsafe { CloseHandle(handle) };
    assert_ne!(waited, WAIT_FAILED, "无法观察自有进程的实际退出");
    waited == WAIT_TIMEOUT
}

// 在任何断言 panic 前释放本测试自有 barrier/terminal；随后 Server 才安全收尾。
struct ReleaseMarker(PathBuf);
impl ReleaseMarker {
    fn prompt(root: &Path, plan: &Plan) -> Self {
        Self(root.join(format!("release-{}", plan.tag)))
    }
    fn terminal(workspace: &Path, plan: &Plan) -> Self {
        Self(workspace.join(format!("terminal-release-{}", plan.tag)))
    }
    fn release(&self) {
        std::fs::write(&self.0, b"release").unwrap();
    }
}
impl Drop for ReleaseMarker {
    fn drop(&mut self) {
        let _released = std::fs::write(&self.0, b"release");
    }
}

/// AH-13/AH-A10：token 已取消而 prompt 尚未终结，权限回调返回 Cancelled 不是 RPC error。
#[test]
fn cancelled_round_permission_returns_cancelled_before_prompt_terminal() {
    use jchtools::acp_api::{ChatMessage, MessageRole, PromptInput, RequestEvent};
    let server = server();
    let plan = Plan::new("permission-after-cancel");
    let cleanup = ReleaseMarker::prompt(&server.root, &plan);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut request = runtime
        .block_on(server.backend.submit(PromptInput {
            model: MODEL_A.into(),
            messages: vec![ChatMessage {
                role: MessageRole::User,
                text: plan.prompt(),
            }],
            session: None,
        }))
        .unwrap();
    wait_event(&server.root, "started", &plan.tag);
    request.cancellation.cancel();
    let event = wait_event(&server.root, "permission-after-cancel", &plan.tag);
    let still_active = server.backend.status().executing;
    let terminal_before_release = request.events.try_recv();
    cleanup.release();
    let terminal = runtime
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(5), request.events.recv()).await
        })
        .unwrap();
    request.cancellation.disarm();
    assert_eq!(still_active, 1, "必须在 prompt 终态前观察权限结果");
    assert!(matches!(
        terminal_before_release,
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(
        event["outcome"]["response"]["outcome"]["outcome"], "cancelled",
        "{event}"
    );
    assert!(event["outcome"].get("error").is_none(), "{event}");
    assert_eq!(terminal, Some(RequestEvent::Cancelled));
}

fn extension_attempt() -> (Value, Option<(u16, Value)>, (u16, Value)) {
    let server = server();
    let plan = Plan::new("extension-interaction");
    let cleanup = ReleaseMarker::prompt(&server.root, &plan);
    let request = body(&plan, false);
    let port = server.port;
    let (send, receive) = mpsc::channel();
    let job = thread::spawn(move || {
        let response = Http::open(port, "POST", "/v1/chat/completions", Some(&request), None);
        let result = (response.status, response.json());
        send.send(result.clone()).unwrap();
        result
    });
    let event = wait_event(&server.root, "extension-result", &plan.tag);
    let before_release = receive.recv_timeout(Duration::from_millis(500)).ok();
    // 旧 SDK 重试带 sessionId 的未知方法，fixture 有界超时后停在此 barrier。
    // 即使观察失败，也先放行请求再 join，不让 red 测试自己永久挂起。
    cleanup.release();
    let eventual = job.join().unwrap();
    (event, before_release, eventual)
}

/// AH-02/AH-13：未知且携带 sessionId 的扩展交互也须明确返回协议拒绝。
#[test]
fn unknown_session_extension_interaction_is_explicitly_rejected() {
    let (event, _, _) = extension_attempt();
    assert!(
        event["outcome"].get("error").is_some(),
        "未明确拒绝：{event}"
    );
    assert!(
        event["outcome"].get("answer").is_none(),
        "不能伪造答案：{event}"
    );
    assert_ne!(
        event["outcome"]["timed_out"], true,
        "不能将交互留在 SDK retry 队列"
    );
}

/// AH-13：Agent 忽略扩展交互失败仍不能让 HTTP 成功；不等人为释放 barrier。
#[test]
fn unknown_session_extension_http_fails_without_barrier_release() {
    let (_, before_release, eventual) = extension_attempt();
    let response = before_release.expect("未知交互导致 HTTP 挂起，必须立即明确失败");
    assert_eq!(
        response.0, 502,
        "不能把失败交互后的 EndTurn 当成功：{eventual:?}"
    );
    assert!(response.1.get("error").is_some());
    assert!(response.1.get("choices").is_none());
}

type HttpOutcome = (u16, Value);

fn held_terminal_failure_attempt(models: bool) -> (Option<HttpOutcome>, HttpOutcome, Value, Value) {
    let server = server();
    let held = Plan::new("terminal-held");
    let cleanup = ReleaseMarker::terminal(&server.workspace, &held);
    let response = server.post(&held);
    assert_eq!(response.status, 200);
    let terminal_key: String = serde_json::from_str(completion(&response.json())).unwrap();
    assert!(!terminal_key.is_empty());
    let old = wait_event(&server.root, "started", &held.tag);
    let terminal_pid_path = server.workspace.join(format!("terminal-{}.pid", held.tag));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !terminal_pid_path.exists() {
        assert!(Instant::now() < deadline, "held terminal 未启动");
        thread::sleep(Duration::from_millis(10));
    }
    let terminal_pid = std::fs::read_to_string(&terminal_pid_path)
        .unwrap()
        .parse()
        .unwrap();
    assert!(process_alive(terminal_pid));
    let response = server.post(&Plan::new("crash"));
    assert_eq!(response.status, 502);
    assert!(response.json().get("error").is_some());
    let agent_pid = u32::try_from(old["pid"].as_u64().unwrap()).unwrap();
    while process_alive(agent_pid) {
        assert!(Instant::now() < deadline, "崩溃 Agent 未退出");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(process_alive(terminal_pid), "旧终端须仍未释放");
    let next = Plan::new("text");
    let request = body(&next, false);
    let port = server.port;
    let (send, receive) = mpsc::channel();
    let job = thread::spawn(move || {
        let response = if models {
            Http::open(port, "GET", "/v1/models", None, None)
        } else {
            Http::open(port, "POST", "/v1/chat/completions", Some(&request), None)
        };
        let result = (response.status, response.json());
        send.send(result.clone()).unwrap();
        result
    });
    let before_release = receive.recv_timeout(Duration::from_secs(1)).ok();
    cleanup.release();
    let eventual = job.join().unwrap();
    let recovered = if models {
        let recovery = Plan::new("text");
        let response = server.post(&recovery);
        assert_eq!(response.status, 200);
        assert_eq!(completion(&response.json()), recovery.text());
        wait_event(&server.root, "started", &recovery.tag)
    } else {
        assert_eq!(eventual.0, 200);
        assert_eq!(completion(&eventual.1), next.text());
        wait_event(&server.root, "started", &next.tag)
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(terminal_pid) {
        assert!(Instant::now() < deadline, "released terminal 未回收");
        thread::sleep(Duration::from_millis(10));
    }
    (before_release, eventual, old, recovered)
}

/// AH-13：死 Agent 的旧 terminal 自然收尾不得阻塞 /models 明确不可用响应。
#[test]
fn unreleased_terminal_after_agent_exit_does_not_block_models_response() {
    let (before_release, eventual, old, recovered) = held_terminal_failure_attempt(true);
    let response = before_release.expect("旧 terminal 未释放时 /models 命令被阻塞");
    // 断裂 Agent 的原始错误须明确回传，不把它改成泛化的未就绪或成功列表。
    assert_eq!(
        response.1["error"]["code"], "agent_disconnected",
        "{eventual:?}"
    );
    assert!(response.1.get("error").is_some());
    assert_ne!(old["pid"], recovered["pid"]);
}

/// AH-13/AH-12：Agent 已退出，旧 terminal 仍在时新会话可恢复唯一 Agent。
#[test]
fn unreleased_terminal_after_agent_exit_does_not_block_new_request_recovery() {
    let (before_release, eventual, old, recovered) = held_terminal_failure_attempt(false);
    let response = before_release.expect("新 HTTP 请求被旧 terminal shutdown 永久阻塞");
    assert_eq!(response.0, 200, "{eventual:?}");
    assert_eq!(response.1["object"], "chat.completion");
    assert_ne!(old["pid"], recovered["pid"]);
}
