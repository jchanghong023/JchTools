//! 托盘窗口使用的 Win32 原生绑定：句柄类型、消息结构与 user32/shell32/kernel32 extern 声明。

use std::ffi::c_void;

pub(super) type Handle = *mut c_void;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Point {
    pub(super) x: i32,
    pub(super) y: i32,
}
#[repr(C)]
pub(super) struct Msg {
    hwnd: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: Point,
    private: u32,
}
#[repr(C)]
pub(super) struct WndClass {
    pub(super) style: u32,
    pub(super) proc: Option<unsafe extern "system" fn(Handle, u32, usize, isize) -> isize>,
    pub(super) cls_extra: i32,
    pub(super) wnd_extra: i32,
    pub(super) instance: Handle,
    pub(super) icon: Handle,
    pub(super) cursor: Handle,
    pub(super) background: Handle,
    pub(super) menu: *const u16,
    pub(super) class: *const u16,
}
#[repr(C)]
pub(super) struct NotifyIconData {
    pub(super) cb_size: u32,
    pub(super) hwnd: Handle,
    pub(super) id: u32,
    pub(super) flags: u32,
    pub(super) callback: u32,
    pub(super) icon: Handle,
    pub(super) tip: [u16; 128],
    pub(super) state: u32,
    pub(super) state_mask: u32,
    pub(super) info: [u16; 256],
    pub(super) timeout: u32,
    pub(super) info_title: [u16; 64],
    pub(super) info_flags: u32,
    pub(super) guid: [u8; 16],
    pub(super) balloon_icon: Handle,
}
#[link(name = "user32")]
extern "system" {
    pub(super) fn RegisterClassW(class: *const WndClass) -> u16;
    pub(super) fn CreateWindowExW(
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
    pub(super) fn DestroyWindow(hwnd: Handle) -> i32;
    pub(super) fn DefWindowProcW(hwnd: Handle, msg: u32, w: usize, l: isize) -> isize;
    pub(super) fn GetMessageW(msg: *mut Msg, hwnd: Handle, min: u32, max: u32) -> i32;
    pub(super) fn TranslateMessage(msg: *const Msg) -> i32;
    pub(super) fn DispatchMessageW(msg: *const Msg) -> isize;
    pub(super) fn PostMessageW(hwnd: Handle, msg: u32, w: usize, l: isize) -> i32;
    pub(super) fn SetTimer(
        hwnd: Handle,
        id: usize,
        interval: u32,
        callback: Option<unsafe extern "system" fn(Handle, u32, usize, u32)>,
    ) -> usize;
    pub(super) fn KillTimer(hwnd: Handle, id: usize) -> i32;
    pub(super) fn RegisterHotKey(hwnd: Handle, id: i32, modifiers: u32, key: u32) -> i32;
    pub(super) fn UnregisterHotKey(hwnd: Handle, id: i32) -> i32;
    pub(super) fn CreatePopupMenu() -> Handle;
    pub(super) fn AppendMenuW(menu: Handle, flags: u32, id: usize, text: *const u16) -> i32;
    pub(super) fn TrackPopupMenu(
        menu: Handle,
        flags: u32,
        x: i32,
        y: i32,
        reserved: i32,
        hwnd: Handle,
        rect: *const c_void,
    ) -> i32;
    pub(super) fn DestroyMenu(menu: Handle) -> i32;
    pub(super) fn GetCursorPos(point: *mut Point) -> i32;
    pub(super) fn SetForegroundWindow(hwnd: Handle) -> i32;
    pub(super) fn RegisterWindowMessageW(message: *const u16) -> u32;
    pub(super) fn PostQuitMessage(code: i32);
}
#[link(name = "shell32")]
extern "system" {
    pub(super) fn Shell_NotifyIconW(action: u32, data: *mut NotifyIconData) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    pub(super) fn GetModuleHandleW(module: *const u16) -> Handle;
}
