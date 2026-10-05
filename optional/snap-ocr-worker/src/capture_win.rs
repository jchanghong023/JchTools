//! 鼠标所在显示器的物理像素 GDI 截图与冻结框选（O-17/O-18）。
//! 不写临时图片；全部 GDI 和窗口句柄在本次调用结束时释放。

#![cfg(windows)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Cursor;
use std::mem::size_of;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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
/// 暂时隐藏本应用的可见顶层窗；截图完成后按隐藏前的窗口状态恢复。
///
/// 恢复语义（行为修复）：隐藏前最小化的窗口必须以 `SW_SHOWMINNOACTIVE` 恢复
/// ——保持最小化且不抢焦点；旧实现一律 `SW_SHOW`/`SW_MAXIMIZE`，会把用户
/// 截图前最小化的窗口强行还原成正常可见并激活，最小化状态丢失。列表里只会
/// 出现无归属（`GW_OWNER` 为空）的顶层窗（`hide_own_window` 过滤掉可见性
/// 不符与有归属的工具/对话框窗口），三种恢复命令对它们语义完备。
#[derive(Clone, Copy)]
enum RestoreState {
    /// 隐藏前最小化（`IsIconic`）。
    Minimized,
    /// 隐藏前最大化（`IsZoomed`）。
    Maximized,
    /// 隐藏前普通可见。
    Normal,
}

/// 由窗口当前状态判定恢复命令（隐藏期间用户无法操作该窗口，状态稳定）。
fn restore_state(hwnd: Handle) -> RestoreState {
    // SAFETY: 只读查询最小化状态；句柄来自 EnumWindows 回调，回调期间有效。
    let minimized = unsafe { IsIconic(hwnd) } != 0;
    if minimized {
        RestoreState::Minimized
    } else {
        // SAFETY: 同一回调句柄，回调期间有效；仅在未最小化分支执行只读的最大化查询。
        let maximized = unsafe { IsZoomed(hwnd) } != 0;
        if maximized {
            RestoreState::Maximized
        } else {
            RestoreState::Normal
        }
    }
}

/// 恢复命令（ShowWindow）：SW_SHOWMINNOACTIVE / SW_MAXIMIZE / SW_SHOW。
fn restore_command(state: RestoreState) -> i32 {
    match state {
        RestoreState::Minimized => 7,
        RestoreState::Maximized => 3,
        RestoreState::Normal => 5,
    }
}

struct HiddenWindows(Vec<(usize, RestoreState)>);
impl Drop for HiddenWindows {
    fn drop(&mut self) {
        for &(hwnd, state) in &self.0 {
            // SAFETY: 句柄来自 hide_own_window 记录的有效顶层窗，恢复命令与其
            // 隐藏前记录的放置状态一一匹配。
            unsafe {
                ShowWindow(hwnd as Handle, restore_command(state));
            }
        }
    }
}
unsafe extern "system" fn hide_own_window(hwnd: Handle, data: isize) -> i32 {
    // SAFETY: hwnd 由 EnumWindows 逐窗传入，回调执行期间句柄有效；本调用只读
    // 查询可见性。
    let visible = unsafe { IsWindowVisible(hwnd) };
    if visible == 0 {
        return 1;
    }
    // SAFETY: 同一回调句柄，回调期间有效；只读查询 GW_OWNER 归属窗口。
    let owner = unsafe { GetWindow(hwnd, 4) };
    if !owner.is_null() {
        return 1;
    }
    let mut pid = 0;
    // SAFETY: pid 是本栈上已初始化的 u32，回调期间独占可写；hwnd 同上有效。
    unsafe { GetWindowThreadProcessId(hwnd, &raw mut pid) };
    if pid == 0 {
        return 1;
    }
    // SAFETY: GetCurrentProcessId 无参数，只读返回当前进程的 PID。
    let own = pid == unsafe { GetCurrentProcessId() };
    let named = if own {
        true
    } else {
        // SAFETY: 仅以 PROCESS_QUERY_LIMITED_INFORMATION 打开已知 pid，不继承
        // 句柄；失败由空句柄表达。
        let process = unsafe { OpenProcess(0x1000, 0, pid) };
        if process.is_null() {
            false
        } else {
            let mut path = vec![0u16; 32_768];
            let mut count = 32_768u32;
            // SAFETY: process 是刚打开且未关闭的有效句柄；path 是本栈上 NUL 结尾
            // 的 32768 字缓冲，count 初值与其容量一致，均只在本次调用期间使用。
            let queried = unsafe {
                QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &raw mut count)
            };
            // SAFETY: process 由上方 OpenProcess 打开且尚无其他引用，关闭恰一次。
            unsafe { CloseHandle(process) };
            // S9-11：按完整进程路径与已登记主程序比对——basename 撞名的
            // 用户另装实例/同名进程不隐藏（旧实现只看 basename 会误隐藏）。
            queried != 0 && {
                let main = MAIN_EXE_IMAGE
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                image_belongs_to_main(
                    &String::from_utf16_lossy(&path[..count as usize]),
                    main.as_deref(),
                )
            }
        }
    };
    if named {
        // SAFETY: data 由 hide_application_windows 经 EnumWindows 传入，指向其
        // 栈上存活的 HiddenWindows；回调同步执行，期间独占可写。
        let hidden = unsafe { &mut *(data as *mut HiddenWindows) };
        // 隐藏前先记录放置状态（最小化/最大化/普通），Drop 时按状态恢复——
        // 最小化窗口保持最小化（SW_SHOWMINNOACTIVE），不被强行正常化+抢焦点。
        hidden.0.push((hwnd as usize, restore_state(hwnd)));
        // SAFETY: 同一有效顶层窗句柄；SW_HIDE 只隐藏本窗，恢复由记录的状态驱动。
        unsafe { ShowWindow(hwnd, 0) };
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
        // SAFETY: shade_dc 是创建遮罩时得到的兼容 DC，此刻仍未销毁；shade_old 是
        // 当时摘下的 DC 原有默认位图，选回以解除对 shade_bitmap 的引用。
        unsafe { SelectObject(self.shade_dc, self.shade_old) };
        // SAFETY: shade_bitmap 已被上一行摘出 DC，删除后不再有任何引用。
        unsafe { DeleteObject(self.shade_bitmap) };
        // SAFETY: shade_dc 是本 Overlay 独占的兼容内存 DC，且已无位图选入，销毁
        // 恰一次。
        unsafe { DeleteDC(self.shade_dc) };
    }
}
thread_local! { static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) }; }
static ACTIVE_OVERLAY: AtomicUsize = AtomicUsize::new(0);

/// 请求当前框选窗口结束并按取消处理；可从服务或托盘控制线程安全调用。
///
/// # Returns
/// 当前存在框选窗口且消息已投递时返回 `true`。
pub fn cancel_selection() -> bool {
    let hwnd = ACTIVE_OVERLAY.load(Ordering::Acquire);
    if hwnd == 0 {
        return false;
    }
    // SAFETY: ACTIVE_OVERLAY 只保存 select 创建且尚未销毁的 HWND；消息投递失败
    // 仅表示窗口已在退出，不解引用该句柄。
    unsafe { PostMessageW(hwnd as Handle, WM_DONE, 0, 0) != 0 }
}
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
            // SAFETY: 实例传 null、名字传预定义 IDC_CROSS(32515)，加载系统共享
            // 光标资源，不创建独占资源。
            let cursor = unsafe { LoadCursorW(null_mut(), 32515usize as *const u16) };
            // SAFETY: 光标句柄来自上一行的系统共享资源，仅设置本线程光标。
            unsafe { SetCursor(cursor) };
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
                    // SAFETY: hwnd 是本窗口过程正在处理的窗口，消息处理期间有效；
                    // 返回的 DC 在本分支内成对 ReleaseDC。
                    let dc = unsafe { GetDC(hwnd) };
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
                        // SAFETY: dc 刚获取且未释放；bgra 指向 OVERLAY 中存活的冻结
                        // 帧缓冲，info 是本栈上完整的位图描述，调用期间均不移动。
                        unsafe {
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
                        }
                        // 暗色蒙层叠加在冻结像素上，选区内部重绘原像素：
                        // 桌面后续变化不会透过遮罩进入捕获结果。
                        // SAFETY: dc 同上；shade_dc 是创建遮罩时准备的已涂黑 1×1
                        // 位图 DC，blend 参数为 AC_SRC_OVER 加常量 alpha 0x50。
                        unsafe {
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
                        }
                        if let Some(start) = state.start {
                            let outline = selection(start, state.end);
                            if outline.right > outline.left && outline.bottom > outline.top {
                                // SAFETY: 同一有效 dc；源矩形限定在冻结帧缓冲和
                                // info 描述的同一尺寸内。
                                unsafe {
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
                            }
                            // SAFETY: 按颜色常量创建纯 GDI 画刷，不涉及外部资源。
                            let brush = unsafe { CreateSolidBrush(0x0000_ffff) };
                            if !brush.is_null() {
                                // SAFETY: outline 是本栈上的矩形，brush 刚创建且非空，
                                // 仅用于本次描边。
                                unsafe { FrameRect(dc, &raw const outline, brush) };
                                // SAFETY: brush 由上一行创建且未选入任何 DC，删除即
                                // 释放。
                                unsafe { DeleteObject(brush) };
                            }
                        }
                        // SAFETY: dc 是本分支开头 GetDC 的返回值，配对释放恰一次。
                        unsafe { ReleaseDC(hwnd, dc) };
                    }
                }
            });
            // SAFETY: hwnd 有效；矩形传 null 表示验证整个窗口客户区。
            unsafe { ValidateRect(hwnd, null()) };
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
            // SAFETY: hwnd 是正在处理的窗口；捕获在窗口销毁时由系统自动解除。
            unsafe { SetCapture(hwnd) };
            return 0;
        }
        WM_MOUSEMOVE => {
            OVERLAY.with(|s| {
                if let Some(o) = s.borrow_mut().as_mut() {
                    if o.start.is_some() {
                        o.end = point_from_lparam(l);
                        // SAFETY: hwnd 有效；矩形传 null 表示整窗重绘且不擦除背景。
                        unsafe { InvalidateRect(hwnd, null(), 0) };
                    }
                }
            });
            return 0;
        }
        WM_LBUTTONUP => {
            // SAFETY: 释放本线程先前 SetCapture 获取的鼠标捕获；无捕获时仅返回。
            unsafe { ReleaseCapture() };
            OVERLAY.with(|s| {
                if let Some(o) = s.borrow_mut().as_mut() {
                    if let Some(start) = o.start {
                        o.selected = Some(selection(start, point_from_lparam(l)));
                    }
                }
            });
            // SAFETY: hwnd 有效；私有消息 WM_DONE 只投递给本窗口，参数为零。
            unsafe { PostMessageW(hwnd, WM_DONE, 0, 0) };
            return 0;
        }
        WM_KEYDOWN if w == 0x1b => {
            // SAFETY: hwnd 有效；同上只向本窗口投递完成消息。
            unsafe { PostMessageW(hwnd, WM_DONE, 0, 0) };
            return 0;
        }
        WM_RBUTTONDOWN | WM_CLOSE => {
            // SAFETY: hwnd 有效；同上只向本窗口投递完成消息。
            unsafe { PostMessageW(hwnd, WM_DONE, 0, 0) };
            return 0;
        }
        _ => {}
    }
    // SAFETY: 未处理的消息按窗口过程约定原样转发给默认过程，参数保持不变。
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
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
    // SAFETY: 模块名传 null 取当前进程 EXE 的模块句柄，进程存活期间保持有效。
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
    // SAFETY: 窗口传 null 获取整个虚拟桌面的屏幕 DC，失败由空句柄表达。
    let screen = unsafe { GetDC(null_mut()) };
    if screen.is_null() {
        return Err(error("无法创建框选遮罩"));
    }
    // SAFETY: screen 是刚获取且尚未释放的有效 DC。
    let shade_dc = unsafe { CreateCompatibleDC(screen) };
    let shade_bitmap = if shade_dc.is_null() {
        null_mut()
    } else {
        // SAFETY: 同一有效屏幕 DC；1×1 尺寸无溢出，失败由空句柄表达。
        unsafe { CreateCompatibleBitmap(screen, 1, 1) }
    };
    let shade_old = if shade_bitmap.is_null() {
        null_mut()
    } else {
        // SAFETY: shade_dc 非空且位图尚未选入；返回值是 DC 原有的 1×1 默认位图
        // 句柄，供 Drop 时选回。
        unsafe { SelectObject(shade_dc, shade_bitmap) }
    };
    // S9-10：与主截图路径同口径——SelectObject 失败返回空句柄或 HGDI_ERROR(-1)，
    // 两种都判；HGDI_ERROR 时不得把失败选入当作成功继续涂黑。
    let shade_ready = !shade_bitmap.is_null() && gdi_replaced_ok(shade_old);
    if shade_ready {
        // SAFETY: shade_dc 有效且 shade_bitmap 已选入；BLACKNESS 只写该 1×1 内存
        // 位图，不触碰屏幕。
        unsafe { PatBlt(shade_dc, 0, 0, 1, 1, 0x0000_0042) };
    }
    // SAFETY: screen 是本函数获取的屏幕 DC，用完即归还系统恰一次。
    unsafe { ReleaseDC(null_mut(), screen) };
    if !shade_ready {
        if !shade_bitmap.is_null() {
            // SAFETY: shade_bitmap 非空且未选入任何 DC，删除即释放。
            unsafe { DeleteObject(shade_bitmap) };
        }
        if !shade_dc.is_null() {
            // SAFETY: shade_dc 非空且已无位图选入，销毁释放该兼容内存 DC。
            unsafe { DeleteDC(shade_dc) };
        }
        return Err(error("无法创建框选遮罩"));
    }
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
    // 窗口过程与线程局部 Overlay 同线程；窗口销毁后才释放图像。
    // SAFETY: wnd 指向本栈上已完整填写的 WndClass，其 class_name 指向的宽字符串
    // 以 NUL 结尾且在注册期间存活；重复注册同名类只取回既有原子。
    unsafe { RegisterClassW(&raw const wnd) };
    let (x, y, w, h) = (origin.0, origin.1, frame_width, frame_height);
    // SAFETY: class/caption 均为 NUL 结尾的宽字符串且调用期间存活；instance 来自
    // GetModuleHandleW；创建参数指针为 null 表示无附加数据。
    let hwnd = unsafe {
        CreateWindowExW(
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
        )
    };
    if hwnd.is_null() {
        OVERLAY.with(|s| {
            s.borrow_mut().take();
        });
        return Err(error("无法创建冻结框选窗口"));
    }
    ACTIVE_OVERLAY.store(hwnd as usize, Ordering::Release);
    // SAFETY: hwnd 是刚创建的有效窗口；SW_SHOW 只负责显示，不改变放置状态。
    unsafe { ShowWindow(hwnd, 5) };
    // SAFETY: 同一有效窗口；置前失败仅影响激活顺序，不破坏窗口本身。
    unsafe { SetForegroundWindow(hwnd) };
    // SAFETY: 同一有效窗口；请求一次立即重绘。
    unsafe { UpdateWindow(hwnd) };
    let mut message_error = None;
    loop {
        // SAFETY: Msg 是仅含句柄与整数的 C POD 结构，全零是合法初始状态。
        let mut msg = unsafe { std::mem::zeroed::<Msg>() };
        // SAFETY: msg 是本栈上刚清零的合法 Msg；窗口过滤传 null 表示接收本线程
        // 全部窗口的消息，消息 ID 范围 0..=0 不过滤。
        let code = unsafe { GetMessageW(&raw mut msg, null_mut(), 0, 0) };
        match interpret_message_result(code) {
            Ok(true) => {}
            Ok(false) => break,
            Err(reason) => {
                message_error = Some(reason);
                break;
            }
        }
        if msg.hwnd == hwnd && msg.message == WM_DONE {
            break;
        }
        // SAFETY: msg 是 GetMessageW 刚填充的合法消息结构，此处只读翻译。
        unsafe { TranslateMessage(&raw const msg) };
        // SAFETY: 同一合法消息结构，分发给已注册的窗口过程。
        unsafe { DispatchMessageW(&raw const msg) };
    }
    // SAFETY: hwnd 是本函数创建的窗口；消息循环退出后销毁，随后才 take 掉
    // OVERLAY 释放冻结图像。
    ACTIVE_OVERLAY.store(0, Ordering::Release);
    // SAFETY: hwnd 是本函数创建的窗口；消息循环退出后销毁，随后才 take 掉
    // OVERLAY 释放冻结图像。
    unsafe { DestroyWindow(hwnd) };
    let Some(state) = OVERLAY.with(|slot| slot.borrow_mut().take()) else {
        return Err(error("框选状态丢失"));
    };
    if let Some(reason) = message_error {
        return Err(reason);
    }
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

#[cfg(test)]
mod tests {
    use super::{
        capture_pixels_are_unusable, gdi_replaced_ok, image_belongs_to_main,
        interpret_message_result,
    };

    // 覆盖 O-17/O-18 回归：全黑是合法截图内容，不能仅凭像素全零拒绝捕获。
    #[test]
    fn all_black_frame_is_not_marked_unusable() {
        assert!(!capture_pixels_are_unusable(&[0; 4 * 16 * 16]));
    }

    // 覆盖 O-18/O-30 回归：GetMessageW 的 -1 是截图链路故障，不能伪装成取消。
    #[test]
    fn failed_message_loop_is_reported_as_error() {
        assert!(interpret_message_result(-1).is_err());
        assert!(!interpret_message_result(0).unwrap());
        assert!(interpret_message_result(1).unwrap());
    }

    // 覆盖 S9-10：SelectObject 摘下旧对象的失败形态有空句柄与 HGDI_ERROR(-1)
    // 两种，只判 is_null 会漏掉 -1——遮罩路径此前与主路径口径不一致。
    #[test]
    fn gdi_selection_rejects_null_and_hgdi_error() {
        assert!(gdi_replaced_ok(0x1234_usize as *mut std::ffi::c_void));
        assert!(!gdi_replaced_ok(std::ptr::null_mut::<std::ffi::c_void>()));
        assert!(
            !gdi_replaced_ok((-1_isize) as *mut std::ffi::c_void),
            "HGDI_ERROR(-1) 必须判为失败（S9-10）"
        );
    }

    // 覆盖 S9-11：隐藏候选窗口按完整进程路径比对——basename 撞名的另装
    // 实例不隐藏；大小写与 \\?\ 前缀差异不影响判定；未登记主程序时一律
    // 不按名字隐藏。
    #[test]
    fn main_window_hidden_only_by_full_registered_path() {
        let main = r"C:\Program Files\JchTools\JchTools.exe";
        // 大小写不敏感。
        assert!(image_belongs_to_main(
            r"c:\program files\jchtools\JchTools.exe",
            Some(main)
        ));
        // \\?\ 前缀差异不影响。
        assert!(image_belongs_to_main(
            r"\\?\C:\Program Files\JchTools\JchTools.exe",
            Some(main)
        ));
        assert!(
            !image_belongs_to_main(r"D:\tools\JchTools\JchTools.exe", Some(main)),
            "另装实例（basename 相同、完整路径不同）不得隐藏（S9-11）"
        );
        assert!(
            !image_belongs_to_main(r"C:\Program Files\JchTools\JchTools.exe", None),
            "未登记主程序路径时不按名字隐藏"
        );
        assert!(!image_belongs_to_main(
            r"C:\Windows\System32\cmd.exe",
            Some(main)
        ));
    }
}
