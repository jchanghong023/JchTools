#![allow(clippy::unwrap_used)]
//! 覆盖 XB-14/XB-15/XB-17：真实代理进程及管道，模拟引擎（不代表真实模型验收）。
use jchtools::{
    markdown_document::{self, Deadline},
    xberg_runtime,
};
use serde_json::json;
use std::{
    path::Path,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn prepare(root: &Path) {
    let status = Command::new("rustc")
        .args(["--edition=2021", "tests/fixtures/shared_xberg.rs", "-o"])
        .arg(root.join("xberg.exe"))
        .status()
        .unwrap();
    assert!(status.success());
    std::env::set_var("JCHTOOLS_TEST_STATE_DIR", root.join("state"));
    std::env::set_var("JCHTOOLS_TEST_BROKER_EXE", env!("CARGO_BIN_EXE_JchTools"));
    jchtools::xberg_settings::save(root).unwrap();
    std::fs::write(root.join("short.txt"), "document").unwrap();
}

struct Broker(u64);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = Command::new("taskkill")
            .args(["/PID", &self.0.to_string(), "/F"])
            .output();
    }
}

#[test]
fn documents_reuse_process_and_snapshot_finishes_during_document() {
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
    let state = xberg_runtime::request(
        root,
        json!({"command":"snapshot_state"}),
        Duration::from_secs(15),
        &AtomicBool::new(false),
    )
    .unwrap();
    let _broker = Broker(state["jchtools_broker_pid"].as_u64().unwrap());
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
