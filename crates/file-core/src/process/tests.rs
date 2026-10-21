use super::capture::run_with_timeout_ext;
use super::tests_runtime::silent_hung_command;
use super::*;
use crate::control::Control;
use std::io::Write;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

#[cfg(windows)]
#[derive(Clone)]
struct DiagnosticBuffer(mpsc::Sender<Vec<u8>>);

#[cfg(windows)]
impl Write for DiagnosticBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(bytes.to_vec())
            .map_err(|_| std::io::Error::other("测试日志接收器已关闭"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// P-10：用真实子进程输出的 PID 核对默认 INFO 级别的磁盘诊断，不依赖 DEBUG subscriber。
#[cfg(windows)]
fn review_f08_default_lifecycle_logs(test_name: &str, streaming: bool) {
    const CHILD: &str = "JCHTOOLS_REVIEW_F08_LOG_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, "1")
            .env_remove("JCH_LOG_LEVEL")
            .status()
            .unwrap();
        assert!(status.success(), "默认 INFO 子进程生命周期诊断回归失败");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let previous_hook = std::panic::take_hook();
    let guard = jchtools_foundation::logging::init(directory.path())
        .expect("隔离进程必须成功初始化真实磁盘日志");
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Console]::WriteLine($PID); [Console]::WriteLine('review-f08-output')",
    ]);
    let output = if streaming {
        let mut lines = Vec::new();
        run_with_idle_timeout(
            &mut command,
            &Control::default(),
            Duration::from_secs(10),
            |stderr, text| {
                assert!(!stderr, "健康短进程不应产生 stderr：{text}");
                lines.push(text.to_owned());
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        lines.join("\n")
    } else {
        let output = run_with_timeout(&mut command, Duration::from_secs(10)).unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        String::from_utf8(output.stdout).unwrap()
    };
    drop(guard);
    std::panic::set_hook(previous_hook);
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines.len(), 2, "必须先验证真实管道输出：{output:?}");
    let child_pid = lines[0].trim().parse::<u32>().unwrap();
    assert!(child_pid > 0);
    assert_eq!(lines[1].trim(), "review-f08-output");
    let mut text = String::new();
    for entry in std::fs::read_dir(directory.path().join("logs")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "log") {
            text.push_str(&std::fs::read_to_string(path).unwrap());
        }
    }
    let records: Vec<_> = text.lines().collect();
    let has_event = |line: &str, event: &str| {
        line.split_whitespace()
            .any(|field| field == format!("event={event}"))
    };
    assert!(
        records.iter().any(|line| has_event(line, "spawn_started")),
        "默认 INFO 磁盘日志缺少子进程开始事件：{text}"
    );
    let peer = format!("peer_pid={child_pid}");
    assert!(
        records.iter().any(|line| has_event(line, "spawn_completed")
            && line
                .split_whitespace()
                .any(|field| field.trim_end_matches(['}', ':']) == peer)),
        "默认 INFO 必须记录真实子进程 PID，而不是只创建无事件的 span：{text}"
    );
    assert!(
        records
            .iter()
            .any(|line| has_event(line, "subprocess_completed")
                && line.contains(&peer)
                && line.contains("elapsed_ms=")),
        "默认 INFO 磁盘日志缺少关联到真实 PID 的完成及耗时：{text}"
    );
    assert!(
        records.iter().any(|line| line.contains(&peer)
            && line.contains("success=true")
            && line.contains("exit_code=0")),
        "默认 INFO 必须保留真实成功退出结果：{text}"
    );
    assert!(!text.contains("review-f08-output"), "管道正文不应进入诊断");
}

// P-10：捕获 API 的正常调用必须在默认级别留下开始、真实 PID、退出结果与耗时。
#[cfg(windows)]
#[test]
fn review_f08_capture_lifecycle_is_persisted_at_default_info() {
    review_f08_default_lifecycle_logs(
        "process::tests::review_f08_capture_lifecycle_is_persisted_at_default_info",
        false,
    );
}

// P-10：流式 API 也必须保留同一生命周期诊断；输出回调接收真实命令结果。
#[cfg(windows)]
#[test]
fn review_f08_stream_lifecycle_is_persisted_at_default_info() {
    review_f08_default_lifecycle_logs(
        "process::tests::review_f08_stream_lifecycle_is_persisted_at_default_info",
        true,
    );
}

// 覆盖 P-10：真实成功/非零退出结果、安全摘要、读线程关联及日志隐私。
#[cfg(windows)]
#[test]
fn subprocess_logs_preserve_results_and_thread_correlation_without_body_leaks() {
    // 与 HTTP 日志测试一致：隔离 tracing 进程级 callsite 缓存，
    // 避免其他测试的 INFO subscriber 屏蔽本测试所需的 DEBUG 结果诊断。
    const CHILD: &str = "JCHTOOLS_PROCESS_RESULT_LOG_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::subprocess_logs_preserve_results_and_thread_correlation_without_body_leaks",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "隔离子进程结果诊断验证失败");
        return;
    }
    let (sender, receiver) = mpsc::channel();
    let writer = DiagnosticBuffer(sender);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let outcomes = tracing::subscriber::with_default(subscriber, || {
        [7, 0].map(|exit_code| {
            let root_id = jchtools_foundation::logging::new_operation_id();
            let root = jchtools_foundation::logging::operation_span_with_id("test", "capture_chain", &root_id);
            let _entered = root.enter();
            let mut command = Command::new("cmd.exe");
            command
                .args([
                    "/D",
                    "/C",
                    &format!(
                        "echo private-terminal-secret& echo private-error-secret 1>&2& exit /b {exit_code}"
                    ),
                ])
                .env("DIAGNOSTIC_TEST_SECRET", "private-environment-secret");
            let out =
                run_with_timeout_ext(&mut command, None, Some(Duration::from_secs(5)), None, 8, false)
                    .unwrap();
            assert_eq!(out.status.code(), Some(exit_code));
            assert_eq!(out.stdout, b"private-");
            assert_eq!(out.stderr, b"private-");
            assert!(out.stdout_truncated && out.stderr_truncated);
            (root_id, out)
        })
    });
    let text = String::from_utf8(receiver.try_iter().flatten().collect()).unwrap();
    for (root_id, out) in outcomes {
        let correlated: Vec<_> = text
            .lines()
            .filter(|line| line.contains(&root_id))
            .collect();
        assert!(
            correlated
                .iter()
                .any(|line| line.contains("timeout_ms=5000")
                    && line.contains("capture_limit=8")
                    && line.contains("stdin_present=false")),
            "子进程缺少关联的开始诊断：{text}"
        );
        let terminal = correlated
            .iter()
            .find(|line| {
                line.contains(&format!("success={}", out.status.success()))
                    && line.contains(&format!("exit_code={}", out.status.code().unwrap()))
                    && line.contains(&format!("stdout_bytes={}", out.stdout.len()))
                    && line.contains(&format!("stderr_bytes={}", out.stderr.len()))
                    && line.contains("stdout_truncated=true")
                    && line.contains("stderr_truncated=true")
                    && line.contains("elapsed_ms=")
            })
            .unwrap_or_else(|| {
                panic!("子进程最终诊断必须匹配真实退出、输出统计、截断和耗时：{text}")
            });
        let peer_pid = terminal
            .split_whitespace()
            .find_map(|field| field.strip_prefix("peer_pid="))
            .map(|value| value.trim_end_matches(['}', ':']))
            .expect("最终诊断必须保留真实子进程 PID");
        assert!(peer_pid.parse::<u32>().is_ok_and(|pid| pid > 0));
        for stream in ["stdout", "stderr"] {
            assert!(
                correlated.iter().any(|line| {
                    line.contains(&format!("peer_pid={peer_pid}"))
                        && line.contains(&format!("stream=\"{stream}\""))
                        && line.contains("limit_bytes=8")
                }),
                "真实截断读线程缺少父操作及子进程关联（{stream}）：{text}"
            );
        }
    }
    for private in [
        "private-terminal-secret",
        "private-error-secret",
        "private-environment-secret",
        "DIAGNOSTIC_TEST_SECRET",
    ] {
        assert!(
            !text.contains(private),
            "日志泄漏参数、环境或管道正文：{text}"
        );
    }
}

// 覆盖 P-10：两个独立失败均有开始、阶段原因和最终失败耗时；不靠日志证明回收。
#[cfg(windows)]
#[test]
fn subprocess_logs_spawn_failure_and_timeout_recovery() {
    const CHILD: &str = "JCHTOOLS_PROCESS_FAILURE_LOG_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process::tests::subprocess_logs_spawn_failure_and_timeout_recovery",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "隔离子进程失败诊断验证失败");
        return;
    }
    let (sender, receiver) = mpsc::channel();
    let writer = DiagnosticBuffer(sender);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    let spawn_id = jchtools_foundation::logging::new_operation_id();
    let timeout_id = jchtools_foundation::logging::new_operation_id();
    tracing::subscriber::with_default(subscriber, || {
        {
            let root = jchtools_foundation::logging::operation_span_with_id(
                "test",
                "failure_chain",
                &spawn_id,
            );
            let _entered = root.enter();
            let mut missing = Command::new("missing-private-executable-secret.exe");
            let _spawn_error = run_with_timeout(&mut missing, Duration::from_secs(1)).unwrap_err();
        }
        {
            let root = jchtools_foundation::logging::operation_span_with_id(
                "test",
                "failure_chain",
                &timeout_id,
            );
            let _entered = root.enter();
            let mut hung = silent_hung_command();
            let error = run_with_timeout(&mut hung, Duration::from_millis(100)).unwrap_err();
            assert!(error.to_string().contains("超时"), "{error:#}");
        }
    });
    let text = String::from_utf8(receiver.try_iter().flatten().collect()).unwrap();
    for (id, timeout_ms) in [(&spawn_id, 1000), (&timeout_id, 100)] {
        assert!(
            text.lines().any(|line| line.contains(id)
                && line.contains(&format!("timeout_ms={timeout_ms}"))
                && line.contains("stdin_present=false")),
            "失败操作缺少开始诊断：{text}"
        );
        assert!(
            text.lines().any(|line| line.contains(id)
                && line.split_whitespace().any(|field| field == "ERROR")
                && line.contains("state=\"failed\"")
                && line.contains("elapsed_ms=")),
            "失败操作缺少关联的最终错误及耗时：{text}"
        );
    }
    assert!(
        text.lines().any(|line| line.contains(&spawn_id)
            && line.contains("stage=\"spawn\"")
            && line.contains("error_type=NotFound")),
        "启动失败缺少系统失败分类：{text}"
    );
    let timeout = text
        .lines()
        .find(|line| {
            line.contains(&timeout_id)
                && line.contains("stage=\"wait\"")
                && line.contains("timeout_ms=100")
                && line.contains("peer_pid=")
        })
        .expect("超时必须记录等待阶段、超时值及子进程关联");
    let peer_pid = timeout
        .split_whitespace()
        .find_map(|field| field.strip_prefix("peer_pid="))
        .map(|value| value.trim_end_matches(['}', ':']))
        .expect("超时诊断必须携带子进程 PID");
    assert!(peer_pid.parse::<u32>().is_ok_and(|pid| pid > 0));
    assert!(
        text.lines().any(|line| line.contains(&timeout_id)
            && line.contains(&format!("peer_pid={peer_pid}"))
            && line.contains("state=\"failed\"")
            && line.contains("elapsed_ms=")),
        "超时最终结果丢失子进程关联：{text}"
    );
    assert!(
        !text.contains("missing-private-executable-secret"),
        "日志泄漏敏感执行路径：{text}"
    );
}
