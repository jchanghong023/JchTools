"""OS/UIA 驱动原语：进程拥有、窗口定位、点击、关闭与退出断言。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import contextlib
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import TYPE_CHECKING, cast

import comtypes
import pywintypes
import win32api
import win32con
import win32gui
import win32job
import win32process
from pywinauto import Application, controls, findbestmatch, findwindows, timings
from pywinauto.application import ProcessNotFoundError, WindowSpecification
from pywinauto.uia_defines import NoPatternInterfaceError

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scripts import isolation_probe, process_runtime

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping
    from typing import IO

    from pywinauto.base_wrapper import BaseWrapper

from scripts.gui_smoke_impl import config

TRANSIENT_GUI_ERRORS: tuple[type[Exception], ...] = (
    pywintypes.error,
    pywintypes.com_error,
    comtypes.COMError,
    ProcessNotFoundError,
    timings.TimeoutError,
    findwindows.ElementNotFoundError,
    findbestmatch.MatchError,
    controls.InvalidWindowHandle,
    controls.InvalidElement,
)

# 任务库（task.sqlite3）读取容忍集，与 GUI 瞬态错误分开维护：
# 任务库可能正被运行中的任务写入或损坏，读不出的任务一律跳过（原为 except Exception，
# 收窄为该处实际会发生的类型）：sqlite3.Error 覆盖库打不开/被锁/损坏；
# ValueError 覆盖元数据 JSON 解析失败（json.JSONDecodeError 及其子类）。
class _OwnedProcessTree:
    """Windows Job Object 持有所属进程树；父 GUI 退出后仍能安全回收后台."""

    def __init__(self, proc: subprocess.Popen[bytes]) -> None:
        # types-pywin32 的 Job API 返回值/参数漏标；按实际 Win32 签名收口，不修改第三方桩。
        create_job = cast("Callable[[object, str], int]", win32job.CreateJobObject)
        query_information = cast("Callable[[int, int], dict[str, object]]", vars(win32job)["QueryInformationJobObject"])
        set_information = cast(
            "Callable[[int, int, dict[str, object]], None]", vars(win32job)["SetInformationJobObject"]
        )
        job = create_job(None, "")
        assigned = False
        try:
            information = query_information(job, win32job.JobObjectExtendedLimitInformation)
            limits = cast("dict[str, int]", information["BasicLimitInformation"])
            limits["LimitFlags"] |= win32job.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            set_information(job, win32job.JobObjectExtendedLimitInformation, information)
            process = win32api.OpenProcess(win32con.PROCESS_SET_QUOTA | win32con.PROCESS_TERMINATE, 0, proc.pid)
            try:
                win32job.AssignProcessToJobObject(job, process)
            finally:
                win32api.CloseHandle(process)
            assigned = True
        finally:
            if not assigned:
                win32api.CloseHandle(job)
        self._job: int | None = job

    def terminate(self, proc: subprocess.Popen[bytes]) -> None:
        """只终止本轮 Job Object 中的 GUI 与后台，不按名称或已退出父 PID 操作."""
        self._kill_tree()
        with contextlib.suppress(OSError, ProcessLookupError):
            if proc.poll() is None:
                proc.kill()

    def close(self) -> None:
        """正常或异常退出后回收所属后台并释放 Job 句柄；重复关闭无副作用."""
        job = self._job
        if job is None:
            return
        try:
            self._kill_tree()
        finally:
            self._job = None
            win32api.CloseHandle(job)

    def _kill_tree(self) -> None:
        if self._job is not None:
            terminate_job = cast("Callable[[int, int], None]", vars(win32job)["TerminateJobObject"])
            terminate_job(self._job, 0)
            query = cast("Callable[[int, int], dict[str, int]]", vars(win32job)["QueryInformationJobObject"])
            deadline = time.monotonic() + 15.0
            while query(self._job, win32job.JobObjectBasicAccountingInformation)["ActiveProcesses"]:
                if time.monotonic() >= deadline:
                    message = "本轮测试 Job Object 的子进程未在清理期限内退出"
                    raise RuntimeError(message)
                time.sleep(0.05)


def start_owned_process(  # noqa: PLR0913 - 统一拥有启动接口保留 Popen 的环境、stdio 及创建标志。
    argv: list[str],
    *,
    env: dict[str, str] | None = None,
    stdin: IO[bytes] | int | None = None,
    stdout: IO[bytes] | int | None = None,
    stderr: IO[bytes] | int | None = None,
    creationflags: int = 0,
) -> tuple[subprocess.Popen[bytes], _OwnedProcessTree]:
    """先暂停创建并绑定独占 Job，再恢复进程，首条用户指令前已拥有整棵树。."""
    proc = subprocess.Popen(
        argv,
        env=env,
        stdin=stdin,
        stdout=stdout,
        stderr=stderr,
        creationflags=creationflags | win32con.CREATE_SUSPENDED,
    )
    owner: _OwnedProcessTree | None = None
    try:
        owner = _OwnedProcessTree(proc)
        process_runtime.resume_owned_process(proc.pid)
    except BaseException:
        # 恢复失败时也可能已有后代；先收回 Job，再回收根进程及本次打开的管道。
        try:
            if owner is not None:
                owner.close()
        finally:
            try:
                if proc.poll() is None:
                    proc.kill()
            finally:
                try:
                    _ = proc.wait()
                finally:
                    for stream in (proc.stdin, proc.stdout, proc.stderr):
                        if stream is not None:
                            stream.close()
        raise
    return proc, owner


def wait_window(pid: int, timeout: int = config.TIMEOUT) -> tuple[Application, WindowSpecification]:
    deadline = time.time() + timeout
    last: Exception | None = None
    while time.time() < deadline:
        try:
            app = Application(backend="uia").connect(process=pid, timeout=1)
            window = app.window(title="JchTools")
            if window.exists():
                return app, window
        except TRANSIENT_GUI_ERRORS as exc:
            last = exc
        time.sleep(0.5)
    msg = f"等待 JchTools 窗口超时：{last}"
    raise RuntimeError(msg)


def _same_physical_control(left: BaseWrapper, right: BaseWrapper) -> bool:
    left_rect = left.rectangle()
    right_rect = right.rectangle()
    width = max(0, min(left_rect.right, right_rect.right) - max(left_rect.left, right_rect.left))
    height = max(0, min(left_rect.bottom, right_rect.bottom) - max(left_rect.top, right_rect.top))
    overlap = width * height
    left_area = max(1, (left_rect.right - left_rect.left) * (left_rect.bottom - left_rect.top))
    right_area = max(1, (right_rect.right - right_rect.left) * (right_rect.bottom - right_rect.top))
    return overlap / min(left_area, right_area) >= config.PHYSICAL_OVERLAP_THRESHOLD


def find_button(window: WindowSpecification, title: str) -> WindowSpecification:
    """返回同一物理按钮组中唯一的可见控件，去除 Slint 外/内层重复节点.

    未找到当前可见节点时仍返回可轮询的 WindowSpecification。旧调用约定会
    对结果调用 ``exists()`` 判断按钮是否尚未出现；此处直接抛错会把正常的
    「运行态按钮尚未出现」误报成阶段失败。真正需要按钮的 ``wait``/``click``
    仍会在超时后失败。
    """
    matching = [button for button in window.descendants(control_type="Button") if (button.window_text() or "") == title]
    buttons: list[BaseWrapper] = []
    for button in matching:
        if (button.window_text() or "") != title:
            continue
        if any(_same_physical_control(button, existing) for existing in buttons):
            continue
        buttons.append(button)
    visible = [button for button in buttons if button.is_visible()]
    candidates = visible or buttons
    if not candidates:
        return window.child_window(title=title, control_type="Button")
    candidates.sort(key=lambda button: (button.rectangle().top, button.rectangle().left))
    chosen_index = next(index for index, button in enumerate(matching) if button is candidates[0])
    return window.child_window(title=title, control_type="Button", found_index=chosen_index)


def activate(window: WindowSpecification | BaseWrapper) -> None:
    """显示被测窗口但不抢焦点、前台或用户输入；局部窗口消息不依赖遮挡状态."""
    with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
        _ = win32gui.ShowWindow(window.handle, win32con.SW_SHOWNOACTIVATE)
    time.sleep(0.3)


def _post_control_click(window: WindowSpecification, control: WindowSpecification | BaseWrapper) -> None:
    """只向所属 HWND 投递控件中心的客户区鼠标消息，不移动系统光标."""
    window_handle = window.handle
    target_pid = win32process.GetWindowThreadProcessId(window_handle)[1]
    handle = control.handle or window_handle
    require(
        cast("BaseWrapper", control).process_id() == target_pid
        and win32process.GetWindowThreadProcessId(handle)[1] == target_pid
        and (handle == window_handle or win32gui.IsChild(window_handle, handle)),
        "局部点击拒绝操作：控件 HWND/PID 不属于被测窗口",
    )
    require(
        control.is_visible()
        and control.is_enabled()
        and win32gui.IsWindowVisible(handle)
        and win32gui.IsWindowEnabled(handle),
        "局部点击拒绝操作：控件或目标窗口不可见/未启用",
    )
    rectangle = control.rectangle()
    left, top = cast("tuple[int, int]", win32gui.ScreenToClient(handle, (rectangle.left, rectangle.top)))
    right, bottom = cast("tuple[int, int]", win32gui.ScreenToClient(handle, (rectangle.right, rectangle.bottom)))
    client_left, client_top, client_right, client_bottom = win32gui.GetClientRect(handle)
    require(
        client_left <= left < right <= client_right and client_top <= top < bottom <= client_bottom,
        "局部点击拒绝操作：控件矩形为空或超出目标客户区",
    )
    point = (((top + bottom) // 2) << 16) | ((left + right) // 2)
    win32gui.PostMessage(handle, win32con.WM_MOUSEMOVE, 0, point)
    win32gui.PostMessage(handle, win32con.WM_LBUTTONDOWN, win32con.MK_LBUTTON, point)
    win32gui.PostMessage(handle, win32con.WM_LBUTTONUP, 0, point)


def click(window: WindowSpecification, control: WindowSpecification | BaseWrapper) -> None:
    activate(window)
    try:
        invoke = getattr(control, "invoke", None)
    except TRANSIENT_GUI_ERRORS:
        invoke = None
    if callable(invoke):
        try:
            _ = invoke()
        except (NoPatternInterfaceError, *TRANSIENT_GUI_ERRORS):
            pass
        else:
            time.sleep(0.3)
            return
    _post_control_click(window, control)
    time.sleep(0.3)


def confirm_dialog(window: WindowSpecification, timeout: int = config.TIMEOUT, *, extraction: bool = False) -> None:
    """勾选「我已确认…」并点「确认」，以**对话框消失**作为成功判据.

    点击可能落在对话框滑入动画的空档或未生效，所以按当前状态重试：
    未勾选就点复选框，已勾选就点确认，直到对话框关闭；
    只检查「点击没报错」会把没生效的点击当成成功，后续等待必然超时。
    """
    deadline = time.time() + timeout
    next_attempt = 0.0
    attempts = 0
    closed_observations = 0
    while time.time() < deadline:
        try:
            # 重试前也必须新枚举：上次观察尾部关闭后，不再等待已删除的复选框。
            texts = [text.window_text() or "" for text in window.descendants(control_type="Text")]
            if texts and "确认文件处理范围" not in texts:
                closed_observations += 1
                if closed_observations >= config.CONFIRM_CLOSED_OBSERVATIONS:
                    return
            else:
                closed_observations = 0
                if texts and time.time() >= next_attempt:
                    checkbox = window.child_window(
                        title=config.EXTRACT_ACK if extraction else config.ORGANIZE_ACK, control_type="CheckBox"
                    )
                    _ = checkbox.wait("visible", timeout=min(10.0, max(0.1, deadline - time.time())))
                    time.sleep(0.3)  # 让对话框完成滑入，避免点到动画中途的位置
                    ok = find_button(window, "确认")
                    click(window, ok if ok.is_enabled() else checkbox)
                    attempts += 1
                    if attempts > 1:
                        print(f"  确认框仍未关闭，重试第 {attempts - 1} 次")
                    next_attempt = time.time() + 6.0
        except TRANSIENT_GUI_ERRORS:
            # 空树、重建和控件等待超时都不是关闭证据；耗尽总期限仍须失败。
            closed_observations = 0
        time.sleep(0.3)
    msg = "确认对话框没有关闭：勾选或确认点击未生效"
    raise RuntimeError(msg)


def set_directory(window: WindowSpecification, path: str) -> None:
    # 目录输入框是与「选择目录…」按钮同排的 Edit 控件。
    button = find_button(window, "选择目录…")
    button_rect = button.rectangle()
    candidates = [
        edit
        for edit in window.descendants(control_type="Edit")
        if abs(edit.rectangle().top - button_rect.top) < config.EDIT_ROW_TOLERANCE_PX
        and edit.rectangle().right <= button_rect.left
    ]
    if not candidates:
        msg = "未找到目录输入框"
        raise RuntimeError(msg)
    # 侧栏搜索框在小窗口下可能与目录输入框同高；取紧挨「选择目录…」左侧的 Edit。
    edit = min(candidates, key=lambda e: button_rect.left - e.rectangle().right)
    _ = edit.set_edit_text(path)


def setup_directory(window: WindowSpecification, path: str, timeout: int = config.TIMEOUT) -> None:
    """设置目录并确认界面已接受它.

    界面未接受时「开始解压与分析」保持禁用，直接点击只会空转、随后以超时收场；
    判据用状态栏文案而不是按钮状态，避免把「扫描中」误判成未就绪。
    """
    for attempt in range(1, 4):
        set_directory(window, path)
        deadline = time.time() + timeout
        while time.time() < deadline:
            for text in window.descendants(control_type="Text"):
                if "目录已就绪" in (text.window_text() or ""):
                    return
            time.sleep(0.5)
        print(f"  目录未被界面接受，重试第 {attempt} 次")
    msg = "目录未被界面接受：状态栏始终没有出现「目录已就绪」"
    raise RuntimeError(msg)


def open_confirm(window: WindowSpecification, button_title: str, timeout: int = config.TIMEOUT) -> None:
    """点击会弹出确认框的按钮，并确认确认框已经出现.

    上一个对话框的关闭动画期间点击会被吞掉，按钮显示 enabled 但不生效；
    因此以「确认框出现」为判据重试按钮点击。
    """
    title = config.EXTRACT_ACK if button_title == "开始解压" else config.ORGANIZE_ACK
    checkbox = window.child_window(title=title, control_type="CheckBox")
    deadline = time.time() + timeout
    while time.time() < deadline:
        button = find_button(window, button_title)
        _ = button.wait("visible enabled", timeout=timeout)
        click(window, button)
        try:
            _ = checkbox.wait("visible", timeout=5)
        except TRANSIENT_GUI_ERRORS:
            print(f"  「{button_title}」后确认框未出现，重试")
        else:
            return


def close_app(window: WindowSpecification) -> None:
    """先发 WM_CLOSE；窗口仍在时仅通过 UIA/所属 HWND 局部消息尝试关闭按钮."""
    handle = window.handle
    win32gui.PostMessage(handle, win32con.WM_CLOSE, 0, 0)
    time.sleep(0.3)
    with contextlib.suppress(*TRANSIENT_GUI_ERRORS, RuntimeError):
        if not window.exists():
            return
    for button in [b for b in window.descendants(control_type="Button") if (b.window_text() or "") == "关闭"]:
        click(window, button)
        return


def _escalate_close(window: WindowSpecification) -> None:
    """关闭兜底：向窗口发标准 WM_CLOSE，走同一条 on_close_requested 路径，不依赖坐标命中."""
    with contextlib.suppress(*TRANSIENT_GUI_ERRORS):
        win32gui.PostMessage(window.handle, win32con.WM_CLOSE, 0, 0)


def _wait_exit_or_kill(
    proc: subprocess.Popen[bytes],
    timeout: int = 15,
    window: WindowSpecification | None = None,
    owner: _OwnedProcessTree | None = None,
) -> tuple[bool, int | None]:
    """等待进程退出，超时则强制结束（避免异常路径泄漏进程），并返回是否被强杀与退出码.

    首次关闭及重试均不依赖系统鼠标或前台窗口；重试补发 WM_CLOSE，
    把「关闭请求未落地」与「关闭路径真的挂死」区分开——后者会耗尽全部重试并
    最终被强杀、由断言判失败。
    """
    deadline = time.time() + timeout
    attempts = 0
    while time.time() < deadline:
        try:
            _ = proc.wait(timeout=min(3.0, max(0.1, deadline - time.time())))
        except subprocess.TimeoutExpired:
            attempts += 1
            if window is not None:
                with contextlib.suppress(*TRANSIENT_GUI_ERRORS, RuntimeError):
                    if attempts <= 1:
                        close_app(window)
                    else:
                        _escalate_close(window)
        else:
            return False, proc.returncode
    if owner is not None:
        owner.terminate(proc)
    else:
        with contextlib.suppress(OSError, ProcessLookupError):
            proc.kill()
    with contextlib.suppress(subprocess.TimeoutExpired, OSError):
        _ = proc.wait(timeout=10)
    return True, proc.returncode


def assert_clean_exit(tag: str, *, killed: bool, code: int | None) -> None:
    """S1-S4 的退出断言：被强杀或非 0 退出码都算失败（AGENTS §3.4 不得把未验证当作通过）."""
    if killed:
        msg = f"{tag}：多次点击关闭后进程仍未退出，已被强杀（关闭路径可能挂死或窗口无法退出）"
        raise RuntimeError(msg)
    if code != 0:
        msg = f"{tag}：进程退出码 {code}（预期 0）"
        raise RuntimeError(msg)


def run_stage(  # noqa: PLR0913 - 脚手架的收尾钩子与子进程环境天然成组
    tag: str,
    exe: str,
    body: Callable[[WindowSpecification], None],
    *,
    pre: Callable[[], None] | None = None,
    after: Callable[[], None] | None = None,
    env: dict[str, str] | None = None,
) -> None:
    """S1-S18 共用的启动/拆除脚手架：拥有启动 → 隔离证明 → pre → 窗口业务 → 统一收尾.

    拆除顺序（含异常路径）为：窗口非 None 才 close_app；随后等待/结束本次 Job
    Object 的 GUI 及其子服务；最后无论前面哪一步失败都执行 after。pre 也在统一
    try/finally 内，且必须先取得本轮 GUI 隔离证明。Job Object 在进程恢复前绑定，
    不能触碰其他用户实例。env 非 None 时传给子进程。
    启动前始终检查两个隔离变量并记录日志快照，旧 SQLite 或旧日志不能授权业务。
    收尾断言与最终 PASS 行统一在此打印。
    """
    effective_env = dict(os.environ if env is None else env)
    missing = missing_isolation_env(effective_env)
    if missing:
        message = f"GUI 启动前必须隔离状态目录与资产根，缺少：{','.join(missing)}；请经 scripts/acceptance.ps1 执行"
        raise RuntimeError(message)
    state = Path(effective_env["JCHTOOLS_TEST_STATE_DIR"])
    existing_logs = isolation_probe._isolation_log_snapshot(state)  # noqa: SLF001 - 合同固定的共享隔离证据接口。
    proc, owner = start_owned_process([exe], env=effective_env)
    window: WindowSpecification | None = None
    try:
        _, window = wait_window(proc.pid)
        if not isolation_probe._wait_process_isolation(  # noqa: SLF001 - 合同固定的共享隔离证据接口。
            state, proc, "gui", existing_logs, time.monotonic() + config.TIMEOUT
        ):
            message = (
                "GUI 未证明隔离 SQLite 与本轮 PID 的启动日志；"
                "请先 cargo build --features test-hooks，禁止操作真实用户配置"
            )
            raise RuntimeError(message)
        if pre is not None:
            pre()
        body(window)
    finally:
        if window is not None:
            with contextlib.suppress(*TRANSIENT_GUI_ERRORS, RuntimeError):
                close_app(window)
        try:
            killed, code = _wait_exit_or_kill(proc, window=window, owner=owner)
        finally:
            try:
                owner.close()
            finally:
                if after is not None:
                    after()
    assert_clean_exit(tag, killed=killed, code=code)
    print(f"{tag} PASS：进程已退出")


def require(condition: object, message: str) -> None:
    """S6-S14 的断言 helper：bandit B101 禁用 assert，统一 raise 口径."""
    if not condition:
        raise RuntimeError(message)


def missing_isolation_env(env: Mapping[str, str]) -> list[str]:
    """返回隔离守卫要求但当前环境缺失的变量键（纯函数；单测见 scripts/test_gui_smoke.py）."""
    return [key for key in config.ISOLATION_REQUIRED_ENV_KEYS if not env.get(key)]


