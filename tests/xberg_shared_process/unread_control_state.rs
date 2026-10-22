use super::*;

fn review_remediation_wait_for_unread_response(pipe: &std::fs::File) {
    use std::os::windows::io::AsRawHandle;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut available = 0;
        // SAFETY: pipe 是有效客户端句柄；仅观察字节数，不读走控制/终态响应。
        let peeked = unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                pipe.as_raw_handle().cast(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &raw mut available,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(peeked, 0, "代理写出响应前管道中断");
        if available > 0 {
            return;
        }
        assert!(Instant::now() < deadline, "代理未及时写出待消费响应");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// P3-16 / XB-08/XB-15/XB-17：控制客户端不读响应不能占住其他场景与原任务取消。
#[test]
fn review_remediation_unread_broker_state_does_not_block_requests_or_cancel() {
    use std::io::BufRead;

    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    prepare(root);
    std::fs::write(root.join("long.txt"), b"synthetic document").unwrap();
    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(state["ok"], true, "合成引擎启动失败：{state}");
    let engine_pid = state["jchtools_xberg_pid"].as_u64().unwrap();
    let shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: Some(engine_pid),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let token = Arc::clone(&cancel);
    let directory = root.to_path_buf();
    let (document_send, document_receive) = std::sync::mpsc::channel();
    let document = std::thread::spawn(move || {
        let result = xberg_runtime::request(
            &directory,
            json!({"command":"extract","path":directory.join("long.txt")}),
            Duration::from_secs(15),
            &token,
        );
        let _ = document_send.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("document-started").exists() {
        assert!(Instant::now() < deadline, "原文档请求未开始");
        std::thread::sleep(Duration::from_millis(5));
    }

    let control = review_remediation_open_broker_request(
        root,
        json!({"id":"review-remediation-unread-state","command":"broker-state"}),
    );
    // 不靠 sleep 推测服务端已持锁：Peek 确认响应已写出，客户端仍完全未读。
    review_remediation_wait_for_unread_response(&control);
    let directory = root.to_path_buf();
    let (snapshot_send, snapshot_receive) = std::sync::mpsc::channel();
    let snapshot = std::thread::spawn(move || {
        let result = xberg_runtime::request(
            &directory,
            json!({"command":"ocr_snapshot","image_base64":"fixture"}),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        );
        let _ = snapshot_send.send(result);
    });
    let snapshot_observed = snapshot_receive.recv_timeout(Duration::from_secs(3));
    let document_still_running = !document.is_finished();
    cancel.store(true, Ordering::Release);
    let terminal_observed = document_receive.recv_timeout(Duration::from_secs(3));

    // finally：先消费控制响应释放服务端 Flush，再释放夹具并清场；失败断言也不留
    // 下持锁连接、原文档或迟到的客户端交换线程。观测值均在清场前取得。
    let mut control_response = String::new();
    let consumed = std::io::BufReader::new(control).read_line(&mut control_response);
    std::fs::write(root.join("release-document"), b"release").unwrap();
    drop(shared);
    snapshot.join().unwrap();
    document.join().unwrap();

    consumed.unwrap();
    let control_response: serde_json::Value = serde_json::from_str(&control_response).unwrap();
    assert_eq!(control_response["id"], "review-remediation-unread-state");
    assert_eq!(control_response["ok"], true);
    let screenshot = snapshot_observed
        .expect("未读 broker-state 响应不得阻塞其他场景超过 3 秒")
        .expect("未读 broker-state 响应不得令截图请求失败");
    assert_eq!(screenshot["text"], "截图结果");
    assert_eq!(screenshot["jchtools_xberg_pid"], engine_pid);
    assert!(document_still_running, "截图应在原长文档仍执行时返回");
    let terminal = terminal_observed
        .expect("未读 broker-state 响应不得阻塞原文档取消超过 3 秒")
        .expect("原文档取消必须交付终态，不能耗尽取消确认期限");
    assert_eq!(terminal["ok"], false);
    assert_eq!(terminal["error_kind"], "cancelled", "{terminal}");
    assert_eq!(terminal["jchtools_xberg_pid"], engine_pid);
}

/// P3-16 / XB-17/XB-23：停止后的状态连接不能通过槽锁加入原终态交付屏障。
#[test]
fn review_remediation_post_stop_unread_broker_state_does_not_delay_retirement() {
    use std::io::BufRead;

    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    prepare(root);
    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(state["ok"], true, "合成引擎启动失败：{state}");
    let broker_pid = state["jchtools_broker_pid"].as_u64().unwrap();
    let engine_pid = state["jchtools_xberg_pid"].as_u64().unwrap();
    let shared = SharedProcess {
        broker_pid,
        engine_pid: Some(engine_pid),
    };
    let original = review_remediation_open_broker_request(
        root,
        json!({
            "id":"review-remediation-pre-stop-terminal",
            "command":"extract",
            "path":root.join("short.txt"),
            "timeout_ms":15000
        }),
    );
    review_remediation_wait_for_unread_response(&original);
    let stopped = xberg_runtime::background_control(true).unwrap();
    assert_eq!(stopped["ok"], true);
    assert_eq!(stopped["active"], 0, "只保留尚未消费的原终态");
    assert_eq!(stopped["stopping"], true);

    let control = review_remediation_open_broker_request(
        root,
        json!({"id":"review-remediation-post-stop-state","command":"broker-state"}),
    );
    review_remediation_wait_for_unread_response(&control);
    let mut terminal = String::new();
    std::io::BufReader::new(original)
        .read_line(&mut terminal)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut broker_alive = true;
    let mut engine_alive = true;
    while Instant::now() < deadline {
        broker_alive = process_alive(broker_pid, "jchtools.exe");
        engine_alive = process_alive(engine_pid, "xberg.exe");
        if !broker_alive && !engine_alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // finally：红测路径先读走状态响应解除 Flush；成功退出时管道已断开也合法。
    // 清场前保存退出观测，不能用关闭连接或 taskkill 冒充生产回收。
    let mut control_response = String::new();
    let consumed = std::io::BufReader::new(control).read_line(&mut control_response);
    drop(shared);
    if broker_alive {
        consumed.expect("失败清场仍须读走已写出的状态响应");
    }
    let terminal: serde_json::Value = serde_json::from_str(&terminal).unwrap();
    assert_eq!(terminal["id"], "review-remediation-pre-stop-terminal");
    assert_eq!(terminal["ok"], true);
    assert_eq!(terminal["document"]["content"], "document");
    assert!(!broker_alive, "停止后未读状态响应仍拖住代理退出");
    assert!(!engine_alive, "原终态交付后引擎未在 5 秒内回收");
}
