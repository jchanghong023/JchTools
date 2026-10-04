#![allow(clippy::unwrap_used)]
//! 覆盖 XB-14/XB-15/XB-17：真实代理进程及管道，模拟引擎（不代表真实模型验收）。

mod common;
use jchtools::{
    markdown_document::{self, Deadline},
    xberg_runtime,
};
use serde_json::json;
use std::{
    os::windows::process::CommandExt,
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn prepare(root: &Path) {
    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &root.join("xberg.exe"));
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.join("state"));
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"));
    jchtools::xberg_settings::save(root).unwrap();
    std::fs::write(root.join("short.txt"), "document").unwrap();
}

/// 测试守卫：同时强制结束本测试启动的共享代理与引擎。引擎是代理的子进程，
/// 但 fixture 引擎可能经句柄继承与代理脱钩，只杀代理会留下占住会话单引擎
/// 执法的孤儿（生产引擎 run53.1 已按 P1 随 stdio 断开自退，此处兜底测试进程）。
struct SharedProcess {
    broker_pid: u64,
    engine_pid: Option<u64>,
}

/// 代理是否仍存活（tasklist 查询失败视为存活，交由后续超时兜底）。
fn process_alive(pid: u64) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map_or(true, |output| {
            String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
        })
}

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
                "broker_does_not_hold_client_capture_pipes",
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
            let _ = output.read_to_string(&mut buffer);
            let _ = send.send(());
        });
        let eof_in_time = receive.recv_timeout(Duration::from_secs(8)).is_ok();
        let _ = mid.wait();
        let broker_pid = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if broker_pid != 0 && process_alive(broker_pid) {
            let _ = Command::new("taskkill")
                .args(["/PID", &broker_pid.to_string(), "/F"])
                .output();
            if eof_in_time {
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
impl Drop for SharedProcess {
    fn drop(&mut self) {
        let _ = Command::new("taskkill")
            .args(["/PID", &self.broker_pid.to_string(), "/F"])
            .output();
        if let Some(pid) = self.engine_pid {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .output();
        }
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
    // 同时抢占初始连接：代理可能启动竞争，但只有一个代理可创建引擎。
    let directory = root.to_path_buf();
    let concurrent = std::thread::spawn(move || {
        xberg_runtime::request(
            &directory,
            json!({"command":"snapshot_state"}),
            Duration::from_secs(15),
            &AtomicBool::new(false),
        )
        .unwrap()
    });
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    assert_eq!(state["ok"], true, "代理启动失败：{state}");
    assert_eq!(
        concurrent.join().unwrap()["jchtools_xberg_pid"],
        state["jchtools_xberg_pid"]
    );
    for _ in 0..2 {
        let result = markdown_document::convert(
            &root.join("short.txt"),
            root,
            false,
            "x_media",
            &Deadline::new(Duration::from_secs(30)),
        )
        .unwrap();
        assert_eq!(result.markdown, "document\n");
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
            true,
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
            "documents_reuse_process_and_snapshot_finishes_during_document",
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
        false,
        "x_media",
        &Deadline::new(Duration::from_secs(60)),
    );
    assert!(first.is_err(), "坏响应必须让该文件失败：{first:?}");
    // ……但不得连坐：代理必须重建引擎，下一个文件正常转换。
    let second = markdown_document::convert(
        &root.join("short.txt"),
        root,
        false,
        "x_media",
        &Deadline::new(Duration::from_secs(60)),
    )
    .unwrap_or_else(|error| panic!("T-23 连坐：断裂后下一个文件失败：{error}"));
    assert_eq!(second.markdown, "document\n");
}

/// 覆盖 T-18/T-24：引擎能力清单声称支持 fast、实际 extract 拒绝时，客户端
/// 不得把该文件记为永久失败；按常规模式重试（全能力，严格强于快速模式），
/// 并以日志留痕。真实批次（e2e-20261003-2）：Lander v1.4/v2 两份 200+ 页
/// 文档因此失败。
#[test]
fn fast_mode_rejection_falls_back_to_normal() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    prepare(root);
    std::fs::write(root.join("nofast.txt"), "document").unwrap();
    let result = markdown_document::convert(
        &root.join("nofast.txt"),
        root,
        true,
        "x_media",
        &Deadline::new(Duration::from_secs(60)),
    )
    .unwrap_or_else(|error| panic!("快速模式被拒后必须按常规模式成功：{error}"));
    assert_eq!(
        result.markdown,
        "document
"
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
            "spawned_broker_reaped_when_spawning_process_exits",
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
    while process_alive(u64::from(pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !process_alive(u64::from(pid)),
        "派生代理在子测试进程退出后仍存活（孤儿泄漏复现）：pid={pid}"
    );
}
