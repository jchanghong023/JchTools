use super::*;

/// 覆盖 U-12 / XB-17：共享代理不得持有客户端的捕获管道。
///
/// 缺陷回归（e2e-20261001-1）：Rust std 在 Windows 上即使把子进程 stdio 全部
/// 置为 null，CreateProcess 仍按可继承句柄链把祖父进程（cargo / PowerShell 的
/// `& cmd 2>&1` 捕获）的匿名管道写端传给代理；代理常驻期间上层捕获永远等不到
/// EOF，acceptance.ps1 的 cargo-test 阶段因此悬挂 40 分钟以上。修复要求代理
/// 启动即关闭继承的管道句柄。本测试用 re-exec 还原三层结构：测试进程捕获
/// mid 的 stdout，mid 以与运行时 connect() 相同的形状 spawn 代理后退出；
/// 断言 EOF 在 mid 退出后很快到达，且代理当时仍然存活（提前退出视为被
/// 既有引擎占用，按既有测试纪律清场后有界重试）。
// [quality-baseline approved 2026-10-03] 函数体内 zombie_processes 豁免（常驻代理测试
// 语义：不 wait、不 kill），经用户裁定保留；锚点置于函数外以免测试体哈希漂移。
#[test]
fn broker_does_not_hold_client_capture_pipes() {
    common::ensure_child_reaper();
    if std::env::var("JT_XBERG_PIPE_MID").is_ok() {
        // mid 层：与 xberg_runtime_windows::connect() 相同的 spawn 形状。
        // 代理必须比 mid 活得久（常驻语义）：不 wait、不 kill，pid 交外层清理。
        #[allow(clippy::zombie_processes)]
        let broker = Command::new(std::env::var("JCHTOOLS_TEST_BROKER_EXE").unwrap())
            .arg("--xberg-broker")
            .creation_flags(0x0800_0000)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(
            std::env::var("JT_XBERG_MID_PIDFILE").unwrap(),
            broker.id().to_string(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        return;
    }
    use std::{io::Read, sync::mpsc, thread};
    let _session = common::session_lock();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.join("state"));
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"));
    let pidfile = root.join("broker-pid.txt");
    std::env::set_var("JT_XBERG_MID_PIDFILE", &pidfile);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        common::cleanup_stray_engines();
        let _ = std::fs::remove_file(&pidfile);
        let mut mid = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shared_lanes::broker_does_not_hold_client_capture_pipes",
                "--nocapture",
            ])
            .env("JT_XBERG_PIPE_MID", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut output = mid.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = String::new();
            let result = output.read_to_string(&mut buffer);
            let _ = send.send(result);
        });
        let captured = receive.recv_timeout(Duration::from_secs(8));
        let mid_status = mid.wait().unwrap();
        let broker_pid = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if broker_pid != 0 && process_alive(broker_pid, "jchtools.exe") {
            let _ = Command::new("taskkill")
                .args(["/PID", &broker_pid.to_string(), "/F"])
                .output();
            if let Ok(result) = captured {
                result.expect("读取捕获管道失败不能视为 EOF");
                assert!(mid_status.success(), "中间层未正常退出：{mid_status}");
                // EOF 到达且代理仍常驻：句柄链已断开，缺陷修复成立。
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "共享代理持有了客户端捕获管道（EOF 未在 mid 退出后到达）"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn documents_reuse_process_and_snapshot_finishes_during_document() {
    common::ensure_child_reaper();
    // 会话锁：与其他引擎测试二进制互斥（生产语义每会话至多一个 Xberg）。
    // 只在非重入分支加锁/清场：重入子进程的父进程正持有锁，子进程再抢会互相等待。
    let session = if std::env::var_os("JCHTOOLS_SHARED_CLIENT_ROOT").is_none() {
        Some(common::session_lock())
    } else {
        None
    };
    if session.is_some() {
        common::cleanup_stray_engines();
    }
    if let Some(root) = std::env::var_os("JCHTOOLS_SHARED_CLIENT_ROOT") {
        let response = xberg_runtime::request(
            Path::new(&root),
            json!({"command":"snapshot_state"}),
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            response["jchtools_xberg_pid"].as_u64().unwrap().to_string(),
            std::env::var("JCHTOOLS_EXPECTED_XBERG_PID").unwrap()
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    prepare(root);
    // 引擎获取等待：每登录会话只允许一个 Xberg（进程扫描执法）。会话锁已保证
    // 与其他测试二进制互斥；若仍遇到「已有 Xberg」（残留孤儿），对拒绝做有界
    // 重试。忙碌应答携带的是占用者 pid，守卫只从成功应答构造。
    // XB-14：两个客户端在首次连接前同步起跑；先完成 state 再 spawn 只覆盖复用，
    // 无法检出首次启动竞争造成的第二个引擎。
    let startup = Arc::new(std::sync::Barrier::new(2));
    let concurrent_startup = startup.clone();
    let directory = root.to_path_buf();
    let concurrent = std::thread::spawn(move || {
        concurrent_startup.wait();
        xberg_runtime::request(
            &directory,
            json!({"command":"snapshot_state"}),
            Duration::from_secs(15),
            &AtomicBool::new(false),
        )
        .unwrap()
    });
    startup.wait();
    let session_deadline = Instant::now() + Duration::from_secs(60);
    let state = loop {
        let response = xberg_runtime::request(
            root,
            json!({"command":"snapshot_state"}),
            Duration::from_secs(15),
            &AtomicBool::new(false),
        )
        .unwrap();
        let busy = response["ok"] == false
            && response["error"]
                .as_str()
                .is_some_and(|error| error.contains("已有 Xberg"));
        if !busy {
            break response;
        }
        assert!(
            Instant::now() < session_deadline,
            "会话被既有 Xberg 占用超时（残留引擎未退出）：{response}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(state["ok"], true, "代理启动失败：{state}");
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let concurrent = concurrent.join().unwrap();
    assert_eq!(concurrent["ok"], true, "并发初始连接失败：{concurrent}");
    assert_eq!(
        concurrent["jchtools_broker_pid"],
        state["jchtools_broker_pid"]
    );
    assert_eq!(
        concurrent["jchtools_xberg_pid"],
        state["jchtools_xberg_pid"]
    );
    for _ in 0..2 {
        let result = markdown_document::convert(
            &root.join("short.txt"),
            root,
            "x_media",
            &Deadline::new(Duration::from_secs(30)),
        )
        .unwrap();
        assert_eq!(result.markdown, "document");
    }
    assert_eq!(
        std::fs::read_to_string(root.join("starts.txt"))
            .unwrap()
            .lines()
            .count(),
        1,
        "连续文档必须复用唯一 Xberg"
    );
    let pid = state["jchtools_xberg_pid"].clone();
    std::fs::write(root.join("long.txt"), "document").unwrap();
    let directory = root.to_path_buf();
    let document = std::thread::spawn(move || {
        markdown_document::convert(
            &directory.join("long.txt"),
            &directory,
            "x_media",
            &Deadline::new(Duration::from_secs(30)),
        )
    });
    let end = Instant::now() + Duration::from_secs(10);
    while !root.join("document-started").exists() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    let screenshot = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(screenshot["text"], "截图结果");
    assert_eq!(screenshot["jchtools_xberg_pid"], pid);
    assert!(!document.is_finished(), "截图必须在文档完成前返回");
    // 取消截图只等它自己的终态，不影响仍在运行的文档。
    let cancellation = Arc::new(AtomicBool::new(false));
    let token = cancellation.clone();
    let directory = root.to_path_buf();
    let pending_snapshot = std::thread::spawn(move || {
        xberg_runtime::request(
            &directory,
            json!({"command":"ocr_snapshot","image_base64":"wait"}),
            Duration::from_secs(5),
            &token,
        )
        .unwrap()
    });
    let end = Instant::now() + Duration::from_secs(5);
    while !root.join("snapshot-started").exists() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
    cancellation.store(true, Ordering::Release);
    assert_eq!(pending_snapshot.join().unwrap()["error_kind"], "cancelled");
    assert!(!document.is_finished());
    let timed_out = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"wait"}),
        Duration::from_millis(300),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(timed_out["error_kind"], "timeout");
    assert!(!document.is_finished());
    // 完全独立的客户端进程接入同一个引擎，退出后不释放代理。
    assert!(Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "shared_lanes::documents_reuse_process_and_snapshot_finishes_during_document",
            "--nocapture"
        ])
        .env("JCHTOOLS_SHARED_CLIENT_ROOT", root)
        .env(
            "JCHTOOLS_EXPECTED_XBERG_PID",
            pid.as_u64().unwrap().to_string()
        )
        .status()
        .unwrap()
        .success());
    std::fs::write(root.join("release-document"), "1").unwrap();
    assert!(document
        .join()
        .unwrap()
        .unwrap()
        .markdown
        .contains("document"));
    let media = xberg_runtime::request(
        root,
        json!({"command":"transcribe","path":root.join("short.txt")}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(media["markdown"], "media");
    assert_eq!(media["jchtools_xberg_pid"], pid);
    assert_eq!(
        std::fs::read_to_string(root.join("starts.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

/// O-16/XB-17：未请求取消的活跃截图仍受保护，显式强退不能意外终结它。
#[test]
fn force_exit_keeps_shared_engine_while_snapshot_task_active() {
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
    assert_eq!(state["ok"], true, "代理启动失败：{state}");
    let pid = state["jchtools_xberg_pid"].clone();
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };

    let cancellation = Arc::new(AtomicBool::new(false));
    let token = cancellation.clone();
    let directory = root.to_path_buf();
    let snapshot = std::thread::spawn(move || {
        xberg_runtime::request(
            &directory,
            json!({"command":"ocr_snapshot","image_base64":"wait"}),
            Duration::from_secs(15),
            &token,
        )
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("snapshot-started").exists() {
        assert!(Instant::now() < deadline, "模拟引擎未进入等待中的截图请求");
        std::thread::sleep(Duration::from_millis(5));
    }

    let rejected = xberg_runtime::force_background_exit().unwrap();
    assert_eq!(
        rejected["ok"], false,
        "截图尚未取消时强退不得终结共享引擎：{rejected}"
    );
    assert!(!snapshot.is_finished(), "拒绝强退不得中断截图请求");
    cancellation.store(true, Ordering::Release);
    let cancelled = snapshot.join().unwrap().unwrap();
    assert_eq!(cancelled["error_kind"], "cancelled");

    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(state["jchtools_xberg_pid"], pid);
    assert_eq!(xberg_runtime::force_background_exit().unwrap()["ok"], true);
}

/// 覆盖 XB-08/XB-17：取消 ACK 不等于原请求终态。取消接口若已确认但模拟引擎
/// 永不返回原 extract 终态，客户端必须在有界收尾时间内报错，同时代理继续保留
/// document lane，拒绝重复文档请求；snapshot lane 仍可用且复用同一引擎 PID。
#[test]
fn cancel_ack_without_terminal_keeps_document_lane_and_snapshot_alive() {
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
    assert_eq!(state["ok"], true);
    let pid = state["jchtools_xberg_pid"].clone();
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };

    std::fs::write(root.join("unresponsive.txt"), "document").unwrap();
    let directory = root.to_path_buf();
    let started = Instant::now();
    let unresponsive = std::thread::spawn(move || {
        xberg_runtime::request(
            &directory,
            json!({"command":"extract","path":directory.join("unresponsive.txt")}),
            Duration::from_millis(300),
            &AtomicBool::new(false),
        )
    });
    let marker_deadline = Instant::now() + Duration::from_secs(2);
    while !root.join("document-started").exists() {
        assert!(
            Instant::now() < marker_deadline,
            "模拟引擎未进入无响应 extract"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let rejected = xberg_runtime::request(
        root,
        json!({"command":"extract","path":root.join("short.txt")}),
        Duration::from_secs(3),
        &AtomicBool::new(false),
    )
    .expect("lane 拒绝应通过共享协议返回结构化失败");
    assert_eq!(rejected["ok"], false);
    assert!(rejected["error"]
        .as_str()
        .unwrap_or_default()
        .contains("上一个请求尚未确认结束"));

    let snapshot = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(3),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(snapshot["ok"], true);
    assert_eq!(snapshot["jchtools_xberg_pid"], pid);

    let result = unresponsive.join().unwrap();
    assert!(result.is_err(), "无响应 extract 不得伪报成功：{result:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        result.as_ref().err().is_some_and(
            |error| error.contains("未在 1 秒内结束") && error.contains("保留未结束任务")
        )
    );
}
