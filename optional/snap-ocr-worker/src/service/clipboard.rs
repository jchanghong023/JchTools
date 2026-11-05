//! OCR 文本与最终 PNG 剪贴板写入、GUI 进程存活探测与退出确认框。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::protocol;

use windows_sys::Win32::{
    Foundation::{GlobalFree, HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard,
            RegisterClipboardFormatW, SetClipboardData,
        },
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        Ole::CF_UNICODETEXT,
    },
};

/// 剪贴板写入需要非空、由本线程拥有的 HWND；窗口不可见且不持有 OCR 正文。
struct ClipboardOwner(HWND);

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
            // SAFETY: 此 message-only 窗口由写入线程创建，独占销毁一次。
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
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, size) };
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
    let copied =
        emptied && !(unsafe { SetClipboardData(u32::from(CF_UNICODETEXT), handle) }).is_null();
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

/// 任务开始时记录；发布前再次核对，避免覆盖用户随后复制的内容。
pub(super) fn sequence_number() -> u32 {
    // SAFETY: 无参数，只查询当前 window station 的剪贴板序列号。
    unsafe { GetClipboardSequenceNumber() }
}

struct PngMemory(HGLOBAL);

impl Drop for PngMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 非空句柄仍由本函数拥有，未成功移交系统。
            unsafe { GlobalFree(self.0) };
        }
    }
}

struct OpenPngClipboard(bool);

impl OpenPngClipboard {
    fn close(&mut self) -> Result<(), String> {
        // SAFETY: 本线程成功打开了剪贴板，关闭后不再访问其数据。
        if unsafe { CloseClipboard() } == 0 {
            return Err("关闭剪贴板失败，未确认复制完成".into());
        }
        self.0 = false;
        Ok(())
    }
}

impl Drop for OpenPngClipboard {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: 本线程仍持有打开的剪贴板，错误路径也必须尝试关闭。
            unsafe { CloseClipboard() };
        }
    }
}

/// 只发布图像模块最终返回的 PNG，不发布原图或中间位图。
/// 最多打开 5 次，相邻失败尝试间隔 20 ms；调用方必须在后台线程执行。
pub(super) fn copy_png(
    png: &[u8],
    expected_sequence: u32,
    valid: &Mutex<bool>,
) -> Result<(), String> {
    const ATTEMPTS: usize = 5;
    const RETRY_DELAY: Duration = Duration::from_millis(20);
    const PNG_FORMAT: [u16; 4] = [80, 78, 71, 0];

    // SAFETY: PNG_FORMAT 为静态、NUL 终止的 Windows 格式名。
    let format = unsafe { RegisterClipboardFormatW(PNG_FORMAT.as_ptr()) };
    if format == 0 {
        return Err("PNG 剪贴板格式注册失败".into());
    }
    // SAFETY: 只为已编码的最终字节分配可移动全局内存。
    let memory = PngMemory(unsafe { GlobalAlloc(GMEM_MOVEABLE, png.len()) });
    if memory.0.is_null() {
        return Err("图片剪贴板内存分配失败".into());
    }
    // SAFETY: memory 持有刚分配的句柄，锁定失败由 Drop 释放。
    let data = unsafe { GlobalLock(memory.0) };
    if data.is_null() {
        return Err("图片剪贴板内存访问失败".into());
    }
    // SAFETY: 目标容量为 png.len()，源切片在复制期间有效且不重叠。
    unsafe { std::ptr::copy_nonoverlapping(png.as_ptr(), data.cast::<u8>(), png.len()) };
    // SAFETY: 解锁刚锁定的句柄；之后不再使用 data。
    unsafe { GlobalUnlock(memory.0) };
    let mut memory = memory;
    let owner = ClipboardOwner::new()?;

    for attempt in 0..ATTEMPTS {
        {
            let current = valid
                .lock()
                .map_err(|_| "图片复制任务已失效，未复制".to_string())?;
            if !*current {
                return Err("图片复制任务已取消或失效，未复制".into());
            }
            if sequence_number() != expected_sequence {
                return Err("剪贴板已有新内容，本次截图已失效，未复制".into());
            }
        }
        // SAFETY: owner 是本写入线程创建的不可见窗口；失败时尚未修改剪贴板。
        if unsafe { OpenClipboard(owner.0) } == 0 {
            if attempt + 1 < ATTEMPTS {
                std::thread::sleep(RETRY_DELAY);
            }
            continue;
        }
        let mut opened = OpenPngClipboard(true);
        // 与取消/失效共用同一把锁，直到最终发布结束；窗口打开后再次检查序列号，
        // 此时其他线程/进程不能在检查与 EmptyClipboard 之间插入新内容。
        let current = valid
            .lock()
            .map_err(|_| "图片复制任务已失效，未复制".to_string())?;
        if !*current {
            return Err("图片复制任务已取消或失效，未复制".into());
        }
        if sequence_number() != expected_sequence {
            return Err("剪贴板已有新内容，本次截图已失效，未复制".into());
        }
        // SAFETY: 本线程已打开剪贴板且持有任务有效性锁。
        if unsafe { EmptyClipboard() } == 0 {
            return Err("清空剪贴板失败，图片未复制".into());
        }
        // SAFETY: 内存已填好并解锁；仅成功时系统取得句柄所有权。
        if unsafe { SetClipboardData(format, memory.0) }.is_null() {
            return Err("PNG 写入剪贴板失败，图片未复制".into());
        }
        memory.0 = std::ptr::null_mut();
        opened.close()?;
        return Ok(());
    }
    Err("剪贴板被其他程序占用，已尝试 5 次，图片未复制".into())
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
