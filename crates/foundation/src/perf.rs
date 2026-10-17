//! 性能耗时打点（`perf-tracing` 特性，默认关闭）。
//!
//! 性能打点代码仅在显式启用 `perf-tracing` 时参与编译：
//! 默认构建仍包含 P-10 诊断日志所需的 `tracing` 依赖，但不启用性能打点或初始化性能日志，
//! 因而不产生性能打点开销（开启方式见 `Cargo.toml` 的 features 段）。
//!
//! 启用后性能日志落在状态目录的 `perf-logs/<本次进程标识>/` 下、按天轮转；
//! 每次真实启动独占目录，与界面运行日志（S-07：最近 300 条、不导出）互不影响，也不写控制台。
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
pub(crate) const LOG_FILE_PREFIX: &str = "jchtools-perf.log";
/// 性能 layer 的固定过滤指令：只放行 target `perf`。
/// 不读环境变量，避免外部 RUST_LOG 把第三方 crate 的日志混进性能日志。
#[cfg(feature = "perf-tracing")]
pub(crate) const FILTER: &str = "perf=info";

#[cfg(feature = "perf-tracing")]
use std::path::{Path, PathBuf};
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
#[macro_export]
macro_rules! perf_span {
    ($name:literal) => {
        #[cfg(feature = "perf-tracing")]
        let _perf_guard = tracing::info_span!(target: $crate::perf::TARGET, $name).entered();
    };
}
pub use crate::perf_span;

/// 性能日志句柄：只负责把后台写入线程活到调用方作用域结束，届时缓冲落盘。
#[cfg(feature = "perf-tracing")]
pub struct Guard {
    _worker: tracing_appender::non_blocking::WorkerGuard,
}

/// 当前进程的性能日志目录；不会读取其他启动的性能文件。
///
/// 此函数只提供路径，不保证目录已经创建或性能日志初始化成功。
#[cfg(feature = "perf-tracing")]
pub fn log_directory(state_dir: &Path) -> PathBuf {
    state_dir
        .join(LOG_DIR)
        .join(crate::logging::process_run_id())
}

/// 独立与组合 subscriber 共用的按天轮转构造器。
#[cfg(feature = "perf-tracing")]
pub(crate) fn appender(state_dir: &Path) -> Option<tracing_appender::rolling::RollingFileAppender> {
    std::fs::create_dir_all(state_dir.join(LOG_DIR)).ok()?;
    let directory = log_directory(state_dir);
    // 排他预留整个启动命名空间；已有目录无论来自谁都不能打开追加。
    // 后续 daily 文件只会在本次成功创建的目录内打开。
    std::fs::create_dir(&directory).ok()?;
    // 可失败的构造器不会因日志文件打开失败触发进程 panic 钩子。
    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .build(&directory)
        .ok()
}

/// 初始化性能日志：写入 `state_dir/perf-logs/<进程标识>/<前缀>.<日期>`，非阻塞、按天轮转。
///
/// 返回的句柄要绑定到活到进程退出前的作用域（见 `gui::run_with_engine_overrides`）；
/// 写成 `let _ = ...` 会立刻丢弃句柄，失去退出前刷盘的保证。
/// 任何一步失败都安静退化成「不打点」，绝不影响业务：目录建不出来、日志文件建不出来、
/// 已有全局 subscriber、过滤指令非法。
#[cfg(feature = "perf-tracing")]
pub fn init(state_dir: &Path) -> Option<Guard> {
    let (writer, worker) = tracing_appender::non_blocking(appender(state_dir)?);
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
pub fn apply_done(deleted: u64, moved: u64, skipped: u64, errors: u64) {
    tracing::info!(
        target: TARGET,
        deleted,
        moved,
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

#[cfg(all(test, feature = "perf-tracing"))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::{collections::BTreeMap, path::PathBuf, process::Command};

    fn workspace() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|path| path.join("scripts/make_tmp.py").is_file())
            .unwrap();
        let directory = root.join(".tmp/perf-tests");
        assert!(Command::new("python")
            .arg(root.join("scripts/make_tmp.py"))
            .args(["workspace", "--destination"])
            .arg(&directory)
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        tempfile::Builder::new()
            .prefix("process-isolation-")
            .tempdir_in(directory)
            .unwrap()
    }

    fn log_files(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                files.extend(log_files(&entry.path()));
            } else {
                files.insert(entry.path(), std::fs::read(entry.path()).unwrap());
            }
        }
        files
    }

    /// 两次真实启动使用同一状态目录，前一启动的日志必须保持逐字节不变。
    #[test]
    fn restarted_processes_never_share_performance_files() {
        const CHILD_STATE: &str = "JCHTOOLS_PERF_RESTART_STATE";
        const CHILD_MARKER: &str = "JCHTOOLS_PERF_RESTART_MARKER";
        if let Some(state) = std::env::var_os(CHILD_STATE) {
            let guard = init(Path::new(&state)).unwrap();
            {
                let marker = std::env::var(CHILD_MARKER).unwrap();
                let _span = tracing::info_span!(target: TARGET, "restart_probe", marker).entered();
                tracing::info!(target: TARGET, "performance restart marker");
            }
            drop(guard);
            return;
        }

        let temporary = workspace();
        let state = temporary.path().join("state");
        let run = |marker: &str| {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "perf::tests::restarted_processes_never_share_performance_files",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_STATE, &state)
                .env(CHILD_MARKER, marker)
                .status()
                .unwrap();
            assert!(status.success(), "性能日志子进程必须正常退出");
        };
        run("first-process-marker");
        let first_files = log_files(&state.join(LOG_DIR));
        assert_eq!(first_files.len(), 1, "首次启动必须写入一个性能日志文件");
        let (first_path, first_bytes) = first_files.first_key_value().unwrap();
        let first_text = std::str::from_utf8(first_bytes).unwrap();
        assert!(first_text.contains("first-process-marker"));
        assert!(first_text.contains("time.busy"));

        run("second-process-marker");
        assert_eq!(
            std::fs::read(first_path).unwrap(),
            *first_bytes,
            "重启不能续写前一进程的性能日志"
        );
        let second_files = log_files(&state.join(LOG_DIR));
        assert_eq!(second_files.len(), 2, "两次启动必须各自使用独立文件");
        let second_bytes = second_files
            .iter()
            .find(|(path, _)| *path != first_path)
            .unwrap()
            .1;
        let second_text = std::str::from_utf8(second_bytes).unwrap();
        assert!(second_text.contains("second-process-marker"));
        assert!(second_text.contains("time.busy"));
        assert!(!second_text.contains("first-process-marker"));
        assert!(!first_text.contains("second-process-marker"));
    }

    /// 已存在的启动目录即使名字恰好相同，也只能关闭采集，不能追加历史数据。
    #[test]
    fn existing_process_namespace_is_never_reopened() {
        let temporary = workspace();
        let directory = log_directory(temporary.path());
        std::fs::create_dir_all(&directory).unwrap();
        let previous = directory.join("jchtools-perf.log.2026-10-17");
        std::fs::write(&previous, b"previous process bytes").unwrap();
        assert!(init(temporary.path()).is_none());
        assert_eq!(std::fs::read(&previous).unwrap(), b"previous process bytes");
        assert_eq!(log_files(&directory).len(), 1);
    }

    /// 性能目录不可用时仍沿用静默关闭采集的原行为。
    #[test]
    fn unavailable_performance_directory_disables_collection() {
        let temporary = workspace();
        let obstruction = temporary.path().join(LOG_DIR);
        std::fs::write(&obstruction, b"not a directory").unwrap();
        assert!(init(temporary.path()).is_none());
        assert_eq!(std::fs::read(obstruction).unwrap(), b"not a directory");
    }
}
