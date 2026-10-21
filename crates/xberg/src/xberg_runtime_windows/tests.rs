use super::background::*;
use super::client::*;
use super::engine::*;
use super::message::*;
use super::server::*;
use super::*;
use jchtools_foundation::logging;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_CONNECTED};

fn perf_workspace() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|path| path.join("scripts/make_tmp.py").is_file())
        .expect("仓库必须包含临时目录工厂");
    let directory = root.join(".tmp/xberg-perf-tests");
    assert!(Command::new("python")
        .arg(root.join("scripts/make_tmp.py"))
        .args(["workspace", "--destination"])
        .arg(&directory)
        .current_dir(root)
        .status()
        .expect("临时目录工厂必须可执行")
        .success());
    tempfile::Builder::new()
        .prefix("engine-restarts-")
        .tempdir_in(directory)
        .expect("测试临时目录必须可创建")
}

/// 同一 broker 进程连续启动引擎也不能复用性能文件的父目录。
#[test]
fn engine_restarts_reserve_distinct_performance_directories() {
    let temporary = perf_workspace();
    let first = create_engine_perf_directory(temporary.path()).expect("首次目录必须可创建");
    let first_log = first.join("perf.log.2026-10-17");
    std::fs::write(&first_log, b"first engine marker").expect("首份性能日志必须可写");
    let second = create_engine_perf_directory(temporary.path()).expect("重启目录必须可创建");
    assert_ne!(first, second);
    assert_eq!(
        first.parent(),
        Some(temporary.path().join("perf-logs/xberg").as_path())
    );
    assert_eq!(first.parent(), second.parent());
    assert_eq!(
        std::fs::read_dir(&second)
            .expect("新目录必须可读取")
            .count(),
        0
    );
    std::fs::write(second.join("perf.log.2026-10-17"), b"second engine marker")
        .expect("第二份性能日志必须可写");
    assert_eq!(
        std::fs::read(first_log).expect("首份性能日志必须可读取"),
        b"first engine marker"
    );
}

/// 目录不可用必须报告不可采集，不能打开既有共享文件作为备用性能文件。
#[test]
fn unavailable_engine_performance_directory_is_not_reused() {
    let temporary = perf_workspace();
    let parent = temporary.path().join("perf-logs");
    std::fs::create_dir(&parent).expect("测试父目录必须可创建");
    let obstruction = parent.join("xberg");
    std::fs::write(&obstruction, b"existing shared bytes").expect("测试障碍必须可写");
    assert!(create_engine_perf_directory(temporary.path()).is_err());
    assert_eq!(
        std::fs::read(obstruction).expect("测试障碍必须可读取"),
        b"existing shared bytes"
    );
}

// 覆盖 T-23 修复的转发收窄：只剔除产物不消费的重型字段，正文、嵌入子文档
// 与告警逐字节保留；非 extract 响应（如 transcribe 的 markdown）不受影响。
#[test]
fn prune_keeps_product_fields_and_strips_heavy_fields_recursively() {
    let response = json!({
        "id": "r1",
        "ok": true,
        "document": {
            "content": "正文",
            "mime_type": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "images": [{"path": "image_0.png", "bytes": 12345}],
            "pages": [{"number": 1}],
            "tables": [{"rows": 2}],
            "processing_warnings": [{"source": "exif", "message": "no exif data found"}],
            "children": [{
                "path": "word/embeddings/nested.docx",
                "result": {
                    "content": "嵌入正文",
                    "images": [{"path": "image_1.png"}],
                    "pages": [1],
                    "processing_warnings": [{"source": "ocr", "message": "low confidence"}]
                }
            }]
        }
    });
    let document = prune_forwarded_document(response)["document"].clone();
    assert_eq!(document["content"], "正文");
    assert_eq!(document["children"][0]["result"]["content"], "嵌入正文");
    assert_eq!(
        document["processing_warnings"][0]["message"],
        "no exif data found"
    );
    assert_eq!(
        document["children"][0]["result"]["processing_warnings"][0]["source"],
        "ocr"
    );
    // T-14（2026-10-04）：images 是图片落盘的唯一数据通道，转发侧必须放行
    //（含嵌入子文档逐层放行）；pages/tables 仍不进产物。
    assert!(document.get("images").is_some());
    assert_eq!(document["images"][0]["path"], "image_0.png");
    assert!(document["children"][0]["result"].get("images").is_some());
    assert!(document.get("pages").is_none());
    assert!(document.get("tables").is_none());
    assert!(document["children"][0]["result"].get("pages").is_none());
    let media = prune_forwarded_document(json!({"id":"m1","ok":true,"markdown":"media"}));
    assert_eq!(media["markdown"], "media");
}

/// 覆盖 XB-05/XB-17：超限响应只排空当前 JSON 行并流式提取顶层 ID，
/// 即使 document 位于 id 之前且正文含嵌套伪 id，也不把另一场景的
/// pending 请求一并标成通信断裂。
#[test]
fn oversized_line_is_drained_without_retaining_payload() {
    let mut input = std::io::Cursor::new(
        br#"{"document":{"nested":{"id":"fake"},"content":"0123456789"},"id":"document-1"}
{"id":"snapshot-1","ok":true}
"#
        .to_vec(),
    );
    let first = read_bounded_line(&mut input, 16).expect("第一行应能排空");
    match first {
        BoundedLine::Oversized { id, bytes } => {
            assert!(bytes > 16);
            assert_eq!(id.as_deref(), Some("document-1"));
        }
        BoundedLine::Complete(_) => panic!("超限行不应保留完整正文"),
    }
    let second = read_bounded_line(&mut input, MAX_MESSAGE_BYTES).expect("下一行应保持同步");
    match second {
        BoundedLine::Complete(bytes) => {
            let response: Value = serde_json::from_slice(&bytes).expect("JSON 应完整");
            assert_eq!(response["id"], "snapshot-1");
        }
        BoundedLine::Oversized { .. } => panic!("第二行不应超限"),
    }
}

#[test]
fn response_id_scanner_honors_json_escapes_and_depth() {
    let mut scanner = ResponseIdScanner::new();
    scanner.feed(br#"{"document":{"id":"fake","text":"quote \"id\": fake"},"id":"real-\u0031"}"#);
    assert_eq!(scanner.id.as_deref(), Some("real-1"));
}

#[test]
fn response_id_scanner_ignores_structural_characters_inside_payload_strings() {
    // 覆盖 XB-14：正文不能影响响应 ID 和请求隔离。
    let mut scanner = ResponseIdScanner::new();
    scanner.feed(br#"{"document":{"text":"}], {\"id\":\"fake\"}"},"note":"{,}","id":"real"}"#);
    assert_eq!(scanner.id.as_deref(), Some("real"));
}

#[test]
fn background_stop_marker_is_not_written_for_rejected_response() {
    let response = json!({
        "ok": false,
        "error": "文档任务仍在运行"
    });
    assert!(!should_mark_background_stopped(
        "broker-force-stop",
        &response
    ));
    assert!(should_mark_background_stopped(
        "broker-stop",
        &json!({"ok":true,"running":true,"stopping":true})
    ));
}

/// 覆盖 XB-08/XB-17：超时取消终态不冒充用户取消，进程异常仍保留原分类。
#[test]
fn timeout_cancel_terminal_retains_actual_reason() {
    let cancelled =
        json!({"id":"request-1","ok":false,"error_kind":"cancelled","error":"cancelled"});
    let timed_out = restore_timeout_kind(cancelled.clone(), true);
    assert_eq!(timed_out["id"], "request-1");
    assert_eq!(timed_out["ok"], false);
    assert_eq!(timed_out["error_kind"], "timeout");
    assert_eq!(timed_out["error"], "Xberg 请求已超时");
    assert_eq!(restore_timeout_kind(cancelled.clone(), false), cancelled);
    let exited = json!({"ok":false,"error_kind":"process_exited","error":"通信中断"});
    assert_eq!(restore_timeout_kind(exited.clone(), true), exited);
}

struct SubmissionProbe<'a> {
    cursor: std::io::Cursor<Vec<u8>>,
    submitted: &'a AtomicBool,
    acknowledgement_bytes: usize,
}

impl std::io::Read for SubmissionProbe<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.cursor, bytes)
    }
}

impl BufRead for SubmissionProbe<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        // 强制检查读终态之前已发布确认，而非读完终态后才置位。
        assert_eq!(
            self.submitted.load(Ordering::Acquire),
            self.cursor.position() >= self.acknowledgement_bytes as u64,
        );
        self.cursor.fill_buf()
    }

    fn consume(&mut self, bytes: usize) {
        self.cursor.consume(bytes);
    }
}

/// 覆盖 XB-08/T-23：取消先发生时必须等待原请求登记确认；
/// 确认与终态连在同一读取缓冲区也不能漏掉确认或串错 ID。
#[test]
fn cancellation_waits_for_original_submission_before_terminal() {
    let submitted = AtomicBool::new(false);
    assert!(!submitted.load(Ordering::Acquire));
    let acknowledgement = submission_acknowledgement(&json!({"id":"original"}), 7);
    let terminal = json!({"id":"original","ok":false,"error_kind":"cancelled","jchtools_broker_protocol":BROKER_PROTOCOL});
    let bytes = format!("{acknowledgement}\n{terminal}\n");
    let mut reader = SubmissionProbe {
        cursor: std::io::Cursor::new(bytes.into_bytes()),
        submitted: &submitted,
        acknowledgement_bytes: acknowledgement.to_string().len() + 1,
    };
    let received =
        read_broker_terminal(&mut reader, &json!("original"), Some(&submitted), None).unwrap();
    assert!(submitted.load(Ordering::Acquire));
    assert_eq!(received, terminal);
    assert_eq!(received["error_kind"], "cancelled");
}

/// 覆盖 XB-08/XB-17：拒绝登记、引擎异常及其他请求的确认不产生取消资格。
#[test]
fn failed_or_unowned_submission_never_enables_cancellation() {
    for kind in ["shared_runtime", "process_exited"] {
        let submitted = AtomicBool::new(false);
        let terminal = json!({"id":"original","ok":false,"error_kind":kind,"jchtools_broker_protocol":BROKER_PROTOCOL});
        let received = read_broker_terminal(
            &mut std::io::Cursor::new(format!("{terminal}\n").into_bytes()),
            &json!("original"),
            Some(&submitted),
            None,
        )
        .unwrap();
        assert_eq!(received, terminal);
        assert!(!submitted.load(Ordering::Acquire));
    }
    let submitted = AtomicBool::new(false);
    let unrelated = submission_acknowledgement(&json!({"id":"snapshot"}), 7);
    assert!(read_broker_terminal(
        &mut std::io::Cursor::new(format!("{unrelated}\n").into_bytes()),
        &json!("original"),
        Some(&submitted),
        None,
    )
    .is_err());
    assert!(!submitted.load(Ordering::Acquire));
}

/// 覆盖 XB-05/XB-08：登记确认不是终态，异常响应不能替代原请求终态。
#[test]
fn submission_confirmation_requires_one_matching_terminal() {
    let acknowledgement = submission_acknowledgement(&json!({"id":"original"}), 7);
    for next in [
        acknowledgement.clone(),
        json!({"id":"snapshot","ok":true,"jchtools_broker_protocol":BROKER_PROTOCOL}),
    ] {
        let submitted = AtomicBool::new(false);
        let bytes = format!("{acknowledgement}\n{next}\n");
        assert!(read_broker_terminal(
            &mut std::io::Cursor::new(bytes.into_bytes()),
            &json!("original"),
            Some(&submitted),
            None,
        )
        .is_err());
        assert!(submitted.load(Ordering::Acquire));
    }
    let submitted = AtomicBool::new(false);
    let old = json!({"id":"original","ok":true,"jchtools_broker_protocol":2});
    assert!(read_broker_terminal(
        &mut std::io::Cursor::new(format!("{old}\n").into_bytes()),
        &json!("original"),
        Some(&submitted),
        None,
    )
    .is_err());
    assert!(!submitted.load(Ordering::Acquire));
}

/// 覆盖 XB-08/XB-17：已结束或引擎异常的原请求仍交付真实终态；
/// 只有确认而没有终态的连接必须失败，不能以确认伪报完成。
#[test]
fn acknowledged_request_preserves_completion_and_engine_failure() {
    let acknowledgement = submission_acknowledgement(&json!({"id":"original"}), 7);
    for terminal in [
        json!({"id":"original","ok":true,"markdown":"已完成","jchtools_broker_protocol":BROKER_PROTOCOL}),
        json!({"id":"original","ok":false,"error_kind":"process_exited","jchtools_broker_protocol":BROKER_PROTOCOL}),
    ] {
        let submitted = AtomicBool::new(false);
        let bytes = format!("{acknowledgement}\n{terminal}\n");
        let received = read_broker_terminal(
            &mut std::io::Cursor::new(bytes.into_bytes()),
            &json!("original"),
            Some(&submitted),
            None,
        )
        .unwrap();
        assert!(submitted.load(Ordering::Acquire));
        assert_eq!(received, terminal);
    }
    let submitted = AtomicBool::new(false);
    assert!(read_broker_terminal(
        &mut std::io::Cursor::new(format!("{acknowledgement}\n").into_bytes()),
        &json!("original"),
        Some(&submitted),
        None,
    )
    .is_err());
    assert!(submitted.load(Ordering::Acquire));
}

/// 覆盖 XB-05/XB-08/XB-23：取消、控制、保活及能力查询仍只读取终态，
/// 不请求登记确认，也不能误吞本来的响应。
#[test]
fn query_and_control_exchanges_do_not_require_submission_acknowledgement() {
    for id in [
        "cancel",
        "broker-state",
        "broker-stop",
        "broker-force-stop",
        "keepalive",
        "capabilities",
    ] {
        let terminal = json!({"id":id,"ok":true,"accepted":false,"commands":["cancel"],"jchtools_broker_protocol":BROKER_PROTOCOL});
        let received = read_broker_terminal(
            &mut std::io::Cursor::new(format!("{terminal}\n").into_bytes()),
            &json!(id),
            None,
            None,
        )
        .unwrap();
        assert_eq!(received, terminal);
    }
    let acknowledgement = submission_acknowledgement(&json!({"id":"query"}), 7);
    assert!(read_broker_terminal(
        &mut std::io::Cursor::new(format!("{acknowledgement}\n").into_bytes()),
        &json!("query"),
        None,
        None,
    )
    .is_err());
}

/// 覆盖 XB-23：停止确认必须交到客户端，不能写完缓冲便释放状态锁并退出。
#[test]
fn background_stop_waits_for_client_to_consume_confirmation() -> Result<(), String> {
    let name = format!(
        r"\\.\pipe\jchtools-xberg-control-test-{}-{}",
        std::process::id(),
        logging::new_operation_id()
    );
    let pipe = create_pipe(&name).map_err(|error| error.to_string())?;
    let (written_send, written_receive) = mpsc::channel();
    let (read_send, read_receive) = mpsc::channel();
    let client = std::thread::spawn(move || -> Result<Value, String> {
        let mut pipe = OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .map_err(|e| e.to_string())?;
        write_json(
            &mut pipe,
            &json!({"request":{"id":"stop-test","command":"broker-stop"}}),
        )?;
        written_send.send(()).map_err(|e| e.to_string())?;
        read_receive.recv().map_err(|e| e.to_string())?;
        read_json(&mut BufReader::new(pipe))
    });
    // SAFETY: 测试独占的同步管道句柄有效，客户端提前连接也算连接成功。
    let connected = unsafe {
        windows_sys::Win32::System::Pipes::ConnectNamedPipe(
            pipe.as_raw_handle().cast(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: 紧邻失败的连接调用读取错误码。
    if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
        let _ = read_send.send(());
        let _ = client.join();
        return Err("测试控制管道连接失败".into());
    }
    let (handled_send, handled_receive) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let engine = Mutex::new(None);
        let stopping = AtomicBool::new(false);
        let active_handlers = Arc::new(AtomicUsize::new(0));
        let result = handle(pipe, &engine, &stopping, &active_handlers);
        let _ = handled_send.send(result);
    });
    let written = written_receive.recv_timeout(QUERY_TIMEOUT);
    let before_read = handled_receive.recv_timeout(Duration::from_millis(50));
    // 先释放、回收全部线程，再断言，失败反证不得留下等待读取的线程。
    let _ = read_send.send(());
    let response = client.join().map_err(|_| "测试客户端线程退出")??;
    let handled = if let Ok(result) = &before_read {
        result.clone()
    } else {
        handled_receive
            .recv_timeout(QUERY_TIMEOUT)
            .map_err(|e| e.to_string())?
    };
    server.join().map_err(|_| "测试服务线程退出")?;
    written.map_err(|e| e.to_string())?;
    handled?;
    assert!(matches!(before_read, Err(mpsc::RecvTimeoutError::Timeout)));
    assert_eq!(response["id"], "stop-test");
    assert_eq!(response["stopping"], true);
    Ok(())
}

#[test]
fn duration_max_is_rejected_before_broker_start() {
    let error = request_via_broker(
        Path::new("C:\\this-path-is-not-read"),
        &json!({"id":"duration-max","command":"keepalive"}),
        Duration::MAX,
        &AtomicBool::new(false),
        "duration-max",
    )
    .expect_err("Duration::MAX 必须在启动代理前明确拒绝");
    assert!(error.contains("未启动共享代理"));
}

#[derive(Clone)]
struct LogCapture(mpsc::Sender<Vec<u8>>);

impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(bytes.to_vec())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "诊断捕获端已关闭"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// tracing 的 callsite/级别缓存属于进程；仅锁捕获者不能隔离其他测试的日志调用。
// 复用 foundation 日志测试的 exact-child 模式，每个捕获测试独占进程内的 tracing 状态。
fn in_log_capture_child(name: &str) -> bool {
    const CHILD: &str = "JCHTOOLS_XBERG_LOG_CAPTURE_TEST_CHILD";
    if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new(name)) {
        return true;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .status()
        .unwrap();
    assert!(status.success(), "隔离进程中的日志行为断言必须通过：{name}");
    false
}

fn capture_logs() -> (tracing::Dispatch, mpsc::Receiver<Vec<u8>>) {
    let (send, bytes) = mpsc::channel();
    let writer = LogCapture(send);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || writer.clone())
        .finish();
    (tracing::Dispatch::new(subscriber), bytes)
}

#[test]
fn consumed_response_logs_safe_results_and_decode_failure_stage() {
    if !in_log_capture_child(
        "xberg_runtime::platform::tests::consumed_response_logs_safe_results_and_decode_failure_stage",
    ) {
        return;
    }
    let (dispatch, bytes) = capture_logs();
    let markdown = "private-success-markdown-sentinel";
    tracing::dispatcher::with_default(&dispatch, || {
        let span = logging::operation_span_with_id("xberg_runtime", "consume", "consume-response");
        let _entered = span.enter();
        let response = json!({"ok":false,"error_kind":"private-kind-sentinel","error":"private-error-sentinel","document":{"content":"private-document-sentinel"},"jchtools_xberg_pid":123});
        let mut encoded = serde_json::to_vec(&response).unwrap();
        encoded.push(b'\n');
        let consumed = read_json(&mut std::io::Cursor::new(encoded)).unwrap();
        assert_eq!(consumed, response, "日志不得改变引擎失败响应");
        log_response("client", "extract", &consumed, Instant::now(), None);
        let success = json!({"ok":true,"markdown":markdown});
        let mut encoded = serde_json::to_vec(&success).unwrap();
        encoded.push(b'\n');
        let consumed = read_json(&mut std::io::Cursor::new(encoded)).unwrap();
        assert_eq!(consumed, success, "诊断不得改变成功响应");
        log_response("client", "transcribe", &consumed, Instant::now(), None);
        let invalid = b"{private-invalid-json-sentinel}\n";
        assert!(read_json(&mut std::io::Cursor::new(invalid)).is_err());
    });
    let logs = String::from_utf8(bytes.try_iter().flatten().collect()).unwrap();
    assert!(logs.contains("xberg_response_failed"), "{logs}");
    assert!(logs.contains("remote_error"), "{logs}");
    assert!(logs.contains("xberg_response_completed"), "{logs}");
    assert!(
        logs.contains(&format!("markdown_bytes={}", markdown.len())),
        "{logs}"
    );
    assert!(!logs.contains(markdown), "成功响应正文泄漏：{logs}");
    assert!(logs.contains("xberg_message_decode_failed"), "{logs}");
    assert!(logs.contains("decode"), "{logs}");
    assert!(logs.contains("consume-response"), "{logs}");
    for sentinel in [
        "private-kind-sentinel",
        "private-error-sentinel",
        "private-document-sentinel",
        "private-invalid-json-sentinel",
    ] {
        assert!(!logs.contains(sentinel), "响应正文或不可信类型泄漏：{logs}");
    }
}

#[test]
fn broken_engine_delivers_terminal_with_thread_request_context() -> Result<(), String> {
    if !in_log_capture_child(
        "xberg_runtime::platform::tests::broken_engine_delivers_terminal_with_thread_request_context",
    ) {
        return Ok(());
    }
    let (dispatch, bytes) = capture_logs();
    let (send, receive) = mpsc::channel();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    tracing::dispatcher::with_default(&dispatch, || {
        let span =
            logging::operation_span_with_id("xberg_runtime", "broker_handle", "pending-operation");
        pending
            .lock()
            .map_err(|_| "请求表锁异常".to_string())?
            .insert(
                "pending-operation".into(),
                PendingRequest {
                    sender: send,
                    lane: Some("document"),
                    span,
                    started: Instant::now(),
                    command: "extract",
                    cancel_requested: false,
                },
            );
        Ok::<(), String>(())
    })?;
    let worker_pending = Arc::clone(&pending);
    std::thread::spawn(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            fail_pending(&worker_pending, &AtomicBool::new(false));
        });
    })
    .join()
    .unwrap();
    let response = receive.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(response["id"], "pending-operation");
    assert_eq!(response["ok"], false);
    assert_eq!(response["error_kind"], "process_exited");
    assert!(pending
        .lock()
        .map_err(|_| "请求表锁异常".to_string())?
        .is_empty());
    let logs = String::from_utf8(bytes.try_iter().flatten().collect()).unwrap();
    let terminal = logs
        .lines()
        .find(|line| line.contains("xberg_engine_pending_failed"))
        .expect("必须记录在途失败终态");
    assert!(terminal.contains("pending-operation"), "{logs}");
    assert!(terminal.contains("communication_broken"), "{logs}");
    assert!(terminal.contains("elapsed_ms"), "{logs}");
    Ok(())
}

#[test]
fn root_preflight_reuses_caller_diagnostic_id_without_logging_request_body() {
    if !in_log_capture_child(
        "xberg_runtime::platform::tests::root_preflight_reuses_caller_diagnostic_id_without_logging_request_body",
    ) {
        return;
    }
    let (dispatch, bytes) = capture_logs();
    tracing::dispatcher::with_default(&dispatch, || {
        let error = super::super::request(
            Path::new("C:\\this-path-is-not-read"),
            json!({"command":"ocr_snapshot","diagnostic_id":"snap-client-operation","image_base64":"private-image-sentinel"}),
            Duration::ZERO,
            &AtomicBool::new(false),
        ).expect_err("零预算必须在连接或能力握手前返回");
        assert!(error.contains("超时"));
    });
    let logs = String::from_utf8(bytes.try_iter().flatten().collect()).unwrap();
    let started = logs
        .lines()
        .find(|line| line.contains("xberg_request_started"))
        .expect("root started");
    let failed = logs
        .lines()
        .find(|line| line.contains("xberg_request_failed"))
        .expect("root failed");
    assert!(started.contains("snap-client-operation"), "{logs}");
    assert!(failed.contains("snap-client-operation"), "{logs}");
    assert!(failed.contains("preflight_timeout"), "{logs}");
    assert!(!logs.contains("private-image-sentinel"), "{logs}");
    assert!(
        !logs.contains("xberg_client_request_started"),
        "零预算不得越过请求预检：{logs}"
    );
}
