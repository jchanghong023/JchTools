#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#![allow(clippy::unwrap_used)]
//! 覆盖 XB-14/XB-15/XB-17：真实代理进程及管道，模拟引擎（不代表真实模型验收）。

mod common;
use jchtools_markdown::markdown_document::{self, Deadline};
use jchtools_xberg::xberg_runtime;
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
    jchtools_foundation::xberg_settings::save(root).unwrap();
    std::fs::write(root.join("short.txt"), "document").unwrap();
}

/// 覆盖 T-23/T-25/XB-15/XB-17：从批处理公开入口停止当前文件，保留已完成
/// 结果且不开始下一文件；截图及唯一共享引擎继续可用。合成引擎只用于协议
/// 回归，不替代真实 GUI / 模型验收。超时后释放夹具并收尾，避免反证遗留进程。
fn assert_batch_stop_cancels_current_file(media: bool) {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    prepare(root);
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if relative != "xberg.exe" {
            let target = root.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    jchtools_markdown::markdown_assets::save_runtime_dir(root).unwrap();
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    let extension = if media { "mp4" } else { "txt" };
    for name in ["01-complete", "02-long", "03-next"] {
        std::fs::write(
            input.join(format!("{name}.{extension}")),
            b"synthetic input",
        )
        .unwrap();
    }
    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(state["ok"], true, "合成引擎启动失败：{state}");
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let options = jchtools_markdown::markdown::Options {
        input_dir: input,
        output_dir: output.clone(),
        flat: false,
        groups: vec![if media {
            jchtools_markdown::markdown::FormatGroup::Media
        } else {
            jchtools_markdown::markdown::FormatGroup::Other
        }],
        timeout_secs: 15,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let token = cancel.clone();
    let started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = started.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let batch = std::thread::spawn(move || {
        let result = jchtools_markdown::markdown::run(&options, &token, |event| {
            if let jchtools_markdown::markdown::Event::FileStarted { relative, .. } = event {
                observed.lock().unwrap().push(relative);
            }
        });
        send.send(result).unwrap();
    });
    let marker = root.join(if media {
        "media-started"
    } else {
        "document-started"
    });
    // CI runner 在整套测试并行进程压力下，批处理初始化（能力握手/资产就绪）
    // 可能明显慢于本地；等待窗口与本文件其他会话等待一致放宽到 60s。批处理若
    // 在当前文件开始前提前结束（run() 提前返回），立即报告其结果而非盲等；
    // T-23 的「停止必须尽快生效」断言在标记出现之后仍按 3s 严格计时。
    let start_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if marker.exists() {
            break;
        }
        if let Ok(early) = receive.try_recv() {
            panic!(
                "批次在当前文件开始前结束：{early:?}（已观察 FileStarted：{:?}）",
                started.lock().unwrap()
            );
        }
        assert!(
            Instant::now() < start_deadline,
            "批次未开始当前文件（已观察 FileStarted：{:?}）",
            started.lock().unwrap(),
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let completed = output.join(format!("01-complete_{extension}.md"));
    let completed_bytes = std::fs::read(&completed).unwrap();
    let screenshot = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        screenshot["jchtools_xberg_pid"],
        state["jchtools_xberg_pid"]
    );
    cancel.store(true, Ordering::Release);
    let first = receive.recv_timeout(Duration::from_secs(3));
    let prompt = first.is_ok();
    let result = if let Ok(result) = first {
        result
    } else {
        // 修复前失败时也先释放夹具、回收线程和进程，再报告反证。
        std::fs::write(root.join("release-document"), b"release").unwrap();
        receive.recv_timeout(Duration::from_secs(15)).unwrap()
    };
    batch.join().unwrap();
    let after = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(after["text"], "截图结果");
    assert_eq!(after["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    assert!(
        prompt,
        "停止请求必须尽快取消当前文件，不得等待当前文件自然结束"
    );
    let summary = result.unwrap();
    assert!(summary.stopped);
    assert_eq!(summary.success, 1, "取消的当前文件不得计为成功");
    assert_eq!(summary.failed, 0, "用户取消不得计为失败");
    assert_eq!(std::fs::read(completed).unwrap(), completed_bytes);
    assert!(!output.join(format!("02-long_{extension}.md")).exists());
    assert!(!output.join(format!("02-long_{extension}_media")).exists());
    assert!(!output.join(format!("03-next_{extension}.md")).exists());
    assert_eq!(started.lock().unwrap().len(), 2, "停止后不得开始下一文件");
}

/// T-03 平台门槛：转 Markdown 仅支持 Windows 11 x64 工作站（platform_preflight
/// 按契约拒绝 Server/旧构建）。CI 的 GitHub windows runner 是 Windows Server，
/// 批处理在该环境无法按契约执行；这三个协议回归测试据此跳过，不伪造覆盖。
/// 被跳过的行为（T-23/T-25 批处理停止）在受支持平台（Windows 11 主机）由
/// fulltest/slowtest 的同一测试保持覆盖。
fn skip_when_platform_unsupported() -> bool {
    match jchtools_markdown::markdown::page_readiness() {
        Err(message) if message.contains("Windows 11 x64") => {
            eprintln!("skip：{message}（CI runner 不在 T-03 支持平台内；覆盖见 Windows 11 主机的本地测试门）");
            true
        }
        _ => false,
    }
}

// 覆盖 T-23/T-25/XB-15/XB-17。
#[test]
fn batch_stop_cancels_current_document_and_keeps_shared_snapshot_alive() {
    if skip_when_platform_unsupported() {
        return;
    }
    assert_batch_stop_cancels_current_file(false);
}

// 覆盖 T-19/T-23/T-25/XB-15/XB-17。
#[test]
fn batch_stop_cancels_current_media_and_keeps_shared_snapshot_alive() {
    if skip_when_platform_unsupported() {
        return;
    }
    assert_batch_stop_cancels_current_file(true);
}

/// 覆盖 T-23/T-24/XB-17：用户停止时，取消确认不能冒充原请求已经结束；
/// 未结束诊断必须经过批处理公开入口保留，停止后不开始下一文件，截图仍可用。
#[test]
fn batch_stop_preserves_unfinished_request_diagnostic_and_snapshot_alive() {
    if skip_when_platform_unsupported() {
        return;
    }
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    prepare(root);
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let relative = member["path"].as_str().unwrap();
        if relative != "xberg.exe" {
            let target = root.join(relative);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, b"synthetic asset").unwrap();
        }
    }
    jchtools_markdown::markdown_assets::save_runtime_dir(root).unwrap();
    let input = root.join("input");
    let output = root.join("output");
    std::fs::create_dir(&input).unwrap();
    std::fs::create_dir(&output).unwrap();
    for name in ["01-unresponsive.txt", "02-next.txt"] {
        std::fs::write(input.join(name), b"synthetic input").unwrap();
    }
    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(state["ok"], true);
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let options = jchtools_markdown::markdown::Options {
        input_dir: input,
        output_dir: output.clone(),
        flat: false,
        groups: vec![jchtools_markdown::markdown::FormatGroup::Other],
        timeout_secs: 15,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let token = cancel.clone();
    let (send, receive) = std::sync::mpsc::channel();
    let batch = std::thread::spawn(move || {
        let mut events = Vec::new();
        let result = jchtools_markdown::markdown::run(&options, &token, |event| events.push(event));
        send.send((result, events)).unwrap();
    });
    // 同批处理标记等待口径：提前结束立即报告批处理结果，慢负载由 60s 窗口吸收。
    let start_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if root.join("document-started").exists() {
            break;
        }
        if let Ok(early) = receive.try_recv() {
            panic!("批次在无响应请求开始前结束：{early:?}");
        }
        assert!(Instant::now() < start_deadline, "批次未进入无响应请求");
        std::thread::sleep(Duration::from_millis(10));
    }
    cancel.store(true, Ordering::Release);
    let (result, events) = receive.recv_timeout(Duration::from_secs(3)).unwrap();
    batch.join().unwrap();
    let after = xberg_runtime::request(
        root,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(after["text"], "截图结果");
    assert_eq!(after["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    let summary = result.unwrap();
    assert!(summary.stopped);
    assert_eq!(
        summary.failed, 1,
        "取消接口已确认但原请求未结束必须保留为异常"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            jchtools_markdown::markdown::Event::FileFinished { success: false, message, .. }
                if message.contains("未在 1 秒内结束") && message.contains("保留未结束任务")
        )),
        "必须向界面保留原请求未结束的具体诊断：{events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            jchtools_markdown::markdown::Event::FileCancelled { .. }
        )),
        "取消失败不得冒充普通取消"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                jchtools_markdown::markdown::Event::FileStarted { .. }
            ))
            .count(),
        1,
        "停止后不得继续下一文件"
    );
    assert_eq!(
        std::fs::read_dir(output).unwrap().count(),
        0,
        "不得留下未完成结果"
    );
}

/// 测试守卫：同时强制结束本测试启动的共享代理与引擎。引擎是代理的子进程，
/// 但 fixture 引擎可能经句柄继承与代理脱钩，只杀代理会留下占住会话单引擎
/// 执法的孤儿（生产引擎 run53.1 已按 P1 随 stdio 断开自退，此处兜底测试进程）。
struct SharedProcess {
    broker_pid: u64,
    engine_pid: Option<u64>,
}

/// 进程是否仍存活：句柄直查退出码并校验映像名，不派生 tasklist。
///
/// 旧实现经 tasklist 文本 contains(pid) 判活，在整门并行高压下有两类假阳性
/// （派生 tasklist 失败按存活处理、其他进程 PID 数字子串命中），曾使
/// spawned_broker_reaped_when_spawning_process_exits 误报孤儿泄漏；句柄直查
/// 零派生、精确匹配，映像名校验排除 PID 复用窗口。image 传小写可执行文件名。
fn process_alive(pid: u64, image: &str) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW,
    };
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    let Some(pid) = u32::try_from(pid).ok() else {
        return false;
    };
    // SAFETY: 仅请求查询权限打开待观测进程；空句柄即进程已不可查询。
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut exit_code = 0u32;
    let mut path = [0u16; 512];
    let mut length = u32::try_from(path.len()).unwrap_or(0);
    // SAFETY: process 为有效查询句柄；退出码与映像名缓冲均为本栈内存。
    let queried =
        unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &raw mut length) };
    let terminated = unsafe { GetExitCodeProcess(process, &raw mut exit_code) } == 0
        || exit_code != STILL_ACTIVE;
    // SAFETY: 查询句柄用毕即关，不跨调用保留。
    unsafe { CloseHandle(process) };
    !terminated
        && queried != 0
        && String::from_utf16_lossy(&path[..length as usize])
            .to_lowercase()
            .ends_with(image)
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
    // 模拟引擎异常死亡（外部终结）；代理侧槽位在下一请求到达时才惰性清理。
    let _ = Command::new("taskkill")
        .args(["/PID", &engine_pid.to_string(), "/F"])
        .output()
        .unwrap();
    let gone = Instant::now() + Duration::from_secs(10);
    while process_alive(engine_pid, "xberg.exe") && Instant::now() < gone {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !process_alive(engine_pid, "xberg.exe"),
        "测试前提：模拟引擎应已被终结"
    );
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
    while process_alive(u64::from(pid), "jchtools.exe") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        !process_alive(u64::from(pid), "jchtools.exe"),
        "派生代理在子测试进程退出后仍存活（孤儿泄漏复现）：pid={pid}"
    );
}

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
    assert!(!process_alive(broker_pid, "jchtools.exe"), "终态读走后代理应完成退出");
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
    while (process_alive(broker_pid, "jchtools.exe") || process_alive(engine_pid, "xberg.exe")) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!process_alive(broker_pid, "jchtools.exe"), "原请求安全结束后代理须退出");
    assert!(!process_alive(engine_pid, "xberg.exe"), "原请求安全结束后引擎须回收");
}

// P3-16 / XB-08/XB-15/XB-17/XB-23：真实客户端可保留已写出但未消费的控制响应。
fn review_remediation_open_broker_request(
    root: &Path,
    request: serde_json::Value,
) -> std::fs::File {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    #[link(name = "kernel32")]
    extern "system" {
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }

    let mut session = 0;
    // SAFETY: 当前进程 ID 有效，session 指向有效的输出位置。
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
        "request": request
    });
    writeln!(pipe, "{envelope}").unwrap();
    pipe.flush().unwrap();
    pipe
}

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

fn place_base_assets(root: &Path) {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-assets.json")).unwrap();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let path = member["path"].as_str().unwrap();
        if path == "xberg.exe"
            || path.starts_with("ffmpeg/")
            || path.starts_with("models/sense_voice_")
            || path.starts_with("models/vad/")
        {
            continue;
        }
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, b"synthetic base asset").unwrap();
    }
}

fn shared_state(root: &Path) -> serde_json::Value {
    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(state["ok"], true, "{state}");
    state
}

/// XB-14/XB-21：真实代理把 idle 检查、唯一引擎替换与来源提交串为一个边界。
/// 合成引擎只证明生命周期/事务，不代表目标模型真实推理已经验收。
#[test]
fn bundled_activation_refuses_active_work_and_preserves_source_on_failure() {
    use jchtools_foundation::xberg_settings;
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let old = temporary.path().join("old");
    let bundled = temporary.path().join("bundled");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&bundled).unwrap();
    prepare(&old);
    place_base_assets(&old);
    place_base_assets(&bundled);
    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &bundled.join("xberg.exe"));
    let state = shared_state(&old);
    let mut shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let previous_source = xberg_settings::settings().unwrap().source;
    std::fs::write(old.join("long.txt"), "document").unwrap();
    let document_root = old.clone();
    let document = std::thread::spawn(move || {
        xberg_runtime::request(
            &document_root,
            json!({"command":"extract","path":document_root.join("long.txt")}),
            Duration::from_secs(30),
            &AtomicBool::new(false),
        )
        .unwrap()
    });
    let until = Instant::now() + Duration::from_secs(15);
    while !old.join("document-started").exists() {
        assert!(Instant::now() < until, "长文档必须先进入推理");
        std::thread::sleep(Duration::from_millis(10));
    }
    let rejected = xberg_runtime::activate_bundled_runtime(&bundled, false);
    let still_active = !document.is_finished();
    let unchanged = xberg_settings::required().unwrap() == old.canonicalize().unwrap();
    let during = xberg_runtime::request(
        &old,
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    std::fs::write(old.join("release-document"), "1").unwrap();
    let terminal = document.join().unwrap();
    assert!(rejected.is_err(), "在途推理必须拒绝迁移");
    assert!(still_active && unchanged, "迁移拒绝不得中断任务或提前切源");
    assert_eq!(during["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    assert_eq!(
        during["text"], "截图结果",
        "没有音频资产仍能在文档执行期间截图"
    );
    assert_eq!(terminal["ok"], true);
    assert_eq!(terminal["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    assert!(
        !bundled.join("starts.txt").exists(),
        "活跃时不得启动目标引擎"
    );

    let database = xberg_settings::open(&xberg_settings::state_dir().unwrap()).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER reject_runtime_source BEFORE INSERT ON app_settings
         WHEN NEW.key='xberg_source'
         BEGIN SELECT RAISE(ABORT, 'synthetic save failure'); END;",
        )
        .unwrap();
    let failed = xberg_runtime::activate_bundled_runtime(&bundled, false);
    let restored = shared_state(&old);
    shared.engine_pid = restored["jchtools_xberg_pid"].as_u64();
    assert!(failed.is_err(), "真实 SQLite 保存失败不得冒称迁移成功");
    assert_eq!(
        xberg_settings::required().unwrap(),
        old.canonicalize().unwrap()
    );
    assert_eq!(xberg_settings::settings().unwrap().source, previous_source);
    assert_eq!(
        restored["jchtools_broker_pid"],
        state["jchtools_broker_pid"]
    );
    assert!(!process_alive(
        state["jchtools_xberg_pid"].as_u64().unwrap(),
        "xberg.exe"
    ));
    for pid in std::fs::read_to_string(bundled.join("starts.txt"))
        .unwrap()
        .lines()
    {
        assert!(
            !process_alive(pid.parse().unwrap(), "xberg.exe"),
            "失败目标必须退出后才恢复原引擎"
        );
    }
    database
        .execute_batch("DROP TRIGGER reject_runtime_source")
        .unwrap();
    std::fs::write(bundled.join("xberg.exe"), b"not an executable").unwrap();
    assert!(xberg_runtime::activate_bundled_runtime(&bundled, false).is_err());
    let recovered = shared_state(&old);
    shared.engine_pid = recovered["jchtools_xberg_pid"].as_u64();
    assert_eq!(
        xberg_settings::required().unwrap(),
        old.canonicalize().unwrap()
    );
    assert_eq!(xberg_settings::settings().unwrap().source, previous_source);

    common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &bundled.join("xberg.exe"));
    xberg_runtime::activate_bundled_runtime(&bundled, false).unwrap();
    let activated = shared_state(&bundled);
    shared.engine_pid = activated["jchtools_xberg_pid"].as_u64();
    assert_eq!(
        xberg_settings::required().unwrap(),
        bundled.canonicalize().unwrap()
    );
    assert_eq!(
        activated["jchtools_broker_pid"],
        state["jchtools_broker_pid"]
    );
    assert!(!process_alive(
        recovered["jchtools_xberg_pid"].as_u64().unwrap(),
        "xberg.exe"
    ));
    let stale = xberg_runtime::request(
        &old,
        json!({"command":"keepalive"}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(stale["ok"], false, "切源后旧客户端不能把引擎切回旧根");
    assert_eq!(
        shared_state(&bundled)["jchtools_xberg_pid"],
        activated["jchtools_xberg_pid"]
    );
}

/// 仅用于观察真实代理的子进程环境、能力握手及不可中断请求；不执行模型推理。
/// 源码与产物都落在本测试 tempfile 内，不修改共享 fixture 或用户资产。
fn review_engine_copy(root: &Path) {
    let source = root.join("review_boundary_engine.rs");
    std::fs::write(
        &source,
        r#"
use std::{
    fs::OpenOptions,
    io::{self, BufRead, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
fn field(line: &str, name: &str) -> String {
    line.split(&format!("\"{name}\":\"")).nth(1)
        .unwrap_or("").split('"').next().unwrap_or("").to_owned()
}
fn main() {
    writeln!(OpenOptions::new().create(true).append(true).open("starts.txt").unwrap(),
        "{}", std::process::id()).unwrap();
    let output = Arc::new(Mutex::new(io::stdout()));
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let command = field(&line, "command");
        let id = field(&line, "id");
        let output = output.clone();
        std::thread::spawn(move || {
            if command == "ocr_snapshot" && field(&line, "image_base64") == "wait" {
                std::fs::write("snapshot-started", b"1").unwrap();
                // 模拟已经进入原生调用：取消 ACK 不会使原请求产生终态。
                while !Path::new("release-snapshot").exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if Path::new("cancel-started").exists() {
                    let mut out = output.lock().unwrap();
                    writeln!(out, "{{\"id\":\"{id}\",\"ok\":false,\"error_kind\":\"cancelled\",\"error\":\"cancelled\"}}").unwrap();
                    out.flush().unwrap();
                    return;
                }
            }
            let payload = match command.as_str() {
                "capabilities" => format!(
                    "\"commands\":[\"extract\",\"ocr_snapshot\",\"transcribe\",\"cancel\"],\
                    \"protocol_version\":2,\"cancellation\":\"cooperative\",\"timeout_ms\":true,\
                    \"document_snapshot_concurrent\":true,\"paddle_ocr\":true,\"transcription\":{0},\"transcription_streaming_path\":{0}",
                    !Path::new("disable-transcription").exists()),
                "snapshot_state" => format!(
                    "\"state\":\"ready\",\"debug_crops_inherited\":{}",
                    std::env::var_os("XBERG_SNAPSHOT_DEBUG_CROPS").is_some()),
                "extract" | "transcribe" => {
                    if line.contains("long.") {
                        let marker = if command == "extract" {
                            "document-started"
                        } else { "media-started" };
                        std::fs::write(marker, b"1").unwrap();
                        while !Path::new("release-document").exists() {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    if command == "extract" {
                        "\"document\":{\"content\":\"document\",\"images\":[],\"processing_warnings\":[]},\"warnings\":[]".into()
                    } else { "\"markdown\":\"media\"".into() }
                }
                "ocr_snapshot" => "\"text\":\"snapshot\"".into(),
                "cancel" => {
                    std::fs::write("cancel-started", b"1").unwrap();
                    "\"accepted\":true".into()
                },
                _ => "\"state\":\"ready\"".into(),
            };
            let mut out = output.lock().unwrap();
            writeln!(out, "{{\"id\":\"{id}\",\"ok\":true,{payload}}}").unwrap();
            out.flush().unwrap();
        });
    }
}
"#,
    )
    .unwrap();
    common::mock_engine_copy(source.to_str().unwrap(), &root.join("xberg.exe"));
}

fn review_wait_for_marker(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "合成进程未进入请求：{}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct ReviewEnvironment(&'static str, Option<std::ffi::OsString>);

impl ReviewEnvironment {
    fn set(name: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self(name, previous)
    }
}

impl Drop for ReviewEnvironment {
    fn drop(&mut self) {
        if let Some(value) = &self.1 {
            std::env::set_var(self.0, value);
        } else {
            std::env::remove_var(self.0);
        }
    }
}

/// XB-21/XB-26：旧来源媒体已就绪、冷启动无活动槽时，也必须拒绝丢失转录能力。
/// 经真实公开迁移入口、离线成员事务、代理激活握手和 SQLite 提交边界验证；
/// 合成收据/引擎只验证事务及协议，不代表真实音轨或模型验收。
#[test]
fn review_f32_cold_migration_preserves_ready_media_on_transcription_false() {
    use jchtools_foundation::xberg_settings;
    use sha2::{Digest, Sha256};

    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temporary = tempfile::tempdir().unwrap();
    let old = temporary.path().join("old");
    let bundled = temporary.path().join("bundled");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&bundled).unwrap();
    prepare(&old);
    place_base_assets(&old);
    place_base_assets(&bundled);
    review_engine_copy(&bundled);
    std::fs::write(bundled.join("disable-transcription"), b"1").unwrap();
    let _bundle = ReviewEnvironment::set("JCHTOOLS_TEST_BUNDLED_XBERG_DIR", &bundled);
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../resources/markdown-assets.json")).unwrap();
    let bytes = b"matching optional audio asset";
    let digest = hex::encode(Sha256::digest(bytes));
    let mut audio = Vec::new();
    for member in manifest["xberg"]["members"].as_array().unwrap() {
        let path = member["path"].as_str().unwrap();
        if !(path.starts_with("ffmpeg/")
            || path.starts_with("models/sense_voice_")
            || path.starts_with("models/vad/"))
        {
            continue;
        }
        let target = old.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, bytes).unwrap();
        audio.push(json!({"path":path,"size_bytes":bytes.len(),"sha256":digest}));
    }
    assert!(!audio.is_empty());
    xberg_runtime::validate_assets(&old, "media").unwrap();
    let engine = std::fs::read(bundled.join("xberg.exe")).unwrap();
    let components: Vec<_> = ["document", "snapshot"]
        .into_iter()
        .map(|id| {
            json!({
                "id":id,"archive":format!("xberg-{id}-x86_64-pc-windows-msvc.zip"),
                "requires":[],"size_bytes":1,"sha256":digest,
                "members":[{
                    "path":"xberg.exe","size_bytes":engine.len(),
                    "sha256":hex::encode(Sha256::digest(&engine))
                }]
            })
        })
        .collect();
    let release = "https://github.com/jchanghong023/xberg/releases/download/v2099.1";
    let receipt = json!({
        "owner":"JchTools-bundled-xberg-v1","tag":"v2099.1",
        "source":"https://api.github.com/repos/jchanghong023/xberg/releases/latest",
        "manifest":{
            "archive":"xberg-components-x86_64-pc-windows-msvc.json",
            "url":format!("{release}/xberg-components-x86_64-pc-windows-msvc.json"),
            "size_bytes":1,"sha256":digest
        },
        "components":components,
        "audio_assets":{
            "id":"audio-assets","archive":"xberg-audio-assets-x86_64-pc-windows-msvc.zip",
            "url":format!("{release}/xberg-audio-assets-x86_64-pc-windows-msvc.zip"),
            "requires":[],"size_bytes":1,"sha256":digest,"members":audio
        }
    });
    std::fs::write(
        bundled.join("jchtools-xberg-release.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    let original_source = xberg_settings::settings().unwrap().source;
    assert!(!old.join("starts.txt").exists(), "冷启动前不能预热旧引擎");
    let mut progress = Vec::new();
    let migrated = jchtools_markdown::markdown_assets::migrate_bundled_runtime(
        &AtomicBool::new(false),
        |message| progress.push(message),
    )
    .unwrap();
    // 即使先红断言失败，也通过进程守卫回收本测试启动的目标/恢复引擎。
    let state = shared_state(&migrated);
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    assert!(
        bundled.join("starts.txt").exists(),
        "必须实际到达目标引擎握手，不能因收据/迁入前置失败误绿：{progress:?}"
    );
    assert_eq!(
        migrated,
        old.canonicalize().unwrap(),
        "transcription:false 不能替代冷启动前媒体已就绪的持久来源"
    );
    assert_eq!(xberg_settings::required().unwrap(), old.canonicalize().unwrap());
    assert_eq!(xberg_settings::settings().unwrap().source, original_source);
    for member in receipt["audio_assets"]["members"].as_array().unwrap() {
        let path = member["path"].as_str().unwrap();
        assert_eq!(std::fs::read(old.join(path)).unwrap(), bytes);
        assert!(!bundled.join(path).exists(), "失败迁移应撤回本次复制成员");
    }
    let media = xberg_runtime::request(
        &old,
        json!({"command":"transcribe","path":old.join("short.txt")}),
        Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(media["ok"], true);
    assert_eq!(media["markdown"], "media");
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
    let _shared = SharedProcess { broker_pid, engine_pid: Some(engine_pid) };
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
        match std::fs::OpenOptions::new().read(true).write(true).open(&name) {
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
    let _shared = SharedProcess { broker_pid, engine_pid: Some(engine_pid) };
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
    assert_eq!(forced["ok"], true, "显式强退必须能够终结仅剩的挂起截图：{forced}");
    let terminal = snapshot.join().unwrap().unwrap();
    assert_eq!(terminal["ok"], false, "强退原请求只能交付失败终态");
    assert_eq!(terminal["error_kind"], "process_exited");
    let deadline = Instant::now() + Duration::from_secs(5);
    while (process_alive(broker_pid, "jchtools.exe") || process_alive(engine_pid, "xberg.exe")) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!process_alive(broker_pid, "jchtools.exe"), "强退须回收本应用代理");
    assert!(!process_alive(engine_pid, "xberg.exe"), "强退须回收本应用挂起引擎");
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
                &directory, json!({"command":"ocr_snapshot","image_base64":"wait"}),
                if timed_out { Duration::from_millis(500) } else { Duration::from_secs(60) },
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
            root, json!({"command":"ocr_snapshot","image_base64":"second"}),
            Duration::from_secs(5), &AtomicBool::new(false),
        ).unwrap();
        let document = xberg_runtime::request(
            root, json!({"command":"extract","path":root.join("short.txt")}),
            Duration::from_secs(5), &AtomicBool::new(false),
        ).unwrap();
        // 先释放原调用并回收线程，再断言，避免失败路径遗留挂起请求。
        std::fs::write(root.join("release-snapshot"), b"1").unwrap();
        let terminal = snapshot.join().unwrap().unwrap();
        assert!(waiting, "取消 ACK 超过1秒仍不是真实终态");
        assert_eq!(ready["state"], "ready", "已加载模型 Ready 不能代表截图空闲");
        assert_eq!(rejected["ok"], false, "原请求未结束时不得进入下一截图");
        assert_eq!(document["ok"], true, "截图取消不得阻塞文档 lane");
        assert_eq!(document["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
        assert_eq!(terminal["ok"], false);
        assert_eq!(terminal["error_kind"], if timed_out { "timeout" } else { "cancelled" });
        let next = xberg_runtime::request(
            root, json!({"command":"ocr_snapshot","image_base64":"next"}),
            Duration::from_secs(5), &AtomicBool::new(false),
        ).unwrap();
        assert_eq!(next["ok"], true, "原请求终态后才释放截图 lane");
        assert_eq!(next["jchtools_xberg_pid"], state["jchtools_xberg_pid"]);
    }
}
