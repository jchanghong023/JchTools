//! AH-08/AH-09/AH-A01：Windows参数、端口及独立进程SQLite恢复；不启动真实模型。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::acp_api::{
    settings, ServiceConfig, ServiceErrorKind, ServicePhase, ServiceStatus, DEFAULT_PORT,
};
use std::process::Command;

/// 覆盖 AH-08：独立argv保持空参数、中文、空格、引号及引号前/末尾反斜杠。
#[test]
fn windows_arguments_round_trip_without_space_splitting() {
    let cases = vec![
        vec!["--model", "中文 模型", "", "--thinking", "low"],
        vec![
            r"C:\中文 路径\",
            r#"literal"quote"#,
            "one\ttwo",
            r#"slashes\\"quoted"#,
        ],
        vec!["simple", "two words", "three\twords", "尾巴\\"],
    ];
    for case in cases {
        let argv = case.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(
            settings::parse_arguments(&settings::format_arguments(&argv)).unwrap(),
            argv
        );
    }
    assert_eq!(
        settings::parse_arguments(r#"--model "中文 模型" "" --thinking low"#).unwrap(),
        vec!["--model", "中文 模型", "", "--thinking", "low"]
    );
    assert_eq!(
        settings::parse_arguments("a\tb  c").unwrap(),
        vec!["a", "b", "c"]
    );
    assert!(settings::parse_arguments("").unwrap().is_empty());
}

/// 覆盖 AH-08：默认8765、全部合法边界和非法端口明确拒绝，不钳制、不换端口。
#[test]
fn port_validation_has_exact_integer_boundaries() {
    assert_eq!(ServiceConfig::default().port, 8765);
    assert_eq!(DEFAULT_PORT, 8765);
    for (text, port) in [("1", 1), ("8765", 8765), ("65535", 65535)] {
        assert_eq!(settings::parse_port(text).unwrap(), port);
    }
    for invalid in [
        "",
        "0",
        "65536",
        "-1",
        "1.5",
        "1e3",
        "NaN",
        "port",
        "18446744073709551616",
    ] {
        assert!(
            settings::parse_port(invalid).is_err(),
            "非法端口被接受: {invalid}"
        );
    }
    assert!(settings::validate_config(&ServiceConfig::default()).is_err());
}

/// 覆盖 AH-08/AH-09/AH-06/AH-A01：写进程完全退出，新进程恢复精确程序/argv/port并验证SQLite格式。
#[test]
fn configuration_survives_new_process_and_failed_save_preserves_previous_value() {
    const CHILD: &str = "JCHTOOLS_ACP_SETTINGS_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        let expected = ServiceConfig {
            executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
            arguments: vec![
                "--agent".into(),
                root.join("中文 空格").to_string_lossy().into_owned(),
                String::new(),
                r#"引号"与尾斜杠\"#.into(),
            ],
            port: 19317,
        };
        match std::env::var("JCHTOOLS_ACP_SETTINGS_ACTION")
            .unwrap()
            .as_str()
        {
            "save" => {
                assert_eq!(settings::load_config().unwrap(), None);
                settings::save_config(&expected).unwrap();
                let mut invalid = expected.clone();
                invalid.port = 0;
                assert!(settings::save_config(&invalid).is_err());
                assert_eq!(settings::load_config().unwrap(), Some(expected));
            }
            "read" => {
                assert_eq!(settings::load_config().unwrap(), Some(expected));
                let workspace = settings::workspace_dir().unwrap();
                assert_eq!(
                    workspace.canonicalize().unwrap(),
                    root.join("acp-workspace").canonicalize().unwrap()
                );
                assert!(workspace.is_absolute());
                assert!(workspace.is_dir());
            }
            action => panic!("未知action {action}"),
        }
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    for action in ["save", "read"] {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "configuration_survives_new_process_and_failed_save_preserves_previous_value",
                "--nocapture",
            ])
            .env(CHILD, temp.path())
            .env("JCHTOOLS_ACP_SETTINGS_ACTION", action)
            .env("JCHTOOLS_TEST_STATE_DIR", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{action}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let bytes = std::fs::read(temp.path().join("config.sqlite3")).unwrap();
    assert!(bytes.starts_with(b"SQLite format 3\0"));
}

/// 覆盖 AH-06/AH-08：工作目录供 Windows ACP Agent 使用，原生会话目录编码仍可实际创建。
#[test]
fn workspace_supports_native_agent_session_directory_creation() {
    const CHILD: &str = "JCHTOOLS_ACP_NATIVE_WORKSPACE_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        let workspace = settings::workspace_dir().unwrap();
        assert_eq!(
            workspace.canonicalize().unwrap(),
            root.join("acp-workspace").canonicalize().unwrap()
        );
        // OMP session-paths.ts 的 absolute 编码：去一个开头分隔符，再替换分隔符和冒号。
        // namespace '?' 不会被编码，修复前真实 mkdir 失败，与已观察的 session/new ENOENT 相同。
        let cwd = workspace.to_str().unwrap();
        let cwd = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
        let encoded: String = cwd
            .chars()
            .map(|ch| {
                if matches!(ch, '/' | '\\' | ':') {
                    '-'
                } else {
                    ch
                }
            })
            .collect();
        let sessions = root.join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let session = sessions.join(format!("--{encoded}--"));
        std::fs::create_dir(&session).expect("ACP 工作目录必须支持真实原生 Agent 会话目录创建");
        std::fs::write(session.join("state"), b"native-session").unwrap();
        assert_eq!(
            std::fs::read(session.join("state")).unwrap(),
            b"native-session"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("中文 空格");
    std::fs::create_dir(&root).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "workspace_supports_native_agent_session_directory_creation",
            "--nocapture",
        ])
        .env(CHILD, &root)
        .env("JCHTOOLS_TEST_STATE_DIR", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 覆盖 AH-09：SQLite写入失败不得冒称保存成功或清空已有持久配置。
#[test]
fn database_write_failure_is_explicit_and_previous_file_is_not_destroyed() {
    const CHILD: &str = "JCHTOOLS_ACP_SETTINGS_FAILURE_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = std::path::PathBuf::from(root);
        let config = ServiceConfig {
            executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
            arguments: vec!["--agent".into(), "合成参数".into()],
            port: 19318,
        };
        settings::save_config(&config).unwrap();
        let db = root.join("config.sqlite3");
        let before = std::fs::read(&db).unwrap();
        // 持有真实SQLite写事务，另一连接必须报告保存失败而非伪成功。
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let mut changed = config.clone();
        changed.port = 19319;
        assert!(settings::save_config(&changed).is_err());
        connection.execute_batch("ROLLBACK;").unwrap();
        assert_eq!(settings::load_config().unwrap(), Some(config));
        assert_eq!(std::fs::read(db).unwrap(), before);
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "database_write_failure_is_explicit_and_previous_file_is_not_destroyed",
            "--nocapture",
        ])
        .env(CHILD, temp.path())
        .env("JCHTOOLS_TEST_STATE_DIR", temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 覆盖 AH-08/AH-09：单个 ASCII 参数超过控制协议整帧容量时明确拒绝，并保留已保存配置。
#[test]
fn oversized_ascii_argument_is_rejected_without_replacing_saved_configuration() {
    const CHILD: &str = "JCHTOOLS_ACP_OVERSIZED_ASCII_SETTINGS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let expected = ServiceConfig {
            executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
            arguments: vec!["--agent".into(), r#"中文 空格 "引号" 与尾斜杠\"#.into()],
            port: 19320,
        };
        settings::save_config(&expected).unwrap();
        assert_eq!(settings::load_config().unwrap(), Some(expected.clone()));
        let mut oversized = expected.clone();
        oversized.arguments = vec!["a".repeat(1024 * 1024 + 1)];
        let validation = settings::validate_config(&oversized);
        let save = settings::save_config(&oversized);
        let loaded = settings::load_config().unwrap();
        for (operation, result) in [("校验", validation), ("保存", save)] {
            let error = result.expect_err("超过整帧容量的参数必须被拒绝");
            assert_eq!(error.kind, ServiceErrorKind::InvalidConfig, "{operation}");
            assert!(
                !error.message.trim().is_empty(),
                "{operation}必须说明失败原因"
            );
        }
        assert!(
            loaded.as_ref() == Some(&expected),
            "超限保存失败必须保留原启动程序、参数和端口"
        );
        // 单份配置尚未超过整帧，也必须为 saved/running 双份状态预留空间。
        let mut over_budget = expected.clone();
        over_budget.arguments = vec!["a".repeat(1024 * 1024 / 4)];
        let encoded_size = serde_json::to_vec(&over_budget).unwrap().len();
        assert!(encoded_size > 1024 * 1024 / 4 && encoded_size < 1024 * 1024);
        assert_eq!(
            settings::save_config(&over_budget).unwrap_err().kind,
            ServiceErrorKind::InvalidConfig
        );
        assert_eq!(settings::load_config().unwrap(), Some(expected));
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "oversized_ascii_argument_is_rejected_without_replacing_saved_configuration",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("JCHTOOLS_TEST_STATE_DIR", temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 覆盖 AH-08/AH-09：按 JSON 转义后的协议容量拒绝超限参数，正常中文及引号仍能精确往返。
#[test]
fn json_expanding_argument_is_rejected_without_replacing_saved_configuration() {
    const CHILD: &str = "JCHTOOLS_ACP_JSON_EXPANDING_SETTINGS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let expected = ServiceConfig {
            executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
            arguments: vec![
                "--agent".into(),
                r#"中文 "引号" 与反斜杠\"#.repeat(2048),
                String::new(),
            ],
            port: 19321,
        };
        settings::validate_config(&expected).unwrap();
        settings::save_config(&expected).unwrap();
        assert_eq!(settings::load_config().unwrap(), Some(expected.clone()));
        let mut oversized = expected.clone();
        // 非 NUL 控制字符在 JSON 中扩为六字节；原参数不足四分之一帧，编码后却超过整帧。
        oversized.arguments = vec!["\u{0001}".repeat(192 * 1024)];
        assert!(oversized.arguments[0].len() < 1024 * 1024 / 4);
        assert!(serde_json::to_vec(&oversized).unwrap().len() > 1024 * 1024);
        let validation = settings::validate_config(&oversized);
        let save = settings::save_config(&oversized);
        let loaded = settings::load_config().unwrap();
        for (operation, result) in [("校验", validation), ("保存", save)] {
            let error = result.expect_err("JSON 转义后超过整帧容量的参数必须被拒绝");
            assert_eq!(error.kind, ServiceErrorKind::InvalidConfig, "{operation}");
            assert!(
                !error.message.trim().is_empty(),
                "{operation}必须说明失败原因"
            );
        }
        assert!(
            loaded.as_ref() == Some(&expected),
            "JSON 转义超限保存失败必须保留原配置"
        );
        // 原字节很小、编码在整帧内但超过配置预算，同样不得替换持久值。
        let mut over_budget = expected.clone();
        over_budget.arguments = vec!["\u{0001}".repeat(44 * 1024)];
        let encoded_size = serde_json::to_vec(&over_budget).unwrap().len();
        assert!(encoded_size > 1024 * 1024 / 4 && encoded_size < 1024 * 1024);
        assert_eq!(
            settings::save_config(&over_budget).unwrap_err().kind,
            ServiceErrorKind::InvalidConfig
        );
        assert_eq!(settings::load_config().unwrap(), Some(expected));
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "json_expanding_argument_is_rejected_without_replacing_saved_configuration",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("JCHTOOLS_TEST_STATE_DIR", temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 覆盖 AH-08/AH-09：初始化失败但后台仍在时，保存的有效配置必须允许显式应用并重启。
#[test]
fn failed_service_with_saved_configuration_exposes_pending_apply() {
    let saved = ServiceConfig {
        executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
        arguments: vec!["--agent".into(), r#"修正后 "中文参数""#.into()],
        port: 19322,
    };
    settings::validate_config(&saved).unwrap();
    let status = ServiceStatus {
        phase: ServicePhase::Error,
        saved_config: Some(saved),
        running_config: None,
        service_pid: Some(42),
        error: Some("ACP 初始化失败".into()),
        ..ServiceStatus::default()
    };
    assert!(
        status.pending_apply(),
        "存活的失败后台没有运行配置时仍须允许应用已保存的有效配置"
    );
}

/// 覆盖 AH-09/AH-10：没有后台或正在主动退出时，不提供会重新启动服务的待应用入口。
#[test]
fn inactive_or_exiting_service_does_not_expose_pending_apply() {
    let running = ServiceConfig {
        executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
        arguments: vec!["--agent".into()],
        port: 19323,
    };
    let mut saved = running.clone();
    saved.arguments.push("新参数".into());
    for phase in [
        ServicePhase::Unconfigured,
        ServicePhase::Stopped,
        ServicePhase::Draining,
        ServicePhase::Stopping,
    ] {
        for running_config in [None, Some(running.clone())] {
            let status = ServiceStatus {
                phase,
                saved_config: Some(saved.clone()),
                running_config,
                service_pid: Some(42),
                ..ServiceStatus::default()
            };
            assert!(
                !status.pending_apply(),
                "{phase:?}阶段不得露出应用并重启入口"
            );
        }
    }
    for phase in [ServicePhase::Ready, ServicePhase::Error] {
        let status = ServiceStatus {
            phase,
            saved_config: Some(saved.clone()),
            running_config: Some(running.clone()),
            service_pid: None,
            ..ServiceStatus::default()
        };
        assert!(
            !status.pending_apply(),
            "{phase:?}没有存活后台时不得露出应用并重启入口"
        );
    }
    let ready = ServiceStatus {
        phase: ServicePhase::Ready,
        saved_config: Some(saved),
        running_config: Some(running.clone()),
        service_pid: Some(42),
        ..ServiceStatus::default()
    };
    assert!(ready.pending_apply(), "存活且就绪的后台配置变更仍须待应用");
    let unchanged = ServiceStatus {
        saved_config: Some(running),
        ..ready
    };
    assert!(!unchanged.pending_apply(), "配置未改变时不应提供重启入口");
}

/// 覆盖 AH-08/AH-09：配置预算按实际 JSON 字节计算，边界允许，超出一字节拒绝，不限制参数数量。
#[test]
fn configuration_json_budget_accounts_for_utf8_escaping_and_array_overhead() {
    let budget = 1024 * 1024 / 4;
    let mut config = ServiceConfig {
        executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
        arguments: vec![String::new()],
        port: 19324,
    };
    let overhead = serde_json::to_vec(&config).unwrap().len();
    let capacity = budget - overhead;
    for (text, encoded_width) in [
        ("a", 1),
        ("界", 3),
        ("\"", 2),
        ("\\", 2),
        ("\u{0001}", 6),
        ("\n", 2),
    ] {
        config.arguments[0] = text.repeat(capacity / encoded_width);
        config.arguments[0].push_str(&"a".repeat(capacity % encoded_width));
        assert_eq!(serde_json::to_vec(&config).unwrap().len(), budget);
        settings::validate_config(&config).expect("预算边界内的合法参数不得被拒绝");
        config.arguments[0].push('a');
        assert_eq!(serde_json::to_vec(&config).unwrap().len(), budget + 1);
        assert_eq!(
            settings::validate_config(&config).unwrap_err().kind,
            ServiceErrorKind::InvalidConfig,
            "实际 JSON 超出预算时必须明确拒绝：{text:?}"
        );
    }
    config.arguments = vec![String::new(); 10_000];
    assert!(serde_json::to_vec(&config).unwrap().len() < budget);
    settings::validate_config(&config).expect("不得增加与编码预算无关的参数数量上限");
}

/// 覆盖 AH-09/AH-10：就绪、启动及失败态仅在有效保存配置可应用时提供入口。
#[test]
fn pending_apply_requires_valid_saved_configuration_and_applicable_service_phase() {
    let running = ServiceConfig {
        executable: env!("CARGO_BIN_EXE_jchtools-acp-fixture").into(),
        arguments: vec!["--agent".into()],
        port: 19325,
    };
    let mut changed = running.clone();
    changed.arguments.push("新参数".into());
    for phase in [
        ServicePhase::Ready,
        ServicePhase::Starting,
        ServicePhase::Error,
    ] {
        for running_config in [None, Some(running.clone()), Some(changed.clone())] {
            let status = ServiceStatus {
                phase,
                saved_config: Some(changed.clone()),
                running_config,
                service_pid: Some(42),
                ..ServiceStatus::default()
            };
            let expected = match status.running_config.as_ref() {
                Some(config) => config != &changed,
                None => phase == ServicePhase::Error,
            };
            assert_eq!(status.pending_apply(), expected, "{phase:?}: {status:?}");
            assert!(
                !ServiceStatus {
                    saved_config: None,
                    ..status.clone()
                }
                .pending_apply(),
                "没有保存配置时不得提供应用入口"
            );
            let mut invalid_port = changed.clone();
            invalid_port.port = 0;
            let mut invalid_arguments = changed.clone();
            invalid_arguments.arguments.push("\0".into());
            let mut oversized = changed.clone();
            oversized.arguments = vec!["a".repeat(1024 * 1024 / 4)];
            for invalid in [
                ServiceConfig::default(),
                invalid_port,
                invalid_arguments,
                oversized,
            ] {
                assert!(
                    !ServiceStatus {
                        saved_config: Some(invalid),
                        ..status.clone()
                    }
                    .pending_apply(),
                    "保存配置非法时不得提供应用入口"
                );
            }
        }
    }
}
