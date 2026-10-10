//! AH-03/AH-04/AH-05/AH-12/AH-13：真实 TCP → Axum → 产品适配 → 官方 SDK Agent。
//! 这些是合成协议测试，不声称 AH-A03～AH-A09 的真实 OpenCode 验收通过。
#![allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fixtures/acp_api/main.rs"]
mod fixture;
use fixture::{
    consumer::{audit, body, completion, release, wait_event, Http, Server},
    Plan, MODEL_A, MODEL_B, MODEL_C,
};
use serde_json::{json, Value};
use std::{
    thread,
    time::{Duration, Instant},
};

fn server(mode: &str) -> Server {
    Server::start(env!("CARGO_BIN_EXE_jchtools-acp-fixture"), mode)
}
fn wait_counts(server: &Server, executing: usize, waiting: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = server.backend.status();
        if status.executing == executing && status.waiting == waiting {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "容量未到 {executing}+{waiting}: {status:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// 覆盖 AH-04/AH-A03：列表来自协商的实际选项，选择 B 到达 Agent；新请求不同会话。
#[test]
fn negotiated_models_are_selected_and_json_is_a_completion() {
    let server = server("normal");
    let models = Http::open(server.port, "GET", "/v1/models", None, None);
    assert_eq!(models.status, 200);
    let list = models.json();
    assert_eq!(list["object"], "list");
    let mut ids = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, vec![MODEL_A, MODEL_B]);
    for model in list["data"].as_array().unwrap() {
        assert!(
            model["created"].as_u64().is_some(),
            "Models.created 必须是非负整数：{model}"
        );
    }
    for model in [MODEL_B, MODEL_A] {
        let plan = Plan::new("text");
        let mut request = body(&plan, false);
        request["model"] = json!(model);
        let response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&request),
            None,
        );
        assert_eq!(response.status, 200);
        let value = response.json();
        assert_eq!(value["model"], model);
        assert_eq!(completion(&value), plan.text());
        assert_eq!(
            wait_event(&server.root, "started", &plan.tag)["model"],
            model
        );
    }
    let starts = audit(&server.root)
        .into_iter()
        .filter(|event| event["event"] == "started")
        .collect::<Vec<_>>();
    assert_ne!(starts[0]["session"], starts[1]["session"]);
    let mut request = body(&Plan::new("text"), false);
    request["model"] = json!("invented-alias");
    assert_eq!(
        Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&request),
            None
        )
        .status,
        400
    );
    server.shutdown();
}

/// 覆盖 AH-04/AH-02：已协商模型 selector 的身份不随可选 category 元数据消失。
/// ACP v1 session-config-options：category 不得成为正确性前提，configId 使用协商 id。
/// 本回归是独立协议兼容问题，不冒称复现真实 OMP 的 session/new 工作区错误。
#[test]
fn negotiated_model_identity_survives_missing_response_category() {
    assert_negotiated_model_category_change("model-category-missing", &Value::Null);
}

/// 覆盖 AH-04/AH-02：未知自定义 category 不能使已协商模型 selector 的后续轮次失效。
#[test]
fn negotiated_model_identity_survives_unknown_response_category() {
    assert_negotiated_model_category_change("model-category-unknown", &json!("_vendor_unknown"));
}

fn assert_negotiated_model_category_change(mode: &str, category: &Value) {
    let server = server(mode);
    let models = Http::open(server.port, "GET", "/v1/models", None, None);
    assert_eq!(models.status, 200);
    let list = models.json();
    let mut ids = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, vec![MODEL_A, MODEL_B]);

    let first = Plan::new("text");
    let mut request = body(&first, false);
    request["model"] = json!(MODEL_B);
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&request),
        None,
    );
    // 修复前会在 SetConfig 回包后因 category 缺失/未知而拒绝，尚未发送 prompt。
    assert_eq!(
        response.status, 200,
        "已协商模型设置响应被可选 category 拒绝：{mode}"
    );
    let session = response.headers["x-jchtools-session-id"].clone();
    let first_value = response.json();
    assert_eq!(first_value["model"], MODEL_B);
    assert_eq!(completion(&first_value), first.text());
    let first_start = wait_event(&server.root, "started", &first.tag);
    assert_eq!(first_start["model"], MODEL_B);

    // 回包完整配置替换本地状态后，下一轮仍须保留此前协商的 selector 身份。
    let second = Plan::new("text");
    let continuation = json!({"model":MODEL_A,"messages":[
        {"role":"user","content":first.prompt()},
        {"role":"assistant","content":completion(&first_value)},
        {"role":"user","content":second.prompt()}
    ]});
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&continuation),
        Some(&session),
    );
    assert_eq!(
        response.status, 200,
        "后续轮次丢失协商模型 selector 身份：{mode}"
    );
    assert_eq!(response.headers["x-jchtools-session-id"], session);
    let second_value = response.json();
    assert_eq!(second_value["model"], MODEL_A);
    assert_eq!(completion(&second_value), second.text());
    let second_start = wait_event(&server.root, "started", &second.tag);
    assert_eq!(second_start["model"], MODEL_A);
    assert_eq!(second_start["session"], first_start["session"]);

    let events = audit(&server.root);
    let selections = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["event"] == "model")
        .collect::<Vec<_>>();
    assert_eq!(selections.len(), 2);
    let starts = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event["event"] == "started")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    for (round, expected) in [MODEL_B, MODEL_A].into_iter().enumerate() {
        let (selection_index, selection) = selections[round];
        assert_eq!(selection["config_id"], "chosen-model");
        assert_eq!(selection["session"], first_start["session"]);
        assert_eq!(selection["model"], expected);
        let options = selection["config_options"].as_array().unwrap();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0]["id"], "chosen-model");
        assert_eq!(options[0]["type"], "select");
        assert_eq!(options[0]["currentValue"], expected);
        assert_eq!(options[0].get("category").unwrap_or(&Value::Null), category);
        let values = options[0]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["value"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, vec![MODEL_A, MODEL_B]);
        let (_, negotiated) = events
            .iter()
            .enumerate()
            .find(|(_, event)| {
                event["event"] == "session" && event["session"] == selection["session"]
            })
            .unwrap();
        assert_eq!(negotiated["config_id"], selection["config_id"]);
        assert!(
            selection_index < starts[round].0,
            "须先确认模型设置再发送 prompt"
        );
    }
    server.shutdown();
}

/// 覆盖 AH-04/AH-A03：缺协商模型能力明确503，不能编造服务别名。
#[test]
fn missing_model_capability_is_unavailable() {
    let server = server("no-models");
    let response = Http::open(server.port, "GET", "/v1/models", None, None);
    assert_eq!(response.status, 503);
    let value = response.json();
    assert!(value.get("error").is_some(), "{value}");
    assert!(value.get("data").is_none());
}

/// 覆盖 AH-04/AH-05：四角色与纯文本块保真；拒绝图片、工具、权限等输入而非忽略。
#[test]
fn text_roles_are_preserved_and_unsupported_inputs_are_rejected() {
    let server = server("normal");
    let plan = Plan::new("text");
    let roles = [
        ("system", "SYS-α"),
        ("developer", "DEV-β"),
        ("user", "USER-γ"),
        ("assistant", "ASSIST-δ"),
    ];
    let mut messages = roles
        .iter()
        .map(|(role, text)| json!({"role":role,"content":[{"type":"text","text":text}]}))
        .collect::<Vec<_>>();
    messages.push(json!({"role":"user","content":plan.prompt()}));
    let request = json!({"model":MODEL_A,"messages":messages});
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&request),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), plan.text());
    let event = wait_event(&server.root, "started", &plan.tag);
    let prompt = event["prompt"].as_str().unwrap();
    let mut offset = 0;
    for (role, text) in roles {
        let pos = prompt[offset..].find(text).unwrap() + offset;
        assert!(
            prompt[offset..pos].contains(role),
            "丢失角色 {role}: {prompt}"
        );
        offset = pos + text.len();
    }
    let before = audit(&server.root)
        .iter()
        .filter(|event| event["event"] == "started")
        .count();
    for invalid in [
        json!({"model":MODEL_A,"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}]}),
        json!({"model":MODEL_A,"messages":[{"role":"tool","content":"tool-result"}]}),
        json!({"model":MODEL_A,"messages":[],"tools":[]}),
        json!({"model":MODEL_A,"messages":[],"tool_choice":"none"}),
        json!({"model":MODEL_A,"messages":[],"permission":"allow"}),
    ] {
        let response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&invalid),
            None,
        );
        assert_eq!(response.status, 400, "{invalid}");
        assert!(response.json().get("error").is_some());
    }
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "started")
            .count(),
        before
    );
}

/// AH-04：音频块（独立或混合文本）和非空音频输出选项不能被静默忽略。
#[test]
fn audio_inputs_and_output_options_are_rejected_before_agent_start() {
    let server = server("normal");
    let first = Plan::new("text");
    let response = server.post(&first);
    assert_eq!(response.status, 200);
    let session = response.headers["x-jchtools-session-id"].clone();
    assert_eq!(completion(&response.json()), first.text());
    let first_start = wait_event(&server.root, "started", &first.tag);
    let before = audit(&server.root)
        .iter()
        .filter(|event| event["event"] == "started")
        .count();
    for case in ["input_audio", "mixed_input_audio", "audio", "modalities"] {
        let rejected = Plan::new("text");
        // 已成功的完整历史加新增 user；拒绝不能被空 messages 或坏历史伪装覆盖。
        let mut request = continuation(&first, &rejected, MODEL_A);
        let audio = json!({"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}});
        let param = match case {
            "input_audio" => {
                request["messages"][2]["content"] = json!([audio]);
                "messages[2].content[0].type"
            }
            "mixed_input_audio" => {
                request["messages"][2]["content"] =
                    json!([{"type":"text","text":rejected.prompt()}, audio]);
                "messages[2].content[1].type"
            }
            "audio" => {
                request["audio"] = json!({"voice":"alloy","format":"wav"});
                "audio"
            }
            "modalities" => {
                request["modalities"] = json!(["text", "audio"]);
                "modalities"
            }
            _ => unreachable!(),
        };
        let response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&request),
            Some(&session),
        );
        assert_eq!(response.status, 400, "{case}: {request}");
        let error = response.json();
        assert_eq!(
            error["error"]["type"], "invalid_request_error",
            "{case}: {error}"
        );
        assert_eq!(error["error"]["code"], "invalid_request", "{case}: {error}");
        assert_eq!(error["error"]["param"], param, "{case}: {error}");
        assert!(!audit(&server.root)
            .iter()
            .any(|event| event["event"] == "started" && event["tag"] == rejected.tag));
    }
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "started")
            .count(),
        before
    );
    let next = Plan::new("text");
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&continuation(&first, &next, MODEL_A)),
        Some(&session),
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    let next_start = wait_event(&server.root, "started", &next.tag);
    assert_eq!(next_start["session"], first_start["session"]);
    assert_eq!(next_start["pid"], first_start["pid"]);
}

// 先于 Server drop 释放挂起的模型设置；显式释放与 panic 收尾均只写一次 marker。
struct ModelSetRelease(Option<std::path::PathBuf>);
impl ModelSetRelease {
    fn release(&mut self) {
        std::fs::write(self.0.as_ref().unwrap(), b"release").unwrap();
        self.0 = None;
    }
}
impl Drop for ModelSetRelease {
    fn drop(&mut self) {
        if let Some(marker) = self.0.take() {
            let _released = std::fs::write(marker, b"release");
        }
    }
}

/// AH-04/AH-12/AH-13：prompt 尚未发出时取消，只结束该请求并保留原历史与排队轮次。
#[test]
fn pre_prompt_cancel_preserves_session_and_waiter() {
    use jchtools::acp_api::{ChatMessage, MessageRole, PromptInput, RequestEvent, SessionKey};
    let server = server("model-set-held");
    let mut cleanup = ModelSetRelease(Some(server.root.join("release-model-set")));
    let first = Plan::new("text");
    let response = server.post(&first);
    assert_eq!(response.status, 200);
    let session = response.headers["x-jchtools-session-id"].clone();
    let first_output = completion(&response.json()).to_owned();
    assert_eq!(first_output, first.text());
    let first_start = wait_event(&server.root, "started", &first.tag);
    std::fs::write(server.root.join("hold-model-set"), b"hold").unwrap();
    let cancelled = Plan::new("text");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut request = runtime
        .block_on(server.backend.submit(PromptInput {
            model: MODEL_B.into(),
            messages: vec![
                ChatMessage {
                    role: MessageRole::User,
                    text: first.prompt(),
                },
                ChatMessage {
                    role: MessageRole::Assistant,
                    text: first_output.clone(),
                },
                ChatMessage {
                    role: MessageRole::User,
                    text: cancelled.prompt(),
                },
            ],
            session: Some(SessionKey(session.clone())),
        }))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let held = loop {
        let events = audit(&server.root);
        if let Some(event) = events.into_iter().find(|event| {
            event["event"] == "model-set-held" && event["session"] == first_start["session"]
        }) {
            break event;
        }
        assert!(Instant::now() < deadline, "模型设置未到达 barrier");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(held["pid"], first_start["pid"]);
    // 同步设置 token，第三轮的提交一定在取消之后、设置响应放行之前。
    request.cancellation.cancel();
    let third = Plan::new("text");
    let third_body = json!({"model":MODEL_A,"messages":[
        {"role":"user","content":first.prompt()},
        {"role":"assistant","content":first_output},
        {"role":"user","content":third.prompt()}
    ]});
    let port = server.port;
    let third_session = session.clone();
    let waiter = thread::spawn(move || {
        let response = Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&third_body),
            Some(&third_session),
        );
        (response.status, response.headers.clone(), response.json())
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = server.backend.status();
        if status.executing == 1 && status.waiting == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "第三轮未排队：{status:?}");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!audit(&server.root).iter().any(|event| {
        event["event"] == "started" && (event["tag"] == cancelled.tag || event["tag"] == third.tag)
    }));
    cleanup.release();
    let terminal = runtime
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(10), request.events.recv()).await
        })
        .unwrap();
    request.cancellation.disarm();
    let (status, headers, value) = waiter.join().unwrap();
    assert_eq!(terminal, Some(RequestEvent::Cancelled));
    assert!(!audit(&server.root)
        .iter()
        .any(|event| event["event"] == "started" && event["tag"] == cancelled.tag));
    assert_eq!(
        status, 200,
        "早取消不得使原会话或已排队的第三轮失效：{value}"
    );
    assert_eq!(headers["x-jchtools-session-id"], session);
    assert_eq!(completion(&value), third.text());
    let third_start = wait_event(&server.root, "started", &third.tag);
    assert_eq!(third_start["session"], first_start["session"]);
    assert_eq!(third_start["pid"], first_start["pid"]);
    let unrelated = Plan::new("text");
    let response = server.post(&unrelated);
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), unrelated.text());
    let unrelated_start = wait_event(&server.root, "started", &unrelated.tag);
    assert_ne!(unrelated_start["session"], first_start["session"]);
    assert_eq!(unrelated_start["pid"], first_start["pid"]);
}

/// 覆盖 AH-04：显式续会话完整前缀包含已生成assistant；只向ACP提交新增文本。
#[test]
fn explicit_history_reuses_session_and_rejects_changed_prefix() {
    let server = server("normal");
    let first = Plan::new("text");
    let response = server.post(&first);
    assert_eq!(response.status, 200);
    let session = response.headers["x-jchtools-session-id"].clone();
    let output = completion(&response.json()).to_owned();
    let second = Plan::new("text");
    let request = json!({"model":MODEL_A,"messages":[{"role":"user","content":first.prompt()},{"role":"assistant","content":output},{"role":"user","content":second.prompt()}]});
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&request),
        Some(&session),
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), second.text());
    let event = wait_event(&server.root, "started", &second.tag);
    assert_eq!(
        event["session"],
        wait_event(&server.root, "started", &first.tag)["session"]
    );
    assert!(
        !event["prompt"].as_str().unwrap().contains(&first.tag),
        "不得重发既有历史"
    );
    assert!(!event["prompt"].as_str().unwrap().contains(&output));
    let mut changed = request;
    changed["messages"][0]["content"] = json!("changed-prefix");
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&changed),
        Some(&session),
    );
    assert_eq!(response.status, 400);
    assert!(response.json().get("error").is_some());
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "started")
            .count(),
        2
    );
}

/// 覆盖 AH-04/AH-12/AH-13/AH-A04/AH-A08/AH-A09：真实首增量在barrier完成前可见；断连隔离。
#[test]
fn two_sessions_overlap_stream_incrementally_and_disconnect_only_cancels_one() {
    let server = server("normal");
    let mut first = Plan::new("text");
    first.barrier = true;
    let mut second = Plan::new("text");
    second.barrier = true;
    second.chunks = vec!["另一会话首段".into(), "独立尾段".into()];
    let mut a = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&first, true)),
        None,
    );
    assert_eq!(a.status, 200);
    assert!(a.headers["content-type"].starts_with("text/event-stream"));
    assert_eq!(a.delta(), first.chunks[0]);
    let mut b = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&second, true)),
        None,
    );
    assert_eq!(b.status, 200);
    assert_eq!(b.delta(), second.chunks[0]);
    let ea = wait_event(&server.root, "started", &first.tag);
    let eb = wait_event(&server.root, "started", &second.tag);
    assert_eq!(ea["pid"], eb["pid"]);
    assert_ne!(ea["session"], eb["session"]);
    assert!(!audit(&server.root)
        .iter()
        .any(|event| event["event"] == "completed"));
    drop(a);
    assert_eq!(
        wait_event(&server.root, "cancelled", &first.tag)["pid"],
        ea["pid"]
    );
    release(&server.root, &second);
    assert_eq!(b.finish_stream(), second.chunks[1]);
    let third = Plan::new("text");
    assert_eq!(completion(&server.post(&third).json()), third.text());
    assert_eq!(
        wait_event(&server.root, "started", &third.tag)["pid"],
        ea["pid"]
    );
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
}

/// 覆盖 AH-04/AH-12：同会话后续提示不取消上一轮，按接收顺序FIFO执行。
#[test]
fn same_session_turns_wait_in_fifo_order_without_implicit_cancel() {
    let server = server("normal");
    let mut first = Plan::new("text");
    first.barrier = true;
    let mut stream = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&first, true)),
        Some("fifo-session"),
    );
    assert_eq!(stream.delta(), first.chunks[0]);
    let second = Plan::new("text");
    let mut third = Plan::new("text");
    third.chunks = vec!["third-only".into()];
    let history = vec![
        json!({"role":"user","content":first.prompt()}),
        json!({"role":"assistant","content":first.text()}),
    ];
    let mut second_history = history.clone();
    second_history.push(json!({"role":"user","content":second.prompt()}));
    let port = server.port;
    let second_body = json!({"model":MODEL_A,"messages":second_history});
    let second_thread = thread::spawn(move || {
        let response = Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&second_body),
            Some("fifo-session"),
        );
        assert_eq!(response.status, 200);
        response.json()
    });
    wait_counts(&server, 1, 1);
    let mut third_history = history;
    third_history.push(json!({"role":"user","content":second.prompt()}));
    third_history.push(json!({"role":"assistant","content":second.text()}));
    third_history.push(json!({"role":"user","content":third.prompt()}));
    let third_body = json!({"model":MODEL_A,"messages":third_history});
    let third_thread = thread::spawn(move || {
        let response = Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&third_body),
            Some("fifo-session"),
        );
        assert_eq!(response.status, 200);
        response.json()
    });
    wait_counts(&server, 1, 2);
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "started")
            .count(),
        1
    );
    assert!(!audit(&server.root)
        .iter()
        .any(|event| event["event"] == "cancelled"));
    release(&server.root, &first);
    assert_eq!(stream.finish_stream(), first.chunks[1]);
    assert_eq!(completion(&second_thread.join().unwrap()), second.text());
    assert_eq!(completion(&third_thread.join().unwrap()), third.text());
    let starts = audit(&server.root)
        .into_iter()
        .filter(|event| event["event"] == "started")
        .collect::<Vec<_>>();
    assert_eq!(
        starts
            .iter()
            .map(|event| event["tag"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![first.tag.as_str(), second.tag.as_str(), third.tag.as_str()]
    );
    assert!(starts
        .iter()
        .all(|event| event["session"] == starts[0]["session"]));
}

/// 覆盖 AH-12：四个实际执行barrier、十六个实际等待，第21个真实HTTP返回429。
#[test]
fn four_active_plus_sixteen_waiting_rejects_twenty_first_request() {
    let server = server("normal");
    let mut active = Vec::new();
    for _ in 0..4 {
        let mut plan = Plan::new("text");
        plan.barrier = true;
        let mut response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&body(&plan, true)),
            None,
        );
        assert_eq!(response.delta(), plan.chunks[0]);
        active.push((plan, response));
    }
    wait_counts(&server, 4, 0);
    let mut queued = Vec::new();
    for index in 0..16 {
        let plan = Plan::new("text");
        let request = body(&plan, false);
        let port = server.port;
        queued.push((
            plan,
            thread::spawn(move || {
                let response =
                    Http::open(port, "POST", "/v1/chat/completions", Some(&request), None);
                assert_eq!(response.status, 200);
                response.json()
            }),
        ));
        wait_counts(&server, 4, index + 1);
    }
    let response = server.post(&Plan::new("text"));
    assert_eq!(response.status, 429);
    assert!(response.json().get("error").is_some());
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "started")
            .count(),
        4
    );
    for (plan, mut response) in active {
        release(&server.root, &plan);
        assert_eq!(response.finish_stream(), plan.chunks[1]);
    }
    for (plan, job) in queued {
        assert_eq!(completion(&job.join().unwrap()), plan.text());
    }
    wait_counts(&server, 0, 0);
}

/// 覆盖 AH-13/AH-A09：Agent崩溃让每个受影响请求失败；新请求单实例重建且旧任务不重放。
#[test]
fn agent_failure_is_not_success_and_next_request_rebuilds_without_replay() {
    let server = server("normal");
    let mut victim = Plan::new("text");
    victim.barrier = true;
    let mut stream = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&victim, true)),
        None,
    );
    assert_eq!(stream.delta(), victim.chunks[0]);
    let old = wait_event(&server.root, "started", &victim.tag)["pid"].clone();
    let crash = Plan::new("crash");
    let response = server.post(&crash);
    assert_eq!(response.status, 502);
    assert!(response.json().get("error").is_some());
    let event = stream.sse();
    assert!(event.contains("error"), "{event}");
    assert!(!event.contains("\"finish_reason\":\"stop\""));
    let next = Plan::new("text");
    let response = server.post(&next);
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    assert_ne!(wait_event(&server.root, "started", &next.tag)["pid"], old);
    let events = audit(&server.root);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2
    );
    for plan in [&victim, &crash] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "started" && event["tag"] == plan.tag)
                .count(),
            1
        );
    }
}

/// 覆盖 AH-04/AH-13：ACP提示错误映射为去敏server_error，不得返回成功响应或Agent错误正文。
#[test]
fn acp_prompt_error_returns_failure_json() {
    let server = server("normal");
    let plan = Plan::new("error");
    let response = server.post(&plan);
    assert_eq!(response.status, 502);
    let error = response.json();
    assert_eq!(error["error"]["type"], "server_error");
    assert!(!error.to_string().contains(&plan.tag));
    assert!(error.get("choices").is_none());
}

fn send_without_reading(port: u16, request: &Value) -> std::net::TcpStream {
    use std::io::Write;
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let payload = request.to_string();
    write!(socket,"POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",payload.len()).unwrap();
    socket.flush().unwrap();
    socket
}

/// 覆盖 AH-13：首event前SSE和非stream静默等待时，HTTP断连都必须取消ACP而无需后继增量。
#[test]
fn disconnect_before_first_event_and_during_json_wait_cancels_prompt() {
    let server = server("normal");
    let mut pid = None;
    for stream in [true, false] {
        let mut plan = Plan::new("silent");
        plan.barrier = true;
        let socket = send_without_reading(server.port, &body(&plan, stream));
        let started = wait_event(&server.root, "started", &plan.tag);
        if let Some(pid) = &pid {
            assert_eq!(&started["pid"], pid);
        } else {
            pid = Some(started["pid"].clone());
        }
        drop(socket);
        wait_event(&server.root, "cancelled", &plan.tag);
        wait_counts(&server, 0, 0);
        assert!(!audit(&server.root)
            .iter()
            .any(|event| event["event"] == "completed" && event["tag"] == plan.tag));
    }
    let next = Plan::new("text");
    assert_eq!(completion(&server.post(&next).json()), next.text());
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
}

/// 覆盖 AH-12/AH-13：容量等待阶段断连释放一个waiting槽，绝不创建/重放被取消的ACP轮次。
#[test]
fn queued_http_disconnect_releases_capacity_without_starting_agent_turn() {
    let server = server("normal");
    let mut active = Vec::new();
    for _ in 0..4 {
        let mut plan = Plan::new("text");
        plan.barrier = true;
        let mut response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&body(&plan, true)),
            None,
        );
        assert_eq!(response.delta(), plan.chunks[0]);
        active.push((plan, response));
    }
    let queued = Plan::new("silent");
    let socket = send_without_reading(server.port, &body(&queued, true));
    wait_counts(&server, 4, 1);
    drop(socket);
    wait_counts(&server, 4, 0);
    assert!(
        !audit(&server.root)
            .iter()
            .any(|event| event["tag"] == queued.tag),
        "队列取消不得提交ACP"
    );
    for (plan, mut response) in active {
        release(&server.root, &plan);
        assert_eq!(response.finish_stream(), plan.chunks[1]);
    }
    let next = Plan::new("text");
    assert_eq!(completion(&server.post(&next).json()), next.text());
    assert!(!audit(&server.root)
        .iter()
        .any(|event| event["tag"] == queued.tag));
}

/// AH-02/AH-04：探测为 Model，后续默认新会话仍以已协商 selector ID 识别。
#[test]
fn second_default_session_accepts_missing_model_category() {
    assert_second_default_session("new-category-missing");
}

/// AH-02/AH-04：未知 UX category 不能破坏后续默认新会话的模型选择。
#[test]
fn second_default_session_accepts_unknown_model_category() {
    assert_second_default_session("new-category-unknown");
}

fn assert_second_default_session(mode: &str) {
    let server = server(mode);
    let first = Plan::new("text");
    assert_eq!(completion(&server.post(&first).json()), first.text());
    let second = Plan::new("text");
    let response = server.post(&second);
    assert_eq!(
        response.status, 200,
        "已协商 selector 在第二默认会话失效：{mode}"
    );
    assert_eq!(completion(&response.json()), second.text());
    let first_start = wait_event(&server.root, "started", &first.tag);
    let second_start = wait_event(&server.root, "started", &second.tag);
    assert_ne!(first_start["session"], second_start["session"]);
    assert_eq!(first_start["pid"], second_start["pid"]);
}

fn continuation(first: &Plan, second: &Plan, model: &str) -> Value {
    json!({"model":model,"messages":[
        {"role":"user","content":first.prompt()},
        {"role":"assistant","content":first.text()},
        {"role":"user","content":second.prompt()}
    ]})
}

fn assert_new_response_config_burst(server: &Server, session: &str) {
    let marker = server.root.join(format!("new-response-burst-{session}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    // 只等待可观察证据，不用时间延迟猜发送/读取顺序；write 后落 marker 与 audit 可并发。
    let events = loop {
        let bytes = match std::fs::read(&marker) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("无法读取 burst marker: {error}"),
        };
        let events = audit(&server.root);
        if bytes == b"response-and-config-update-in-one-pipe-write"
            && events
                .iter()
                .any(|event| event["event"] == "new-config-update" && event["session"] == session)
        {
            break events;
        }
        assert!(
            Instant::now() < deadline,
            "未观察到原样单次 pipe write 和 A/C audit: marker={bytes:?}, audit={events:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let response_index = events
        .iter()
        .position(|event| event["event"] == "session-options" && event["session"] == session)
        .unwrap();
    let update_index = events
        .iter()
        .position(|event| event["event"] == "new-config-update" && event["session"] == session)
        .unwrap();
    assert!(response_index < update_index);
    let first_response_index = events
        .iter()
        .position(|event| event["event"] == "session-options")
        .unwrap();
    let response_pool = if response_index == first_response_index {
        vec![MODEL_A, MODEL_B]
    } else {
        vec![MODEL_A, MODEL_C]
    };
    for (index, expected) in [
        (response_index, response_pool),
        (update_index, vec![MODEL_A, MODEL_C]),
    ] {
        assert_eq!(
            events[index]["config_options"][0]["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| option["value"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected,
        );
    }
}

fn wait_updated_catalog(server: &Server) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = Http::open(server.port, "GET", "/v1/models", None, None);
        assert_eq!(response.status, 200);
        let catalog = response.json();
        assert_eq!(catalog["object"], "list");
        let ids = catalog["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        if ids == [MODEL_A, MODEL_C] {
            return;
        }
        // Ready 不是两条 RPC 的原子边界；以公开结果确认通知已实际处理。
        assert!(
            Instant::now() < deadline,
            "目录必须反映 A/C 更新，旧 A/B 不得永久残留：{catalog}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// AH-02/AH-04：未消费探测会话的 A/C 更新必须替换公开目录，不能永久保留 A/B 快照。
/// 只按观测谓词通过，不靠睡够时长：Agent 原样一次 pipe write 两条 SDK 帧。
#[test]
fn new_session_immediate_config_update_replaces_public_model_catalog() {
    let server = server("new-response-config-update");
    let sessions = audit(&server.root);
    let session = sessions
        .iter()
        .find(|event| event["event"] == "session-options")
        .unwrap()["session"]
        .as_str()
        .unwrap();
    assert_new_response_config_burst(&server, session);
    wait_updated_catalog(&server);
    let rejected = Plan::new("text");
    let mut request = body(&rejected, false);
    request["model"] = json!(MODEL_B);
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&request),
        None,
    );
    assert_eq!(
        response.status, 400,
        "B 必须在请求准入时拒绝，不能交给 Agent 再失败"
    );
    assert!(response.json().get("error").is_some());
    assert!(!audit(&server.root).iter().any(|event| {
        event["event"] == "model" || (event["event"] == "started" && event["tag"] == rejected.tag)
    }));
    server.shutdown();
}

/// AH-02/AH-04：更新新增的 C 必须实际到达 Agent；探测 spare 和后续 session/new 都覆盖。
#[test]
fn new_session_immediate_config_update_admits_and_selects_latest_model() {
    let server = server("new-response-config-update");
    // 先从公开目录观察到 C，再选择它；不把 Ready 当成后继通知已处理的承诺。
    wait_updated_catalog(&server);
    let mut completed = Vec::new();
    for _ in 0..2 {
        let plan = Plan::new("text");
        let mut request = body(&plan, false);
        request["model"] = json!(MODEL_C);
        let response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&request),
            None,
        );
        if response.status != 200 {
            let status = response.status;
            let headers = response.headers.clone();
            let failure = response.json();
            panic!(
                "已公开的 C 必须实际可选：completed={}, status={status}, \
                 headers={headers:?}, failure={failure}, audit={:?}",
                completed.len(),
                audit(&server.root)
            );
        }
        let session = response.headers["x-jchtools-session-id"].clone();
        let value = response.json();
        assert_eq!(value["model"], MODEL_C);
        assert_eq!(completion(&value), plan.text());
        let started = wait_event(&server.root, "started", &plan.tag);
        assert_eq!(started["model"], MODEL_C);
        assert_new_response_config_burst(&server, started["session"].as_str().unwrap());
        completed.push((plan, session, started));
    }
    assert_ne!(completed[0].2["session"], completed[1].2["session"]);
    assert_eq!(completed[0].2["pid"], completed[1].2["pid"]);

    let rejected = Plan::new("text");
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&continuation(&completed[1].0, &rejected, MODEL_B)),
        Some(&completed[1].1),
    );
    assert_eq!(response.status, 400, "最新会话配置不得保留已经移除的 B");
    assert!(response.json().get("error").is_some());
    let events = audit(&server.root);
    assert!(!events.iter().any(|event| event["tag"] == rejected.tag));
    let selections = events
        .iter()
        .filter(|event| event["event"] == "model")
        .collect::<Vec<_>>();
    assert_eq!(selections.len(), 2);
    assert!(selections.iter().all(|event| event["model"] == MODEL_C));
    server.shutdown();
}

/// AH-04：set 完整回包的 A/C 替换探测 A/B，原会话 C 必须实际可选择。
#[test]
fn set_response_new_model_is_usable_in_existing_session() {
    let server = server("model-options-expand");
    let first = Plan::new("text");
    let response = server.post(&first);
    assert_eq!(response.status, 200);
    let session = response.headers["x-jchtools-session-id"].clone();
    assert_eq!(completion(&response.json()), first.text());
    // 此会话的扩展不能污染启动 catalog，也不能冒充其他新会话的可选模型。
    let catalog = Http::open(server.port, "GET", "/v1/models", None, None).json();
    assert_eq!(
        catalog["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![MODEL_A, MODEL_B],
    );
    let mut unrelated = body(&Plan::new("text"), false);
    unrelated["model"] = json!(MODEL_C);
    let unavailable = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&unrelated),
        None,
    );
    assert_eq!(unavailable.status, 400);
    assert!(unavailable.json().get("error").is_some());
    let second = Plan::new("text");
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&continuation(&first, &second, MODEL_C)),
        Some(&session),
    );
    assert_eq!(
        response.status, 200,
        "set 回包新增 C 不能被启动时 A/B 快照拒绝"
    );
    let value = response.json();
    assert_eq!(value["model"], MODEL_C);
    assert_eq!(completion(&value), second.text());
    assert_eq!(
        wait_event(&server.root, "started", &second.tag)["model"],
        MODEL_C
    );
    assert_eq!(
        wait_event(&server.root, "started", &first.tag)["session"],
        wait_event(&server.root, "started", &second.tag)["session"],
    );
}

/// AH-02/AH-04：ConfigOptionUpdate 的完整 A/B 替换 set 回包 B，下一轮可选 A。
#[test]
fn config_option_update_restores_model_for_next_turn() {
    let server = server("model-options-update");
    let first = Plan::new("text");
    let mut request = body(&first, false);
    request["model"] = json!(MODEL_B);
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&request),
        None,
    );
    assert_eq!(response.status, 200);
    let session = response.headers["x-jchtools-session-id"].clone();
    assert_eq!(completion(&response.json()), first.text());
    wait_event(&server.root, "config-update", &first.tag);
    let second = Plan::new("text");
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&continuation(&first, &second, MODEL_A)),
        Some(&session),
    );
    assert_eq!(
        response.status, 200,
        "忽略 ConfigOptionUpdate 导致 A 无法续轮"
    );
    assert_eq!(completion(&response.json()), second.text());
    assert_eq!(
        wait_event(&server.root, "started", &second.tag)["model"],
        MODEL_A
    );
    assert_eq!(
        wait_event(&server.root, "started", &first.tag)["session"],
        wait_event(&server.root, "started", &second.tag)["session"],
    );
}

/// AH-04/AH-12：active 时排队的坏历史只拒绝本请求，不能销毁已完成的会话。
#[test]
fn queued_history_mismatch_does_not_invalidate_existing_session() {
    let server = server("normal");
    let mut first = Plan::new("text");
    first.barrier = true;
    let mut stream = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&first, true)),
        Some("history-recovery"),
    );
    assert_eq!(stream.delta(), first.chunks[0]);
    let bad = Plan::new("text");
    let bad_request = body(&bad, false);
    let port = server.port;
    let queued = thread::spawn(move || {
        let response = Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&bad_request),
            Some("history-recovery"),
        );
        (response.status, response.json())
    });
    wait_counts(&server, 1, 1);
    release(&server.root, &first);
    assert_eq!(stream.finish_stream(), first.chunks[1]);
    let (status, error) = queued.join().unwrap();
    assert_eq!(status, 400);
    assert!(error.get("error").is_some());
    let second = Plan::new("text");
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&continuation(&first, &second, MODEL_A)),
        Some("history-recovery"),
    );
    assert_eq!(response.status, 200, "坏历史输入不能使原会话失效");
    assert_eq!(completion(&response.json()), second.text());
    let events = audit(&server.root);
    assert!(!events
        .iter()
        .any(|event| event["event"] == "started" && event["tag"] == bad.tag));
    assert_eq!(
        wait_event(&server.root, "started", &first.tag)["session"],
        wait_event(&server.root, "started", &second.tag)["session"],
    );
}

/// AH-06/AH-08/AH-13：owner 已确认即时退出并清除 PID，后端不得在 handoff 后复活它。
#[test]
fn immediately_exited_agent_never_republishes_dead_pid() {
    let server = server("exit-immediately");
    let exited = std::fs::read_to_string(server.root.join("owner-exited")).unwrap();
    assert!(exited.parse::<u32>().unwrap() > 0);
    let status = server.backend.status();
    assert_eq!(status.phase, jchtools::acp_api::ServicePhase::Error);
    assert_eq!(status.agent_pid, None, "已退出 PID {exited} 被后端重复发布");
}
