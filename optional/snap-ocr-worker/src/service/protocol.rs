//! 当前用户会话专属命名管道：小型状态/控制消息，不传递截图或文字。

use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{self, Sender};

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
    let name = wide(&pipe_name());
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
        let mut request = Vec::with_capacity(256);
        let mut byte = [0u8; 1];
        let mut terminated = false;
        while request.len() < 4096 {
            if pipe.read_exact(&mut byte).is_err() {
                break;
            }
            if byte[0] == b'\n' {
                terminated = true;
                break;
            }
            request.push(byte[0]);
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
            let _ = pipe.write_all(&line);
            unsafe {
                FlushFileBuffers(raw);
            }
        }
        unsafe {
            DisconnectNamedPipe(raw);
        }
    }
}
