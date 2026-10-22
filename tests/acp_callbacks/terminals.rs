use super::*;

/// 覆盖 AH-13/AH-A11：同一个实际终端的stdout/stderr、非零退出、重复累计查询及release。
#[test]
fn terminal_callbacks_capture_wait_and_release_the_same_child() {
    let server = server();
    let plan = Plan::new("terminal");
    let response = server.post(&plan);
    assert_eq!(response.status, 200);
    let value = response.json();
    let terminal: Value = serde_json::from_str(completion(&value)).unwrap();
    assert_eq!(terminal["wait"]["exitCode"], 7);
    let output = terminal["first"]["output"].as_str().unwrap();
    assert!(
        output.contains(&format!("stdout:{}", plan.tag)),
        "{terminal}"
    );
    assert!(
        output.contains(&format!("stderr:{}", plan.tag)),
        "{terminal}"
    );
    assert_eq!(
        terminal["first"], terminal["second"],
        "output不得重启命令或消耗累计输出"
    );
    assert_eq!(terminal["released_error"], true);
    let pid = std::fs::read_to_string(server.workspace.join(format!("terminal-{}.pid", plan.tag)))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(!process_alive(pid), "已wait/release的终端进程仍在运行");
    let initialization = audit(&server.root)
        .into_iter()
        .find(|event| event["event"] == "initialize")
        .unwrap();
    assert_eq!(initialization["capabilities"]["terminal"], true);
}

// P3-20/AH-13/AH-A11：产物来自真实 terminal/create → 子进程 → SDK output/wait/release。
fn review_remediation_terminal_limit_snapshot(action: &str) -> (Value, String) {
    let server = server();
    let plan = Plan::new(action);
    let expected = format!("{}尾部:{}:完成🙂\n", "A中🙂β\n".repeat(150_000), plan.tag);
    assert!(expected.len() > 1024 * 1024);
    let response = server.post(&plan);
    assert_eq!(response.status, 200);
    let value = response.json();
    let terminal: Value = serde_json::from_str(completion(&value)).unwrap();
    assert_eq!(terminal["wait"]["exitCode"], 7);
    assert_eq!(
        terminal["first"], terminal["second"],
        "累计输出不得被查询消费"
    );
    assert_eq!(
        terminal["released_error"], true,
        "release 后 handle 必须失效"
    );
    let pid = std::fs::read_to_string(server.workspace.join(format!("terminal-{}.pid", plan.tag)))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(
        !process_alive(pid),
        "预算策略不得破坏 wait/release 的实际进程回收"
    );
    (terminal, expected)
}

/// P3-20/AH-13：省略 limit 使用本客户端的末尾 1MiB 默认，不是 ACP 协议默认。
#[test]
fn review_remediation_terminal_omitted_limit_retains_utf8_tail_within_one_mib() {
    let (terminal, expected) = review_remediation_terminal_limit_snapshot("terminal-limit-default");
    let output = terminal["first"]["output"].as_str().unwrap();
    assert!(
        output.len() <= 1024 * 1024,
        "省略 limit 的输出无界：{} bytes",
        output.len()
    );
    assert!(
        output.len() >= 1024 * 1024 - 3,
        "默认预算应保留可容纳的完整 UTF-8 尾部"
    );
    assert_eq!(terminal["first"]["truncated"], true);
    assert!(
        expected.ends_with(output),
        "输出必须是原始混合 UTF-8 的连续末尾"
    );
    assert!(output.ends_with("完成🙂\n"));
    assert!(output.contains('中') && output.contains('β') && output.contains('🙂'));
}

/// P3-20/AH-13：显式小限额仍按字节保留尾部，显式大限额不得被默认值覆盖。
#[test]
fn review_remediation_terminal_explicit_limits_preserve_caller_budgets() {
    let (small, expected) = review_remediation_terminal_limit_snapshot("terminal-limit-small");
    let output = small["first"]["output"].as_str().unwrap();
    assert!(output.len() <= 137 && output.len() >= 134);
    assert!(expected.ends_with(output));
    assert!(output.ends_with("完成🙂\n"));
    assert_eq!(small["first"]["truncated"], true);

    let (large, expected) = review_remediation_terminal_limit_snapshot("terminal-limit-large");
    assert_eq!(large["first"]["output"].as_str().unwrap(), expected);
    assert_eq!(large["first"]["truncated"], false);
}

/// P3-20/AH-13：显式零仍丢弃全部输出并报告截断，终端等待与释放正常完成。
#[test]
fn review_remediation_terminal_zero_limit_discards_output_without_breaking_lifecycle() {
    let (terminal, _) = review_remediation_terminal_limit_snapshot("terminal-limit-zero");
    assert_eq!(terminal["first"]["output"], "");
    assert_eq!(terminal["first"]["truncated"], true);
}

/// 覆盖 AH-13/AH-A11：kill只结束目标终端，后续模型请求仍使用唯一Agent。
#[test]
fn terminal_kill_reaps_child_without_terminating_agent() {
    let server = server();
    let plan = Plan::new("terminal-kill");
    // kill 回调或断言失败时也放行合成子进程，避免其永久等待影响 Server 清场。
    let _cleanup = ReleaseMarker::terminal(&server.workspace, &plan);
    let response = server.post(&plan);
    assert_eq!(response.status, 200);
    let value = response.json();
    let terminal: Value = serde_json::from_str(completion(&value)).unwrap();
    assert_eq!(terminal["released_error"], true);
    let pid = std::fs::read_to_string(server.workspace.join(format!("terminal-{}.pid", plan.tag)))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert!(!process_alive(pid));
    let next = Plan::new("text");
    assert_eq!(completion(&server.post(&next).json()), next.text());
    assert_eq!(
        wait_event(&server.root, "started", &plan.tag)["pid"],
        wait_event(&server.root, "started", &next.tag)["pid"]
    );
    assert_eq!(
        audit(&server.root)
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        1
    );
}

fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_FAILED, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
    };
    // SAFETY: 只以同步权限打开测试自己记录的子进程 PID；不修改或终止进程。
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // 不存在的 PID 可视为已退出；权限或其他查询失败不能冒充成功回收。
        let error = std::io::Error::last_os_error();
        assert_eq!(
            error.raw_os_error(),
            Some(ERROR_INVALID_PARAMETER as i32),
            "无法打开自有进程以观察退出: {error}"
        );
        return false;
    }
    // SAFETY: 有效进程句柄，零时等待只查询进程对象是否已完成退出。
    let waited = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: 查询完成，释放本次唯一拥有的Windows句柄。
    unsafe { CloseHandle(handle) };
    assert_ne!(waited, WAIT_FAILED, "无法观察自有进程的实际退出");
    waited == WAIT_TIMEOUT
}

fn held_terminal_failure_attempt(models: bool) -> (Option<HttpOutcome>, HttpOutcome, Value, Value) {
    let server = server();
    let held = Plan::new("terminal-held");
    let cleanup = ReleaseMarker::terminal(&server.workspace, &held);
    let response = server.post(&held);
    assert_eq!(response.status, 200);
    let terminal_key: String = serde_json::from_str(completion(&response.json())).unwrap();
    assert!(!terminal_key.is_empty());
    let old = wait_event(&server.root, "started", &held.tag);
    let terminal_pid_path = server.workspace.join(format!("terminal-{}.pid", held.tag));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !terminal_pid_path.exists() {
        assert!(Instant::now() < deadline, "held terminal 未启动");
        thread::sleep(Duration::from_millis(10));
    }
    let terminal_pid = std::fs::read_to_string(&terminal_pid_path)
        .unwrap()
        .parse()
        .unwrap();
    assert!(process_alive(terminal_pid));
    let response = server.post(&Plan::new("crash"));
    assert_eq!(response.status, 502);
    assert!(response.json().get("error").is_some());
    let agent_pid = u32::try_from(old["pid"].as_u64().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(agent_pid) {
        assert!(Instant::now() < deadline, "崩溃 Agent 未退出");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(process_alive(terminal_pid), "旧终端须仍未释放");
    let next = Plan::new("text");
    let mut pending = PendingHttp::start(
        cleanup,
        server.port,
        if models { "GET" } else { "POST" },
        if models {
            "/v1/models"
        } else {
            "/v1/chat/completions"
        },
        if models {
            None
        } else {
            Some(body(&next, false, &server.workspace))
        },
    );
    // 等待响应只设防挂看门狗，不把调度速度当 SLA；放行前的存活和 marker
    // 观察证明响应/恢复没有依赖旧 terminal 自然退出。
    let before_release = match pending.receive.recv_timeout(Duration::from_secs(10)) {
        Ok(response) => Some(response),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("HTTP 请求线程未返回结果"),
    };
    assert!(
        process_alive(terminal_pid),
        "响应观察点旧 terminal 必须仍存活"
    );
    assert!(
        !pending.release.0.exists(),
        "响应观察点旧 terminal 不得提前放行"
    );
    let eventual = pending.finish(before_release.clone());
    let recovered = if models {
        let recovery = Plan::new("text");
        let response = server.post(&recovery);
        assert_eq!(response.status, 200);
        assert_eq!(completion(&response.json()), recovery.text());
        wait_event(&server.root, "started", &recovery.tag)
    } else {
        assert_eq!(eventual.0, 200);
        assert_eq!(completion(&eventual.1), next.text());
        wait_event(&server.root, "started", &next.tag)
    };
    let followup = Plan::new("text");
    let response = server.post(&followup);
    assert_eq!(response.status, 200);
    assert_eq!(completion(&response.json()), followup.text());
    assert_eq!(
        wait_event(&server.root, "started", &followup.tag)["pid"],
        recovered["pid"],
        "恢复后的后续请求必须复用同一个 Agent"
    );
    let events = audit(&server.root);
    for pid in [&old["pid"], &recovered["pid"]] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event["event"] == "spawn" && &event["pid"] == pid)
                .count(),
            1,
            "每个生命周期只能 spawn 一次 Agent"
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "spawn")
            .count(),
        2
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(terminal_pid) {
        assert!(Instant::now() < deadline, "released terminal 未回收");
        thread::sleep(Duration::from_millis(10));
    }
    (before_release, eventual, old, recovered)
}

/// AH-13：死 Agent 的旧 terminal 自然收尾不得阻塞 /models 明确不可用响应。
#[test]
fn unreleased_terminal_after_agent_exit_does_not_block_models_response() {
    let (before_release, eventual, old, recovered) = held_terminal_failure_attempt(true);
    let response = before_release.expect("旧 terminal 未释放时 /models 命令被阻塞");
    assert_eq!(response.0, 502, "{eventual:?}");
    // 断裂 Agent 的原始错误须明确回传，不把它改成泛化的未就绪或成功列表。
    assert_eq!(
        response.1["error"]["code"], "agent_disconnected",
        "{eventual:?}"
    );
    assert!(response.1.get("error").is_some());
    assert_ne!(old["pid"], recovered["pid"]);
}

/// AH-13/AH-12：Agent 已退出，旧 terminal 仍在时新会话可恢复唯一 Agent。
#[test]
fn unreleased_terminal_after_agent_exit_does_not_block_new_request_recovery() {
    let (before_release, eventual, old, recovered) = held_terminal_failure_attempt(false);
    let response = before_release.expect("新 HTTP 请求被旧 terminal shutdown 永久阻塞");
    assert_eq!(response.0, 200, "{eventual:?}");
    assert_eq!(response.1["object"], "chat.completion");
    assert_ne!(old["pid"], recovered["pid"]);
}
