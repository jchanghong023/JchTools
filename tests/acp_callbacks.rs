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

type HttpOutcome = (u16, Value);

// 无论测试正常完成或断言失败，先放行自有进程，再有界等待 HTTP 线程收尾。
struct PendingHttp {
    release: ReleaseMarker,
    receive: mpsc::Receiver<HttpOutcome>,
    job: Option<thread::JoinHandle<()>>,
}
impl PendingHttp {
    fn start(
        release: ReleaseMarker,
        port: u16,
        method: &'static str,
        route: &'static str,
        request: Option<Value>,
    ) -> Self {
        let (send, receive) = mpsc::channel();
        let job = thread::spawn(move || {
            let response = Http::open(port, method, route, request.as_ref(), None);
            let _sent = send.send((response.status, response.json()));
        });
        Self {
            release,
            receive,
            job: Some(job),
        }
    }
    fn finish(&mut self, observed: Option<HttpOutcome>) -> HttpOutcome {
        self.release.release();
        let result = observed.unwrap_or_else(|| {
            self.receive
                .recv_timeout(Duration::from_secs(10))
                .expect("放行后 HTTP 请求未在看门狗期限内收尾")
        });
        self.job.take().unwrap().join().unwrap();
        result
    }
}
impl Drop for PendingHttp {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            self.release.release();
            // 超时只用于防止 red 测试自身挂死，不强杀 Agent 或终端。
            if !matches!(
                self.receive.recv_timeout(Duration::from_secs(10)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                let _joined = job.join();
            }
        }
    }
}

fn extension_attempt() -> (Value, Option<HttpOutcome>, HttpOutcome) {
    let server = server();
    let plan = Plan::new("extension-interaction");
    let mut pending = PendingHttp::start(
        ReleaseMarker::prompt(&server.root, &plan),
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(body(&plan, false)),
    );
    let event = wait_event(&server.root, "extension-result", &plan.tag);
    let still_active = server.backend.status().executing;
    let before_release = match pending.receive.try_recv() {
        Ok(response) => Some(response),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => panic!("HTTP 请求线程未返回结果"),
    };
    assert!(!pending.release.0.exists(), "safe terminal 观察前不得放行");
    assert!(
        !audit(&server.root)
            .iter()
            .any(|entry| entry["event"] == "extension-terminal" && entry["tag"] == plan.tag),
        "extension-result 必须先于 safe terminal"
    );
    let eventual = pending.finish(before_release.clone());
    assert_eq!(still_active, 1, "扩展拒绝后仍须等待 Agent 安全终结");
    let terminal = audit(&server.root)
        .into_iter()
        .find(|entry| entry["event"] == "extension-terminal" && entry["tag"] == plan.tag)
        .expect("HTTP 返回失败前必须已观察到 Agent 安全终结");
    assert_eq!(terminal["tag"], plan.tag);
    (event, before_release, eventual)
}

/// AH-02/AH-13：未知且携带 sessionId 的扩展交互也须明确返回协议拒绝。
#[test]
fn unknown_session_extension_interaction_is_explicitly_rejected() {
    let (event, before_release, eventual) = extension_attempt();
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
    assert!(before_release.is_none(), "安全终结前 HTTP 仍须在途");
    assert_eq!(eventual.0, 502, "{eventual:?}");
    assert!(eventual.1.get("error").is_some());
    assert!(eventual.1.get("choices").is_none());
}

/// AH-13：扩展交互拒绝后须等 Agent 安全终结，不能把随后 EndTurn 当成功。
#[test]
fn unknown_session_extension_http_waits_for_safe_terminal_then_fails() {
    let (_, before_release, eventual) = extension_attempt();
    assert!(
        before_release.is_none(),
        "Agent 尚未安全终结时 HTTP 请求必须仍在途：{before_release:?}"
    );
    assert_eq!(
        eventual.0, 502,
        "不能把失败交互后的 EndTurn 当成功：{eventual:?}"
    );
    assert!(eventual.1.get("error").is_some());
    assert!(eventual.1.get("choices").is_none());
}

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
    let deadline = Instant::now() + Duration::from_secs(10);
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
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(agent_pid) {
        assert!(Instant::now() < deadline, "崩溃 Agent 未退出");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(process_alive(terminal_pid), "旧终端须仍未释放");
    let next = Plan::new("text");
    let mut pending = PendingHttp::start(
        cleanup,
        server.port,
        if models { "GET" } else { "POST" },
        if models {
            "/v1/models"
        } else {
            "/v1/chat/completions"
        },
        if models {
            None
        } else {
            Some(body(&next, false))
        },
    );
    // 等待响应只设防挂看门狗，不把调度速度当 SLA；放行前的存活和 marker
    // 观察证明响应/恢复没有依赖旧 terminal 自然退出。
    let before_release = match pending.receive.recv_timeout(Duration::from_secs(10)) {
        Ok(response) => Some(response),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("HTTP 请求线程未返回结果"),
    };
    assert!(
        process_alive(terminal_pid),
        "响应观察点旧 terminal 必须仍存活"
    );
    assert!(
        !pending.release.0.exists(),
        "响应观察点旧 terminal 不得提前放行"
    );
    let eventual = pending.finish(before_release.clone());
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
    let followup = Plan::new("text");
    let response = server.post(&followup);
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), followup.text());
    assert_eq!(
        wait_event(&server.root, "started", &followup.tag)["pid"],
        recovered["pid"],
        "恢复后的后续请求必须复用同一个 Agent"
    );
    let events = audit(&server.root);
    for pid in [&old["pid"], &recovered["pid"]] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "spawn" && &event["pid"] == pid)
                .count(),
            1,
            "每个生命周期只能 spawn 一次 Agent"
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2
    );
    let deadline = Instant::now() + Duration::from_secs(10);
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
    assert_eq!(response.0, 502, "{eventual:?}");
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

/// AH-05：真实 SDK 工具通知只作为 Agent 内部事件，JSON/SSE 仍交付完整文本。
#[test]
fn server_tool_events_remain_text_only() {
    let server = server();
    let json_plan = Plan::new("tool-events");
    let response = server.post(&json_plan);
    assert_eq!(response.status, 200);
    let value = response.json();
    assert_eq!(completion(&value), json_plan.text());
    for choice in value["choices"].as_array().unwrap() {
        assert!(choice["message"].is_object(), "{value}");
        assert!(choice["message"].get("tool_calls").is_none(), "{value}");
        assert!(choice["delta"].get("tool_calls").is_none(), "{value}");
        assert_eq!(choice["finish_reason"], "stop", "{value}");
    }

    let stream_plan = Plan::new("tool-events");
    let mut response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&stream_plan, true)),
        None,
    );
    assert_eq!(response.status, 200);
    assert!(
        response.headers["content-type"].starts_with("text/event-stream"),
        "{:?}",
        response.headers
    );
    let mut text = String::new();
    let mut stopped = false;
    loop {
        let event = response.sse();
        let data = event
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("真实 SSE 事件缺少 data");
        if data == "[DONE]" {
            assert!(stopped, "文本 SSE 缺少 stop 终态");
            break;
        }
        let value: Value = serde_json::from_str(data).unwrap();
        assert!(value.get("error").is_none(), "{event}");
        assert_eq!(value["object"], "chat.completion.chunk", "{event}");
        let choices = value["choices"].as_array().unwrap();
        assert!(!choices.is_empty(), "{event}");
        for choice in choices {
            assert!(choice["delta"].is_object(), "{event}");
            assert!(choice["delta"].get("tool_calls").is_none(), "{event}");
            assert!(choice["message"].get("tool_calls").is_none(), "{event}");
            if let Some(role) = choice["delta"].get("role") {
                assert_eq!(role, "assistant", "{event}");
            }
            if let Some(content) = choice["delta"].get("content") {
                text.push_str(content.as_str().expect("SSE 内容必须是文本"));
            }
            let finish = &choice["finish_reason"];
            assert!(
                finish.is_null() || finish == "stop",
                "不得把工具通知映射成 tool_calls finish_reason：{event}"
            );
            stopped |= finish == "stop";
        }
    }
    assert_eq!(text, stream_plan.text(), "工具事件不得吞掉或混入文本");

    let followup = Plan::new("text");
    let response = server.post(&followup);
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), followup.text());
    let next = wait_event(&server.root, "started", &followup.tag);
    let events = audit(&server.root);
    for plan in [&json_plan, &stream_plan] {
        let started = wait_event(&server.root, "started", &plan.tag);
        assert_eq!(started["pid"], next["pid"], "工具事件后不得换 Agent");
        let notifications = events
            .iter()
            .filter(|event| event["event"] == "tool-event" && event["tag"] == plan.tag)
            .collect::<Vec<_>>();
        assert_eq!(notifications.len(), 2, "两种官方 SDK 通知必须真实发出");
        for kind in ["tool_call", "tool_call_update"] {
            let notification = notifications
                .iter()
                .find(|event| event["kind"] == kind)
                .expect("缺少对应 SDK 工具通知审计");
            assert_eq!(notification["session"], started["session"]);
        }
        assert_eq!(
            notifications[0]["tool_call_id"],
            notifications[1]["tool_call_id"]
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1,
        "JSON、SSE 与正常后续请求只能使用唯一 Agent"
    );
}
