//! 常开诊断日志（P-10）：与 perf 共用 tracing registry，不读取外部 RUST_LOG。
//!
//! 主落点为状态目录 `logs/jchtools.<UTC日期>.<pid>[.<分段>].log`；每天新建，
//! 单文件不超过 1 MiB，自有文件最多 10 个，最长保留 14 天（兼容旧 `jchtools.log.<日期>`）。
//! 主目录不可写时使用当前用户 LocalAppData 下的 `JchTools/Logs`；测试构建只使用隔离状态目录
//! 下的 `logs-fallback`。两处失败仅向 stderr 报告错误类型/码，不影响业务。
//!
//! 每个事件为 UTC 微秒时间戳的单行 key=value，包含级别、PID、线程、组件、
//! 稳定 event 与当前 span 链。文本有界，控制字符转义，已知凭据与 URL 去敏；
//! 调用方仍不得提交文件/OCR/模型/终端/协议正文。panic 只记录元数据。

use std::fmt::{self, Write as _};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::{FmtContext, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

pub const LOG_DIR: &str = "logs";
const LOG_FILE_PREFIX: &str = "jchtools";
const RETENTION_DAYS: u64 = 14;
const TEXT_LIMIT: usize = 2048;
const FILE_LIMIT: u64 = 1_048_576;
const FILE_COUNT_LIMIT: usize = 10;
const RECORD_LIMIT: usize = 65_536;

fn selected_level(value: Option<&str>) -> tracing::level_filters::LevelFilter {
    use tracing::level_filters::LevelFilter;
    match value.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("debug") => LevelFilter::DEBUG,
        Some(value) if value.eq_ignore_ascii_case("warning") => LevelFilter::WARN,
        Some(value) if value.eq_ignore_ascii_case("error") => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}
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

/// 仅保留 URL 的 scheme/host/path，删除 userinfo、query、fragment。
/// 无法识别的 URL 不原样返回；这不是任意外部正文的安全摘要器。
pub fn safe_url(value: &str) -> String {
    let Some((scheme, rest)) = value.split_once("://") else {
        return "[invalid_url]".into();
    };
    if !["http", "https", "socks", "socks5", "ftp", "ssh"]
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
        || value.chars().any(char::is_control)
    {
        return "[invalid_url]".into();
    }
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let clean = &rest[..end];
    let authority_end = clean.find('/').unwrap_or(clean.len());
    let authority = &clean[..authority_end];
    let host = authority.rsplit('@').next().unwrap_or_default();
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return "[invalid_url]".into();
    }
    let mut output = BoundedText {
        value: String::new(),
        truncated: false,
    };
    let _ = write!(output, "{scheme}://{host}{}", &clean[authority_end..]);
    if output.truncated {
        output.value.push_str("[truncated]");
    }
    bounded(&output.value)
}

/// 去敏已知凭据/URL并有界；模型、ACP、OCR错误正文不得交给本函数冒称安全。
pub fn safe_error(value: &str) -> String {
    // Never cut an unparsed URL before its @/query delimiter: a long userinfo
    // could otherwise be mistaken for a hostname and expose its prefix.
    if value.len() > TEXT_LIMIT {
        return "[truncated]".into();
    }
    let mut result = String::with_capacity(value.len());
    let mut at = 0;
    while at < value.len() {
        let remaining = &value[at..];
        if let Some(scheme_end) = remaining.find("://").filter(|end| *end <= 8) {
            let scheme = &remaining[..scheme_end];
            if scheme
                .chars()
                .all(|character| character.is_ascii_alphabetic())
            {
                // comma/apostrophe/parenthesis are valid userinfo characters.
                // Parse the whole authority before stripping credentials.
                let end = remaining
                    .find(char::is_whitespace)
                    .unwrap_or(remaining.len());
                result.push_str(&safe_url(&remaining[..end]));
                at += end;
                continue;
            }
        }
        let word_end = remaining
            .find(|character: char| {
                !character.is_ascii_alphanumeric() && character != '_' && character != '-'
            })
            .unwrap_or(remaining.len());
        if word_end > 0 {
            let word = &remaining[..word_end];
            let tail = remaining[word_end..].trim_start_matches([' ', '\t', '"', '\'']);
            if (sensitive_name(word)
                && (tail.starts_with(['=', ':'])
                    || ["bearer", "authorization", "cookie", "set-cookie"]
                        .iter()
                        .any(|name| word.eq_ignore_ascii_case(name))))
                || ["sk-", "ghp_", "github_pat_"].iter().any(|prefix| {
                    word.get(..prefix.len())
                        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
                })
            {
                // 保守丢弃剩余诊断文本，避免引号、空白、多值 Cookie 绕过去敏。
                result.push_str("[redacted]");
                break;
            }
            result.push_str(word);
            at += word_end;
        } else {
            let Some(character) = remaining.chars().next() else {
                break;
            };
            result.push(character);
            at += character.len_utf8();
        }
    }
    // helper 本身也不返回物理控制字符；格式器随后转义反斜杠/引号。
    let mut escaped = String::with_capacity(result.len());
    for character in result.chars() {
        if character.is_control() {
            let _ = write!(escaped, "\\u{{{:x}}}", u32::from(character));
        } else {
            escaped.push(character);
        }
    }
    bounded(&escaped)
}

fn sensitive_name(name: &str) -> bool {
    [
        "password",
        "passwd",
        "pwd",
        "authorization",
        "bearer",
        "token",
        "access_token",
        "refresh_token",
        "api_key",
        "apikey",
        "api-key",
        "secret",
        "client_secret",
        "private_key",
        "cookie",
        "set_cookie",
        "set-cookie",
        "session",
        "session_token",
        "credential",
        "credentials",
    ]
    .iter()
    .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
}

fn bounded(value: &str) -> String {
    if value.len() <= TEXT_LIMIT {
        return value.to_owned();
    }
    let mut end = TEXT_LIMIT - "[truncated]".len();
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}[truncated]", &value[..end])
}

// Display/Debug may write arbitrary chunks; collect only a bounded prefix, without
// first allocating the complete rendered value. Numeric fields use Writer directly.
struct BoundedText {
    value: String,
    truncated: bool,
}

impl fmt::Write for BoundedText {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let available = TEXT_LIMIT.saturating_sub(self.value.len());
        let mut end = value.len().min(available);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.value.push_str(&value[..end]);
        self.truncated |= end < value.len();
        if self.truncated {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

fn render(value: &dyn fmt::Debug) -> String {
    let mut output = BoundedText {
        value: String::new(),
        truncated: false,
    };
    let _ = write!(output, "{value:?}");
    if output.truncated {
        return "[truncated]".into();
    }
    output.value
}

fn quoted(writer: &mut Writer<'_>, value: &str) -> fmt::Result {
    writer.write_char('"')?;
    for character in value.chars() {
        match character {
            '"' => writer.write_str("\\\"")?,
            '\\' => writer.write_str("\\\\")?,
            '\n' => writer.write_str("\\n")?,
            '\r' => writer.write_str("\\r")?,
            '\t' => writer.write_str("\\t")?,
            character if character.is_control() => {
                write!(writer, "\\u{{{:x}}}", u32::from(character))?;
            }
            character => writer.write_char(character)?,
        }
    }
    writer.write_char('"')
}

fn atom(writer: &mut Writer<'_>, value: &str) -> fmt::Result {
    if !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ':' | '.')
        })
    {
        writer.write_str(value)
    } else {
        quoted(writer, value)
    }
}

#[derive(Clone, Copy)]
struct DiagnosticFields;

struct FieldVisitor<'a, 'writer> {
    writer: &'a mut Writer<'writer>,
    result: fmt::Result,
    skip_header: bool,
}

impl FieldVisitor<'_, '_> {
    fn field(&mut self, field: &Field, value: impl fmt::Display) {
        if self.skip_header && matches!(field.name(), "component" | "event" | "pid") {
            return;
        }
        if self.result.is_ok() {
            self.result = if sensitive_name(field.name()) {
                write!(self.writer, " {}=\"[redacted]\"", field.name())
            } else {
                write!(self.writer, " {}={value}", field.name())
            };
        }
    }

    fn text(&mut self, field: &Field, value: &str) {
        if self.skip_header && matches!(field.name(), "component" | "event" | "pid") {
            return;
        }
        if self.result.is_ok() {
            let safe = if sensitive_name(field.name()) {
                "[redacted]".into()
            } else {
                safe_error(value)
            };
            self.result =
                write!(self.writer, " {}=", field.name()).and_then(|()| quoted(self.writer, &safe));
        }
    }
}

impl Visit for FieldVisitor<'_, '_> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.field(field, value);
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.field(field, value);
    }
    fn record_i128(&mut self, field: &Field, value: i128) {
        self.field(field, value);
    }
    fn record_u128(&mut self, field: &Field, value: u128) {
        self.field(field, value);
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.field(field, value);
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.field(field, value);
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.text(field, value);
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.text(field, &render(value));
    }
}

impl<'writer> FormatFields<'writer> for DiagnosticFields {
    fn format_fields<R: RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut visitor = FieldVisitor {
            writer: &mut writer,
            result: Ok(()),
            skip_header: false,
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

#[derive(Default)]
struct HeaderVisitor {
    component: Option<String>,
    event: Option<String>,
}

impl Visit for HeaderVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "component" => self.component = Some(safe_error(value)),
            "event" => self.event = Some(safe_error(value)),
            _ => {}
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            "component" => self.component = Some(safe_error(&render(value))),
            "event" => self.event = Some(safe_error(&render(value))),
            _ => {}
        }
    }
}

struct DiagnosticEvent {
    dropped: tracing_appender::non_blocking::ErrorCounter,
    reported_drops: AtomicUsize,
}

fn header(
    writer: &mut Writer<'_>,
    level: tracing::Level,
    component: &str,
    event: &str,
) -> fmt::Result {
    writer.write_str("timestamp=")?;
    tracing_subscriber::fmt::time::SystemTime.format_time(writer)?;
    write!(
        writer,
        " level={level} pid={} thread_id={:?} component=",
        std::process::id(),
        std::thread::current().id()
    )?;
    atom(writer, component)?;
    writer.write_str(" event=")?;
    atom(writer, event)
}

impl<S> FormatEvent<S, DiagnosticFields> for DiagnosticEvent
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, DiagnosticFields>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let dropped = self.dropped.dropped_lines();
        let previous = self.reported_drops.swap(dropped, Ordering::Relaxed);
        if dropped > previous {
            // Queue overload must remain visible even when the warning itself cannot enqueue.
            report_stderr(format_args!(
                "event=diagnostic_queue_dropped error_type=QueueFull dropped_lines={dropped}"
            ));
            header(
                &mut writer,
                tracing::Level::WARN,
                "logging",
                "diagnostic_queue_dropped",
            )?;
            writeln!(
                writer,
                " dropped_lines={dropped} message=\"诊断日志队列记录丢失\""
            )?;
        }
        let mut fields = HeaderVisitor::default();
        event.record(&mut fields);
        header(
            &mut writer,
            *event.metadata().level(),
            fields
                .component
                .as_deref()
                .unwrap_or(event.metadata().target()),
            fields.event.as_deref().unwrap_or("diagnostic"),
        )?;
        if let Some(scope) = context.event_scope() {
            for span in scope.from_root() {
                write!(writer, " span_id={} span=", span.id().into_u64())?;
                quoted(&mut writer, span.name())?;
                if let Some(fields) = span.extensions().get::<FormattedFields<DiagnosticFields>>() {
                    writer.write_str(&fields.fields)?;
                }
            }
        }
        let mut visitor = FieldVisitor {
            writer: &mut writer,
            result: Ok(()),
            skip_header: true,
        };
        event.record(&mut visitor);
        visitor.result?;
        writeln!(writer)
    }
}

// tracing-appender 保留文件写入与非阻塞队列；容量、分段和保留期由同一个扩展管理。
// native max_log_files 不启用，避免它按另一个顺序清理历史。
struct ObservedWriter {
    appender: io::BufWriter<BoundedAppender>,
    failure_reported: Arc<AtomicBool>,
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
    workers: Vec<tracing_appender::non_blocking::WorkerGuard>,
    dropped: tracing_appender::non_blocking::ErrorCounter,
    started: Instant,
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

fn fallback_directory(state_dir: &Path) -> Option<PathBuf> {
    if cfg!(any(test, feature = "test-hooks")) {
        Some(state_dir.join("logs-fallback"))
    } else {
        directories_next::BaseDirs::new()
            .map(|directories| directories.data_local_dir().join("JchTools").join("Logs"))
    }
}

fn open_appender(directory: &Path) -> io::Result<BoundedAppender> {
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

fn native_appender(
    directory: &Path,
    name: &str,
) -> io::Result<tracing_appender::rolling::RollingFileAppender> {
    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::NEVER)
        .filename_prefix(name)
        .build(directory)
        .map_err(|error| {
            // InitError preserves its io::Error as source. Never persist its Display (path).
            use std::error::Error;
            let source = error
                .source()
                .and_then(|source| source.downcast_ref::<io::Error>());
            source.map_or_else(
                || io::Error::other("appender initialization"),
                |source| {
                    source.raw_os_error().map_or_else(
                        || io::Error::from(source.kind()),
                        io::Error::from_raw_os_error,
                    )
                },
            )
        })
}

struct BoundedAppender {
    directory: PathBuf,
    lock: std::fs::File,
    writer: Option<tracing_appender::rolling::RollingFileAppender>,
    path: PathBuf,
    day: u64,
    segment: u64,
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
            if size.saturating_add(u64::try_from(incoming).unwrap_or(u64::MAX)) <= FILE_LIMIT {
                // 所有进程共享同一把锁。其他进程可能已按策略淘汰本进程的旧分段；
                // 文件不存在时丢弃旧句柄，重新打开，避免向已删除的文件写入。
                if metadata.is_none() {
                    self.writer = None;
                }
                if self.writer.is_none() {
                    retain_owned(
                        &self.directory,
                        Some(&self.path),
                        usize::from(metadata.is_none()),
                    )?;
                    let name = self
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| io::Error::other("日志文件名无效"))?;
                    self.writer = Some(native_appender(&self.directory, name)?);
                }
                return Ok(());
            }
            self.writer = None;
            self.segment = self
                .segment
                .checked_add(1)
                .ok_or_else(|| io::Error::other("日志分段编号耗尽"))?;
            self.path = self.segment_path();
        }
    }

    fn segment_path(&self) -> PathBuf {
        let mut timestamp = String::new();
        let _ =
            tracing_subscriber::fmt::time::SystemTime.format_time(&mut Writer::new(&mut timestamp));
        let date = timestamp.get(..10).unwrap_or("unknown-date");
        let pid = std::process::id();
        let name = if self.segment == 0 {
            format!("{LOG_FILE_PREFIX}.{date}.{pid}.log")
        } else {
            format!("{LOG_FILE_PREFIX}.{date}.{pid}.{}.log", self.segment)
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

/// 最早入口初始化；只有全局 subscriber 安装成功才返回 Some 并接管 panic。
pub fn init(state_dir: &Path) -> Option<Guard> {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};
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
    let Ok(filter) = EnvFilter::try_new(format!("jchtools={level},snap_ocr_worker={level}")) else {
        report_stderr(format_args!(
            "event=diagnostic_init_failed stage=filter error_type=InvalidFilter"
        ));
        return None;
    };
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
            let perf_directory = state_dir.join(crate::perf::LOG_DIR);
            let perf_parts = std::fs::create_dir_all(&perf_directory)
                .ok()
                .and_then(|()| {
                    tracing_appender::rolling::RollingFileAppender::builder()
                        .rotation(tracing_appender::rolling::Rotation::DAILY)
                        .filename_prefix(crate::perf::LOG_FILE_PREFIX)
                        .build(&perf_directory)
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
    tracing::info!(component = "logging", event = "application_started", application = "JchTools", role = detect_role(), version = env!("CARGO_PKG_VERSION"), log_file = %file_path.display(), %level, mode = detect_role(), debug_assertions = cfg!(debug_assertions), elapsed_ms = elapsed_ms(started), "应用诊断日志已启动");
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
    match std::env::args_os().nth(1).as_deref() {
        Some(flag) if flag == std::ffi::OsStr::new("--acp-http-service") => "acp-http-service",
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

fn product_log_name(name: &str) -> bool {
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
    let Some(pid) = parts.next() else {
        return false;
    };
    let numeric =
        |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    log_date(date) && numeric(pid) && parts.next().is_none_or(numeric) && parts.next().is_none()
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

fn prune_expired(directory: &Path) -> (u64, u64) {
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
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn workspace() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|path| path.join("scripts/make_tmp.py").is_file())
            .unwrap();
        let directory = root.join(".tmp/logging-tests");
        assert!(std::process::Command::new("python")
            .arg(root.join("scripts/make_tmp.py"))
            .args(["workspace", "--destination"])
            .arg(&directory)
            .current_dir(root)
            .status()
            .unwrap()
            .success());
        tempfile::Builder::new()
            .prefix("diagnostics-")
            .tempdir_in(directory)
            .unwrap()
    }

    fn in_child(name: &str) -> bool {
        const CHILD: &str = "JCHTOOLS_LOGGING_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "隔离进程中的日志行为断言必须通过");
        false
    }

    fn contents(directory: &Path) -> String {
        std::fs::read_dir(directory)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().is_file())
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap())
            .collect()
    }

    #[test]
    fn failed_log_file_initialization_does_not_call_panic_hook() {
        if !in_child("logging::tests::failed_log_file_initialization_does_not_call_panic_hook") {
            return;
        }
        let temp = workspace();
        let directory = temp.path().join(LOG_DIR);
        drop(open_appender(&directory).unwrap());
        let path = std::fs::read_dir(&directory)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let panic_count = Arc::new(AtomicUsize::new(0));
        let hook_count = Arc::clone(&panic_count);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| {
            hook_count.fetch_add(1, Ordering::Relaxed);
        }));
        let result = std::panic::catch_unwind(|| init(temp.path()));
        std::panic::set_hook(previous);
        let guard = result.unwrap();
        assert!(guard.is_some(), "仅主目录失败时必须使用隔离备用目录");
        drop(guard);
        assert_eq!(panic_count.load(Ordering::Relaxed), 0);
        assert!(contents(&temp.path().join("logs-fallback"))
            .contains("event=diagnostic_directory_fallback"));
    }

    #[test]
    fn both_log_directories_blocked_do_not_panic() {
        if !in_child("logging::tests::both_log_directories_blocked_do_not_panic") {
            return;
        }
        let temp = workspace();
        std::fs::write(temp.path().join(LOG_DIR), b"blocked").unwrap();
        std::fs::write(temp.path().join("logs-fallback"), b"blocked").unwrap();
        let panic_count = Arc::new(AtomicUsize::new(0));
        let hook_count = Arc::clone(&panic_count);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| {
            hook_count.fetch_add(1, Ordering::Relaxed);
        }));
        let result = std::panic::catch_unwind(|| init(temp.path()));
        std::panic::set_hook(previous);
        assert!(result.unwrap().is_none());
        assert_eq!(panic_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn panic_payload_is_not_written_to_diagnostic_log() {
        if !in_child("logging::tests::panic_payload_is_not_written_to_diagnostic_log") {
            return;
        }
        let temp = workspace();
        let previous = std::panic::take_hook();
        let guard = init(temp.path()).unwrap();
        let result = std::panic::catch_unwind(|| {
            std::panic::panic_any("synthetic OCR payload C:/synthetic/private-image.png");
        });
        std::panic::set_hook(previous);
        drop(guard);
        assert!(result.is_err());
        let captured = contents(&temp.path().join(LOG_DIR));
        assert!(captured.contains("进程 panic"));
        assert!(captured.contains("event=application_panicked"));
        assert!(!captured.contains("synthetic OCR payload"));
        assert!(!captured.contains("private-image.png"));
    }

    #[test]
    fn warns_and_errors_reach_disk_file() {
        if !in_child("logging::tests::warns_and_errors_reach_disk_file") {
            return;
        }
        let temp = workspace();
        let guard = init(temp.path()).unwrap();
        tracing::warn!(event = "probe_warning", "p10-warn-marker");
        tracing::error!(event = "probe_error", path = "probe", "p10-error-marker");
        drop(guard);
        let found = contents(&temp.path().join(LOG_DIR));
        assert!(found.contains("p10-warn-marker") && found.contains("p10-error-marker"));
        assert!(found.contains("level=ERROR"));
    }

    #[test]
    fn disk_records_are_single_line_correlated_and_redacted() {
        if !in_child("logging::tests::disk_records_are_single_line_correlated_and_redacted") {
            return;
        }
        let temp = workspace();
        let guard = init(temp.path()).unwrap();
        let parent = operation_span_with_id("probe", "parent", "parent-probe-id");
        let _entered = parent.enter();
        let child = operation_span_with_id("probe", "child", "child-probe-id");
        let child_entered = child.enter();
        tracing::info!(event = "probe_started", display = %"first\nsecond\r\0", debug = ?"third\nfourth", password = "private-password", endpoint = "https://user:private-userinfo@example.invalid/a?token=private-query#private-fragment", detail = "Authorization: Bearer private-bearer", "诊断开始\n裸续行不允许");
        tracing::warn!(
            event = "probe_failed",
            stage = "connect",
            error_code = 10061,
            elapsed_ms = 7_u64,
            "诊断连接失败"
        );
        drop(child_entered);
        tracing::info!(event = "probe_completed", count = 2_u64, "诊断完成");
        let thread_span = parent.clone();
        std::thread::spawn(move || {
            thread_span.in_scope(|| {
                tracing::info!(
                    event = "probe_thread_completed",
                    token = 987_654_321_u64,
                    "关联线程已完成"
                );
            });
        })
        .join()
        .unwrap();
        hold_for_process(guard);
        flush_before_exit();
        flush_before_exit();
        let directory = temp.path().join(LOG_DIR);
        for entry in std::fs::read_dir(&directory).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(name.ends_with(&format!(".{}.log", std::process::id())));
        }
        let text = contents(&directory);
        for line in text.lines() {
            assert!(line.starts_with("timestamp="), "裸续行：{line}");
            let timestamp = line
                .split_whitespace()
                .next()
                .unwrap()
                .trim_start_matches("timestamp=");
            assert!(timestamp.ends_with('Z') && timestamp.split('.').nth(1).unwrap().len() >= 4);
            assert!(line.contains(&format!("pid={}", std::process::id())));
            assert!(
                line.contains(" level=")
                    && line.contains(" thread_id=")
                    && line.contains(" component=")
                    && line.contains(" event=")
            );
            assert!(!line.chars().any(char::is_control));
        }
        assert!(
            text.contains("event=probe_started")
                && text.contains("event=probe_failed")
                && text.contains("event=probe_completed")
        );
        let failed = text
            .lines()
            .find(|line| line.contains("event=probe_failed"))
            .unwrap();
        assert!(
            failed.contains("parent-probe-id")
                && failed.contains("child-probe-id")
                && failed.contains("stage=\"connect\"")
                && failed.contains("error_code=10061")
                && failed.contains("elapsed_ms=7")
        );
        for secret in [
            "private-password",
            "private-userinfo",
            "private-query",
            "private-fragment",
            "private-bearer",
        ] {
            assert!(!text.contains(secret), "凭据泄漏：{secret}");
        }
        let thread_event = text
            .lines()
            .find(|line| line.contains("event=probe_thread_completed"))
            .unwrap();
        assert!(thread_event.contains("parent-probe-id") && !thread_event.contains("987654321"));
        assert_eq!(text.matches("event=application_stopping").count(), 1);
        assert_eq!(text.matches("event=application_stopped").count(), 1);
        assert!(text.contains("https://example.invalid/a"));
    }

    #[test]
    fn retention_preserves_non_logs_and_recent_records() {
        let temp = workspace();
        let directory = temp.path().join(LOG_DIR);
        std::fs::create_dir_all(&directory).unwrap();
        let old = SystemTime::now() - Duration::from_hours(16 * 24);
        for name in [
            "jchtools.log.2026-01-01",
            "jchtools.2026-01-01.123.log",
            "jchtools.log.notes",
            "jchtools.2026-01-01.123.log.backup",
            "jchtools.2026-99-99.123.log",
            "business.log",
        ] {
            let file = std::fs::File::create(directory.join(name)).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        std::fs::write(directory.join("jchtools.2026-01-02.456.log"), b"recent").unwrap();
        std::fs::create_dir(directory.join("jchtools.log.2026-01-03")).unwrap();
        assert_eq!(prune_expired(&directory), (2, 0));
        for name in [
            "jchtools.log.notes",
            "jchtools.2026-01-01.123.log.backup",
            "jchtools.2026-99-99.123.log",
            "business.log",
            "jchtools.2026-01-02.456.log",
            "jchtools.log.2026-01-03",
        ] {
            assert!(directory.join(name).exists(), "不得删除：{name}");
        }
    }

    #[test]
    fn conflicting_subscriber_is_not_reported_as_initialized() {
        if !in_child("logging::tests::conflicting_subscriber_is_not_reported_as_initialized") {
            return;
        }
        tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
        let temp = workspace();
        assert!(init(temp.path()).is_none());
        assert!(!contents(&temp.path().join(LOG_DIR)).contains("event=application_started"));
    }

    #[test]
    fn safe_helpers_bound_diagnostic_data() {
        assert_eq!(
            safe_url("https://name:password@example.invalid/a?token=secret#fragment"),
            "https://example.invalid/a"
        );
        assert_eq!(safe_url("not a URL with private data"), "[invalid_url]");
        for text in [
            "password = private-value",
            "token=private-value",
            "Cookie: session=private-value",
            "Bearer private-value",
            "{\"api_key\":\"private-value\"}",
            "sk-private-value",
        ] {
            assert!(!safe_error(text).contains("private-value"));
        }
        let long = "诊".repeat(5000);
        let result = safe_error(&long);
        assert!(result.len() <= TEXT_LIMIT && result.ends_with("[truncated]"));
        assert!(!safe_error("a\nb\rc\0d").chars().any(char::is_control));
        let long_userinfo = format!(
            "https://{}@example.invalid/a?token=private-query",
            "private-credential".repeat(200)
        );
        assert_eq!(safe_url(&long_userinfo), "https://example.invalid/a");
        assert!(!safe_error(&long_userinfo).contains("private-credential"));
        assert_ne!(new_operation_id(), new_operation_id());
    }

    /// 覆盖 P-10：配置阈值接受大小写和首尾空白，非法值回到 INFO。
    #[test]
    fn log_level_selection_accepts_supported_values_and_defaults() {
        use tracing::level_filters::LevelFilter;
        for (input, expected) in [
            (None, LevelFilter::INFO),
            (Some(""), LevelFilter::INFO),
            (Some(" trace "), LevelFilter::INFO),
            (Some(" DeBuG \t"), LevelFilter::DEBUG),
            (Some("INFO"), LevelFilter::INFO),
            (Some(" Warning "), LevelFilter::WARN),
            (Some("error"), LevelFilter::ERROR),
        ] {
            assert_eq!(selected_level(input), expected);
        }
    }

    /// 覆盖 P-10：UTF-8 和 LF 均计入容量；淘汰按同一顺序且保留未知文件。
    #[test]
    fn bounded_sink_rotates_before_overflow_and_limits_owned_history() {
        let temp = workspace();
        let directory = temp.path().join(LOG_DIR);
        let mut appender = open_appender(&directory).unwrap();
        let unowned = directory.join("jchtools.2026-01-01.123.log.gz");
        std::fs::write(&unowned, b"unowned compressed history").unwrap();
        let record = format!("{}aaa\n", "诊".repeat((RECORD_LIMIT - 4) / 3));
        assert_eq!(record.len(), RECORD_LIMIT);
        for _ in 0..(FILE_COUNT_LIMIT + 2) * 16 {
            appender.write_all(record.as_bytes()).unwrap();
        }
        appender.write_all(b"final-observable-record\n").unwrap();
        appender.flush().unwrap();
        let owned: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .flatten()
            .filter(|entry| product_log_name(&entry.file_name().to_string_lossy()))
            .collect();
        assert_eq!(owned.len(), FILE_COUNT_LIMIT);
        assert!(owned
            .iter()
            .all(|entry| entry.metadata().unwrap().len() <= FILE_LIMIT));
        assert!(owned
            .iter()
            .any(|entry| entry.metadata().unwrap().len() == FILE_LIMIT));
        assert!(contents(&directory).contains("final-observable-record"));
        assert_eq!(
            std::fs::read(&unowned).unwrap(),
            b"unowned compressed history"
        );
    }

    /// 覆盖 P-10：超大单条事件保留身份、去敏诊断字段及截断标记，不拆成多行。
    #[test]
    fn oversized_record_preserves_identity_and_error_metadata() {
        let temp = workspace();
        let directory = temp.path().join(LOG_DIR);
        let mut appender = open_appender(&directory).unwrap();
        let record = format!(
            "timestamp=2026-10-11T00:00:00.000Z level=ERROR pid=1 thread_id=ThreadId(1) component=\"安全组件\" event=oversized_failure span_id=1 span=\"outer\" stage=\"外层阶段\" error_type=OuterContext error_code=1 elapsed_ms=999 detail=\"{}\" stage=\"连接失败\" error_type=ConnectionRefused error_code=10061 elapsed_ms=7 request_id=probe-id\n",
            "诊断\\\"".repeat(RECORD_LIMIT)
        );
        appender.write_all(record.as_bytes()).unwrap();
        appender.flush().unwrap();
        let captured = contents(&directory);
        assert_eq!(captured.lines().count(), 1);
        assert!(captured.len() <= RECORD_LIMIT);
        for field in [
            "event=oversized_failure",
            "component=\"安全组件\"",
            "stage=\"连接失败\"",
            "error_type=ConnectionRefused",
            "error_code=10061",
            "elapsed_ms=7",
            "request_id=probe-id",
            "truncated=true",
        ] {
            assert!(captured.contains(field), "缺少诊断字段：{field}");
        }
        assert!(!captured.contains("detail="));
    }

    /// 覆盖 P-10：已有超大日志不截断、不删除，也不借数量清理迁移。
    #[test]
    fn oversized_existing_history_blocks_without_destructive_migration() {
        let temp = workspace();
        let directory = temp.path().join(LOG_DIR);
        std::fs::create_dir_all(&directory).unwrap();
        let existing = directory.join("jchtools.2026-01-01.123.log");
        std::fs::File::create(&existing)
            .unwrap()
            .set_len(FILE_LIMIT + 1)
            .unwrap();
        assert!(open_appender(&directory).is_err());
        assert_eq!(std::fs::metadata(existing).unwrap().len(), FILE_LIMIT + 1);
    }
}
