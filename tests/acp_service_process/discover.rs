use super::*;

// 2026-10-24 fulltest 并行负载复现：29 个真实进程用例并行时，服务拉起 Agent、握手
// 与逐项探查可能超过 10 秒（杀软扫描/负载尖峰，与 tests/git_tools.rs 同一现象），
// 先后触发本文件两个 10 秒等待与夹具 release 看门狗（initialize 应答转错误、backend
// 转 error）。预算提到 60 秒只延长失败路径；健康机器整个用例约 2 秒完成。
fn wait_process_audit(root: &Path, event: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(found) = audit(root)
            .into_iter()
            .find(|entry| entry["event"] == event)
        {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "真实后台Agent未发布{event}: {:?}",
            audit(root)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_service_phase(harness: &Harness, test: &str, phase: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = harness.call(test, "status", None);
        if status["ok"]["phase"] == phase {
            return status["ok"].clone();
        }
        assert!(Instant::now() < deadline, "后台未进入{phase}: {status}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn discovery_without_side_effects(
    harness: &Harness,
    test: &str,
    phase: &str,
    base_url: Option<&str>,
    instance: Option<&Value>,
) -> Value {
    let before = harness.call(test, "status", None)["ok"].clone();
    assert_eq!(before["phase"], phase);
    let saved = harness.call(test, "load", None);
    let before_audit = audit(&harness.audit);
    let first = harness.call(test, "discover", None);
    let descriptor = first["ok"].clone();
    let identity = descriptor["instance_id"].clone();
    assert!(
        uuid::Uuid::parse_str(identity.as_str().unwrap()).is_ok_and(|id| !id.is_nil()),
        "发现必须提供真实后台实例标识: {first}"
    );
    if let Some(expected) = instance {
        assert_eq!(&identity, expected, "同一后台进程不能改变instance_id");
    }
    // 精确比较字段集合和能力：不能泄漏PID、启动命令、配置、凭据或模型快照。
    assert_eq!(
        descriptor,
        json!({
            "protocol_version":1,
            "service_id":"jchtools-acp-http",
            "instance_id":identity,
            "phase":phase,
            "base_url":base_url,
            "execution_mode":"server_agent",
            "capabilities":{
                "text":true,
                "streaming":true,
                "client_tools":false,
                "server_tools":true
            }
        }),
        "Discover只能返回最小版本化描述: {first}"
    );
    for _ in 0..2 {
        assert_eq!(harness.call(test, "discover", None), first);
    }
    assert_eq!(
        harness.call(test, "status", None)["ok"],
        before,
        "Discover不能改变实际阶段、PID、配置或在途请求"
    );
    assert_eq!(harness.call(test, "load", None), saved);
    assert_eq!(
        audit(&harness.audit),
        before_audit,
        "Discover不能启动Agent、触发模型轮次或取消既有请求"
    );
    identity
}

/// AH-06/AH-12/AH-13：真实独立后台的三种HTTP断连只取消对应轮次，保留另一会话和唯一Agent。
#[test]
fn real_service_http_disconnect_cancels_only_corresponding_round() {
    if child_operation() {
        return;
    }
    use std::{
        io::Write,
        net::{Shutdown, TcpStream},
    };
    let test = "discover::real_service_http_disconnect_cancels_only_corresponding_round";
    let harness = Harness::new();
    let saved = harness.call(test, "save", Some(&harness.config("normal")));
    assert!(saved.get("error").is_none(), "{saved}");
    let ready = wait_service_phase(&harness, test, "ready");
    let service_pid = u32::try_from(ready["service_pid"].as_u64().unwrap()).unwrap();
    let agent_pid = u32::try_from(ready["agent_pid"].as_u64().unwrap()).unwrap();
    assert_ne!(service_pid, std::process::id());
    assert_ne!(agent_pid, service_pid);
    let service = ObservedProcess::open(service_pid);
    let agent = ObservedProcess::open(agent_pid);
    // 在同一个独立后台里分别覆盖首个SSE前、JSON完成前、已收到增量SSE。
    for (stream, incremental) in [(true, false), (false, false), (true, true)] {
        let mut victim = Plan::new(if incremental { "text" } else { "silent" });
        victim.barrier = true;
        let mut survivor = Plan::new("text");
        survivor.barrier = true;
        survivor.chunks = vec!["另一会话的首段".into(), "另一会话的完整尾段".into()];
        harness.register_hold(&victim);
        harness.register_hold(&survivor);
        let (socket, response) = if incremental {
            let mut response = Http::open(
                harness.port,
                "POST",
                "/v1/chat/completions",
                Some(&body(&victim, stream, &harness.root.join("acp-workspace"))),
                None,
            );
            assert_eq!(response.status, 200);
            assert_eq!(response.delta(), victim.chunks[0]);
            (None, Some(response))
        } else {
            // 直接发送完整HTTP请求；不等待响应头或首事件，也不退出消费者进程。
            let mut socket = TcpStream::connect(("127.0.0.1", harness.port)).unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let request = body(&victim, stream, &harness.root.join("acp-workspace")).to_string();
            write!(
                socket,
                "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{request}",
                request.len()
            )
            .unwrap();
            socket.flush().unwrap();
            (Some(socket), None)
        };
        let started = wait_event(&harness.audit, "started", &victim.tag);
        assert_eq!(started["pid"], agent_pid);
        let mut other = Http::open(
            harness.port,
            "POST",
            "/v1/chat/completions",
            Some(&body(&survivor, true, &harness.root.join("acp-workspace"))),
            None,
        );
        assert_eq!(other.status, 200);
        let prefix = other.delta();
        assert_eq!(prefix, survivor.chunks[0]);
        let other_started = wait_event(&harness.audit, "started", &survivor.tag);
        assert_ne!(started["session"], other_started["session"]);
        assert_eq!(other_started["pid"], agent_pid);
        assert!(!audit(&harness.audit)
            .iter()
            .any(|event| { event["event"] == "completed" && event["tag"] == victim.tag }));

        // shutdown(Both) / response owning socket的析构是真实断连；当前测试进程继续存活。
        if let Some(socket) = socket {
            socket.shutdown(Shutdown::Both).unwrap();
            drop(socket);
        }
        drop(response);
        let deadline = Instant::now() + Duration::from_secs(10);
        let cancel_events = loop {
            let events = audit(&harness.audit);
            let received = events.iter().any(|event| {
                event["event"] == "cancel-received"
                    && event["session"] == started["session"]
                    && event["pid"] == agent_pid
            });
            let consumed = events.iter().any(|event| {
                event["event"] == "cancelled"
                    && event["tag"] == victim.tag
                    && event["pid"] == agent_pid
            });
            if received && consumed {
                break events;
            }
            assert!(
                Instant::now() < deadline,
                "HTTP断连未取消对应真实Agent轮次 stream={stream} incremental={incremental}: {events:?}"
            );
            thread::sleep(Duration::from_millis(10));
        };
        // 所有取消证据在任何release/Stop之前取得，不能把清理取消当作HTTP断连。
        for plan in [&victim, &survivor] {
            assert!(!harness.audit.join(format!("release-{}", plan.tag)).exists());
        }
        assert!(
            !cancel_events.iter().any(|event| {
                (event["event"] == "cancel-received"
                    && event["session"] == other_started["session"])
                    || (event["event"] == "cancelled" && event["tag"] == survivor.tag)
                    || (event["event"] == "completed" && event["tag"] == survivor.tag)
            }),
            "另一会话必须仍持有barrier且未被取消: {cancel_events:?}"
        );
        assert!(service.is_alive());
        assert!(agent.is_alive());
        release(&harness.audit, &survivor);
        assert_eq!(prefix + &other.finish_stream(), survivor.text());
        wait_event(&harness.audit, "completed", &survivor.tag);
        drop(other);
        let next = Plan::new("text");
        let response = Http::open(
            harness.port,
            "POST",
            "/v1/chat/completions",
            Some(&body(&next, false, &harness.root.join("acp-workspace"))),
            None,
        );
        assert_eq!(response.status, 200);
        assert_eq!(completion(&response.json()), next.text());
        assert_eq!(
            wait_event(&harness.audit, "started", &next.tag)["pid"],
            agent_pid
        );
        let unchanged = harness.call(test, "status", None);
        assert_eq!(unchanged["ok"]["phase"], "ready");
        assert_eq!(unchanged["ok"]["service_pid"], service_pid);
        assert_eq!(unchanged["ok"]["agent_pid"], agent_pid);
        assert_eq!(
            audit(&harness.audit)
                .iter()
                .filter(|event| event["event"] == "spawn")
                .count(),
            1,
            "断连后第三个请求仍须复用原Agent"
        );
    }
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    agent.wait_gone();
    service.wait_gone();
}

/// AH-15/AH-A14：真实初始化hold窗口内Discover只报告Starting，不公布可用地址或推进初始化。
#[test]
fn real_service_discover_during_starting_is_minimal_stable_and_read_only() {
    if child_operation() {
        return;
    }
    let test = "discover::real_service_discover_during_starting_is_minimal_stable_and_read_only";
    let harness = Harness::new();
    let marker = harness.audit.join("release-initialize");
    harness.release_markers.borrow_mut().push(marker.clone());
    let config = harness.config("initialize-held");
    let saved = harness.call(test, "save", Some(&config));
    assert!(saved.get("error").is_none(), "{saved}");
    let held = wait_process_audit(&harness.audit, "initialize-held");
    let starting = wait_service_phase(&harness, test, "starting");
    assert_eq!(held["pid"], starting["agent_pid"]);
    let service_pid = u32::try_from(starting["service_pid"].as_u64().unwrap()).unwrap();
    let agent_pid = u32::try_from(starting["agent_pid"].as_u64().unwrap()).unwrap();
    assert_ne!(service_pid, std::process::id());
    assert!(process_alive(service_pid));
    assert!(process_alive(agent_pid));
    let instance = discovery_without_side_effects(&harness, test, "starting", None, None);
    assert!(!marker.exists(), "Discover不能释放初始化hold");
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    std::fs::write(&marker, b"initialize").unwrap();
    let ready = wait_service_phase(&harness, test, "ready");
    assert_eq!(ready["service_pid"], service_pid);
    assert_eq!(ready["agent_pid"], agent_pid);
    let address = format!("http://127.0.0.1:{}", harness.port);
    discovery_without_side_effects(&harness, test, "ready", Some(&address), Some(&instance));
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(agent_pid);
    wait_gone(service_pid);
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
}

/// AH-09/AH-15/AH-A14：真实Apply drain持有轮次时Discover不公布地址、不取消轮次、不应用保存配置。
#[test]
fn real_service_discover_during_draining_is_minimal_stable_and_read_only() {
    if child_operation() {
        return;
    }
    let test = "discover::real_service_discover_during_draining_is_minimal_stable_and_read_only";
    let harness = Harness::new();
    let original = harness.config("barrier-held");
    let saved = harness.call(test, "save", Some(&original));
    assert!(saved.get("error").is_none(), "{saved}");
    let ready = wait_service_phase(&harness, test, "ready");
    let address = format!("http://127.0.0.1:{}", harness.port);
    let instance = discovery_without_side_effects(&harness, test, "ready", Some(&address), None);
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true, &harness.root.join("acp-workspace"))),
        None,
    );
    let prefix = stream.delta();
    assert_eq!(prefix, plan.chunks[0]);
    wait_event(&harness.audit, "started", &plan.tag);
    let mut changed = original.clone();
    changed.port = unused_port();
    assert!(harness
        .call(test, "save", Some(&changed))
        .get("error")
        .is_none());
    let (apply, result) = harness.launch(test, "apply", None);
    let draining = wait_service_phase(&harness, test, "draining");
    assert_eq!(draining["service_pid"], ready["service_pid"]);
    assert_eq!(draining["agent_pid"], ready["agent_pid"]);
    assert_eq!(
        draining["saved_config"],
        serde_json::to_value(&changed).unwrap()
    );
    assert_eq!(
        draining["running_config"],
        serde_json::to_value(&original).unwrap()
    );
    discovery_without_side_effects(&harness, test, "draining", None, Some(&instance));
    assert!(!result.exists(), "Discover不能提前完成Apply");
    assert!(!harness.audit.join(format!("release-{}", plan.tag)).exists());
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    assert_admission_closed(harness.port, &harness.root.join("acp-workspace"));
    release(&harness.audit, &plan);
    assert_eq!(prefix + &stream.finish_stream(), plan.text());
    let applied = finish_child(apply, &result);
    assert_eq!(applied["ok"]["phase"], "ready", "{applied}");
    assert_eq!(applied["ok"]["service_pid"], ready["service_pid"]);
    assert_eq!(
        applied["ok"]["running_config"],
        serde_json::to_value(&changed).unwrap()
    );
    let address = format!("http://127.0.0.1:{}", changed.port);
    discovery_without_side_effects(&harness, test, "ready", Some(&address), Some(&instance));
    let next = Plan::new("text");
    let response = Http::open(
        changed.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), next.text());
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(u32::try_from(applied["ok"]["agent_pid"].as_u64().unwrap()).unwrap());
    wait_gone(u32::try_from(ready["service_pid"].as_u64().unwrap()).unwrap());
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
}

/// AH-10/AH-15/AH-A14：真实Stop等待存活自有terminal时Discover只报告Stopping，不改变安全等待。
#[test]
fn real_service_discover_during_stopping_is_minimal_stable_and_read_only() {
    if child_operation() {
        return;
    }
    let test = "discover::real_service_discover_during_stopping_is_minimal_stable_and_read_only";
    let harness = Harness::new();
    let saved = harness.call(test, "save", Some(&harness.config("normal")));
    assert!(saved.get("error").is_none(), "{saved}");
    let ready = wait_service_phase(&harness, test, "ready");
    let address = format!("http://127.0.0.1:{}", harness.port);
    let instance = discovery_without_side_effects(&harness, test, "ready", Some(&address), None);
    let plan = Plan::new("terminal-held");
    harness.register_hold(&plan);
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, false, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.status, 200);
    let value = response.json();
    assert!(serde_json::from_str::<Value>(completion(&value))
        .unwrap()
        .is_string());
    wait_event(&harness.audit, "completed", &plan.tag);
    let workspace = harness.root.join("acp-workspace");
    let pid_file = workspace.join(format!("terminal-{}.pid", plan.tag));
    let deadline = Instant::now() + Duration::from_secs(10);
    let terminal_pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "自有terminal未发布真实PID");
        thread::sleep(Duration::from_millis(10));
    };
    let terminal = ObservedProcess::open(terminal_pid);
    assert!(terminal.is_alive());
    let (stop, result) = harness.launch(test, "stop", None);
    let stopping = wait_service_phase(&harness, test, "stopping");
    assert_eq!(stopping["service_pid"], ready["service_pid"]);
    assert_eq!(stopping["agent_pid"], ready["agent_pid"]);
    discovery_without_side_effects(&harness, test, "stopping", None, Some(&instance));
    let marker = workspace.join(format!("terminal-release-{}", plan.tag));
    assert!(!marker.exists(), "Discover不能放行terminal");
    assert!(!result.exists(), "存活terminal未自然结束前Stop必须等待");
    assert!(terminal.is_alive());
    assert!(process_alive(
        u32::try_from(ready["service_pid"].as_u64().unwrap()).unwrap()
    ));
    assert!(process_alive(
        u32::try_from(ready["agent_pid"].as_u64().unwrap()).unwrap()
    ));
    std::fs::write(marker, b"terminal-completed").unwrap();
    let stopped = finish_child(stop, &result);
    assert_eq!(stopped["ok"]["phase"], "stopped", "{stopped}");
    assert!(stopped["ok"]["service_pid"].is_null());
    assert!(stopped["ok"]["agent_pid"].is_null());
    terminal.wait_gone();
    wait_gone(u32::try_from(ready["agent_pid"].as_u64().unwrap()).unwrap());
    wait_gone(u32::try_from(ready["service_pid"].as_u64().unwrap()).unwrap());
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
    let absent = harness.call(test, "status", None);
    assert_eq!(absent["ok"]["phase"], "stopped", "{absent}");
    assert!(absent["ok"]["service_pid"].is_null());
    assert!(absent["ok"]["agent_pid"].is_null());
}

/// AH-09/AH-10/AH-13：Stop抢占Apply时Agent取消后异常退出；收尾错误不得永久占住后台和单实例锁。
#[test]
fn stop_cancellation_agent_exit_releases_daemon_and_allows_restart() {
    if child_operation() {
        return;
    }
    let test = "discover::stop_cancellation_agent_exit_releases_daemon_and_allows_restart";
    let harness = Harness::new();
    let config = harness.config("cancel-exit");
    let saved = harness.call(test, "save", Some(&config));
    assert!(saved.get("error").is_none(), "{saved}");
    let running = harness.ready(test);
    let service_pid = running.service_pid.unwrap();
    let agent_pid = running.agent_pid.unwrap();
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(stream.delta(), plan.chunks[0]);
    let started = wait_event(&harness.audit, "started", &plan.tag);
    assert_eq!(started["pid"], agent_pid);
    let (apply, apply_result) = harness.launch(test, "apply", None);
    let draining = harness.phase(test, "draining");
    assert_eq!(draining["service_pid"], service_pid);
    assert_eq!(draining["agent_pid"], agent_pid);
    assert!(!apply_result.exists(), "自然Apply必须等待未释放的真实轮次");
    let (stop, stop_result) = harness.launch(test, "stop", None);
    let terminal = stream.sse();
    let stopped = finish_child(stop, &stop_result);
    let applied = finish_child(apply, &apply_result);
    assert!(terminal.contains("error"), "{terminal}");
    assert!(
        !terminal.contains("\"finish_reason\":\"stop\""),
        "{terminal}"
    );
    assert!(!harness.audit.join(format!("release-{}", plan.tag)).exists());
    assert!(
        audit(&harness.audit).iter().any(|event| {
            event["event"] == "cancel-exit"
                && event["session"] == started["session"]
                && event["pid"] == agent_pid
                && event["code"] == 75
        }),
        "必须由真实Stop的session/cancel触发自有Agent异常退出"
    );
    assert!(
        applied.get("error").is_some() || applied["ok"]["phase"] == "stopped",
        "被Stop抢占的Apply不得创建替换Agent: {applied}"
    );
    if let Some(error) = stopped.get("error") {
        assert!(error["kind"].is_string(), "{stopped}");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| !message.trim().is_empty()),
            "Stop收尾错误必须明确报告: {stopped}"
        );
    } else {
        assert_eq!(stopped["ok"]["phase"], "stopped", "{stopped}");
    }
    // 错误本身不是验收结果：旧Agent/HTTP实际收尾后，旧daemon必须自然退出。
    wait_gone(agent_pid);
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
    wait_gone(service_pid);
    let inactive = harness.call(test, "status", None);
    assert_eq!(inactive["ok"]["phase"], "stopped", "{inactive}");
    assert!(inactive["ok"]["service_pid"].is_null(), "{inactive}");
    assert!(inactive["ok"]["agent_pid"].is_null(), "{inactive}");
    assert_eq!(
        inactive["ok"]["saved_config"],
        serde_json::to_value(&config).unwrap()
    );
    let ensured = harness.call(test, "ensure", None);
    assert!(ensured.get("error").is_none(), "{ensured}");
    let reopened = harness.ready(test);
    assert_ne!(reopened.service_pid, Some(service_pid));
    assert_ne!(reopened.agent_pid, Some(agent_pid));
    assert!(process_alive(reopened.service_pid.unwrap()));
    assert!(process_alive(reopened.agent_pid.unwrap()));
    let models = Http::open(harness.port, "GET", "/v1/models", None, None);
    assert_eq!(models.status, 200);
    let models = models.json();
    assert_eq!(models["object"], "list");
    assert_eq!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![fixture::MODEL_A, fixture::MODEL_B]
    );
    let events = audit(&harness.audit);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "started" && event["tag"] == plan.tag)
            .count(),
        1,
        "取消失败轮次不得在重开后台后重放"
    );
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(reopened.agent_pid.unwrap());
    wait_gone(reopened.service_pid.unwrap());
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
}
