//! Windows 命名管道代理。锁与管道名不包含运行目录，保存不同目录也不会另起引擎。
//! 停止后新连接不加入终态交付屏障，但仍解析请求，让已登记任务的取消可达；
//! 业务入口继续由 stopping gate 拒绝，未交付 JSON 的新连接不能拖住后台退出。
// 本文件经 xberg_runtime.rs 的 #[path] 以非同名模块（platform）加载，
// rustc 对其子模块按同目录（src/）解析；显式 #[path] 指向同名子目录。
#[path = "xberg_runtime_windows/background.rs"]
mod background;
#[path = "xberg_runtime_windows/client.rs"]
mod client;
#[path = "xberg_runtime_windows/deadline.rs"]
mod deadline;
#[path = "xberg_runtime_windows/engine.rs"]
mod engine;
#[path = "xberg_runtime_windows/message.rs"]
mod message;
#[path = "xberg_runtime_windows/server.rs"]
mod server;

pub(crate) use background::{
    activate_bundled_runtime, background_allowed, background_control, force_background_exit,
    resume_background,
};
pub(crate) use client::request;
pub(crate) use server::serve;

use super::{
    checked, diagnostic_command, diagnostic_error_kind, diagnostic_id, startup_config,
    validate_assets, validate_capabilities, MAX_MESSAGE_BYTES,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[link(name = "kernel32")]
extern "system" {
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
}
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
struct PendingRequest {
    sender: mpsc::Sender<Value>,
    lane: Option<&'static str>,
    span: tracing::Span,
    started: Instant,
    command: &'static str,
    cancel_requested: bool,
}
type Pending = Arc<Mutex<HashMap<String, PendingRequest>>>;
const BROKER_PROTOCOL: u64 = 3;

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(any(test, feature = "test-hooks"))]
#[cfg(test)]
#[path = "xberg_runtime_windows/tests.rs"]
mod tests;
