use super::*;

/// AH-17：真实内部 CLI 缺后台幂等成功，不读写 SQLite、不进入 serve 或启动 Agent。
#[test]
fn ah17_maintenance_absent_does_not_start_service_or_open_sqlite() {
    let harness = Harness::new();
    let database = harness.root.join("config.sqlite3");
    std::fs::create_dir(&database).unwrap();
    for _ in 0..2 {
        let mut stop = harness.maintenance();
        let status = wait_consumer(&mut stop, Duration::from_secs(3))
            .expect("AH-17: --stop 缺后台应立即成功，不能进入 serve");
        assert!(status.success(), "AH-17: 缺后台必须幂等成功");
        assert!(!harness.root.join("acp-workspace").exists());
        assert!(database.is_dir(), "维护退出不能创建/修复数据库");
        assert!(audit(&harness.audit).is_empty());
    }
}

/// AH-17：控制失败必须返回非零，不能把错误当作缺后台成功或启动普通 GUI。
#[test]
fn ah17_maintenance_control_failure_exits_nonzero() {
    let mut stop = ConsumerChild(
        Command::new(env!("CARGO_BIN_EXE_JchTools"))
            .args(["--acp-http-service", "--stop"])
            .env(
                "JCHTOOLS_TEST_STATE_DIR",
                "relative-invalid-maintenance-root",
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let status = wait_consumer(&mut stop, Duration::from_secs(3))
        .expect("AH-17: 控制命名空间失败必须退出，不能进入 serve");
    assert!(!status.success(), "AH-17: 停止失败必须非零");
}

/// AH-17：GUI Stop 已完成、管道消失后，后台仍可能卡在 main 的日志刷盘等退出尾段。
/// 维护必须等真实 OS process object signaled，不能把可取得 lifetime 锁当成已退出。
#[test]
fn ah17_maintenance_waits_for_os_exit_after_serve_returns() {
    if child_operation() {
        return;
    }
    let test = "maintenance::ah17_maintenance_waits_for_os_exit_after_serve_returns";
    let harness = Harness::new();
    let mut daemon = HeldServiceExit {
        harness: &harness,
        child: Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD_ROOT, &harness.root)
            .env("JCHTOOLS_ACP_PROCESS_ACTION", "serve-held-exit")
            .env("JCHTOOLS_TEST_STATE_DIR", &harness.root)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    };
    // 在任何 Stop 前保留测试自有后台的 OS 对象；不在退出后按裸 PID 重建观察身份。
    let service = ObservedProcess::open(daemon.child.id());
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let discovery = harness.call(test, "discover", None);
        if discovery["ok"].is_object() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "AH-17: 后台控制管道未就绪: {discovery}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(harness.call(test, "stop", None)["ok"]["phase"], "stopped");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !harness.root.join("service-exit-tail-entered").exists() {
        assert!(
            Instant::now() < deadline,
            "AH-17: 未进入 serve 返回后的退出尾段"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
    assert!(
        service.is_alive(),
        "AH-17: 尾段 barrier 必须保留原后台 OS 对象"
    );
    let mut stop = harness.maintenance();
    let premature = wait_consumer(&mut stop, Duration::from_secs(3));
    assert!(
        premature.is_none(),
        "AH-17: 原后台尚未 OS 退出，维护不能提前结束: {premature:?}"
    );
    assert!(service.is_alive(), "AH-17: 维护不得强杀尾段中的后台");
    std::fs::write(harness.root.join("release-service-exit-tail"), b"release").unwrap();
    let status = wait_consumer(&mut stop, Duration::from_secs(15))
        .expect("AH-17: 放行 OS 退出尾段后维护未完成");
    assert!(status.success(), "AH-17: 自然退出后的维护必须成功");
    assert!(
        !service.is_alive(),
        "AH-17: 维护成功必须晚于原后台 OS process object signaled"
    );
    assert!(wait_consumer(&mut daemon.child, Duration::from_secs(5))
        .expect("AH-17: 自有后台未自然退出")
        .success());
    assert!(
        audit(&harness.audit).is_empty(),
        "无配置的尾段回归不得启动 Agent"
    );
}

/// AH-17：真实维护命令取消在途请求，等待自有终端、Agent 和后台自然退出才释放镜像。
#[test]
fn ah17_maintenance_cancels_and_waits_for_owned_resources_before_image_release() {
    if child_operation() {
        return;
    }
    let test =
        "maintenance::ah17_maintenance_cancels_and_waits_for_owned_resources_before_image_release";
    let mut harness = Harness::new();
    harness.service_executable = harness.temp.path().join("maintenance-service.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_JchTools"), &harness.service_executable).unwrap();
    assert!(harness
        .call(test, "save", Some(&harness.config("barrier-held")))
        .get("error")
        .is_none());
    let ready = harness.ready(test);
    let service = ObservedProcess::open(ready.service_pid.unwrap());
    let agent = ObservedProcess::open(ready.agent_pid.unwrap());
    let terminal_plan = Plan::new("terminal-held");
    harness.register_hold(&terminal_plan);
    let workspace = harness.root.join("acp-workspace");
    let response = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&terminal_plan, false, &workspace)),
        None,
    );
    assert_eq!(response.status, 200);
    let _completed = response.json();
    wait_event(&harness.audit, "completed", &terminal_plan.tag);
    let pid_file = workspace.join(format!("terminal-{}.pid", terminal_plan.tag));
    let deadline = Instant::now() + Duration::from_secs(10);
    let terminal_pid = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            if let Ok(pid) = text.parse::<u32>() {
                break pid;
            }
        }
        assert!(Instant::now() < deadline, "AH-17: 自有终端未发布 PID");
        thread::sleep(Duration::from_millis(10));
    };
    let terminal = ObservedProcess::open(terminal_pid);
    let mut in_flight = Plan::new("text");
    in_flight.barrier = true;
    harness.register_hold(&in_flight);
    let mut stream = Http::open(
        harness.port,
        "POST",
        "/v1/chat/completions",
        Some(&body(&in_flight, true, &workspace)),
        None,
    );
    assert_eq!(stream.status, 200);
    assert!(!stream.delta().is_empty());
    wait_event(&harness.audit, "started", &in_flight.tag);
    let storage = rusqlite::Connection::open(harness.root.join("config.sqlite3")).unwrap();
    storage.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let mut stop = harness.maintenance();
    // 不调用读配置的 status：SQLite 被锁定时维护仍必须发出真实取消。
    let deadline = Instant::now() + Duration::from_secs(15);
    while !audit(&harness.audit)
        .iter()
        .any(|event| event["event"] == "cancel-received")
    {
        assert!(Instant::now() < deadline, "AH-17: 维护没有取消在途轮次");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        wait_consumer(&mut stop, Duration::from_millis(200)).is_none(),
        "AH-17: 存活终端未释放前不能只凭 ACK 退出"
    );
    assert!(terminal.is_alive() && agent.is_alive() && service.is_alive());
    assert!(std::fs::OpenOptions::new()
        .write(true)
        .open(&harness.service_executable)
        .is_err());
    std::fs::write(
        workspace.join(format!("terminal-release-{}", terminal_plan.tag)),
        b"release",
    )
    .unwrap();
    let status =
        wait_consumer(&mut stop, Duration::from_secs(20)).expect("AH-17: 终端放行后维护未安全完成");
    assert!(status.success(), "AH-17: 安全维护退出失败");
    assert!(
        !terminal.is_alive() && !agent.is_alive() && !service.is_alive(),
        "AH-17: 成功必须对应真实 OS 退出，而非仅 Stop 回执"
    );
    storage.execute_batch("ROLLBACK").unwrap();
    std::fs::write(&harness.service_executable, b"replacement").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_file(&harness.service_executable) {
            Ok(()) => break,
            Err(error) if error.raw_os_error() == Some(32) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("AH-17: 删除已替换的后台镜像失败：{error}"),
        }
    }
    let _finished = stream.sse();
    assert_eq!(harness.call(test, "discover", None), json!({"ok":null}));
}

/// 异常路径清理也通过真实产品stop；无配置时验证返回可分类结果，不启动服务。
#[test]
fn cleanup_isolated_service() {
    if child_operation() {
        return;
    }
    let harness = Harness::new();
    let result = harness.call("maintenance::cleanup_isolated_service", "status", None);
    assert_eq!(result["ok"]["phase"], "unconfigured");
    assert!(result["ok"]["service_pid"].is_null());
    assert!(result["ok"]["agent_pid"].is_null());
    assert!(audit(&harness.audit).is_empty(), "无有效配置不得启动Agent");
}
