use super::*;

/// 覆盖 T-23：单文件失败不能让后续文件永久失去继续处理的机会。
///
/// 缺陷回归（e2e-20261002-1，真实 502 文件批次）：超大文档的引擎响应超过代理
/// 单行消息上限（实测 204 MB > 140 MB），读取断裂置 broken；引擎进程本身健康
/// 常驻，child-exit 检查不触发，broken 又永不复位——其后每个文档请求立即失败，
/// 一轮批次中途 264 个文件连坐。模拟引擎对 `corrupt.txt` 写一行非法 JSON 后
/// 照常运行，复现「broken 但进程存活」现场；修复要求代理终结并重建引擎，
/// 下一个文件正常转换。
#[test]
fn broken_engine_is_replaced_and_batch_continues() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    prepare(root);
    std::fs::write(root.join("corrupt.txt"), "document").unwrap();
    let session_deadline = Instant::now() + Duration::from_secs(60);
    let broker_pid = loop {
        let response = xberg_runtime::request(
            root,
            json!({"command":"snapshot_state"}),
            Duration::from_secs(15),
            &AtomicBool::new(false),
        )
        .unwrap();
        if response["ok"] == true {
            break response["jchtools_broker_pid"].as_u64().unwrap();
        }
        assert!(
            Instant::now() < session_deadline,
            "会话被既有 Xberg 占用超时（残留引擎未退出）：{response}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    let _guard = SharedProcess {
        broker_pid,
        engine_pid: None,
    };
    // 命中坏响应：该文件必须失败（错误可区分），这是正确的单文件失败语义。
    let first = markdown_document::convert(
        &root.join("corrupt.txt"),
        root,
        "x_media",
        &Deadline::new(Duration::from_secs(60)),
    );
    assert!(first.is_err(), "坏响应必须让该文件失败：{first:?}");
    // ……但不得连坐：代理必须重建引擎，下一个文件正常转换。
    let second = markdown_document::convert(
        &root.join("short.txt"),
        root,
        "x_media",
        &Deadline::new(Duration::from_secs(60)),
    )
    .unwrap_or_else(|error| panic!("T-23 连坐：断裂后下一个文件失败：{error}"));
    assert_eq!(second.markdown, "document");
}

/// 覆盖 XB-08/XB-17（缺陷 S6-02）：引擎死亡后迟到的取消请求不得重新拉起引擎。
/// 取消的目标只存在于引擎内的在途任务；引擎槽位已空（进程退出或通信断裂
/// 终结后的空档、停止看门狗窗口）时目标必然已按失败终态交付，为一条取消
/// 重启引擎只会白付进程启动与模型加载，停止窗口内还会被看门狗立刻终结。
#[test]
fn late_cancel_after_engine_death_does_not_respawn_engine() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    prepare(root);
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
    let engine_pid = state["jchtools_xberg_pid"].as_u64().unwrap();
    let _guard = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: Some(engine_pid),
    };
    // 持有原进程句柄并等待 OS 确认退出，不能将映像查询失败当作死亡。
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, u32::try_from(engine_pid).unwrap()) };
    assert!(!handle.is_null(), "无法观察测试引擎：{}", std::io::Error::last_os_error());
    // SAFETY: 有效句柄仅转移一次并由 OwnedHandle 关闭。
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let killed = Command::new("taskkill")
        .args(["/PID", &engine_pid.to_string(), "/F"])
        .output()
        .unwrap();
    assert!(killed.status.success(), "终止测试引擎失败：{killed:?}");
    use std::os::windows::io::AsRawHandle;
    // SAFETY: 句柄在等待期间保持有效，只观察进程状态。
    assert_eq!(unsafe { WaitForSingleObject(handle.as_raw_handle(), 10_000) }, WAIT_OBJECT_0, "测试引擎未退出");
    // 迟到的取消：必须按「无事可取消」立即应答，不得进入 spawn 分支。
    let cancel = xberg_runtime::request(
        root,
        json!({"command":"cancel","target_id":"late-target"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        cancel["ok"], true,
        "引擎不在场时取消应按 noop 成功应答：{cancel}"
    );
    assert_eq!(
        cancel["accepted"], false,
        "无事可取消必须显式标记 accepted=false：{cancel}"
    );
    assert!(
        cancel.get("jchtools_xberg_pid").is_none(),
        "取消响应不得携带引擎 PID（未为此 spawn 引擎）：{cancel}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("starts.txt"))
            .unwrap()
            .lines()
            .count(),
        1,
        "取消请求不得重新拉起引擎（starts.txt 启动计数应保持 1）"
    );
}

/// 覆盖 P-10（缺陷 S6-06）：停止看门狗以 `std::process::exit(0)` 收尾会跳过
/// main 栈上日志句柄的析构，非阻塞写入线程缓冲中的关键记录（引擎终结、退出
/// 事件本身）随之丢失。修复要求退出路径显式刷盘：代理进程消亡后，退出前写入
/// 的记录必须已经在磁盘日志文件中，而不是停留在内存缓冲里。
#[test]
fn watchdog_exit_flushes_diagnostic_log_before_process_exit() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    prepare(root);
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
    let broker_pid = state["jchtools_broker_pid"].as_u64().unwrap();
    let engine_pid = state["jchtools_xberg_pid"].as_u64();
    let _shared = SharedProcess {
        broker_pid,
        engine_pid,
    };
    // 触发托盘统一退出（XB-23）：无在途任务时看门狗应终结引擎并退出代理。
    // 停止确认与 background_settings 的 stop() 同口径：看门狗最快 100ms 后
    // 退出，响应先于退出写回；管道竞态时有界重试（代理消失后按未运行应答）。
    let stop_deadline = Instant::now() + Duration::from_secs(10);
    let stop = loop {
        match xberg_runtime::background_control(true) {
            Ok(response) => break response,
            Err(error) => {
                assert!(Instant::now() < stop_deadline, "停止请求持续失败：{error}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert_eq!(stop["ok"], true, "停止请求被拒绝：{stop}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(broker_pid, "jchtools.exe") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !process_alive(broker_pid, "jchtools.exe"),
        "停止后代理进程必须退出"
    );
    // P-10：退出前写入的关键记录必须已经落盘（exit 前显式刷盘），不能停留在
    // 非阻塞写入线程的缓冲里随 exit(0) 整批丢失。
    let log_dir = root.join("state").join("logs");
    let mut logs = String::new();
    for entry in std::fs::read_dir(&log_dir).unwrap().flatten() {
        logs.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
    }
    let broker_pid_field = format!("pid={broker_pid}");
    let broker_record = |event: &str| {
        logs.lines().find(|line| {
            line.split_ascii_whitespace()
                .any(|field| field == broker_pid_field)
                && line
                    .split_ascii_whitespace()
                    .any(|field| field.strip_prefix("event=") == Some(event))
        })
    };
    assert!(
        broker_record("xberg_broker_listening").is_some(),
        "日志应含代理启动记录（证明初始化本身生效）：目录 {log_dir:?}；{logs}"
    );
    let engine_pid = engine_pid.expect("此退出场景必须启动真实引擎");
    let engine_pid_field = format!("engine_pid={engine_pid}");
    let engine_exit = broker_record("xberg_engine_exited")
        .unwrap_or_else(|| panic!("退出前的引擎终态必须落盘（P-10）：{logs}"));
    assert!(
        engine_exit
            .split_ascii_whitespace()
            .any(|field| field == engine_pid_field)
            && engine_exit
                .split_ascii_whitespace()
                .any(|field| field == "reason=\"stop_watchdog\"")
            && engine_exit
                .split_ascii_whitespace()
                .any(|field| field.starts_with("exit_code=")),
        "引擎终态须归属本次看门狗回收的真实进程并包含退出码：{logs}"
    );
    let stopped = broker_record("xberg_broker_stopped")
        .unwrap_or_else(|| panic!("代理退出事件自身必须落盘（P-10）：{logs}"));
    assert!(
        stopped
            .split_ascii_whitespace()
            .any(|field| field == "exit_code=0")
            && broker_record("application_stopped").is_some(),
        "代理实际退出前必须完成日志生命周期并记录退出结果：{logs}"
    );
    // XB-23：退出须实际回收本应用拥有的引擎；守卫只负责失败清场，不能把泄漏
    // 强杀后计为成功。
    let deadline = Instant::now() + Duration::from_secs(8);
    while process_alive(engine_pid, "xberg.exe") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !process_alive(engine_pid, "xberg.exe"),
        "后台退出后共享引擎仍存活：pid={engine_pid}"
    );
}

/// 覆盖（孤儿回收回归；缺陷 2026-10-03 两次复现：broker 无自退条件、测试清场
/// 为扫描式可漏杀，漏杀者存活并锁住构建产物）：子测试进程以 connect() 同形
/// 拉起常驻代理后立即退出且不做任何清理——模拟全部漏杀路径的公共形态。
/// 派生进程的退出必须使代理随之消亡，不得存活到测试二进制之外。
#[test]
fn spawned_broker_reaped_when_spawning_process_exits() {
    common::ensure_child_reaper();
    if std::env::var_os("JT_XBERG_ORPHAN_CLIENT").is_some() {
        // 子层：拉起代理、落 pid、立即退出、不清理（pid 交外层观测）。
        let broker = Command::new(std::env::var_os("JCHTOOLS_TEST_BROKER_EXE").unwrap())
            .arg("--xberg-broker")
            .creation_flags(0x0800_0000)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pidfile = std::env::var_os("JT_XBERG_ORPHAN_PIDFILE").unwrap();
        std::fs::write(pidfile, broker.id().to_string()).unwrap();
        std::process::exit(0);
    }
    // 外层：与其他引擎测试二进制互斥（cleanup_stray_engines 会按命令行特征扫杀，
    // 并发清场会让断言空转通过）。
    let _session = common::session_lock();
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let pidfile = temp.path().join("orphan-broker.pid");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "engine_lifecycle::spawned_broker_reaped_when_spawning_process_exits",
            "--nocapture",
        ])
        .env("JT_XBERG_ORPHAN_CLIENT", "1")
        .env("JT_XBERG_ORPHAN_PIDFILE", &pidfile)
        .env("JCHTOOLS_TEST_STATE_DIR", &state)
        .env("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "子层测试进程必须正常退出：{status:?}");
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap_or_else(|error| panic!("子层未落 pidfile：{error}"))
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(u64::from(pid), "jchtools.exe") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !process_alive(u64::from(pid), "jchtools.exe"),
        "派生代理在子测试进程退出后仍存活（孤儿泄漏复现）：pid={pid}"
    );
}
