//! 覆盖 XB-20～XB-23：真实 SQLite 跨进程恢复与共享代理生命周期；引擎为合成程序。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use jchtools::{
    xberg_runtime,
    xberg_settings::{self, Source},
};
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
                assert!(jchtools::markdown_assets::download_runtime(&cancelled, |_| {}).is_err());
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

fn stop() {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(state) = xberg_runtime::background_control(true) {
            if state["running"] == false {
                break;
            }
        }
        assert!(Instant::now() < until, "后台应退出，不留下代理或引擎");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 确认旧引擎确已退出：fixture 引擎可能因句柄继承看不到代理死亡引发的
/// stdin EOF（P1 自退依赖 EOF 可达），后台代理退出后仍有残留——有界等待
/// 后按守卫口径强制结束，避免 resume 阶段被「已有 Xberg」挡死。
fn ensure_engine_exit(pid: u64) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        let alive = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map_or(true, |output| {
                String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            });
        if !alive {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
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
                jchtools::markdown_document::convert(
                    &document_root.join("long.txt"),
                    &document_root,
                    "x_media",
                    &jchtools::markdown_document::Deadline::new(Duration::from_secs(30)),
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
            stop();
            // 后台已停：确认旧引擎随之退场（fixture 的 EOF 依赖句柄可达，兜底强清）。
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
