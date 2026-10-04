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
    fn GetCurrentProcessId() -> u32;
    fn GetCurrentThreadId() -> u32;
    fn OpenThread(access: u32, inherit: i32, id: u32) -> *mut c_void;
    fn CancelSynchronousIo(thread: *mut c_void) -> i32;
    fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
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

pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn identity() -> String {
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "anonymous".to_string());
    let mut session = u32::MAX;
    // SAFETY: GetCurrentProcessId 无参数、无副作用，任意线程可安全调用。
    let pid = unsafe { GetCurrentProcessId() };
    // SAFETY: session 指向本栈上类型与宽度都匹配 DWORD 的变量；调用失败时保留 u32::MAX 兜底值。
    unsafe {
        ProcessIdToSessionId(pid, &raw mut session);
    }
    format!("{user}:{session}")
}

fn hash() -> String {
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
        Sha256::digest(identity().as_bytes())
    };
    let mut text = if isolated.is_some() {
        "test-".to_string()
    } else {
        String::with_capacity(16)
    };
    for byte in digest.iter().take(8) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

pub(crate) fn pipe_name() -> String {
    format!(r"\\.\pipe\jchtools-snap-ocr-{}", hash())
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
        let watchdog = std::thread::Builder::new()
            .name("snap-ocr-pipe-deadline".into())
            .spawn(move || {
                if receiver.recv_timeout(timeout).is_ok() {
                    return;
                }
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
    let name = wide(&format!("Local\\JchToolsSnapOcr-{}", hash()));
    // SAFETY: name 是 NUL 结尾宽字符串且在本函数存续期间有效；返回 NULL 表示创建失败。
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
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
    serve_named(&pipe_name(), commands, ready, Duration::from_secs(15));
}

fn serve_named(
    pipe_name: &str,
    commands: &Sender<Command>,
    ready: &mpsc::SyncSender<Result<(), String>>,
    request_timeout: Duration,
) {
    let name = wide(pipe_name);
    // CancelSynchronousIo 需要真实线程句柄及 THREAD_TERMINATE 权限。
    // SAFETY: GetCurrentThreadId 无参数且无副作用，仅返回当前线程 ID。
    let thread_id = unsafe { GetCurrentThreadId() };
    // SAFETY: thread_id 即当前线程，以 THREAD_TERMINATE(0x0001)——CancelSynchronousIo 所需的
    // 访问权限——打开；返回 NULL 时由下方判空退出，非空句柄交给 ThreadHandle 唯一释放。
    let server_thread = unsafe { OpenThread(0x0001, 0, thread_id) };
    if server_thread.is_null() {
        let _ = ready.send(Err("截图控制管道无法设置请求期限".into()));
        return;
    }
    let server_thread = ThreadHandle(server_thread);
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
        let _ = ready.send(Err(format!("截图控制管道权限初始化失败：{convert_error}")));
        return;
    }
    let Ok(length) = u32::try_from(std::mem::size_of::<SecurityAttributes>()) else {
        // SAFETY: descriptor 是上方转换成功返回、尚未释放的本地分配指针；本分支直接返回，
        // 不会再构造 attributes 或使用该指针，此处恰好释放一次。
        unsafe {
            LocalFree(descriptor);
        }
        let _ = ready.send(Err("截图控制管道权限结构尺寸无效".into()));
        return;
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
        let _ = ready.send(Err(format!("截图控制管道创建失败：{create_error}")));
        return;
    }
    // SAFETY: 返回的独占句柄交给 File 管理，服务存续期间不关闭。
    let mut pipe = unsafe { std::fs::File::from_raw_handle(raw) };
    let _ = ready.send(Ok(()));
    loop {
        // SAFETY: raw 是本服务创建的管道实例句柄，在循环存续期间有效；同步等待客户端连接。
        let connect_result = unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) };
        // SAFETY: 535（ERROR_NO_DATA）表示客户端已连接后断开，视为已连接；仅在
        // ConnectNamedPipe 返回 0 时经 || 短路求值读取，错误码属于本次连接调用。
        let connected = connect_result != 0 || unsafe { GetLastError() } == 535;
        if !connected {
            continue;
        }
        let Ok(deadline) = RequestDeadline::start(&server_thread, request_timeout) else {
            // SAFETY: raw 是刚连接的本服务管道实例句柄；Disconnect 只断开客户端连接不关闭句柄，
            // 句柄仍由 pipe（File）独占持有，继续下一轮连接。
            unsafe {
                DisconnectNamedPipe(raw);
            }
            continue;
        };
        let started = Instant::now();
        let mut request = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        let mut terminated = false;
        while request.len() < 4096 && started.elapsed() < request_timeout {
            if pipe.read_exact(&mut byte).is_err() {
                break;
            }
            if byte[0] == b'\n' {
                terminated = true;
                break;
            }
            request.push(byte[0]);
        }
        if started.elapsed() >= request_timeout {
            // SAFETY: raw 是本服务管道实例句柄；断开超时客户端不关闭句柄，
            // 句柄仍由 pipe（File）独占持有，继续下一轮连接。
            unsafe {
                DisconnectNamedPipe(raw);
            }
            drop(deadline);
            continue;
        }
        let response = if terminated {
            match serde_json::from_slice::<Value>(&request) {
                Ok(value) => {
                    let (tx, rx) = mpsc::sync_channel(1);
                    if commands.send(Command::Pipe(value, tx)).is_ok() {
                        rx.recv_timeout(std::time::Duration::from_secs(10))
                            .unwrap_or_else(|_| json!({"ok":false,"error":"截图服务响应超时"}))
                    } else {
                        json!({"ok":false,"error":"截图服务已关闭"})
                    }
                }
                Err(_) => json!({"ok":false,"error":"无效的控制消息"}),
            }
        } else {
            json!({"ok":false,"error":"控制消息过长或未完成"})
        };
        if let Ok(mut line) = serde_json::to_vec(&response) {
            line.push(b'\n');
            if started.elapsed() < request_timeout && pipe.write_all(&line).is_ok() {
                // SAFETY: raw 与刚完成 write_all 的 pipe 是同一独占句柄；阻塞刷新确保响应送达，
                // 失败仅影响本次响应，忽略返回值后进入断开与下一轮连接。
                unsafe {
                    FlushFileBuffers(raw);
                }
            }
        }
        // SAFETY: raw 是本服务管道实例句柄；请求处理完毕断开客户端不关闭句柄，
        // 句柄仍由 pipe（File）独占持有，继续下一轮连接。
        unsafe {
            DisconnectNamedPipe(raw);
        }
        drop(deadline);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{serve_named, Command};

    static NEXT_PIPE: AtomicU64 = AtomicU64::new(0);

    // 覆盖 O-11：单个无响应客户端不能让常驻服务的控制入口永久失效。
    #[test]
    fn unfinished_client_does_not_block_next_control_request(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let name = format!(
            r"\\.\pipe\jchtools-snap-ocr-test-{}-{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        );
        let (commands_tx, commands_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let server_name = name.clone();
        std::thread::spawn(move || {
            serve_named(
                &server_name,
                &commands_tx,
                &ready_tx,
                Duration::from_millis(300),
            );
        });
        ready_rx.recv_timeout(Duration::from_secs(2))??;
        std::thread::spawn(move || {
            while let Ok(Command::Pipe(_, response)) = commands_rx.recv() {
                let _ = response.send(json!({"ok":true}));
            }
        });

        let mut unfinished = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&name)?;
        unfinished.write_all(b"{\"command\":\"ping\"")?;
        std::thread::sleep(Duration::from_millis(50));

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut next = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
            {
                Ok(pipe) => break pipe,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        };
        next.write_all(b"{\"command\":\"ping\"}\n")?;
        let mut response = String::new();
        BufReader::new(&mut next).read_line(&mut response)?;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response)?["ok"],
            true
        );
        drop(next);
        drop(unfinished);

        // 完整请求的客户端若不读取响应，FlushFileBuffers 也不能永久占住服务。
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut unread = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
            {
                Ok(pipe) => break pipe,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        };
        unread.write_all(b"{\"command\":\"ping\"}\n")?;
        std::thread::sleep(Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut after_unread = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
            {
                Ok(pipe) => break pipe,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        };
        after_unread.write_all(b"{\"command\":\"ping\"}\n")?;
        response.clear();
        BufReader::new(&mut after_unread).read_line(&mut response)?;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response)?["ok"],
            true
        );
        Ok(())
    }
}

#[cfg(test)]
mod background_isolation_tests {
    use super::*;
    // 覆盖 XB-22：真实后台测试与 GUI 使用同一隔离控制端点，不接触用户服务。
    #[test]
    fn isolated_background_pipe_matches_gui_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let old = std::env::var_os("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
        std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", root.path());
        let mut identity = b"test:".to_vec();
        identity.extend_from_slice(root.path().as_os_str().as_encoded_bytes());
        let digest = format!("{:x}", Sha256::digest(&identity));
        let actual = pipe_name();
        if let Some(value) = old {
            std::env::set_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", value);
        } else {
            std::env::remove_var("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT");
        }
        assert_eq!(
            actual,
            format!(r"\\.\pipe\jchtools-snap-ocr-test-{}", &digest[..16])
        );
    }
}
