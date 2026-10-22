use super::*;

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
        let mut request = body(&plan, false, &server.workspace);
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
    let mut request = body(&Plan::new("text"), false, &server.workspace);
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
    let mut request = body(&first, false, &server.workspace);
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
    let continuation = json!({"model":MODEL_A,"cwd":server.workspace.clone(),"messages":[
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
    let first_response = server.post(&first);
    assert_eq!(first_response.status, 200);
    let first_session = first_response.headers["x-jchtools-session-id"].clone();
    assert_eq!(completion(&first_response.json()), first.text());
    let second = Plan::new("text");
    let mut request = body(&second, false, &server.workspace);
    request["model"] = json!(MODEL_B);
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&request),
        None,
    );
    assert_eq!(
        response.status, 200,
        "已协商 selector 在第二默认会话失效：{mode}"
    );
    assert_ne!(response.headers["x-jchtools-session-id"], first_session);
    let value = response.json();
    assert_eq!(value["model"], MODEL_B);
    assert_eq!(completion(&value), second.text());
    let first_start = wait_event(&server.root, "started", &first.tag);
    let second_start = wait_event(&server.root, "started", &second.tag);
    assert_eq!(first_start["model"], MODEL_A);
    assert_eq!(second_start["model"], MODEL_B);
    assert_ne!(first_start["session"], second_start["session"]);
    assert_eq!(first_start["pid"], second_start["pid"]);
    // 两个新会话都必须先以该会话协商的 ID 确认所请求模型，再提交 prompt。
    let events = audit(&server.root);
    for (start, requested) in [(&first_start, MODEL_A), (&second_start, MODEL_B)] {
        let selection_index = events
            .iter()
            .position(|event| {
                event["event"] == "model"
                    && event["session"] == start["session"]
                    && event["model"] == requested
            })
            .unwrap();
        let negotiated = events
            .iter()
            .find(|event| event["event"] == "session" && event["session"] == start["session"])
            .unwrap();
        assert_eq!(
            events[selection_index]["config_id"],
            negotiated["config_id"]
        );
        let start_index = events
            .iter()
            .position(|event| event["event"] == "started" && event["tag"] == start["tag"])
            .unwrap();
        assert!(selection_index < start_index);
    }
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
    let mut request = body(&rejected, false, &server.workspace);
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
        let mut request = body(&plan, false, &server.workspace);
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
        Some(&continuation(
            &completed[1].0,
            &rejected,
            MODEL_B,
            &server.workspace,
        )),
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
    let mut unrelated = body(&Plan::new("text"), false, &server.workspace);
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
        Some(&continuation(&first, &second, MODEL_C, &server.workspace)),
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
    let mut request = body(&first, false, &server.workspace);
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
        Some(&continuation(&first, &second, MODEL_A, &server.workspace)),
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

/// AH-06/AH-08/AH-13：owner 已确认即时退出并清除 PID，后端不得在 handoff 后复活它。
#[test]
fn immediately_exited_agent_never_republishes_dead_pid() {
    let server = server("exit-immediately");
    let exited = std::fs::read_to_string(server.root.join("owner-exited")).unwrap();
    assert!(exited.parse::<u32>().unwrap() > 0);
    let status = server.backend.status();
    assert_eq!(status.phase, jchtools_acp_api::acp_api::ServicePhase::Error);
    assert_eq!(status.agent_pid, None, "已退出 PID {exited} 被后端重复发布");
}
