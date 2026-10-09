//! O-17/O-18：真实 GDI 像素与原生框选提交回归。
//! 合成画面只在内存；测试桌面不切到前台，不占用用户鼠标或抓取用户画面。

use super::{
    BitmapInfo, BitmapInfoHeader, CaptureFrame, Handle, Overlay, Point, Rect, SelectedImage,
};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::Graphics::Gdi::GetPixel;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumThreadWindows, GetClassNameW, IsWindowVisible, SendMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT,
};

#[link(name = "gdi32")]
extern "system" {
    fn CreateDIBSection(
        dc: Handle,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: Handle,
        offset: u32,
    ) -> Handle;
}
#[link(name = "user32")]
extern "system" {
    fn CreateDesktopW(
        name: *const u16,
        device: *const u16,
        mode: *const c_void,
        flags: u32,
        access: u32,
        security: *const c_void,
    ) -> Handle;
    fn SetThreadDesktop(desktop: Handle) -> i32;
    fn GetThreadDesktop(thread: u32) -> Handle;
}

const WIDTH: usize = 240;
const HEIGHT: usize = 180;

fn pixel(x: usize, y: usize) -> [u8; 3] {
    [u8::try_from(x).unwrap(), u8::try_from(y).unwrap(), 200]
}

fn frame() -> CaptureFrame {
    let mut bgra = Vec::with_capacity(WIDTH * HEIGHT * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            bgra.extend_from_slice(&pixel(x, y));
            bgra.push(0);
        }
    }
    CaptureFrame {
        width: WIDTH,
        height: HEIGHT,
        origin: (0, 0),
        work: (0, 0, 240, 180),
        bgra,
    }
}

struct Surface {
    dc: Handle,
    bitmap: Handle,
    old: Handle,
}
impl Surface {
    fn new() -> Self {
        // SAFETY: 创建测试独占的离屏 DC，不访问屏幕。
        let dc = unsafe { super::CreateCompatibleDC(null_mut()) };
        assert!(!dc.is_null());
        let info = BitmapInfo {
            header: BitmapInfoHeader {
                size: u32::try_from(size_of::<BitmapInfoHeader>()).unwrap(),
                width: 240,
                height: -180,
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
        let mut bits = null_mut();
        // SAFETY: info 完整描述 240×180 BGRA；位图及存储由 GDI 分配。
        let bitmap =
            unsafe { CreateDIBSection(dc, &raw const info, 0, &raw mut bits, null_mut(), 0) };
        assert!(!bitmap.is_null());
        // SAFETY: 两句柄均为本测试新建且有效；保留旧对象以成对恢复。
        let old = unsafe { super::SelectObject(dc, bitmap) };
        assert!(super::gdi_replaced_ok(old));
        Self { dc, bitmap, old }
    }
    fn pixels(&self) -> Vec<[u8; 3]> {
        let mut pixels = Vec::with_capacity(WIDTH * HEIGHT);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                // SAFETY: dc 有效，坐标位于测试离屏位图；GetPixel 同步 GDI 绘制。
                let color = unsafe {
                    GetPixel(
                        self.dc,
                        i32::try_from(x).unwrap(),
                        i32::try_from(y).unwrap(),
                    )
                };
                let bytes = color.to_le_bytes();
                pixels.push([bytes[2], bytes[1], bytes[0]]);
            }
        }
        pixels
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: 恢复 DC 原有对象以解除位图引用。
        unsafe { super::SelectObject(self.dc, self.old) };
        // SAFETY: 本测试位图已解除引用，成对删除。
        unsafe { super::DeleteObject(self.bitmap) };
        // SAFETY: 独占 DC 仍有效，成对销毁。
        unsafe { super::DeleteDC(self.dc) };
    }
}

fn rendered_preview(start: (i16, i16), end: (i16, i16)) -> Vec<[u8; 3]> {
    let surface = Surface::new();
    // SAFETY: 仅使用本测试离屏 DC 创建暗色遮罩资源。
    let shade_dc = unsafe { super::CreateCompatibleDC(surface.dc) };
    assert!(!shade_dc.is_null());
    // SAFETY: dc 有效；1×1 位图由本次测试拥有。
    let shade_bitmap = unsafe { super::CreateCompatibleBitmap(surface.dc, 1, 1) };
    assert!(!shade_bitmap.is_null());
    // SAFETY: 将新建位图选入独占的兼容 DC，保存旧对象供 Overlay 析构恢复。
    let shade_old = unsafe { super::SelectObject(shade_dc, shade_bitmap) };
    assert!(super::gdi_replaced_ok(shade_old));
    // SAFETY: 遮罩 DC 有效且已选入位图；仅涂黑该离屏像素。
    unsafe { super::PatBlt(shade_dc, 0, 0, 1, 1, 0x0000_0042) };
    let mut state = Overlay {
        frame: frame(),
        start: None,
        end: Point::default(),
        selected: None,
        shade_dc,
        shade_bitmap,
        shade_old,
    };
    let header_size = u32::try_from(size_of::<BitmapInfoHeader>()).unwrap();
    super::paint_overlay(surface.dc, &state, 240, 180, header_size);
    state.start = Some(Point {
        x: i32::from(start.0),
        y: i32::from(start.1),
    });
    // 先扩大，再收缩：最终框外不能残留上次选区的亮色拖影。
    state.end = Point { x: 220, y: 170 };
    super::paint_overlay(surface.dc, &state, 240, 180, header_size);
    state.end = Point {
        x: i32::from(end.0),
        y: i32::from(end.1),
    };
    super::paint_overlay(surface.dc, &state, 240, 180, header_size);
    assert_eq!(state.frame.bgra, frame().bgra, "绘制不能改写冻结帧");
    surface.pixels()
}

unsafe extern "system" fn find_visible(hwnd: HWND, data: LPARAM) -> i32 {
    let mut name = [0u16; 64];
    // SAFETY: name 为同步回调独占的宽字符缓冲。
    let length = unsafe { GetClassNameW(hwnd, name.as_mut_ptr(), 64) };
    if length <= 0
        || String::from_utf16_lossy(&name[..usize::try_from(length).unwrap()])
            != "JchSnapOcrFrozenSelection"
    {
        return 1;
    }
    // SAFETY: hwnd 来自本次 selector 线程的窗口枚举。
    if unsafe { IsWindowVisible(hwnd) } != 0 {
        // SAFETY: data 指向调用方栈上的 usize，同步枚举期间独占可写。
        unsafe { *(data as *mut usize) = hwnd as usize };
        return 0;
    }
    1
}

fn mouse_point(point: (i16, i16)) -> isize {
    let (x, y) = point;
    isize::try_from(u32::from(x.cast_unsigned()) | (u32::from(y.cast_unsigned()) << 16)).unwrap()
}

/// 真实 select、Win32 鼠标消息与裁剪提交；测试 desktop 始终不切到前台。
fn drag_selection(start: (i16, i16), end: (i16, i16)) -> SelectedImage {
    // SAFETY: 控制线程按物理像素发消息，避免 Windows 跨 DPI 上下文缩放坐标。
    let previous_dpi =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let (thread_tx, thread_rx) = mpsc::channel();
    let selector = std::thread::spawn(move || {
        // SAFETY: 只查询本测试线程 ID。
        let thread_id = unsafe { GetCurrentThreadId() };
        // SAFETY: 查询本线程原有 desktop，借用句柄仅用于测试结束时恢复。
        let previous_desktop = unsafe { GetThreadDesktop(thread_id) };
        let name = super::wide(&format!(
            "JchToolsSelectionRegression-{}-{thread_id}",
            std::process::id()
        ));
        // SAFETY: 创建测试自有 desktop，不切换用户桌面；字符串在调用期间有效。
        let desktop = unsafe { CreateDesktopW(name.as_ptr(), null(), null(), 0, 0x01ff, null()) };
        assert!(!desktop.is_null());
        // 系统可能创建隐式 IME 窗口并暂时占用 desktop；句柄由本独立测试进程
        // 持有，子进程退出时由 Windows 统一释放，不访问或切换用户桌面。
        // SAFETY: 本线程尚无窗口，绑定新建且有效的测试 desktop。
        assert_ne!(unsafe { SetThreadDesktop(desktop) }, 0);
        // SAFETY: 只设本测试线程的 DPI 上下文。
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        thread_tx.send(thread_id).unwrap();
        let selected = super::select(frame());
        // SAFETY: select 已销毁框选窗口，恢复本测试线程原有的有效 desktop。
        assert_ne!(unsafe { SetThreadDesktop(previous_desktop) }, 0);
        selected
    });
    let thread_id = thread_rx.recv().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut window = 0usize;
    while window == 0 && Instant::now() < deadline {
        // SAFETY: 只枚举本次 selector 线程；window 在同步枚举期间独占可写。
        unsafe { EnumThreadWindows(thread_id, Some(find_visible), (&raw mut window) as isize) };
        if window == 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert_ne!(window, 0, "冻结框选窗口必须在期限内创建");
    let hwnd = window as HWND;
    // SAFETY: hwnd 属于本次测试 desktop；同步发送鼠标按下消息。
    unsafe { SendMessageW(hwnd, WM_LBUTTONDOWN, 1, mouse_point(start)) };
    // SAFETY: 同一窗口；同步更新选区，允许模拟捕获拖出边缘。
    unsafe { SendMessageW(hwnd, WM_MOUSEMOVE, 1, mouse_point(end)) };
    // SAFETY: 同一窗口；调用产品 WM_PAINT 处理路径。
    unsafe { SendMessageW(hwnd, WM_PAINT, 0, 0) };
    // SAFETY: 松开左键走产品提交，不使用测试专用裁剪逻辑。
    unsafe { SendMessageW(hwnd, WM_LBUTTONUP, 0, mouse_point(end)) };
    let selected = selector.join().unwrap().unwrap();
    // SAFETY: 恢复控制线程原有的有效上下文。
    unsafe { SetThreadDpiAwarenessContext(previous_dpi) };
    selected
}

// 覆盖 O-17/O-18：上下半屏、四种拖动方向和越出帧边缘，预览与提交裁剪来自
// 同一冻结位置；框外保持暗色，收缩选区不残留亮色。像素断言使用产品绘制函数
// 与真实离屏 GDI，提交断言使用真实原生窗口；不依赖模型、用户桌面或鼠标输入。
#[test]
fn selection_preview_matches_frozen_pixels_and_submitted_crop() {
    const CHILD_FLAG: &str = "JCHTOOLS_SELECTION_PREVIEW_TEST_CHILD";
    if std::env::var_os(CHILD_FLAG).is_none() {
        // TrayHandle 析构会取消进程内 ACTIVE_OVERLAY；独立进程避免服务测试干扰。
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "capture_win::preview_tests::selection_preview_matches_frozen_pixels_and_submitted_crop", "--test-threads=1"])
            .env(CHILD_FLAG, "1").output().unwrap();
        assert!(
            output.status.success(),
            "框选回归子进程失败：{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for (start, end) in [
        ((20, 10), (100, 60)),
        ((100, 60), (20, 10)),
        ((20, 150), (100, 100)),
        ((100, 100), (20, 150)),
        ((20, 10), (-10, 60)),
        ((100, 100), (250, 190)),
    ] {
        let displayed = rendered_preview(start, end);
        let Rect {
            left,
            top,
            right,
            bottom,
        } = super::selection(
            Point {
                x: i32::from(start.0),
                y: i32::from(start.1),
            },
            Point {
                x: i32::from(end.0),
                y: i32::from(end.1),
            },
        );
        let x0 = usize::try_from(left.clamp(0, 240)).unwrap();
        let y0 = usize::try_from(top.clamp(0, 180)).unwrap();
        let x1 = usize::try_from(right.clamp(0, 240)).unwrap();
        let y1 = usize::try_from(bottom.clamp(0, 180)).unwrap();
        let (image, work) = drag_selection(start, end).unwrap();
        assert_eq!(work, (0, 0, 240, 180));
        assert_eq!((image.width(), image.height()), (x1 - x0, y1 - y0));
        for y in y0..y1 {
            for x in x0..x1 {
                let index = ((y - y0) * image.width() + x - x0) * 3;
                assert_eq!(&image.data[index..index + 3], pixel(x, y));
                // 排除描边占用的一圈像素，内部必须保持原始颜色和位置。
                if x > x0 && x + 1 < x1 && y > y0 && y + 1 < y1 {
                    assert_eq!(
                        displayed[y * WIDTH + x],
                        pixel(x, y),
                        "框内预览必须与冻结位置一致，坐标 ({x}, {y})，拖动 {start:?} → {end:?}"
                    );
                }
            }
        }
        // 此点在最终选区外，检测缩小后留下的亮色拖影。
        assert!(displayed[80 * WIDTH + 230][2] < 180, "框外必须保持暗色遮罩");
    }
}
