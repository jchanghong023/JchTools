use super::*;

/// 覆盖 AH-13/AH-A10：allow-always优先于allow-once，且无允许选项必须明确失败。
#[test]
fn permission_uses_broadest_allow_and_never_fabricates_approval() {
    let server = server();
    let permission = Plan::new("permission");
    let response = server.post(&permission);
    assert_eq!(response.status, 200);
    let value = response.json();
    let outcome: Value = serde_json::from_str(completion(&value)).unwrap();
    assert_eq!(outcome["outcome"]["outcome"], "selected");
    assert_eq!(outcome["outcome"]["optionId"], "always");
    let denied = server.post(&Plan::new("permission-denied"));
    assert_eq!(denied.status, 502);
    let value = denied.json();
    assert!(value.get("error").is_some());
    assert!(value.get("choices").is_none());
}

/// 覆盖 AH-02/AH-05/AH-13/AH-A11：被宣告的文件能力确实写盘、按行读取并明确返回不存在失败。
#[test]
fn file_callbacks_write_actual_bytes_and_read_only_requested_line() {
    let server = server();
    let plan = Plan::new("files");
    let response = server.post(&plan);
    assert_eq!(response.status, 200);
    let value = response.json();
    assert_eq!(completion(&value).trim_end_matches('\n'), plan.tag);
    assert_eq!(
        std::fs::read_to_string(server.workspace.join(format!("{}.txt", plan.tag))).unwrap(),
        format!("first\n{}\nlast\n", plan.tag)
    );
    let initialization = audit(&server.root)
        .into_iter()
        .find(|event| event["event"] == "initialize")
        .unwrap();
    assert_eq!(initialization["capabilities"]["fs"]["readTextFile"], true);
    assert_eq!(initialization["capabilities"]["fs"]["writeTextFile"], true);
    let missing = server.post(&Plan::new("file-missing"));
    assert_eq!(missing.status, 502);
    assert!(missing.json().get("error").is_some());
}

/// AH-04/AH-06：可访问的 verbatim 尾点/尾空格目录不能因去前缀被误判为不存在。
#[test]
fn review_f45_existing_verbatim_cwd_is_not_rejected() {
    let server = server();
    let root = server.temp.path().canonicalize().unwrap();
    for name in ["caller-dot.", "caller-space "] {
        let caller = root.join(name);
        std::fs::create_dir(&caller).unwrap();
        assert!(caller.is_dir(), "夹具必须先创建真实可访问的特殊目录");
        let plan = Plan::new("files");
        let request = body(&plan, false, &caller);
        let response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&request),
            None,
        );
        assert_eq!(
            response.status, 200,
            "AH-04：已存在的 verbatim cwd 必须可用：{caller:?}"
        );
        assert_eq!(
            completion(&response.json()).trim_end_matches('\n'),
            plan.tag
        );
        assert_eq!(
            std::fs::read_to_string(caller.join(format!("{}.txt", plan.tag))).unwrap(),
            format!("first\n{}\nlast\n", plan.tag)
        );
    }
}

/// AH-04/AH-06/AH-13：普通同名目录存在时，去 verbatim 前缀也不能把文件回调重定向过去。
#[test]
fn review_f45_verbatim_cwd_never_redirects_file_callbacks() {
    let server = server();
    let root = server.temp.path().canonicalize().unwrap();
    for (special_name, ordinary_name) in [
        ("caller-dot.", "caller-dot"),
        ("caller-space ", "caller-space"),
    ] {
        let caller = root.join(special_name);
        let ordinary = root.join(ordinary_name);
        std::fs::create_dir(&caller).unwrap();
        std::fs::create_dir(&ordinary).unwrap();
        assert_ne!(
            caller.canonicalize().unwrap(),
            ordinary.canonicalize().unwrap()
        );
        let plan = Plan::new("files");
        let filename = format!("{}.txt", plan.tag);
        let request = body(&plan, false, &caller);
        let response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&request),
            None,
        );
        assert_eq!(response.status, 200);
        assert_eq!(
            completion(&response.json()).trim_end_matches('\n'),
            plan.tag
        );
        assert!(
            !ordinary.join(&filename).exists(),
            "AH-04：请求 cwd 不得被重定向到无尾点/尾空格的其他目录"
        );
        assert_eq!(
            std::fs::read_to_string(caller.join(filename)).unwrap(),
            format!("first\n{}\nlast\n", plan.tag)
        );
    }
}

fn review_f46_callback_logs(test_name: &str, action: &str, operations: &[&str]) {
    const CHILD: &str = "JCHTOOLS_REVIEW_F46_LOG_CHILD";
    const FILE_TAG: &str = "review-f46-file";
    const PRIVATE_FILE_LINE: &str = "review-f46-body-only-7d2b94e1";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        assert!(std::env::var_os("JCH_LOG_LEVEL").is_none());
        let guard = jchtools_foundation::logging::init(&root).unwrap();
        {
            let server = server();
            let mut plan = Plan::new(action);
            plan.tag = FILE_TAG.into();
            if action == "files-from-chunks" {
                plan.chunks = vec![format!("first\n{PRIVATE_FILE_LINE}\nlast\n")];
            }
            let response = server.post(&plan);
            assert_eq!(response.status, 200);
            let value = response.json();
            if action == "files-from-chunks" {
                let read_line = completion(&value);
                assert_eq!(read_line.trim_end_matches('\n'), PRIVATE_FILE_LINE);
                let path = server.workspace.join(format!("{FILE_TAG}.txt"));
                assert!(
                    !path.to_string_lossy().contains(PRIVATE_FILE_LINE),
                    "隐私 marker 必须只在正文中，不能来自允许的文件路径元数据"
                );
                let written_body = std::fs::read_to_string(path).unwrap();
                assert_eq!(written_body, format!("first\n{PRIVATE_FILE_LINE}\nlast\n"));
                // 两个实际 SDK 结果都携带同一独占 ASCII marker；
                // 记录写入全文或仅读取单行，下面的隐私断言都会命中。
                assert!(written_body.contains(PRIVATE_FILE_LINE));
                assert!(read_line.contains(PRIVATE_FILE_LINE));
            } else {
                let outcome: Value = serde_json::from_str(completion(&value)).unwrap();
                assert_eq!(outcome["outcome"]["outcome"], "selected");
                assert_eq!(outcome["outcome"]["optionId"], "always");
            }
            server.shutdown();
        }
        // 真实日志 guard 收尾后再由父进程读盘，不依赖 sleep 或异步写盘时序。
        drop(guard);
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, temp.path())
        .env("JCHTOOLS_TEST_STATE_DIR", temp.path())
        .env_remove("JCH_LOG_LEVEL")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let logs = std::fs::read_dir(temp.path().join("logs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(logs
        .lines()
        .any(|line| { line.contains("event=application_started") && line.contains("level=INFO") }));
    // 不依赖 LF 的落盘转义层数：safe_error/quoted 及 Debug 编码均保留 ASCII marker。
    // marker 不在文件名或请求路径中；写入全文、读取单行或请求正文泄露均不得出现它。
    for private in [PRIVATE_FILE_LINE, "第一段 α", "第二段 β"] {
        assert!(
            !logs.contains(private),
            "P-10：默认日志不得泄露文件或请求正文"
        );
    }
    let mut missing = Vec::new();
    for operation in operations {
        let events = logs
            .lines()
            .filter(|line| {
                line.contains("component=acp_callback")
                    && line.contains(&format!("operation=\"{operation}\""))
            })
            .collect::<Vec<_>>();
        let started = events
            .iter()
            .filter(|line| line.contains("event=acp_call_started") && line.contains("level=INFO"));
        if started.count() != 1 {
            missing.push(format!("{operation}: INFO 开始事件"));
        }
        let completed = events.iter().filter(|line| {
            line.contains("event=acp_call_completed")
                && line.contains("level=INFO")
                && line.contains("status=\"success\"")
                && line.split_whitespace().any(|field| {
                    field
                        .strip_prefix("elapsed_ms=")
                        .is_some_and(|elapsed| elapsed.parse::<u64>().is_ok())
                })
        });
        if completed.count() != 1 {
            missing.push(format!("{operation}: INFO 成功结果及数值耗时"));
        }
    }
    if action == "permission"
        && !logs.lines().any(|line| {
            line.contains("component=acp_callback")
                && line.contains("event=acp_permission_completed")
                && line.contains("level=INFO")
                && line.contains("outcome=\"selected_allow\"")
                && line.contains("permission_kind=")
        })
    {
        missing.push("permission: INFO 允许结果及授权类别".into());
    }
    assert!(
        missing.is_empty(),
        "P-10：默认日志缺少 {}",
        missing.join("；")
    );
}

/// P-10/AH-02/AH-13：真实 SDK 文件回调成功后，默认 INFO 文件须含开始、结果和耗时且不含正文。
#[test]
fn review_f46_file_callbacks_log_default_info_lifecycle_without_bodies() {
    review_f46_callback_logs(
        "permissions_files::review_f46_file_callbacks_log_default_info_lifecycle_without_bodies",
        "files-from-chunks",
        &["write", "read"],
    );
}

/// P-10/AH-13：真实自动允许回调的开始、授权类别、结果和耗时不能仅留在 DEBUG。
#[test]
fn review_f46_permission_callback_logs_default_info_lifecycle_without_bodies() {
    review_f46_callback_logs(
        "permissions_files::review_f46_permission_callback_logs_default_info_lifecycle_without_bodies",
        "permission",
        &["permission"],
    );
}

/// AH-04/AH-06/AH-12：并发业务会话各自使用请求cwd，session/new及ACP文件回调均隔离到调用方目录。
#[test]
fn concurrent_sessions_write_files_inside_their_caller_directories() {
    let server = server();
    let first_dir = server.temp.path().join("caller-first");
    let second_dir = server.temp.path().join("caller-second");
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&second_dir).unwrap();
    let mut first = Plan::new("files");
    first.barrier = true;
    let mut second = Plan::new("files");
    second.barrier = true;
    let port = server.port;
    let first_body = body(&first, false, &first_dir);
    let second_body = body(&second, false, &second_dir);
    let first_request = thread::spawn(move || {
        Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&first_body),
            None,
        )
    });
    let second_request = thread::spawn(move || {
        Http::open(
            port,
            "POST",
            "/v1/chat/completions",
            Some(&second_body),
            None,
        )
    });
    let first_start = wait_event(&server.root, "started", &first.tag);
    let second_start = wait_event(&server.root, "started", &second.tag);
    assert_ne!(first_start["session"], second_start["session"]);
    let pid = first_start["pid"].as_u64().expect("Agent 必须报告实际 PID");
    assert_eq!(second_start["pid"].as_u64(), Some(pid));
    release(&server.root, &first);
    release(&server.root, &second);
    let first_response = first_request.join().unwrap();
    let second_response = second_request.join().unwrap();
    assert_eq!(first_response.status, 200);
    assert_eq!(second_response.status, 200);
    assert_eq!(
        completion(&first_response.json()).trim_end_matches('\n'),
        first.tag
    );
    assert_eq!(
        completion(&second_response.json()).trim_end_matches('\n'),
        second.tag
    );
    assert_eq!(
        std::fs::read_to_string(first_dir.join(format!("{}.txt", first.tag))).unwrap(),
        format!("first\n{}\nlast\n", first.tag)
    );
    assert_eq!(
        std::fs::read_to_string(second_dir.join(format!("{}.txt", second.tag))).unwrap(),
        format!("first\n{}\nlast\n", second.tag)
    );
    assert!(!first_dir.join(format!("{}.txt", second.tag)).exists());
    assert!(!second_dir.join(format!("{}.txt", first.tag)).exists());
    let events = audit(&server.root);
    let spawns = events
        .iter()
        .filter(|event| event["event"] == "spawn")
        .collect::<Vec<_>>();
    assert_eq!(spawns.len(), 1, "并发会话必须复用唯一 Agent 进程");
    assert_eq!(spawns[0]["pid"].as_u64(), Some(pid));

    let expected = [
        first_dir.canonicalize().unwrap(),
        second_dir.canonicalize().unwrap(),
    ];
    let mut actual = events
        .iter()
        .filter(|event| event["event"] == "session")
        .filter_map(|event| {
            let cwd = PathBuf::from(event["cwd"].as_str()?);
            let canonical = cwd.canonicalize().ok()?;
            if !expected.contains(&canonical) {
                return None;
            }
            #[cfg(windows)]
            assert!(
                !cwd.to_string_lossy().starts_with(r"\\?\"),
                "session/new cwd must use an ordinary Windows path"
            );
            Some(canonical)
        })
        .collect::<Vec<_>>();
    actual.sort();
    let mut expected = expected.to_vec();
    expected.sort();
    assert_eq!(actual, expected, "session/new must receive each caller cwd");
}

/// AH-04/AH-06/AH-13：终端省略cwd继承会话目录，显式cwd覆盖会话目录。
#[test]
fn terminal_current_directory_matches_session_with_and_without_explicit_cwd() {
    let server = server();
    for (action, dirname) in [
        ("terminal-pwd", "终端-默认调用方"),
        ("terminal-pwd-explicit", "终端-显式调用方"),
    ] {
        let caller_dir = server.temp.path().join(dirname);
        std::fs::create_dir(&caller_dir).unwrap();
        let plan = Plan::new(action);
        let terminal_dir = if action == "terminal-pwd-explicit" {
            caller_dir.with_file_name("终端-显式指定目录")
        } else {
            caller_dir.clone()
        };
        if terminal_dir != caller_dir {
            std::fs::create_dir(&terminal_dir).unwrap();
        }
        let request = body(&plan, false, &caller_dir);
        let response = Http::open(
            server.port,
            "POST",
            "/v1/chat/completions",
            Some(&request),
            None,
        );
        assert_eq!(response.status, 200);
        let actual =
            std::fs::read_to_string(terminal_dir.join(format!("terminal-{}.cwd", plan.tag)))
                .unwrap();
        assert_eq!(
            PathBuf::from(actual).canonicalize().unwrap(),
            terminal_dir.canonicalize().unwrap(),
            "action={action}"
        );
        if terminal_dir != caller_dir {
            assert!(!caller_dir
                .join(format!("terminal-{}.cwd", plan.tag))
                .exists());
        }
    }
}

/// 覆盖 AH-13：非权限交互不得伪造答案、无限等待或被转换成HTTP工具循环。
#[test]
fn non_permission_interaction_fails_explicitly() {
    let server = server();
    let response = server.post(&Plan::new("elicitation"));
    assert_eq!(response.status, 502);
    let value = response.json();
    assert!(value.get("error").is_some());
    assert!(value.get("choices").is_none());
}
