//! 性能耗时打点（`perf-tracing` 特性，默认关闭）。
//!
//! 打点长期保留在业务代码里，但只有显式启用 `perf-tracing` 时才参与编译：
//! 默认构建不引入 tracing 依赖、不初始化性能日志、不产生任何额外开销
//! （开启方式见 `Cargo.toml` 的 features 段）。
//!
//! 启用后性能日志落在状态目录的 `perf-logs/` 下、按天轮转；它与界面运行日志
//! （S-07：最近 300 条、不导出）是两套互不影响的东西，也不写控制台。
//! 日志内容两类：
//! - 阶段 span 的关闭事件，带 `time.busy`（span 处于 enter 状态的总时长）与
//!   `time.idle`（两次 enter 之间的等待），单位自动在 ns/µs/ms/s 之间选择；
//! - 每个整体任务收尾的一条工作量事件（文件数 / 字节数 / 结果计数）。
//!
//! **打点契约**：span 名称与字段名是跨版本可比较的口径，只增不改名、不改语义。
//! 已落地的 span（括号内为覆盖范围）：
//! - `organize_analyze`（目录整理·分析：整体任务）
//! - `scan`（扫描所选目录）、`hash`（计算候选文件完整 Hash）
//! - `plan`（生成计划，下含 `plan_cleanup` / `plan_dedup` / `plan_moves` / `plan_empty_dirs`）
//! - `organize_apply`（目录整理·执行：整体任务）、`execute_actions`（实际执行计划动作）
//! - `count_archives`（确认框前的压缩包清点）
//! - `archive_extract`（递归解压：整体任务）、`extract_batch`（逐个包串行解压）
//!
//! 刻意不打点的位置：逐个文件 / 逐个包 / 逐个动作的高频循环（只统计批次总量，
//! 因为单项 span 会明显拖慢被测程序），以及 getter、简单包装、字段转换一类极短函数。

/// 性能日志子目录名（相对状态目录）。
pub const LOG_DIR: &str = "perf-logs";
/// 性能打点的统一 target：性能 layer 只收这个 target，界面的运行日志不受影响。
/// 函数上的 `#[cfg_attr(..., tracing::instrument(target = ...))]` 只接受字面量或标识符，
/// 所以那些位置直接写同值字面量 `"perf"`。
pub const TARGET: &str = "perf";
/// 性能日志文件名前缀，实际文件名是 `<前缀>.<日期>`（按天轮转）。
#[cfg(feature = "perf-tracing")]
const LOG_FILE_PREFIX: &str = "jchtools-perf.log";
/// 性能 layer 的固定过滤指令：只放行 target `perf`。
/// 不读环境变量，避免外部 RUST_LOG 把第三方 crate 的日志混进性能日志。
#[cfg(feature = "perf-tracing")]
const FILTER: &str = "perf=info";

#[cfg(feature = "perf-tracing")]
use std::path::Path;
#[cfg(feature = "perf-tracing")]
use tracing_subscriber::{
    fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};

/// 进入一个性能 span：守卫绑定到当前作用域，作用域结束时写出耗时。
///
/// 未启用 `perf-tracing` 时展开为空（零代码、零依赖）。名称必须是字面量——
/// span 名称是对外契约，路径 / ID / 时间戳一类动态信息只能作为字段记录。
/// 同一作用域内连续调用会互相遮蔽（前一个守卫仍活到作用域结束），
/// 需要两个平行区间时各自开一个 `{}` 块。
macro_rules! perf_span {
    ($name:literal) => {
        #[cfg(feature = "perf-tracing")]
        let _perf_guard = tracing::info_span!(target: $crate::perf::TARGET, $name).entered();
    };
}
pub(crate) use perf_span;

/// 性能日志句柄：只负责把后台写入线程活到调用方作用域结束，届时缓冲落盘。
#[cfg(feature = "perf-tracing")]
pub struct Guard {
    _worker: tracing_appender::non_blocking::WorkerGuard,
}

/// 初始化性能日志：写入 `state_dir/perf-logs/<前缀>.<日期>`，非阻塞、按天轮转。
///
/// 返回的句柄要绑定到活到进程退出前的作用域（见 `gui::run_with_engine_overrides`）；
/// 写成 `let _ = ...` 会立刻丢弃句柄，失去退出前刷盘的保证。
/// 任何一步失败都安静退化成「不打点」，绝不影响业务：目录建不出来、日志文件建不出来
/// （tracing-appender 此时会 panic）、已有全局 subscriber、过滤指令非法。
#[cfg(feature = "perf-tracing")]
pub fn init(state_dir: &Path) -> Option<Guard> {
    use std::panic::catch_unwind;

    let directory = state_dir.join(LOG_DIR);
    std::fs::create_dir_all(&directory).ok()?;
    // tracing-appender 在日志文件建不出来时会 panic；性能日志初始化不得把 GUI 拖下来。
    let appender =
        catch_unwind(|| tracing_appender::rolling::daily(&directory, LOG_FILE_PREFIX)).ok()?;
    let (writer, worker) = tracing_appender::non_blocking(appender);
    let layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_ansi(false)
        .with_span_events(FmtSpan::CLOSE)
        .with_writer(writer)
        .with_filter(EnvFilter::try_new(FILTER).ok()?);
    // 全局 subscriber 只能有一个：已有则安静退化（try_init 而非 init，不 panic）。
    tracing_subscriber::registry().with(layer).try_init().ok()?;
    Some(Guard { _worker: worker })
}

/// 目录整理·分析收尾的工作量字段（不含路径等敏感信息）。
#[cfg(feature = "perf-tracing")]
pub fn analyze_done(files: u64, bytes: u64, errors: u64) {
    tracing::info!(target: TARGET, files, bytes, errors, "目录整理分析完成");
}

/// 目录整理·执行收尾的工作量字段。
#[cfg(feature = "perf-tracing")]
pub fn apply_done(deleted: u64, moved: u64, linked: u64, skipped: u64, errors: u64) {
    tracing::info!(
        target: TARGET,
        deleted,
        moved,
        linked,
        skipped,
        errors,
        "目录整理执行完成"
    );
}

/// 递归解压收尾的工作量字段。
#[cfg(feature = "perf-tracing")]
pub fn extract_done(files: u64, ok: u64, failed: u64) {
    tracing::info!(target: TARGET, files, ok, failed, "递归解压结束");
}
