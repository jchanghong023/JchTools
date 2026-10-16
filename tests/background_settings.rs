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

//! 覆盖 XB-20～XB-23：真实 SQLite 跨进程恢复与共享代理生命周期；引擎为合成程序。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use jchtools_foundation::xberg_settings::{self, Source};
use jchtools_xberg::xberg_runtime;
use serde_json::json;
use std::{
    path::Path,
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn child(name: &str, root: &Path, action: &str) {
    assert!(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("JT_BACKGROUND_CASE", action)
        .env("JCHTOOLS_TEST_STATE_DIR", root)
        .env("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"))
        .status()
        .unwrap()
        .success());
}

#[test]
fn both_sources_survive_process_restart_and_failed_save() {
    common::ensure_child_reaper();
    if let Ok(action) = std::env::var("JT_BACKGROUND_CASE") {
        let root = xberg_settings::state_dir().unwrap();
        let custom = root.join("用户目录");
        let downloaded = root.join("下载目录");
        match action.as_str() {
            "save" => {
                for path in [&custom, &downloaded] {
                    std::fs::create_dir(path).unwrap();
                    std::fs::write(path.join("xberg.exe"), b"configuration fixture").unwrap();
                }
                // 用旧版唯一键模拟升级，切换来源不能遗失旧目录。
                xberg_settings::save(&custom).unwrap();
                xberg_settings::save_source(Source::Downloaded, &downloaded).unwrap();
            }
            "switch" => {
                let saved = xberg_settings::settings().unwrap();
                assert_eq!(saved.source, Source::Downloaded);
                assert_eq!(saved.custom.unwrap(), custom.canonicalize().unwrap());
                assert_eq!(
                    saved.downloaded.unwrap(),
                    downloaded.canonicalize().unwrap()
                );
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    downloaded.canonicalize().unwrap()
                );
                assert!(xberg_settings::save(&root.join("missing")).is_err());
                assert_eq!(
                    xberg_settings::settings().unwrap().source,
                    Source::Downloaded
                );
                xberg_settings::select(Source::Custom).unwrap();
            }
            "read" => {
                assert_eq!(xberg_settings::settings().unwrap().source, Source::Custom);
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    custom.canonicalize().unwrap()
                );
                assert_eq!(
                    xberg_settings::settings().unwrap().downloaded.unwrap(),
                    downloaded.canonicalize().unwrap()
                );
                let cancelled = AtomicBool::new(true);
                assert!(jchtools_markdown::markdown_assets::download_audio_runtime(
                    &cancelled,
                    |_| {},
                )
                .is_err());
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    custom.canonicalize().unwrap()
                );
                std::fs::remove_file(downloaded.join("xberg.exe")).unwrap();
                assert!(xberg_settings::select(Source::Downloaded).is_err());
                assert_eq!(
                    xberg_settings::required().unwrap(),
                    custom.canonicalize().unwrap()
                );
            }
            _ => panic!("未知测试阶段"),
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    for action in ["save", "switch", "read"] {
        child(
            "both_sources_survive_process_restart_and_failed_save",
            root.path(),
            action,
        );
    }
}

fn request(root: &Path) -> serde_json::Value {
    xberg_runtime::request(
        root,
        json!({"command":"keepalive"}),
        Duration::from_secs(10),
        &AtomicBool::new(false),
    )
    .unwrap()
}

/// 引擎获取等待：每登录会话只允许一个 Xberg（进程扫描执法）。其他测试二进制
/// 可能正持有会话引擎；对「已有 Xberg」拒绝做有界重试，等它退出后自然获得
/// 会话。其余失败仍由调用方断言报错。
fn request_waiting_for_session(root: &Path) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let state = request(root);
        let busy = state["ok"] == false
            && state["error"]
                .as_str()
                .is_some_and(|error| error.contains("已有 Xberg"));
        if !busy || Instant::now() >= deadline {
            return state;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn wait_for_background_exit() {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        // 等待只查询状态；重复发退出命令会掩盖第一次命令未真正触发退出的缺陷。
        if let Ok(state) = xberg_runtime::background_control(false) {
            if state["running"] == false {
                break;
            }
        }
        assert!(Instant::now() < until, "后台应退出，不留下代理或引擎");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop() {
    assert_eq!(xberg_runtime::background_control(true).unwrap()["ok"], true);
    wait_for_background_exit();
}

/// XB-23：后台退出必须自行结束引擎；测试不能强杀残留后继续当作成功。
/// 失败路径由外层测试进程的回收 Job 清场，不在验收断言前修饰现场。
fn ensure_engine_exit(pid: u64) {
    let deadline = Instant::now() + Duration::from_secs(8);
    let pid = pid.to_string();
    loop {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("必须能查询引擎是否已退出");
        assert!(
            output.status.success(),
            "引擎退出查询失败：{}",
            output.status
        );
        // 精确匹配 CSV 的 PID 字段，不能让内存占用或其他列中的数字命中。
        let alive = String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.split(',')
                .nth(1)
                .is_some_and(|value| value.trim_matches('"') == pid)
        });
        if !alive {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "退出后台后引擎 {pid} 仍存活，不得由测试强杀后冒称退出成功"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn broker_survives_client_exit_and_obeys_explicit_stop() {
    common::ensure_child_reaper();
    // 会话锁：父进程持有跨两个子进程阶段（start 与 resume-stop）；子进程
    // （JT_BACKGROUND_CASE 在场）不重复加锁，靠父进程的锁覆盖全程。
    let session = if std::env::var("JT_BACKGROUND_CASE").is_err() {
        Some(common::session_lock())
    } else {
        None
    };
    if session.is_some() {
        common::cleanup_stray_engines();
    }
    if let Ok(action) = std::env::var("JT_BACKGROUND_CASE") {
        let root = xberg_settings::state_dir().unwrap().join("engine");
        if action == "start" {
            std::fs::create_dir(&root).unwrap();
            common::mock_engine_copy("tests/fixtures/shared_xberg.rs", &root.join("xberg.exe"));
            xberg_settings::save(&root).unwrap();
            xberg_runtime::resume_background().unwrap();
            let state = request_waiting_for_session(&root);
            assert_eq!(state["ok"], true);
            std::fs::write(root.join("pid"), state["jchtools_xberg_pid"].to_string()).unwrap();
        } else {
            let state = request(&root);
            assert_eq!(
                state["jchtools_xberg_pid"].to_string(),
                std::fs::read_to_string(root.join("pid")).unwrap()
            );
            // O-16/XB-23：显式强退不能杀死仍有文档任务的共享进程。
            std::fs::write(root.join("long.txt"), "document").unwrap();
            let document_root = root.clone();
            let document = std::thread::spawn(move || {
                jchtools_markdown::markdown_document::convert(
                    &document_root.join("long.txt"),
                    &document_root,
                    "x_media",
                    &jchtools_markdown::markdown_document::Deadline::new(Duration::from_secs(30)),
                )
            });
            let until = Instant::now() + Duration::from_secs(10);
            while !root.join("document-started").exists() {
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(xberg_runtime::force_background_exit().unwrap()["ok"], false);
            assert!(!document.is_finished());
            std::fs::write(root.join("release-document"), "1").unwrap();
            assert!(document.join().unwrap().is_ok());
            assert_eq!(xberg_runtime::force_background_exit().unwrap()["ok"], true);
            wait_for_background_exit();
            // 后台已停：必须观察到旧引擎自行退场，不能用测试清场代替生命周期断言。
            let old_engine: u64 = std::fs::read_to_string(root.join("pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            ensure_engine_exit(old_engine);
            // 取证：restart 前枚举残留进程（成功路径也会写，日志落任务目录）。
            let _ = Command::new("powershell")
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command",
                    "Get-CimInstance Win32_Process | Where-Object { $_.Name -match 'xberg|JchTools|snap-ocr' } | ForEach-Object { \"{0} {1} :: {2}\" -f $_.ProcessId, $_.Name, $_.CommandLine } | Out-File -Encoding utf8 process-log.txt"])
                .current_dir(root.clone())
                .output();
            assert!(xberg_runtime::background_allowed().is_err());
            assert!(xberg_runtime::request(
                &root,
                json!({"command":"keepalive"}),
                Duration::from_secs(2),
                &AtomicBool::new(false)
            )
            .is_err());
            xberg_runtime::resume_background().unwrap();
            let restarted = request_waiting_for_session(&root);
            assert_eq!(restarted["ok"], true);
            // keepalive 由代理立即应答（Engine::spawn 只发起 CreateProcess，不保证
            // 引擎进程已执行到写启动记录的 main 首行）；负载高/冷启动时新引擎
            // 可能尚未跑到该行，若随即 stop()，代理 watch 线程会把它终结在
            // 启动记录落盘之前，starts.txt 间歇丢一行（曾观测 1 != 2）。先有界
            // 等待新引擎留下启动记录，再进入收尾；计数断言本身不变。
            let boot = Instant::now() + Duration::from_secs(10);
            while std::fs::read_to_string(root.join("starts.txt"))
                .map_or(true, |text| text.lines().count() < 2)
            {
                assert!(Instant::now() < boot, "新引擎应在 starts.txt 留下启动记录");
                std::thread::sleep(Duration::from_millis(50));
            }
            // 重启后刷新引擎 pid：收尾清理必须指向新引擎，否则 engine2 会以
            // 孤儿身份堵住后续测试（会话单引擎执法）。
            let new_engine = restarted["jchtools_xberg_pid"].as_u64().unwrap();
            std::fs::write(root.join("pid"), new_engine.to_string()).unwrap();
            stop();
            ensure_engine_exit(new_engine);
            assert_eq!(
                std::fs::read_to_string(root.join("starts.txt"))
                    .unwrap()
                    .lines()
                    .count(),
                2
            );
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    child(
        "broker_survives_client_exit_and_obeys_explicit_stop",
        root.path(),
        "start",
    );
    child(
        "broker_survives_client_exit_and_obeys_explicit_stop",
        root.path(),
        "resume-stop",
    );
}

/// XB-23：停止前已连接但未交付完整请求的客户端不是在途任务，不能拖住代理退出。
/// 使用真实内部 broker 入口和隔离管道；不启动引擎、模型、托盘或用户桌面。
#[test]
fn background_stop_does_not_wait_for_preexisting_incomplete_request() {
    use sha2::{Digest, Sha256};
    use std::io::Write as _;

    const TEST: &str = "background_stop_does_not_wait_for_preexisting_incomplete_request";
    if std::env::var("JT_BACKGROUND_CASE").is_err() {
        struct Workspace(std::path::PathBuf);
        impl Drop for Workspace {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).expect("清理本测试工厂目录");
            }
        }
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".tmp")
            .join(format!(
                "background-incomplete-{}-{unique}",
                std::process::id()
            ));
        assert!(Command::new("python")
            .args(["scripts/make_tmp.py", "workspace", "--destination"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        let workspace = Workspace(root);
        child(TEST, &workspace.0, "incomplete-stop");
        return;
    }

    struct Broker(std::process::Child);
    impl Drop for Broker {
        fn drop(&mut self) {
            if self.0.try_wait().unwrap().is_none() {
                // 只回收本测试直接创建的 broker；断言结果在清场前保存。
                self.0.kill().expect("回收本测试 broker");
            }
            self.0.wait().expect("等待本测试 broker 回收");
        }
    }
    let mut broker = Broker(
        Command::new(env!("CARGO_BIN_EXE_JchTools"))
            .arg("--xberg-broker")
            .spawn()
            .unwrap(),
    );
    let mut session = 0;
    #[link(name = "kernel32")]
    extern "system" {
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }
    // SAFETY: 当前进程 ID 和有效可写的 session 输出指针。
    assert_ne!(
        unsafe { ProcessIdToSessionId(std::process::id(), &raw mut session) },
        0
    );
    let root = xberg_settings::state_dir().unwrap();
    let digest = Sha256::digest(format!("{}:{session}", root.display()).as_bytes());
    let name = format!(r"\\.\pipe\jchtools-xberg-{}", hex::encode(digest));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut incomplete = loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)
        {
            Ok(pipe) => break pipe,
            Err(error) => {
                assert!(
                    broker.0.try_wait().unwrap().is_none(),
                    "broker 启动时意外退出"
                );
                assert!(Instant::now() < deadline, "隔离 broker 未监听：{error}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    incomplete.write_all(b"{\"request\":").unwrap();
    // 第二条连接能完成状态交换，证明 listener 已接受第一条不完整请求；
    // 不用 sleep 猜测它是否已经进入停止前的 ActiveHandler 屏障。
    loop {
        if let Ok(state) = xberg_runtime::background_control(false) {
            assert_eq!(state["running"], true);
            break;
        }
        assert!(Instant::now() < deadline, "隔离 broker 未接受后续状态请求");
        std::thread::sleep(Duration::from_millis(10));
    }
    let stopped = xberg_runtime::background_control(true).unwrap();
    assert_eq!(stopped["ok"], true);
    assert_eq!(stopped["stopping"], true);
    assert_eq!(stopped["active"], 0, "不完整 JSON 尚未形成业务任务");
    assert!(
        xberg_runtime::background_allowed().is_err(),
        "停止回执已建立防拉起标记"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let exited = loop {
        if broker.0.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // 保持不完整连接直到观测结束；不能先断开它再冒称 broker 自行停止。
    drop(incomplete);
    assert!(
        exited,
        "broker-stop 已回执且 active=0，但停止前不完整请求仍拖住 JchTools 代理退出"
    );
}
