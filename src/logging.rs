//! 常开诊断日志（P-10）：关键步骤的告警与错误必须落盘。
//!
//! 与界面运行日志（S-07：内存 300 条、不导出）互不影响；与性能打点
//! （`perf-tracing` 特性，见 `perf.rs`）共用一套 tracing 基建但各自独立过滤：
//! 本模块的层只收 `jchtools` target，性能层只收 `perf` target。
//!
//! 落点：状态目录 `logs/jchtools.log.<日期>`，按天轮转，保留最近
//! 14 天（初始化时清理超期文件）。GUI 与 `--xberg-broker`
//! 代理进程都各自初始化；任何初始化失败都安静退化为无日志，绝不影响业务。
//! 初始化成功时同时接管 panic 钩子：panic 消息先写入日志再走默认输出。
//!
//! 记录范围（P-10）：进程间通信（共享代理/引擎生命周期、通信断裂与重建、
//! 请求失败）、网络出口（可选组件下载与初始化失败）、关键功能任务
//! （开始/结束统计、逐文件失败原因）。只记路径与诊断文本，不记文件正文。

use std::path::Path;
use std::time::{Duration, SystemTime};

/// 诊断日志子目录名（相对状态目录）。
pub const LOG_DIR: &str = "logs";
/// 日志文件名前缀，实际文件名是 `<前缀>.<日期>`（按天轮转）。
const LOG_FILE_PREFIX: &str = "jchtools.log";
/// 保留天数：初始化时清理超期日志文件。
const RETENTION_DAYS: u64 = 14;
/// 本模块层的固定过滤指令：只收本仓库两个 crate 的 target（共享模块在
/// snap-ocr-worker 内编译时 target 前缀是 `snap_ocr_worker`），不混入第三方
/// crate。不读环境变量，避免外部 RUST_LOG 改变产品诊断记录的完整性。
const FILTER: &str = "jchtools=info,snap_ocr_worker=info";

/// 诊断日志句柄：把非阻塞写入线程活到调用方作用域结束，保证退出前刷盘。
pub struct Guard {
    _workers: Vec<tracing_appender::non_blocking::WorkerGuard>,
}

/// 初始化诊断日志并返回句柄；句柄须绑定到活到进程退出前的作用域。
///
/// 防御性初始化：目录/文件建不出、全局 subscriber 已被占用时安静返回 `None`，
/// 业务不受影响。返回 `Some` 表示日志已生效（同时接管 panic 钩子）。
pub fn init(state_dir: &Path) -> Option<Guard> {
    use std::panic::catch_unwind;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

    prune_expired(&state_dir.join(LOG_DIR));
    let directory = state_dir.join(LOG_DIR);
    std::fs::create_dir_all(&directory).ok()?;
    // tracing-appender 在日志文件建不出来时会 panic；诊断日志不得拖垮业务。
    let appender =
        catch_unwind(|| tracing_appender::rolling::daily(&directory, LOG_FILE_PREFIX)).ok()?;
    let (writer, worker) = tracing_appender::non_blocking(appender);
    let layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(EnvFilter::try_new(FILTER).ok()?);
    // 全局 subscriber 只能有一个：已有则安静退化（try_init 而非 init，不 panic）。
    // perf-tracing 启用时性能层（target `perf`）与诊断层合并进同一 registry，
    // 构造内联在单一表达式块中，让具体类型流动，不为层手写泛型约束。
    let workers = {
        #[cfg(feature = "perf-tracing")]
        {
            let perf_directory = state_dir.join(crate::perf::LOG_DIR);
            let perf_parts = std::fs::create_dir_all(&perf_directory)
                .ok()
                .and_then(|()| {
                    std::panic::catch_unwind(|| {
                        tracing_appender::rolling::daily(
                            &perf_directory,
                            crate::perf::LOG_FILE_PREFIX,
                        )
                    })
                    .ok()
                })
                .map(tracing_appender::non_blocking);
            match perf_parts {
                Some((perf_writer, perf_worker)) => {
                    use tracing_subscriber::fmt::format::FmtSpan;
                    let perf_layer = tracing_subscriber::fmt::layer()
                        .compact()
                        .with_ansi(false)
                        .with_span_events(FmtSpan::CLOSE)
                        .with_writer(perf_writer)
                        .with_filter(
                            tracing_subscriber::EnvFilter::try_new(crate::perf::FILTER).ok(),
                        );
                    let registry = tracing_subscriber::registry().with(layer).with(perf_layer);
                    registry.try_init().ok().map(|()| vec![worker, perf_worker])
                }
                None => tracing_subscriber::registry()
                    .with(layer)
                    .try_init()
                    .ok()
                    .map(|()| vec![worker]),
            }
        }
        #[cfg(not(feature = "perf-tracing"))]
        {
            tracing_subscriber::registry()
                .with(layer)
                .try_init()
                .ok()
                .map(|()| vec![worker])
        }
    };
    let workers = workers?;
    install_panic_hook();
    tracing::info!(pid = std::process::id(), "诊断日志已初始化（P-10）");
    Some(Guard { _workers: workers })
}

/// panic 先入日志再走默认钩子：GUI 与代理进程的崩溃现场必须有磁盘记录。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "非字符串 panic payload".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        tracing::error!(%message, %location, thread = std::thread::current().name().unwrap_or("?"), "进程 panic");
        default_hook(info);
    }));
}

/// 清理超过保留期的日志文件；失败安静忽略（best-effort，不阻塞初始化）。
fn prune_expired(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let cutoff = SystemTime::now() - Duration::from_secs(RETENTION_DAYS * 24 * 3600);
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file()
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with(LOG_FILE_PREFIX)
        {
            continue;
        }
        let expired = metadata.modified().is_ok_and(|modified| modified < cutoff);
        if expired {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 覆盖 P-10：初始化后 WARN/ERROR 必须落到磁盘日志文件。
    // 全局 subscriber 每进程只有一个：本测试若与其他 init 竞争，
    // try_init 失败方安静退化，断言只要求磁盘文件出现记录。
    #[test]
    fn warns_and_errors_reach_disk_file() {
        let temp = tempfile::tempdir().unwrap();
        let guard = init(temp.path());
        tracing::warn!("p10-warn-marker");
        tracing::error!(path = "probe", "p10-error-marker");
        drop(guard); // 触发非阻塞写入线程刷盘
        let directory = temp.path().join(LOG_DIR);
        let mut found = String::new();
        for entry in std::fs::read_dir(&directory).unwrap().flatten() {
            let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
            if text.contains("p10-warn-marker") && text.contains("p10-error-marker") {
                found = text;
                break;
            }
        }
        assert!(
            found.contains("p10-warn-marker") && found.contains("p10-error-marker"),
            "诊断日志必须包含落盘的 WARN 与 ERROR 记录"
        );
        assert!(found.contains("ERROR"), "错误级别必须可见：{found}");
    }
}
