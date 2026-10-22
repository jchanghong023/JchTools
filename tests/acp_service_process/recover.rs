use super::*;

/// 覆盖 AH-06/AH-13/AH-A09：真实后台持有的Agent异常退出，所有旧轮次失败，下一个请求重建唯一Agent。
#[test]
fn real_service_recovers_agent_exit_without_replaying_failed_requests() {
    if child_operation() {
        return;
    }
    let test = "recover::real_service_recovers_agent_exit_without_replaying_failed_requests";
    let harness = Harness::new();
    assert!(harness
        .call(test, "save", Some(&harness.config("normal")))
        .get("error")
        .is_none());
    let status = harness.ready(test);
    // 在退出发生前持有原 Agent 身份，不能用可能已复用的 PID 证明它退出。
    let agent = ObservedProcess::open(status.agent_pid.unwrap());
    let mut victim = Plan::new("text");
    victim.barrier = true;
    harness.register_hold(&victim);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&victim, true, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(stream.delta(), victim.chunks[0]);
    let crash = Plan::new("crash");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&crash, false, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.status, 502);
    assert!(response.json().get("error").is_some());
    let event = stream.sse();
    assert!(event.contains("error"), "{event}");
    assert!(!event.contains("\"finish_reason\":\"stop\""));
    agent.wait_gone();
    assert!(process_alive(status.service_pid.unwrap()));
    let next = Plan::new("text");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false, &harness.root.join("acp-workspace"))),
        None,
    );
    let response_status = response.status;
    let response = response.json();
    assert_eq!(response_status, 200, "退出后首次新请求必须恢复：{response}");
    assert_eq!(completion(&response), next.text());
    let recovered = harness.ready(test);
    assert_eq!(recovered.service_pid, status.service_pid);
    assert_ne!(recovered.agent_pid, status.agent_pid);
    let events = audit(&harness.audit);
    assert_eq!(
        events
            .iter()
            .filter(|entry| entry["event"] == "spawn")
            .count(),
        2
    );
    for plan in [&victim, &crash] {
        assert_eq!(
            events
                .iter()
                .filter(|entry| entry["event"] == "started" && entry["tag"] == plan.tag)
                .count(),
            1
        );
    }
}

/// 覆盖 AH-10/AH-13/AH-A11：服务退出释放仍归本应用拥有的实际terminal子进程，不留孤儿。
#[test]
fn stopping_service_releases_owned_terminal_process() {
    if child_operation() {
        return;
    }
    let test = "recover::stopping_service_releases_owned_terminal_process";
    let harness = Harness::new();
    assert!(harness
        .call(test, "save", Some(&harness.config("normal")))
        .get("error")
        .is_none());
    let status = harness.ready(test);
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
    let terminal_id: Value = serde_json::from_str(completion(&value)).unwrap();
    assert!(terminal_id.is_string());
    let pid_file = harness
        .root
        .join("acp-workspace")
        .join(format!("terminal-{}.pid", plan.tag));
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "terminal创建回调没有执行实际命令"
        );
        thread::sleep(Duration::from_millis(10));
    };
    let (stop, result) = harness.launch(test, "stop", None);
    let stopping = harness.phase(test, "stopping");
    assert_eq!(stopping["service_pid"], status.service_pid.unwrap());
    assert_eq!(stopping["agent_pid"], status.agent_pid.unwrap());
    assert!(process_alive(status.service_pid.unwrap()));
    assert!(process_alive(status.agent_pid.unwrap()));
    assert!(!result.exists(), "自有terminal仍运行时Stop不得完成");
    assert!(process_alive(pid), "必须观察Stop正在等待存活terminal");
    // 合成终端自己完成工作；安全退出必须随后回收它，而不是只清掉terminal表。
    std::fs::write(
        harness
            .root
            .join("acp-workspace")
            .join(format!("terminal-release-{}", plan.tag)),
        b"done",
    )
    .unwrap();
    assert_eq!(finish_child(stop, &result)["ok"]["phase"], "stopped");
    wait_gone(pid);
    wait_gone(status.agent_pid.unwrap());
    wait_gone(status.service_pid.unwrap());
}

/// 覆盖 AH-08/AH-10/AH-13：初始化失败且子进程不因EOF退出时，安全等待其自然结束并回收。
#[test]
fn initialize_failure_reclaims_agent_that_does_not_exit_on_eof() {
    if child_operation() {
        return;
    }
    let test = "recover::initialize_failure_reclaims_agent_that_does_not_exit_on_eof";
    let harness = Harness::new();
    let saved = harness.call(test, "save", Some(&harness.config("handshake-error-held")));
    assert_ne!(saved["ok"]["phase"], "ready");
    harness.phase(test, "error");
    let events = audit(&harness.audit);
    let spawned = events
        .iter()
        .find(|event| event["event"] == "spawn")
        .unwrap();
    let pid = u32::try_from(spawned["pid"].as_u64().unwrap()).unwrap();
    let agent = ObservedProcess::open(pid);
    assert!(agent.is_alive(), "夹具必须在初始化失败后仍存活");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    // handshake-error-held 只有 release-exit 才自然退出；不能要求产品强杀自有Agent。
    let (stop, result) = harness.launch(test, "stop", None);
    let stopping = harness.phase(test, "stopping");
    assert_eq!(stopping["agent_pid"], pid);
    assert!(agent.is_alive(), "Stop不得强杀尚未自然退出的Agent");
    assert!(!result.exists(), "失败Agent仍存活时Stop必须等待");
    std::fs::write(harness.audit.join("release-exit"), b"natural-exit").unwrap();
    agent.wait_gone();
    let stopped = finish_child(stop, &result);
    assert_eq!(stopped["ok"]["phase"], "stopped");
}

/// 覆盖 AH-12/AH-13：Ready后连接EOF但原Agent还存活，有限时间明确失败且不得增开第二Agent。
#[test]
fn live_retiring_agent_blocks_reconnect_with_specific_error_not_a_hang() {
    if child_operation() {
        return;
    }
    let test = "recover::live_retiring_agent_blocks_reconnect_with_specific_error_not_a_hang";
    let harness = Harness::new();
    assert!(harness
        .call(test, "save", Some(&harness.config("disconnect-held")))
        .get("error")
        .is_none());
    let ready = harness.ready(test);
    // 在 Agent 仍存活时固定进程身份；退出观察不能重新打开可能复用的 PID。
    let agent = ObservedProcess::open(ready.agent_pid.unwrap());
    let failed = Plan::new("disconnect");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&failed, false, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.status, 502);
    assert!(response.json().get("error").is_some());
    assert!(agent.is_alive(), "本用例必须观察到连接退休但PID仍存活");
    let retry = Plan::new("text");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&retry, false, &harness.root.join("acp-workspace"))),
        None,
    );
    assert_eq!(response.status, 502);
    let error = response.json();
    assert_eq!(error["error"]["code"], "agent_disconnected");
    let retiring = harness.call(test, "status", None);
    assert_eq!(retiring["ok"]["agent_pid"], ready.agent_pid.unwrap());
    assert!(agent.is_alive());
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
    assert!(!audit(&harness.audit)
        .iter()
        .any(|event| event["event"] == "started" && event["tag"] == retry.tag));
    std::fs::write(harness.audit.join("release-exit"), b"natural-exit").unwrap();
    agent.wait_gone();
    let next = Plan::new("text");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&next, false, &harness.root.join("acp-workspace"))),
        None,
    );
    let status = response.status;
    let response = response.json();
    let failed_status = (status != 200).then(|| harness.call(test, "status", None));
    assert_eq!(
        status,
        200,
        "自然退出后首次新请求必须恢复：{response}；status={failed_status:?}；audit={:?}",
        audit(&harness.audit)
    );
    assert_eq!(completion(&response), next.text());
    assert_ne!(
        wait_event(&harness.audit, "started", &next.tag)["pid"],
        ready.agent_pid.unwrap()
    );
    assert!(!audit(&harness.audit)
        .iter()
        .any(|event| event["event"] == "started" && event["tag"] == retry.tag));
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "started" && event["tag"] == failed.tag)
            .count(),
        1
    );
    assert_eq!(
        audit(&harness.audit)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2
    );
}

// P2-03/AH-09/AH-10：只同步“不完整头已被服务读取”的前提；
// 结论只由自然 socket 关闭、公开控制结果和真实进程/HTTP 消费结果判定。
struct PartialHeaderSocket(std::net::TcpStream);
impl PartialHeaderSocket {
    fn open(harness: &Harness) -> Self {
        use std::io::Write;
        let mut socket = Self(std::net::TcpStream::connect(("127.0.0.1", harness.port)).unwrap());
        socket
            .0
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .0
            .write_all(b"GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Review-Remediation: ")
            .unwrap();
        let first_read = harness.root.join(format!(
            "http-read-{}",
            socket.0.local_addr().unwrap().port()
        ));
        let deadline = Instant::now() + Duration::from_secs(15);
        while !first_read.exists() {
            assert!(
                Instant::now() < deadline,
                "半个HTTP头未进入真实服务的连接读取器"
            );
            thread::sleep(Duration::from_millis(10));
        }
        socket.0.set_nonblocking(true).unwrap();
        socket
    }

    fn finish_control(
        &mut self,
        mut child: ConsumerChild,
        result: &Path,
        hold_for: Duration,
        release: impl FnOnce(),
    ) -> Value {
        use std::io::Read;
        // 仅本项容纳原有30秒header timeout；不放宽suite其他控制/业务deadline。
        let deadline = Instant::now() + Duration::from_secs(40);
        let release_at = Instant::now() + hold_for;
        let mut release = Some(release);
        let mut socket_closed = false;
        let mut exited = None;
        let mut bytes = [0_u8; 1024];
        while Instant::now() < deadline {
            if Instant::now() >= release_at {
                if let Some(release) = release.take() {
                    release();
                }
            }
            if !socket_closed {
                match self.0.read(&mut bytes) {
                    Ok(0) => socket_closed = true,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) =>
                    {
                        socket_closed = true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("观察半头socket自然关闭失败: {error}"),
                }
            }
            if exited.is_none() {
                exited = child.try_wait().unwrap();
            }
            if socket_closed && exited.is_some() && release.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // 必须先观察服务关闭socket，不能只信控制完成（abort外层serve也可能丢detach连接）。
        assert!(
            socket_closed,
            "P2-03：半HTTP头socket在40秒内未自然关闭；不得永久占用Stop/Apply"
        );
        let exited = exited.expect("P2-03：socket关闭后公开Stop/Apply仍未在40秒内完成");
        assert!(exited.success(), "控制消费者异常退出: {exited}");
        serde_json::from_slice(&std::fs::read(result).unwrap()).unwrap()
    }
}
impl Drop for PartialHeaderSocket {
    fn drop(&mut self) {
        // finally：失败也先主动关测试socket，再由稍后析构的Harness正常Stop；
        // 不把服务/Agent强杀当作资源已释放，也不留无timer版本的僵持连接。
        let _closed = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// P2-03/AH-10/AH-A09：真实后台Stop必须等半头连接自然退役，而非无限等待或detach。
#[test]
fn review_remediation_http_partial_header_stop_releases_socket_and_service() {
    if child_operation() {
        return;
    }
    let test = "recover::review_remediation_http_partial_header_stop_releases_socket_and_service";
    let harness = Harness::new();
    let saved = harness.call(test, "save", Some(&harness.config("normal")));
    assert!(saved.get("error").is_none(), "{saved}");
    let running = harness.ready(test);
    let service = ObservedProcess::open(running.service_pid.unwrap());
    let agent = ObservedProcess::open(running.agent_pid.unwrap());
    let mut socket = PartialHeaderSocket::open(&harness);
    let (stop, result) = harness.launch(test, "stop", None);
    let stopped = socket.finish_control(stop, &result, Duration::ZERO, || {});
    assert_eq!(stopped["ok"]["phase"], "stopped", "{stopped}");
    assert!(stopped["ok"]["agent_pid"].is_null(), "{stopped}");
    agent.wait_gone();
    service.wait_gone();
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
}

/// P2-03/AH-09/AH-A09：Apply自然关闭旧半头socket后，新端点必须可消费models。
#[test]
fn review_remediation_http_partial_header_apply_releases_socket_and_serves_new_endpoint() {
    if child_operation() {
        return;
    }
    let test =
        "recover::review_remediation_http_partial_header_apply_releases_socket_and_serves_new_endpoint";
    let harness = Harness::new();
    let original = harness.config("barrier-held");
    let saved = harness.call(test, "save", Some(&original));
    assert!(saved.get("error").is_none(), "{saved}");
    let running = harness.ready(test);
    let agent = ObservedProcess::open(running.agent_pid.unwrap());
    // P2-03：合法业务轮次保持超过30秒，header timer不得变成全局排空deadline。
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
    let mut changed = original;
    changed.port = unused_port();
    let saved = harness.call(test, "save", Some(&changed));
    assert!(saved.get("error").is_none(), "{saved}");
    let mut socket = PartialHeaderSocket::open(&harness);
    let (apply, result) = harness.launch(test, "apply", None);
    let applied = socket.finish_control(apply, &result, Duration::from_secs(32), || {
        assert!(
            !result.exists(),
            "P2-03：超过30秒的合法在途轮次未结束前Apply不得完成"
        );
        assert!(agent.is_alive(), "合法轮次排空前不得终止旧Agent");
        release(&harness.audit, &plan);
    });
    assert_eq!(stream.finish_stream(), plan.chunks[1]);
    assert_eq!(applied["ok"]["phase"], "ready", "{applied}");
    assert_eq!(
        applied["ok"]["running_config"],
        serde_json::to_value(&changed).unwrap()
    );
    assert_eq!(applied["ok"]["service_pid"], running.service_pid.unwrap());
    assert_ne!(applied["ok"]["agent_pid"], running.agent_pid.unwrap());
    agent.wait_gone();
    assert!(std::net::TcpStream::connect(("127.0.0.1", harness.port)).is_err());
    let models = Http::open(changed.port, "GET", "/v1/models", None, None);
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
    let stopped = harness.call(test, "stop", None);
    assert_eq!(stopped["ok"]["phase"], "stopped", "{stopped}");
    wait_gone(
        applied["ok"]["agent_pid"]
            .as_u64()
            .unwrap()
            .try_into()
            .unwrap(),
    );
    wait_gone(running.service_pid.unwrap());
}
