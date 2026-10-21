//! 鼠标所在显示器的物理像素 GDI 截图与冻结框选（O-17/O-18）。
//! 不写临时图片；全部 GDI 和窗口句柄在本次调用结束时释放。

#![cfg(windows)]

use std::ffi::c_void;
use std::io::Cursor;
use std::mem::size_of;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::Mutex;

mod hide;
mod overlay;

use hide::hide_application_windows;
pub use overlay::{cancel_selection, select, SelectedImage};
#[cfg(test)]
use overlay::{completed_selection, paint_overlay, selection, Overlay};

type Handle = *mut c_void;

/// GDI SelectObject 摘下旧对象的成败判定（S9-10）：失败返回空句柄或
/// HGDI_ERROR(-1)，两种都要判——只判 is_null 会漏 HGDI_ERROR，把失败位图
/// 选入当作成功继续绘制。主截图路径与遮罩路径同口径使用本函数。
fn gdi_replaced_ok(old: Handle) -> bool {
    !old.is_null() && old as isize != -1
}

/// 已登记的主程序完整映像路径（S9-11）：截图期间按完整进程路径比对，只
/// 隐藏属于本安装的主程序窗口；launcher.json 更新（attach-main-exe）时由
/// 服务侧刷新。None 表示未登记——此时不按名字隐藏主程序窗口，宁可少隐藏
/// 也不误隐藏用户另装的实例/同名进程。
static MAIN_EXE_IMAGE: Mutex<Option<String>> = Mutex::new(None);

/// 登记主程序完整路径（服务启动与 attach-main-exe 时调用）。
pub fn register_main_exe(path: Option<&Path>) {
    let mut slot = MAIN_EXE_IMAGE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = path.map(|p| p.to_string_lossy().into_owned());
}

/// 进程映像路径的比对键：去掉 `\\?\` 前缀并统一小写（Windows 路径不区分
/// 大小写；来源分别是 QueryFullProcessImageNameW 与登记的 current_exe，
/// 前缀形态可能不同）。
fn image_key(path: &str) -> String {
    path.trim_start_matches(r"\\?\").to_ascii_lowercase()
}

/// S9-11：候选窗口的进程映像是否属于已登记的本安装主程序（完整路径相等；
/// basename 撞名的其他实例不隐藏）。
fn image_belongs_to_main(image: &str, main_exe: Option<&str>) -> bool {
    main_exe.is_some_and(|main| image_key(image) == image_key(main))
}

/// BGR 交错的 8bit 图像（H×W×3 行主序）：冻结框选的裁剪产物，经内存 PNG
/// 编码交给 Xberg 推理子进程（O-29：字节只驻内存，不落盘）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgrImage {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl BgrImage {
    /// 由现有缓冲构造，校验 `data.len() == width * height * 3`。
    ///
    /// # Errors
    /// 长度不符或尺寸乘积溢出时返回错误。
    pub fn from_vec(width: usize, height: usize, data: Vec<u8>) -> Result<Self, String> {
        let expected = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(3));
        if expected != Some(data.len()) {
            return Err("图像缓冲长度与宽高不符".into());
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    /// 图宽（像素）。
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// 图高（像素）。
    #[must_use]
    pub fn height(&self) -> usize {
        self.height
    }

    /// 编码为 PNG 字节（全内存完成）。
    ///
    /// # Errors
    /// 宽高超出 PNG 上限或编码器失败。
    pub fn png_bytes(&self) -> Result<Vec<u8>, String> {
        let width = u32::try_from(self.width).map_err(|_| "图像宽度超出 PNG 上限".to_string())?;
        let height = u32::try_from(self.height).map_err(|_| "图像高度超出 PNG 上限".to_string())?;
        // BGR → RGB：交换每像素首尾通道，其余字节不动。
        let mut rgb = self.data.clone();
        for pixel in rgb.as_chunks_mut::<3>().0 {
            pixel.swap(0, 2);
        }
        let image = image::RgbImage::from_vec(width, height, rgb)
            .ok_or_else(|| "图像缓冲与尺寸不符".to_string())?;
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|error| format!("截图 PNG 编码失败：{error}"))?;
        Ok(png)
    }
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
}
#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    image_size: u32,
    x_pixels_per_meter: i32,
    y_pixels_per_meter: i32,
    used: u32,
    important: u32,
}
#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}
#[repr(C)]
struct Msg {
    hwnd: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: Point,
    private: u32,
}
#[repr(C)]
struct WndClass {
    style: u32,
    proc: Option<unsafe extern "system" fn(Handle, u32, usize, isize) -> isize>,
    cls_extra: i32,
    wnd_extra: i32,
    instance: Handle,
    icon: Handle,
    cursor: Handle,
    background: Handle,
    menu_name: *const u16,
    class_name: *const u16,
}

#[link(name = "user32")]
extern "system" {
    fn SetProcessDpiAwarenessContext(context: isize) -> i32;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn MonitorFromPoint(point: Point, flags: u32) -> Handle;
    fn GetMonitorInfoW(monitor: Handle, info: *mut MonitorInfo) -> i32;
    fn GetDC(hwnd: Handle) -> Handle;
    fn ReleaseDC(hwnd: Handle, dc: Handle) -> i32;
    fn RegisterClassW(class: *const WndClass) -> u16;
    fn CreateWindowExW(
        ex: u32,
        class: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Handle,
        menu: Handle,
        instance: Handle,
        param: *mut c_void,
    ) -> Handle;
    fn DestroyWindow(hwnd: Handle) -> i32;
    fn ShowWindow(hwnd: Handle, command: i32) -> i32;
    fn SetForegroundWindow(hwnd: Handle) -> i32;
    fn UpdateWindow(hwnd: Handle) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: Handle, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn DefWindowProcW(hwnd: Handle, message: u32, w: usize, l: isize) -> isize;
    fn InvalidateRect(hwnd: Handle, rect: *const Rect, erase: i32) -> i32;
    fn ValidateRect(hwnd: Handle, rect: *const Rect) -> i32;
    fn SetCapture(hwnd: Handle) -> Handle;
    fn ReleaseCapture() -> i32;
    fn LoadCursorW(instance: Handle, name: *const u16) -> Handle;
    fn SetCursor(cursor: Handle) -> Handle;
    fn PostMessageW(hwnd: Handle, message: u32, w: usize, l: isize) -> i32;
    fn EnumWindows(callback: unsafe extern "system" fn(Handle, isize) -> i32, data: isize) -> i32;
    fn GetWindowThreadProcessId(hwnd: Handle, process: *mut u32) -> u32;
    fn IsWindowVisible(hwnd: Handle) -> i32;
    fn IsIconic(hwnd: Handle) -> i32;
    fn IsZoomed(hwnd: Handle) -> i32;
    fn GetWindow(hwnd: Handle, command: u32) -> Handle;
    fn FrameRect(dc: Handle, rect: *const Rect, brush: Handle) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcessId() -> u32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: u32,
        path: *mut u16,
        length: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetModuleHandleW(module: *const u16) -> Handle;
}
#[link(name = "gdi32")]
extern "system" {
    fn CreateCompatibleDC(dc: Handle) -> Handle;
    fn DeleteDC(dc: Handle) -> i32;
    fn CreateCompatibleBitmap(dc: Handle, width: i32, height: i32) -> Handle;
    fn SelectObject(dc: Handle, object: Handle) -> Handle;
    fn DeleteObject(object: Handle) -> i32;
    fn BitBlt(
        dst: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        src: Handle,
        sx: i32,
        sy: i32,
        operation: u32,
    ) -> i32;
    fn GetDIBits(
        dc: Handle,
        bitmap: Handle,
        first: u32,
        count: u32,
        bits: *mut c_void,
        info: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
    fn StretchDIBits(
        dc: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        sx: i32,
        sy: i32,
        sw: i32,
        sh: i32,
        bits: *const c_void,
        info: *const BitmapInfo,
        usage: u32,
        operation: u32,
    ) -> i32;
    fn CreateSolidBrush(color: u32) -> Handle;
    fn PatBlt(dc: Handle, x: i32, y: i32, width: i32, height: i32, operation: u32) -> i32;
    fn SaveDC(dc: Handle) -> i32;
    fn IntersectClipRect(dc: Handle, left: i32, top: i32, right: i32, bottom: i32) -> i32;
    fn RestoreDC(dc: Handle, saved: i32) -> i32;
}
#[link(name = "msimg32")]
extern "system" {
    fn AlphaBlend(
        dst: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        src: Handle,
        sx: i32,
        sy: i32,
        sw: i32,
        sh: i32,
        blend: u32,
    ) -> i32;
}
#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}

const SRCCOPY_CAPTUREBLT: u32 = 0x40cc_0020;
const SRCCOPY: u32 = 0x00cc_0020;
const WS_POPUP: u32 = 0x8000_0000;
const WS_EX_TOPMOST: u32 = 0x0000_0008;
const WM_PAINT: u32 = 0x000f;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_SETCURSOR: u32 = 0x0020;
const WM_KEYDOWN: u32 = 0x0100;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_CLOSE: u32 = 0x0010;
const WM_DONE: u32 = 0x8000 + 18;
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
fn error(step: &str) -> String {
    format!("截图失败：{step}")
}

/// 只拒绝没有完整 BGRA 像素的缓冲；全黑画面本身是合法截图内容。
fn capture_pixels_are_unusable(pixels: &[u8]) -> bool {
    pixels.is_empty() || !pixels.len().is_multiple_of(4)
}

/// 将 Win32 消息循环结果映射为继续、正常退出或截图链路故障。
fn interpret_message_result(code: i32) -> Result<bool, String> {
    match code.cmp(&0) {
        std::cmp::Ordering::Less => Err(error("框选消息循环失败")),
        std::cmp::Ordering::Equal => Ok(false),
        std::cmp::Ordering::Greater => Ok(true),
    }
}

/// 冻结的显示器画面；工作区坐标为物理虚拟桌面像素。
pub struct CaptureFrame {
    pub width: usize,
    pub height: usize,
    pub origin: (i32, i32),
    pub work: (i32, i32, i32, i32),
    bgra: Vec<u8>,
}

/// 设置 PerMonitorV2；必须在创建任何截图相关窗口前执行。
pub fn enable_per_monitor_v2() -> Result<(), String> {
    // SAFETY: 传入预定义常量 DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2。
    let ok = unsafe { SetProcessDpiAwarenessContext(-4) };
    if ok == 0 {
        return Err(error("无法启用逐显示器 DPI 感知"));
    }
    Ok(())
}

/// 从鼠标所在屏幕取得无鼠标指针的物理像素；系统 API 失败时返回捕获错误。
pub fn capture() -> Result<CaptureFrame, String> {
    let mut cursor = Point::default();
    // SAFETY: cursor 是本栈上已初始化的 Point，本函数独占可写。
    if unsafe { GetCursorPos(&raw mut cursor) } == 0 {
        return Err(error("无法定位鼠标"));
    }
    // SAFETY: 只读传入坐标定位显示器；取不到时由空句柄表达。
    let monitor = unsafe { MonitorFromPoint(cursor, 2) };
    if monitor.is_null() {
        return Err(error("无法定位显示器"));
    }
    let mut info = MonitorInfo {
        size: u32::try_from(size_of::<MonitorInfo>())
            .map_err(|_| error("显示器信息结构尺寸无效"))?,
        monitor: Rect::default(),
        work: Rect::default(),
        flags: 0,
    };
    // SAFETY: monitor 刚由 MonitorFromPoint 返回；info 在本栈上且已按 API 要求
    // 把 size 填成结构体实际尺寸，调用期间独占可写。
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) } == 0 {
        return Err(error("无法读取显示器信息"));
    }
    let w = info.monitor.right - info.monitor.left;
    let h = info.monitor.bottom - info.monitor.top;
    if w <= 0 || h <= 0 {
        return Err(error("显示器尺寸无效"));
    }
    // S9-12：工作区非正尺寸（任务栏挤满等异常）在此显式报错，不把非正值
    // 传给结果窗的 scaled_extent（旧实现会钳成 1×1 继续建窗）。
    if info.work.right <= info.work.left || info.work.bottom <= info.work.top {
        return Err(error("显示器工作区尺寸无效"));
    }
    let len = usize::try_from(w)
        .ok()
        .and_then(|w| usize::try_from(h).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| error("显示器尺寸无效"))?;
    let bitmap_info_size =
        u32::try_from(size_of::<BitmapInfoHeader>()).map_err(|_| error("位图信息结构尺寸无效"))?;
    let hidden = hide_application_windows()?;
    // SAFETY: DwmFlush 无参数，仅等待桌面合成呈现一帧，失败由返回值表达。
    if unsafe { DwmFlush() } != 0 {
        return Err(error("桌面合成刷新失败"));
    }
    // SAFETY: 窗口传 null 获取整个虚拟桌面的屏幕 DC，失败由空句柄表达。
    let screen = unsafe { GetDC(null_mut()) };
    if screen.is_null() {
        return Err(error("无法读取桌面"));
    }
    // SAFETY: screen 是刚获取且尚未释放的有效 DC。
    let mem = unsafe { CreateCompatibleDC(screen) };
    let bitmap = if mem.is_null() {
        null_mut()
    } else {
        // SAFETY: 同一有效屏幕 DC；宽高已验证为正，失败由空句柄表达。
        unsafe { CreateCompatibleBitmap(screen, w, h) }
    };
    let old = if bitmap.is_null() {
        null_mut()
    } else {
        // SAFETY: mem 与 bitmap 均为本函数刚创建的有效 GDI 对象；返回值是 DC
        // 原有默认位图，稍后选回。
        unsafe { SelectObject(mem, bitmap) }
    };
    let mut pixels = vec![0u8; len];
    let mut bmi = BitmapInfo {
        header: BitmapInfoHeader {
            size: bitmap_info_size,
            width: w,
            height: -h,
            planes: 1,
            bit_count: 32,
            compression: 0,
            image_size: 0,
            x_pixels_per_meter: 0,
            y_pixels_per_meter: 0,
            used: 0,
            important: 0,
        },
        colors: [0],
    };
    let copied = gdi_replaced_ok(old)
        && unsafe {
            // SAFETY: mem/screen 是本函数创建且尚未释放的 DC；源坐标取自
            // GetMonitorInfoW 返回的显示器矩形，CAPTUREBLT 连分层窗口一并捕获。
            BitBlt(
                mem,
                0,
                0,
                w,
                h,
                screen,
                info.monitor.left,
                info.monitor.top,
                SRCCOPY_CAPTUREBLT,
            )
        } != 0;
    let restored = if gdi_replaced_ok(old) {
        // SAFETY: old 是当初 SelectObject 摘下的 DC 原有默认位图；选回后 bitmap
        // 不再被该 DC 引用，满足下方 GetDIBits 的前置条件。
        unsafe { SelectObject(mem, old) }
    } else {
        null_mut()
    };
    // GetDIBits 要求 bitmap 未选入任何 DC；使用源屏幕 DC 读取已摘下的位图。
    let good = copied && gdi_replaced_ok(restored)
        // SAFETY: screen 有效且 bitmap 已摘出 DC；pixels 容量按 w*h*4 预分配，
        // bmi 已填成自顶向下的 32bit 描述，两者在本栈上独占可写。
        && unsafe {
            GetDIBits(
                screen,
                bitmap,
                0,
                h.cast_unsigned(),
                pixels.as_mut_ptr().cast(),
                &raw mut bmi,
                0,
            )
        } == h;
    // 先销毁内存 DC（即使恢复选中失败也解除位图引用），再销毁位图。
    // SAFETY: mem 为空时短路不调用；非空时是本函数创建且尚未销毁的内存 DC。
    let dc_released = mem.is_null() || unsafe { DeleteDC(mem) } != 0;
    // SAFETY: bitmap 为空时短路不调用；非空时已摘出 DC，删除恰一次。
    let bitmap_released = bitmap.is_null() || unsafe { DeleteObject(bitmap) } != 0;
    // SAFETY: screen 是本函数从 null 窗口获取的屏幕 DC，此处配对归还恰一次。
    let screen_released = unsafe { ReleaseDC(null_mut(), screen) } != 0;
    drop(hidden);
    if !dc_released || !bitmap_released || !screen_released {
        return Err(error("截图资源清理失败"));
    }
    if !good {
        return Err(error("桌面像素读取失败"));
    }
    if capture_pixels_are_unusable(&pixels) {
        return Err(error("桌面像素缓冲无效"));
    }
    Ok(CaptureFrame {
        width: usize::try_from(w).map_err(|_| error("显示器尺寸无效"))?,
        height: usize::try_from(h).map_err(|_| error("显示器尺寸无效"))?,
        origin: (info.monitor.left, info.monitor.top),
        work: (
            info.work.left,
            info.work.top,
            info.work.right - info.work.left,
            info.work.bottom - info.work.top,
        ),
        bgra: pixels,
    })
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
#[path = "capture_win_preview_tests.rs"]
mod preview_tests;

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
