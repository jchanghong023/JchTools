//! 当前会话的通知区图标、全局热键和框选窗口。消息循环独立于 Slint 与 ONNX。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc, LazyLock, Mutex};

use super::{protocol::wide, Command};
use crate::capture_win;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconFromResourceEx, DestroyIcon, GetSystemMetrics, SM_CXSMICON, SM_CYSMICON,
};

type Handle = *mut c_void;
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
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
    menu: *const u16,
    class: *const u16,
}
#[repr(C)]
struct NotifyIconData {
    cb_size: u32,
    hwnd: Handle,
    id: u32,
    flags: u32,
    callback: u32,
    icon: Handle,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    timeout: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: [u8; 16],
    balloon_icon: Handle,
}
#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(class: *const WndClass) -> u16;
    fn CreateWindowExW(
        ex: u32,
        class: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: Handle,
        menu: Handle,
        instance: Handle,
        param: *mut c_void,
    ) -> Handle;
    fn DestroyWindow(hwnd: Handle) -> i32;
    fn DefWindowProcW(hwnd: Handle, msg: u32, w: usize, l: isize) -> isize;
    fn GetMessageW(msg: *mut Msg, hwnd: Handle, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostMessageW(hwnd: Handle, msg: u32, w: usize, l: isize) -> i32;
    fn SetTimer(
        hwnd: Handle,
        id: usize,
        interval: u32,
        callback: Option<unsafe extern "system" fn(Handle, u32, usize, u32)>,
    ) -> usize;
    fn KillTimer(hwnd: Handle, id: usize) -> i32;
    fn RegisterHotKey(hwnd: Handle, id: i32, modifiers: u32, key: u32) -> i32;
    fn UnregisterHotKey(hwnd: Handle, id: i32) -> i32;
    fn CreatePopupMenu() -> Handle;
    fn AppendMenuW(menu: Handle, flags: u32, id: usize, text: *const u16) -> i32;
    fn TrackPopupMenu(
        menu: Handle,
        flags: u32,
        x: i32,
        y: i32,
        reserved: i32,
        hwnd: Handle,
        rect: *const c_void,
    ) -> i32;
    fn DestroyMenu(menu: Handle) -> i32;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn SetForegroundWindow(hwnd: Handle) -> i32;
    fn RegisterWindowMessageW(message: *const u16) -> u32;
    fn PostQuitMessage(code: i32);
}
#[link(name = "shell32")]
extern "system" {
    fn Shell_NotifyIconW(action: u32, data: *mut NotifyIconData) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(module: *const u16) -> Handle;
}
const WM_HOTKEY: u32 = 0x312;
const WM_COMMAND: u32 = 0x111;
const WM_APP_TRAY: u32 = 0x8000 + 17;
const WM_APP_CONTROL: u32 = 0x8000 + 19;
const WM_CLOSE: u32 = 0x10;
const WM_TIMER: u32 = 0x113;
const ID_ICON_RETRY: usize = 0x5455;
const ICON_RETRY_INTERVAL_MS: u32 = 250;
const ICON_RETRY_LIMIT: u8 = 5;
const ID_ACTIVE: i32 = 0x5453;
const ID_STANDBY: i32 = 0x5454;
static TASKBAR_CREATED: LazyLock<u32> = LazyLock::new(|| {
    // SAFETY: RegisterWindowMessageW 只读取传入的宽字符串；wide() 已追加 NUL 终止符，
    // 临时 Vec 存活至调用结束，注册失败仅返回 0。
    unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) }
});

fn fill<const N: usize>(buffer: &mut [u16; N], value: &str) {
    for (dst, src) in buffer.iter_mut().zip(value.encode_utf16().take(N - 1)) {
        *dst = src;
    }
}
// CreateIconFromResourceEx 要求资源起点按 DWORD 对齐；图像直接编入 worker，不依赖外部文件。
#[repr(align(4))]
struct IconPng([u8; include_bytes!("../../../../resources/app-icon.png").len()]);
static ICON_PNG: IconPng = IconPng(*include_bytes!("../../../../resources/app-icon.png"));

struct TrayIcon(Handle);
impl Drop for TrayIcon {
    fn drop(&mut self) {
        // SAFETY: 此句柄仅由 CreateIconFromResourceEx 创建，归当前托盘线程独占。
        if unsafe { DestroyIcon(self.0) } == 0 {
            tracing::warn!(event = "ocr_tray_icon_release_failed", error = %std::io::Error::last_os_error(), "释放托盘品牌图标失败");
        }
    }
}

fn load_notification_icon() -> Option<TrayIcon> {
    let size = u32::try_from(ICON_PNG.0.len()).ok()?;
    // SAFETY: 只读取 Windows 通知区使用的小图标尺寸，不修改 DPI 或系统设置。
    let width = unsafe { GetSystemMetrics(SM_CXSMICON) };
    // SAFETY: 同上，仅查询系统度量。
    let height = unsafe { GetSystemMetrics(SM_CYSMICON) };
    // SAFETY: 对齐的内置 PNG 在进程存续期间有效，API 仅读取资源字节；
    // 0x30000 为 Win32 图标资源格式，flags=0 使返回句柄由本线程负责释放。
    let icon = unsafe {
        CreateIconFromResourceEx(
            ICON_PNG.0.as_ptr().cast_mut(),
            size,
            1,
            0x0003_0000,
            width,
            height,
            0,
        )
    };
    if icon.is_null() {
        tracing::error!(event = "ocr_tray_icon_load_failed", error = %std::io::Error::last_os_error(), "内置 JchTools 托盘图标加载失败");
        None
    } else {
        Some(TrayIcon(icon))
    }
}

thread_local! {
    static NOTIFICATION_ICON: Option<TrayIcon> = load_notification_icon();
}

fn notification_icon() -> Handle {
    // 图标按托盘线程缓存一次，复用注册、通知和 Explorer 重建；线程结束时释放句柄。
    NOTIFICATION_ICON.with(|icon| icon.as_ref().map_or(null_mut(), |icon| icon.0))
}

fn notify(hwnd: Handle, message: Option<&str>, hotkey: &str, action: u32) -> bool {
    // SAFETY: NotifyIconData 是 repr(C) 纯数据结构，全零是有效初始值（句柄为 null、数值为 0）。
    let mut data: NotifyIconData = unsafe { std::mem::zeroed() };
    let Ok(size) = u32::try_from(size_of::<NotifyIconData>()) else {
        return false;
    };
    data.cb_size = size;
    data.hwnd = hwnd;
    data.id = 1;
    data.flags = 0x1 | 0x2 | 0x4;
    data.callback = WM_APP_TRAY;
    data.icon = notification_icon();
    if data.icon.is_null() && action != 2 {
        return false;
    }
    fill(&mut data.tip, &format!("JchTools 后台服务 ({hotkey})"));
    if let Some(message) = message {
        data.flags |= 0x10;
        fill(&mut data.info, message);
        fill(&mut data.info_title, "截图 OCR");
        data.info_flags = 1;
    }
    // SAFETY: data 为本函数栈上已填好的 NOTIFYICONDATAW（cb_size 已设置），
    // &raw mut 指针在调用期间独占有效。
    unsafe { Shell_NotifyIconW(action, &raw mut data) != 0 }
}

fn register_tray_icon(hwnd: Handle, hotkey: &str) -> bool {
    if notify(hwnd, None, hotkey, 0) {
        return true;
    }
    tracing::warn!(
        event = "ocr_tray_icon_registration_failed",
        error_type = "shell_notify_icon",
        "Shell_NotifyIconW(NIM_ADD) 返回失败，通知区图标尚未注册"
    );
    false
}

fn stop_icon_retry(hwnd: Handle, state: &mut TrayState) {
    // SAFETY: 只取消本线程窗口的自有定时器；尚未设置或已经取消时无副作用。
    unsafe { KillTimer(hwnd, ID_ICON_RETRY) };
    state.icon_retries_left = 0;
}

fn message_loop_requires_cleanup(code: i32) -> bool {
    code <= 0
}

fn cleanup_tray_state(hwnd: Handle, failure: Option<&str>) {
    STATE.with(|slot| {
        if let Some(mut state) = slot.borrow_mut().take() {
            stop_icon_retry(hwnd, &mut state);
            // 与投递共用同一把锁：死亡之后不能再留下无人消费的控制请求。
            let _ = state
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let _ = state.commands.send(Command::TrayStopped);
            if state.active != 0 {
                // SAFETY: state.active 是本线程成功注册的热键 ID；0 表示未注册。
                unsafe { UnregisterHotKey(hwnd, state.active) };
            }
            // SAFETY: hwnd 与图标 ID 属于本线程创建的通知区图标；删除失败不影响
            // 其余句柄清理。
            notify(hwnd, None, &state.hotkey, 2);
            if let Some(reason) = failure {
                let _ = state
                    .commands
                    .send(Command::HotkeyUnavailable(reason.to_owned()));
            }
        }
    });
}

/// 原 TextSnap 热键键域：修饰键 + 字母/数字/F1～F24/固定命名键；
/// 解析只做固定映射，注册是否可用由 Win32 RegisterHotKey 决定。
pub(crate) fn parse_hotkey(raw: &str) -> Result<(u32, u32), String> {
    let mut flags = 0x4000;
    let mut key = None;
    for part in raw.split('+') {
        let part = part.trim().to_ascii_uppercase();
        let modifier = match part.as_str() {
            "CTRL" | "CONTROL" => 2,
            "ALT" => 1,
            "SHIFT" => 4,
            "WIN" | "META" => 8,
            _ => 0,
        };
        if modifier != 0 {
            if flags & modifier != 0 {
                return Err("快捷键包含重复修饰键".into());
            }
            flags |= modifier;
            continue;
        }
        if key.is_some() {
            return Err("快捷键不能包含多个主键".into());
        }
        let bytes = part.as_bytes();
        key = if bytes.len() == 1 && (bytes[0].is_ascii_uppercase() || bytes[0].is_ascii_digit()) {
            Some(u32::from(bytes[0]))
        } else if let Some(n) = part.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
            (1..=24).contains(&n).then_some(0x6f + n)
        } else {
            match part.as_str() {
                "BACKSPACE" => Some(0x08),
                "TAB" => Some(0x09),
                "RETURN" => Some(0x0d),
                "ESCAPE" => Some(0x1b),
                "SPACE" => Some(0x20),
                "PAGEUP" => Some(0x21),
                "PAGEDOWN" => Some(0x22),
                "END" => Some(0x23),
                "HOME" => Some(0x24),
                "LEFT" => Some(0x25),
                "UP" => Some(0x26),
                "RIGHT" => Some(0x27),
                "DOWN" => Some(0x28),
                "INSERT" => Some(0x2d),
                "DELETE" => Some(0x2e),
                _ => None,
            }
        };
        if key.is_none() {
            return Err("快捷键主键不支持".into());
        }
    }
    if flags == 0x4000 || key.is_none() {
        return Err("快捷键需包含修饰键与有效主键".into());
    }
    Ok((flags, key.unwrap_or_default()))
}

enum Control {
    Trigger(tracing::Span),
    Replace(String, mpsc::SyncSender<Result<(), String>>),
    Notice(String),
    Stop(mpsc::SyncSender<()>),
}
struct TrayState {
    commands: mpsc::Sender<Command>,
    queue: Arc<Mutex<Option<VecDeque<Control>>>>,
    active: i32,
    hotkey: String,
    icon_retries_left: u8,
}
thread_local! { static STATE:RefCell<Option<TrayState>>=const { RefCell::new(None) }; }

// P3-31：仅在该测试托盘线程冻结选区交互；不捕获屏幕、不创建遮罩。
#[cfg(test)]
thread_local! {
    static REVIEW_SELECTION: RefCell<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>> =
        const { RefCell::new(None) };
}

fn capture_selected(state: &mpsc::Sender<Command>) {
    #[cfg(test)]
    if let Some((entered, release)) = REVIEW_SELECTION.with(|slot| slot.borrow_mut().take()) {
        let _ = entered.send(());
        let _ = release.recv();
        let _ = state.send(Command::CancelledSelection);
        return;
    }
    let started = std::time::Instant::now();
    tracing::info!(event = "ocr_capture_started", "开始截图与选区");
    // 完整截图发生在遮罩出现前；旧结果由 Slint 线程先隐藏，服务不主动写图片。
    let mut capture_phase = "capture";
    let outcome = capture_win::capture().and_then(|frame| {
        tracing::debug!(
            event = "ocr_screen_capture_completed",
            elapsed_ms = crate::logging::elapsed_ms(started),
            "屏幕截图已取得，仅保留内存"
        );
        capture_phase = "selection";
        tracing::debug!(event = "ocr_selection_started", "开始截图框选");
        capture_win::select(frame)
    });
    match outcome {
        Ok(Some((image, work))) => {
            tracing::info!(
                event = "ocr_capture_completed",
                result = "selected",
                width = image.width(),
                height = image.height(),
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图选区完成"
            );
            let _ = state.send(Command::Image(image, work));
        }
        Ok(None) => {
            tracing::info!(
                event = "ocr_capture_completed",
                result = "cancelled",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图选区已取消"
            );
            let _ = state.send(Command::CancelledSelection);
        }
        Err(reason) => {
            tracing::warn!(
                event = "ocr_capture_failed",
                stage = capture_phase,
                error_type = "capture",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "截图选区失败"
            );
            let _ = state.send(Command::CaptureFailed(reason));
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: Handle, msg: u32, w: usize, l: isize) -> isize {
    if msg == *TASKBAR_CREATED && msg != 0 {
        STATE.with(|slot| {
            if let Some(state) = slot.borrow_mut().as_mut() {
                stop_icon_retry(hwnd, state);
                if !register_tray_icon(hwnd, &state.hotkey) {
                    // SAFETY: hwnd 属于本线程，固定 ID 仅用于图标恢复；由窗口消息泵分发。
                    if unsafe { SetTimer(hwnd, ID_ICON_RETRY, ICON_RETRY_INTERVAL_MS, None) } != 0 {
                        state.icon_retries_left = ICON_RETRY_LIMIT;
                    } else {
                        tracing::error!(
                            event = "ocr_tray_icon_retry_failed",
                            error = %std::io::Error::last_os_error(),
                            "托盘图标恢复定时器创建失败"
                        );
                    }
                }
            }
        });
        return 0;
    }
    match msg {
        WM_TIMER if w == ID_ICON_RETRY => {
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    if state.icon_retries_left == 0 {
                        return;
                    }
                    state.icon_retries_left -= 1;
                    if register_tray_icon(hwnd, &state.hotkey) {
                        stop_icon_retry(hwnd, state);
                    } else if state.icon_retries_left == 0 {
                        stop_icon_retry(hwnd, state);
                        tracing::error!(
                            event = "ocr_tray_icon_retry_exhausted",
                            retries = ICON_RETRY_LIMIT,
                            "托盘图标注册重试已耗尽，通知区图标仍不可用"
                        );
                    }
                }
            });
            return 0;
        }
        WM_HOTKEY => {
            let commands = STATE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .and_then(|s| (usize::try_from(s.active) == Ok(w)).then(|| s.commands.clone()))
            });
            if let Some(commands) = commands {
                tracing::info!(event = "ocr_hotkey_received", "收到截图热键");
                let _ = commands.send(Command::CaptureRequested);
            }
            return 0;
        }
        WM_APP_TRAY if l == 0x205 || l == 0x204 => {
            // SAFETY: 无参数调用，仅返回菜单句柄，失败时为 null 由下方判空处理。
            let menu = unsafe { CreatePopupMenu() };
            if !menu.is_null() {
                // 每次打开菜单读取当前用户 Run 值，避免设置窗口或外部更改后的旧状态。
                let autostart = super::autostart_enabled();
                for (id, title) in [
                    (5, "打开主界面"),
                    (1, "截图识别"),
                    (2, "设置"),
                    (3, "开机启动"),
                    (4, "退出后台服务"),
                ] {
                    let text = wide(title);
                    let flags = if id == 3 && autostart { 0x0008 } else { 0 }; // MF_CHECKED
                                                                               // SAFETY: menu 已判空，text 为 wide() 生成的 NUL 结尾宽字符串，指针在调用期间存活。
                    unsafe { AppendMenuW(menu, flags, id, text.as_ptr()) };
                }
                let mut point = Point::default();
                // SAFETY: point 为栈上已初始化的 POINT，输出指针在调用期间有效。
                unsafe { GetCursorPos(&raw mut point) };
                // SAFETY: 仅按值传递窗口句柄。
                unsafe { SetForegroundWindow(hwnd) };
                // SAFETY: menu 已判空，rect 允许传 null，其余参数按值传递。
                unsafe { TrackPopupMenu(menu, 0, point.x, point.y, 0, hwnd, null()) };
                // 通知区菜单关闭后投递 WM_NULL，按 Win32 要求完成任务切换；
                // 否则第二次打开菜单会立即消失，主界面/设置/退出入口不可用。
                // SAFETY: hwnd 为本线程创建的窗口，WM_NULL 不携带指针或资源。
                unsafe { PostMessageW(hwnd, 0, 0, 0) };
                // SAFETY: menu 为本次打开的有效菜单句柄，按值传递，销毁失败仅返回 0。
                unsafe { DestroyMenu(menu) };
            }
            return 0;
        }
        WM_COMMAND => {
            STATE.with(|slot| {
                if let Some(state) = slot.borrow().as_ref() {
                    let command = match w & 0xffff {
                        5 => Some(Command::OpenMain),
                        1 => Some(Command::CaptureRequested),
                        2 => Some(Command::OpenSettings),
                        3 => Some(Command::ToggleAutostart),
                        4 => Some(Command::ExitRequested),
                        _ => None,
                    };
                    if let Some(cmd) = command {
                        let _ = state.commands.send(cmd);
                    }
                }
            });
            return 0;
        }
        WM_APP_CONTROL => {
            let queue = STATE.with(|slot| slot.borrow().as_ref().map(|s| s.queue.clone()));
            if let Some(queue) = queue {
                loop {
                    let control = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_mut()
                        .and_then(VecDeque::pop_front);
                    let Some(control) = control else {
                        break;
                    };
                    match control {
                        Control::Trigger(span) => {
                            let _entered = span.enter();
                            let sender = STATE
                                .with(|slot| slot.borrow().as_ref().map(|s| s.commands.clone()));
                            if let Some(sender) = sender {
                                capture_selected(&sender);
                            }
                        }
                        Control::Replace(name, response) => {
                            let started = std::time::Instant::now();
                            tracing::info!(
                                event = "ocr_hotkey_replace_started",
                                "开始替换截图热键"
                            );
                            let result = parse_hotkey(&name).and_then(|(mods, key)| {
                                STATE.with(|slot| {
                                    let mut slot = slot.borrow_mut();
                                    let state = slot
                                        .as_mut()
                                        .ok_or_else(|| "托盘服务已退出".to_string())?;
                                    // 同值且已注册才可幂等确认；启动冲突留下 active=0
                                    // 时必须实际重试注册，服务不能按字符串相同跳过。
                                    if name == state.hotkey && state.active != 0 {
                                        return Ok(());
                                    }
                                    let next = if state.active == ID_ACTIVE {
                                        ID_STANDBY
                                    } else {
                                        ID_ACTIVE
                                    };
                                    // SAFETY: 参数均为句柄、热键 ID 与键值按值传递，hwnd 属于本线程。
                                    if unsafe { RegisterHotKey(hwnd, next, mods, key) } == 0 {
                                        return Err("快捷键冲突；原快捷键保持有效".into());
                                    }
                                    if state.active != 0
                                        // SAFETY: 仅按值传递窗口句柄与本线程注册的热键 ID。
                                        && unsafe { UnregisterHotKey(hwnd, state.active) } == 0
                                    {
                                        // SAFETY: 仅按值传递窗口句柄与上一步刚注册的热键 ID。
                                        unsafe { UnregisterHotKey(hwnd, next) };
                                        return Err("原快捷键无法注销；设置未更改".into());
                                    }
                                    state.active = next;
                                    state.hotkey = name;
                                    notify(hwnd, None, &state.hotkey, 1);
                                    Ok(())
                                })
                            });
                            if result.is_ok() {
                                tracing::info!(
                                    event = "ocr_hotkey_replace_completed",
                                    elapsed_ms = crate::logging::elapsed_ms(started),
                                    "截图热键替换完成"
                                );
                            } else {
                                tracing::warn!(
                                    event = "ocr_hotkey_replace_failed",
                                    error_type = "registration",
                                    elapsed_ms = crate::logging::elapsed_ms(started),
                                    "截图热键替换失败，保留原状态"
                                );
                            }
                            let _ = response.send(result);
                        }
                        Control::Notice(message) => {
                            let name = STATE.with(|slot| {
                                slot.borrow()
                                    .as_ref()
                                    .map(|s| s.hotkey.clone())
                                    .unwrap_or_default()
                            });
                            notify(hwnd, Some(&message), &name, 1);
                        }
                        Control::Stop(response) => {
                            let _ = crate::capture_win::cancel_selection();
                            // SAFETY: 仅按值传递本线程创建的窗口句柄。
                            unsafe { DestroyWindow(hwnd) };
                            let _ = response.send(());
                            return 0;
                        }
                    }
                }
            }
            return 0;
        }
        WM_CLOSE => {
            // SAFETY: 仅按值传递本线程创建的窗口句柄。
            unsafe { DestroyWindow(hwnd) };
            return 0;
        }
        0x0002 => {
            cleanup_tray_state(hwnd, None);
            // SAFETY: 仅传递整数退出码，向本线程消息队列投递 WM_QUIT。
            unsafe { PostQuitMessage(0) };
            return 0;
        }
        _ => {}
    }
    // SAFETY: 参数均为按值标量，默认窗口过程只在对应窗口的线程上下文内调用。
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

#[derive(Clone)]
pub(crate) struct TrayHandle {
    hwnd: usize,
    queue: Arc<Mutex<Option<VecDeque<Control>>>>,
}

/// 托盘热键替换回执（S8-01）：2 秒内回执的同步结果，或超时后的迟到回执
/// 通道——排队的替换操作在托盘线程恢复泵消息后仍会执行，超时不得被当作
/// 失败终态，须由调用方持有通道等待迟到回执再裁决。
pub(crate) enum HotkeyReceipt {
    /// 托盘线程在时限内完成并回执。
    Done(Result<(), String>),
    /// 时限内未回执；接管接收端继续等待（有界通道容量 1，托盘稍后的
    /// send 仍会成功入队，不会丢失回执）。
    Late(mpsc::Receiver<Result<(), String>>),
}

impl TrayHandle {
    fn send(&self, command: Control) -> Result<(), ()> {
        let mut slot = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let queue = slot.as_mut().ok_or(())?;
        queue.push_back(command);
        // SAFETY: 仅按值传递本托盘句柄与常量；与死亡清理互斥，避免向复用的句柄投递。
        if unsafe { PostMessageW(self.hwnd as Handle, WM_APP_CONTROL, 0, 0) } == 0 {
            // 持锁期间窗口线程不能取走请求，撤回本次投递而不是留下迟到副作用。
            let _ = queue.pop_back();
            return Err(());
        }
        Ok(())
    }
    /// 成功仅表示已投递；终止由 TrayStopped 通知，不为合法框选设置时限。
    pub(crate) fn trigger(&self, span: tracing::Span) -> Result<(), ()> {
        self.send(Control::Trigger(span))
    }
    /// 请求当前冻结框选结束；服务取消/退出路径可在不阻塞托盘线程的情况下调用。
    pub(crate) fn cancel_selection() -> bool {
        crate::capture_win::cancel_selection()
    }
    pub(crate) fn notice(&self, text: impl Into<String>) {
        let _ = self.send(Control::Notice(text.into()));
    }
    /// 请求替换热键：等待 2 秒；超时不报失败，改为交出迟到回执通道（S8-01）。
    pub(crate) fn replace(&self, name: String) -> HotkeyReceipt {
        let (tx, rx) = mpsc::sync_channel(1);
        if self.send(Control::Replace(name, tx)).is_err() {
            return HotkeyReceipt::Done(Err("托盘服务已退出或无法投递热键更新".into()));
        }
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(result) => HotkeyReceipt::Done(result),
            Err(_) => HotkeyReceipt::Late(rx),
        }
    }
    /// 只排队替换、不等待回执（S8-01 回执超时后的自愈路径：FIFO 重排当前
    /// 期望的热键；托盘侧对「已是当前热键」的替换直接回成功，幂等）。
    pub(crate) fn replace_detached(&self, name: String) {
        let (tx, _rx) = mpsc::sync_channel(1);
        let _ = self.send(Control::Replace(name, tx));
    }
    pub(crate) fn stop(&self) {
        let _ = Self::cancel_selection();
        let (tx, rx) = mpsc::sync_channel(1);
        if self.send(Control::Stop(tx)).is_ok() {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
impl TrayHandle {
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
    pub(super) fn for_test() -> Self {
        Self {
            hwnd: 0,
            queue: Arc::new(Mutex::new(Some(VecDeque::new()))),
        }
    }
}

// P3-31：只创建 message-only 窗口；不注册全局热键、不接触通知区、不抢焦点。
// 门控发生在真实消息循环前，可重现已经排队但尚未交付的 Trigger。
#[cfg(test)]
pub(super) struct ReviewTray {
    pub(super) handle: TrayHandle,
    release: Option<mpsc::SyncSender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    selecting: bool,
    selection_release: Option<mpsc::SyncSender<()>>,
    selection_entered: mpsc::Receiver<()>,
}

#[cfg(test)]
impl ReviewTray {
    pub(super) fn start(commands: mpsc::Sender<Command>) -> Result<Self, String> {
        Self::start_inner(commands, false)
    }

    pub(super) fn selecting(commands: mpsc::Sender<Command>) -> Result<Self, String> {
        Self::start_inner(commands, true)
    }

    fn start_inner(commands: mpsc::Sender<Command>, selecting: bool) -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::sync_channel(1);
        let (selection_started, selection_entered) = mpsc::sync_channel(1);
        let (selection_release, selection_wait) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let class_name = wide("JchToolsReviewRemediationTray");
            // SAFETY: null 只查询当前模块。
            let instance = unsafe { GetModuleHandleW(null()) };
            let class = WndClass {
                style: 0,
                proc: Some(wnd_proc),
                cls_extra: 0,
                wnd_extra: 0,
                instance,
                icon: null_mut(),
                cursor: null_mut(),
                background: null_mut(),
                menu: null(),
                class: class_name.as_ptr(),
            };
            // SAFETY: 类名和结构在调用期间有效；已注册时复用同一窗口过程。
            unsafe { RegisterClassW(&raw const class) };
            // SAFETY: HWND_MESSAGE(-3) 创建不可见且没有用户桌面交互的消息窗口。
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    class_name.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    -3isize as Handle,
                    null_mut(),
                    instance,
                    null_mut(),
                )
            };
            if hwnd.is_null() {
                let _ = ready_tx.send(Err("隔离消息窗口创建失败".to_owned()));
                return;
            }
            let queue = Arc::new(Mutex::new(Some(VecDeque::new())));
            STATE.with(|slot| {
                *slot.borrow_mut() = Some(TrayState {
                    commands,
                    queue: queue.clone(),
                    active: 0,
                    hotkey: String::new(),
                    icon_retries_left: 0,
                });
            });
            if selecting {
                REVIEW_SELECTION.with(|slot| {
                    *slot.borrow_mut() = Some((selection_started, selection_wait));
                });
            }
            let _ = ready_tx.send(Ok(TrayHandle {
                hwnd: hwnd as usize,
                queue,
            }));
            if selecting {
                loop {
                    // SAFETY: Msg 是纯数据输出缓冲，GetMessageW 在本线程填充。
                    let mut msg = unsafe { std::mem::zeroed::<Msg>() };
                    // SAFETY: 只接收本线程消息，缓冲有效。
                    if unsafe { GetMessageW(&raw mut msg, null_mut(), 0, 0) } <= 0 {
                        break;
                    }
                    // SAFETY: 消息来自本线程 GetMessageW，目标为本线程窗口。
                    unsafe { DispatchMessageW(&raw const msg) };
                }
            } else {
                let _ = gate.recv();
                // 与 GetMessageW 返回 -1 的生产路径相同；不消费排队中的 Trigger。
                cleanup_tray_state(hwnd, Some("托盘消息循环失败；快捷键与托盘已清理"));
                // SAFETY: 本线程独占创建的 message-only 窗口，仅销毁一次。
                unsafe { DestroyWindow(hwnd) };
            }
        });
        let handle = ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())??;
        Ok(Self {
            handle,
            release: Some(release),
            thread: Some(thread),
            selecting,
            selection_release: Some(selection_release),
            selection_entered,
        })
    }

    /// 只填满隔离线程的原生消息队列，令下一次真实 PostMessageW 返回失败。
    pub(super) fn fill_message_queue(&self) -> Result<(), String> {
        if self.selecting {
            return Err("只能填充冻结的隔离消息窗口".into());
        }
        for _ in 0..=262_144 {
            // SAFETY: 仅投递无副作用的 WM_NULL 到本测试独占的 message-only 窗口。
            if unsafe { PostMessageW(self.handle.hwnd as Handle, 0, 0, 0) } == 0 {
                return Ok(());
            }
        }
        Err("隔离原生消息队列未达到投递上限".into())
    }

    pub(super) fn wait_selection(&self) -> Result<(), String> {
        self.selection_entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())
    }

    pub(super) fn finish_selection(&mut self) {
        if let Some(release) = self.selection_release.take() {
            let _ = release.send(());
        }
    }

    pub(super) fn terminate(&mut self) -> Result<(), String> {
        self.finish_selection();
        if self.selecting && self.thread.is_some() {
            // SAFETY: 只向本测试创建的 message-only 窗口请求关闭。
            unsafe { PostMessageW(self.handle.hwnd as Handle, WM_CLOSE, 0, 0) };
        }
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| "隔离托盘线程异常".to_owned())?;
        }
        Ok(())
    }
}

#[cfg(test)]
impl Drop for ReviewTray {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

pub(crate) fn start(
    sender: mpsc::Sender<Command>,
    hotkey: String,
    autostart: bool,
) -> Result<TrayHandle, String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let span = tracing::Span::current();
    std::thread::spawn(move || {
        let _entered = span.enter();
        let started = std::time::Instant::now();
        tracing::info!(event = "ocr_tray_started", "开始初始化截图托盘");
        let class_name = wide("JchToolsSnapOcrTray");
        // SAFETY: 模块名传 null 表示取当前进程可执行文件的句柄，不解引用任何指针。
        let instance = unsafe { GetModuleHandleW(null()) };
        let wnd = WndClass {
            style: 0,
            proc: Some(wnd_proc),
            cls_extra: 0,
            wnd_extra: 0,
            instance,
            icon: null_mut(),
            cursor: null_mut(),
            background: null_mut(),
            menu: null(),
            class: class_name.as_ptr(),
        };
        // SAFETY: wnd 为栈上已填好的 WNDCLASSW，其 class 字段指向仍存活的 class_name（NUL 结尾宽字符串）。
        if unsafe { RegisterClassW(&raw const wnd) } == 0 {
            tracing::error!(
                event = "ocr_tray_failed",
                stage = "register_class",
                error_type = "win32",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "托盘窗口类型注册失败"
            );
            let _ = ready_tx.send(Err("托盘窗口类型注册失败".into()));
            return;
        }
        // SAFETY: class 与 title 均指向仍存活的 class_name（NUL 结尾宽字符串），
        // 其余句柄为 null 或当前实例句柄，lpParam 为 null。
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                class_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            )
        };
        if hwnd.is_null() {
            tracing::error!(
                event = "ocr_tray_failed",
                stage = "create_window",
                error_type = "win32",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "托盘窗口创建失败"
            );
            let _ = ready_tx.send(Err("托盘消息窗口创建失败".into()));
            return;
        }
        let (modifiers, key) = match parse_hotkey(&hotkey) {
            Ok(keys) => keys,
            Err(reason) => {
                tracing::error!(
                    event = "ocr_tray_failed",
                    stage = "parse_hotkey",
                    error_type = "invalid_hotkey",
                    elapsed_ms = crate::logging::elapsed_ms(started),
                    "托盘热键配置无效"
                );
                let _ = ready_tx.send(Err(reason));
                // SAFETY: 仅按值传递本线程刚创建的窗口句柄，销毁失败仅返回非零。
                unsafe { DestroyWindow(hwnd) };
                return;
            }
        };
        // SAFETY: 参数均为句柄、热键 ID 与键值按值传递，hwnd 属于本线程。
        let hotkey_registered = unsafe { RegisterHotKey(hwnd, ID_ACTIVE, modifiers, key) } != 0;
        if !hotkey_registered {
            tracing::warn!(
                event = "ocr_hotkey_unavailable",
                stage = "initial_registration",
                error_type = "registration",
                "截图热键不可用，后台仍运行"
            );
            let _ = sender.send(Command::HotkeyUnavailable(
                "快捷键被占用，请在截图 OCR 页设置其他组合；后台服务仍在运行".into(),
            ));
        }
        let queue = Arc::new(Mutex::new(Some(VecDeque::new())));
        STATE.with(|slot| {
            *slot.borrow_mut() = Some(TrayState {
                commands: sender,
                queue: queue.clone(),
                active: if hotkey_registered { ID_ACTIVE } else { 0 },
                hotkey: hotkey.clone(),
                icon_retries_left: 0,
            });
        });
        let mut icon_registered = register_tray_icon(hwnd, &hotkey);
        for _ in 0..ICON_RETRY_LIMIT {
            let interval = std::time::Duration::from_millis(u64::from(ICON_RETRY_INTERVAL_MS));
            if icon_registered || started.elapsed() + interval >= std::time::Duration::from_secs(5)
            {
                break;
            }
            // 仅初始化阶段等待；最多五次，且不越过调用方原有的五秒初始化预算。
            std::thread::sleep(interval);
            icon_registered = register_tray_icon(hwnd, &hotkey);
        }
        if !icon_registered {
            tracing::error!(
                event = "ocr_tray_failed",
                stage = "notify_icon",
                error_type = "win32",
                elapsed_ms = crate::logging::elapsed_ms(started),
                "托盘图标创建失败，有限注册重试已结束"
            );
            let _ = ready_tx.send(Err("通知区图标创建失败；截图服务未启动".into()));
            // SAFETY: 仅按值传递本线程刚创建的窗口句柄，销毁失败仅返回非零。
            unsafe { DestroyWindow(hwnd) };
            return;
        }
        if !autostart && hotkey_registered {
            let _ = notify(hwnd, Some(&format!("已启动，按 {hotkey} 截图")), &hotkey, 1);
        }
        tracing::info!(
            event = "ocr_tray_ready",
            hotkey_registered,
            elapsed_ms = crate::logging::elapsed_ms(started),
            "截图托盘就绪"
        );
        let _ = ready_tx.send(Ok(TrayHandle {
            hwnd: hwnd as usize,
            queue,
        }));
        loop {
            // SAFETY: Msg 为纯数据 C 结构体，全零是有效初始值，字段随后由 GetMessageW 填写。
            let mut msg = unsafe { std::mem::zeroed::<Msg>() };
            // SAFETY: msg 为栈上有效输出缓冲，指针在调用期间有效；hwnd 传 null 表示取本线程全部消息。
            let code = unsafe { GetMessageW(&raw mut msg, null_mut(), 0, 0) };
            if message_loop_requires_cleanup(code) {
                if code < 0 {
                    tracing::error!(
                        event = "ocr_tray_failed",
                        stage = "message_loop",
                        error_type = "win32",
                        "托盘消息循环失败"
                    );
                    cleanup_tray_state(hwnd, Some("托盘消息循环失败；快捷键与托盘已清理"));
                } else {
                    cleanup_tray_state(hwnd, None);
                }
                break;
            }
            // SAFETY: msg 已由 GetMessageW 填成有效消息结构，只读指针在调用期间有效。
            unsafe { TranslateMessage(&raw const msg) };
            // SAFETY: msg 为同一有效消息结构，只读指针在调用期间有效。
            unsafe { DispatchMessageW(&raw const msg) };
        }
        tracing::info!(event = "ocr_tray_stopped", "截图托盘线程已停止");
    });
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "托盘初始化超时".to_string())?
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
mod tests {
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
            DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC,
            BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};
        struct IconBitmaps(ICONINFO);
        impl Drop for IconBitmaps {
            fn drop(&mut self) {
                // SAFETY: GetIconInfo 创建的两个位图归本测试独占，非空时各释放一次。
                if !self.0.hbmColor.is_null() { unsafe { DeleteObject(self.0.hbmColor) }; }
                // SAFETY: 同上，掩码位图由 GetIconInfo 分配。
                if !self.0.hbmMask.is_null() { unsafe { DeleteObject(self.0.hbmMask) }; }
            }
        }
        // SAFETY: ICONINFO 是纯 Win32 输出结构，全零有效。
        let mut info = IconBitmaps(unsafe { std::mem::zeroed() });
        // SAFETY: 图标句柄来自生产加载路径，输出结构有效；不注册通知区、热键或窗口。
        assert_ne!(unsafe { GetIconInfo(super::notification_icon(), &raw mut info.0) }, 0);
        // SAFETY: BITMAP 是纯输出结构，全零有效。
        let mut bitmap: BITMAP = unsafe { std::mem::zeroed() };
        // SAFETY: 只读取 GetIconInfo 复制出的彩色位图，缓冲大小与结构一致。
        assert_ne!(unsafe { GetObjectW(info.0.hbmColor, i32::try_from(std::mem::size_of::<BITMAP>()).unwrap(), (&raw mut bitmap).cast()) }, 0);
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
        let rows = unsafe { GetDIBits(dc, info.0.hbmColor, 0, u32::try_from(height).unwrap(), pixels.as_mut_ptr().cast(), &raw mut dib, DIB_RGB_COLORS) };
        // SAFETY: 归还本测试 GetDC 获得的同一 DC。
        unsafe { ReleaseDC(std::ptr::null_mut(), dc) };
        assert_eq!(usize::try_from(rows).unwrap(), height);
        let pixel = |x: usize, y: usize| {
            let index = ((y * height / 16) * width + x * width / 16) * 4;
            &pixels[index..index + 4]
        };
        let blue = pixel(4, 4); // 品牌圆角矩形内部的靛蓝色。
        assert!(blue[0] > 180 && blue[1] < 180 && blue[2] < 120 && blue[3] > 200,
            "必须显示 JchTools 品牌蓝底，而非 Windows 通用图标：{blue:?}");
        for point in [(10, 5), (6, 10)] { // 白色 J 的竖笔与横笔。
            let white = pixel(point.0, point.1);
            assert!(white[..3].iter().all(|channel| *channel > 220) && white[3] > 200,
                "品牌白色 J 必须完整可见：{point:?}={white:?}");
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
            DispatchMessageW, PeekMessageW, SendMessageW, SetWindowLongPtrW,
            GWLP_WNDPROC, MSG, PM_REMOVE, WM_TIMER,
        };

        std::thread::spawn(|| {
            #[link(name = "user32")]
            extern "system" {
                fn CreateDesktopW(
                    name: *const u16, device: *const u16, mode: *const std::ffi::c_void,
                    flags: u32, access: u32, security: *const std::ffi::c_void,
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
            let name = wide(&format!("JchToolsTrayRecovery-{}-{thread}", std::process::id()));
            // SAFETY: 创建未显示的测试 desktop；字符串存活，安全属性使用默认值。
            let desktop = unsafe { CreateDesktopW(name.as_ptr(), null(), null(), 0, 0x01ff, null()) };
            assert!(!desktop.is_null(), "必须成功创建隔离 desktop，不能降级到用户桌面");
            let mut isolated = IsolatedDesktop { previous, desktop, window: null_mut() };
            // SAFETY: 新线程尚无窗口或 hook，只绑定本线程，不调用 SwitchDesktop。
            assert_ne!(unsafe { SetThreadDesktop(desktop) }, 0);
            let class = wide("STATIC");
            // SAFETY: 系统 STATIC 类和 HWND_MESSAGE(-3) 仅创建本线程的隐藏消息窗口。
            isolated.window = unsafe {
                super::CreateWindowExW(
                    0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0,
                    -3isize as Handle, null_mut(), null_mut(), null_mut(),
                )
            };
            assert!(!isolated.window.is_null(), "必须创建隔离消息窗口");
            // SAFETY: 本线程拥有窗口，回调使用正确 Win32 ABI，存活至窗口销毁。
            assert_ne!(unsafe {
                SetWindowLongPtrW(isolated.window, GWLP_WNDPROC, super::wnd_proc as *const () as isize)
            }, 0);
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
            assert!(!notify(isolated.window, None, "Ctrl+Alt+O", 0),
                "隔离 desktop 无 Explorer，真实 NIM_ADD 必须失败");
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
                if unsafe { PeekMessageW(&raw mut message, isolated.window, WM_TIMER, WM_TIMER, PM_REMOVE) } != 0 {
                    // SAFETY: 消息来自该窗口原生队列，执行原生定时器恢复路径。
                    unsafe { DispatchMessageW(&raw const message) };
                    retried = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(retried, "TaskbarCreated 的真实 NIM_ADD 失败后必须安排原生定时重试，不能静默永久失图");
        }).join().expect("隔离托盘恢复回归失败");
    }
}
