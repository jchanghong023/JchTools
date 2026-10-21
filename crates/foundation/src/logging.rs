//! 常开诊断日志（P-10）：与 perf 共用 tracing registry，不读取外部 RUST_LOG。
//!
//! 主落点为状态目录 `logs/jchtools.<UTC日期>.<pid>-<启动纳秒>-<序号>[.<分段>].log`；
//! 每次进程运行独占新文件并按天轮转。单文件不超过 1 MiB，自有文件最多 10 个，
//! 最长保留 14 天（精确识别旧 PID 分段及 `jchtools.log.<日期>` 历史文件）。
//! 主目录不可写时使用当前用户 LocalAppData 下的 `JchTools/Logs`；测试构建只使用隔离状态目录
//! 下的 `logs-fallback`。两处失败仅向 stderr 报告错误类型/码，不影响业务。
//!
//! 每个事件为 UTC 微秒时间戳的单行 key=value，包含级别、PID、线程、组件、
//! 稳定 event 与当前 span 链。文本有界，控制字符转义，已知凭据与 URL 去敏；
//! 调用方仍不得提交文件/OCR/模型/终端/协议正文。panic 只记录元数据。
//! 日志阈值只过滤事件，INFO 关联 span 始终保留；启动模式描述实际 debug assertions
//! 配置（release 配置也可启用），不将其误当成 Cargo profile 或进程角色。
use std::fmt;
use std::io::{self, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};
pub const LOG_DIR: &str = "logs";
const LOG_FILE_PREFIX: &str = "jchtools";
const RETENTION_DAYS: u64 = 14;
const TEXT_LIMIT: usize = 2048;
const FILE_LIMIT: u64 = 1_048_576;
const FILE_COUNT_LIMIT: usize = 10;
const RECORD_LIMIT: usize = 65_536;

static OPERATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn report_stderr(message: fmt::Arguments<'_>) {
    // stderr 可能已断开；诊断退化不能导致业务 panic 或析构二次 panic。
    let _ = writeln!(io::stderr().lock(), "{message}");
}

/// 进程级唯一 ID，不包含用户数据。
pub fn new_operation_id() -> String {
    let sequence = OPERATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{}-{nanos}-{sequence}", std::process::id())
}

/// 本次进程运行共用的日志命名空间；只在首次使用时分配启动标识。
pub(crate) fn process_run_id() -> &'static str {
    static RUN_ID: OnceLock<String> = OnceLock::new();
    RUN_ID.get_or_init(new_operation_id)
}

/// 建立独立操作 span；tracing 保留当前父 span，格式器输出完整上下文链。
#[must_use]
pub fn operation_span(component: &'static str, operation: &'static str) -> tracing::Span {
    operation_span_with_id(component, operation, &new_operation_id())
}

/// 建立复用已知请求 ID 的操作 span；异步调用方应使用 Instrument 传播。
pub fn operation_span_with_id(
    component: &'static str,
    operation: &'static str,
    operation_id: &str,
) -> tracing::Span {
    tracing::info_span!("operation", component, operation, operation_id)
}

#[must_use]
pub fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

mod appender;
mod format;
mod lifecycle;
mod sanitize;

pub use appender::Guard;
pub use lifecycle::{flush_before_exit, hold_for_process, init, FlushOnDrop};
pub use sanitize::{safe_error, safe_url};
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
#[cfg(test)]
mod tests;
