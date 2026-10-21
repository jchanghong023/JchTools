//! //! 剪贴板回归：独立 window station 中的复制与逐字符读回。
//! 从 `service.rs` 的内联测试模块原样搬移（仅调整 use 导入与自引用测试名）。

// 覆盖 O-22：在独立非交互 window station 的剪贴板复制并读回，
// 不读取、清空或恢复当前用户 station 的任何剪贴板格式。
#[test]
fn copy_all_round_trips_on_isolated_clipboard() {
    const CHILD_FLAG: &str = "JCHTOOLS_ISOLATED_CLIPBOARD_TEST_CHILD";
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    #[link(name = "user32")]
    extern "system" {
        fn GetProcessWindowStation() -> *mut c_void;
        fn CreateWindowStationW(
            name: *const u16,
            flags: u32,
            access: u32,
            security: *const c_void,
        ) -> *mut c_void;
        fn SetProcessWindowStation(station: *mut c_void) -> i32;
        fn CloseWindowStation(station: *mut c_void) -> i32;
        fn GetThreadDesktop(thread: u32) -> *mut c_void;
        fn SetThreadDesktop(desktop: *mut c_void) -> i32;
        fn CloseDesktop(desktop: *mut c_void) -> i32;
        fn CreateDesktopW(
            name: *const u16,
            device: *const u16,
            mode: *const c_void,
            flags: u32,
            access: u32,
            security: *const c_void,
        ) -> *mut c_void;
        fn CreateWindowExW(
            ex: u32,
            class: *const u16,
            title: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: *mut c_void,
            menu: *mut c_void,
            instance: *mut c_void,
            param: *mut c_void,
        ) -> *mut c_void;
        fn DestroyWindow(window: *mut c_void) -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalSize(memory: *mut c_void) -> usize;
        fn GetCurrentThreadId() -> u32;
    }
    struct IsolatedClipboard {
        previous: *mut c_void,
        station: *mut c_void,
        previous_desktop: *mut c_void,
        desktop: *mut c_void,
        window: *mut c_void,
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
    if std::env::var_os(CHILD_FLAG).as_deref() != Some(std::ffi::OsStr::new("1")) {
        // station 是进程级状态，必须用仅运行此用例的独立测试进程，
        // 避免完整测试集并行时改变其他线程的窗口与剪贴板环境。
        let output =
            std::process::Command::new(std::env::current_exe().expect("必须定位当前测试程序"))
                .args([
                    "--exact",
                    "service::tests::clipboard::copy_all_round_trips_on_isolated_clipboard",
                    "--test-threads=1",
                ])
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
    let station = unsafe { CreateWindowStationW(null(), 0, 0x037f, null()) };
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
    // SAFETY: 在已切换的独立 station 中创建测试 desktop，字符串有效；
    // DESKTOP_ALL_ACCESS(0x01ff) 用于本线程隐藏窗口及消息队列。
    isolated.desktop =
        unsafe { CreateDesktopW(desktop_name.as_ptr(), null(), null(), 0, 0x01ff, null()) };
    assert!(!isolated.desktop.is_null(), "无法创建隔离 desktop");
    // SAFETY: 当前测试线程尚无窗口或hook，可绑定刚创建的有效 desktop。
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
            0x8000_0000,
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
    let text = "截图 OCR 合成文本  ABC\n  缩进 😀";
    let copied = super::copy_text(text);
    assert!(copied.is_ok(), "复制全部必须成功：{copied:?}");
    // SAFETY: clipboard 属于已切换的独立 station，窗口是本线程有效 owner。
    let opened = unsafe { super::OpenClipboard(isolated.window) };
    assert_ne!(opened, 0, "无法读取隔离剪贴板");
    // SAFETY: 本线程已打开隔离剪贴板，查询 CF_UNICODETEXT。
    let memory = unsafe { GetClipboardData(13) };
    let actual = if memory.is_null() {
        None
    } else {
        // SAFETY: CF_UNICODETEXT 使用全局内存，剪贴板保持打开期间有效。
        let size = unsafe { GlobalSize(memory) };
        // SAFETY: 同一有效全局内存句柄，锁定后只读。
        let data = unsafe { super::GlobalLock(memory) };
        if data.is_null() {
            None
        } else {
            // SAFETY: size 为该块字节容量，按完整 u16 单元只读，不越界。
            let units = unsafe { std::slice::from_raw_parts(data.cast::<u16>(), size / 2) };
            let length = units
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(units.len());
            let text = String::from_utf16(&units[..length]).ok();
            // SAFETY: 解锁刚锁定的剪贴板内存，不释放系统所有物。
            unsafe { super::GlobalUnlock(memory) };
            text
        }
    };
    // SAFETY: 配对关闭本线程打开的隔离剪贴板。
    unsafe { super::CloseClipboard() };
    assert_eq!(actual.as_deref(), Some(text), "剪贴板正文必须逐字符保持");
}
