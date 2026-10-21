//! 截图前隐藏本应用可见顶层窗并按隐藏前状态恢复（S9-11 完整路径比对）。
//! 从 `capture_win.rs` 按内聚职责原样搬移；行为与注释保持不变。

use super::MAIN_EXE_IMAGE;
use super::{
    error, image_belongs_to_main, CloseHandle, EnumWindows, GetCurrentProcessId, GetWindow,
    GetWindowThreadProcessId, Handle, IsIconic, IsWindowVisible, IsZoomed, OpenProcess,
    QueryFullProcessImageNameW, ShowWindow,
};

/// 暂时隐藏本应用的可见顶层窗；截图完成后按隐藏前的窗口状态恢复。
///
/// 恢复语义（行为修复）：隐藏前最小化的窗口必须以 `SW_SHOWMINNOACTIVE` 恢复
/// ——保持最小化且不抢焦点；旧实现一律 `SW_SHOW`/`SW_MAXIMIZE`，会把用户
/// 截图前最小化的窗口强行还原成正常可见并激活，最小化状态丢失。列表里只会
/// 出现无归属（`GW_OWNER` 为空）的顶层窗（`hide_own_window` 过滤掉可见性
/// 不符与有归属的工具/对话框窗口），三种恢复命令对它们语义完备。
#[derive(Clone, Copy)]
enum RestoreState {
    /// 隐藏前最小化（`IsIconic`）。
    Minimized,
    /// 隐藏前最大化（`IsZoomed`）。
    Maximized,
    /// 隐藏前普通可见。
    Normal,
}

/// 由窗口当前状态判定恢复命令（隐藏期间用户无法操作该窗口，状态稳定）。
fn restore_state(hwnd: Handle) -> RestoreState {
    // SAFETY: 只读查询最小化状态；句柄来自 EnumWindows 回调，回调期间有效。
    let minimized = unsafe { IsIconic(hwnd) } != 0;
    if minimized {
        RestoreState::Minimized
    } else {
        // SAFETY: 同一回调句柄，回调期间有效；仅在未最小化分支执行只读的最大化查询。
        let maximized = unsafe { IsZoomed(hwnd) } != 0;
        if maximized {
            RestoreState::Maximized
        } else {
            RestoreState::Normal
        }
    }
}

/// 恢复命令（ShowWindow）：SW_SHOWMINNOACTIVE / SW_MAXIMIZE / SW_SHOW。
fn restore_command(state: RestoreState) -> i32 {
    match state {
        RestoreState::Minimized => 7,
        RestoreState::Maximized => 3,
        RestoreState::Normal => 5,
    }
}

pub(super) struct HiddenWindows(Vec<(usize, RestoreState)>);
impl Drop for HiddenWindows {
    fn drop(&mut self) {
        for &(hwnd, state) in &self.0 {
            // SAFETY: 句柄来自 hide_own_window 记录的有效顶层窗，恢复命令与其
            // 隐藏前记录的放置状态一一匹配。
            unsafe {
                ShowWindow(hwnd as Handle, restore_command(state));
            }
        }
    }
}
unsafe extern "system" fn hide_own_window(hwnd: Handle, data: isize) -> i32 {
    // SAFETY: hwnd 由 EnumWindows 逐窗传入，回调执行期间句柄有效；本调用只读
    // 查询可见性。
    let visible = unsafe { IsWindowVisible(hwnd) };
    if visible == 0 {
        return 1;
    }
    // SAFETY: 同一回调句柄，回调期间有效；只读查询 GW_OWNER 归属窗口。
    let owner = unsafe { GetWindow(hwnd, 4) };
    if !owner.is_null() {
        return 1;
    }
    let mut pid = 0;
    // SAFETY: pid 是本栈上已初始化的 u32，回调期间独占可写；hwnd 同上有效。
    unsafe { GetWindowThreadProcessId(hwnd, &raw mut pid) };
    if pid == 0 {
        return 1;
    }
    // SAFETY: GetCurrentProcessId 无参数，只读返回当前进程的 PID。
    let own = pid == unsafe { GetCurrentProcessId() };
    let named = if own {
        true
    } else {
        // SAFETY: 仅以 PROCESS_QUERY_LIMITED_INFORMATION 打开已知 pid，不继承
        // 句柄；失败由空句柄表达。
        let process = unsafe { OpenProcess(0x1000, 0, pid) };
        if process.is_null() {
            false
        } else {
            let mut path = vec![0u16; 32_768];
            let mut count = 32_768u32;
            // SAFETY: process 是刚打开且未关闭的有效句柄；path 是本栈上 NUL 结尾
            // 的 32768 字缓冲，count 初值与其容量一致，均只在本次调用期间使用。
            let queried = unsafe {
                QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &raw mut count)
            };
            // SAFETY: process 由上方 OpenProcess 打开且尚无其他引用，关闭恰一次。
            unsafe { CloseHandle(process) };
            // S9-11：按完整进程路径与已登记主程序比对——basename 撞名的
            // 用户另装实例/同名进程不隐藏（旧实现只看 basename 会误隐藏）。
            queried != 0 && {
                let main = MAIN_EXE_IMAGE
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                image_belongs_to_main(
                    &String::from_utf16_lossy(&path[..count as usize]),
                    main.as_deref(),
                )
            }
        }
    };
    if named {
        // SAFETY: data 由 hide_application_windows 经 EnumWindows 传入，指向其
        // 栈上存活的 HiddenWindows；回调同步执行，期间独占可写。
        let hidden = unsafe { &mut *(data as *mut HiddenWindows) };
        // 隐藏前先记录放置状态（最小化/最大化/普通），Drop 时按状态恢复——
        // 最小化窗口保持最小化（SW_SHOWMINNOACTIVE），不被强行正常化+抢焦点。
        hidden.0.push((hwnd as usize, restore_state(hwnd)));
        // SAFETY: 同一有效顶层窗句柄；SW_HIDE 只隐藏本窗，恢复由记录的状态驱动。
        unsafe { ShowWindow(hwnd, 0) };
    }
    1
}
pub(super) fn hide_application_windows() -> Result<HiddenWindows, String> {
    let mut windows = HiddenWindows(Vec::new());
    // SAFETY: 回调同步运行，data 指向本栈上仍然有效的 Vec；Drop 恢复窗口。
    if unsafe {
        EnumWindows(
            hide_own_window,
            (&raw mut windows).cast::<HiddenWindows>() as isize,
        )
    } == 0
    {
        return Err(error("无法隐藏程序窗口"));
    }
    Ok(windows)
}
