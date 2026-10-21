//! OCR 结果剪贴板写入（message-only owner 窗口）、GUI 进程存活探测与退出确认框。
//! 从 `service.rs` 按内聚职责原样搬移；行为与注释保持不变。

use std::time::Instant;

use super::protocol;

#[link(name = "user32")]
extern "system" {
    pub(super) fn OpenClipboard(hwnd: *mut std::ffi::c_void) -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, handle: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    pub(super) fn CloseClipboard() -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GlobalAlloc(flags: u32, size: usize) -> *mut std::ffi::c_void;
    pub(super) fn GlobalLock(handle: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    pub(super) fn GlobalUnlock(handle: *mut std::ffi::c_void) -> i32;
    fn GlobalFree(handle: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

/// 剪贴板写入需要非空、由本线程拥有的 HWND；窗口不可见且不持有 OCR 正文。
struct ClipboardOwner(*mut std::ffi::c_void);

impl ClipboardOwner {
    fn new() -> Result<Self, String> {
        const CLASS: [u16; 7] = [83, 84, 65, 84, 73, 67, 0]; // STATIC
        let started = Instant::now();
        // SAFETY: 系统类名带 NUL；message-only 窗口无正文、不抢焦点，由 owner 释放。
        let owner = Self(unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::CreateWindowExW(
                0,
                CLASS.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                windows_sys::Win32::UI::WindowsAndMessaging::HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        });
        if owner.0.is_null() {
            tracing::error!(
                component = "snap_ocr",
                event = "ocr_copy_failed",
                stage = "create_clipboard_owner",
                error_type = "window_creation",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "剪贴板所有者窗口创建失败"
            );
            return Err("剪贴板窗口创建失败".into());
        }
        Ok(owner)
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 此 message-only 窗口由 copy_text 在本线程创建，独占销毁一次。
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0) };
        }
    }
}
pub(super) fn copy_text(text: &str) -> Result<(), String> {
    let started = Instant::now();
    tracing::debug!(
        component = "snap_ocr",
        event = "ocr_copy_started",
        text_bytes = text.len(),
        "开始复制 OCR 结果到剪贴板"
    );
    let wide = protocol::wide(text);
    let size = wide
        .len()
        .checked_mul(2)
        .ok_or_else(|| "文本过长无法复制".to_string())?;
    // SAFETY: GMEM_MOVEABLE(2) 按字节长度分配；失败返回 null，由本函数检查。
    let handle = unsafe { GlobalAlloc(2, size) };
    if handle.is_null() {
        tracing::error!(
            component = "snap_ocr",
            event = "ocr_copy_failed",
            stage = "allocate",
            error_type = "global_alloc",
            elapsed_ms = crate::logging::elapsed_ms(started),
            "OCR 结果剪贴板内存分配失败"
        );
        return Err("剪贴板内存分配失败".into());
    }
    // SAFETY: handle 为刚分配的可移动内存句柄；锁定失败返回 null。
    let data = unsafe { GlobalLock(handle) };
    if data.is_null() {
        // SAFETY: 分配成功但锁定失败，所有权仍在手，由本函数释放。
        unsafe { GlobalFree(handle) };
        tracing::error!(
            component = "snap_ocr",
            event = "ocr_copy_failed",
            stage = "lock",
            error_type = "global_lock",
            elapsed_ms = crate::logging::elapsed_ms(started),
            "OCR 结果剪贴板内存访问失败"
        );
        return Err("剪贴板内存访问失败".into());
    }
    // SAFETY: 源为存活宽字符串切片，目标为锁定内存且按 u16 计数不越界。
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), data.cast::<u16>(), wide.len()) };
    // SAFETY: 解锁刚锁定的句柄；解锁后 data 指针不再使用。
    unsafe { GlobalUnlock(handle) };
    // OpenClipboard(NULL) 后 EmptyClipboard 会把 owner 设为 NULL，导致
    // SetClipboardData 失败。使用本线程的不可见 message-only 窗口，不抢焦点。
    let owner = match ClipboardOwner::new() {
        Ok(owner) => owner,
        Err(error) => {
            // SAFETY: 尚未移交剪贴板，释放本函数分配的内存。
            unsafe { GlobalFree(handle) };
            return Err(error);
        }
    };
    // SAFETY: owner 为本线程创建的有效窗口；失败时内存仍由本函数释放。
    if unsafe { OpenClipboard(owner.0) } == 0 {
        // SAFETY: 尚未交给剪贴板的所有权重归本函数。
        unsafe { GlobalFree(handle) };
        tracing::error!(
            component = "snap_ocr",
            event = "ocr_copy_failed",
            stage = "open_clipboard",
            error_type = "unavailable",
            elapsed_ms = crate::logging::elapsed_ms(started),
            "无法打开剪贴板"
        );
        return Err("剪贴板被其他程序占用".into());
    }
    // SAFETY: 清空剪贴板为设置数据的前置步骤。
    let emptied = unsafe { EmptyClipboard() != 0 };
    // SAFETY: 句柄数据已填好；成功后所有权移交系统，失败时由本函数释放。
    let copied = emptied && !(unsafe { SetClipboardData(13, handle) }).is_null();
    // SAFETY: 关闭剪贴板结束本线程占用。
    unsafe { CloseClipboard() };
    if !copied {
        // SAFETY: 设置失败时系统未接管句柄，由本函数释放。
        unsafe { GlobalFree(handle) };
        tracing::error!(
            component = "snap_ocr",
            event = "ocr_copy_failed",
            stage = "write",
            error_type = "clipboard_write",
            elapsed_ms = crate::logging::elapsed_ms(started),
            "OCR 结果写入剪贴板失败"
        );
        return Err("复制到剪贴板失败".into());
    }
    tracing::debug!(
        component = "snap_ocr",
        event = "ocr_copy_completed",
        result = "copied",
        text_bytes = text.len(),
        elapsed_ms = crate::logging::elapsed_ms(started),
        "OCR 结果已复制到剪贴板"
    );
    Ok(())
}

pub(super) fn gui_process_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    // SAFETY: 只请求受限查询权限；pid 来自共享配置，句柄仅在本函数内使用并关闭。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut code = 0;
    // SAFETY: 句柄刚打开且仅查询退出码；code 为本栈出参。
    let alive = unsafe { GetExitCodeProcess(handle, &raw mut code) } != 0 && code == 259;
    // SAFETY: 查询完成即关闭句柄，此后不再使用。
    unsafe { CloseHandle(handle) };
    alive
}
pub(super) fn confirm_background_exit() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_YESNO,
    };
    let text = protocol::wide(
        "退出后台服务将停止当前任务、截图及 Xberg。正在处理的内容会在安全边界结束。确定退出？",
    );
    let title = protocol::wide("退出 JchTools 后台服务");
    // SAFETY: 同步系统确认框，宽字符串在调用期间有效。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES
    }
}
