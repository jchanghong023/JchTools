use super::*;

/// 覆盖 AH-06/AH-07/AH-08/AH-09：程序/参数空格中文实际启动；client退出后旧新HTTP请求继续；新client连同一PID。
#[test]
fn independent_service_survives_facade_clients_and_reuses_one_agent() {
    if child_operation() {
        return;
    }
    let harness = Harness::new();
    let config = harness.config("normal");
    let saved = harness.call(
        "service_lifecycle::independent_service_survives_facade_clients_and_reuses_one_agent",
        "save",
        Some(&config),
    );
    assert!(saved.get("error").is_none(), "{saved}");
    let before = harness.ready(
        "service_lifecycle::independent_service_survives_facade_clients_and_reuses_one_agent",
    );
    assert_ne!(before.service_pid, Some(std::process::id()));
    let service_pid = before.service_pid.unwrap();
    let agent_pid = before.agent_pid.unwrap();
    assert_ne!(service_pid, agent_pid);
    assert!(process_alive(service_pid));
    assert!(process_alive(agent_pid));
    // Windows系统网络表的实际绑定，不用HTTP可达性推断没有局域网监听。
    let network = Command::new("netstat.exe")
        .args(["-ano", "-p", "TCP"])
        .output()
        .unwrap();
    assert!(network.status.success());
    let text = String::from_utf8_lossy(&network.stdout);
    let suffix = format!(":{}", harness.port);
    let pid_text = service_pid.to_string();
    let bindings = text
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.len() >= 5
                && fields[0] == "TCP"
                && fields[1].ends_with(&suffix)
                && fields[fields.len() - 1] == pid_text)
                .then(|| fields[1].to_owned())
        })
        .collect::<Vec<_>>();
    assert_eq!(bindings, vec![format!("127.0.0.1:{}", harness.port)]);
    let mut plan = Plan::new("text");
    plan.barrier = true;
    harness.register_hold(&plan);
    let mut response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&plan, true, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.delta(), plan.chunks[0]);
    // 两个启动/连接调用来自不同进程；退出这些消费者不得关掉HTTP或Agent。
    for _ in 0..2 {
        let connected = harness.call(
            "service_lifecycle::independent_service_survives_facade_clients_and_reuses_one_agent",
            "ensure",
            None,
        );
        assert_eq!(connected["ok"]["service_pid"], service_pid);
        assert_eq!(connected["ok"]["agent_pid"], agent_pid);
    }
    release(&harness.audit, &plan);
    assert_eq!(response.finish_stream(), plan.chunks[1]);
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
    let events = audit(&harness.audit);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    let spawn = events
        .iter()
        .find(|event| event["event"] == "spawn")
        .unwrap();
    let workspace = harness.root.join("acp-workspace").canonicalize().unwrap();
    assert_eq!(
        PathBuf::from(spawn["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        workspace
    );
    for session in events.iter().filter(|event| event["event"] == "session") {
        let cwd = PathBuf::from(session["cwd"].as_str().unwrap());
        assert!(cwd.is_absolute());
        assert_eq!(cwd.canonicalize().unwrap(), workspace);
    }
    assert_eq!(spawn["argv"][1], "--agent");
    assert_eq!(
        PathBuf::from(spawn["argv"][2].as_str().unwrap()),
        harness.audit
    );
    let persisted = harness.call(
        "service_lifecycle::independent_service_survives_facade_clients_and_reuses_one_agent",
        "load",
        None,
    );
    assert_eq!(persisted["config"], serde_json::to_value(&config).unwrap());
    let stopped = harness.call(
        "service_lifecycle::independent_service_survives_facade_clients_and_reuses_one_agent",
        "stop",
        None,
    );
    assert_eq!(stopped["ok"]["phase"], "stopped");
    wait_gone(service_pid);
    wait_gone(agent_pid);
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
}

/// 覆盖 AH-09：save立即持久不打断请求；apply停止接新、等待已入队请求结束才切换端口/argv。
#[test]
fn saving_is_non_disruptive_and_apply_drains_before_switching() {
    if child_operation() {
        return;
    }
    let test = "service_lifecycle::saving_is_non_disruptive_and_apply_drains_before_switching";
    let harness = Harness::new();
    let original = harness.config("normal");
    assert!(harness
        .call(test, "save", Some(&original))
        .get("error")
        .is_none());
    let running = harness.ready(test);
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
    let queued = Plan::new("text");
    let session = stream.headers["x-jchtools-session-id"].clone();
    let port = harness.port;
    let queued_body = json!({"model":fixture::MODEL_A,"messages":[{"role":"user","content":plan.prompt()},{"role":"assistant","content":plan.text()},{"role":"user","content":queued.prompt()}],"cwd":harness.root.join("acp-workspace")});
    let queued_job = thread::spawn(move || {
        let response = Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&queued_body),
            Some(&session),
        );
        assert_eq!(response.status, 200);
        response.json()
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = harness.call(test, "status", None);
        if status["ok"]["waiting"] == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "续会话等待未入队: {status}");
        thread::sleep(Duration::from_millis(10));
    }
    let mut changed = original.clone();
    changed.port = unused_port();
    let new_audit = harness.temp.path().join("应用后 参数");
    std::fs::create_dir(&new_audit).unwrap();
    changed.arguments[1] = new_audit.to_string_lossy().into_owned();
    let saved = harness.call(test, "save", Some(&changed));
    assert_eq!(
        saved["ok"]["saved_config"],
        serde_json::to_value(&changed).unwrap()
    );
    assert_eq!(
        saved["ok"]["running_config"],
        serde_json::to_value(&original).unwrap()
    );
    assert_eq!(saved["ok"]["agent_pid"], running.agent_pid.unwrap());
    assert_eq!(
        harness.call(test, "load", None)["config"],
        serde_json::to_value(&changed).unwrap()
    );
    let (apply, result) = harness.launch(test, "apply", None);
    harness.phase(test, "draining");
    assert!(process_alive(running.agent_pid.unwrap()));
    assert_admission_closed(harness.port, &harness.root.join("acp-workspace"));
    assert!(!result.exists(), "已有请求结束前不得应用完成");
    assert!(audit(&new_audit).is_empty(), "drain前不得增开Agent");
    release(&harness.audit, &plan);
    assert_eq!(stream.finish_stream(), plan.chunks[1]);
    assert_eq!(completion(&queued_job.join().unwrap()), queued.text());
    assert_eq!(
        wait_event(&harness.audit, "started", &queued.tag)["pid"],
        running.agent_pid.unwrap()
    );
    let applied = finish_child(apply, &result);
    assert_eq!(
        applied["ok"]["running_config"],
        serde_json::to_value(&changed).unwrap()
    );
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
    wait_gone(running.agent_pid.unwrap());
    assert_ne!(
        wait_event(&new_audit, "started", &next.tag)["pid"],
        running.agent_pid.unwrap()
    );
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
    let stopped = harness.call(test, "stop", None);
    assert_eq!(stopped["ok"]["phase"], "stopped");
}

/// 覆盖 AH-09/AH-10/AH-13：另一消费者的Stop必须打断Apply的drain等待，而不是排在其后。
#[test]
fn stop_during_apply_cancels_barrier_without_starting_replacement_agent() {
    if child_operation() {
        return;
    }
    let test =
        "service_lifecycle::stop_during_apply_cancels_barrier_without_starting_replacement_agent";
    let harness = Harness::new();
    let original = harness.config("barrier-held");
    let saved = harness.call(test, "save", Some(&original));
    assert!(saved.get("error").is_none(), "{saved}");
    let running = harness.ready(test);
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
    let replacement_audit = harness.temp.path().join("不得启动的替换Agent");
    std::fs::create_dir(&replacement_audit).unwrap();
    let mut changed = original;
    changed.port = unused_port();
    changed.arguments[1] = replacement_audit.to_string_lossy().into_owned();
    let saved = harness.call(test, "save", Some(&changed));
    assert!(saved.get("error").is_none(), "{saved}");
    let (apply, apply_result) = harness.launch(test, "apply", None);
    let draining = harness.phase(test, "draining");
    assert_eq!(draining["agent_pid"], running.agent_pid.unwrap());
    assert!(!apply_result.exists(), "barrier仍持有时Apply必须等待");
    assert!(audit(&replacement_audit).is_empty());
    // 不释放barrier：只能由第二个真实控制消费者的Stop取消本轮次。
    let (stop, stop_result) = harness.launch(test, "stop", None);
    wait_event(&harness.audit, "cancelled", &plan.tag);
    assert!(!harness.audit.join(format!("release-{}", plan.tag)).exists());
    let cancelled = stream.sse();
    assert!(cancelled.contains("error"), "{cancelled}");
    assert!(
        !cancelled.contains("\"finish_reason\":\"stop\""),
        "{cancelled}"
    );
    let stopped = finish_child(stop, &stop_result);
    assert_eq!(stopped["ok"]["phase"], "stopped");
    assert!(stopped["ok"]["agent_pid"].is_null());
    let applied = finish_child(apply, &apply_result);
    assert!(
        applied.get("error").is_some() || applied["ok"]["phase"] == "stopped",
        "被Stop抢占的Apply不得重新报告Ready: {applied}"
    );
    wait_gone(running.agent_pid.unwrap());
    wait_gone(running.service_pid.unwrap());
    assert!(
        audit(&replacement_audit).is_empty(),
        "Stop不得触发替换Agent"
    );
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|entry| entry["event"] == "spawn")
            .count(),
        1
    );
    let final_status = harness.call(test, "status", None);
    assert_eq!(final_status["ok"]["phase"], "stopped");
    assert!(final_status["ok"]["agent_pid"].is_null());
    assert!(std::net::TcpStream::connect(("127.0.0.1", changed.port)).is_err());
}

/// 覆盖 AH-06/AH-08/AH-09：首次端口冲突保留可查询后台，保存后允许GUI应用并直接Apply恢复同一后台。
#[test]
fn occupied_initial_port_preserves_service_and_apply_recovers_without_stop() {
    if child_operation() {
        return;
    }
    let test = "service_lifecycle::occupied_initial_port_preserves_service_and_apply_recovers_without_stop";
    let harness = Harness::new();
    let occupied = std::net::TcpListener::bind(("127.0.0.1", harness.port)).unwrap();
    let saved = harness.call(test, "save", Some(&harness.config("normal")));
    assert!(saved.get("error").is_none(), "{saved}");
    let failed = harness.phase(test, "error");
    let service_pid = u32::try_from(failed["service_pid"].as_u64().unwrap()).unwrap();
    assert!(process_alive(service_pid), "端口冲突不得退出控制后台");
    assert!(failed["agent_pid"].is_null());
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains(&harness.port.to_string()));
    for _ in 0..3 {
        let queried = harness.call(test, "status", None);
        assert_eq!(queried["ok"]["phase"], "error");
        assert_eq!(queried["ok"]["service_pid"], service_pid);
        assert!(process_alive(service_pid));
    }
    assert!(audit(&harness.audit).is_empty(), "端口占用不得启动Agent");
    let mut changed = harness.config("normal");
    changed.port = unused_port();
    let saved = harness.call(test, "save", Some(&changed));
    assert_eq!(saved["ok"]["service_pid"], service_pid);
    assert_eq!(saved["ok"]["phase"], "error", "save不得暗中重启");
    let saved_status: ServiceStatus = serde_json::from_value(saved["ok"].clone()).unwrap();
    assert!(
        saved_status.pending_apply(),
        "首次启动失败后保存可用端口必须允许GUI应用，即使尚无运行实例: {saved}"
    );
    assert_eq!(
        harness.call(test, "load", None)["config"],
        serde_json::to_value(&changed).unwrap()
    );
    // 原端口继续由本测试占用，且不先Stop；Apply必须使用刚保存的可用端口。
    let applied = harness.call(test, "apply", None);
    assert_eq!(applied["ok"]["phase"], "ready", "{applied}");
    assert_eq!(applied["ok"]["service_pid"], service_pid);
    assert_eq!(
        applied["ok"]["running_config"],
        serde_json::to_value(&changed).unwrap()
    );
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
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|entry| entry["event"] == "spawn")
            .count(),
        1
    );
    drop(occupied);
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(service_pid);
}

/// 覆盖 AH-08/AH-13：不提供ACP且立即自然退出的Agent，其旧PID不得被status重新公布。
#[test]
fn immediately_exited_agent_pid_stays_absent_across_status_queries() {
    if child_operation() {
        return;
    }
    let test = "service_lifecycle::immediately_exited_agent_pid_stays_absent_across_status_queries";
    let harness = Harness::new();
    let tag = uuid::Uuid::new_v4().to_string();
    let mut short_lived = harness.config("normal");
    short_lived.arguments = vec![
        "--terminal".into(),
        harness.audit.to_string_lossy().into_owned(),
        tag.clone(),
        "terminal".into(),
    ];
    // 既有fixture的terminal模式记录自身PID，输出非ACP字节后立即以7退出。
    let saved = harness.call(test, "save", Some(&short_lived));
    assert!(saved.get("error").is_none(), "{saved}");
    let pid_file = harness.audit.join(format!("terminal-{tag}.pid"));
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "短寿命Agent没有记录真实PID");
        thread::sleep(Duration::from_millis(10));
    };
    wait_gone(pid);
    let deadline = Instant::now() + Duration::from_secs(10);
    let settled = loop {
        let queried = harness.call(test, "status", None);
        assert!(queried.get("error").is_none(), "{queried}");
        assert_ne!(queried["ok"]["phase"], "ready", "无ACP程序不得就绪");
        if queried["ok"]["phase"] == "error" && queried["ok"]["agent_pid"].is_null() {
            break queried;
        }
        assert!(
            Instant::now() < deadline,
            "已退出Agent的PID未清除: {queried}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(settled["ok"]["error"].is_string());
    for _ in 0..5 {
        let queried = harness.call(test, "status", None);
        assert_eq!(queried["ok"]["phase"], "error");
        assert!(
            queried["ok"]["agent_pid"].is_null(),
            "查询复活了旧PID {pid}: {queried}"
        );
        assert_eq!(queried["ok"]["service_pid"], settled["ok"]["service_pid"]);
        assert!(!process_alive(pid));
    }
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
}

/// 覆盖 AH-08/AH-A07：占用端口与握手失败明确错误、不换端口、不报告Ready。
#[test]
fn occupied_port_and_initialize_failure_are_specific_and_never_ready() {
    if child_operation() {
        return;
    }
    let test =
        "service_lifecycle::occupied_port_and_initialize_failure_are_specific_and_never_ready";
    let harness = Harness::new();
    let occupied = std::net::TcpListener::bind(("127.0.0.1", harness.port)).unwrap();
    let result = harness.call(test, "save", Some(&harness.config("normal")));
    assert_ne!(result["ok"]["phase"], "ready");
    let failed = harness.phase(test, "error");
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains(&harness.port.to_string()));
    assert!(
        audit(&harness.audit).is_empty(),
        "端口占用不得改端口或启动Agent"
    );
    drop(occupied);
    let _stop = harness.call(test, "stop", None);
    let failed = harness.call(test, "save", Some(&harness.config("handshake-error")));
    assert_ne!(failed["ok"]["phase"], "ready");
    let error = harness.phase(test, "error");
    let diagnostic = error["error"].as_str().unwrap();
    assert!(
        diagnostic.contains("initialize") || diagnostic.contains("初始化"),
        "{error}"
    );
}

/// 覆盖 AH-10/AH-13/AH-A09：主动stop取消在途轮次，不伪成功，最终释放服务和自有Agent。
#[test]
fn explicit_stop_cancels_inflight_requests_then_releases_owned_processes() {
    if child_operation() {
        return;
    }
    let test =
        "service_lifecycle::explicit_stop_cancels_inflight_requests_then_releases_owned_processes";
    let harness = Harness::new();
    assert!(harness
        .call(test, "save", Some(&harness.config("normal")))
        .get("error")
        .is_none());
    let status = harness.ready(test);
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
    let (stop, result) = harness.launch(test, "stop", None);
    wait_event(&harness.audit, "cancelled", &plan.tag);
    let terminal = stream.sse();
    assert!(terminal.contains("error"), "{terminal}");
    assert!(!terminal.contains("\"finish_reason\":\"stop\""));
    let stopped = finish_child(stop, &result);
    assert_eq!(stopped["ok"]["phase"], "stopped");
    wait_gone(status.agent_pid.unwrap());
    wait_gone(status.service_pid.unwrap());
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
}

/// AH-09/AH-10：已排队但晚于显式 Stop 的 Apply 只能作用于已有 daemon，不能充当 Ensure。
#[test]
fn queued_apply_after_explicit_stop_does_not_restart_absent_service() {
    if child_operation() {
        return;
    }
    let test =
        "service_lifecycle::queued_apply_after_explicit_stop_does_not_restart_absent_service";
    let harness = Harness::new();
    let config = harness.config("normal");
    let saved_config = serde_json::to_value(&config).unwrap();
    // AH-09 的首次保存自动启动保持不变；不是把所有自动启动一并禁止。
    let saved = harness.call(test, "save", Some(&config));
    assert!(saved.get("error").is_none(), "{saved}");
    let running = harness.ready(test);
    let service_pid = running.service_pid.unwrap();
    let agent_pid = running.agent_pid.unwrap();
    assert!(process_alive(service_pid));
    assert!(process_alive(agent_pid));
    let initial_audit = audit(&harness.audit);
    assert_eq!(
        initial_audit
            .iter()
            .filter(|event| event["event"] == "spawn" && event["pid"] == agent_pid)
            .count(),
        1,
        "必须先观察真实 Agent 启动，而不是只相信 Ready 快照"
    );
    let discovery = harness.call(test, "discover", None);
    assert_eq!(discovery["ok"]["phase"], "ready", "{discovery}");

    // 模拟 GUI observer 已排队、但还没有发 IPC 的 Apply；不占用硬件鼠标。
    let (apply, apply_result) = harness.launch(test, "queued-apply", None);
    let queued = ReleaseMarker {
        path: harness.root.join("release-queued-apply"),
        consumer: Some(apply),
    };
    assert_eq!(
        wait_file(&harness.root.join("queued-apply.json"))["queued"],
        true
    );
    assert!(!apply_result.exists(), "放行前晚到Apply不得发出IPC");
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(agent_pid);
    wait_gone(service_pid);
    assert!(std::net::TcpStream::connect(("127.0.0.1", config.port)).is_err());
    let retired_audit = audit(&harness.audit);
    assert_eq!(retired_audit, initial_audit, "空闲 Stop 不得另开 Agent");
    let absent = harness.call(test, "discover", None);
    assert_eq!(absent, json!({"ok":null}), "只读 Discover 必须确认管道缺席");
    let stopped = harness.call(test, "status", None);
    assert_eq!(stopped["ok"]["phase"], "stopped", "{stopped}");
    assert!(stopped["ok"]["service_pid"].is_null(), "{stopped}");
    assert!(stopped["ok"]["agent_pid"].is_null(), "{stopped}");
    assert_eq!(stopped["ok"]["saved_config"], saved_config);
    assert_eq!(harness.call(test, "load", None)["config"], saved_config);

    // 两个旧 PID 已真实退出、Discover 已确认管道缺席后才执行公开 Apply API。
    let applied = queued.finish(&apply_result);
    let after_discovery = harness.call(test, "discover", None);
    let after_status = harness.call(test, "status", None);
    let after_audit = audit(&harness.audit);
    let still_saved = harness.call(test, "load", None);
    let listening = std::net::TcpStream::connect(("127.0.0.1", config.port)).is_ok();
    assert!(
        matches!(
            applied["error"]["kind"].as_str(),
            Some("not_ready" | "stopping")
        ),
        "缺席后台的晚到Apply必须明确拒绝，不得通过ensure_started回退重新启动: \
         Apply={applied}, Discover={after_discovery}, Status={after_status}, audit={after_audit:?}"
    );
    assert!(
        applied["error"]["message"]
            .as_str()
            .is_some_and(|message| !message.trim().is_empty()),
        "{applied}"
    );
    assert_eq!(after_discovery, json!({"ok":null}), "Apply 不得创建后台");
    assert_eq!(after_status["ok"]["phase"], "stopped", "{after_status}");
    assert!(
        after_status["ok"]["service_pid"].is_null(),
        "{after_status}"
    );
    assert!(after_status["ok"]["agent_pid"].is_null(), "{after_status}");
    assert_eq!(after_audit, retired_audit, "Apply 不得另开 Agent");
    assert!(!listening, "Apply 不得重新建立 HTTP 监听");
    assert_eq!(after_status["ok"]["saved_config"], saved_config);
    assert_eq!(still_saved["config"], saved_config);
    assert!(!process_alive(service_pid));
    assert!(!process_alive(agent_pid));

    // 用户新打开 GUI 的显式 Ensure 仍可从原样保存的配置启动。
    let ensured = harness.call(test, "ensure", None);
    assert!(ensured.get("error").is_none(), "{ensured}");
    let reopened = harness.ready(test);
    assert!(process_alive(reopened.service_pid.unwrap()));
    assert!(process_alive(reopened.agent_pid.unwrap()));
    assert_eq!(reopened.saved_config, Some(config.clone()));
    assert_eq!(reopened.running_config, Some(config));
    assert_eq!(harness.call(test, "discover", None)["ok"]["phase"], "ready");
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2,
        "只有首次保存和新的显式 Ensure 可以各启动一次 Agent"
    );
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    wait_gone(reopened.agent_pid.unwrap());
    wait_gone(reopened.service_pid.unwrap());
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
    assert_eq!(harness.call(test, "load", None)["config"], saved_config);
}

/// 覆盖 AH-08/AH-A07：缺失命令和存在但不可执行的文件均显示具体启动错误，不冒充Ready。
#[test]
fn missing_or_unlaunchable_command_reports_start_failure() {
    if child_operation() {
        return;
    }
    let test = "service_lifecycle::missing_or_unlaunchable_command_reports_start_failure";
    for exists in [false, true] {
        let harness = Harness::new();
        let mut config = harness.config("normal");
        let invalid = harness.temp.path().join(if exists {
            "not executable.exe"
        } else {
            "missing Agent.exe"
        });
        if exists {
            std::fs::write(&invalid, b"synthetic non executable").unwrap();
        }
        config.executable = invalid.to_string_lossy().into_owned();
        let saved = harness.call(test, "save", Some(&config));
        assert_ne!(saved["ok"]["phase"], "ready");
        let error = harness.phase(test, "error");
        let message = error["error"].as_str().unwrap();
        assert!(
            message.contains("启动") && message.contains("Agent"),
            "{error}"
        );
        assert!(audit(&harness.audit).is_empty());
        assert!(error["agent_pid"].is_null());
    }
}
