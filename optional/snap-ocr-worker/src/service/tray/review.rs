//! 测试专用隔离托盘（P3-31）：message-only 窗口与可门控的选区交互。
//! 从 `tray.rs` 原样搬移；可见性由 `pub(super)` 调整为 `pub(in crate::service)` 等价原范围。

use std::collections::VecDeque;
use std::ptr::{null, null_mut};
use std::sync::{mpsc, Arc, Mutex};

use super::super::Command;
use super::{
    cleanup_tray_state, wide, wnd_proc, CreateWindowExW, DestroyWindow, DispatchMessageW,
    GetMessageW, GetModuleHandleW, Handle, Msg, PostMessageW, RegisterClassW, TrayHandle,
    TrayState, WndClass, REVIEW_SELECTION, STATE, WM_CLOSE,
};

// P3-31：只创建 message-only 窗口；不注册全局热键、不接触通知区、不抢焦点。
// 门控发生在真实消息循环前，可重现已经排队但尚未交付的 Trigger。
#[cfg(test)]
pub(in crate::service) struct ReviewTray {
    pub(in crate::service) handle: TrayHandle,
    release: Option<mpsc::SyncSender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    selecting: bool,
    selection_release: Option<mpsc::SyncSender<()>>,
    selection_entered: mpsc::Receiver<()>,
}

#[cfg(test)]
impl ReviewTray {
    pub(in crate::service) fn start(commands: mpsc::Sender<Command>) -> Result<Self, String> {
        Self::start_inner(commands, false)
    }

    pub(in crate::service) fn selecting(commands: mpsc::Sender<Command>) -> Result<Self, String> {
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
    pub(in crate::service) fn fill_message_queue(&self) -> Result<(), String> {
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

    pub(in crate::service) fn wait_selection(&self) -> Result<(), String> {
        self.selection_entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())
    }

    pub(in crate::service) fn finish_selection(&mut self) {
        if let Some(release) = self.selection_release.take() {
            let _ = release.send(());
        }
    }

    pub(in crate::service) fn terminate(&mut self) -> Result<(), String> {
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
