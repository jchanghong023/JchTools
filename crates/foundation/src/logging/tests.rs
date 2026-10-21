#![allow(clippy::unwrap_used)]

use super::appender::*;
use super::lifecycle::*;
use super::sanitize::*;
use super::*;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

fn workspace() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join("scripts/make_tmp.py").is_file())
        .unwrap();
    let directory = root.join(".tmp/logging-tests");
    assert!(std::process::Command::new("python")
        .arg(root.join("scripts/make_tmp.py"))
        .args(["workspace", "--destination"])
        .arg(&directory)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
    tempfile::Builder::new()
        .prefix("diagnostics-")
        .tempdir_in(directory)
        .unwrap()
}

fn in_child(name: &str) -> bool {
    const CHILD: &str = "JCHTOOLS_LOGGING_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success(), "隔离进程中的日志行为断言必须通过");
    false
}

fn contents(directory: &Path) -> String {
    std::fs::read_dir(directory)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_file())
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
        .collect()
}

#[test]
fn failed_log_file_initialization_does_not_call_panic_hook() {
    if !in_child("logging::tests::failed_log_file_initialization_does_not_call_panic_hook") {
        return;
    }
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    drop(open_appender(&directory).unwrap());
    let path = std::fs::read_dir(&directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let panic_count = Arc::new(AtomicUsize::new(0));
    let hook_count = Arc::clone(&panic_count);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |_| {
        hook_count.fetch_add(1, Ordering::Relaxed);
    }));
    let result = std::panic::catch_unwind(|| init(temp.path()));
    std::panic::set_hook(previous);
    let guard = result.unwrap();
    assert!(guard.is_some(), "仅主目录失败时必须使用隔离备用目录");
    drop(guard);
    assert_eq!(panic_count.load(Ordering::Relaxed), 0);
    assert!(contents(&temp.path().join("logs-fallback"))
        .contains("event=diagnostic_directory_fallback"));
}

#[test]
fn both_log_directories_blocked_do_not_panic() {
    if !in_child("logging::tests::both_log_directories_blocked_do_not_panic") {
        return;
    }
    let temp = workspace();
    std::fs::write(temp.path().join(LOG_DIR), b"blocked").unwrap();
    std::fs::write(temp.path().join("logs-fallback"), b"blocked").unwrap();
    let panic_count = Arc::new(AtomicUsize::new(0));
    let hook_count = Arc::clone(&panic_count);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |_| {
        hook_count.fetch_add(1, Ordering::Relaxed);
    }));
    let result = std::panic::catch_unwind(|| init(temp.path()));
    std::panic::set_hook(previous);
    assert!(result.unwrap().is_none());
    assert_eq!(panic_count.load(Ordering::Relaxed), 0);
}

#[test]
fn panic_payload_is_not_written_to_diagnostic_log() {
    if !in_child("logging::tests::panic_payload_is_not_written_to_diagnostic_log") {
        return;
    }
    let temp = workspace();
    let previous = std::panic::take_hook();
    let guard = init(temp.path()).unwrap();
    let result = std::panic::catch_unwind(|| {
        std::panic::panic_any("synthetic OCR payload C:/synthetic/private-image.png");
    });
    std::panic::set_hook(previous);
    drop(guard);
    assert!(result.is_err());
    let captured = contents(&temp.path().join(LOG_DIR));
    assert!(captured.contains("进程 panic"));
    assert!(captured.contains("event=application_panicked"));
    assert!(!captured.contains("synthetic OCR payload"));
    assert!(!captured.contains("private-image.png"));
}

#[test]
fn warns_and_errors_reach_disk_file() {
    if !in_child("logging::tests::warns_and_errors_reach_disk_file") {
        return;
    }
    let temp = workspace();
    let guard = init(temp.path()).unwrap();
    tracing::warn!(event = "probe_warning", "p10-warn-marker");
    tracing::error!(event = "probe_error", path = "probe", "p10-error-marker");
    drop(guard);
    let found = contents(&temp.path().join(LOG_DIR));
    assert!(found.contains("p10-warn-marker") && found.contains("p10-error-marker"));
    assert!(found.contains("level=ERROR"));
}

// P-10：真实 tracing 的主 EXE target 必须在默认级别落盘，放行不能扩大到第三方正文。
#[test]
fn review_f35_main_binary_error_target_reaches_disk_without_credentials() {
    const CHILD: &str = "JCHTOOLS_REVIEW_F35_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "logging::tests::review_f35_main_binary_error_target_reaches_disk_without_credentials",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("JCH_LOG_LEVEL")
            .env("RUST_LOG", "off")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "独立进程必须保存主 EXE 的最终错误且不泄漏凭据"
        );
        return;
    }
    let temp = workspace();
    let guard = init(temp.path()).unwrap();
    tracing::info!(event = "review_f35_control", "默认日志初始化成功");
    tracing::error!(
        target: "JchTools",
        component = "xberg_broker",
        event = "review_f35_application_failed",
        stage = "serve",
        error_type = "broker_failure",
        exit_code = 1,
        password = "review-f35-private-password",
        endpoint = "https://private-user:review-f35-private-userinfo@example.invalid/broker?token=review-f35-private-query#review-f35-private-fragment",
        detail = "Authorization: Bearer review-f35-private-bearer",
        "主程序异常退出"
    );
    tracing::error!(
        target: "JchTools::background",
        event = "review_f35_nested_failure",
        exit_code = 1,
        "主程序子模块异常退出"
    );
    tracing::error!(
        target: "third_party_engine",
        event = "review_f35_foreign_body",
        "review-f35-private-ocr-body"
    );
    tracing::error!(
        target: "JchTools_dependency",
        event = "review_f35_lookalike_body",
        "review-f35-private-lookalike-body"
    );
    drop(guard);
    let captured = contents(&temp.path().join(LOG_DIR));
    assert!(
        captured.contains("event=review_f35_control"),
        "必须实际启用默认 INFO 诊断"
    );
    let failure = captured
        .lines()
        .find(|line| line.contains("event=review_f35_application_failed"))
        .expect("主 EXE target=JchTools 的 ERROR 不能在过滤器处静默丢失");
    assert!(failure.contains("level=ERROR") && failure.contains("exit_code=1"));
    assert!(
        failure.contains("stage=\"serve\"") && failure.contains("error_type=\"broker_failure\"")
    );
    assert!(failure.contains("https://example.invalid/broker"));
    assert_eq!(
        captured
            .lines()
            .filter(|line| line.contains("event=review_f35_nested_failure"))
            .count(),
        1,
        "主程序子模块必须使用同一诊断策略"
    );
    for private in [
        "review-f35-private-password",
        "private-user",
        "review-f35-private-userinfo",
        "review-f35-private-query",
        "review-f35-private-fragment",
        "review-f35-private-bearer",
        "review-f35-private-ocr-body",
        "review-f35-private-lookalike-body",
        "review_f35_foreign_body",
        "review_f35_lookalike_body",
    ] {
        assert!(!captured.contains(private), "诊断隐私泄漏：{private}");
    }
}

#[test]
fn disk_records_are_single_line_correlated_and_redacted() {
    if !in_child("logging::tests::disk_records_are_single_line_correlated_and_redacted") {
        return;
    }
    let temp = workspace();
    let guard = init(temp.path()).unwrap();
    let parent = operation_span_with_id("probe", "parent", "parent-probe-id");
    let _entered = parent.enter();
    let child = operation_span_with_id("probe", "child", "child-probe-id");
    let child_entered = child.enter();
    tracing::info!(event = "probe_started", display = %"first\nsecond\r\0", debug = ?"third\nfourth", password = "private-password", endpoint = "https://user:private-userinfo@example.invalid/a?token=private-query#private-fragment", detail = "Authorization: Bearer private-bearer", "诊断开始\n裸续行不允许");
    tracing::warn!(
        event = "probe_failed",
        stage = "connect",
        error_code = 10061,
        elapsed_ms = 7_u64,
        "诊断连接失败"
    );
    drop(child_entered);
    tracing::info!(event = "probe_completed", count = 2_u64, "诊断完成");
    let thread_span = parent.clone();
    std::thread::spawn(move || {
        thread_span.in_scope(|| {
            tracing::info!(
                event = "probe_thread_completed",
                token = 987_654_321_u64,
                "关联线程已完成"
            );
        });
    })
    .join()
    .unwrap();
    hold_for_process(guard);
    flush_before_exit();
    flush_before_exit();
    let directory = temp.path().join(LOG_DIR);
    let text = contents(&directory);
    for line in text.lines() {
        assert!(line.starts_with("timestamp="), "裸续行：{line}");
        let timestamp = line
            .split_whitespace()
            .next()
            .unwrap()
            .trim_start_matches("timestamp=");
        assert!(timestamp.ends_with('Z') && timestamp.split('.').nth(1).unwrap().len() >= 4);
        assert!(line.contains(&format!("pid={}", std::process::id())));
        assert!(
            line.contains(" level=")
                && line.contains(" thread_id=")
                && line.contains(" component=")
                && line.contains(" event=")
        );
        assert!(!line.chars().any(char::is_control));
    }
    assert!(
        text.contains("event=probe_started")
            && text.contains("event=probe_failed")
            && text.contains("event=probe_completed")
    );
    let failed = text
        .lines()
        .find(|line| line.contains("event=probe_failed"))
        .unwrap();
    assert!(
        failed.contains("parent-probe-id")
            && failed.contains("child-probe-id")
            && failed.contains("stage=\"connect\"")
            && failed.contains("error_code=10061")
            && failed.contains("elapsed_ms=7")
    );
    for secret in [
        "private-password",
        "private-userinfo",
        "private-query",
        "private-fragment",
        "private-bearer",
    ] {
        assert!(!text.contains(secret), "凭据泄漏：{secret}");
    }
    let thread_event = text
        .lines()
        .find(|line| line.contains("event=probe_thread_completed"))
        .unwrap();
    assert!(thread_event.contains("parent-probe-id") && !thread_event.contains("987654321"));
    assert_eq!(text.matches("event=application_stopping").count(), 1);
    assert_eq!(text.matches("event=application_stopped").count(), 1);
    assert!(text.contains("https://example.invalid/a"));
}

/// 覆盖 P-10：默认 INFO 配置必须记录设置操作开始、成功终态和耗时。
#[test]
fn settings_operations_are_observable_at_default_info_level() {
    if !in_child("logging::tests::settings_operations_are_observable_at_default_info_level") {
        return;
    }
    let temp = workspace();
    // exact 子进程只执行本测试，覆盖变量不与其他测试线程共享。
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", temp.path());
    std::env::set_var("JCH_LOG_LEVEL", "info");
    let previous = std::panic::take_hook();
    let guard = init(temp.path()).unwrap();
    let runtime = temp.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::write(runtime.join("xberg.exe"), b"settings logging fixture").unwrap();
    crate::xberg_settings::settings().unwrap();
    crate::xberg_settings::save_bundled_directory(&runtime).unwrap();
    drop(guard);
    std::panic::set_hook(previous);
    std::env::remove_var("JCHTOOLS_TEST_STATE_DIR");
    std::env::remove_var("JCH_LOG_LEVEL");
    let captured = contents(&temp.path().join(LOG_DIR));
    for operation in ["read_sources", "save_source"] {
        let events: Vec<_> = captured
            .lines()
            .filter(|line| line.contains(&format!("operation=\"{operation}\"")))
            .collect();
        let started = events
            .iter()
            .find(|line| line.contains("event=settings_operation_started"))
            .unwrap();
        let completed = events
            .iter()
            .find(|line| line.contains("event=settings_operation_completed"))
            .unwrap();
        let correlation = started
            .split_whitespace()
            .find(|field| field.starts_with("operation_id="))
            .unwrap();
        assert!(started.contains("level=INFO"));
        assert!(completed.contains("level=INFO"));
        assert!(completed.contains("elapsed_ms="));
        assert!(completed.contains(correlation));
    }
}

#[test]
fn retention_preserves_non_logs_and_recent_records() {
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    std::fs::create_dir_all(&directory).unwrap();
    let old = SystemTime::now() - Duration::from_hours(16 * 24);
    for name in [
        "jchtools.log.2026-01-01",
        "jchtools.2026-01-01.123.log",
        "jchtools.log.notes",
        "jchtools.2026-01-01.123.log.backup",
        "jchtools.2026-99-99.123.log",
        "business.log",
    ] {
        let file = std::fs::File::create(directory.join(name)).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    std::fs::write(directory.join("jchtools.2026-01-02.456.log"), b"recent").unwrap();
    std::fs::create_dir(directory.join("jchtools.log.2026-01-03")).unwrap();
    assert_eq!(prune_expired(&directory), (2, 0));
    for name in [
        "jchtools.log.notes",
        "jchtools.2026-01-01.123.log.backup",
        "jchtools.2026-99-99.123.log",
        "business.log",
        "jchtools.2026-01-02.456.log",
        "jchtools.log.2026-01-03",
    ] {
        assert!(directory.join(name).exists(), "不得删除：{name}");
    }
}

#[test]
fn conflicting_subscriber_is_not_reported_as_initialized() {
    if !in_child("logging::tests::conflicting_subscriber_is_not_reported_as_initialized") {
        return;
    }
    tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
    let temp = workspace();
    assert!(init(temp.path()).is_none());
    assert!(!contents(&temp.path().join(LOG_DIR)).contains("event=application_started"));
}
/// 覆盖 P-10：Debug 字符串的转义引号不能绕过凭据去敏。
#[test]
fn debug_quoted_credentials_are_redacted_on_disk() {
    if !in_child("logging::tests::debug_quoted_credentials_are_redacted_on_disk") {
        return;
    }
    let temp = workspace();
    let guard = init(temp.path()).unwrap();
    let detail = r#"{"api_key":"synthetic-private-key","password":"synthetic-private-password"}"#;
    tracing::error!(event = "debug_credential_probe", ?detail, "去敏探针");
    drop(guard);
    let captured = contents(&temp.path().join(LOG_DIR));
    assert!(captured.contains("event=debug_credential_probe"));
    assert!(captured.contains("[redacted]"));
    assert!(!captured.contains("synthetic-private-key"));
    assert!(!captured.contains("synthetic-private-password"));
}

/// 覆盖 P-10：WARNING 阈值仍保留操作上下文，但不输出 INFO 事件。
#[test]
fn warning_threshold_preserves_operation_correlation() {
    const CHILD: &str = "JCHTOOLS_WARNING_LEVEL_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "logging::tests::warning_threshold_preserves_operation_correlation",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("JCH_LOG_LEVEL", "warning")
            .status()
            .unwrap();
        assert!(status.success(), "WARNING 阈值日志必须保留关联 ID");
        return;
    }
    let temp = workspace();
    let guard = init(temp.path()).unwrap();
    let span = operation_span_with_id("probe", "warning_threshold", "warning-probe-id");
    span.in_scope(|| {
        tracing::info!(event = "filtered_info_probe", "此事件必须被过滤");
        tracing::warn!(
            event = "correlated_warning_probe",
            "此异常必须保留操作上下文"
        );
    });
    drop(guard);
    let captured = contents(&temp.path().join(LOG_DIR));
    let warning = captured
        .lines()
        .find(|line| line.contains("event=correlated_warning_probe"))
        .unwrap();
    assert!(warning.contains("warning-probe-id"));
    assert!(warning.contains("operation=\"warning_threshold\""));
    assert!(!captured.contains("event=filtered_info_probe"));
}

/// 覆盖 P-10 / CA-05：每种实际内部入口都具有明确的进程角色。
#[test]
fn internal_process_roles_are_identified() {
    use std::ffi::OsStr;
    for (flag, expected) in [
        ("--codeagent-service", "codeagent-service"),
        ("--acp-http-service", "acp-http-service"),
        ("--xberg-broker", "xberg-broker"),
        ("--service", "snap-ocr-service"),
        ("--service--autostart", "snap-ocr-service"),
    ] {
        assert_eq!(role_for_mode(Some(OsStr::new(flag))), expected);
    }
    assert_eq!(role_for_mode(None), "gui");
}

/// 覆盖 P-10：实际 CLI 进程不应标成 GUI，CLI 启动的内部代理仍标成代理。
#[test]
fn cli_process_role_uses_executable_identity_and_preserves_broker_mode() {
    use std::ffi::OsStr;
    for name in ["jchocr.exe", "JCHOCR.EXE"] {
        let executable = Some(Path::new(name));
        assert_eq!(role_for_process(executable, None), "jchocr");
        assert_eq!(
            role_for_process(executable, Some(OsStr::new("C:/synthetic-image.png"))),
            "jchocr"
        );
        assert_eq!(
            role_for_process(executable, Some(OsStr::new("--xberg-broker"))),
            "xberg-broker"
        );
    }
    assert_eq!(
        role_for_process(Some(Path::new("JchTools.exe")), None),
        "gui"
    );
    assert_eq!(role_for_process(None, None), "gui");
}

/// 覆盖 P-10：启动记录的构建模式不能误用进程角色。
#[test]
fn startup_record_identifies_build_mode() {
    if !in_child("logging::tests::startup_record_identifies_build_mode") {
        return;
    }
    let temp = workspace();
    let guard = init(temp.path()).unwrap();
    drop(guard);
    let captured = contents(&temp.path().join(LOG_DIR));
    let started = captured
        .lines()
        .find(|line| line.contains("event=application_started"))
        .unwrap();
    let expected = if cfg!(debug_assertions) {
        "debug-assertions-enabled"
    } else {
        "debug-assertions-disabled"
    };
    assert!(started.contains(&format!("mode=\"{expected}\"")));
}

#[test]
fn safe_helpers_bound_diagnostic_data() {
    assert_eq!(
        safe_url("https://name:password@example.invalid/a?token=secret#fragment"),
        "https://example.invalid/a"
    );
    assert_eq!(safe_url("not a URL with private data"), "[invalid_url]");
    for text in [
        "password = private-value",
        "token=private-value",
        "Cookie: session=private-value",
        "Bearer private-value",
        "{\"api_key\":\"private-value\"}",
        "sk-private-value",
    ] {
        assert!(!safe_error(text).contains("private-value"));
    }
    let long = "诊".repeat(5000);
    let result = safe_error(&long);
    assert!(result.len() <= TEXT_LIMIT && result.ends_with("[truncated]"));
    assert!(!safe_error("a\nb\rc\0d").chars().any(char::is_control));
    let long_userinfo = format!(
        "https://{}@example.invalid/a?token=private-query",
        "private-credential".repeat(200)
    );
    assert_eq!(safe_url(&long_userinfo), "https://example.invalid/a");
    assert!(!safe_error(&long_userinfo).contains("private-credential"));
    assert_ne!(new_operation_id(), new_operation_id());
}

/// 覆盖 P-10：配置阈值接受大小写和首尾空白，非法值回到 INFO。
#[test]
fn log_level_selection_accepts_supported_values_and_defaults() {
    use tracing::level_filters::LevelFilter;
    for (input, expected) in [
        (None, LevelFilter::INFO),
        (Some(""), LevelFilter::INFO),
        (Some(" trace "), LevelFilter::INFO),
        (Some(" DeBuG \t"), LevelFilter::DEBUG),
        (Some("INFO"), LevelFilter::INFO),
        (Some(" Warning "), LevelFilter::WARN),
        (Some("error"), LevelFilter::ERROR),
    ] {
        assert_eq!(selected_level(input), expected);
    }
}

/// 覆盖 P-10：即使 PID 被复用，同日历史日志也不能接续写入。
#[test]
fn same_pid_history_is_never_reopened_by_a_new_appender() {
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    std::fs::create_dir_all(&directory).unwrap();
    let mut timestamp = String::new();
    tracing_subscriber::fmt::time::SystemTime
        .format_time(&mut Writer::new(&mut timestamp))
        .unwrap();
    let date = &timestamp[..10];
    let pid = std::process::id();
    let history = [
        directory.join(format!("jchtools.{date}.{pid}.log")),
        directory.join(format!("jchtools.{date}.{pid}-1-1.log")),
    ];
    for path in &history {
        std::fs::write(path, b"previous-process-record\n").unwrap();
    }
    let mut appender = open_appender(&directory).unwrap();
    appender.write_all(b"new-process-record\n").unwrap();
    appender.flush().unwrap();
    for path in &history {
        assert_eq!(std::fs::read(path).unwrap(), b"previous-process-record\n");
        assert_ne!(&appender.path, path);
    }
    assert_eq!(
        std::fs::read(&appender.path).unwrap(),
        b"new-process-record\n"
    );
}

/// 覆盖 P-10：同一进程重新创建 appender 也必须保留各 writer 的独立文件。
#[test]
fn reinitialized_appenders_never_share_existing_files() {
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    let mut first = open_appender(&directory).unwrap();
    first.write_all(b"first-writer-record\n").unwrap();
    first.flush().unwrap();
    let mut second = open_appender(&directory).unwrap();
    second.write_all(b"second-writer-record\n").unwrap();
    second.flush().unwrap();
    assert_ne!(first.path, second.path);
    first.write_all(b"first-writer-still-active\n").unwrap();
    first.flush().unwrap();
    assert_eq!(
        std::fs::read(&first.path).unwrap(),
        b"first-writer-record\nfirst-writer-still-active\n"
    );
    assert_eq!(
        std::fs::read(&second.path).unwrap(),
        b"second-writer-record\n"
    );
}

/// 覆盖 P-10：轮转目标已被其他 writer 占用时必须跳过，不能追加旧字节。
#[test]
fn rotation_skips_files_reserved_by_other_writers() {
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    let mut appender = open_appender(&directory).unwrap();
    let record = format!("{}\n", "x".repeat(RECORD_LIMIT - 1));
    for _ in 0..FILE_LIMIT / RECORD_LIMIT as u64 {
        appender.write_all(record.as_bytes()).unwrap();
    }
    appender.flush().unwrap();
    let filename = appender.path.file_name().unwrap().to_str().unwrap();
    let suffix = if appender.segment == 0 {
        ".log".to_owned()
    } else {
        format!(".{}.log", appender.segment)
    };
    let prefix = filename.strip_suffix(&suffix).unwrap();
    let occupied = directory.join(format!("{prefix}.{}.log", appender.segment + 1));
    std::fs::write(&occupied, b"other-writer-record\n").unwrap();
    appender.write_all(b"rotated-record\n").unwrap();
    appender.flush().unwrap();
    assert_eq!(std::fs::read(&occupied).unwrap(), b"other-writer-record\n");
    assert_ne!(appender.path, occupied);
    assert_eq!(std::fs::read(&appender.path).unwrap(), b"rotated-record\n");
}

/// 覆盖 P-10：新旧精确归属均参与保留期，近似名称与未知文件不受影响。
#[test]
fn retention_recognizes_exact_process_run_names_only() {
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    std::fs::create_dir_all(&directory).unwrap();
    let owned = [
        "jchtools.log.2026-01-01",
        "jchtools.2026-01-01.123.log",
        "jchtools.2026-01-01.123.4.log",
        "jchtools.2026-01-01.123-456-7.log",
        "jchtools.2026-01-01.123-456-7.8.log",
    ];
    let unknown = [
        "jchtools.2026-01-01.123-456.log",
        "jchtools.2026-01-01.123-456-7-8.log",
        "jchtools.2026-01-01.123--7.log",
        "jchtools.2026-01-01.123-456-sequence.log",
        "jchtools.2026-01-01.123-456-7..log",
        "jchtools.2026-01-01.123-456-7.8.9.log",
        "jchtools.2026-99-01.123-456-7.log",
        "jchtools.2026-01-01.123-456-7.log.backup",
        "business.log",
    ];
    let old = SystemTime::now() - Duration::from_hours(16 * 24);
    for name in owned.iter().chain(&unknown) {
        let file = std::fs::File::create(directory.join(name)).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    assert_eq!(prune_expired(&directory), (owned.len() as u64, 0));
    for name in owned {
        assert!(!directory.join(name).exists(), "应清理过期日志：{name}");
    }
    for name in unknown {
        assert!(directory.join(name).exists(), "不得删除未知文件：{name}");
    }
}

/// 覆盖 P-10：UTF-8 和 LF 均计入容量；淘汰按同一顺序且保留未知文件。
#[test]
fn bounded_sink_rotates_before_overflow_and_limits_owned_history() {
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    let mut appender = open_appender(&directory).unwrap();
    let unowned = directory.join("jchtools.2026-01-01.123.log.gz");
    std::fs::write(&unowned, b"unowned compressed history").unwrap();
    let record = format!("{}aaa\n", "诊".repeat((RECORD_LIMIT - 4) / 3));
    assert_eq!(record.len(), RECORD_LIMIT);
    for _ in 0..(FILE_COUNT_LIMIT + 2) * 16 {
        appender.write_all(record.as_bytes()).unwrap();
    }
    appender.write_all(b"final-observable-record\n").unwrap();
    appender.flush().unwrap();
    let owned: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .flatten()
        .filter(|entry| product_log_name(&entry.file_name().to_string_lossy()))
        .collect();
    assert_eq!(owned.len(), FILE_COUNT_LIMIT);
    assert!(owned
        .iter()
        .all(|entry| entry.metadata().unwrap().len() <= FILE_LIMIT));
    assert!(owned
        .iter()
        .any(|entry| entry.metadata().unwrap().len() == FILE_LIMIT));
    assert!(contents(&directory).contains("final-observable-record"));
    assert_eq!(
        std::fs::read(&unowned).unwrap(),
        b"unowned compressed history"
    );
}

/// 覆盖 P-10：超大单条事件保留身份、去敏诊断字段及截断标记，不拆成多行。
#[test]
fn oversized_record_preserves_identity_and_error_metadata() {
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    let mut appender = open_appender(&directory).unwrap();
    let record = format!(
        "timestamp=2026-10-11T00:00:00.000Z level=ERROR pid=1 thread_id=ThreadId(1) component=\"安全组件\" event=oversized_failure span_id=1 span=\"outer\" stage=\"外层阶段\" error_type=OuterContext error_code=1 elapsed_ms=999 detail=\"{}\" stage=\"连接失败\" error_type=ConnectionRefused error_code=10061 elapsed_ms=7 request_id=probe-id\n",
        "诊断\\\"".repeat(RECORD_LIMIT)
    );
    appender.write_all(record.as_bytes()).unwrap();
    appender.flush().unwrap();
    let captured = contents(&directory);
    assert_eq!(captured.lines().count(), 1);
    assert!(captured.len() <= RECORD_LIMIT);
    for field in [
        "event=oversized_failure",
        "component=\"安全组件\"",
        "stage=\"连接失败\"",
        "error_type=ConnectionRefused",
        "error_code=10061",
        "elapsed_ms=7",
        "request_id=probe-id",
        "truncated=true",
    ] {
        assert!(captured.contains(field), "缺少诊断字段：{field}");
    }
    assert!(!captured.contains("detail="));
}

/// 覆盖 P-10：已有超大日志不截断、不删除，也不借数量清理迁移。
#[test]
fn oversized_existing_history_blocks_without_destructive_migration() {
    let temp = workspace();
    let directory = temp.path().join(LOG_DIR);
    std::fs::create_dir_all(&directory).unwrap();
    let existing = directory.join("jchtools.2026-01-01.123.log");
    std::fs::File::create(&existing)
        .unwrap()
        .set_len(FILE_LIMIT + 1)
        .unwrap();
    assert!(open_appender(&directory).is_err());
    assert_eq!(std::fs::metadata(existing).unwrap().len(), FILE_LIMIT + 1);
}
