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

// 测试按主题拆分为子模块；本文件保留共享夹具、辅助函数与模块声明。
// 跨子模块共享的环境变量 RAII 助手：set 恢复原值，drop 保证清理。
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

#[path = "xberg_shared_process/background_stop.rs"]
mod background_stop;
#[path = "xberg_shared_process/batch_stop.rs"]
mod batch_stop;
#[path = "xberg_shared_process/bundled_activation.rs"]
mod bundled_activation;
#[path = "xberg_shared_process/engine_lifecycle.rs"]
mod engine_lifecycle;
#[path = "xberg_shared_process/review_regressions.rs"]
mod review_regressions;
#[path = "xberg_shared_process/shared_lanes.rs"]
mod shared_lanes;
#[path = "xberg_shared_process/unread_control_state.rs"]
mod unread_control_state;
