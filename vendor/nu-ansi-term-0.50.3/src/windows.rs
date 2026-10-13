/// Enables ANSI code support on Windows 10.
///
/// This uses Windows API calls to alter the properties of the console that
/// the program is running in.
///
/// https://msdn.microsoft.com/en-us/library/windows/desktop/mt638032(v=vs.85).aspx
///
/// Returns a `Result` with the Windows error code if unsuccessful.
#[cfg(windows)]
pub fn enable_ansi_support() -> Result<(), u32> {
    // ref: https://docs.microsoft.com/en-us/windows/console/console-virtual-terminal-sequences#EXAMPLE_OF_ENABLING_VIRTUAL_TERMINAL_PROCESSING @@ https://archive.is/L7wRJ#76%
    use windows::w;
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};
    use windows::Win32::Storage::FileSystem::{
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_WRITE,
    };

    unsafe {
        // ref: https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
        // Using `CreateFileW("CONOUT$", ...)` to retrieve the console handle works correctly even if STDOUT and/or STDERR are redirected
        let console_handle = CreateFileW(
            w!("CONOUT$"),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_WRITE,
            std::ptr::null_mut(), // SECURITY_ATTRIBUTES
            OPEN_EXISTING,
            0,                    // FILE_FLAGS_AND_ATTRIBUTES
            std::ptr::null_mut(), // hTemplateFile: HANDLE
        );
        if console_handle == INVALID_HANDLE_VALUE {
            return Err(GetLastError());
        }

        enable_ansi_support_for_handle(console_handle)
    }
}

struct OwnedConsoleHandle(windows::Win32::Foundation::HANDLE);

impl Drop for OwnedConsoleHandle {
    fn drop(&mut self) {
        unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

// 调用方须移交有效且由自己创建的句柄；返回前在所有路径释放该句柄。
unsafe fn enable_ansi_support_for_handle(
    console_handle: windows::Win32::Foundation::HANDLE,
) -> Result<(), u32> {
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    use windows::Win32::System::Console::{GetConsoleMode, SetConsoleMode};

    let _owned = OwnedConsoleHandle(console_handle);
    let mut console_mode = 0;
    if 0 == GetConsoleMode(console_handle, &mut console_mode) {
        return Err(GetLastError());
    }

    if console_mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0 {
        if 0 == SetConsoleMode(
            console_handle,
            console_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        ) {
            return Err(GetLastError());
        }
    }

    Ok(())
}

// 此模块仅在 Windows/std 下编译；使用 NUL 句柄，不创建或修改控制台窗口。
#[cfg(test)]
mod tests {
    use windows::w;
    use windows::Win32::Foundation::{
        CloseHandle, GetHandleInformation, GetLastError, ERROR_INVALID_HANDLE,
        INVALID_HANDLE_VALUE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    fn open_nul() -> windows::Win32::Foundation::HANDLE {
        let handle = unsafe {
            CreateFileW(
                w!("NUL"),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        handle
    }

    fn assert_handle_closed(handle: windows::Win32::Foundation::HANDLE) {
        let mut flags = 0;
        let valid = unsafe { GetHandleInformation(handle, &mut flags) };
        let error = unsafe { GetLastError() };
        if valid != 0 {
            // 旧实现反证失败前也释放测试自有句柄，不留下泄漏。
            unsafe { CloseHandle(handle) };
        }
        assert_eq!(valid, 0);
        assert_eq!(error, ERROR_INVALID_HANDLE);
    }

    #[test]
    fn console_mode_error_closes_owned_native_handle() {
        let handle = open_nul();
        let result = unsafe { super::enable_ansi_support_for_handle(handle) };
        assert!(result.is_err());
        assert_handle_closed(handle);
    }

    #[test]
    fn owned_handle_guard_closes_native_handle() {
        let handle = open_nul();
        drop(super::OwnedConsoleHandle(handle));
        assert_handle_closed(handle);
    }
}
