//! 当前会话的通知区图标、全局热键和框选窗口。消息循环独立于 Slint 与 ONNX。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc, LazyLock, Mutex};

use super::{protocol::wide, Command};
use crate::capture_win;

mod hotkey;
mod icon;
mod win32;

pub(crate) use hotkey::parse_hotkey;
use icon::notification_icon;
use win32::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetModuleHandleW, Handle, KillTimer, Msg,
    NotifyIconData, Point, PostMessageW, PostQuitMessage, RegisterClassW, RegisterHotKey,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, Shell_NotifyIconW, TrackPopupMenu,
    TranslateMessage, UnregisterHotKey, WndClass,
};

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

#[cfg(test)]
mod review;
#[cfg(test)]
pub(super) use review::ReviewTray;

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
mod tests;
