//! 清场前的原生必要条件探测：只观察测试路径下的引擎/worker，不启动或结束进程。
//!
//! worker 清场要求自身路径属于测试；JchTools 代理清场要求拥有测试路径下的
//! xberg 子进程。因此两类映像均无测试路径命中时，原有脚本没有可结束的目标。
//! 探测不代替原脚本的所有权过滤；任何无法确定的查询结果都保守进入原清场。

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

#[cfg(windows)]
struct OwnedHandle(HANDLE);

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: 仅在创建成功后构造，独占真实快照或进程句柄，且只在此关闭一次。
        unsafe { CloseHandle(self.0) };
    }
}

/// 判断是否可能存在原清场脚本可处理的测试派生引擎或 worker。
#[cfg(windows)]
pub(super) fn any_test_owned_engine_alive() -> bool {
    // SAFETY: 仅获取进程快照，无外部指针；进程 ID 在此标志下被忽略。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return true;
    }
    let snapshot = OwnedHandle(snapshot);
    let Ok(entry_size) = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()) else {
        return true;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: entry_size,
        ..PROCESSENTRY32W::default()
    };
    // SAFETY: 快照句柄有效，entry 为已设置正确 dwSize 的完整可写结构。
    if unsafe { Process32FirstW(snapshot.0, &raw mut entry) } == 0 {
        // SAFETY: 紧接失败的枚举调用读取本线程错误码，无指针或额外资源。
        return unsafe { GetLastError() } != ERROR_NO_MORE_FILES;
    }
    loop {
        let Some(length) = entry.szExeFile.iter().position(|&unit| unit == 0) else {
            // 未终止的映像名无法可靠判断，不能据此跳过清场。
            return true;
        };
        let name = &entry.szExeFile[..length];
        let candidate = ["xberg.exe", "snap-ocr-worker.exe"].iter().any(|image| {
            name.len() == image.len()
                && name.iter().zip(image.bytes()).all(|(&unit, byte)| {
                    unit == u16::from(byte.to_ascii_lowercase())
                        || unit == u16::from(byte.to_ascii_uppercase())
                })
        });
        if candidate && test_owned_path_or_unknown(entry.th32ProcessID) {
            return true;
        }
        // SAFETY: 同一有效快照及完整可写 entry，结构和缓冲区在调用期间保持有效。
        if unsafe { Process32NextW(snapshot.0, &raw mut entry) } == 0 {
            // SAFETY: 必须在任何析构关闭句柄前读取本次枚举的线程错误码。
            return unsafe { GetLastError() } != ERROR_NO_MORE_FILES;
        }
    }
}

#[cfg(windows)]
fn test_owned_path_or_unknown(pid: u32) -> bool {
    // SAFETY: 只申请路径查询所需的最小权限，不继承句柄，不涉及外部指针。
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        // 包括快照后进程退出或访问被拒绝：保守进入原有清场，而非推断无目标。
        return true;
    }
    let process = OwnedHandle(process);
    // 常规路径在栈上查询；超出缓冲区同其他查询失败一样保守交回原清场。
    let mut path = [0u16; 260];
    let mut length = 260;
    // SAFETY: 进程句柄有效，path 为 length 个 UTF-16 单元的可写缓冲区；
    // length 是有效的输入/输出参数，标志 0 请求与现有过滤规则一致的 Win32 路径。
    if unsafe { QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &raw mut length) } == 0
    {
        return true;
    }
    let Ok(length) = usize::try_from(length) else {
        return true;
    };
    let Some(path) = path.get(..length).filter(|path| !path.is_empty()) else {
        return true;
    };
    let Ok(path) = String::from_utf16(path) else {
        // 不使用有损转换，避免把无法精确表示的路径误判为非测试所有。
        return true;
    };
    super::is_test_owned_engine_path(&path)
}

/// 非 Windows 没有此原生探测能力，保留原清场路径而不跳过所有权检查。
#[cfg(not(windows))]
pub(super) fn any_test_owned_engine_alive() -> bool {
    true
}
