#![allow(clippy::unwrap_used)]
//! 覆盖 XB-05/XB-07（缺陷 S6-04）：引擎返回未知请求 id 的响应（幽灵/重复
//! 响应）属协议异常，代理不得静默丢弃——等待者必须立即收到失败终态且场景
//! lane 随之释放，与「超限响应且无法定位请求」的断裂口径一致；否则等待者
//! 只能靠超时报错，而在途项不随超时删除，场景 lane 被永久占用，直到引擎
//! 死亡才恢复。与 tests/xberg_shared_process.rs 分文件：各自使用不同的模拟
//! 引擎 fixture（common 的编译缓存按二进制内首个 fixture 生效）。

mod common;
use jchtools::xberg_runtime;
use serde_json::json;
use std::{
    path::Path,
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// 测试守卫：结束本测试启动的共享代理与引擎（与 xberg_shared_process 同口径）。
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
fn unknown_id_response_fails_waiter_and_releases_lane() {
    common::ensure_child_reaper();
    let _session = common::session_lock();
    common::cleanup_stray_engines();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    common::mock_engine_copy(
        "tests/fixtures/shared_xberg_unknownid.rs",
        &root.join("xberg.exe"),
    );
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.join("state"));
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"));
    jchtools::xberg_settings::save(root).unwrap();
    // 引擎获取等待：会话锁已与其他引擎测试二进制互斥；若仍遇到「已有 Xberg」
    // （残留孤儿），对拒绝做有界重试，成功应答才继续。
    let session_deadline = Instant::now() + Duration::from_secs(60);
    let state = loop {
        let response = xberg_runtime::request(
            Path::new(root),
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
    let _guard = SharedProcess {
        broker_pid: state["jchtools_broker_pid"].as_u64().unwrap(),
        engine_pid: state["jchtools_xberg_pid"].as_u64(),
    };
    // 触发请求：模拟引擎先发一条无人认领（未知 id）的响应，且对该请求自身
    // 永不回应。修复前：幽灵响应被静默丢弃，客户端只能等超时后报错，且
    // snapshot lane 被在途项占住；修复后：立即按通信断裂交付失败终态。
    let first = xberg_runtime::request(
        Path::new(root),
        json!({"command":"ocr_snapshot","image_base64":"phantom"}),
        Duration::from_millis(800),
        &AtomicBool::new(false),
    )
    .expect("未知 id 响应必须立即按协议异常交付失败终态，而不是让等待者超时");
    assert_eq!(
        first["ok"], false,
        "未知 id 是引擎侧协议异常，该请求必须失败：{first}"
    );
    assert_eq!(
        first["error_kind"], "process_exited",
        "须按通信断裂口径交付失败（与超限响应未知 id 一致）：{first}"
    );
    // lane 释放：同场景新请求可再次发起，不应被「上一请求尚未确认结束」拒绝。
    let second = xberg_runtime::request(
        Path::new(root),
        json!({"command":"ocr_snapshot","image_base64":"fixture"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap_or_else(|error| panic!("lane 未释放，同场景请求被连坐拒绝：{error}"));
    assert_eq!(
        second["ok"], true,
        "lane 释放后同场景请求应重建引擎并成功：{second}"
    );
    assert_eq!(second["text"], "截图结果");
    // 重建后的新引擎也要清场：守卫只登记了初代引擎 pid。
    if let Some(pid) = second["jchtools_xberg_pid"].as_u64() {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
}
