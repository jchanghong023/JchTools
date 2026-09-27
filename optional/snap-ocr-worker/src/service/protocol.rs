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

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn identity() -> String {
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "anonymous".to_string());
    let mut session = u32::MAX;
    unsafe {
        ProcessIdToSessionId(GetCurrentProcessId(), &raw mut session);
    }
    format!("{user}:{session}")
}

fn hash() -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(identity().as_bytes());
    let mut text = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

pub fn pipe_name() -> String {
    format!(r"\\.\pipe\jchtools-snap-ocr-{}", hash())
}

pub struct InstanceGuard(*mut c_void);
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct ThreadHandle(*mut c_void);
impl Drop for ThreadHandle {
    fn drop(&mut self) {
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
pub fn claim_instance() -> Result<Option<InstanceGuard>, String> {
    let name = wide(&format!("Local\\JchToolsSnapOcr-{}", hash()));
    // SAFETY: NUL 结尾宽字符串在调用期间保持有效，句柄由 guard 释放。
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            return Err("截图服务单实例锁创建失败".into());
        }
        if GetLastError() == 183 {
            CloseHandle(handle);
            return Ok(None);
        }
        Ok(Some(InstanceGuard(handle)))
    }
}

/// 单请求一连接；同一个管道实例在请求间 Disconnect/Connect，不留无监听者的空窗。
/// SDDL 仅允许对象所有者（当前登录用户）及 SYSTEM；拒绝远程管道客户端。
pub fn serve(commands: &Sender<Command>, ready: &mpsc::SyncSender<Result<(), String>>) {
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
    let server_thread = unsafe { OpenThread(0x0001, 0, GetCurrentThreadId()) };
    if server_thread.is_null() {
        let _ = ready.send(Err("截图控制管道无法设置请求期限".into()));
        return;
    }
    let server_thread = ThreadHandle(server_thread);
    let sddl = wide("D:P(A;;GA;;;SY)(A;;GA;;;OW)");
    let mut descriptor = std::ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        let _ = ready.send(Err(format!(
            "截图控制管道权限初始化失败：{}",
            unsafe { GetLastError() }
        )));
        return;
    }
    let Ok(length) = u32::try_from(std::mem::size_of::<SecurityAttributes>()) else {
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
    let create_error = unsafe { GetLastError() };
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
        let connected =
            unsafe { ConnectNamedPipe(raw, std::ptr::null_mut()) != 0 || GetLastError() == 535 };
        if !connected {
            continue;
        }
        let Ok(deadline) = RequestDeadline::start(&server_thread, request_timeout) else {
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
                unsafe {
                    FlushFileBuffers(raw);
                }
            }
        }
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
