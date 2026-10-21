//! 窗口定位与系统主题的 Win32 支撑：光标位置、居中与深浅色读取（自 gui.rs 拆分）。
/// 光标的屏幕坐标（物理像素）。拖动必须基于屏幕坐标：窗口自身移动不会改变它，因此不会出现
/// “移动窗口 → 局部坐标回跳 → 窗口被拉回去”的反馈抖动。
#[cfg(windows)]
pub(super) fn pointer_position() -> Option<(f64, f64)> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: point 是有效的栈上 POINT；调用只向它写入光标坐标。
    (unsafe { GetCursorPos(&raw mut point) } != 0)
        .then_some((f64::from(point.x), f64::from(point.y)))
}

#[cfg(not(windows))]
pub(super) fn pointer_position() -> Option<(f64, f64)> {
    None
}

/// 监视器 API 在 windows-sys 的 `Win32_Graphics_Gdi` 特性下，本 crate 未启用该特性，
/// 因此在 gui 内声明最小 FFI（user32），避免改 Cargo.toml。
#[cfg(windows)]
mod win32_monitor {
    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    /// MONITORINFO：字段名仅为 Rust 侧标识，ABI 布局由 `repr(C)` 的字段顺序与类型决定，
    /// 因此按 Rust 命名惯例写；Win32 的 `cbSize`/`rcMonitor`/`rcWork`/`dwFlags` 语义见各字段注释。
    #[repr(C)]
    pub(super) struct MonitorInfo {
        /// cbSize：调用方必须填入结构体字节数
        pub cb_size: u32,
        /// rcMonitor：监视器完整矩形
        pub rc_monitor: RECT,
        /// rcWork：监视器工作区矩形（排除任务栏）
        pub rc_work: RECT,
        /// dwFlags：MONITORINFOF_* 标志
        pub dw_flags: u32,
    }
    pub(super) type HMonitor = *mut core::ffi::c_void;
    /// MONITOR_DEFAULTTONEAREST：取包含点/窗口的最近监视器
    pub(super) const MONITOR_DEFAULTTONEAREST: u32 = 2;
    extern "system" {
        pub(super) fn MonitorFromWindow(hwnd: HWND, dw_flags: u32) -> HMonitor;
        pub(super) fn MonitorFromPoint(pt: POINT, dw_flags: u32) -> HMonitor;
        pub(super) fn GetMonitorInfoW(hmonitor: HMonitor, lpmi: *mut MonitorInfo) -> i32;
    }
}

/// 启动时把窗口居中：基于当前/光标所在监视器的工作区计算对称留白，
/// 满足 AGENTS「主窗口启动时居中显示在当前显示器中央」（多显示器下不固定主屏）。
#[cfg(windows)]
pub(super) fn center_window(window: &slint::Window) {
    use win32_monitor::{
        GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MonitorInfo, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow};
    // 优先当前前台窗口所在监视器；没有前台窗口时退到光标所在监视器。
    // SAFETY: GetForegroundWindow 只是查询前台窗口句柄，不产生所有权或别名约束。
    let foreground = unsafe { GetForegroundWindow() };
    let monitor = if foreground.is_null() {
        let mut point = POINT { x: 0, y: 0 };
        // SAFETY: point 是有效的栈上 POINT；调用只向它写入光标坐标。
        if unsafe { GetCursorPos(&raw mut point) } == 0 {
            return;
        }
        // SAFETY: point 由 GetCursorPos 刚写入；MONITOR_DEFAULTTONEAREST 只读该值。
        unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) }
    } else {
        // SAFETY: foreground 非空（上方已判空）；调用只读取该句柄。
        unsafe { MonitorFromWindow(foreground, MONITOR_DEFAULTTONEAREST) }
    };
    if monitor.is_null() {
        return;
    }
    // Win32 ABI 要求的 cbSize：结构体仅数十字节，饱和兜底不可能触发。
    let cb_size = u32::try_from(std::mem::size_of::<MonitorInfo>()).unwrap_or(u32::MAX);
    let mut info = MonitorInfo {
        cb_size,
        rc_monitor: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rc_work: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dw_flags: 0,
    };
    // 兼容部分声明布局：cb_size 必须正确
    // SAFETY: monitor 是刚取得的有效 HMONITOR；info.cb_size 已按 ABI 要求填好，
    // 调用只向 info 写入监视器信息。
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) } == 0 {
        return;
    }
    let work = info.rc_work;
    let size = window.size();
    // 窗口尺寸远小于 i32 上限；饱和转换仅防御性兜底（显示用途）。
    let width = i32::try_from(size.width).unwrap_or(i32::MAX);
    let height = i32::try_from(size.height).unwrap_or(i32::MAX);
    // 在监视器工作区内居中，并夹回工作区，避免压住任务栏或跑到屏幕外
    let x = work.left + ((work.right - work.left) - width) / 2;
    let y = work.top + ((work.bottom - work.top) - height) / 2;
    let x = x.clamp(work.left, (work.right - width).max(work.left));
    let y = y.clamp(work.top, (work.bottom - height).max(work.top));
    window.set_position(slint::PhysicalPosition::new(x, y));
}

#[cfg(not(windows))]
pub(super) fn center_window(_window: &slint::Window) {}

/// 读取系统“应用使用浅色主题”设置，供自绘配色在“跟随系统”时保持一致。
#[cfg(windows)]
pub(super) fn system_dark() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name: Vec<u16> = "AppsUseLightTheme"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut value: u32 = 1;
    // DWORD 缓冲区大小恒为 4，饱和兜底不可能触发。
    let mut size = u32::try_from(std::mem::size_of::<u32>()).unwrap_or(u32::MAX);
    // SAFETY: path/name 都是以 NUL 结尾的 UTF-16 缓冲区；value/size 是配套的
    // DWORD 输出缓冲区（RRF_RT_REG_DWORD 要求大小恰为 4），调用期间指针均有效。
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut value).cast::<core::ffi::c_void>(),
            &raw mut size,
        )
    };
    status == 0 && value == 0
}

#[cfg(not(windows))]
pub(super) fn system_dark() -> bool {
    false
}
