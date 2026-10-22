//! `tray` 内联测试模块原样搬移（热键解析、消息循环清理与托盘图标回归）。

use super::{message_loop_requires_cleanup, parse_hotkey};

// 覆盖 O-14：可设置原版命名键与功能键；重复修饰键/无修饰键不得注册。
#[test]
fn accepts_original_key_domain_but_rejects_invalid_combinations() {
    assert_eq!(parse_hotkey("Ctrl+Alt+O"), Ok((0x4003, 0x4f)));
    assert_eq!(parse_hotkey("Ctrl+Shift+PageDown"), Ok((0x4006, 0x22)));
    assert_eq!(parse_hotkey("Win+F24"), Ok((0x4008, 0x87)));
    for invalid in ["O", "Ctrl+F25", "Ctrl+Ctrl+O", "Alt+O+P", "Ctrl+"] {
        assert!(parse_hotkey(invalid).is_err(), "{invalid} 不应替换有效热键");
    }
}

// 覆盖 O-11/O-14/O-16 回归：消息循环异常退出也必须进入托盘清理路径。
#[test]
fn failed_message_loop_requires_tray_cleanup() {
    assert!(message_loop_requires_cleanup(-1));
    assert!(message_loop_requires_cleanup(0));
    assert!(!message_loop_requires_cleanup(1));
}

// 覆盖 O-12：检查实际 HICON 的像素，避免托盘退回通用程序图标或丢失透明圆角。
#[test]
fn notification_icon_renders_application_brand() {
    use windows_sys::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};
    struct IconBitmaps(ICONINFO);
    impl Drop for IconBitmaps {
        fn drop(&mut self) {
            // SAFETY: GetIconInfo 创建的两个位图归本测试独占，非空时各释放一次。
            if !self.0.hbmColor.is_null() {
                unsafe { DeleteObject(self.0.hbmColor) };
            }
            // SAFETY: 同上，掩码位图由 GetIconInfo 分配。
            if !self.0.hbmMask.is_null() {
                unsafe { DeleteObject(self.0.hbmMask) };
            }
        }
    }
    // SAFETY: ICONINFO 是纯 Win32 输出结构，全零有效。
    let mut info = IconBitmaps(unsafe { std::mem::zeroed() });
    // SAFETY: 图标句柄来自生产加载路径，输出结构有效；不注册通知区、热键或窗口。
    assert_ne!(
        unsafe { GetIconInfo(super::notification_icon(), &raw mut info.0) },
        0
    );
    // SAFETY: BITMAP 是纯输出结构，全零有效。
    let mut bitmap: BITMAP = unsafe { std::mem::zeroed() };
    // SAFETY: 只读取 GetIconInfo 复制出的彩色位图，缓冲大小与结构一致。
    assert_ne!(
        unsafe {
            GetObjectW(
                info.0.hbmColor,
                i32::try_from(std::mem::size_of::<BITMAP>()).unwrap(),
                (&raw mut bitmap).cast(),
            )
        },
        0
    );
    let width = usize::try_from(bitmap.bmWidth).unwrap();
    let height = usize::try_from(bitmap.bmHeight).unwrap();
    // SAFETY: BITMAPINFO 为纯数据结构，下方完整填写所需头部。
    let mut dib: BITMAPINFO = unsafe { std::mem::zeroed() };
    dib.bmiHeader.biSize = u32::try_from(std::mem::size_of::<BITMAPINFOHEADER>()).unwrap();
    dib.bmiHeader.biWidth = bitmap.bmWidth;
    dib.bmiHeader.biHeight = -bitmap.bmHeight;
    dib.bmiHeader.biPlanes = 1;
    dib.bmiHeader.biBitCount = 32;
    dib.bmiHeader.biCompression = BI_RGB;
    let mut pixels = vec![0_u8; width * height * 4];
    // SAFETY: 仅取得读取位图所需的屏幕 DC，不捕获屏幕或改变焦点。
    let dc = unsafe { GetDC(std::ptr::null_mut()) };
    assert!(!dc.is_null());
    // SAFETY: 位图未选入 DC；输出缓冲按完整 32bpp、top-down 图像大小分配。
    let rows = unsafe {
        GetDIBits(
            dc,
            info.0.hbmColor,
            0,
            u32::try_from(height).unwrap(),
            pixels.as_mut_ptr().cast(),
            &raw mut dib,
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: 归还本测试 GetDC 获得的同一 DC。
    unsafe { ReleaseDC(std::ptr::null_mut(), dc) };
    assert_eq!(usize::try_from(rows).unwrap(), height);
    let pixel = |x: usize, y: usize| {
        let index = ((y * height / 16) * width + x * width / 16) * 4;
        &pixels[index..index + 4]
    };
    let blue = pixel(4, 4); // 品牌圆角矩形内部的靛蓝色。
    assert!(
        blue[0] > 180 && blue[1] < 180 && blue[2] < 120 && blue[3] > 200,
        "必须显示 JchTools 品牌蓝底，而非 Windows 通用图标：{blue:?}"
    );
    for point in [(10, 5), (6, 10)] {
        // 白色 J 的竖笔与横笔。
        let white = pixel(point.0, point.1);
        assert!(
            white[..3].iter().all(|channel| *channel > 220) && white[3] > 200,
            "品牌白色 J 必须完整可见：{point:?}={white:?}"
        );
    }
    assert_eq!(pixel(0, 0)[3], 0, "圆角外部必须透明，不能显示实心方块");
}

// 覆盖 XB-23/O-11/O-12：Explorer 恢复期间首次 NIM_ADD 失败不能永久丢失托盘。
// 只验证真实 Shell API 失败后安排原生重试；不代表真实 Explorer 已绘制图标。
#[test]
fn taskbar_recreation_retries_after_native_registration_failure() {
    use super::{notify, wide, Handle, TrayState, STATE, TASKBAR_CREATED};
    use std::ptr::{null, null_mut};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, SendMessageW, SetWindowLongPtrW, GWLP_WNDPROC, MSG,
        PM_REMOVE, WM_TIMER,
    };

    std::thread::spawn(|| {
        #[link(name = "user32")]
        extern "system" {
            fn CreateDesktopW(
                name: *const u16,
                device: *const u16,
                mode: *const std::ffi::c_void,
                flags: u32,
                access: u32,
                security: *const std::ffi::c_void,
            ) -> Handle;
            fn GetThreadDesktop(thread: u32) -> Handle;
            fn SetThreadDesktop(desktop: Handle) -> i32;
            fn CloseDesktop(desktop: Handle) -> i32;
        }
        struct IsolatedDesktop {
            previous: Handle,
            desktop: Handle,
            window: Handle,
        }
        impl Drop for IsolatedDesktop {
            fn drop(&mut self) {
                if !self.window.is_null() {
                    // SAFETY: 仅销毁本线程的隐藏窗口；托盘清理仍在隔离 desktop 内。
                    unsafe { super::DestroyWindow(self.window) };
                }
                let _ = STATE.with(|slot| slot.borrow_mut().take());
                // SAFETY: 本线程窗口已销毁，恢复系统持有的原 desktop。
                assert_ne!(unsafe { SetThreadDesktop(self.previous) }, 0);
                // SAFETY: 关闭本测试创建且不再绑定线程的 desktop，不切换显示桌面。
                assert_ne!(unsafe { CloseDesktop(self.desktop) }, 0);
            }
        }
        // SAFETY: 无参数线程查询，不改变用户环境。
        let thread = unsafe { GetCurrentThreadId() };
        // SAFETY: 查询当前线程已绑定的 desktop，句柄仍由系统持有。
        let previous = unsafe { GetThreadDesktop(thread) };
        let name = wide(&format!(
            "JchToolsTrayRecovery-{}-{thread}",
            std::process::id()
        ));
        // SAFETY: 创建未显示的测试 desktop；字符串存活，安全属性使用默认值。
        let desktop = unsafe { CreateDesktopW(name.as_ptr(), null(), null(), 0, 0x01ff, null()) };
        assert!(
            !desktop.is_null(),
            "必须成功创建隔离 desktop，不能降级到用户桌面"
        );
        let mut isolated = IsolatedDesktop {
            previous,
            desktop,
            window: null_mut(),
        };
        // SAFETY: 新线程尚无窗口或 hook，只绑定本线程，不调用 SwitchDesktop。
        assert_ne!(unsafe { SetThreadDesktop(desktop) }, 0);
        let class = wide("STATIC");
        // SAFETY: 系统 STATIC 类和 HWND_MESSAGE(-3) 仅创建本线程的隐藏消息窗口。
        isolated.window = unsafe {
            super::CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                -3isize as Handle,
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        assert!(!isolated.window.is_null(), "必须创建隔离消息窗口");
        // SAFETY: 本线程拥有窗口，回调使用正确 Win32 ABI，存活至窗口销毁。
        assert_ne!(
            unsafe {
                SetWindowLongPtrW(
                    isolated.window,
                    GWLP_WNDPROC,
                    super::wnd_proc as *const () as isize,
                )
            },
            0
        );
        let (commands, _received) = mpsc::channel();
        STATE.with(|slot| {
            *slot.borrow_mut() = Some(TrayState {
                commands,
                queue: Arc::new(Mutex::new(Some(std::collections::VecDeque::new()))),
                active: 0, // 不注册或注销任何用户全局热键。
                hotkey: "Ctrl+Alt+O".into(),
                icon_retries_left: 0,
            });
        });
        assert!(
            !notify(isolated.window, None, "Ctrl+Alt+O", 0),
            "隔离 desktop 无 Explorer，真实 NIM_ADD 必须失败"
        );
        let taskbar_created = *TASKBAR_CREATED;
        assert_ne!(taskbar_created, 0, "TaskbarCreated 消息必须成功注册");
        // SAFETY: 同步投递至本线程独占消息窗口，执行真实生产窗口过程。
        unsafe { SendMessageW(isolated.window, taskbar_created, 0, 0) };
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut retried = false;
        while Instant::now() < deadline {
            // SAFETY: MSG 是纯输出缓冲，全零有效；仅读取测试窗口的 WM_TIMER。
            let mut message: MSG = unsafe { std::mem::zeroed() };
            // SAFETY: 输出缓冲有效，过滤本线程独占窗口，不消费用户消息。
            if unsafe {
                PeekMessageW(
                    &raw mut message,
                    isolated.window,
                    WM_TIMER,
                    WM_TIMER,
                    PM_REMOVE,
                )
            } != 0
            {
                // SAFETY: 消息来自该窗口原生队列，执行原生定时器恢复路径。
                unsafe { DispatchMessageW(&raw const message) };
                retried = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            retried,
            "TaskbarCreated 的真实 NIM_ADD 失败后必须安排原生定时重试，不能静默永久失图"
        );
    })
    .join()
    .expect("隔离托盘恢复回归失败");
}
