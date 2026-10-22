use super::*;

/// AH-13/AH-A10：token 已取消而 prompt 尚未终结，权限回调返回 Cancelled 不是 RPC error。
#[test]
fn cancelled_round_permission_returns_cancelled_before_prompt_terminal() {
    use jchtools_acp_api::acp_api::{ChatMessage, MessageRole, PromptInput, RequestEvent};
    let server = server();
    let plan = Plan::new("permission-after-cancel");
    let cleanup = ReleaseMarker::prompt(&server.root, &plan);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut request = runtime
        .block_on(server.backend.submit(PromptInput {
            model: MODEL_A.to_owned(),
            cwd: server.workspace.clone(),
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

fn extension_attempt() -> (Value, Option<HttpOutcome>, HttpOutcome) {
    let server = server();
    let plan = Plan::new("extension-interaction");
    let mut pending = PendingHttp::start(
        ReleaseMarker::prompt(&server.root, &plan),
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(body(&plan, false, &server.workspace)),
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
        Some(&body(&stream_plan, true, &server.workspace)),
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
    response.assert_stream_end();
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
