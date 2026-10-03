#![allow(clippy::unwrap_used)]
//! 特征测试：钉住 run49.1 旧发布物 worker 协议（引擎不认识 capabilities 命令）
//! 的回退行为——能力查询失败（仅限 unsupported command）时请求必须直接放行
//! 进入实际命令，而不是被「Xberg 共享接口尚不可用」拒绝（XB 分区兼容语义）。
//! 与 tests/xberg_shared_process.rs 分文件：两侧都改写进程级环境变量，
//! 分开成独立测试进程避免竞争。

mod common;
use jchtools::xberg_runtime;
use serde_json::json;
use std::{path::Path, process::Command, sync::atomic::AtomicBool, time::Duration};

/// 测试守卫：同时强制结束本测试启动的共享代理与引擎（fixture 引擎可能经
/// 句柄继承与代理脱钩，只杀代理会留下占住会话执法的孤儿）。
struct SharedProcess {
    broker_pid: u64,
    engine_pid: Option<u64>,
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
fn legacy_worker_without_capabilities_still_serves_requests() {
    common::ensure_child_reaper();
    // 会话锁：与其他引擎测试二进制互斥（生产语义每会话至多一个 Xberg）。
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    common::mock_engine_copy(
        "tests/fixtures/shared_xberg_nocap.rs",
        &root.join("xberg.exe"),
    );
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.join("state"));
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"));
    jchtools::xberg_settings::save(root).unwrap();
    // 生产语义：每登录会话只允许一个 Xberg（进程扫描执法）。会话锁已保证
    // 与其他测试互斥；若仍遇到「已有 Xberg」（残留孤儿），对拒绝做有界重试。
    // 注意：忙碌应答里携带的是占用者的 pid，守卫只能从成功应答构造。
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let state = loop {
        let response = xberg_runtime::request(
            Path::new(root),
            json!({"command":"snapshot_state"}),
            Duration::from_secs(20),
            &AtomicBool::new(false),
        )
        .unwrap_or_else(|error| panic!("代理请求失败：{error}"));
        let busy = response["ok"] == false
            && response["error"]
                .as_str()
                .is_some_and(|error| error.contains("已有 Xberg"));
        if !busy {
            break response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "会话被既有 Xberg 占用超时（残留引擎未退出）：{response}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(state["ok"], true, "代理启动失败：{state}");
    let _shared = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    let screenshot = xberg_runtime::request(
        Path::new(root),
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(20),
        &AtomicBool::new(false),
    )
    .unwrap_or_else(|error| panic!("旧协议引擎的能力查询失败必须放行实际请求，被拒：{error}"));
    assert_eq!(screenshot["text"], "截图结果");
}
