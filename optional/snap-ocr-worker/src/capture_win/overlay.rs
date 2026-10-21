//! 冻结帧全屏框选遮罩：暗色蒙层、选区预览、鼠标/键盘交互与裁剪提交（O-18）。
//! 从 `capture_win.rs` 按内聚职责原样搬移；行为与注释保持不变。

use std::cell::RefCell;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::interpret_message_result;
use super::{
    error, gdi_replaced_ok, wide, AlphaBlend, BgrImage, BitmapInfo, BitmapInfoHeader, CaptureFrame,
    CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, CreateWindowExW, DefWindowProcW,
    DeleteDC, DeleteObject, DestroyWindow, DispatchMessageW, FrameRect, GetDC, GetMessageW,
    GetModuleHandleW, Handle, IntersectClipRect, InvalidateRect, LoadCursorW, Msg, PatBlt, Point,
    PostMessageW, Rect, RegisterClassW, ReleaseCapture, ReleaseDC, RestoreDC, SaveDC, SelectObject,
    SetCapture, SetCursor, SetForegroundWindow, ShowWindow, StretchDIBits, TranslateMessage,
    UpdateWindow, ValidateRect, WndClass, SRCCOPY, WM_CLOSE, WM_DONE, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR,
    WS_EX_TOPMOST, WS_POPUP,
};

pub(super) struct Overlay {
    pub(super) frame: CaptureFrame,
    pub(super) start: Option<Point>,
    pub(super) end: Point,
    pub(super) selected: Option<Rect>,
    pub(super) shade_dc: Handle,
    pub(super) shade_bitmap: Handle,
    pub(super) shade_old: Handle,
}
impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: shade_dc 是创建遮罩时得到的兼容 DC，此刻仍未销毁；shade_old 是
        // 当时摘下的 DC 原有默认位图，选回以解除对 shade_bitmap 的引用。
        unsafe { SelectObject(self.shade_dc, self.shade_old) };
        // 先销毁 DC，确保即使选回旧位图失败也解除对遮罩位图的引用；
        // 仍选入 DC 的位图不能由 DeleteObject 释放。
        // SAFETY: shade_dc 为本 Overlay 独占且尚未销毁，销毁恰一次。
        unsafe { DeleteDC(self.shade_dc) };
        // SAFETY: shade_bitmap 不再被本 Overlay 的 DC 引用，删除恰一次。
        unsafe { DeleteObject(self.shade_bitmap) };
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
pub(super) fn selection(a: Point, b: Point) -> Rect {
    Rect {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

/// 完成消息明确区分提交与取消，不能把取消前已写入的选区当成提交。
pub(super) fn completed_selection(selected: Option<Rect>, submitted: bool) -> Option<Rect> {
    if submitted {
        selected
    } else {
        None
    }
}

// O-18：窗口与离屏像素回归共用同一 GDI 绘制路径，冻结帧不作改写。
pub(super) fn paint_overlay(
    dc: Handle,
    state: &Overlay,
    width: i32,
    height: i32,
    header_size: u32,
) {
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
            // O-18：沿用整张冻结帧的绘制坐标，只把目标裁剪到选区。
            // 直接用选区 top 作为 StretchDIBits 的子图源 y，在实际
            // GDI 上会读到另一高度的像素，导致预览与最终截图不符。
            // SAFETY: dc 是本次绘制的有效 DC；保存后仅临时改变裁剪区。
            let saved = unsafe { SaveDC(dc) };
            if saved != 0 {
                // SAFETY: 同一有效 dc；GDI 自动与窗口已有裁剪区取交集，
                // 拖动越出显示器边缘时也不会读取帧外像素。
                unsafe {
                    IntersectClipRect(dc, outline.left, outline.top, outline.right, outline.bottom);
                }
                // SAFETY: 缓冲与 info 完整描述同一冻结帧；源坐标为全图，
                // 只有选区内的目标像素可绘制，不使用实时桌面内容。
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
                // SAFETY: saved 是同一 dc 刚成功保存的状态；恢复后
                // 后续描边与下一次绘制不会继承选区裁剪。
                unsafe { RestoreDC(dc, saved) };
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
                        paint_overlay(dc, state, width, height, header_size);
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
            // SAFETY: hwnd 有效；私有完成消息以 wParam=1 表示左键提交，
            // 取消路径保持 wParam=0，避免取消消息误提交已有矩形。
            unsafe { PostMessageW(hwnd, WM_DONE, 1, 0) };
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
    let mut submitted = false;
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
            submitted = msg.wparam == 1;
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
    let Some(rect) = completed_selection(state.selected, submitted) else {
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
