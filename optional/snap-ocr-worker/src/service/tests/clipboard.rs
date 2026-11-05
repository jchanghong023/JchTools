//! 剪贴板回归：独立 window station 中的文本、PNG 与迟到发布保护。
// 不读取、清空或恢复当前用户 station 的任何剪贴板格式。

use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::service::clipboard::{copy_png, sequence_number};
use windows_sys::Win32::{
    Foundation::HWND,
    System::{
        DataExchange::{GetClipboardData, RegisterClipboardFormatW},
        Memory::GlobalSize,
        Ole::CF_UNICODETEXT,
        StationsAndDesktops::{
            CloseDesktop, CloseWindowStation, CreateDesktopW, CreateWindowStationW,
            GetProcessWindowStation, GetThreadDesktop, SetProcessWindowStation, SetThreadDesktop,
            DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_ENUMERATE, DESKTOP_HOOKCONTROL,
            DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READOBJECTS,
            DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS, HDESK, HWINSTA,
        },
        Threading::GetCurrentThreadId,
    },
    UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINSTA_ALL_ACCESS, WS_POPUP},
};

struct IsolatedClipboard {
    previous: HWINSTA,
    station: HWINSTA,
    previous_desktop: HDESK,
    desktop: HDESK,
    window: HWND,
}

impl Drop for IsolatedClipboard {
    fn drop(&mut self) {
        if !self.window.is_null() {
            // SAFETY: 本测试线程创建并独占隐藏窗口，销毁一次。
            unsafe { DestroyWindow(self.window) };
        }
        // SAFETY: previous_desktop 保留系统原句柄；隐藏窗口已销毁。
        let restored_desktop = unsafe { SetThreadDesktop(self.previous_desktop) };
        assert_ne!(restored_desktop, 0, "必须恢复线程原 desktop");
        if !self.desktop.is_null() {
            // SAFETY: 测试 desktop 已不再关联当前线程，关闭独占句柄。
            unsafe { CloseDesktop(self.desktop) };
        }
        // SAFETY: previous 为进入测试前的有效 station，仍由系统持有。
        let restored = unsafe { SetProcessWindowStation(self.previous) };
        assert_ne!(restored, 0, "必须恢复进程原 window station");
        // SAFETY: 已恢复原 station，关闭本测试创建的独占 station。
        unsafe { CloseWindowStation(self.station) };
    }
}

fn with_isolated_clipboard(test_name: &str, run: impl FnOnce(&IsolatedClipboard)) {
    const CHILD_FLAG: &str = "JCHTOOLS_ISOLATED_CLIPBOARD_TEST_CHILD";
    if std::env::var_os(CHILD_FLAG).as_deref() != Some(std::ffi::OsStr::new("1")) {
        // station 是进程级状态，每个用例在仅运行自身的独立进程中切换。
        let output =
            std::process::Command::new(std::env::current_exe().expect("必须定位当前测试程序"))
                .args(["--exact", test_name, "--test-threads=1"])
                .env(CHILD_FLAG, "1")
                .output()
                .expect("必须启动独立剪贴板测试进程");
        assert!(
            output.status.success(),
            "隔离剪贴板子进程失败：{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    // SAFETY: 只查询进程当前 station；不取得所有权。
    let previous = unsafe { GetProcessWindowStation() };
    assert!(!previous.is_null());
    // SAFETY: 无参数只读查询当前线程 ID。
    let thread = unsafe { GetCurrentThreadId() };
    // SAFETY: 查询线程当前 desktop，不取得所有权。
    let previous_desktop = unsafe { GetThreadDesktop(thread) };
    assert!(!previous_desktop.is_null());
    // SAFETY: 空名字创建独立非交互 station；访问仅用于本进程窗口/剪贴板。
    let station = unsafe { CreateWindowStationW(null(), 0, WINSTA_ALL_ACCESS as u32, null()) };
    assert!(!station.is_null(), "无法创建隔离 clipboard station");
    let mut isolated = IsolatedClipboard {
        previous,
        station,
        previous_desktop,
        desktop: null_mut(),
        window: null_mut(),
    };
    // SAFETY: station 为本测试刚创建的有效句柄，尚无测试窗口。
    let selected = unsafe { SetProcessWindowStation(station) };
    assert_ne!(selected, 0, "无法切换到隔离 clipboard station");
    let desktop_name = super::protocol::wide("JchToolsClipboardRegression");
    // SAFETY: 在独立 station 中创建测试 desktop，字符串有效，权限仅用于测试。
    isolated.desktop = unsafe {
        CreateDesktopW(
            desktop_name.as_ptr(),
            null(),
            null(),
            0,
            DESKTOP_READOBJECTS
                | DESKTOP_CREATEWINDOW
                | DESKTOP_CREATEMENU
                | DESKTOP_HOOKCONTROL
                | DESKTOP_JOURNALRECORD
                | DESKTOP_JOURNALPLAYBACK
                | DESKTOP_ENUMERATE
                | DESKTOP_WRITEOBJECTS
                | DESKTOP_SWITCHDESKTOP,
            null(),
        )
    };
    assert!(!isolated.desktop.is_null(), "无法创建隔离 desktop");
    // SAFETY: 当前测试线程尚无窗口或 hook，可绑定刚创建的有效 desktop。
    let selected_desktop = unsafe { SetThreadDesktop(isolated.desktop) };
    assert_ne!(selected_desktop, 0, "无法切换到隔离 desktop");
    let class = super::protocol::wide("STATIC");
    let title = super::protocol::wide("JchTools clipboard regression");
    // SAFETY: 系统 STATIC 类，NUL 终止字符串，隐藏窗口无父级和额外参数。
    isolated.window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
        )
    };
    assert!(!isolated.window.is_null(), "无法创建隔离剪贴板 owner 窗口");
    run(&isolated);
}

fn read_clipboard(isolated: &IsolatedClipboard, format: u32) -> Vec<u8> {
    // SAFETY: 仅打开测试独立 station 中本线程窗口对应的剪贴板。
    assert_ne!(unsafe { super::OpenClipboard(isolated.window) }, 0);
    // SAFETY: 本线程已打开隔离剪贴板，返回的句柄仍归系统所有。
    let memory = unsafe { GetClipboardData(format) };
    assert!(!memory.is_null(), "剪贴板必须提供请求的格式");
    // SAFETY: 剪贴板保持打开，句柄有效且使用全局内存。
    let size = unsafe { GlobalSize(memory) };
    // SAFETY: 只读锁定同一有效全局内存句柄。
    let data = unsafe { super::GlobalLock(memory) };
    assert!(!data.is_null());
    // SAFETY: size 为该块字节容量，锁定期间只读且不越界。
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec();
    // SAFETY: 解锁刚锁定的系统内存，不释放其句柄。
    unsafe { super::GlobalUnlock(memory) };
    // SAFETY: 配对关闭本线程打开的隔离剪贴板。
    assert_ne!(unsafe { super::CloseClipboard() }, 0);
    bytes
}

fn read_text(isolated: &IsolatedClipboard) -> String {
    let bytes = read_clipboard(isolated, u32::from(CF_UNICODETEXT));
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|unit| u16::from_ne_bytes([unit[0], unit[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16(&units).expect("合成文本必须是有效 UTF-16")
}

fn synthetic_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut bytes),
        &[20, 80, 160, 255],
        1,
        1,
        image::ExtendedColorType::Rgba8,
    )
    .expect("合成 PNG 必须能编码");
    bytes
}

#[test]
fn copy_all_round_trips_on_isolated_clipboard() {
    with_isolated_clipboard(
        "service::tests::clipboard::copy_all_round_trips_on_isolated_clipboard",
        |isolated| {
            let text = "截图 OCR 合成文本  ABC\n  缩进 😀";
            let copied = super::copy_text(text);
            assert!(copied.is_ok(), "复制全部必须成功：{copied:?}");
            assert_eq!(read_text(isolated), text, "剪贴板正文必须逐字符保持");
        },
    );
}

#[test]
fn png_round_trips_exact_encoded_bytes_on_isolated_clipboard() {
    with_isolated_clipboard(
        "service::tests::clipboard::png_round_trips_exact_encoded_bytes_on_isolated_clipboard",
        |isolated| {
            let png = synthetic_png();
            copy_png(&png, sequence_number(), &Mutex::new(true)).expect("PNG 必须复制成功");
            let name = super::protocol::wide("PNG");
            // SAFETY: NUL 终止的格式名，仅注册名称，不访问用户剪贴板。
            let format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
            assert_ne!(format, 0);
            let actual = read_clipboard(isolated, format);
            assert!(actual.len() >= png.len());
            // GlobalSize 可包含分配器尾部容量，完整 PNG 的编码字节必须逐字节不变。
            assert_eq!(&actual[..png.len()], png.as_slice());
            let decoded = image::load_from_memory(&actual).expect("剪贴板 PNG 必须能直接解码");
            assert_eq!(decoded.to_rgba8().as_raw(), &[20, 80, 160, 255]);
        },
    );
}

#[test]
fn stale_and_cancelled_png_leave_newer_clipboard_untouched() {
    with_isolated_clipboard(
        "service::tests::clipboard::stale_and_cancelled_png_leave_newer_clipboard_untouched",
        |isolated| {
            super::copy_text("较早内容").expect("准备较早内容");
            let expected = sequence_number();
            super::copy_text("用户新复制的内容").expect("准备后续复制");
            let newer = sequence_number();
            assert_ne!(expected, newer);
            let png = synthetic_png();
            let stale =
                copy_png(&png, expected, &Mutex::new(true)).expect_err("旧任务不得覆盖新内容");
            assert!(stale.contains("新内容") && stale.contains("未复制"));
            assert_eq!(sequence_number(), newer);
            assert_eq!(read_text(isolated), "用户新复制的内容");
            let cancelled = copy_png(&png, newer, &Mutex::new(false)).expect_err("取消不得发布");
            assert!(cancelled.contains("取消") && cancelled.contains("未复制"));
            assert_eq!(sequence_number(), newer);
            assert_eq!(read_text(isolated), "用户新复制的内容");
        },
    );
}

#[test]
fn occupied_clipboard_fails_boundedly_and_cancellation_preserves_content() {
    with_isolated_clipboard(
        "service::tests::clipboard::occupied_clipboard_fails_boundedly_and_cancellation_preserves_content",
        |isolated| {
            super::copy_text("占用期间必须保留").expect("准备占用前内容");
            let expected = sequence_number();
            let png = synthetic_png();
            // SAFETY: 仅持有隔离剪贴板；后台线程必须报告占用，不能清空。
            assert_ne!(unsafe { super::OpenClipboard(isolated.window) }, 0);
            let worker_png = png.clone();
            let (result_tx, result_rx) = mpsc::channel();
            let started = Instant::now();
            let worker = std::thread::spawn(move || {
                let result = copy_png(&worker_png, expected, &Mutex::new(true));
                result_tx.send(result).expect("回传占用结果");
            });
            let occupied = result_rx.recv_timeout(Duration::from_secs(3));
            // SAFETY: 必须在断言前配对关闭，超时时也释放占用以便线程退出。
            assert_ne!(unsafe { super::CloseClipboard() }, 0);
            worker.join().expect("占用测试线程正常结束");
            let failure = occupied.expect("占用重试必须有限结束").expect_err("占用不能误报成功");
            assert!(failure.contains("占用") && failure.contains("未复制"));
            assert!(started.elapsed() < Duration::from_secs(3));
            assert_eq!(sequence_number(), expected);
            assert_eq!(read_text(isolated), "占用期间必须保留");

            // 锁住有效性门，在后台线程尚不能通过检查时取消；释放占用后仍不得发布。
            let valid = Arc::new(Mutex::new(true));
            let mut validity = valid.lock().expect("锁定任务有效性");
            // SAFETY: 再次只打开隔离 clipboard。
            assert_ne!(unsafe { super::OpenClipboard(isolated.window) }, 0);
            let worker_valid = Arc::clone(&valid);
            let (ready_tx, ready_rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                ready_tx.send(()).expect("通知后台复制开始");
                copy_png(&png, expected, &worker_valid)
            });
            ready_rx.recv().expect("后台复制线程已开始");
            *validity = false;
            drop(validity);
            let cancelled = worker.join().expect("取消测试线程正常结束");
            // SAFETY: 关闭测试线程持有的隔离 clipboard。
            assert_ne!(unsafe { super::CloseClipboard() }, 0);
            let failure = cancelled.expect_err("占用期间取消不得发布");
            assert!(failure.contains("取消") && failure.contains("未复制"));
            assert_eq!(sequence_number(), expected);
            assert_eq!(read_text(isolated), "占用期间必须保留");
        },
    );
}
