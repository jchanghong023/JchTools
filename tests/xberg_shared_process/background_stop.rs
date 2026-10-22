use super::*;

/// 覆盖 XB-05/XB-23：引擎终态已到、但客户端尚未读走时，停止后台不能丢弃响应。
/// 直接保留一个未读取的请求管道，确定性复现 pending 已清空、转发尚未结束的窗口。
#[test]
fn background_stop_waits_for_outstanding_terminal_delivery() {
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, Write};
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }

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
    let _shared = SharedProcess {
        broker_pid,
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let mut session = 0;
    // SAFETY: 当前进程 ID 有效，session 是有效输出指针。
    assert_ne!(
        unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) },
        0
    );
    let state_root = jchtools_foundation::xberg_settings::state_dir().unwrap();
    let digest = Sha256::digest(format!("{}:{session}", state_root.display()).as_bytes());
    let name = format!(r"\\.\pipe\jchtools-xberg-{}", hex::encode(digest));
    let mut pipe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name)
        .unwrap();
    let envelope = json!({
        "runtime_dir": std::fs::canonicalize(root).unwrap(),
        "request": {
            "id": "unread-terminal",
            "command": "extract",
            "path": root.join("short.txt"),
            "timeout_ms": 15000
        }
    });
    writeln!(pipe, "{envelope}").unwrap();
    pipe.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut available = 0;
        // SAFETY: pipe 持有有效客户端句柄；只查询缓冲字节数，不消费响应。
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
        assert_ne!(peeked, 0, "等待终态时管道中断");
        if available > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "引擎未交付待转发终态");
        std::thread::sleep(Duration::from_millis(5));
    }
    let stopped = xberg_runtime::background_control(true).unwrap();
    assert_eq!(stopped["ok"], true);
    assert_eq!(stopped["active"], 0, "引擎任务必须已结束，只有响应尚未读走");
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        process_alive(broker_pid, "jchtools.exe"),
        "停止看门狗不得在客户端读走终态之前退出代理"
    );
    let mut response = String::new();
    std::io::BufReader::new(pipe)
        .read_line(&mut response)
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["id"], "unread-terminal");
    assert_eq!(response["ok"], true);
    assert_eq!(response["document"]["content"], "document");
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(broker_pid, "jchtools.exe") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_alive(broker_pid, "jchtools.exe"),
        "终态读走后代理应完成退出"
    );
}

/// 覆盖 XB-05/XB-17/XB-23：停止前已经完成的终态必须交付，但停止后的不完整
/// 新连接不能延长后台寿命。真实公开代理入口、真实命名管道与 PID 退出观测；
/// 合成引擎仅替代模型，不替代代理的接入、停止或回收逻辑。
#[test]
fn review_fix_post_stop_incomplete_connections_do_not_delay_retirement() {
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, Write};
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }

    // 不经 request()/connect() 重连，避免退出后测试自己重新拉起代理。
    // 新实现可直接拒绝连接；成功打开的连接只写 JSON 前缀，绝不交付完整请求。
    let open_incomplete = |name: &str| -> Option<std::fs::File> {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(mut pipe) => match pipe.write_all(br#"{"request":"#) {
                Ok(()) => Some(pipe),
                Err(error) if matches!(error.raw_os_error(), Some(109 | 232 | 233)) => None,
                Err(error) => panic!("停止后的不完整连接写入异常：{error}"),
            },
            Err(error) if matches!(error.raw_os_error(), Some(2 | 109 | 231 | 232 | 233)) => None,
            Err(error) => panic!("停止后的原始管道连接异常：{error}"),
        }
    };

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
    let _shared = SharedProcess {
        broker_pid,
        engine_pid: Some(engine_pid),
    };
    let mut session = 0;
    // SAFETY: 当前进程 ID 有效，session 是有效输出指针。
    assert_ne!(
        unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) },
        0
    );
    let state_root = jchtools_foundation::xberg_settings::state_dir().unwrap();
    let digest = Sha256::digest(format!("{}:{session}", state_root.display()).as_bytes());
    let name = format!(r"\\.\pipe\jchtools-xberg-{}", hex::encode(digest));
    let mut original = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&name)
        .unwrap();
    let envelope = json!({
        "runtime_dir": std::fs::canonicalize(root).unwrap(),
        "request": {
            "id": "review-fix-original-terminal",
            "command": "extract",
            "path": root.join("short.txt"),
            "timeout_ms": 15000
        }
    });
    writeln!(original, "{envelope}").unwrap();
    original.flush().unwrap();
    let response_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut available = 0;
        // SAFETY: original 持有有效客户端句柄；查询缓冲，不消费原请求终态。
        let peeked = unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                original.as_raw_handle().cast(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &raw mut available,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(peeked, 0, "原请求终态交付前管道中断");
        if available > 0 {
            break;
        }
        assert!(Instant::now() < response_deadline, "原请求未返回终态");
        std::thread::sleep(Duration::from_millis(5));
    }
    let stopped = xberg_runtime::background_control(true).unwrap();
    assert_eq!(stopped["ok"], true, "停止未被接受：{stopped}");
    assert_eq!(stopped["stopping"], true);
    assert_eq!(stopped["active"], 0, "原引擎任务已结束，只剩未消费的终态");

    // 原终态保持未读期间开始接入；保留每个成功连接的客户端句柄，使它们的
    // 15s JSON 读取期限彼此重叠。拒绝连接也是合法结果，不要求停止后仍接纳请求。
    let mut incomplete = Vec::new();
    let hold_deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < hold_deadline {
        if let Some(pipe) = open_incomplete(&name) {
            incomplete.push(pipe);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        process_alive(broker_pid, "jchtools.exe"),
        "原终态尚未读走，代理不得提前退出"
    );
    assert!(
        process_alive(engine_pid, "xberg.exe"),
        "原终态尚未交付，停止不得提前回收引擎"
    );

    let mut response = String::new();
    let mut reader = std::io::BufReader::new(original);
    reader.read_line(&mut response).unwrap();
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["id"], "review-fix-original-terminal");
    assert_eq!(response["ok"], true);
    assert_eq!(response["document"]["content"], "document");
    drop(reader);

    // 与已有终态交付用例相同的 5s 退出界限，短于新连接自己的 15s 读期限。
    // 观察期间继续制造不完整连接，不能靠停下接入并等全部读期限耗尽而通过。
    let retirement_deadline = Instant::now() + Duration::from_secs(5);
    let mut broker_alive = true;
    let mut engine_alive = true;
    while Instant::now() < retirement_deadline {
        if let Some(pipe) = open_incomplete(&name) {
            incomplete.push(pipe);
        }
        broker_alive = process_alive(broker_pid, "jchtools.exe");
        engine_alive = process_alive(engine_pid, "xberg.exe");
        if !broker_alive && !engine_alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // 必须先观察生产回收，再由 SharedProcess 做失败清场；测试关闭不完整连接
    // 或守卫 taskkill 都不能冒充停止语义已经生效。
    assert!(
        !broker_alive,
        "终态已交付后，停止后的不完整连接仍拖住代理退出：pid={broker_pid}，连接数={}",
        incomplete.len()
    );
    assert!(
        !engine_alive,
        "全部原任务结束后，共享引擎未在界限内回收：pid={engine_pid}"
    );
}

/// 覆盖 XB-08/XB-17/XB-23：后台停止后仍须接受已登记任务的取消连接，
/// 安全结束原任务并交付终态；停止后的不完整新连接仍不得阻止代理回收。
#[test]
fn background_stop_preserves_inflight_request_cancellation() {
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
    let broker_pid = state["jchtools_broker_pid"].as_u64().unwrap();
    let engine_pid = state["jchtools_xberg_pid"].as_u64().unwrap();
    let _shared = SharedProcess {
        broker_pid,
        engine_pid: Some(engine_pid),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let token = Arc::clone(&cancel);
    let directory = root.to_path_buf();
    let (send, receive) = std::sync::mpsc::channel();
    let document = std::thread::spawn(move || {
        let result = xberg_runtime::request(
            &directory,
            json!({"command":"extract","path":directory.join("long.txt")}),
            Duration::from_secs(15),
            &token,
        );
        let _ = send.send(result);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("document-started").exists() {
        assert!(Instant::now() < deadline, "原请求未开始执行");
        std::thread::sleep(Duration::from_millis(5));
    }
    let stopped = xberg_runtime::background_control(true).unwrap();
    assert_eq!(stopped["ok"], true);
    assert_eq!(stopped["stopping"], true);
    assert_eq!(stopped["active"], 1, "停止时必须仍有原请求");
    cancel.store(true, Ordering::Release);
    let terminal = receive
        .recv_timeout(Duration::from_secs(3))
        .expect("后台停止后原请求的取消必须尽快结束");
    document.join().unwrap();
    let terminal = terminal.expect("后台停止不得拒绝原请求的取消连接");
    assert_eq!(terminal["ok"], false);
    assert_eq!(terminal["error_kind"], "cancelled", "{terminal}");
    assert_eq!(terminal["jchtools_xberg_pid"], engine_pid);
    assert_eq!(
        std::fs::read_to_string(root.join("starts.txt"))
            .unwrap()
            .lines()
            .count(),
        1,
        "停止与取消不得创建第二个引擎"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while (process_alive(broker_pid, "jchtools.exe") || process_alive(engine_pid, "xberg.exe"))
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_alive(broker_pid, "jchtools.exe"),
        "原请求安全结束后代理须退出"
    );
    assert!(
        !process_alive(engine_pid, "xberg.exe"),
        "原请求安全结束后引擎须回收"
    );
}
