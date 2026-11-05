//! 剪贴板回归：独立 window station 中的文本、PNG 与迟到发布保护。
// 不读取、清空或恢复当前用户 station 的任何剪贴板格式。

use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::service::clipboard::{copy_png, sequence_number};
use crate::service::CopyState;
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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|unit| u16::from_ne_bytes([unit[0], unit[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16(&units).expect("合成文本必须是有效 UTF-16")
}

fn synthetic_png() -> crate::image_copy::CompressedImage {
    let image =
        crate::capture_win::BgrImage::from_vec(1, 1, vec![160, 80, 20]).expect("合成 BGR 像素");
    crate::image_copy::compress(&image, usize::MAX, || false).expect("合成 PNG 必须能编码")
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
            copy_png(&png, sequence_number(), &Mutex::new(CopyState::Active))
                .expect("PNG 必须复制成功");
            let name = super::protocol::wide("PNG");
            // SAFETY: NUL 终止的格式名，仅注册名称，不访问用户剪贴板。
            let format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
            assert_ne!(format, 0);
            let actual = read_clipboard(isolated, format);
            assert!(actual.len() >= png.byte_len());
            // GlobalSize 可包含分配器尾部容量，完整 PNG 的编码字节必须逐字节不变。
            assert_eq!(&actual[..png.byte_len()], png.png.as_slice());
            let decoded = image::load_from_memory(&actual).expect("剪贴板 PNG 必须能直接解码");
            assert_eq!(decoded.to_rgba8().as_raw(), &[20, 80, 160, 255]);
        },
    );
}

#[test]
fn ordinary_image_paste_receives_final_pixels_and_padded_dib_rows() {
    // O-38/O-42：普通图片接收端需要标准 DIB，且必须与压缩后的 PNG 一致。
    with_isolated_clipboard(
        "service::tests::clipboard::ordinary_image_paste_receives_final_pixels_and_padded_dib_rows",
        |isolated| {
            for (width, height, shrink) in [(3, 2, false), (160, 160, true)] {
                let mut noise = 0x9e37_79b9_u32;
                let pixels = (0..width * height * 3)
                    .map(|_| {
                        noise ^= noise << 13;
                        noise ^= noise >> 17;
                        noise ^= noise << 5;
                        noise.to_le_bytes()[0]
                    })
                    .collect();
                let source = crate::capture_win::BgrImage::from_vec(width, height, pixels)
                    .expect("合成彩色选区");
                let original = crate::image_copy::compress(&source, usize::MAX, || false)
                    .expect("编码合成选区");
                let limit = if shrink {
                    original.byte_len() / 2
                } else {
                    original.byte_len()
                };
                let output =
                    crate::image_copy::compress(&source, limit, || false).expect("按实际上限压缩");
                if shrink {
                    assert!(
                        output.width < original.width || output.height < original.height,
                        "本例必须覆盖缩图后的兼容位图，而非原图"
                    );
                }
                copy_png(&output, sequence_number(), &Mutex::new(CopyState::Active))
                    .expect("发布最终图片");
                assert_ne!(
                    // SAFETY: 只查询本用例独立 station 的标准图片格式。
                    unsafe {
                        windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable(8)
                    },
                    0,
                    "复制成功后必须提供普通图片接收端可读取的 CF_DIB"
                );
                let dib = read_clipboard(isolated, 8);
                let read_u32 = |offset| {
                    u32::from_le_bytes(dib[offset..offset + 4].try_into().expect("完整字段"))
                };
                assert_eq!(read_u32(0), 40);
                assert_eq!(read_u32(4), output.width);
                assert_eq!(read_u32(8), output.height);
                assert_eq!(&dib[12..16], &[1, 0, 24, 0]);
                assert_eq!(read_u32(16), 0, "标准 BI_RGB，不嵌入 PNG 编码");
                let row_bytes = output.width as usize * 3;
                let stride = (row_bytes + 3) & !3;
                let height = output.height as usize;
                assert_eq!(read_u32(20) as usize, stride * height);
                assert!(dib.len() >= 40 + stride * height);
                let decoded = image::load_from_memory(&output.png)
                    .expect("最终 PNG 可解码")
                    .to_rgb8();
                for (row, rgb) in decoded.as_raw().chunks_exact(row_bytes).enumerate() {
                    let offset = 40 + (height - 1 - row) * stride;
                    for (column, pixel) in rgb.as_chunks::<3>().0.iter().enumerate() {
                        let offset = offset + column * 3;
                        assert_eq!(&dib[offset..offset + 3], &[pixel[2], pixel[1], pixel[0]]);
                    }
                    assert!(dib[offset + row_bytes..offset + stride]
                        .iter()
                        .all(|byte| *byte == 0));
                }
            }
        },
    );
}

#[test]
fn copy_fix_published_png_waits_for_receipt_despite_cancel() {
    with_isolated_clipboard(
        "service::tests::clipboard::copy_fix_published_png_waits_for_receipt_despite_cancel",
        |isolated| {
            let temp = tempfile::tempdir().expect("创建服务测试目录");
            let (mut service, work_rx) = super::ready_busy_service(temp.path());
            let (commands, events) = mpsc::channel();
            service.commands = commands;
            let state = Arc::new(Mutex::new(CopyState::Active));
            let png = synthetic_png();
            let expected = sequence_number();
            service.phase = super::ActivityPhase::Copying;
            service.copy_task = Some(super::CopyTask {
                generation: 41,
                max_image_bytes: 200_000,
                sequence: expected,
                state: Arc::clone(&state),
            });
            service.copy_jobs.insert(41);

            copy_png(&png, expected, &state).expect("实际发布最终 PNG");
            assert!(matches!(
                *state.lock().expect("读取实际发布状态"),
                CopyState::Published
            ));
            service
                .commands
                .send(super::Command::CopyFinished(41, Ok(png.byte_len())))
                .expect("完成回执留在队列，尚未交给服务");
            let published_sequence = sequence_number();
            service.cancel_recognition();
            assert_eq!(
                service.phase,
                super::ActivityPhase::Copying,
                "发布完成后取消不能抢在完成回执前把任务结束为取消"
            );
            assert_eq!(
                service.copy_task.as_ref().map(|task| task.generation),
                Some(41),
                "已发布任务必须保留到自身完成回执"
            );
            assert!(!service.cancel.load(std::sync::atomic::Ordering::Acquire));
            assert!(service.copy_jobs.contains(&41));
            assert!(matches!(
                *state.lock().expect("读取取消后的发布状态"),
                CopyState::Published
            ));

            service.handle(events.try_recv().expect("取得已排队完成回执"));
            assert_eq!(service.phase, super::ActivityPhase::Idle);
            assert!(service.copy_task.is_none());
            assert!(service.copy_jobs.is_empty());
            assert!(matches!(
                *state.lock().expect("读取完成后的发布状态"),
                CopyState::Published
            ));
            assert!(matches!(work_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            assert_eq!(sequence_number(), published_sequence);
            let name = super::protocol::wide("PNG");
            // SAFETY: 仅注册格式名；读取只发生在本用例的独立 station。
            let format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
            assert_ne!(format, 0);
            let actual = read_clipboard(isolated, format);
            assert!(actual.len() >= png.byte_len());
            assert_eq!(&actual[..png.byte_len()], png.png.as_slice());
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
            let stale = copy_png(&png, expected, &Mutex::new(CopyState::Active))
                .expect_err("旧任务不得覆盖新内容");
            assert!(stale.contains("新内容") && stale.contains("未复制"));
            assert_eq!(sequence_number(), newer);
            assert_eq!(read_text(isolated), "用户新复制的内容");
            let cancelled =
                copy_png(&png, newer, &Mutex::new(CopyState::Cancelled)).expect_err("取消不得发布");
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
            let worker_png = synthetic_png();
            let (result_tx, result_rx) = mpsc::channel();
            let started = Instant::now();
            let worker = std::thread::spawn(move || {
                let result = copy_png(&worker_png, expected, &Mutex::new(CopyState::Active));
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

            // 锁住发布状态门，在后台线程尚不能通过检查时取消；释放占用后仍不得发布。
            let state = Arc::new(Mutex::new(CopyState::Active));
            let mut current = state.lock().expect("锁定任务发布状态");
            // SAFETY: 再次只打开隔离 clipboard。
            assert_ne!(unsafe { super::OpenClipboard(isolated.window) }, 0);
            let worker_state = Arc::clone(&state);
            let (ready_tx, ready_rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                ready_tx.send(()).expect("通知后台复制开始");
                copy_png(&png, expected, &worker_state)
            });
            ready_rx.recv().expect("后台复制线程已开始");
            *current = CopyState::Cancelled;
            drop(current);
            let cancelled = worker.join().expect("取消测试线程正常结束");
            // SAFETY: 关闭测试线程持有的隔离 clipboard。
            assert_ne!(unsafe { super::CloseClipboard() }, 0);
            let failure = cancelled.expect_err("占用期间取消不得发布");
            assert!(failure.contains("取消") && failure.contains("未复制"));
            assert_eq!(sequence_number(), expected);
            assert_eq!(read_text(isolated), "占用期间必须保留");
            assert!(matches!(
                *state.lock().expect("读取取消状态"),
                CopyState::Cancelled
            ));
        },
    );
}
