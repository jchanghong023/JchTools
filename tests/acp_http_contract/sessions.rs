use super::*;

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

/// AH-04/AH-06/AH-12/AH-13：cwd 必须是明确、非空、绝对且已存在的目录，拒绝发生在业务 session/new 前。
#[test]
fn caller_cwd_is_required_and_invalid_paths_do_not_create_sessions() {
    let server = server("normal");
    let file = server.temp.path().join("not-a-directory");
    std::fs::write(&file, b"file").unwrap();
    let initial_sessions = audit(&server.root)
        .iter()
        .filter(|event| event["event"] == "session")
        .count();
    let initial_starts = audit(&server.root)
        .iter()
        .filter(|event| event["event"] == "started")
        .count();
    let invalid = [
        ("missing", None),
        ("null", Some(Value::Null)),
        ("empty", Some(json!(""))),
        ("relative", Some(json!("relative-cwd"))),
        (
            "nonexistent",
            Some(json!(server.temp.path().join("absent-cwd"))),
        ),
        ("file", Some(json!(file))),
    ];
    for (case, cwd) in invalid {
        let plan = Plan::new("text");
        let mut request = json!({
            "model":MODEL_A,
            "messages":[{"role":"user","content":plan.prompt()}]
        });
        if let Some(cwd) = cwd {
            request["cwd"] = cwd;
        }
        let response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&request),
            None,
        );
        let status = response.status;
        let has_session_id = response.headers.contains_key("x-jchtools-session-id");
        let value = response.json();
        assert_eq!(status, 400, "{case}: {value}");
        assert!(
            !has_session_id,
            "{case} must be rejected before allocating a business session"
        );
    }
    let events = audit(&server.root);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "session")
            .count(),
        initial_sessions,
        "invalid cwd requests must not create business sessions"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "started")
            .count(),
        initial_starts,
        "invalid cwd requests must not start a business turn"
    );
}

/// AH-04/AH-06/AH-12：session ID 绑定首次规范化 cwd；拒绝跨目录请求后原目录续会话仍可用。
#[test]
fn session_cwd_is_bound_and_rejected_request_preserves_original_session() {
    let server = server("normal");
    let first_dir = server.temp.path().join("session-first");
    let other_dir = server.temp.path().join("session-other");
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&other_dir).unwrap();
    let first = Plan::new("text");
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&first, false, &first_dir)),
        Some("bound-cwd-session"),
    );
    assert_eq!(response.status, 200);
    let session = response.headers["x-jchtools-session-id"].clone();
    let first_output = completion(&response.json()).to_owned();

    let changed = Plan::new("text");
    let changed_body = json!({"model":MODEL_A,"cwd":other_dir,"messages":[
        {"role":"user","content":first.prompt()},
        {"role":"assistant","content":first_output},
        {"role":"user","content":changed.prompt()}
    ]});
    let rejected = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&changed_body),
        Some(&session),
    );
    assert_eq!(rejected.status, 400);
    assert_eq!(rejected.json()["error"]["code"], "session_unavailable");
    assert!(!audit(&server.root)
        .iter()
        .any(|event| event["event"] == "started" && event["tag"] == changed.tag));

    let next = Plan::new("text");
    let accepted = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&continuation(&first, &next, MODEL_A, &first_dir)),
        Some(&session),
    );
    assert_eq!(accepted.status, 200);
    assert_eq!(accepted.headers["x-jchtools-session-id"], session);
    assert_eq!(completion(&accepted.json()), next.text());
    assert_eq!(
        wait_event(&server.root, "started", &next.tag)["session"],
        wait_event(&server.root, "started", &first.tag)["session"]
    );
}

/// AH-04/AH-06/AH-12：即使首轮仍在途且后续请求会排队，也必须先拒绝相同session ID的跨cwd续用。
#[test]
fn queued_session_cwd_change_is_rejected_before_waiting() {
    let server = server("normal");
    let first_dir = server.temp.path().join("queued-first");
    let other_dir = server.temp.path().join("queued-other");
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&other_dir).unwrap();
    let mut first = Plan::new("text");
    first.barrier = true;
    let mut stream = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&first, true, &first_dir)),
        Some("queued-bound-cwd-session"),
    );
    assert_eq!(stream.delta(), first.chunks[0]);
    wait_counts(&server, 1, 0);

    let changed = Plan::new("text");
    let changed_body = json!({"model":MODEL_A,"cwd":other_dir,"messages":[
        {"role":"user","content":first.prompt()},
        {"role":"assistant","content":first.text()},
        {"role":"user","content":changed.prompt()}
    ]});
    let (send, receive) = std::sync::mpsc::channel();
    let port = server.port;
    let request = thread::spawn(move || {
        let response = Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&changed_body),
            Some("queued-bound-cwd-session"),
        );
        let _sent = send.send((response.status, response.json()));
    });
    let outcome = receive.recv_timeout(Duration::from_secs(5));
    release(&server.root, &first);
    assert_eq!(stream.finish_stream(), first.chunks[1]);
    let outcome = outcome.expect("跨目录续会话必须在首轮释放前拒绝，不能先排队");
    request.join().unwrap();
    assert_eq!(outcome.0, 400, "{:?}", outcome.1);
    assert_eq!(outcome.1["error"]["code"], "session_unavailable");
    assert!(!audit(&server.root)
        .iter()
        .any(|event| event["event"] == "started" && event["tag"] == changed.tag));
}
/// AH-04/AH-12/AH-13：prompt 尚未发出时取消，只结束该请求并保留原历史与排队轮次。
#[test]
fn pre_prompt_cancel_preserves_session_and_waiter() {
    use jchtools_acp_api::acp_api::{
        ChatMessage, MessageRole, PromptInput, RequestEvent, SessionKey,
    };
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
            model: MODEL_B.to_owned(),
            cwd: server.workspace.clone(),
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
    assert!(audit(&server.root).iter().any(|event| {
        event["event"] == "model"
            && event["session"] == first_start["session"]
            && event["model"] == MODEL_B
    }));
    // 同步设置 token，第三轮的提交一定在取消之后、设置响应放行之前。
    request.cancellation.cancel();
    let third = Plan::new("text");
    let third_body = json!({"model":MODEL_A,"cwd":server.workspace.clone(),"messages":[
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
    assert_eq!(third_start["model"], MODEL_A);
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
    let mut second = Plan::new("text");
    second.chunks = vec!["续会话专属回答".into()];
    let request = json!({"model":MODEL_A,"cwd":server.workspace.clone(),"messages":[{"role":"user","content":first.prompt()},{"role":"assistant","content":output},{"role":"user","content":second.prompt()}]});
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
    // 已完成第二轮后，坏前缀用例仍须带完整第二轮和新的 user 输入。
    // 否则即使不校验首轮前缀，也可能仅因历史缺失/没有新增轮次而返回 400。
    let third = Plan::new("text");
    let mut next = request;
    next["messages"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":second.text()}),
        json!({"role":"user","content":third.prompt()}),
    ]);
    let mut changed = next.clone();
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
    let response = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&next),
        Some(&session),
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), third.text());
    let third_start = wait_event(&server.root, "started", &third.tag);
    assert_eq!(third_start["session"], event["session"]);
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "started")
            .count(),
        3
    );
}
