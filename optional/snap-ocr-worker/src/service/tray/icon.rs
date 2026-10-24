//! 内嵌品牌托盘图标：对齐的 PNG 资源加载为 HICON，并按托盘线程缓存复用。

use std::ptr::null_mut;

use super::win32::Handle;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, DestroyIcon, GetSystemMetrics, SM_CXSMICON, SM_CYSMICON,
};

// CreateIconFromResourceEx 要求资源起点按 DWORD 对齐；图像直接编入 worker，不依赖外部文件。
#[repr(align(4))]
struct IconPng([u8; include_bytes!("../../../../../resources/app-icon.png").len()]);
static ICON_PNG: IconPng = IconPng(*include_bytes!("../../../../../resources/app-icon.png"));

struct TrayIcon(Handle);
impl Drop for TrayIcon {
    fn drop(&mut self) {
        // SAFETY: 此句柄仅由 CreateIconFromResourceEx 创建，归当前托盘线程独占。
        if unsafe { DestroyIcon(self.0) } == 0 {
            tracing::warn!(event = "ocr_tray_icon_release_failed", error = %std::io::Error::last_os_error(), "释放托盘品牌图标失败");
        }
    }
}

fn load_notification_icon() -> Option<TrayIcon> {
    let size = u32::try_from(ICON_PNG.0.len()).ok()?;
    // SAFETY: 只读取 Windows 通知区使用的小图标尺寸，不修改 DPI 或系统设置。
    let width = unsafe { GetSystemMetrics(SM_CXSMICON) };
    // SAFETY: 同上，仅查询系统度量。
    let height = unsafe { GetSystemMetrics(SM_CYSMICON) };
    // SAFETY: 对齐的内置 PNG 在进程存续期间有效，API 仅读取资源字节；
    // 0x30000 为 Win32 图标资源格式，flags=0 使返回句柄由本线程负责释放。
    let icon = unsafe {
        CreateIconFromResourceEx(
            ICON_PNG.0.as_ptr().cast_mut(),
            size,
            1,
            0x0003_0000,
            width,
            height,
            0,
        )
    };
    if icon.is_null() {
        tracing::error!(event = "ocr_tray_icon_load_failed", error = %std::io::Error::last_os_error(), "内置 JchTools 托盘图标加载失败");
        None
    } else {
        Some(TrayIcon(icon))
    }
}

thread_local! {
    static NOTIFICATION_ICON: Option<TrayIcon> = load_notification_icon();
}

/// 图标按托盘线程缓存一次，复用注册、通知和 Explorer 重建；线程结束时释放句柄。
pub(super) fn notification_icon() -> Handle {
    NOTIFICATION_ICON.with(|icon| icon.as_ref().map_or(null_mut(), |icon| icon.0))
}
