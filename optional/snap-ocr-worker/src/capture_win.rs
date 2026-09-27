//! 鼠标所在显示器的物理像素 GDI 截图与冻结框选（O-17/O-18）。
//! 不写临时图片；全部 GDI 和窗口句柄在本次调用结束时释放。

#![cfg(windows)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Cursor;
use std::mem::size_of;
use std::ptr::{null, null_mut};

type Handle = *mut c_void;

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
    /// 长度不符时返回错误。
    pub fn from_vec(width: usize, height: usize, data: Vec<u8>) -> Result<Self, String> {
        if data.len() != width * height * 3 {
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
/// 暂时隐藏本应用的可见顶层窗；截图完成后按原有最大化状态恢复。
struct HiddenWindows(Vec<(usize, bool)>);
impl Drop for HiddenWindows {
    fn drop(&mut self) {
        for &(hwnd, maximized) in &self.0 {
            unsafe {
                ShowWindow(hwnd as Handle, if maximized { 3 } else { 5 });
            }
        }
    }
}
unsafe extern "system" fn hide_own_window(hwnd: Handle, data: isize) -> i32 {
    if IsWindowVisible(hwnd) == 0 || !GetWindow(hwnd, 4).is_null() {
        return 1;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, &raw mut pid);
    if pid == 0 {
        return 1;
    }
    let own = pid == GetCurrentProcessId();
    let named = if own {
        true
    } else {
        let process = OpenProcess(0x1000, 0, pid);
        if process.is_null() {
            false
        } else {
            let mut path = vec![0u16; 32_768];
            let mut count = 32_768u32;
            let got =
                QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &raw mut count) != 0;
            CloseHandle(process);
            got && String::from_utf16_lossy(&path[..count as usize])
                .rsplit(['\\', '/'])
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("JchTools.exe"))
        }
    };
    if named {
        let hidden = &mut *(data as *mut HiddenWindows);
        hidden.0.push((hwnd as usize, IsZoomed(hwnd) != 0));
        ShowWindow(hwnd, 0);
    }
    1
}
fn hide_application_windows() -> Result<HiddenWindows, String> {
    let mut windows = HiddenWindows(Vec::new());
    // SAFETY: 回调同步运行，data 指向本栈上仍然有效的 Vec；Drop 恢复窗口。
    if unsafe {
        EnumWindows(
            hide_own_window,
            (&raw mut windows).cast::<HiddenWindows>() as isize,
        )
    } == 0
    {
        return Err(error("无法隐藏程序窗口"));
    }
    Ok(windows)
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

/// 从鼠标所在屏幕取得无鼠标指针的物理像素；全黑表面视为不可捕获。
pub fn capture() -> Result<CaptureFrame, String> {
    let mut cursor = Point::default();
    // SAFETY: cursor/info 是有效的可写结构体；GDI 句柄仅在此函数中使用。
    unsafe {
        if GetCursorPos(&raw mut cursor) == 0 {
            return Err(error("无法定位鼠标"));
        }
        let monitor = MonitorFromPoint(cursor, 2);
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
        if GetMonitorInfoW(monitor, &raw mut info) == 0 {
            return Err(error("无法读取显示器信息"));
        }
        let w = info.monitor.right - info.monitor.left;
        let h = info.monitor.bottom - info.monitor.top;
        if w <= 0 || h <= 0 {
            return Err(error("显示器尺寸无效"));
        }
        let len = usize::try_from(w)
            .ok()
            .and_then(|w| usize::try_from(h).ok().and_then(|h| w.checked_mul(h)))
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| error("显示器尺寸无效"))?;
        let bitmap_info_size = u32::try_from(size_of::<BitmapInfoHeader>())
            .map_err(|_| error("位图信息结构尺寸无效"))?;
        let hidden = hide_application_windows()?;
        if DwmFlush() != 0 {
            return Err(error("桌面合成刷新失败"));
        }
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return Err(error("无法读取桌面"));
        }
        let mem = CreateCompatibleDC(screen);
        let bitmap = if mem.is_null() {
            null_mut()
        } else {
            CreateCompatibleBitmap(screen, w, h)
        };
        let old = if bitmap.is_null() {
            null_mut()
        } else {
            SelectObject(mem, bitmap)
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
        let copied = !old.is_null()
            && old as isize != -1
            && BitBlt(
                mem,
                0,
                0,
                w,
                h,
                screen,
                info.monitor.left,
                info.monitor.top,
                SRCCOPY_CAPTUREBLT,
            ) != 0;
        let restored = if !old.is_null() && old as isize != -1 {
            SelectObject(mem, old)
        } else {
            null_mut()
        };
        // GetDIBits 要求 bitmap 未选入任何 DC；使用源屏幕 DC 读取已摘下的位图。
        let good = copied
            && !restored.is_null()
            && restored as isize != -1
            && GetDIBits(
                screen,
                bitmap,
                0,
                h.cast_unsigned(),
                pixels.as_mut_ptr().cast(),
                &raw mut bmi,
                0,
            ) == h;
        // 先销毁内存 DC（即使恢复选中失败也解除位图引用），再销毁位图。
        let dc_released = mem.is_null() || DeleteDC(mem) != 0;
        let bitmap_released = bitmap.is_null() || DeleteObject(bitmap) != 0;
        let screen_released = ReleaseDC(null_mut(), screen) != 0;
        drop(hidden);
        if !dc_released || !bitmap_released || !screen_released {
            return Err(error("截图资源清理失败"));
        }
        if !good {
            return Err(error("桌面像素读取失败"));
        }
        if pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0)
        {
            return Err(error("当前安全桌面或受保护表面无法捕获"));
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
}

struct Overlay {
    frame: CaptureFrame,
    start: Option<Point>,
    end: Point,
    selected: Option<Rect>,
    shade_dc: Handle,
    shade_bitmap: Handle,
    shade_old: Handle,
}
impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.shade_dc, self.shade_old);
            DeleteObject(self.shade_bitmap);
            DeleteDC(self.shade_dc);
        }
    }
}
thread_local! { static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) }; }
fn point_from_lparam(l: isize) -> Point {
    // Win32 mouse lParam stores signed 16-bit x/y coordinates in its low two words.
    let bytes = l.to_le_bytes();
    Point {
        x: i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        y: i32::from(i16::from_le_bytes([bytes[2], bytes[3]])),
    }
}
fn selection(a: Point, b: Point) -> Rect {
    Rect {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

unsafe extern "system" fn overlay_proc(hwnd: Handle, msg: u32, w: usize, l: isize) -> isize {
    match msg {
        WM_ERASEBKGND => return 1,
        WM_SETCURSOR => {
            SetCursor(LoadCursorW(null_mut(), 32515usize as *const u16));
            return 1;
        }
        WM_PAINT => {
            OVERLAY.with(|slot| {
                if let Some(state) = slot.borrow().as_ref() {
                    let (Ok(width), Ok(height), Ok(header_size)) = (
                        i32::try_from(state.frame.width),
                        i32::try_from(state.frame.height),
                        u32::try_from(size_of::<BitmapInfoHeader>()),
                    ) else {
                        return;
                    };
                    let dc = GetDC(hwnd);
                    if !dc.is_null() {
                        let info = BitmapInfo {
                            header: BitmapInfoHeader {
                                size: header_size,
                                width,
                                height: -height,
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
                        StretchDIBits(
                            dc,
                            0,
                            0,
                            width,
                            height,
                            0,
                            0,
                            width,
                            height,
                            state.frame.bgra.as_ptr().cast(),
                            &raw const info,
                            0,
                            SRCCOPY,
                        );
                        // 暗色蒙层叠加在冻结像素上，选区内部重绘原像素：
                        // 桌面后续变化不会透过遮罩进入捕获结果。
                        AlphaBlend(
                            dc,
                            0,
                            0,
                            width,
                            height,
                            state.shade_dc,
                            0,
                            0,
                            1,
                            1,
                            0x0050_0000,
                        );
                        if let Some(start) = state.start {
                            let outline = selection(start, state.end);
                            if outline.right > outline.left && outline.bottom > outline.top {
                                StretchDIBits(
                                    dc,
                                    outline.left,
                                    outline.top,
                                    outline.right - outline.left,
                                    outline.bottom - outline.top,
                                    outline.left,
                                    outline.top,
                                    outline.right - outline.left,
                                    outline.bottom - outline.top,
                                    state.frame.bgra.as_ptr().cast(),
                                    &raw const info,
                                    0,
                                    SRCCOPY,
                                );
                            }
                            let brush = CreateSolidBrush(0x0000_ffff);
                            if !brush.is_null() {
                                FrameRect(dc, &raw const outline, brush);
                                DeleteObject(brush);
                            }
                        }
                        ReleaseDC(hwnd, dc);
                    }
                }
            });
            ValidateRect(hwnd, null());
            return 0;
        }
        WM_LBUTTONDOWN => {
            OVERLAY.with(|s| {
                if let Some(o) = s.borrow_mut().as_mut() {
                    let p = point_from_lparam(l);
                    o.start = Some(p);
                    o.end = p;
                }
            });
            SetCapture(hwnd);
            return 0;
        }
        WM_MOUSEMOVE => {
            OVERLAY.with(|s| {
                if let Some(o) = s.borrow_mut().as_mut() {
                    if o.start.is_some() {
                        o.end = point_from_lparam(l);
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
            });
            return 0;
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            OVERLAY.with(|s| {
                if let Some(o) = s.borrow_mut().as_mut() {
                    if let Some(start) = o.start {
                        o.selected = Some(selection(start, point_from_lparam(l)));
                    }
                }
            });
            PostMessageW(hwnd, WM_DONE, 0, 0);
            return 0;
        }
        WM_KEYDOWN if w == 0x1b => {
            PostMessageW(hwnd, WM_DONE, 0, 0);
            return 0;
        }
        WM_RBUTTONDOWN | WM_CLOSE => {
            PostMessageW(hwnd, WM_DONE, 0, 0);
            return 0;
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// 展示冻结全屏框选；用户取消或任一边小于八像素返回 None。
/// 返回裁剪后的 BGR 与选区所属显示器工作区。
pub type SelectedImage = Option<(BgrImage, (i32, i32, i32, i32))>;
pub fn select(frame: CaptureFrame) -> Result<SelectedImage, String> {
    let frame_width = i32::try_from(frame.width).map_err(|_| error("框选宽度无效"))?;
    let frame_height = i32::try_from(frame.height).map_err(|_| error("框选高度无效"))?;
    let origin = frame.origin;
    let class = wide("JchSnapOcrFrozenSelection");
    let caption = wide("截图 OCR 框选");
    let instance = unsafe { GetModuleHandleW(null()) };
    let wnd = WndClass {
        style: 0,
        proc: Some(overlay_proc),
        cls_extra: 0,
        wnd_extra: 0,
        instance,
        icon: null_mut(),
        cursor: null_mut(),
        background: null_mut(),
        menu_name: null(),
        class_name: class.as_ptr(),
    };
    let (shade_dc, shade_bitmap, shade_old) = unsafe {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return Err(error("无法创建框选遮罩"));
        }
        let shade_dc = CreateCompatibleDC(screen);
        let shade_bitmap = if shade_dc.is_null() {
            null_mut()
        } else {
            CreateCompatibleBitmap(screen, 1, 1)
        };
        let shade_old = if shade_bitmap.is_null() {
            null_mut()
        } else {
            SelectObject(shade_dc, shade_bitmap)
        };
        if !shade_old.is_null() {
            PatBlt(shade_dc, 0, 0, 1, 1, 0x0000_0042);
        }
        ReleaseDC(null_mut(), screen);
        if shade_old.is_null() {
            if !shade_bitmap.is_null() {
                DeleteObject(shade_bitmap);
            }
            if !shade_dc.is_null() {
                DeleteDC(shade_dc);
            }
            return Err(error("无法创建框选遮罩"));
        }
        (shade_dc, shade_bitmap, shade_old)
    };
    OVERLAY.with(|slot| {
        *slot.borrow_mut() = Some(Overlay {
            frame,
            start: None,
            end: Point::default(),
            selected: None,
            shade_dc,
            shade_bitmap,
            shade_old,
        });
    });
    // SAFETY: 窗口过程与线程局部 Overlay 同线程；窗口销毁后才释放图像。
    unsafe {
        RegisterClassW(&raw const wnd);
        let (x, y, w, h) = (origin.0, origin.1, frame_width, frame_height);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            class.as_ptr(),
            caption.as_ptr(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            instance,
            null_mut(),
        );
        if hwnd.is_null() {
            OVERLAY.with(|s| {
                s.borrow_mut().take();
            });
            return Err(error("无法创建冻结框选窗口"));
        }
        ShowWindow(hwnd, 5);
        SetForegroundWindow(hwnd);
        UpdateWindow(hwnd);
        loop {
            let mut msg = std::mem::zeroed::<Msg>();
            let code = GetMessageW(&raw mut msg, null_mut(), 0, 0);
            if code <= 0 {
                break;
            }
            if msg.hwnd == hwnd && msg.message == WM_DONE {
                break;
            }
            TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
        DestroyWindow(hwnd);
    }
    let Some(state) = OVERLAY.with(|slot| slot.borrow_mut().take()) else {
        return Err(error("框选状态丢失"));
    };
    let Some(rect) = state.selected else {
        return Ok(None);
    };
    // Dimensions were checked before the selection overlay was created.
    let x0 = usize::try_from(rect.left.clamp(0, frame_width)).map_err(|_| error("框选位置无效"))?;
    let y0 = usize::try_from(rect.top.clamp(0, frame_height)).map_err(|_| error("框选位置无效"))?;
    let x1 =
        usize::try_from(rect.right.clamp(0, frame_width)).map_err(|_| error("框选位置无效"))?;
    let y1 =
        usize::try_from(rect.bottom.clamp(0, frame_height)).map_err(|_| error("框选位置无效"))?;
    if x1.saturating_sub(x0) < 8 || y1.saturating_sub(y0) < 8 {
        return Ok(None);
    }
    let width = x1 - x0;
    let height = y1 - y0;
    let mut bgr = Vec::with_capacity(width * height * 3);
    for y in y0..y1 {
        let start = (y * state.frame.width + x0) * 4;
        for pixel in state.frame.bgra[start..start + width * 4]
            .as_chunks::<4>()
            .0
        {
            bgr.extend_from_slice(&pixel[..3]);
        }
    }
    BgrImage::from_vec(width, height, bgr)
        .map(|image| Some((image, state.frame.work)))
        .map_err(|_| error("裁剪图像无效"))
}
