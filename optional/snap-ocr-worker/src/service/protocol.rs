//! 当前用户会话专属命名管道：小型状态/控制消息，不传递截图或文字。

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::Command;

#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open: u32,
        mode: u32,
        instances: u32,
        out_buffer: u32,
        in_buffer: u32,
        timeout: u32,
        security: *const c_void,
    ) -> *mut c_void;
    fn ConnectNamedPipe(pipe: *mut c_void, overlapped: *mut c_void) -> i32;
    fn DisconnectNamedPipe(pipe: *mut c_void) -> i32;
    fn FlushFileBuffers(handle: *mut c_void) -> i32;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn GetLastError() -> u32;
    fn CreateMutexW(attributes: *const c_void, initial_owner: i32, name: *const u16)
        -> *mut c_void;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetCurrentThreadId() -> u32;
    fn OpenThread(access: u32, inherit: i32, id: u32) -> *mut c_void;
    fn CancelSynchronousIo(thread: *mut c_void) -> i32;
    fn GetNamedPipeClientProcessId(pipe: *mut c_void, pid: *mut u32) -> i32;
}
#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}

#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}

/// Win32 原生句柄值：管道实例句柄在创建与连接循环间传递的统一表示。
type Handle = *mut c_void;

pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hash() -> Result<String, String> {
    use std::fmt::Write as _;
    let isolated = if cfg!(any(test, feature = "test-hooks")) {
        std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
    } else {
        None
    };
    let digest = if let Some(root) = &isolated {
        let mut bytes = b"test:".to_vec();
        bytes.extend_from_slice(root.as_os_str().as_encoded_bytes());
        Sha256::digest(bytes)
    } else {
        Sha256::digest(jchtools_foundation::process_identity::user_session_key()?.as_bytes())
    };
    let mut text = if isolated.is_some() {
        "test-".to_string()
    } else {
        String::with_capacity(16)
    };
    for byte in digest.iter().take(8) {
        let _ = write!(text, "{byte:02x}");
    }
    Ok(text)
}

pub(crate) fn pipe_name() -> Result<String, String> {
    Ok(format!(r"\\.\pipe\jchtools-snap-ocr-{}", hash()?))
}

pub(crate) struct InstanceGuard(*mut c_void);
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: self.0 是 CreateMutexW 返回并由本 guard 独占持有的互斥体句柄，
        // Drop 恰好关闭一次；句柄从未交给 File::from_raw_handle 等其他所有者，不会双重释放。
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct ThreadHandle(*mut c_void);
impl Drop for ThreadHandle {
    fn drop(&mut self) {
        // SAFETY: self.0 是 OpenThread 返回并由本类型独占持有的线程句柄，
        // Drop 恰好关闭一次；CloseHandle 只释放句柄不终止线程，服务线程生命周期不受影响。
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// 单个客户端不能无限占用唯一管道实例；超时后取消本线程正在等待的同步 I/O。
struct RequestDeadline {
    done: Sender<()>,
    watchdog: Option<std::thread::JoinHandle<()>>,
}
impl RequestDeadline {
    fn start(thread: &ThreadHandle, timeout: Duration) -> Result<Self, String> {
        let (done, receiver) = mpsc::channel();
        let handle = thread.0 as usize;
        let span = tracing::Span::current();
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        let watchdog = std::thread::Builder::new()
            .name("snap-ocr-pipe-deadline".into())
            .spawn(move || {
                tracing::dispatcher::with_default(&dispatcher, || {
                    let _entered = span.enter();
                    if receiver.recv_timeout(timeout).is_ok() {
                        return;
                    }
                    tracing::warn!(
                        event = "ocr_control_deadline_expired",
                        stage = "synchronous_io",
                        error_type = "timeout",
                        "截图控制管道请求期限耗尽"
                    );
                    // 期限恰好落在两次 I/O 之间时，第一次取消可能找不到待处理请求。
                    // 持续取消直到服务线程结束该连接，避免下一次 read/flush 无限等待。
                    loop {
                        // SAFETY: 句柄值拷贝自 OpenThread(THREAD_TERMINATE) 打开的本服务线程，底层
                        // 句柄由 serve_named 里的 ThreadHandle 独占持有，watchdog 运行期间保持有效；
                        // 目标线程没有等待中的同步 I/O 时调用仅返回错误，忽略返回值由循环重试兜底。
                        unsafe {
                            CancelSynchronousIo(handle as *mut c_void);
                        }
                        match receiver.recv_timeout(Duration::from_millis(10)) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                    }
                });
            })
            .map_err(|error| format!("截图控制管道计时器启动失败：{error}"))?;
        Ok(Self {
            done,
            watchdog: Some(watchdog),
        })
    }
}
impl Drop for RequestDeadline {
    fn drop(&mut self) {
        let _ = self.done.send(());
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

/// 单实例锁是 Local（登录会话隔离），相同用户的不同远程登录会话不互相阻塞。
pub(crate) fn claim_instance() -> Result<Option<InstanceGuard>, String> {
    let name = wide(&format!("Local\\JchToolsSnapOcr-{}", hash()?));
    // SAFETY: name 是 NUL 结尾宽字符串且在本函数存续期间有效；返回 NULL 表示创建失败。
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        // SAFETY: 紧接失败的 CreateMutexW，读取本线程 Win32 错误码。
        let error_code = unsafe { GetLastError() };
        tracing::error!(
            event = "ocr_instance_claim_failed",
            stage = "create_mutex",
            error_code,
            "截图服务单实例锁创建失败"
        );
        return Err("截图服务单实例锁创建失败".into());
    }
    // SAFETY: 紧接 CreateMutexW 之后读取错误码，其间无其它 API 调用，183（ERROR_ALREADY_EXISTS）语义有效。
    let last_error = unsafe { GetLastError() };
    if last_error == 183 {
        // SAFETY: 句柄非空且尚未交给 InstanceGuard，所有权仍在本函数，关闭一次不与 Drop 双重释放。
        unsafe {
            CloseHandle(handle);
        }
        return Ok(None);
    }
    Ok(Some(InstanceGuard(handle)))
}

/// 单请求一连接；同一个管道实例在请求间 Disconnect/Connect，不留无监听者的空窗。
/// SDDL 仅允许对象所有者（当前登录用户）及 SYSTEM；拒绝远程管道客户端。
pub(crate) fn serve(commands: &Sender<Command>, ready: &mpsc::SyncSender<Result<(), String>>) {
    match pipe_name() {
        Ok(name) => serve_named(&name, commands, ready, Duration::from_secs(15)),
        Err(error) => {
            // 身份失败时禁止回退到其他用户或匿名端点。
            let _ = ready.send(Err(error));
        }
    }
}

fn serve_named(
    pipe_name: &str,
    commands: &Sender<Command>,
    ready: &mpsc::SyncSender<Result<(), String>>,
    request_timeout: Duration,
) {
    let startup = Instant::now();
    tracing::info!(
        event = "ocr_control_listener_started",
        "开始监听截图控制管道"
    );
    let name = wide(pipe_name);
    // CancelSynchronousIo 需要真实线程句柄及 THREAD_TERMINATE 权限。
    // SAFETY: GetCurrentThreadId 无参数且无副作用，仅返回当前线程 ID。
    let thread_id = unsafe { GetCurrentThreadId() };
    // SAFETY: thread_id 即当前线程，以 THREAD_TERMINATE(0x0001)——CancelSynchronousIo 所需的
    // 访问权限——打开；返回 NULL 时由下方判空退出，非空句柄交给 ThreadHandle 唯一释放。
    let server_thread = unsafe { OpenThread(0x0001, 0, thread_id) };
    if server_thread.is_null() {
        // SAFETY: 紧接失败的 OpenThread，读取本线程 Win32 错误码。
        let error_code = unsafe { GetLastError() };
        tracing::error!(
            event = "ocr_control_listener_failed",
            stage = "open_thread",
            error_type = "win32",
            error_code,
            elapsed_ms = crate::logging::elapsed_ms(startup),
            "截图控制管道请求期限初始化失败"
        );
        let _ = ready.send(Err("截图控制管道无法设置请求期限".into()));
        return;
    }
    let server_thread = ThreadHandle(server_thread);
    let raw = match create_pipe_instance(&name, startup) {
        Ok(raw) => raw,
        Err(reason) => {
            let _ = ready.send(Err(reason));
            return;
        }
    };
    // SAFETY: 返回的独占句柄交给 File 管理，服务存续期间不关闭。
    let mut pipe = unsafe { std::fs::File::from_raw_handle(raw) };
    let _ = ready.send(Ok(()));
    tracing::info!(
        event = "ocr_control_listener_ready",
        elapsed_ms = crate::logging::elapsed_ms(startup),
        "截图控制管道已监听"
    );
    loop {
        // SAFETY: raw 是本服务创建的管道实例句柄，在循环存续期间有效；同步等待客户端连接。
        let connect_result = unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) };
        // SAFETY: 535（ERROR_PIPE_CONNECTED）表示客户端先于监听调用连接，仍是有效连接；
        // 仅在 ConnectNamedPipe 返回 0 时读取，错误码属于本次连接调用。
        let connect_error = if connect_result == 0 {
            // SAFETY: 读取当前线程刚刚失败的 Win32 调用错误码，无指针或句柄参数。
            unsafe { GetLastError() }
        } else {
            0
        };
        let connected = connect_result != 0 || connect_error == 535;
        if !connected {
            tracing::warn!(
                event = "ocr_control_connection_failed",
                stage = "connect",
                error_code = connect_error,
                "截图控制管道连接失败"
            );
            if connect_error == 232 {
                // SAFETY: ERROR_NO_DATA 表示客户端已关闭、但本管道实例尚未断开。
                // 必须先 Disconnect 才能再次监听；raw 仍由 pipe 独占持有，不关闭句柄。
                unsafe {
                    DisconnectNamedPipe(raw);
                }
            }
            continue;
        }
        serve_connection(&mut pipe, raw, commands, &server_thread, request_timeout);
    }
}

/// 创建一个命名管道实例：承接 SDDL 转换、安全属性装配与 CreateNamedPipeW，
/// 安全描述符在函数内恰好释放一次；初始化失败时记录对应错误日志并携带原
/// 错误文案上抛，由调用方统一 ready 通知。startup 用于延续监听启动日志的耗时口径。
fn create_pipe_instance(name: &[u16], startup: Instant) -> Result<Handle, String> {
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;OW)");
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: sddl 是 NUL 结尾宽字符串且在调用期间有效；descriptor 出参指向本栈变量，成功时
    // 承接由 LocalFree 释放的安全描述符，失败时保持初始 NULL；revision=1 即 SDDL_REVISION_1。
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        // SAFETY: 读取 ConvertStringSecurityDescriptorToSecurityDescriptorW 失败留下的错误码，
        // 其间未调用会改写错误码的其它 API。
        let convert_error = unsafe { GetLastError() };
        tracing::error!(
            event = "ocr_control_listener_failed",
            stage = "security_descriptor",
            error_code = convert_error,
            elapsed_ms = crate::logging::elapsed_ms(startup),
            "截图控制管道权限初始化失败"
        );
        return Err(format!("截图控制管道权限初始化失败：{convert_error}"));
    }
    let Ok(length) = u32::try_from(std::mem::size_of::<SecurityAttributes>()) else {
        tracing::error!(
            event = "ocr_control_listener_failed",
            stage = "security_size",
            error_type = "size_overflow",
            elapsed_ms = crate::logging::elapsed_ms(startup),
            "截图控制管道权限结构尺寸无效"
        );
        // SAFETY: descriptor 是上方转换成功返回、尚未释放的本地分配指针；本分支直接返回，
        // 不会再构造 attributes 或使用该指针，此处恰好释放一次。
        unsafe {
            LocalFree(descriptor);
        }
        return Err("截图控制管道权限结构尺寸无效".into());
    };
    let attributes = SecurityAttributes {
        length,
        descriptor,
        inherit: 0,
    };
    // PIPE_ACCESS_DUPLEX；PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT |
    // PIPE_REJECT_REMOTE_CLIENTS。只有对象所有者及 SYSTEM 能读写。
    // SAFETY: name 是 NUL 结尾宽字符串且在调用期间有效；attributes 指向本栈上 repr(C) 布局与
    // Win32 SECURITY_ATTRIBUTES 一致的结构，调用只读取不保留指针；失败返回 INVALID_HANDLE_VALUE(-1)，
    // 由下方判断处理。
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            0x0000_0003,
            0x0000_0008,
            1,
            4096,
            4096,
            0,
            (&raw const attributes).cast(),
        )
    };
    // SAFETY: 在 CreateNamedPipeW 之后、LocalFree 之前读取，错误码属于本次管道创建。
    let create_error = unsafe { GetLastError() };
    // SAFETY: descriptor 是转换成功返回的本地分配指针；CreateNamedPipeW 已读取完安全属性，
    // 此后不再使用该指针，恰好释放一次。
    unsafe {
        LocalFree(descriptor);
    }
    if raw as isize == -1 {
        tracing::error!(
            event = "ocr_control_listener_failed",
            stage = "create_pipe",
            error_code = create_error,
            elapsed_ms = crate::logging::elapsed_ms(startup),
            "截图控制管道创建失败"
        );
        return Err(format!("截图控制管道创建失败：{create_error}"));
    }
    Ok(raw)
}

/// 处理单个已连接客户端：读取一条控制请求、分派到服务、写回响应并记录
/// 结果日志，最后断开本连接；请求期限由 request_timeout 与挂靠 server_thread
/// 的同步 I/O 取消共同保障。
fn serve_connection(
    pipe: &mut std::fs::File,
    raw: Handle,
    commands: &Sender<Command>,
    server_thread: &ThreadHandle,
    request_timeout: Duration,
) {
    let connection_span = crate::logging::operation_span("snap_ocr_control", "connection");
    let _connection_entered = connection_span.enter();
    let connection_started = Instant::now();
    let mut pid = 0u32;
    // SAFETY: raw 是已连接的独占管道句柄，pid 出参指向本栈 DWORD。
    let peer_pid = (unsafe { GetNamedPipeClientProcessId(raw, &raw mut pid) } != 0).then_some(pid);
    tracing::debug!(
        event = "ocr_control_connection_started",
        peer_pid,
        "收到截图控制管道连接"
    );
    let Ok(deadline) = RequestDeadline::start(server_thread, request_timeout) else {
        tracing::error!(
            event = "ocr_control_request_failed",
            stage = "deadline_start",
            error_type = "thread_spawn",
            peer_pid,
            elapsed_ms = crate::logging::elapsed_ms(connection_started),
            "截图控制请求计时器启动失败"
        );
        // SAFETY: raw 是刚连接的本服务管道实例句柄；Disconnect 只断开客户端连接不关闭句柄，
        // 句柄仍由 pipe（File）独占持有，继续下一轮连接。
        unsafe {
            DisconnectNamedPipe(raw);
        }
        return;
    };
    let started = Instant::now();
    let mut request = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    let mut terminated = false;
    while request.len() < 4096 && started.elapsed() < request_timeout {
        if let Err(error) = pipe.read_exact(&mut byte) {
            tracing::warn!(
                event = "ocr_control_read_failed",
                stage = "read",
                error_type = "io",
                error_code = error.raw_os_error(),
                peer_pid,
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图控制消息读取失败"
            );
            break;
        }
        if byte[0] == b'\n' {
            terminated = true;
            break;
        }
        request.push(byte[0]);
    }
    if started.elapsed() >= request_timeout {
        tracing::warn!(
            event = "ocr_control_request_failed",
            stage = "read_deadline",
            error_type = "timeout",
            peer_pid,
            elapsed_ms = crate::logging::elapsed_ms(started),
            "截图控制消息读取超时"
        );
        // SAFETY: raw 是本服务管道实例句柄；断开超时客户端不关闭句柄，
        // 句柄仍由 pipe（File）独占持有，继续下一轮连接。
        unsafe {
            DisconnectNamedPipe(raw);
        }
        drop(deadline);
        return;
    }
    let mut diagnostic_id = crate::logging::new_operation_id();
    let mut command_kind = "invalid";
    let mut failure_stage = None;
    let response = if terminated {
        if let Ok(value) = serde_json::from_slice::<Value>(&request) {
            if let Some(id) = value["diagnostic_id"].as_str().filter(|id| {
                !id.is_empty()
                    && id.len() <= 96
                    && id.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            }) {
                id.clone_into(&mut diagnostic_id);
            }
            command_kind = diagnostic_command(value["command"].as_str());
            let span = crate::logging::operation_span_with_id(
                "snap_ocr_control",
                "request",
                &diagnostic_id,
            );
            let _entered = span.enter();
            tracing::debug!(
                event = "ocr_control_request_started",
                command = command_kind,
                peer_pid,
                "开始处理截图控制请求"
            );
            tracing::debug!(
                event = "ocr_control_request_received",
                command = command_kind,
                peer_pid,
                "截图控制请求已解码"
            );
            let (tx, rx) = mpsc::sync_channel(1);
            if commands
                .send(Command::Pipe(value, tx, span.clone()))
                .is_ok()
            {
                rx.recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|_| {
                        failure_stage = Some("service_response_timeout");
                        json!({"ok":false,"error":"截图服务响应超时"})
                    })
            } else {
                failure_stage = Some("service_dispatch");
                json!({"ok":false,"error":"截图服务已关闭"})
            }
        } else {
            failure_stage = Some("decode");
            json!({"ok":false,"error":"无效的控制消息"})
        }
    } else {
        failure_stage = Some("framing");
        json!({"ok":false,"error":"控制消息过长或未完成"})
    };
    let span =
        crate::logging::operation_span_with_id("snap_ocr_control", "request", &diagnostic_id);
    let _entered = span.enter();
    let mut response = response;
    response["diagnostic_id"] = json!(diagnostic_id);
    let mut delivered = false;
    match serde_json::to_vec(&response) {
        Ok(mut line) => {
            line.push(b'\n');
            if started.elapsed() < request_timeout {
                match pipe.write_all(&line) {
                    Ok(()) => {
                        // SAFETY: raw 与写入响应的 pipe 为同一独占有效句柄。
                        delivered = unsafe { FlushFileBuffers(raw) } != 0;
                        if !delivered {
                            // SAFETY: 紧接失败的 FlushFileBuffers，读取本线程错误码。
                            let error_code = unsafe { GetLastError() };
                            tracing::warn!(
                                event = "ocr_control_flush_failed",
                                stage = "flush",
                                error_code,
                                peer_pid,
                                elapsed_ms = crate::logging::elapsed_ms(started),
                                "截图控制响应刷新失败"
                            );
                            failure_stage = Some("flush");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            event = "ocr_control_write_failed",
                            stage = "write",
                            error_type = "io",
                            error_code = error.raw_os_error(),
                            peer_pid,
                            elapsed_ms = crate::logging::elapsed_ms(started),
                            "截图控制响应写入失败"
                        );
                        failure_stage = Some("write");
                    }
                }
            } else {
                failure_stage = Some("write_deadline");
            }
        }
        Err(_) => failure_stage = Some("encode"),
    }
    if delivered && response["ok"] == true && failure_stage.is_none() {
        tracing::debug!(
            event = "ocr_control_request_completed",
            command = command_kind,
            peer_pid,
            elapsed_ms = crate::logging::elapsed_ms(started),
            "截图控制请求完成"
        );
    } else {
        tracing::warn!(
            event = "ocr_control_request_failed",
            command = command_kind,
            peer_pid,
            stage = failure_stage.unwrap_or("command"),
            delivered,
            elapsed_ms = crate::logging::elapsed_ms(started),
            "截图控制请求未成功"
        );
    }
    // SAFETY: raw 是本服务管道实例句柄；请求处理完毕断开客户端不关闭句柄，
    // 句柄仍由 pipe（File）独占持有，继续下一轮连接。
    unsafe {
        DisconnectNamedPipe(raw);
    }
    drop(deadline);
}

pub(super) fn diagnostic_command(command: Option<&str>) -> &'static str {
    match command {
        Some("ping") => "ping",
        Some("get-state") => "get-state",
        Some("open-settings") => "open-settings",
        Some("save-settings") => "save-settings",
        Some("attach-main-exe") => "attach-main-exe",
        Some("retry-load") => "retry-load",
        Some("capture") => "capture",
        Some("cancel") => "cancel",
        Some("shutdown") => "shutdown",
        _ => "unknown",
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
mod tests;

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
mod background_isolation_tests;
