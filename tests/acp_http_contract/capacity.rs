use super::*;

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
        Some(&body(&first, true, &server.workspace)),
        None,
    );
    assert_eq!(a.status, 200);
    assert!(a.headers["content-type"].starts_with("text/event-stream"));
    assert_eq!(a.delta(), first.chunks[0]);
    let mut b = Http::open(
        server.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&second, true, &server.workspace)),
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
        Some(&body(&first, true, &server.workspace)),
        Some("fifo-session"),
    );
    assert_eq!(stream.delta(), first.chunks[0]);
    let mut second = Plan::new("text");
    second.chunks = vec!["second-only".into()];
    let mut third = Plan::new("text");
    third.chunks = vec!["third-only".into()];
    let history = vec![
        json!({"role":"user","content":first.prompt()}),
        json!({"role":"assistant","content":first.text()}),
    ];
    let mut second_history = history.clone();
    second_history.push(json!({"role":"user","content":second.prompt()}));
    let port = server.port;
    let second_body =
        json!({"model":MODEL_A,"cwd":server.workspace.clone(),"messages":second_history});
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
    let third_body =
        json!({"model":MODEL_A,"cwd":server.workspace.clone(),"messages":third_history});
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
            Some(&body(&plan, true, &server.workspace)),
            None,
        );
        assert_eq!(response.delta(), plan.chunks[0]);
        active.push((plan, response));
    }
    wait_counts(&server, 4, 0);
    let mut queued = Vec::new();
    for index in 0..16 {
        let plan = Plan::new("text");
        let request = body(&plan, false, &server.workspace);
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
        Some(&body(&victim, true, &server.workspace)),
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
        let socket = send_without_reading(server.port, &body(&plan, stream, &server.workspace));
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
            Some(&body(&plan, true, &server.workspace)),
            None,
        );
        assert_eq!(response.delta(), plan.chunks[0]);
        active.push((plan, response));
    }
    let queued = Plan::new("silent");
    let socket = send_without_reading(server.port, &body(&queued, true, &server.workspace));
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
        Some(&body(&first, true, &server.workspace)),
        Some("history-recovery"),
    );
    assert_eq!(stream.delta(), first.chunks[0]);
    let bad = Plan::new("text");
    let bad_request = body(&bad, false, &server.workspace);
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
        Some(&continuation(&first, &second, MODEL_A, &server.workspace)),
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
