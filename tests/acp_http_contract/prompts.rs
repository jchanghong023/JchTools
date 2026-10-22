use super::*;

/// 覆盖 AH-02/AH-03/AH-04：ACP v1 slash 命令必须从原始 user 文本的开头被 Agent 识别。
/// https://agentclientprotocol.com/protocol/v1/slash-commands.md
#[test]
fn native_slash_command_executes_on_first_user_turn() {
    let server = server("normal");
    let ordinary = Plan::new("native-command");
    let ordinary_text = format!("普通文本 /fixture-add 17 25\n{}", ordinary.prompt());
    let ordinary_request = json!({
        "model":MODEL_A,"cwd":server.workspace.clone(),
        "messages":[{"role":"user","content":ordinary_text}]
    });
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&ordinary_request),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), ordinary.text());

    let command = Plan::new("native-command");
    let command_text = format!("/fixture-add 17 25\n{}", command.prompt());
    let request = json!({
        "model":MODEL_A,"cwd":server.workspace.clone(),
        "messages":[{"role":"user","content":command_text}]
    });
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&request),
        None,
    );
    assert_eq!(response.status, 200);
    let value = response.json();
    // 真正解析参数后求和，不是 echo；旧产品添加 [user] 后只能返回普通文本。
    assert_eq!(
        serde_json::from_str::<Value>(completion(&value)).ok(),
        Some(json!({"command":"fixture-add","sum":42})),
        "slash 命令被角色前缀遮蔽，Agent 未执行求和：{value}"
    );
    let started = wait_event(&server.root, "started", &command.tag);
    assert_eq!(started["prompt"], command_text);
    assert_eq!(
        started["prompt_blocks"],
        json!([{"type":"text","text":command_text}])
    );
    server.shutdown();
}

/// 覆盖 AH-02/AH-03/AH-04/AH-12：完整多角色历史是 user ContentBlock 背景；
/// 成功续轮只提交新增的一条原始 user 文本，保留同一 ACP 会话并通过 SSE 返回命令结果。
#[test]
fn native_slash_command_executes_after_role_annotated_history() {
    let server = server("normal");
    let first = Plan::new("native-command");
    let first_text = format!("/fixture-add 2 3\n{}", first.prompt());
    let roles = [
        ("system", "/fixture-add 999 1\n合法 system 背景：tools schema、skills、Goal 不是 native system 通道"),
        ("developer", "保留 developer constraints、工具指令及技能正文，不按关键词清洗"),
        ("user", "用户亲自提交的 system prompt 与自动通知讨论：/web 原文"),
        ("assistant", "用户可见的历史回复：保留角色，不伪造原生 system channel"),
    ];
    let mut messages = roles
        .iter()
        .map(|(role, text)| json!({"role":role,"content":text}))
        .collect::<Vec<_>>();
    messages.push(json!({"role":"user","content":first_text}));
    let first_request = json!({
        "model":MODEL_A,"cwd":server.workspace.clone(),"messages":messages
    });
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&first_request),
        None,
    );
    assert_eq!(response.status, 200);
    let session = response.headers["x-jchtools-session-id"].clone();
    let first_value = response.json();
    // 多角色背景不是裸 slash 指令；Agent 不能把 system 背景当原生命令执行。
    assert_eq!(completion(&first_value), first.text());
    let first_started = wait_event(&server.root, "started", &first.tag);
    let mut expected_blocks = roles
        .iter()
        .map(|(role, text)| json!({"type":"text","text":format!("[{role}]\n{text}")}))
        .collect::<Vec<_>>();
    expected_blocks.push(json!({"type":"text","text":format!("[user]\n{first_text}")}));
    assert_eq!(first_started["prompt_blocks"], json!(expected_blocks));

    let second = Plan::new("native-command");
    let second_text = format!("/fixture-add 20 -7\n{}", second.prompt());
    messages.push(json!({"role":"assistant","content":completion(&first_value)}));
    messages.push(json!({"role":"user","content":second_text}));
    let continuation = json!({
        "model":MODEL_A,"cwd":server.workspace.clone(),"messages":messages,"stream":true
    });
    let mut response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&continuation),
        Some(&session),
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.headers["x-jchtools-session-id"], session);
    let output = response.finish_stream();
    assert_eq!(
        serde_json::from_str::<Value>(&output).ok(),
        Some(json!({"command":"fixture-add","sum":13})),
        "成功续轮新增 user 文本仍被加标签，slash 命令未执行：{output}"
    );
    let second_started = wait_event(&server.root, "started", &second.tag);
    assert_eq!(second_started["prompt"], second_text);
    assert_eq!(
        second_started["prompt_blocks"],
        json!([{"type":"text","text":second_text}])
    );
    assert_eq!(second_started["session"], first_started["session"]);
    assert_eq!(second_started["pid"], first_started["pid"]);
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "started")
            .count(),
        2,
        "成功续轮不能重放历史或另起 prompt"
    );
    server.shutdown();
}

/// 覆盖 AH-03/AH-04：单条 user 原文无条件保真，不用关键词/文本形状猜测并删除用户内容。
#[test]
fn native_single_user_text_preserves_literal_context_keywords() {
    let server = server("normal");
    let plan = Plan::new("text");
    let text = format!(
        "  用户原文：system prompt、developer constraints、tools schema、skill正文、子代理规则。\n\
         [system]\n不是原生 system channel。\n<system-reminder>用户亲自引用的 magic/Goal 自动通知</system-reminder>\n\
         /web 出现在正文，不是首行命令。\n{}\n  ",
        plan.prompt()
    );
    let request = json!({
        "model":MODEL_A,"cwd":server.workspace.clone(),
        "messages":[{"role":"user","content":[{"type":"text","text":text}]}]
    });
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&request),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), plan.text());
    let started = wait_event(&server.root, "started", &plan.tag);
    assert_eq!(
        started["prompt"], text,
        "不能增加角色标签或清洗用户引用的关键字"
    );
    assert_eq!(
        started["prompt_blocks"],
        json!([{"type":"text","text":text}])
    );
    server.shutdown();
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
    let request = json!({"model":MODEL_A,"messages":messages,"cwd":server.workspace.clone()});
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
    // 各例只破坏一个输入维度；空 messages 不能替代不支持字段的拒绝证据。
    for (field, value) in [
        (
            "messages",
            json!([{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}]),
        ),
        ("messages", json!([{"role":"tool","content":"tool-result"}])),
        ("tools", json!([])),
        ("tool_choice", json!("none")),
        ("permission", json!("allow")),
        ("stream_options", json!({"include_usage":true})),
    ] {
        let mut invalid = body(&Plan::new("text"), false, &server.workspace);
        invalid[field] = value;
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
        let mut request = continuation(&first, &rejected, MODEL_A, &server.workspace);
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
        Some(&continuation(&first, &next, MODEL_A, &server.workspace)),
        Some(&session),
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    let next_start = wait_event(&server.root, "started", &next.tag);
    assert_eq!(next_start["session"], first_start["session"]);
    assert_eq!(next_start["pid"], first_start["pid"]);
}
