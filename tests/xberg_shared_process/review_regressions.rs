use super::*;

fn review_wait_for_marker(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "合成进程未进入请求：{}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// XB-05/XB-15/XB-17：64 个同时占用的管道实例是局部容量，不是代理致命错误。
#[test]
fn review_f36_pipe_capacity_does_not_kill_broker_or_inflight_document() {
    use sha2::{Digest, Sha256};
    use std::io::BufRead;
    #[link(name = "kernel32")]
    extern "system" {
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }

    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    prepare(root);
    let state = shared_state(root);
    let broker_pid = state["jchtools_broker_pid"].as_u64().unwrap();
    let engine_pid = state["jchtools_xberg_pid"].as_u64().unwrap();
    let _shared = SharedProcess {
        broker_pid,
        engine_pid: Some(engine_pid),
    };
    std::fs::write(root.join("long.txt"), b"document").unwrap();
    let document = review_remediation_open_broker_request(
        root,
        json!({
            "id":"review-f36-document","command":"extract",
            "path":root.join("long.txt"),"timeout_ms":60000
        }),
    );
    review_wait_for_marker(&root.join("document-started"));
    let mut session = 0;
    // SAFETY: 当前进程 ID 有效，session 指向可写输出。
    assert_ne!(
        unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) },
        0
    );
    let state_root = jchtools_foundation::xberg_settings::state_dir().unwrap();
    let digest = Sha256::digest(format!("{}:{session}", state_root.display()).as_bytes());
    let name = format!(r"\\.\pipe\jchtools-xberg-{}", hex::encode(digest));
    let mut held = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    // 原文档占一个实例；另 63 个连接不写完整 JSON，不受 pending 队列上限影响。
    while held.len() < 63 {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(pipe) => held.push(pipe),
            Err(error) => {
                assert!(Instant::now() < deadline, "未达到 64 活跃实例：{error}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    // 等待 listener 接受最后一个连接并尝试分配下一个实例。
    std::thread::sleep(Duration::from_millis(300));
    let broker_survived = process_alive(broker_pid, "jchtools.exe");
    let engine_survived = process_alive(engine_pid, "xberg.exe");
    drop(held);
    std::fs::write(root.join("release-document"), b"1").unwrap();
    let mut terminal = String::new();
    let delivered = std::io::BufReader::new(document).read_line(&mut terminal);
    assert!(broker_survived, "64 实例容量耗尽不能使共享代理退出");
    assert!(engine_survived, "管道容量耗尽不能杀死其他场景的共享引擎");
    delivered.expect("原文档终态必须仍能交付");
    let terminal: serde_json::Value = serde_json::from_str(&terminal).unwrap();
    assert_eq!(terminal["ok"], true);
    assert_eq!(terminal["document"]["content"], "document");
    assert_eq!(terminal["jchtools_xberg_pid"], engine_pid);
    let snapshot = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(snapshot["text"], "截图结果");
    assert_eq!(snapshot["jchtools_xberg_pid"], engine_pid);
    assert_eq!(snapshot["jchtools_broker_pid"], broker_pid);
}

/// O-16/XB-17/XB-23：取消超 10 秒仍未终态的截图可显式强退，但文档/媒体必须安全结束。
#[test]
fn review_f39_force_exit_allows_only_cancelled_hung_snapshot_and_protects_other_lanes() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    prepare(root);
    review_engine_copy(root);
    let state = shared_state(root);
    let broker_pid = state["jchtools_broker_pid"].as_u64().unwrap();
    let engine_pid = state["jchtools_xberg_pid"].as_u64().unwrap();
    let _shared = SharedProcess {
        broker_pid,
        engine_pid: Some(engine_pid),
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let token = cancellation.clone();
    let directory = root.to_path_buf();
    let snapshot = std::thread::spawn(move || {
        xberg_runtime::request(
            &directory,
            json!({"command":"ocr_snapshot","image_base64":"wait"}),
            Duration::from_secs(60),
            &token,
        )
    });
    review_wait_for_marker(&root.join("snapshot-started"));
    let cancel_started = Instant::now();
    cancellation.store(true, Ordering::Release);
    review_wait_for_marker(&root.join("cancel-started"));
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!snapshot.is_finished(), "取消 ACK 不能释放挂起截图原请求");
    for (command, extension, marker) in [
        ("extract", "txt", "document-started"),
        ("transcribe", "mp4", "media-started"),
    ] {
        let release = root.join("release-document");
        if release.exists() {
            std::fs::remove_file(&release).unwrap();
        }
        let input = root.join(format!("long.{extension}"));
        std::fs::write(&input, b"synthetic input").unwrap();
        let directory = root.to_path_buf();
        let other = std::thread::spawn(move || {
            xberg_runtime::request(
                &directory,
                json!({"command":command,"path":input}),
                Duration::from_secs(60),
                &AtomicBool::new(false),
            )
        });
        review_wait_for_marker(&root.join(marker));
        let rejected = xberg_runtime::force_background_exit().unwrap();
        let still_running = !other.is_finished();
        std::fs::write(&release, b"1").unwrap();
        let completed = other.join().unwrap().unwrap();
        assert_eq!(rejected["ok"], false, "不能强杀仍有 {command} 的共享引擎");
        assert!(still_running, "拒绝强退不能提前结束其他场景");
        assert_eq!(completed["ok"], true);
        assert_eq!(completed["jchtools_xberg_pid"], engine_pid);
    }
    if let Some(remaining) = Duration::from_secs(10).checked_sub(cancel_started.elapsed()) {
        std::thread::sleep(remaining);
    }
    let before_force = xberg_runtime::background_control(false).unwrap();
    assert_eq!(before_force["active"], 1, "只剩取消后仍挂起的截图原请求");
    let forced = xberg_runtime::force_background_exit().unwrap();
    assert_eq!(
        forced["ok"], true,
        "显式强退必须能够终结仅剩的挂起截图：{forced}"
    );
    let terminal = snapshot.join().unwrap().unwrap();
    assert_eq!(terminal["ok"], false, "强退原请求只能交付失败终态");
    assert_eq!(terminal["error_kind"], "process_exited");
    let deadline = Instant::now() + Duration::from_secs(5);
    while (process_alive(broker_pid, "jchtools.exe") || process_alive(engine_pid, "xberg.exe"))
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_alive(broker_pid, "jchtools.exe"),
        "强退须回收本应用代理"
    );
    assert!(
        !process_alive(engine_pid, "xberg.exe"),
        "强退须回收本应用挂起引擎"
    );
}

/// O-34/O-29/P-10/XB-04：真实共享代理启动的引擎不能继承截图内容落盘开关。
#[test]
fn review_f43_engine_does_not_inherit_snapshot_debug_crops() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    prepare(root);
    review_engine_copy(root);
    let debug = ReviewEnvironment::set("XBERG_SNAPSHOT_DEBUG_CROPS", &root.join("private-crops"));
    let state = shared_state(root);
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    drop(debug);
    // 仅观测子进程环境位，不生成截图、正文或假转储文件，不冒充真实模型隐私验收。
    assert_eq!(
        state["debug_crops_inherited"], false,
        "Engine::spawn 必须清除 XBERG_SNAPSHOT_DEBUG_CROPS"
    );
}

/// F38/O-19：取消确认与模型 Ready 不解除截图占用，超时同样等待原请求终态。
#[test]
fn review_f38_snapshot_cancel_and_timeout_wait_for_original_terminal() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    prepare(root);
    review_engine_copy(root);
    let state = shared_state(root);
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    for timed_out in [false, true] {
        for name in ["snapshot-started", "cancel-started", "release-snapshot"] {
            let path = root.join(name);
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
        }
        let cancellation = Arc::new(AtomicBool::new(false));
        let token = Arc::clone(&cancellation);
        let directory = root.to_path_buf();
        let snapshot = std::thread::spawn(move || {
            xberg_runtime::request(
                &directory,
                json!({"command":"ocr_snapshot","image_base64":"wait"}),
                if timed_out {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(60)
                },
                &token,
            )
        });
        review_wait_for_marker(&root.join("snapshot-started"));
        if !timed_out {
            cancellation.store(true, Ordering::Release);
        }
        review_wait_for_marker(&root.join("cancel-started"));
        std::thread::sleep(Duration::from_millis(1200));
        let waiting = !snapshot.is_finished();
        let ready = shared_state(root);
        let rejected = xberg_runtime::request(
            root,
            json!({"command":"ocr_snapshot","image_base64":"second"}),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
        let document = xberg_runtime::request(
            root,
            json!({"command":"extract","path":root.join("short.txt")}),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
        // 先释放原调用并回收线程，再断言，避免失败路径遗留挂起请求。
        std::fs::write(root.join("release-snapshot"), b"1").unwrap();
        let terminal = snapshot.join().unwrap().unwrap();
        assert!(waiting, "取消 ACK 超过1秒仍不是真实终态");
        assert_eq!(ready["state"], "ready", "已加载模型 Ready 不能代表截图空闲");
        assert_eq!(rejected["ok"], false, "原请求未结束时不得进入下一截图");
        assert_eq!(document["ok"], true, "截图取消不得阻塞文档 lane");
        assert_eq!(document["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
        assert_eq!(terminal["ok"], false);
        assert_eq!(
            terminal["error_kind"],
            if timed_out { "timeout" } else { "cancelled" }
        );
        let next = xberg_runtime::request(
            root,
            json!({"command":"ocr_snapshot","image_base64":"next"}),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(next["ok"], true, "原请求终态后才释放截图 lane");
        assert_eq!(next["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    }
}
