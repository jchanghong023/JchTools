use super::*;
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;

// tracing-appender 保留非阻塞队列；文件容量、分段和保留期只由 BoundedAppender 管理。
pub(super) struct ObservedWriter {
    pub(super) appender: io::BufWriter<BoundedAppender>,
    pub(super) failure_reported: Arc<AtomicBool>,
}

impl io::Write for ObservedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.appender.write(bytes);
        if let Err(error) = &result {
            self.report(error);
        }
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        let result = self.appender.flush();
        if let Err(error) = &result {
            self.report(error);
        }
        result
    }
}

impl ObservedWriter {
    fn report(&self, error: &io::Error) {
        if !self.failure_reported.swap(true, Ordering::Relaxed) {
            report_stderr(format_args!(
                "event=diagnostic_write_failed error_type={:?} error_code={:?}",
                error.kind(),
                error.raw_os_error()
            ));
        }
    }
}

/// 持有原有非阻塞 worker；Drop 先写退出终态，再由 WorkerGuard 刷盘。
pub struct Guard {
    pub(super) workers: Vec<tracing_appender::non_blocking::WorkerGuard>,
    pub(super) dropped: tracing_appender::non_blocking::ErrorCounter,
    pub(super) started: Instant,
}

impl Drop for Guard {
    fn drop(&mut self) {
        tracing::info!(
            component = "logging",
            event = "application_stopping",
            "应用正在停止"
        );
        let dropped = self.dropped.dropped_lines();
        if dropped > 0 {
            report_stderr(format_args!(
                "event=diagnostic_queue_dropped error_type=QueueFull dropped_lines={dropped}"
            ));
        }
        tracing::info!(
            component = "logging",
            event = "application_stopped",
            elapsed_ms = elapsed_ms(self.started),
            dropped_lines = dropped,
            "应用已停止"
        );
        // Explicitly drop now rather than relying on field drop order.
        self.workers.clear();
    }
}

pub(super) fn fallback_directory(state_dir: &Path) -> Option<PathBuf> {
    if cfg!(any(test, feature = "test-hooks")) {
        Some(state_dir.join("logs-fallback"))
    } else {
        directories_next::BaseDirs::new()
            .map(|directories| directories.data_local_dir().join("JchTools").join("Logs"))
    }
}

pub(super) fn open_appender(directory: &Path) -> io::Result<BoundedAppender> {
    std::fs::create_dir_all(directory)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.with_extension("retention.lock"))?;
    let mut appender = BoundedAppender {
        directory: directory.to_owned(),
        lock,
        writer: None,
        path: PathBuf::new(),
        day: u64::MAX,
        segment: 0,
    };
    appender.lock.lock()?;
    let result = appender.prepare(RECORD_LIMIT);
    let unlock = appender.lock.unlock();
    result.and(unlock)?;
    Ok(appender)
}

pub(super) struct BoundedAppender {
    directory: PathBuf,
    lock: std::fs::File,
    writer: Option<std::fs::File>,
    pub(super) path: PathBuf,
    day: u64,
    pub(super) segment: u64,
}

impl BoundedAppender {
    fn prepare(&mut self, incoming: usize) -> io::Result<()> {
        let day = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_secs()
            / 86_400;
        if day != self.day {
            self.writer = None;
            self.day = day;
            self.segment = 0;
            self.path = self.segment_path();
        }
        loop {
            let metadata = match std::fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.is_file() => Some(metadata),
                Ok(_) => return Err(io::Error::other("日志路径不是普通文件")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let size = metadata.as_ref().map_or(0, std::fs::Metadata::len);
            if size > FILE_LIMIT {
                return Err(io::Error::other("已有诊断日志超出容量，需要另行批准迁移"));
            }
            // 只有当前 writer 已持有的文件才能继续写；重建或轮转时跳过已占用分段。
            if self.writer.is_none() && metadata.is_some() {
                self.advance_segment()?;
                continue;
            }
            if size.saturating_add(u64::try_from(incoming).unwrap_or(u64::MAX)) <= FILE_LIMIT {
                // 所有进程共享同一把锁。其他进程可能已按策略淘汰本进程的旧分段；
                // 文件不存在时丢弃旧句柄，重新排他创建，避免向已删除的文件写入。
                if metadata.is_none() {
                    self.writer = None;
                }
                if self.writer.is_none() {
                    retain_owned(&self.directory, Some(&self.path), 1)?;
                    match std::fs::OpenOptions::new()
                        .append(true)
                        .create_new(true)
                        .open(&self.path)
                    {
                        Ok(file) => self.writer = Some(file),
                        // 排他创建是最终边界；重新检查文件类型并跳过已有普通文件。
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                        Err(error) => return Err(error),
                    }
                }
                return Ok(());
            }
            self.advance_segment()?;
        }
    }

    fn advance_segment(&mut self) -> io::Result<()> {
        self.writer = None;
        self.segment = self
            .segment
            .checked_add(1)
            .ok_or_else(|| io::Error::other("日志分段编号耗尽"))?;
        self.path = self.segment_path();
        Ok(())
    }

    fn segment_path(&self) -> PathBuf {
        let mut timestamp = String::new();
        let _ =
            tracing_subscriber::fmt::time::SystemTime.format_time(&mut Writer::new(&mut timestamp));
        let date = timestamp.get(..10).unwrap_or("unknown-date");
        let run_id = process_run_id();
        let name = if self.segment == 0 {
            format!("{LOG_FILE_PREFIX}.{date}.{run_id}.log")
        } else {
            format!("{LOG_FILE_PREFIX}.{date}.{run_id}.{}.log", self.segment)
        };
        self.directory.join(name)
    }

    fn write_locked(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // 非阻塞 worker 按完整事件提交；缓冲批次也只在事件边界刷盘。
        // 一批最多 RECORD_LIMIT 字节时一次加锁/容量检查，不逐条访问文件系统。
        if bytes.len() <= RECORD_LIMIT {
            self.prepare(bytes.len())?;
            self.writer
                .as_mut()
                .ok_or_else(|| io::Error::other("日志写入器不可用"))?
                .write_all(bytes)?;
            return Ok(bytes.len());
        }
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            let summary;
            let record = if line.len() > RECORD_LIMIT {
                summary = summarize_record(line)?;
                summary.as_bytes()
            } else {
                line
            };
            self.prepare(record.len())?;
            self.writer
                .as_mut()
                .ok_or_else(|| io::Error::other("日志写入器不可用"))?
                .write_all(record)?;
        }
        Ok(bytes.len())
    }
}

impl io::Write for BoundedAppender {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.lock.lock()?;
        let result = self.write_locked(bytes);
        let unlock = self.lock.unlock();
        result.and_then(|count| unlock.map(|()| count))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.as_mut().map_or(Ok(()), io::Write::flush)
    }
}

// 只在整条记录超限时分配摘要；不截断 UTF-8 或带引号的字段。
// 保留事件身份、错误元数据及首个可用关联 ID，不复制 span/正文大块内容。
fn summarize_record(bytes: &[u8]) -> io::Result<String> {
    const KEYS: [&str; 12] = [
        "timestamp",
        "level",
        "pid",
        "thread_id",
        "component",
        "event",
        "stage",
        "error_type",
        "error_code",
        "elapsed_ms",
        "operation_id",
        "request_id",
    ];
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let mut fields: [Option<&str>; 12] = [None; 12];
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if escaped {
            escaped = false;
        } else if quoted && character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if !quoted && character.is_whitespace() {
            let field = &text[start..index];
            let key = field.split_once('=').map_or("", |(key, _)| key);
            if let Some(slot) = KEYS.iter().position(|candidate| *candidate == key) {
                // 头部身份和首个关联 ID 保持；事件字段位于 span 链之后，
                // 因而阶段/错误/耗时必须取最后一个，不能被外层 span 同名字段遮蔽。
                if fields[slot].is_none() || (6..10).contains(&slot) {
                    fields[slot] = Some(field);
                }
            }
            start = index + character.len_utf8();
        }
    }
    let capacity = fields
        .iter()
        .flatten()
        .map(|field| field.len() + 1)
        .sum::<usize>()
        + 80;
    let mut summary = String::with_capacity(capacity.min(RECORD_LIMIT));
    for field in fields.into_iter().flatten() {
        if summary.len() + field.len() + 80 < RECORD_LIMIT {
            summary.push_str(field);
            summary.push(' ');
        }
    }
    let _ = writeln!(summary, "truncated=true original_bytes={}", bytes.len());
    Ok(summary)
}

fn log_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
        && value[5..7]
            .parse::<u8>()
            .is_ok_and(|month| (1..=12).contains(&month))
        && value[8..10]
            .parse::<u8>()
            .is_ok_and(|day| (1..=31).contains(&day))
}

pub(super) fn product_log_name(name: &str) -> bool {
    if let Some(date) = name.strip_prefix("jchtools.log.") {
        return log_date(date);
    }
    let Some(rest) = name
        .strip_prefix("jchtools.")
        .and_then(|rest| rest.strip_suffix(".log"))
    else {
        return false;
    };
    let mut parts = rest.split('.');
    let Some(date) = parts.next() else {
        return false;
    };
    let Some(identity) = parts.next() else {
        return false;
    };
    let numeric =
        |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    let mut run = identity.split('-');
    let owned_identity = numeric(identity)
        || (run.next().is_some_and(numeric)
            && run.next().is_some_and(numeric)
            && run.next().is_some_and(numeric)
            && run.next().is_none());
    log_date(date) && owned_identity && parts.next().is_none_or(numeric) && parts.next().is_none()
}

fn retention_failure(stage: &'static str, error: &io::Error) {
    tracing::warn!(component = "logging", event = "diagnostic_retention_io_failed", stage, error_type = ?error.kind(), error_code = error.raw_os_error(), "诊断日志保留期清理发生文件错误");
}

/// 唯一保留机制：先校验归属和容量，再按修改时间、文件名排序；不遍历链接。
/// protected 是本次写入的文件；reserve 为即将创建的活动文件预留名额。
fn retain_owned(directory: &Path, protected: Option<&Path>, reserve: usize) -> io::Result<u64> {
    let mut owned = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !product_log_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > FILE_LIMIT {
            return Err(io::Error::other("已有诊断日志超出容量，需要另行批准迁移"));
        }
        owned.push((metadata.modified()?, entry.path()));
    }
    owned.sort_unstable();
    let cutoff = SystemTime::now() - Duration::from_secs(RETENTION_DAYS * 24 * 3600);
    let mut remaining = owned.len();
    let mut removed = 0;
    for (modified, path) in owned {
        if protected == Some(path.as_path()) {
            continue;
        }
        if modified < cutoff || remaining + reserve > FILE_COUNT_LIMIT {
            std::fs::remove_file(path)?;
            remaining -= 1;
            removed += 1;
        }
    }
    Ok(removed)
}

pub(super) fn prune_expired(directory: &Path) -> (u64, u64) {
    // 与写入器使用相同锁和相同排序/删除策略，不启动第二套清理器。
    let result = (|| {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.with_extension("retention.lock"))?;
        lock.lock()?;
        let retained = retain_owned(directory, None, 0);
        let unlock = lock.unlock();
        retained.and_then(|removed| unlock.map(|()| removed))
    })();
    match result {
        Ok(removed) => (removed, 0),
        Err(error) => {
            retention_failure("retention", &error);
            (0, 1)
        }
    }
}
