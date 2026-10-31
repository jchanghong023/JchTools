use super::appender::{fallback_directory, open_appender, prune_expired, Guard, ObservedWriter};
use super::format::{DiagnosticEvent, DiagnosticFields};
use super::{elapsed_ms, report_stderr, LOG_DIR, RECORD_LIMIT};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(super) fn selected_level(value: Option<&str>) -> tracing::level_filters::LevelFilter {
    use tracing::level_filters::LevelFilter;
    match value.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("debug") => LevelFilter::DEBUG,
        Some(value) if value.eq_ignore_ascii_case("warning") => LevelFilter::WARN,
        Some(value) if value.eq_ignore_ascii_case("error") => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}

/// 最早入口初始化；只有全局 subscriber 安装成功才返回 Some 并接管 panic。
pub fn init(state_dir: &Path) -> Option<Guard> {
    use tracing_subscriber::{
        filter::filter_fn, layer::SubscriberExt, util::SubscriberInitExt, Layer,
    };
    let started = Instant::now();
    let primary = state_dir.join(LOG_DIR);
    let (directory, appender, fallback_error) = match open_appender(&primary) {
        Ok(appender) => (primary, appender, None),
        Err(primary_error) => {
            let Some(fallback) = fallback_directory(state_dir) else {
                report_stderr(format_args!(
                    "event=diagnostic_init_failed stage=fallback_directory error_type=Unavailable"
                ));
                return None;
            };
            match open_appender(&fallback) {
                Ok(appender) => (fallback, appender, Some(primary_error)),
                Err(error) => {
                    report_stderr(format_args!("event=diagnostic_init_failed stage=primary error_type={:?} error_code={:?}", primary_error.kind(), primary_error.raw_os_error()));
                    report_stderr(format_args!("event=diagnostic_init_failed stage=fallback error_type={:?} error_code={:?}", error.kind(), error.raw_os_error()));
                    return None;
                }
            }
        }
    };
    let file_path = appender.path.clone();
    let (writer, worker) = tracing_appender::non_blocking(ObservedWriter {
        appender: io::BufWriter::with_capacity(RECORD_LIMIT, appender),
        failure_reported: Arc::new(AtomicBool::new(false)),
    });
    let dropped = writer.error_counter();
    let level_value = std::env::var("JCH_LOG_LEVEL").ok();
    let level = selected_level(level_value.as_deref());
    // 阈值不能禁用 INFO 操作 span：WARN/ERROR 仍需关联 ID。
    // 不放宽 DEBUG/TRACE 细节阈值，不读取 RUST_LOG 或启用第三方正文。
    let filter = filter_fn(move |metadata| {
        (metadata.target().starts_with("jchtools")
            || metadata.target() == "JchTools"
            || metadata.target().starts_with("JchTools::")
            || metadata.target().starts_with("snap_ocr_worker")
            || metadata.target() == "jchocr"
            || metadata.target().starts_with("jchocr::"))
            && (*metadata.level() <= level
                || (metadata.is_span() && *metadata.level() <= tracing::Level::INFO))
    });
    let layer = tracing_subscriber::fmt::layer()
        .fmt_fields(DiagnosticFields)
        .event_format(DiagnosticEvent {
            dropped: dropped.clone(),
            reported_drops: AtomicUsize::new(0),
        })
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(filter);
    let workers = {
        #[cfg(feature = "perf-tracing")]
        {
            let perf_parts = crate::perf::appender(state_dir).map(tracing_appender::non_blocking);
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
                    tracing_subscriber::registry()
                        .with(layer)
                        .with(perf_layer)
                        .try_init()
                        .ok()
                        .map(|()| vec![worker, perf_worker])
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
    let Some(workers) = workers else {
        report_stderr(format_args!(
            "event=diagnostic_init_failed stage=subscriber error_type=SubscriberConflict"
        ));
        return None;
    };
    install_panic_hook();
    let mode = if cfg!(debug_assertions) {
        "debug-assertions-enabled"
    } else {
        "debug-assertions-disabled"
    };
    tracing::info!(component = "logging", event = "application_started", application = "JchTools", role = detect_role(), version = env!("CARGO_PKG_VERSION"), log_file = %file_path.display(), %level, mode, debug_assertions = cfg!(debug_assertions), elapsed_ms = elapsed_ms(started), "应用诊断日志已启动");
    if let Some(error) = fallback_error {
        tracing::warn!(component = "logging", event = "diagnostic_directory_fallback", stage = "primary", error_type = ?error.kind(), error_code = error.raw_os_error(), "主日志目录不可写，已使用用户级备用目录");
    }
    let (removed, failed) = prune_expired(&directory);
    if failed > 0 {
        tracing::warn!(
            component = "logging",
            event = "diagnostic_retention_failed",
            stage = "retention",
            error_type = "FileIo",
            failed_count = failed,
            "部分过期诊断日志清理失败"
        );
    }
    if removed > 0 || failed > 0 {
        tracing::info!(
            component = "logging",
            event = "diagnostic_retention_completed",
            removed_count = removed,
            failed_count = failed,
            "诊断日志保留期清理完成"
        );
    }
    Some(Guard {
        workers,
        dropped,
        started,
    })
}

static PROCESS_GUARD: Mutex<Option<Guard>> = Mutex::new(None);

pub fn hold_for_process(guard: Guard) {
    let mut slot = PROCESS_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some(guard);
}

/// 正常/强退统一刷盘，重复调用不会重复停止事件。
pub fn flush_before_exit() {
    let mut slot = PROCESS_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // 同时退出的调用必须等待实际刷盘结束，不能在空槽可见后提前 process::exit。
    drop(slot.take());
}

pub struct FlushOnDrop;
impl Drop for FlushOnDrop {
    fn drop(&mut self) {
        flush_before_exit();
    }
}

fn detect_role() -> &'static str {
    let executable = std::env::current_exe().ok();
    role_for_process(executable.as_deref(), std::env::args_os().nth(1).as_deref())
}

pub(super) fn role_for_process(
    executable: Option<&Path>,
    mode: Option<&std::ffi::OsStr>,
) -> &'static str {
    let role = role_for_mode(mode);
    // jchocr 也会以内部代理模式重启自身；明确的后台角色优先于程序名。
    if role == "gui"
        && executable
            .and_then(Path::file_stem)
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("jchocr"))
    {
        "jchocr"
    } else {
        role
    }
}

pub(super) fn role_for_mode(mode: Option<&std::ffi::OsStr>) -> &'static str {
    match mode {
        Some(flag) if flag == std::ffi::OsStr::new("--codeagent-service") => "codeagent-service",
        Some(flag) if flag == std::ffi::OsStr::new("--xberg-broker") => "xberg-broker",
        Some(flag)
            if flag
                .to_str()
                .is_some_and(|flag| flag.starts_with("--service")) =>
        {
            "snap-ocr-service"
        }
        _ => "gui",
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload_kind = if info.payload().is::<&str>() || info.payload().is::<String>() {
            "string"
        } else {
            "opaque"
        };
        let location = info
            .location()
            .map(|location| {
                let file = location.file().rsplit(['/', '\\']).next().unwrap_or("?");
                format!("{file}:{}:{}", location.line(), location.column())
            })
            .unwrap_or_default();
        tracing::error!(component = "logging", event = "application_panicked", payload_kind, %location, "进程 panic");
        default_hook(info);
    }));
}
