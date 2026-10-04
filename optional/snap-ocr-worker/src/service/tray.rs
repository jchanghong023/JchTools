//! 当前会话的通知区图标、全局热键和框选窗口。消息循环独立于 Slint 与 ONNX。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc, LazyLock, Mutex};

use super::{protocol::wide, Command};
use crate::capture_win;

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
    fn RegisterHotKey(hwnd: Handle, id: i32, modifiers: u32, key: u32) -> i32;
    fn UnregisterHotKey(hwnd: Handle, id: i32) -> i32;
    fn LoadIconW(instance: Handle, name: *const u16) -> Handle;
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
    // SAFETY: 实例句柄为 null、名称为系统预定义常量 IDI_APPLICATION(32512)，
    // 按 MAKEINTRESOURCE 语义传整数标识，LoadIconW 不解引用该指针。
    data.icon = unsafe { LoadIconW(null_mut(), 32512usize as *const u16) };
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

fn message_loop_requires_cleanup(code: i32) -> bool {
    code <= 0
}

fn cleanup_tray_state(hwnd: Handle, failure: Option<&str>) {
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().take() {
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
    Trigger,
    Replace(String, mpsc::SyncSender<Result<(), String>>),
    Notice(String),
    Stop(mpsc::SyncSender<()>),
}
struct TrayState {
    commands: mpsc::Sender<Command>,
    queue: Arc<Mutex<VecDeque<Control>>>,
    active: i32,
    hotkey: String,
}
thread_local! { static STATE:RefCell<Option<TrayState>>=const { RefCell::new(None) }; }

fn capture_selected(state: &mpsc::Sender<Command>) {
    // 完整截图发生在遮罩出现前；旧结果由 Slint 线程先隐藏，服务不主动写图片。
    let outcome = capture_win::capture().and_then(capture_win::select);
    match outcome {
        Ok(Some((image, work))) => {
            let _ = state.send(Command::Image(image, work));
        }
        Ok(None) => {
            let _ = state.send(Command::CancelledSelection);
        }
        Err(reason) => {
            let _ = state.send(Command::CaptureFailed(reason));
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: Handle, msg: u32, w: usize, l: isize) -> isize {
    if msg == *TASKBAR_CREATED && msg != 0 {
        STATE.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                let _ = notify(hwnd, None, &state.hotkey, 0);
            }
        });
        return 0;
    }
    match msg {
        WM_HOTKEY => {
            let commands = STATE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .and_then(|s| (usize::try_from(s.active) == Ok(w)).then(|| s.commands.clone()))
            });
            if let Some(commands) = commands {
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
                        .pop_front();
                    let Some(control) = control else {
                        break;
                    };
                    match control {
                        Control::Trigger => {
                            let sender = STATE
                                .with(|slot| slot.borrow().as_ref().map(|s| s.commands.clone()));
                            if let Some(sender) = sender {
                                capture_selected(&sender);
                            }
                        }
                        Control::Replace(name, response) => {
                            let result = parse_hotkey(&name).and_then(|(mods, key)| {
                                STATE.with(|slot| {
                                    let mut slot = slot.borrow_mut();
                                    let state = slot
                                        .as_mut()
                                        .ok_or_else(|| "托盘服务已退出".to_string())?;
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
    queue: Arc<Mutex<VecDeque<Control>>>,
}
impl TrayHandle {
    fn send(&self, command: Control) {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(command);
        // SAFETY: 仅按值传递句柄与常量；窗口已销毁时 PostMessageW 仅返回 0，不产生未定义行为。
        unsafe {
            PostMessageW(self.hwnd as Handle, WM_APP_CONTROL, 0, 0);
        }
    }
    pub(crate) fn trigger(&self) {
        self.send(Control::Trigger);
    }
    /// 请求当前冻结框选结束；服务取消/退出路径可在不阻塞托盘线程的情况下调用。
    pub(crate) fn cancel_selection() -> bool {
        crate::capture_win::cancel_selection()
    }
    pub(crate) fn notice(&self, text: impl Into<String>) {
        self.send(Control::Notice(text.into()));
    }
    pub(crate) fn replace(&self, name: String) -> Result<(), String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Control::Replace(name, tx));
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or_else(|_| Err("快捷键更新超时".into()))
    }
    pub(crate) fn stop(&self) {
        let _ = Self::cancel_selection();
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(Control::Stop(tx));
        let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
impl TrayHandle {
    pub(super) fn for_test() -> Self {
        Self {
            hwnd: 0,
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

pub(crate) fn start(
    sender: mpsc::Sender<Command>,
    hotkey: String,
    autostart: bool,
) -> Result<TrayHandle, String> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
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
            let _ = ready_tx.send(Err("托盘消息窗口创建失败".into()));
            return;
        }
        let (modifiers, key) = match parse_hotkey(&hotkey) {
            Ok(keys) => keys,
            Err(reason) => {
                let _ = ready_tx.send(Err(reason));
                // SAFETY: 仅按值传递本线程刚创建的窗口句柄，销毁失败仅返回非零。
                unsafe { DestroyWindow(hwnd) };
                return;
            }
        };
        // SAFETY: 参数均为句柄、热键 ID 与键值按值传递，hwnd 属于本线程。
        let hotkey_registered = unsafe { RegisterHotKey(hwnd, ID_ACTIVE, modifiers, key) } != 0;
        if !hotkey_registered {
            let _ = sender.send(Command::HotkeyUnavailable(
                "快捷键被占用，请在截图 OCR 页设置其他组合；后台服务仍在运行".into(),
            ));
        }
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        STATE.with(|slot| {
            *slot.borrow_mut() = Some(TrayState {
                commands: sender,
                queue: queue.clone(),
                active: if hotkey_registered { ID_ACTIVE } else { 0 },
                hotkey: hotkey.clone(),
            });
        });
        if !notify(hwnd, None, &hotkey, 0) {
            let _ = ready_tx.send(Err("通知区图标创建失败；截图服务未启动".into()));
            // SAFETY: 仅按值传递本线程刚创建的窗口句柄，销毁失败仅返回非零。
            unsafe { DestroyWindow(hwnd) };
            return;
        }
        if !autostart && hotkey_registered {
            let _ = notify(hwnd, Some(&format!("已启动，按 {hotkey} 截图")), &hotkey, 1);
        }
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
    });
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "托盘初始化超时".to_string())?
}

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
}
