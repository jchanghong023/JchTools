//! Windows 命名管道代理。锁与管道名不包含运行目录，保存不同目录也不会另起引擎。
use super::{diagnostic_id, startup_config};
use crate::logging;
use crate::xberg_settings;
use fs2::FileExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, THREAD_TERMINATE,
};

#[link(name = "kernel32")]
extern "system" {
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
}
use windows_sys::Win32::System::IO::CancelSynchronousIo;

const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
// 单行消息上限按方向共用：请求侧远小于该值；响应侧承载完整 extract 结果，
// 含正文之外未被产物消费的图片/页/表数据（实测 50 MB 源文档可达 204 MB）。
// 512 MB 覆盖现有批次峰值约 2.5 倍；超限属于引擎通信故障，由 broken 重建兜底，
// 可定位请求时只失败该请求，无法定位时才重建；不得静默截断或冒充成功。
const MAX_MESSAGE: u64 = 512 * 1024 * 1024;
const MAX_RESPONSE_ID_BYTES: usize = 4096;
struct PendingRequest {
    sender: mpsc::Sender<Value>,
    lane: Option<&'static str>,
    span: tracing::Span,
    started: Instant,
    command: &'static str,
}
type Pending = Arc<Mutex<HashMap<String, PendingRequest>>>;
const BROKER_PROTOCOL: u64 = 2;

fn identity() -> Result<String, String> {
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
    Ok(format!("{digest:x}"))
}

fn pipe_name() -> Result<String, String> {
    Ok(format!(r"\\.\pipe\jchtools-xberg-{}", identity()?))
}

fn stopped_marker() -> Result<PathBuf, String> {
    Ok(xberg_settings::state_dir()?.join(format!("background-stopped-{}", identity()?)))
}
pub(super) fn resume_background() -> Result<(), String> {
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
    match &result {
        Ok(()) => tracing::info!(
            event = "xberg_background_resume_completed",
            elapsed_ms = logging::elapsed_ms(started),
            "后台自动启动状态已恢复"
        ),
        Err(_) => tracing::warn!(
            event = "xberg_background_resume_failed",
            elapsed_ms = logging::elapsed_ms(started),
            "后台自动启动状态恢复失败"
        ),
    }
    result
}
pub(super) fn background_allowed() -> Result<(), String> {
    if stopped_marker()?.exists() {
        Err("后台已从托盘退出；重新打开 JchTools 后自动启动".into())
    } else {
        Ok(())
    }
}
pub(super) fn background_control(stop: bool) -> Result<Value, String> {
    background_command(if stop { "broker-stop" } else { "broker-state" })
}
pub(super) fn force_background_exit() -> Result<Value, String> {
    background_command("broker-force-stop")
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
    match &result {
        Ok(response) => log_response(
            "background_control",
            super::diagnostic_command(Some(command)),
            response,
            started,
            None,
        ),
        Err(_) => tracing::warn!(
            event = "xberg_background_control_failed",
            stage,
            elapsed_ms = logging::elapsed_ms(started),
            "后台控制请求失败"
        ),
    }
    result
}

fn should_mark_background_stopped(command: &str, response: &Value) -> bool {
    command != "broker-state"
        && response["ok"] == true
        && (response["stopping"] == true || response["running"] == false)
}

fn mark_background_stopped() -> Result<(), String> {
    let marker = stopped_marker()?;
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(marker, b"stopped").map_err(|e| e.to_string())
}

/// 看门狗反复取消当前线程同步 I/O，覆盖期限到达恰在两次 I/O 之间的竞态。
struct IoDeadline {
    done: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl IoDeadline {
    fn new(timeout: Duration) -> Result<Self, String> {
        // SAFETY: 打开当前线程的真实句柄，供看门狗取消同步 I/O；不继承句柄。
        let id = unsafe { GetCurrentThreadId() };
        // SAFETY: 当前线程有效，不继承句柄。
        let handle = unsafe { OpenThread(THREAD_TERMINATE, 0, id) };
        if handle.is_null() {
            let error_code = std::io::Error::last_os_error().raw_os_error();
            tracing::warn!(
                event = "xberg_io_deadline_failed",
                stage = "thread_handle",
                error_code,
                "无法取得同步通信看门狗所需的线程句柄"
            );
            return Err("无法设置共享管道通信期限".into());
        }
        let raw = handle as usize;
        let (done, receiver) = mpsc::channel();
        let span = tracing::Span::current();
        let thread = std::thread::spawn(move || {
            let _entered = span.enter();
            if matches!(
                receiver.recv_timeout(timeout),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                tracing::warn!(
                    event = "xberg_io_deadline_reached",
                    stage = "synchronous_io",
                    timed_out = true,
                    timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    "共享管道同步通信期限已到，开始取消阻塞调用"
                );
                loop {
                    // SAFETY: 本线程独占 raw 句柄，目标线程在 guard 析构前存活。
                    unsafe {
                        CancelSynchronousIo(raw as _);
                    }
                    if !matches!(
                        receiver.recv_timeout(Duration::from_millis(5)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
            }
            // SAFETY: raw 是本看门狗独占、未关闭的线程句柄。
            unsafe {
                CloseHandle(raw as _);
            }
        });
        Ok(Self {
            done,
            thread: Some(thread),
        })
    }
}
impl Drop for IoDeadline {
    fn drop(&mut self) {
        let _ = self.done.send(());
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::warn!(
                    event = "xberg_io_watchdog_thread_failed",
                    stage = "watchdog_join",
                    "共享管道通信看门狗线程异常退出"
                );
            }
        }
    }
}

enum BoundedLine {
    Complete(Vec<u8>),
    Oversized { id: Option<String>, bytes: u64 },
}

#[derive(Clone, Copy)]
enum CapturedString {
    Key,
    IdValue,
}

/// 在排空超限行时只保留顶层 `id`，不依赖字段顺序，也不把嵌套对象中的
/// 同名字段误认为请求 ID。字符串按 JSON 转义规则扫描，正文永不进入日志。
// 各布尔状态对应 JSON 层级、转义、键和值的独立词法条件。
#[allow(clippy::struct_excessive_bools)]
struct ResponseIdScanner {
    depth: usize,
    in_string: bool,
    escaped: bool,
    expecting_key: bool,
    key_complete: bool,
    expecting_value: bool,
    pending_id: bool,
    capture: Option<(CapturedString, Vec<u8>, bool)>,
    id: Option<String>,
}

impl ResponseIdScanner {
    fn new() -> Self {
        Self {
            depth: 0,
            in_string: false,
            escaped: false,
            expecting_key: false,
            key_complete: false,
            expecting_value: false,
            pending_id: false,
            capture: None,
            id: None,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.feed_byte(byte);
        }
    }

    fn feed_byte(&mut self, byte: u8) {
        if self.in_string {
            if self.escaped {
                self.push_captured(byte);
                self.escaped = false;
                return;
            }
            if byte == b'\\' {
                self.push_captured(byte);
                self.escaped = true;
                return;
            }
            if byte == b'"' {
                self.push_captured(byte);
                self.finish_string();
                self.in_string = false;
                return;
            }
            self.push_captured(byte);
            return;
        }

        match byte {
            b'{' => {
                if self.depth == 1 && self.expecting_value {
                    self.expecting_value = false;
                    self.pending_id = false;
                }
                self.depth = self.depth.saturating_add(1);
                if self.depth == 1 {
                    self.expecting_key = true;
                    self.key_complete = false;
                    self.expecting_value = false;
                    self.pending_id = false;
                }
            }
            b'}' => {
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.expecting_key = false;
                    self.expecting_value = false;
                    self.pending_id = false;
                }
            }
            b'[' => {
                if self.depth == 1 && self.expecting_value {
                    self.expecting_value = false;
                    self.pending_id = false;
                }
                self.depth = self.depth.saturating_add(1);
            }
            b']' => self.depth = self.depth.saturating_sub(1),
            b',' if self.depth == 1 => {
                self.expecting_key = true;
                self.key_complete = false;
                self.expecting_value = false;
                self.pending_id = false;
            }
            b':' if self.depth == 1 && self.key_complete => {
                self.expecting_key = false;
                self.expecting_value = true;
            }
            b'"' if self.depth == 1 && self.expecting_key => {
                self.in_string = true;
                self.escaped = false;
                self.capture = Some((CapturedString::Key, vec![b'"'], false));
            }
            b'"' if self.depth == 1 && self.expecting_value && self.pending_id => {
                self.in_string = true;
                self.escaped = false;
                self.capture = Some((CapturedString::IdValue, vec![b'"'], false));
            }
            b'"' => {
                // 非 ID 字符串也必须按字符串跳过，正文中的括号和逗号
                // 不能改变 JSON 层级或被当作顶层字段。
                self.in_string = true;
                self.escaped = false;
                self.capture = None;
                if self.depth == 1 && self.expecting_value {
                    self.expecting_value = false;
                    self.pending_id = false;
                }
            }
            byte if self.depth == 1 && self.expecting_value && !byte.is_ascii_whitespace() => {
                self.expecting_value = false;
                self.pending_id = false;
            }
            _ => {}
        }
    }

    fn push_captured(&mut self, byte: u8) {
        if let Some((_, bytes, overflowed)) = self.capture.as_mut() {
            if bytes.len() < MAX_RESPONSE_ID_BYTES {
                bytes.push(byte);
            } else {
                *overflowed = true;
            }
        }
    }

    fn finish_string(&mut self) {
        let Some((kind, bytes, overflowed)) = self.capture.take() else {
            return;
        };
        if overflowed {
            if matches!(kind, CapturedString::Key) {
                self.key_complete = true;
                self.pending_id = false;
            }
            return;
        }
        match kind {
            CapturedString::Key => {
                self.key_complete = true;
                self.pending_id =
                    serde_json::from_slice::<String>(&bytes).is_ok_and(|key| key == "id");
            }
            CapturedString::IdValue => {
                self.id = serde_json::from_slice(&bytes).ok();
                self.expecting_value = false;
            }
        }
    }
}

/// 逐块读取一行；超过上限后继续排空该行，但不再保留正文，避免一条异常
/// 响应把代理一次性推到无界内存。只有上限以内的行才交给 serde_json 解码。
fn read_bounded_line(reader: &mut impl BufRead, limit: u64) -> Result<BoundedLine, String> {
    let mut data = Vec::new();
    let mut id_scanner = ResponseIdScanner::new();
    let mut total = 0u64;
    let mut oversized = false;
    loop {
        let chunk = reader
            .fill_buf()
            .map_err(|e| format!("共享 Xberg 读取失败：{e}"))?;
        if chunk.is_empty() {
            return Err("共享 Xberg 响应中断或缺少换行".into());
        }
        let consumed = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |index| index + 1);
        let part = &chunk[..consumed];
        total = total.saturating_add(part.len() as u64);
        id_scanner.feed(part);
        if !oversized {
            if total <= limit {
                data.extend_from_slice(part);
            } else {
                oversized = true;
                data.clear();
                data.shrink_to_fit();
            }
        }
        let ended = part.last() == Some(&b'\n');
        reader.consume(consumed);
        if ended {
            return if oversized {
                Ok(BoundedLine::Oversized {
                    id: id_scanner.id,
                    bytes: total,
                })
            } else {
                Ok(BoundedLine::Complete(data))
            };
        }
    }
}

fn read_json(reader: &mut impl BufRead) -> Result<Value, String> {
    let line = read_bounded_line(reader, MAX_MESSAGE).map_err(|error| {
        tracing::warn!(
            event = "xberg_message_read_failed",
            stage = "read",
            "共享管道消息读取失败"
        );
        error
    })?;
    match line {
        BoundedLine::Complete(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            tracing::warn!(
                event = "xberg_message_decode_failed",
                stage = "decode",
                bytes = bytes.len(),
                "共享管道消息不是有效 JSON"
            );
            "共享 Xberg 返回了无效 JSON".into()
        }),
        BoundedLine::Oversized { bytes, .. } => {
            tracing::warn!(
                event = "xberg_message_read_failed",
                stage = "message_limit",
                bytes,
                "共享管道消息超过上限"
            );
            Err(format!("共享 Xberg 响应超过消息上限（{bytes} 字节）"))
        }
    }
}

enum EngineResponse {
    Json(Value),
    Oversized { id: Option<String>, bytes: u64 },
}

fn read_engine_response(reader: &mut impl BufRead) -> Result<EngineResponse, String> {
    match read_bounded_line(reader, MAX_MESSAGE)? {
        BoundedLine::Complete(bytes) => serde_json::from_slice(&bytes)
            .map(EngineResponse::Json)
            .map_err(|_| "共享 Xberg 返回了无效 JSON".into()),
        BoundedLine::Oversized { id, bytes } => Ok(EngineResponse::Oversized { id, bytes }),
    }
}
fn write_json(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| {
        tracing::warn!(
            event = "xberg_message_write_failed",
            stage = "encode",
            "共享管道消息编码失败"
        );
        e.to_string()
    })?;
    if bytes.len() as u64 > MAX_MESSAGE {
        tracing::warn!(
            event = "xberg_message_write_failed",
            stage = "message_limit",
            bytes = bytes.len(),
            "共享管道待发送消息超过上限"
        );
        return Err("Xberg 请求超过消息上限".into());
    }
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .and_then(|()| writer.flush())
        .map_err(|e| {
            tracing::warn!(
                event = "xberg_message_write_failed",
                stage = "write_or_flush",
                error_code = e.raw_os_error(),
                "共享管道消息写入或刷新失败"
            );
            format!("共享 Xberg 写入失败：{e}")
        })
}

fn connect(start: bool, deadline: Instant, quiet: bool) -> Result<File, String> {
    let started = Instant::now();
    if quiet {
        tracing::debug!(
            event = "xberg_broker_connect_started",
            allow_start = start,
            "开始连接共享代理"
        );
    } else {
        tracing::info!(
            event = "xberg_broker_connect_started",
            allow_start = start,
            "开始连接共享代理"
        );
    }
    let mut last_error_code = None;
    let mut attempt = 0u64;
    let result = (|| {
        let name = pipe_name()?;
        let open = || OpenOptions::new().read(true).write(true).open(&name);
        if Instant::now() >= deadline {
            return Err("共享 Xberg 代理连接期限已到".into());
        }
        match open() {
            Ok(pipe) => return Ok(pipe),
            Err(error) => last_error_code = error.raw_os_error(),
        }
        if start {
            background_allowed()?;
            // 代理是 JchTools/截图服务自身的内部模式；只有赢得会话锁的代理可创建引擎。
            let executable = if cfg!(feature = "test-hooks")
                && std::env::var_os("JCHTOOLS_TEST_STATE_DIR").is_some()
            {
                std::env::var_os("JCHTOOLS_TEST_BROKER_EXE").map(PathBuf::from)
            } else {
                None
            }
            .map_or_else(|| std::env::current_exe().map_err(|e| e.to_string()), Ok)?;
            tracing::info!(
                event = "xberg_broker_spawn_started",
                executable = "JchTools",
                mode = "xberg_broker",
                "开始启动共享代理候选进程"
            );
            let child = Command::new(executable)
                .arg("--xberg-broker")
                .creation_flags(0x0800_0000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| {
                    tracing::warn!(
                        event = "xberg_broker_spawn_failed",
                        stage = "spawn",
                        error_code = e.raw_os_error(),
                        elapsed_ms = logging::elapsed_ms(started),
                        "共享代理候选进程启动失败"
                    );
                    format!("无法启动共享 Xberg 代理：{e}")
                })?;
            tracing::info!(
                event = "xberg_broker_spawn_completed",
                peer_pid = child.id(),
                "共享代理候选进程已启动"
            );
        }
        loop {
            attempt = attempt.saturating_add(1);
            match open() {
                Ok(pipe) => return Ok(pipe),
                Err(error) => last_error_code = error.raw_os_error(),
            }
            tracing::debug!(
                event = "xberg_broker_connect_retry",
                attempt,
                "共享代理尚未可连接，继续既有轮询"
            );
            if Instant::now() >= deadline {
                return Err("共享 Xberg 代理连接超时".into());
            }
            std::thread::sleep(
                Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    match &result {
        Ok(_) if quiet => tracing::debug!(
            event = "xberg_broker_connect_completed",
            attempt,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理连接完成"
        ),
        Ok(_) => tracing::info!(
            event = "xberg_broker_connect_completed",
            attempt,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理连接完成"
        ),
        Err(_) => tracing::warn!(
            event = "xberg_broker_connect_failed",
            stage = "connect",
            attempt,
            error_code = last_error_code,
            elapsed_ms = logging::elapsed_ms(started),
            timed_out = Instant::now() >= deadline,
            "共享代理连接失败"
        ),
    }
    result
}

fn pipe_server_pid(pipe: &File) -> Option<u32> {
    let mut pid = 0;
    // SAFETY: File 持有有效客户端管道句柄，pid 是有效输出指针；查询不改变通信状态。
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId(
            pipe.as_raw_handle().cast(),
            &raw mut pid,
        )
    };
    (ok != 0).then_some(pid)
}

fn exchange(mut pipe: File, envelope: &Value, timeout: Duration) -> Result<Value, String> {
    let started = Instant::now();
    let mut stage = "io_deadline";
    let quiet = envelope["request"]["command"] == "keepalive";
    let peer_pid = pipe_server_pid(&pipe);
    tracing::debug!(event = "xberg_exchange_started", "共享代理消息交换开始");
    let result = (|| {
        let _deadline = IoDeadline::new(timeout)?;
        stage = "write";
        write_json(&mut pipe, envelope)?;
        if quiet {
            tracing::debug!(
                event = "xberg_request_sent",
                peer_pid,
                "共享代理请求消息已发送"
            );
        } else {
            tracing::info!(
                event = "xberg_request_sent",
                peer_pid,
                "共享代理请求消息已发送"
            );
        }
        stage = "read";
        let response = read_json(&mut BufReader::new(pipe))?;
        stage = "protocol";
        if response["jchtools_broker_protocol"] != BROKER_PROTOCOL {
            return Err(
                "常驻 Xberg 代理协议不兼容，请结束旧版本会话后重试；未启动第二个引擎".into(),
            );
        }
        if response["id"] != envelope["request"]["id"] {
            return Err("共享 Xberg 响应 ID 不匹配".into());
        }
        if quiet {
            tracing::debug!(
                event = "xberg_response_received",
                peer_pid,
                broker_pid = response["jchtools_broker_pid"].as_u64(),
                engine_pid = response["jchtools_xberg_pid"].as_u64(),
                "共享代理响应已接收并验证"
            );
        } else {
            tracing::info!(
                event = "xberg_response_received",
                peer_pid,
                broker_pid = response["jchtools_broker_pid"].as_u64(),
                engine_pid = response["jchtools_xberg_pid"].as_u64(),
                "共享代理响应已接收并验证"
            );
        }
        Ok(response)
    })();
    if result.is_err() {
        tracing::warn!(
            event = "xberg_exchange_failed",
            peer_pid,
            stage,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理消息交换失败"
        );
    }
    result
}

fn log_response(
    side: &'static str,
    command: &'static str,
    response: &Value,
    started: Instant,
    peer_pid: Option<u32>,
) {
    let elapsed_ms = logging::elapsed_ms(started);
    let broker_pid = response["jchtools_broker_pid"].as_u64();
    let engine_pid = response["jchtools_xberg_pid"].as_u64();
    let accepted = response["accepted"].as_bool();
    let error_code = response["error_code"]
        .as_i64()
        .or_else(|| response["code"].as_i64());
    if response["ok"] != true {
        tracing::warn!(
            event = "xberg_response_failed",
            side,
            command,
            stage = "remote",
            error_kind = super::diagnostic_error_kind(response),
            error_code,
            peer_pid,
            broker_pid,
            engine_pid,
            elapsed_ms,
            "共享引擎返回失败结果，通信成功不代表业务成功"
        );
    } else if command == "keepalive" {
        tracing::debug!(
            event = "xberg_response_completed",
            side,
            command,
            peer_pid,
            broker_pid,
            engine_pid,
            elapsed_ms,
            "共享引擎保活完成"
        );
    } else {
        tracing::info!(
            event = "xberg_response_completed",
            side,
            command,
            peer_pid,
            broker_pid,
            engine_pid,
            accepted,
            elapsed_ms,
            page_count = response["document"]["pages"].as_array().map(Vec::len),
            content_bytes = response["document"]["content"].as_str().map(str::len),
            markdown_bytes = response["markdown"].as_str().map(str::len),
            running = response["running"].as_bool(),
            active = response["active"].as_u64(),
            stopping = response["stopping"].as_bool(),
            "共享引擎响应完成"
        );
    }
}

pub(super) fn request(
    root: &Path,
    value: &Value,
    timeout: Duration,
    cancel: &AtomicBool,
    parent_operation_id: &str,
) -> Result<Value, String> {
    let started = Instant::now();
    let id = value["id"].as_str().unwrap_or_default();
    let span = logging::operation_span_with_id("xberg_runtime", "broker_request", id);
    let _entered = span.enter();
    let command = super::diagnostic_command(value["command"].as_str());
    if command == "keepalive" {
        tracing::debug!(
            event = "xberg_client_request_started",
            parent_operation_id,
            request_id = id,
            command,
            timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "客户端提交共享引擎请求"
        );
    } else {
        tracing::info!(
            event = "xberg_client_request_started",
            parent_operation_id,
            request_id = id,
            command,
            timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            "客户端提交共享引擎请求"
        );
    }
    let result = request_via_broker(root, value, timeout, cancel, parent_operation_id);
    match &result {
        Ok(response) => log_response("client", command, response, started, None),
        Err(_) => tracing::warn!(
            event = "xberg_client_request_failed",
            stage = "transport_or_cancel",
            command,
            elapsed_ms = logging::elapsed_ms(started),
            "共享引擎客户端请求失败"
        ),
    }
    result
}

fn request_via_broker(
    root: &Path,
    value: &Value,
    timeout: Duration,
    cancel: &AtomicBool,
    parent_operation_id: &str,
) -> Result<Value, String> {
    if timeout.is_zero() {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "timeout_budget",
            timed_out = true,
            "共享请求期限已到，未启动代理"
        );
        return Err("共享 Xberg 请求期限已到".into());
    }
    let _exchange_extra = timeout.checked_add(QUERY_TIMEOUT).ok_or_else(|| {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "timeout_range",
            "共享请求期限过大，未启动代理"
        );
        "Xberg 请求期限过大，未启动共享代理"
    })?;
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "deadline_range",
            "共享请求期限无法转换，未启动代理"
        );
        "Xberg 请求期限过大，未启动共享代理"
    })?;
    let root = std::fs::canonicalize(root).map_err(|e| {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "runtime_directory",
            error_code = e.raw_os_error(),
            "共享引擎目录不可读，未提交请求"
        );
        format!("Xberg 目录不可读：{e}")
    })?;
    let pipe = connect(true, deadline, value["command"] == "keepalive")?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        tracing::warn!(
            event = "xberg_client_preflight_failed",
            stage = "timeout_budget",
            timed_out = true,
            "共享请求连接已耗尽期限，未提交推理"
        );
        return Err("Xberg 请求期限已到，未提交推理".into());
    }
    let exchange_timeout = remaining
        .checked_add(QUERY_TIMEOUT)
        .ok_or("Xberg 请求期限过大，未提交推理")?;
    let envelope = json!({"runtime_dir":root, "request":value, "diagnostic":{"parent_operation_id":parent_operation_id,"caller_pid":std::process::id()}});
    let cloned = envelope.clone();
    let (send, receive) = mpsc::channel();
    let span = tracing::Span::current();
    let reader = std::thread::spawn(move || {
        let _entered = span.enter();
        if send
            .send(exchange(pipe, &cloned, exchange_timeout))
            .is_err()
        {
            tracing::warn!(
                event = "xberg_client_response_delivery_failed",
                stage = "caller_left",
                "共享代理交换已结束，但客户端等待端已离开"
            );
        }
    });
    let end = deadline;
    let mut cancellation_sent = false;
    let mut cancellation_error = None;
    let mut cancellation_io_deadline = None;
    let mut cancellation_receive: Option<mpsc::Receiver<Result<(), String>>> = None;
    let mut timed_out = false;
    loop {
        if let Some(receiver) = cancellation_receive.as_ref() {
            match receiver.try_recv() {
                Ok(result) => {
                    cancellation_receive = None;
                    if let Err(error) = result {
                        cancellation_error = Some(error);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    tracing::warn!(
                        event = "xberg_cancel_failed",
                        stage = "cancel_thread",
                        "请求级取消接口线程提前退出"
                    );
                    cancellation_receive = None;
                    cancellation_error = Some("取消接口线程退出".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => {
                let _ = reader.join();
                if cancellation_sent {
                    tracing::info!(
                        event = "xberg_cancel_original_terminal",
                        timed_out,
                        transport_ok = result.is_ok(),
                        "取消后已收到原请求终态"
                    );
                }
                return match (result, cancellation_error) {
                    (Err(error), Some(cancel_error)) => Err(format!("取消接口失败：{cancel_error}；{error}；代理仍保留未结束任务，阻止同场景重入")),
                    (result, _) => result,
                };
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!(
                    event = "xberg_response_thread_failed",
                    stage = "response_thread",
                    "共享引擎响应线程提前退出"
                );
                let _ = reader.join();
                return Err("共享 Xberg 响应线程退出".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if cancellation_sent
            && cancellation_io_deadline.is_some_and(|deadline| Instant::now() >= deadline)
        {
            let reason = if cancellation_receive.is_some() {
                if timed_out {
                    "Xberg 请求超时且取消接口未在 1 秒内确认"
                } else {
                    "请求已取消但取消接口未在 1 秒内确认"
                }
            } else if cancellation_error.is_some() {
                if timed_out {
                    "Xberg 请求超时且取消接口失败，原请求未在 1 秒内结束"
                } else {
                    "取消接口失败，原请求未在 1 秒内结束"
                }
            } else if timed_out {
                "Xberg 请求超时且取消已确认，原请求未在 1 秒内结束"
            } else {
                "请求已取消且取消已确认，原请求未在 1 秒内结束"
            };
            tracing::warn!(
                event = "xberg_cancel_original_timeout",
                timed_out,
                cancel_pending = cancellation_receive.is_some(),
                cancel_failed = cancellation_error.is_some(),
                stage = "original_terminal",
                "取消后原请求未在期限内结束，场景仍保持占用"
            );
            return Err(format!("{reason}；代理仍保留未结束任务，阻止同场景重入"));
        }
        if !cancellation_sent && (cancel.load(Ordering::Acquire) || Instant::now() >= end) {
            cancellation_sent = true;
            timed_out = Instant::now() >= end && !cancel.load(Ordering::Acquire);
            let target_id = value["id"].as_str().unwrap_or_default();
            let cancel_id = format!("cancel-{target_id}");
            tracing::info!(event = "xberg_cancel_started", target_request_id = target_id, cancel_request_id = %cancel_id, timed_out, "开始发送请求级取消");
            let cancellation = json!({"runtime_dir":root,"request":{"id":cancel_id,"command":"cancel","target_id":value["id"]},"diagnostic":{"parent_operation_id":target_id,"caller_pid":std::process::id()}});
            let io_deadline = Instant::now()
                .checked_add(Duration::from_secs(1))
                .ok_or("取消接口期限无效")?;
            let (cancel_send, cancel_receive) = mpsc::channel();
            let cancel_deadline = io_deadline;
            let span = logging::operation_span_with_id("xberg_runtime", "cancel", &cancel_id);
            std::thread::spawn(move || {
                let _entered = span.enter();
                let started = Instant::now();
                let result = connect(false, cancel_deadline, false)
                    .and_then(|pipe| {
                        let remaining = cancel_deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Err("取消接口期限已到".into());
                        }
                        exchange(pipe, &cancellation, remaining)
                    })
                    .and_then(|response| {
                        log_response("cancel", "cancel", &response, started, None);
                        super::checked(response)
                    })
                    .map(|response| {
                        tracing::info!(
                            event = "xberg_cancel_completed",
                            accepted = response["accepted"].as_bool(),
                            elapsed_ms = logging::elapsed_ms(started),
                            "请求级取消接口返回，仍等待原请求终态"
                        );
                    });
                if result.is_err() {
                    tracing::warn!(
                        event = "xberg_cancel_failed",
                        stage = "cancel_exchange",
                        elapsed_ms = logging::elapsed_ms(started),
                        "请求级取消接口失败，原任务仍由代理管理"
                    );
                }
                if cancel_send.send(result).is_err() {
                    tracing::warn!(
                        event = "xberg_cancel_delivery_failed",
                        stage = "caller_left",
                        "取消接口已返回，但原请求等待端已离开"
                    );
                }
            });
            cancellation_io_deadline = Some(io_deadline);
            cancellation_receive = Some(cancel_receive);
            // 接受取消不等于任务结束；继续等待原请求的终态，禁止伪报成功。
        }
    }
}

struct Engine {
    child: Child,
    root: PathBuf,
    outgoing: mpsc::SyncSender<(Value, tracing::Span)>,
    pending: Pending,
    broken: Arc<AtomicBool>,
    _job: Job,
    lifecycle_span: tracing::Span,
    started: Instant,
}
impl Engine {
    fn spawn(root: &Path) -> Result<Self, String> {
        let span = logging::operation_span("xberg_runtime", "engine_spawn");
        let _entered = span.enter();
        let started = Instant::now();
        let mut stage = "existing_engine_guard";
        tracing::info!(
            event = "xberg_engine_spawn_started",
            executable = "xberg.exe",
            mode = "worker",
            "开始创建唯一共享引擎"
        );
        let result = (|| {
            reject_existing_engine()?;
            stage = "startup_config";
            let mut command = Command::new(root.join("xberg.exe"));
            command
                .args(["worker", "--no-config-discovery", "--config-json"])
                .arg(startup_config(root)?.to_string())
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(0x0800_0000);
            for name in [
                "HF_HUB_OFFLINE",
                "HUGGINGFACE_HUB_OFFLINE",
                "TRANSFORMERS_OFFLINE",
                "HF_DATASETS_OFFLINE",
                "NO_COLOR",
            ] {
                command.env(name, "1");
            }
            command
                .env("ORT_DYLIB_PATH", root.join("onnxruntime.dll"))
                .env("HF_HOME", root)
                .env("HF_HUB_CACHE", root.join("models"))
                .env("XBERG_MAX_REQUEST_BODY_BYTES", "104857600")
                .env("XBERG_API_ALLOW_LOCAL_URI_INPUTS", "1")
                .env("XBERG_ORT_EP", "cpu")
                .env("XBERG_SENSEVOICE_MODEL_DIR", root.join("models"))
                .env("XBERG_SHERPA_DLL_DIR", root.join("sherpa-onnx"))
                .env("XBERG_FFMPEG_DLL_DIR", root.join("ffmpeg"))
                .env(
                    "XBERG_CACHE_DIR",
                    xberg_settings::state_dir()?.join("xberg-cache"),
                )
                .env(
                    "XBERG_PERF_LOG_DIR",
                    xberg_settings::state_dir()?.join("perf-logs").join("xberg"),
                );
            stage = "spawn";
            let mut child = command
                .spawn()
                .map_err(|e| format!("共享 Xberg 启动失败：{e}"))?;
            tracing::info!(
                event = "xberg_engine_process_started",
                engine_pid = child.id(),
                executable = "xberg.exe",
                "共享 Xberg 引擎进程已启动"
            );
            stage = "job_attach";
            let job = match Job::attach(&child) {
                Ok(job) => job,
                Err(error) => {
                    if let Err(error) = child.kill() {
                        tracing::warn!(
                            event = "xberg_engine_kill_failed",
                            stage = "job_attach_cleanup",
                            engine_pid = child.id(),
                            error_code = error.raw_os_error(),
                            "作业绑定失败后的引擎终结失败"
                        );
                    }
                    match child.wait() {
                        Ok(status) => {
                            tracing::info!(event = "xberg_engine_exited", stage = "job_attach_cleanup", engine_pid = child.id(), exit_code = status.code(), exit_status = %status, elapsed_ms = logging::elapsed_ms(started), "作业绑定失败后的引擎已退出")
                        }
                        Err(error) => tracing::warn!(
                            event = "xberg_engine_wait_failed",
                            stage = "job_attach_cleanup",
                            engine_pid = child.id(),
                            error_code = error.raw_os_error(),
                            "作业绑定失败后的引擎退出等待失败"
                        ),
                    }
                    return Err(error);
                }
            };
            stage = "stdio_setup";
            let mut stdin = child.stdin.take().ok_or("Xberg stdin 缺失")?;
            let stdout = child.stdout.take().ok_or("Xberg stdout 缺失")?;
            let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
            let broken = Arc::new(AtomicBool::new(false));
            let (outgoing, incoming) = mpsc::sync_channel::<(Value, tracing::Span)>(32);
            let writer_pending = Arc::clone(&pending);
            let writer_broken = Arc::clone(&broken);
            let writer_span = tracing::Span::current();
            let engine_pid = child.id();
            std::thread::spawn(move || {
                let _entered = writer_span.enter();
                for (request, request_span) in incoming {
                    let _request_entered = request_span.enter();
                    let started = Instant::now();
                    tracing::info!(
                        event = "xberg_engine_write_started",
                        engine_pid,
                        command = super::diagnostic_command(request["command"].as_str()),
                        "开始向共享引擎提交请求"
                    );
                    let result = IoDeadline::new(QUERY_TIMEOUT)
                        .and_then(|_guard| write_json(&mut stdin, &request));
                    if result.is_err() {
                        tracing::warn!(
                            event = "xberg_engine_write_failed",
                            stage = "stdin_write",
                            engine_pid,
                            elapsed_ms = logging::elapsed_ms(started),
                            "共享引擎 stdin 写入失败，通信断裂"
                        );
                        fail_pending(&writer_pending, &writer_broken);
                        break;
                    }
                    tracing::info!(
                        event = "xberg_engine_write_completed",
                        engine_pid,
                        elapsed_ms = logging::elapsed_ms(started),
                        "共享引擎请求已写入 stdin"
                    );
                }
            });
            let reader_pending = Arc::clone(&pending);
            let reader_broken = Arc::clone(&broken);
            let reader_span = tracing::Span::current();
            std::thread::spawn(move || {
                let _entered = reader_span.enter();
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut response = match read_engine_response(&mut reader) {
                        Ok(EngineResponse::Json(response)) => response,
                        Ok(EngineResponse::Oversized { id, bytes }) => {
                            if let Some(id) = id {
                                let sender = reader_pending
                                    .lock()
                                    .ok()
                                    .and_then(|mut entries| entries.remove(&id));
                                if let Some(sender) = sender {
                                    let _request_entered = sender.span.enter();
                                    tracing::warn!(
                                        event = "xberg_engine_response_failed",
                                        engine_pid,
                                        stage = "message_limit",
                                        bytes,
                                        elapsed_ms = logging::elapsed_ms(sender.started),
                                        "共享引擎响应超限，该请求按失败终态交付"
                                    );
                                    if sender.sender.send(json!({
                                    "id": id,
                                    "ok": false,
                                    "error_kind": "response_too_large",
                                    "error": format!("共享 Xberg 响应超过消息上限（{bytes} 字节）")
                                })).is_err() {
                                    tracing::warn!(event = "xberg_engine_response_delivery_failed", stage = "broker_receiver_closed", engine_pid, "超限失败终态已生成，但代理等待端已离开");
                                }
                                    continue;
                                }
                            }
                            tracing::warn!(
                                event = "xberg_engine_response_failed",
                                engine_pid,
                                stage = "message_limit_unmapped",
                                bytes,
                                "共享引擎返回超限响应且无法定位请求"
                            );
                            break;
                        }
                        Err(error) => {
                            let stage = if error == "共享 Xberg 返回了无效 JSON" {
                                "stdout_decode"
                            } else {
                                "stdout_read"
                            };
                            tracing::warn!(
                                event = "xberg_engine_response_failed",
                                engine_pid,
                                stage,
                                "共享引擎响应读取失败，通信断裂"
                            );
                            break;
                        }
                    };
                    response = prune_forwarded_document(response);
                    let Some(id) = response["id"].as_str() else {
                        tracing::warn!(
                            event = "xberg_engine_response_failed",
                            engine_pid,
                            stage = "response_id_missing",
                            "共享引擎响应缺少 id，通信断裂"
                        );
                        break;
                    };
                    let sender = reader_pending
                        .lock()
                        .ok()
                        .and_then(|mut entries| entries.remove(id));
                    let Some(sender) = sender else {
                        // 未知 id（含重复响应）是引擎侧协议异常：静默丢弃会让等待者
                        // 只能靠超时报错，而在途项不随超时删除，场景 lane 被永久占用
                        // 直到引擎死亡。与超限响应无法定位请求的路径同口径：warn 后
                        // 按通信断裂交付全部在途失败并释放 lane（XB-05/XB-07）。
                        tracing::warn!(
                            event = "xberg_engine_response_failed",
                            engine_pid,
                            stage = "response_id_unknown",
                            response_id_bytes = id.len(),
                            "共享引擎返回未知请求 ID 的响应，通信断裂"
                        );
                        break;
                    };
                    let _request_entered = sender.span.enter();
                    log_response(
                        "engine",
                        sender.command,
                        &response,
                        sender.started,
                        Some(engine_pid),
                    );
                    if sender.sender.send(response).is_err() {
                        tracing::warn!(
                            event = "xberg_engine_response_delivery_failed",
                            stage = "broker_receiver_closed",
                            engine_pid,
                            "共享引擎请求已有终态，但代理等待端已离开"
                        );
                    }
                }
                fail_pending(&reader_pending, &reader_broken);
            });
            Ok(Self {
                child,
                root: root.to_path_buf(),
                outgoing,
                pending,
                broken,
                _job: job,
                lifecycle_span: tracing::Span::current(),
                started,
            })
        })();
        match &result {
            Ok(engine) => tracing::info!(
                event = "xberg_engine_spawn_completed",
                engine_pid = engine.child.id(),
                elapsed_ms = logging::elapsed_ms(started),
                "唯一共享引擎创建完成"
            ),
            Err(_) => tracing::warn!(
                event = "xberg_engine_spawn_failed",
                stage,
                elapsed_ms = logging::elapsed_ms(started),
                "唯一共享引擎创建失败"
            ),
        }
        result
    }
    fn submit(&self, request: Value) -> Result<mpsc::Receiver<Value>, String> {
        let started = Instant::now();
        tracing::info!(
            event = "xberg_engine_submit_started",
            engine_pid = self.child.id(),
            command = super::diagnostic_command(request["command"].as_str()),
            "开始登记并排队共享引擎请求"
        );
        let id = request["id"]
            .as_str()
            .ok_or_else(|| {
                tracing::warn!(
                    event = "xberg_engine_submit_failed",
                    stage = "request_id",
                    "共享请求 ID 无效"
                );
                "请求 ID 无效"
            })?
            .to_string();
        let (send, receive) = mpsc::channel();
        let mut pending = self.pending.lock().map_err(|_| {
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "pending_lock",
                "共享请求登记表锁异常"
            );
            "共享请求表异常"
        })?;
        if self.broken.load(Ordering::Acquire) {
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "broken",
                "共享请求被拒绝：引擎通信已中断"
            );
            return Err("共享 Xberg 通信已中断；不会为仍存活的引擎启动副本".into());
        }
        if pending.len() >= 64 || pending.contains_key(&id) {
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "queue_limit_or_duplicate",
                pending = pending.len(),
                "共享请求被拒绝：请求队列已满或 ID 重复"
            );
            return Err("共享 Xberg 请求队列已满或 ID 重复".into());
        }
        let lane = match request["command"].as_str() {
            Some("extract" | "transcribe") => Some("document"),
            Some("ocr_snapshot") => Some("snapshot"),
            _ => None,
        };
        if lane.is_some() && pending.values().any(|entry| entry.lane == lane) {
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "lane_busy",
                lane = lane.unwrap_or("?"),
                "共享请求被拒绝：同场景上一请求未结束"
            );
            return Err("该场景的上一个请求尚未确认结束，请等待其终态；其他场景可继续使用".into());
        }
        pending.insert(
            id.clone(),
            PendingRequest {
                sender: send,
                lane,
                span: tracing::Span::current(),
                started,
                command: super::diagnostic_command(request["command"].as_str()),
            },
        );
        if self
            .outgoing
            .try_send((request, tracing::Span::current()))
            .is_err()
        {
            pending.remove(&id);
            tracing::warn!(
                event = "xberg_engine_submit_failed",
                stage = "outgoing_queue",
                "共享请求排队失败，已撤销登记"
            );
            return Err("共享 Xberg 请求队列不可用".into());
        }
        tracing::info!(
            event = "xberg_engine_submit_completed",
            engine_pid = self.child.id(),
            pending = pending.len(),
            lane,
            elapsed_ms = logging::elapsed_ms(started),
            "共享引擎请求已登记并排队"
        );
        Ok(receive)
    }
}

/// 升级期间旧截图服务可能仍持有独占引擎。不能接管未知 stdio，也不能另开副本。
fn reject_existing_engine() -> Result<(), String> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: 只读获取进程目录快照，不访问进程内容。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err("无法检查已有 Xberg 进程，未启动新引擎".into());
    }
    // SAFETY: 快照句柄所有权转移给 File，析构仅调用 CloseHandle。
    let snapshot = unsafe { File::from_raw_handle(snapshot.cast()) };
    // SAFETY: PROCESSENTRY32W 是 C POD 结构，dwSize 按 API 要求填写。
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = u32::try_from(std::mem::size_of_val(&entry)).map_err(|e| e.to_string())?;
    let mut session = 0;
    // SAFETY: 有效进程 ID 与可写的 session 指针。
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: session 是有效输出指针，pid 是当前进程。
    if unsafe { ProcessIdToSessionId(pid, &raw mut session) } == 0 {
        return Err("无法确认当前登录会话".into());
    }
    // SAFETY: 有效快照与已初始化长度的输出结构。
    let mut found = unsafe { Process32FirstW(snapshot.as_raw_handle().cast(), &raw mut entry) };
    while found != 0 {
        let length = entry
            .szExeFile
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..length]).eq_ignore_ascii_case("xberg.exe") {
            let mut other_session = u32::MAX;
            // SAFETY: 快照提供的进程 ID，输出指针有效；已退出进程的查询允许失败。
            if unsafe { ProcessIdToSessionId(entry.th32ProcessID, &raw mut other_session) } != 0
                && other_session == session
            {
                return Err(format!("当前会话已有 Xberg（PID {}），无法接管其通信；请先结束旧实例。未启动第二个引擎", entry.th32ProcessID));
            }
        }
        // SAFETY: 同一有效快照及输出结构，逐项只读枚举。
        found = unsafe { Process32NextW(snapshot.as_raw_handle().cast(), &raw mut entry) };
    }
    Ok(())
}

fn fail_pending(pending: &Pending, broken: &AtomicBool) {
    broken.store(true, Ordering::Release);
    tracing::warn!(
        event = "xberg_engine_communication_broken",
        "共享引擎通信断裂：在途请求全部按失败交付"
    );
    if let Ok(mut entries) = pending.lock() {
        for (id, send) in entries.drain() {
            let _entered = send.span.enter();
            tracing::warn!(
                event = "xberg_engine_pending_failed",
                stage = "communication_broken",
                error_kind = "process_exited",
                elapsed_ms = logging::elapsed_ms(send.started),
                "共享引擎在途请求按失败终态交付"
            );
            if send.sender.send(json!({"id":id,"ok":false,"error_kind":"process_exited","error":"共享 Xberg 通信中断或进程退出"})).is_err() {
                tracing::warn!(event = "xberg_engine_response_delivery_failed", stage = "broker_receiver_closed", "通信断裂失败终态已生成，但代理等待端已离开");
            }
        }
    } else {
        tracing::warn!(
            event = "xberg_engine_pending_delivery_failed",
            stage = "pending_lock",
            error_kind = "mutex_poisoned",
            "共享引擎通信已断裂，但请求登记表锁异常，不能交付在途终态"
        );
    }
}

/// 转发前剔除客户端不消费的重型字段，收窄响应体积的主要来源。
///
/// Markdown 产物消费 `content`（引擎最终 Markdown，markdown_document 零改写
/// 采用）、`warnings`（顶层副本，恒在）以及 T-14（2026-10-04）起产物要保留的
/// `images`（`data_base64`/`data` 是图片落盘的数据通道，转发侧必须放行）；
/// `pages`/`tables` 仍剔除，`children`/`ocr_elements` 虽不再被文档适配器消费，
/// 仍按原样转发。剔除只发生在代理转发侧，引擎落盘缓存与 CLI 行为不受影响；
/// 嵌入子文档逐层递归处理，路径、正文与告警保持逐字节不变。
fn prune_forwarded_document(mut response: Value) -> Value {
    if let Some(document) = response.get_mut("document") {
        prune_document_heavy_fields(document, 0);
    }
    response
}

fn prune_document_heavy_fields(value: &mut Value, depth: usize) {
    // 嵌套深度与引擎提取期的归档/嵌入深度上限同量级，防御性兜底即可。
    const MAX_PRUNE_DEPTH: usize = 8;
    if depth > MAX_PRUNE_DEPTH {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.remove("pages");
    object.remove("tables");
    if let Some(children) = object.get_mut("children").and_then(Value::as_array_mut) {
        for child in children {
            if let Some(result) = child.get_mut("result") {
                prune_document_heavy_fields(result, depth + 1);
            }
        }
    }
}

// Job 只由代理拥有，GUI/截图服务退出不会关闭此句柄。
struct Job(windows_sys::Win32::Foundation::HANDLE);
// SAFETY: 句柄只在 Engine 的互斥锁内转移/访问；Drop 只执行一次 CloseHandle。
unsafe impl Send for Job {}
impl Job {
    fn attach(child: &Child) -> Result<Self, String> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        // SAFETY: 创建无继承的未命名 job，不传外部指针。
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            let error_code = std::io::Error::last_os_error().raw_os_error();
            tracing::warn!(
                event = "xberg_engine_job_failed",
                stage = "job_create",
                engine_pid = child.id(),
                error_code,
                "无法创建共享引擎进程组"
            );
            return Err("无法创建共享引擎进程组".into());
        }
        let job = Self(handle);
        // SAFETY: C POD 结构允许全零初始化。
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: 有效 job 及同步读取的配置结构。
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(std::mem::size_of_val(&limits)).map_err(|e| e.to_string())?,
            )
        } == 0
        {
            let error_code = std::io::Error::last_os_error().raw_os_error();
            tracing::warn!(
                event = "xberg_engine_job_failed",
                stage = "job_configure",
                engine_pid = child.id(),
                error_code,
                "无法配置共享引擎进程组"
            );
            return Err("无法配置共享引擎进程组".into());
        }
        // SAFETY: child 为当前代理创建的有效进程，job 由本代理持有。
        if unsafe { AssignProcessToJobObject(handle, child.as_raw_handle().cast()) } == 0 {
            let error_code = std::io::Error::last_os_error().raw_os_error();
            tracing::warn!(
                event = "xberg_engine_job_failed",
                stage = "job_assign",
                engine_pid = child.id(),
                error_code,
                "无法将共享引擎加入进程组"
            );
            return Err("无法将共享引擎加入进程组".into());
        }
        Ok(job)
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        /* SAFETY: 独占有效 job 句柄。 */
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn create_pipe(name: &str) -> Result<File, String> {
    use windows_sys::Win32::Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    };
    use windows_sys::Win32::System::Pipes::CreateNamedPipeW;
    let wide = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let name = wide(name);
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;OW)");
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: 有效宽字符串和可写输出指针；descriptor 随后由 LocalFree 释放。
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err("无法设置共享管道权限".into());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|e| e.to_string())?,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: 属性与名字在调用期间存活；拒绝远程客户端，允许多个本地请求并发连接。
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            3,
            8,
            64,
            65536,
            65536,
            0,
            &raw const attributes,
        )
    };
    // SAFETY: descriptor 为上方 API 分配的内存，管道创建已复制所需内容。
    unsafe {
        LocalFree(descriptor);
    }
    if raw == INVALID_HANDLE_VALUE {
        let os_error = std::io::Error::last_os_error();
        tracing::error!(
            event = "xberg_pipe_create_failed",
            stage = "create_pipe",
            error_code = os_error.raw_os_error(),
            "创建共享管道失败"
        );
        return Err(format!("创建共享管道失败：{os_error}"));
    }
    // SAFETY: 将新建管道句柄唯一所有权交给 File。
    Ok(unsafe { File::from_raw_handle(raw.cast()) })
}

fn kill_engine(engine: &mut Engine, reason: &'static str) -> std::io::Result<()> {
    let _entered = engine.lifecycle_span.enter();
    tracing::info!(
        event = "xberg_engine_terminate_started",
        engine_pid = engine.child.id(),
        reason,
        "开始终结共享引擎"
    );
    let result = engine.child.kill();
    if let Err(error) = &result {
        tracing::warn!(
            event = "xberg_engine_kill_failed",
            engine_pid = engine.child.id(),
            reason,
            error_code = error.raw_os_error(),
            "共享引擎终结调用失败"
        );
    }
    result
}

fn wait_engine(
    engine: &mut Engine,
    reason: &'static str,
) -> std::io::Result<std::process::ExitStatus> {
    let result = engine.child.wait();
    match &result {
        Ok(status) => log_engine_exit(engine, status, reason),
        Err(error) => {
            let _entered = engine.lifecycle_span.enter();
            tracing::warn!(
                event = "xberg_engine_wait_failed",
                engine_pid = engine.child.id(),
                reason,
                error_code = error.raw_os_error(),
                elapsed_ms = logging::elapsed_ms(engine.started),
                "共享引擎退出等待失败"
            );
        }
    }
    result
}

fn log_engine_exit(engine: &Engine, status: &std::process::ExitStatus, reason: &'static str) {
    let _entered = engine.lifecycle_span.enter();
    if reason == "observed_exit" && !status.success() {
        tracing::warn!(event = "xberg_engine_exited", engine_pid = engine.child.id(), reason, exit_code = status.code(), exit_status = %status, elapsed_ms = logging::elapsed_ms(engine.started), "共享引擎进程异常退出");
    } else {
        tracing::info!(event = "xberg_engine_exited", engine_pid = engine.child.id(), reason, exit_code = status.code(), exit_status = %status, success = status.success(), elapsed_ms = logging::elapsed_ms(engine.started), "共享引擎进程已退出");
    }
}

fn handle(pipe: File, engine: &Mutex<Option<Engine>>, stopping: &AtomicBool) -> Result<(), String> {
    let started = Instant::now();
    let mut reader = BufReader::new(pipe);
    let envelope = {
        let _guard = IoDeadline::new(QUERY_TIMEOUT)?;
        read_json(&mut reader)?
    };
    let request = envelope["request"].clone();
    let fallback_id;
    let request_id = if let Some(id) =
        diagnostic_id(envelope["diagnostic"]["operation_id"].as_str())
            .or_else(|| diagnostic_id(request["id"].as_str()))
    {
        id
    } else {
        fallback_id = logging::new_operation_id();
        &fallback_id
    };
    let span = logging::operation_span_with_id("xberg_runtime", "broker_handle", request_id);
    let _entered = span.enter();
    let command = super::diagnostic_command(request["command"].as_str());
    let parent_operation_id = diagnostic_id(envelope["diagnostic"]["parent_operation_id"].as_str());
    let caller_pid = envelope["diagnostic"]["caller_pid"].as_u64();
    if command == "keepalive" {
        tracing::debug!(
            event = "xberg_broker_request_started",
            parent_operation_id,
            caller_pid,
            command,
            timeout_ms = request["timeout_ms"].as_u64(),
            "共享代理接收到客户端请求"
        );
    } else {
        tracing::info!(
            event = "xberg_broker_request_started",
            parent_operation_id,
            caller_pid,
            command,
            timeout_ms = request["timeout_ms"].as_u64(),
            target_request_id = diagnostic_id(request["target_id"].as_str()),
            "共享代理接收到客户端请求"
        );
    }
    let mut stage = "dispatch";
    let result = (|| {
        if matches!(
            request["command"].as_str(),
            Some("broker-state" | "broker-stop" | "broker-force-stop")
        ) {
            if request["command"] == "broker-stop" {
                stopping.store(true, Ordering::Release);
            }
            let mut slot = engine.lock().map_err(|_| "后台状态异常")?;
            if request["command"] == "broker-force-stop" {
                let task_active = slot.as_ref().is_some_and(|e| {
                    e.pending
                        .lock()
                        .map_or(true, |p| p.values().any(|v| v.lane.is_some()))
                });
                if task_active {
                    tracing::warn!(
                        event = "xberg_broker_control_failed",
                        stage = "active_tasks",
                        command,
                        "共享代理拒绝强制停止：仍有其他场景任务"
                    );
                    return write_json(
                        reader.get_mut(),
                        &json!({"id":request["id"],"ok":false,"error":"文档、媒体或截图任务尚未结束，不能强制退出","jchtools_broker_protocol":BROKER_PROTOCOL}),
                    );
                }
                stopping.store(true, Ordering::Release);
                if let Some(mut current) = slot.take() {
                    kill_engine(&mut current, "force_stop").map_err(|e| e.to_string())?;
                    wait_engine(&mut current, "force_stop").map_err(|e| e.to_string())?;
                    fail_pending(&current.pending, &current.broken);
                }
            }
            let active = slot
                .as_ref()
                .and_then(|e| {
                    e.pending
                        .lock()
                        .ok()
                        .map(|p| p.values().filter(|v| v.lane.is_some()).count())
                })
                .unwrap_or(0);
            let response = json!({"id":request["id"],"ok":true,"running":true,"active":active,"stopping":stopping.load(Ordering::Acquire),"jchtools_broker_protocol":BROKER_PROTOCOL});
            log_response("broker", command, &response, started, None);
            stage = "response_write";
            return write_json(reader.get_mut(), &response);
        }
        stage = "runtime_dir";
        let root = PathBuf::from(envelope["runtime_dir"].as_str().ok_or("缺少 Xberg 目录")?);
        let response = (|| {
            stage = "engine_lock";
            let mut slot = engine.lock().map_err(|_| "共享引擎状态异常")?;
            stage = "stopping_gate";
            if stopping.load(Ordering::Acquire) && request["command"] != "cancel" {
                return Err("后台正在退出，不接受新请求".into());
            }
            stage = "engine_exit_probe";
            if let Some(current) = slot.as_mut() {
                if let Some(status) = current.child.try_wait().map_err(|e| e.to_string())? {
                    log_engine_exit(current, &status, "observed_exit");
                    slot.take();
                }
            }
            // T-23：通信断裂（响应超限/非法行/流中断）不终止引擎进程时，broken 永不
            // 复位会让后续所有请求连坐失败。终结断裂引擎并重建，让批次继续；在途
            // 请求已由 fail_pending 逐个交付失败终态，此处不存在并发在途项。
            let broken = slot
                .as_ref()
                .is_some_and(|engine| engine.broken.load(Ordering::Acquire));
            if broken {
                if let Some(mut dead) = slot.take() {
                    tracing::warn!(
                        event = "xberg_engine_rebuild_started",
                        reason = "communication_broken",
                        old_pid = dead.child.id(),
                        "共享引擎通信断裂：终结旧引擎并重建（T-23）"
                    );
                    let _ = kill_engine(&mut dead, "broken_rebuild");
                    let _ = wait_engine(&mut dead, "broken_rebuild");
                }
            }
            if let Some(current) = slot.as_mut() {
                if current.root != root {
                    stage = "runtime_switch_validate";
                    let saved = std::fs::canonicalize(xberg_settings::required()?)
                        .map_err(|e| e.to_string())?;
                    if saved != root {
                        return Err("请求使用旧目录，请读取最新共享配置".into());
                    }
                    if current
                        .pending
                        .lock()
                        .map_err(|_| "共享请求表异常")?
                        .values()
                        .any(|p| p.lane.is_some())
                    {
                        return Err("新来源已保存，等待当前任务安全结束后切换".into());
                    }
                    kill_engine(current, "runtime_switch").map_err(|e| e.to_string())?;
                    wait_engine(current, "runtime_switch").map_err(|e| e.to_string())?;
                    slot.take();
                }
            }
            if slot.is_none() {
                // 取消的目标只存在于引擎内的在途任务；引擎不在场（进程退出、通信
                // 断裂终结后的空档或停止窗口）时目标必然已按失败终态交付，为一条
                // 取消重新拉起引擎只会白付进程启动与模型加载，停止窗口内还会被
                // 看门狗立刻终结。按「无事可取消」直接应答，不进入 spawn 分支。
                if request["command"] == "cancel" {
                    tracing::info!(
                        event = "xberg_cancel_without_engine",
                        accepted = false,
                        "取消目标的引擎已不在场，不重新启动引擎"
                    );
                    return Ok(json!({"id":request["id"],"ok":true,"accepted":false}));
                }
                stage = "saved_runtime_validate";
                let saved = std::fs::canonicalize(xberg_settings::required()?)
                    .map_err(|e| format!("已保存的 Xberg 目录不可读：{e}"))?;
                if saved != root {
                    return Err("请求目录与 SQLite 保存的共享 Xberg 目录不一致".into());
                }
                stage = "engine_spawn";
                *slot = Some(Engine::spawn(&saved)?);
            }
            let current = slot.as_ref().ok_or("共享引擎初始化失败")?;
            if current.root != root {
                return Err("Xberg 新目录已保存；常驻引擎仍使用原目录。请从托盘退出后台并重新打开 JchTools 后使用新目录，不会中断当前任务或启动第二个引擎".into());
            }
            if request["command"] == "keepalive" {
                return Ok(
                    json!({"id":request["id"],"ok":true,"jchtools_xberg_pid":current.child.id()}),
                );
            }
            stage = "engine_submit";
            let receive = current.submit(request.clone())?;
            let timeout = Duration::from_millis(request["timeout_ms"].as_u64().unwrap_or(15_000))
                + QUERY_TIMEOUT;
            let pid = current.child.id();
            drop(slot); // 等待响应时不持有引擎锁；截图、取消和查询可立即提交。
            stage = "engine_terminal_wait";
            let result = receive.recv_timeout(timeout).map_err(|error| {
                tracing::warn!(
                    event = "xberg_broker_terminal_wait_failed",
                    engine_pid = pid,
                    stage = "engine_terminal_wait",
                    timed_out = matches!(error, mpsc::RecvTimeoutError::Timeout),
                    elapsed_ms = logging::elapsed_ms(started),
                    "共享代理未收到请求终态，保留场景占用"
                );
                "共享 Xberg 未返回请求终态；未终止其他任务".to_string()
            });
            // 超时/客户端离开不删除在途项；只有引擎终态或通信失效才能解除场景占用。
            result.map(|mut response| {
                response["jchtools_xberg_pid"] = json!(pid);
                response["jchtools_broker_pid"] = json!(std::process::id());
                response
            })
        })();
        let mut response = response.unwrap_or_else(|error: String| {
            tracing::warn!(
                event = "xberg_broker_dispatch_failed",
                command,
                stage,
                elapsed_ms = logging::elapsed_ms(started),
                "共享代理请求调度失败"
            );
            json!({"id":request["id"],"ok":false,"error_kind":"shared_runtime","error":error})
        });
        response["jchtools_broker_protocol"] = json!(BROKER_PROTOCOL);
        response["jchtools_broker_pid"] = json!(std::process::id());
        log_response("broker", command, &response, started, None);
        stage = "response_write";
        let _guard = IoDeadline::new(QUERY_TIMEOUT)?;
        write_json(reader.get_mut(), &response)?;
        tracing::debug!(
            event = "xberg_broker_response_sent",
            caller_pid,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理响应已发送"
        );
        stage = "response_flush";
        // SAFETY: 有效管道，等待客户端取走响应后断开；受上方 I/O 期限保护。
        let flushed = unsafe {
            windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(
                reader.get_ref().as_raw_handle().cast(),
            )
        };
        if flushed == 0 {
            // SAFETY: 本线程紧邻失败的 FlushFileBuffers 查询错误码。
            let code = unsafe { GetLastError() };
            tracing::warn!(
                event = "xberg_broker_response_flush_failed",
                stage = "response_flush",
                caller_pid,
                code,
                elapsed_ms = logging::elapsed_ms(started),
                "共享代理响应已写出，但未确认客户端取走；保持原有返回语义"
            );
        }
        Ok(())
    })();
    match &result {
        Ok(()) if command == "keepalive" => tracing::debug!(
            event = "xberg_broker_request_completed",
            caller_pid,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理已完成保活响应发送"
        ),
        Ok(()) => tracing::info!(
            event = "xberg_broker_request_completed",
            caller_pid,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理已完成请求响应发送"
        ),
        Err(_) => tracing::warn!(
            event = "xberg_broker_request_failed",
            caller_pid,
            command,
            stage,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理请求处理失败"
        ),
    }
    result
}

/// 代理是内部常驻进程，只经命名管道通信，不使用任何继承来的标准句柄。
/// Windows 上 Rust std 即使把本进程 stdio 全部置为 null，CreateProcess 仍会沿
/// 可继承句柄链把祖父进程（cargo / PowerShell 的 `& cmd 2>&1` 捕获）的匿名管道
/// 写端带进本进程；代理常驻会让上层捕获永远等不到 EOF，测试与验收管线悬挂
/// （回归：tests/xberg_shared_process.rs broker_does_not_hold_client_capture_pipes）。
/// 此处仅在 serve() 最前执行：本进程尚未创建任何自有管道，std 句柄是 NUL
/// 字符设备，能命中的只有继承来的管道。
fn close_inherited_pipes() {
    use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
    for value in (4usize..65536).step_by(4) {
        // SAFETY: handle 只是本进程句柄表的探测值，转换本身无副作用。
        let handle: HANDLE = value as HANDLE;
        let mut flags = 0u32;
        // SAFETY: 探测本进程句柄表中的值；无效值由返回 0 过滤，flags 是有效输出指针。
        let known = unsafe { GetHandleInformation(handle, &raw mut flags) };
        if known == 0 {
            continue;
        }
        // SAFETY: handle 已被 GetHandleInformation 确认有效；GetFileType 只读句柄类型。
        let file_type = unsafe { GetFileType(handle) };
        if file_type != FILE_TYPE_PIPE {
            continue;
        }
        // SAFETY: 关闭本进程内确认有效的继承管道句柄；此后不再有任何引用。
        let closed = unsafe { CloseHandle(handle) };
        debug_assert_ne!(closed, 0);
    }
}

pub(super) fn serve() -> Result<(), String> {
    let span = logging::operation_span("xberg_runtime", "broker_serve");
    let _entered = span.enter();
    let started = Instant::now();
    let mut stage = "state_directory";
    tracing::info!(event = "xberg_broker_serve_started", "共享代理初始化开始");
    let result = (|| {
        close_inherited_pipes();
        let root = xberg_settings::state_dir()?;
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        stage = "session_lock";
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(format!("xberg-{}.lock", identity()?)))
            .map_err(|e| e.to_string())?;
        match lock.try_lock_exclusive() {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tracing::info!(
                    event = "xberg_broker_already_running",
                    "共享代理已在运行，本进程不重复启动"
                );
                return Ok(());
            }
            Err(e) => {
                tracing::error!(
                    event = "xberg_broker_lock_failed",
                    error_code = e.raw_os_error(),
                    "共享引擎锁失败"
                );
                return Err(format!("共享引擎锁失败：{e}"));
            }
        }
        let name = pipe_name()?;
        if let Err(error) = background_allowed() {
            tracing::warn!(
                event = "xberg_broker_start_blocked",
                reason = "tray_stopped",
                "共享代理被托盘退出标记阻止"
            );
            return Err(error);
        }
        tracing::info!(
            event = "xberg_broker_listening",
            pid = std::process::id(),
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理开始监听"
        );
        let engine: Arc<Mutex<Option<Engine>>> = Arc::new(Mutex::new(None));
        let stopping = Arc::new(AtomicBool::new(false));
        let watch_engine = engine.clone();
        let watch_stopping = stopping.clone();
        let watch_span = tracing::Span::current();
        std::thread::spawn(move || {
            let _entered = watch_span.enter();
            let mut engine_lock_failure_logged = false;
            let mut pending_lock_failure_logged = false;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if !watch_stopping.load(Ordering::Acquire) {
                    // T-23 复审：reader 线程通信断裂时只能置 broken 并交付在途失败；
                    // 引擎进程本体若此后再无请求，会带着模型内存一直存活到代理退出。
                    // 非停止期巡检 broken 引擎并立即终结，把该窗口从「下一请求」
                    // 收敛到本循环周期；期间 handle() 的 T-23 重建分支同样以空槽重建，
                    // 两者不冲突（槽锁互斥）。
                    if let Ok(mut slot) = watch_engine.lock() {
                        let broken = slot
                            .as_ref()
                            .is_some_and(|engine| engine.broken.load(Ordering::Acquire));
                        if broken {
                            if let Some(mut dead) = slot.take() {
                                let kill_ok = kill_engine(&mut dead, "broken_watchdog").is_ok();
                                let wait_ok = wait_engine(&mut dead, "broken_watchdog").is_ok();
                                tracing::info!(
                                    event = "xberg_engine_watchdog_terminated",
                                    kill_ok,
                                    wait_ok,
                                    engine_pid = dead.child.id(),
                                    "看门狗已完成通信断裂引擎的终结处理（T-23）"
                                );
                            }
                        }
                    } else if !engine_lock_failure_logged {
                        engine_lock_failure_logged = true;
                        tracing::warn!(
                            event = "xberg_engine_watchdog_failed",
                            stage = "engine_lock",
                            error_kind = "mutex_poisoned",
                            "共享引擎看门狗状态锁异常，保留既有隔离状态"
                        );
                    }
                    continue;
                }
                if let Ok(mut slot) = watch_engine.lock() {
                    let active = slot.as_ref().is_some_and(|e| {
                        e.pending.lock().map_or_else(
                            |_| {
                                if !pending_lock_failure_logged {
                                    pending_lock_failure_logged = true;
                                    tracing::warn!(
                                        event = "xberg_engine_watchdog_failed",
                                        stage = "pending_lock",
                                        error_kind = "mutex_poisoned",
                                        "停止看门狗不能确认在途场景任务，保持后台运行"
                                    );
                                }
                                true
                            },
                            |p| p.values().any(|v| v.lane.is_some()),
                        )
                    });
                    if active {
                        continue;
                    }
                    if let Some(mut current) = slot.take() {
                        let kill_ok = kill_engine(&mut current, "stop_watchdog").is_ok();
                        let wait_ok = wait_engine(&mut current, "stop_watchdog").is_ok();
                        tracing::info!(
                            event = "xberg_engine_stop_watchdog_terminated",
                            kill_ok,
                            wait_ok,
                            engine_pid = current.child.id(),
                            "停止看门狗已完成共享引擎终结处理（XB-23）"
                        );
                    }
                    // P-10：exit(0) 会跳过 main 栈上的日志句柄析构，非阻塞写入线程
                    // 缓冲中的关键记录（含上方终结记录与本行）会整批丢失；退出前
                    // 显式刷盘，再终止进程。
                    tracing::info!(
                        event = "xberg_broker_stopped",
                        exit_code = 0,
                        elapsed_ms = logging::elapsed_ms(started),
                        "共享代理退出前刷盘诊断日志"
                    );
                    crate::logging::flush_before_exit();
                    std::process::exit(0);
                } else if !engine_lock_failure_logged {
                    engine_lock_failure_logged = true;
                    tracing::warn!(
                        event = "xberg_engine_watchdog_failed",
                        stage = "engine_lock",
                        error_kind = "mutex_poisoned",
                        "停止看门狗状态锁异常，保持后台运行"
                    );
                }
            }
        });
        loop {
            stage = "listener";
            let pipe = create_pipe(&name)?;
            // SAFETY: 同步管道的有效句柄；客户端先连接的 ERROR_PIPE_CONNECTED 也有效。
            let connected = unsafe {
                windows_sys::Win32::System::Pipes::ConnectNamedPipe(
                    pipe.as_raw_handle().cast(),
                    std::ptr::null_mut(),
                )
            };
            // SAFETY: 读取本线程紧邻 API 调用的错误码。
            if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
                // P-10：管道接受失败是 IPC 关键异常，必须落盘（含 Windows 错误码）。
                // SAFETY: 与上一行读取同一错误码，两次调用之间没有任何其他 API。
                let code = unsafe { GetLastError() };
                tracing::warn!(
                    event = "xberg_pipe_accept_failed",
                    stage = "accept",
                    code,
                    "共享管道接受客户端连接失败"
                );
                continue;
            }
            let engine = Arc::clone(&engine);
            let stopping = stopping.clone();
            let span = tracing::Span::current();
            std::thread::spawn(move || {
                let _entered = span.enter();
                if handle(pipe, &engine, &stopping).is_err() {
                    tracing::warn!(
                        event = "xberg_broker_connection_failed",
                        stage = "receive_or_handle",
                        "共享代理连接处理失败"
                    );
                }
            });
        }
    })();
    if result.is_err() {
        tracing::error!(
            event = "xberg_broker_serve_failed",
            stage,
            elapsed_ms = logging::elapsed_ms(started),
            "共享代理服务失败"
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // 覆盖 T-23 修复的转发收窄：只剔除产物不消费的重型字段，正文、嵌入子文档
    // 与告警逐字节保留；非 extract 响应（如 transcribe 的 markdown）不受影响。
    #[test]
    fn prune_keeps_product_fields_and_strips_heavy_fields_recursively() {
        let response = json!({
            "id": "r1",
            "ok": true,
            "document": {
                "content": "正文",
                "mime_type": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "images": [{"path": "image_0.png", "bytes": 12345}],
                "pages": [{"number": 1}],
                "tables": [{"rows": 2}],
                "processing_warnings": [{"source": "exif", "message": "no exif data found"}],
                "children": [{
                    "path": "word/embeddings/nested.docx",
                    "result": {
                        "content": "嵌入正文",
                        "images": [{"path": "image_1.png"}],
                        "pages": [1],
                        "processing_warnings": [{"source": "ocr", "message": "low confidence"}]
                    }
                }]
            }
        });
        let document = prune_forwarded_document(response)["document"].clone();
        assert_eq!(document["content"], "正文");
        assert_eq!(document["children"][0]["result"]["content"], "嵌入正文");
        assert_eq!(
            document["processing_warnings"][0]["message"],
            "no exif data found"
        );
        assert_eq!(
            document["children"][0]["result"]["processing_warnings"][0]["source"],
            "ocr"
        );
        // T-14（2026-10-04）：images 是图片落盘的唯一数据通道，转发侧必须放行
        //（含嵌入子文档逐层放行）；pages/tables 仍不进产物。
        assert!(document.get("images").is_some());
        assert_eq!(document["images"][0]["path"], "image_0.png");
        assert!(document["children"][0]["result"].get("images").is_some());
        assert!(document.get("pages").is_none());
        assert!(document.get("tables").is_none());
        assert!(document["children"][0]["result"].get("pages").is_none());
        let media = prune_forwarded_document(json!({"id":"m1","ok":true,"markdown":"media"}));
        assert_eq!(media["markdown"], "media");
    }

    /// 覆盖 XB-05/XB-17：超限响应只排空当前 JSON 行并流式提取顶层 ID，
    /// 即使 document 位于 id 之前且正文含嵌套伪 id，也不把另一场景的
    /// pending 请求一并标成通信断裂。
    #[test]
    fn oversized_line_is_drained_without_retaining_payload() {
        let mut input = std::io::Cursor::new(
            br#"{"document":{"nested":{"id":"fake"},"content":"0123456789"},"id":"document-1"}
{"id":"snapshot-1","ok":true}
"#
            .to_vec(),
        );
        let first = read_bounded_line(&mut input, 16).expect("第一行应能排空");
        match first {
            BoundedLine::Oversized { id, bytes } => {
                assert!(bytes > 16);
                assert_eq!(id.as_deref(), Some("document-1"));
            }
            BoundedLine::Complete(_) => panic!("超限行不应保留完整正文"),
        }
        let second = read_bounded_line(&mut input, MAX_MESSAGE).expect("下一行应保持同步");
        match second {
            BoundedLine::Complete(bytes) => {
                let response: Value = serde_json::from_slice(&bytes).expect("JSON 应完整");
                assert_eq!(response["id"], "snapshot-1");
            }
            BoundedLine::Oversized { .. } => panic!("第二行不应超限"),
        }
    }

    #[test]
    fn response_id_scanner_honors_json_escapes_and_depth() {
        let mut scanner = ResponseIdScanner::new();
        scanner
            .feed(br#"{"document":{"id":"fake","text":"quote \"id\": fake"},"id":"real-\u0031"}"#);
        assert_eq!(scanner.id.as_deref(), Some("real-1"));
    }

    #[test]
    fn response_id_scanner_ignores_structural_characters_inside_payload_strings() {
        // 覆盖 XB-14：正文不能影响响应 ID 和请求隔离。
        let mut scanner = ResponseIdScanner::new();
        scanner.feed(br#"{"document":{"text":"}], {\"id\":\"fake\"}"},"note":"{,}","id":"real"}"#);
        assert_eq!(scanner.id.as_deref(), Some("real"));
    }

    #[test]
    fn background_stop_marker_is_not_written_for_rejected_response() {
        let response = json!({
            "ok": false,
            "error": "文档任务仍在运行"
        });
        assert!(!should_mark_background_stopped(
            "broker-force-stop",
            &response
        ));
        assert!(should_mark_background_stopped(
            "broker-stop",
            &json!({"ok":true,"running":true,"stopping":true})
        ));
    }

    #[test]
    fn duration_max_is_rejected_before_broker_start() {
        let error = request_via_broker(
            Path::new("C:\\this-path-is-not-read"),
            &json!({"id":"duration-max","command":"keepalive"}),
            Duration::MAX,
            &AtomicBool::new(false),
            "duration-max",
        )
        .expect_err("Duration::MAX 必须在启动代理前明确拒绝");
        assert!(error.contains("未启动共享代理"));
    }

    #[derive(Clone)]
    struct LogCapture(mpsc::Sender<Vec<u8>>);

    impl Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.send(bytes.to_vec()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "诊断捕获端已关闭")
            })?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs() -> (tracing::Dispatch, mpsc::Receiver<Vec<u8>>) {
        let (send, bytes) = mpsc::channel();
        let writer = LogCapture(send);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        (tracing::Dispatch::new(subscriber), bytes)
    }

    #[test]
    fn consumed_response_logs_safe_results_and_decode_failure_stage() {
        let (dispatch, bytes) = capture_logs();
        let markdown = "private-success-markdown-sentinel";
        tracing::dispatcher::with_default(&dispatch, || {
            let span =
                logging::operation_span_with_id("xberg_runtime", "consume", "consume-response");
            let _entered = span.enter();
            let response = json!({"ok":false,"error_kind":"private-kind-sentinel","error":"private-error-sentinel","document":{"content":"private-document-sentinel"},"jchtools_xberg_pid":123});
            let encoded = serde_json::to_vec(&response).unwrap();
            let consumed = read_json(&mut std::io::Cursor::new(encoded)).unwrap();
            assert_eq!(consumed, response, "日志不得改变引擎失败响应");
            log_response("client", "extract", &consumed, Instant::now(), None);
            let success = json!({"ok":true,"markdown":markdown});
            let consumed = read_json(&mut std::io::Cursor::new(
                serde_json::to_vec(&success).unwrap(),
            ))
            .unwrap();
            assert_eq!(consumed, success, "诊断不得改变成功响应");
            log_response("client", "transcribe", &consumed, Instant::now(), None);
            let invalid = b"{private-invalid-json-sentinel}\n";
            assert!(read_json(&mut std::io::Cursor::new(invalid)).is_err());
        });
        let logs = String::from_utf8(bytes.try_iter().flatten().collect()).unwrap();
        assert!(logs.contains("xberg_response_failed"), "{logs}");
        assert!(logs.contains("remote_error"), "{logs}");
        assert!(logs.contains("xberg_response_completed"), "{logs}");
        assert!(
            logs.contains(&format!("markdown_bytes={}", markdown.len())),
            "{logs}"
        );
        assert!(!logs.contains(markdown), "成功响应正文泄漏：{logs}");
        assert!(logs.contains("xberg_message_decode_failed"), "{logs}");
        assert!(logs.contains("decode"), "{logs}");
        assert!(logs.contains("consume-response"), "{logs}");
        for sentinel in [
            "private-kind-sentinel",
            "private-error-sentinel",
            "private-document-sentinel",
            "private-invalid-json-sentinel",
        ] {
            assert!(!logs.contains(sentinel), "响应正文或不可信类型泄漏：{logs}");
        }
    }

    #[test]
    fn broken_engine_delivers_terminal_with_thread_request_context() -> Result<(), String> {
        let (dispatch, bytes) = capture_logs();
        let (send, receive) = mpsc::channel();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        tracing::dispatcher::with_default(&dispatch, || {
            let span = logging::operation_span_with_id(
                "xberg_runtime",
                "broker_handle",
                "pending-operation",
            );
            pending
                .lock()
                .map_err(|_| "请求表锁异常".to_string())?
                .insert(
                    "pending-operation".into(),
                    PendingRequest {
                        sender: send,
                        lane: Some("document"),
                        span,
                        started: Instant::now(),
                        command: "extract",
                    },
                );
            Ok::<(), String>(())
        })?;
        let worker_pending = Arc::clone(&pending);
        std::thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                fail_pending(&worker_pending, &AtomicBool::new(false));
            });
        })
        .join()
        .unwrap();
        let response = receive.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(response["id"], "pending-operation");
        assert_eq!(response["ok"], false);
        assert_eq!(response["error_kind"], "process_exited");
        assert!(pending
            .lock()
            .map_err(|_| "请求表锁异常".to_string())?
            .is_empty());
        let logs = String::from_utf8(bytes.try_iter().flatten().collect()).unwrap();
        let terminal = logs
            .lines()
            .find(|line| line.contains("xberg_engine_pending_failed"))
            .expect("必须记录在途失败终态");
        assert!(terminal.contains("pending-operation"), "{logs}");
        assert!(terminal.contains("communication_broken"), "{logs}");
        assert!(terminal.contains("elapsed_ms"), "{logs}");
        Ok(())
    }

    #[test]
    fn root_preflight_reuses_caller_diagnostic_id_without_logging_request_body() {
        let (dispatch, bytes) = capture_logs();
        tracing::dispatcher::with_default(&dispatch, || {
            let error = super::super::request(
                Path::new("C:\\this-path-is-not-read"),
                json!({"command":"ocr_snapshot","diagnostic_id":"snap-client-operation","image_base64":"private-image-sentinel"}),
                Duration::ZERO,
                &AtomicBool::new(false),
            ).expect_err("零预算必须在连接或能力握手前返回");
            assert!(error.contains("超时"));
        });
        let logs = String::from_utf8(bytes.try_iter().flatten().collect()).unwrap();
        let started = logs
            .lines()
            .find(|line| line.contains("xberg_request_started"))
            .expect("root started");
        let failed = logs
            .lines()
            .find(|line| line.contains("xberg_request_failed"))
            .expect("root failed");
        assert!(started.contains("snap-client-operation"), "{logs}");
        assert!(failed.contains("snap-client-operation"), "{logs}");
        assert!(failed.contains("preflight_timeout"), "{logs}");
        assert!(!logs.contains("private-image-sentinel"), "{logs}");
        assert!(
            !logs.contains("xberg_client_request_started"),
            "零预算不得越过请求预检：{logs}"
        );
    }
}
