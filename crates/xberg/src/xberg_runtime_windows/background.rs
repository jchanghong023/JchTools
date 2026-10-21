use super::client::{connect, exchange, log_response};
use super::*;
use jchtools_foundation::{logging, xberg_settings};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

pub(super) fn identity() -> Result<String, String> {
    let mut session = 0;
    // SAFETY: 当前进程 ID 与有效可写 session 指针。
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: session 指针有效，pid 为当前进程。
    let ok = unsafe { ProcessIdToSessionId(pid, &raw mut session) };
    if ok == 0 {
        return Err("无法查询 Windows 登录会话".into());
    }
    let root = xberg_settings::state_dir()?;
    let digest = Sha256::digest(format!("{}:{session}", root.display()).as_bytes());
    Ok(hex::encode(digest))
}

pub(super) fn pipe_name() -> Result<String, String> {
    Ok(format!(r"\\.\pipe\jchtools-xberg-{}", identity()?))
}

fn stopped_marker() -> Result<PathBuf, String> {
    Ok(xberg_settings::state_dir()?.join(format!("background-stopped-{}", identity()?)))
}
pub(crate) fn resume_background() -> Result<(), String> {
    let span = logging::operation_span("xberg_runtime", "background_resume");
    let _entered = span.enter();
    let started = Instant::now();
    tracing::info!(
        event = "xberg_background_resume_started",
        "开始恢复后台自动启动状态"
    );
    let result = (|| match std::fs::remove_file(stopped_marker()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            tracing::warn!(
                event = "xberg_background_marker_remove_failed",
                stage = "marker_remove",
                error_code = e.raw_os_error(),
                "恢复后台启动标记失败"
            );
            Err(format!("恢复后台启动状态失败：{e}"))
        }
    })();
    if let Ok(()) = &result {
        tracing::debug!(
            event = "xberg_background_resume_completed",
            elapsed_ms = logging::elapsed_ms(started),
            "后台自动启动状态已恢复"
        );
    } else {
        tracing::warn!(
            event = "xberg_background_resume_failed",
            elapsed_ms = logging::elapsed_ms(started),
            "后台自动启动状态恢复失败"
        );
    }
    result
}
pub(crate) fn background_allowed() -> Result<(), String> {
    if stopped_marker()?.exists() {
        Err("后台已从托盘退出；重新打开 JchTools 后自动启动".into())
    } else {
        Ok(())
    }
}
pub(crate) fn background_control(stop: bool) -> Result<Value, String> {
    background_command(if stop { "broker-stop" } else { "broker-state" })
}
pub(crate) fn force_background_exit() -> Result<Value, String> {
    background_command("broker-force-stop")
}
pub(crate) fn activate_bundled_runtime(root: &Path, preserve_media: bool) -> Result<(), String> {
    let root =
        std::fs::canonicalize(root).map_err(|error| format!("随包 Xberg 目录不可读：{error}"))?;
    let timeout = Duration::from_secs(60);
    let pipe = connect(true, Instant::now() + timeout, false)?;
    let operation_id = logging::new_operation_id();
    let response = exchange(
        pipe,
        &json!({
            "runtime_dir": root,
            "request": {"id": operation_id, "command": "broker-activate-bundled", "preserve_media": preserve_media},
            "diagnostic": {"operation_id": operation_id, "caller_pid": std::process::id()}
        }),
        timeout,
    )?;
    super::checked(response).map(|_| ())
}

fn background_command(command: &str) -> Result<Value, String> {
    let operation_id = logging::new_operation_id();
    let span =
        logging::operation_span_with_id("xberg_runtime", "background_control", &operation_id);
    let _entered = span.enter();
    let started = Instant::now();
    let mut stage = "connect";
    tracing::info!(
        event = "xberg_background_control_started",
        command = super::diagnostic_command(Some(command)),
        "后台控制请求开始"
    );
    let result = (|| {
        let name = pipe_name()?;
        let pipe = match OpenOptions::new().read(true).write(true).open(name) {
            Ok(pipe) => pipe,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let response = json!({"ok":true,"running":false,"active":0});
                if command != "broker-state" {
                    stage = "stopped_marker";
                    mark_background_stopped()?;
                }
                return Ok(response);
            }
            Err(e) => {
                tracing::warn!(
                    event = "xberg_background_connect_failed",
                    error_code = e.raw_os_error(),
                    "后台控制连接失败"
                );
                return Err(format!("后台控制连接失败：{e}"));
            }
        };
        stage = "exchange";
        let response = exchange(
            pipe,
            &json!({"request":{"id":"background-control", "command":command},"diagnostic":{"operation_id":operation_id,"caller_pid":std::process::id()}}),
            QUERY_TIMEOUT,
        )?;
        // 只有代理确认接受停止（或已明确不在运行）后才阻止后续自动拉起。
        // 拒绝、协议错误和 IPC 失败都保留当前后台状态，避免一次失败的退出请求
        // 把仍在运行的服务永久标成 stopped。
        if should_mark_background_stopped(command, &response) {
            stage = "stopped_marker";
            mark_background_stopped()?;
        }
        Ok(response)
    })();
    if let Ok(response) = &result {
        log_response(
            "background_control",
            super::diagnostic_command(Some(command)),
            response,
            started,
            None,
        );
    } else {
        tracing::warn!(
            event = "xberg_background_control_failed",
            stage,
            elapsed_ms = logging::elapsed_ms(started),
            "后台控制请求失败"
        );
    }
    result
}

pub(super) fn should_mark_background_stopped(command: &str, response: &Value) -> bool {
    command != "broker-state"
        && response["ok"] == true
        && (response["stopping"] == true || response["running"] == false)
}

fn mark_background_stopped() -> Result<(), String> {
    let marker = stopped_marker()?;
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            tracing::warn!(
                event = "xberg_stopped_marker_failed",
                stage = "stopped_marker",
                operation = "create_directory",
                error_type = ?error.kind(),
                error_code = ?error.raw_os_error(),
                "无法准备共享后台停止标记目录"
            );
            error.to_string()
        })?;
    }
    std::fs::write(marker, b"stopped").map_err(|error| {
        tracing::warn!(
            event = "xberg_stopped_marker_failed",
            stage = "stopped_marker",
            operation = "write",
            error_type = ?error.kind(),
            error_code = ?error.raw_os_error(),
            "无法写入共享后台停止标记"
        );
        error.to_string()
    })
}
