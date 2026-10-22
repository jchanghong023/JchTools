"""转 Markdown 验收 pywinauto GUI 驱动原语（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

# pyright: reportImportCycles=false
import contextlib
import os
import subprocess
import time
from pathlib import Path
from typing import TYPE_CHECKING, cast

import comtypes
import pywintypes
import win32con
import win32gui
from pywinauto import Application, controls, findbestmatch, findwindows, timings
from pywinauto.application import ProcessNotFoundError
from pywinauto.uia_defines import IUIA

from scripts import isolation_probe
from scripts.gui_smoke import (
    assert_clean_exit,
    click,
    completion_confirms_new_run,
    missing_isolation_env,
    start_owned_process,
)
from scripts.markdown_acceptance_impl.model import (
    STATUS_FAILED,
    STATUS_NOT_RUN,
    GuiRun,
    Outcome,
    _isolated_environment_error,
)

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping

    from pywinauto.application import WindowSpecification
    from pywinauto.base_wrapper import BaseWrapper

    from scripts.markdown_acceptance_impl.runner import Context

__all__ = [
    "BUSY_TIMEOUT",
    "CONVERSION_TIMEOUT",
    "CONVERT_ROW_COUNT",
    "EDIT_ROW_TOLERANCE_PX",
    "GUI_WINDOW_TIMEOUT",
    "PHYSICAL_OVERLAP_THRESHOLD",
    "READINESS_TIMEOUT",
    "TRANSIENT_ERRORS",
    "_MAX_START_RECLICKS",
    "_RECLICK_INTERVAL_S",
    "_ReclickState",
    "_button_state",
    "_click_button",
    "_close_conversion_gui",
    "_configure_conversion_paths",
    "_connect_window",
    "_conversion_preflight_error",
    "_convert_directory_rows",
    "_isolation_precondition",
    "_request_close",
    "_resolve_gui",
    "_same_physical_control",
    "_set_row_edit",
    "_terminate",
    "_wait_start_ready",
    "_window_texts",
    "drive_conversion",
    "unique_visible_buttons",
]


# 转 Markdown 页自上而下的「选择目录…」行数：输入 / 输出（XB-20）。
CONVERT_ROW_COUNT = 2


GUI_WINDOW_TIMEOUT = 60


READINESS_TIMEOUT = 180


BUSY_TIMEOUT = 30


CONVERSION_TIMEOUT = 1800  # 真实 OCR/媒体转录按分钟级计，验收宁等勿假。


EDIT_ROW_TOLERANCE_PX = 20


PHYSICAL_OVERLAP_THRESHOLD = 0.8


# pywinauto/pywin32 窗口操作在窗口建立/销毁竞态下的瞬态错误族；枚举与 gui_smoke.py
# 的 TRANSIENT_GUI_ERRORS 同源（那边附有逐项理由），此处等待循环内一律按可重试处理。
TRANSIENT_ERRORS: tuple[type[Exception], ...] = (
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


def _connect_window(pid: int) -> tuple[Application, WindowSpecification]:
    deadline = time.time() + GUI_WINDOW_TIMEOUT
    last: Exception | None = None
    while time.time() < deadline:
        try:
            app = Application(backend="uia").connect(process=pid, timeout=1)
            # 同进程的原生对话框可能也叫 JchTools；只连接 winit 的可见主窗口。
            window = app.window(title="JchTools", class_name="Window Class", visible_only=True)
            if window.exists():
                return app, window
        except TRANSIENT_ERRORS as exc:
            last = exc
        time.sleep(0.5)
    message = f"等待 JchTools 窗口超时：{last}"
    raise RuntimeError(message)


def _click_button(window: WindowSpecification, title: str) -> None:
    deadline = time.time() + GUI_WINDOW_TIMEOUT
    button: BaseWrapper | None = None
    while time.time() < deadline:
        try:
            candidates = unique_visible_buttons(window, title)
            for candidate in candidates:
                if candidate.is_visible() and candidate.is_enabled():
                    button = candidate
                    break
        except TRANSIENT_ERRORS:
            # 控件树刷新时重新查询；不重试已经发出的启动/停止动作。
            time.sleep(0.2)
            continue
        if button is not None:
            break
        time.sleep(0.2)
    if button is None:
        message = f"按钮「{title}」未找到唯一可见可用控件"
        raise RuntimeError(message)
    # 共用 UIA Invoke / 窗口局部消息边界，不移动硬件鼠标或抢占前台。
    click(window, button)


def _button_state(window: WindowSpecification, title: str) -> tuple[bool, bool]:
    """返回（可见, 可用）；元素未解析按（False, False）处理，由调用方轮询."""
    try:
        candidates = unique_visible_buttons(window, title)
        visible = [button for button in candidates if button.is_visible()]
        return bool(visible), bool(visible and visible[0].is_enabled())
    except TRANSIENT_ERRORS:
        return False, False


def _convert_directory_rows(window: WindowSpecification) -> list[BaseWrapper]:
    """按可见按钮的物理矩形去重，返回输入/输出两行控件。."""
    return unique_visible_buttons(window, "选择目录…")


def _same_physical_control(left: BaseWrapper, right: BaseWrapper) -> bool:
    left_rect = left.rectangle()
    right_rect = right.rectangle()
    intersection_width = max(0, min(left_rect.right, right_rect.right) - max(left_rect.left, right_rect.left))
    intersection_height = max(0, min(left_rect.bottom, right_rect.bottom) - max(left_rect.top, right_rect.top))
    intersection = intersection_width * intersection_height
    left_area = max(1, (left_rect.right - left_rect.left) * (left_rect.bottom - left_rect.top))
    right_area = max(1, (right_rect.right - right_rect.left) * (right_rect.bottom - right_rect.top))
    return intersection / min(left_area, right_area) >= PHYSICAL_OVERLAP_THRESHOLD


def unique_visible_buttons(window: WindowSpecification, title: str) -> list[BaseWrapper]:
    candidates: list[BaseWrapper] = []
    for button in window.descendants(control_type="Button"):
        if (button.window_text() or "") != title:
            continue
        if not button.is_visible():
            continue
        if any(_same_physical_control(button, existing) for existing in candidates):
            continue
        candidates.append(button)
    candidates.sort(key=lambda button: (button.rectangle().top, button.rectangle().left))
    return candidates


def _set_row_edit(window: WindowSpecification, row_button: BaseWrapper, value: str) -> None:
    top = row_button.rectangle().top
    candidates = [
        edit
        for edit in window.descendants(control_type="Edit")
        if abs(edit.rectangle().top - top) < EDIT_ROW_TOLERANCE_PX
    ]
    if not candidates:
        message = "未找到与「选择目录…」同排的目录输入框"
        raise RuntimeError(message)
    _ = min(candidates, key=lambda edit: edit.rectangle().left).set_edit_text(value)


def _window_texts(window: WindowSpecification) -> str:
    parts: list[str] = []
    with contextlib.suppress(*TRANSIENT_ERRORS):
        for text in window.descendants(control_type="Text"):
            value = text.window_text() or ""
            if value.strip():
                parts.append(value)
        # Slint 的运行日志是只读 TextEdit，UIA 暴露为 Edit/Value 而不是 Text/Name。
        for edit in window.descendants(control_type="Edit"):
            editor = cast("controls.uia_controls.EditWrapper", edit)
            # 数值输入框也属于 Edit，但只提供 RangeValue，不能调用文本 Value。
            value_available = IUIA().UIA_dll.UIA_IsValuePatternAvailablePropertyId
            if not editor.element_info.element.GetCurrentPropertyValue(value_available):
                continue
            value = editor.get_value() or ""
            if value.strip():
                parts.append(value)
    return "\n".join(parts)


def _request_close(window: WindowSpecification) -> None:
    """先发 WM_CLOSE，未退出时共用不占用硬件输入的按钮激活边界."""
    handle = window.handle
    win32gui.PostMessage(handle, win32con.WM_CLOSE, 0, 0)
    time.sleep(0.3)
    with contextlib.suppress(*TRANSIENT_ERRORS):
        if not window.exists():
            return
    for button in unique_visible_buttons(window, "关闭"):
        click(window, button)
        return


def _terminate(proc: subprocess.Popen[bytes]) -> tuple[bool, int | None]:
    """先等待实际自然退出；仅超时且仍存活时强杀，并保留退出证据。."""
    try:
        return False, proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        if proc.poll() is not None:
            return False, proc.returncode
        proc.kill()
        return True, proc.wait(timeout=10)


_MAX_START_RECLICKS = 3


_RECLICK_INTERVAL_S = 10.0


def _isolation_precondition(env: Mapping[str, str]) -> Outcome | None:
    """只把缺必需隔离键归为未执行；非法路径仍按失败呈现。."""
    invalid = _isolated_environment_error(env)
    if invalid is not None:
        return Outcome(STATUS_FAILED, invalid)
    missing = missing_isolation_env(env)
    if missing:
        return Outcome(STATUS_NOT_RUN, f"GUI 验收缺少隔离环境变量：{missing}，拒绝启动")
    return None


def _conversion_preflight_error() -> str | None:
    """启动 GUI 前同时检查必需隔离键与隔离根，保持原拒绝诊断。."""
    pending = _isolation_precondition(os.environ)
    return None if pending is None else pending.reason


def _resolve_gui(ctx: Context, exe: Path | None = None) -> tuple[Path | None, str | None]:
    target = exe or ctx.gui_exe
    if target is None:
        return None, "未提供被测 GUI（--gui-exe；验收驱动器不猜测默认被测物，避免拿陈旧 debug 产物充当验收对象）"
    if not target.is_file():
        return None, f"被测 GUI 不存在：{target}"
    return target, None


def _wait_start_ready(window: WindowSpecification, timeout: float) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if _button_state(window, "开始转换") == (True, True):
            return True
        time.sleep(0.5)
    return False


def _close_conversion_gui(
    proc: subprocess.Popen[bytes], window: WindowSpecification | None, close_tree: Callable[[], None] | None
) -> None:
    """仅在自然 exit 0 且 Job 回收成功后容忍关闭阶段的瞬态 UIA 竞态。."""
    close_error: Exception | None = None
    try:
        if proc.poll() is None and window is not None:
            try:
                _request_close(window)
            except TRANSIENT_ERRORS as error:
                close_error = error
    finally:
        try:
            killed, code = _terminate(proc)
        finally:
            if close_tree is not None:
                close_tree()
    if close_error is not None and (killed or code != 0):
        raise close_error
    assert_clean_exit("转 Markdown GUI", killed=killed, code=code)


class _ReclickState:
    """「开始转换」重按状态机：运行态/产物出现前周期性补偿落空的合成点击."""

    def __init__(self) -> None:
        self.count: int = 0
        self.last_click: float = time.time()

    def maybe_reclick(
        self, window: WindowSpecification, *, saw_busy: bool, produced: bool, start_enabled: bool
    ) -> None:
        if saw_busy or produced or not start_enabled:
            return
        if self.count >= _MAX_START_RECLICKS or time.time() - self.last_click <= _RECLICK_INTERVAL_S:
            return
        self.count += 1
        self.last_click = time.time()
        _click_button(window, "开始转换")


def _configure_conversion_paths(window: WindowSpecification, input_dir: Path, output_dir: Path) -> str | None:
    """进入转换页、设置输入输出并等待开始就绪，不参与进程归属或启动守卫。."""
    _click_button(window, "转 Markdown")
    rows = _convert_directory_rows(window)
    if len(rows) < CONVERT_ROW_COUNT:
        return f"转 Markdown 页「选择目录…」按钮不足两行（实得 {len(rows)}）"
    _set_row_edit(window, rows[0], str(input_dir))
    _set_row_edit(window, rows[1], str(output_dir))
    if not _wait_start_ready(window, READINESS_TIMEOUT):
        return "「开始转换」始终未就绪（组件未初始化或就绪检查失败）"
    return None


def drive_conversion(exe: Path, input_dir: Path, output_dir: Path, *, stop_after_busy: bool = False) -> GuiRun:
    """经真实 GUI 公开入口执行一次转换并收集可观察结果.

    注意：本函数只在资产齐全的真实环境被调用（前置检查先行）；无资产环境从不执行，
    其内部链路未在当前环境验证——这与 NOT RUN 的如实呈现是同一立场，不得虚构已验证。
    """
    environment_error = _conversion_preflight_error()
    if environment_error is not None:
        return GuiRun([], "", environment_error)
    state = Path(os.environ["JCHTOOLS_TEST_STATE_DIR"])
    existing_logs = isolation_probe._isolation_log_snapshot(state)  # noqa: SLF001  # 固定共享隔离 API。
    startup_deadline = time.monotonic() + GUI_WINDOW_TIMEOUT
    proc, owner = start_owned_process([str(exe)])
    window: WindowSpecification | None = None
    try:
        _, window = _connect_window(proc.pid)
        if not isolation_probe._wait_process_isolation(  # noqa: SLF001  # 固定共享隔离 API。
            state, proc, "gui", existing_logs, startup_deadline
        ):
            return GuiRun(
                [],
                "",
                "GUI 未证明使用隔离配置（须隔离 SQLite 与本轮 GUI PID 启动日志）；请使用 test-hooks 构建",
            )
        setup_error = _configure_conversion_paths(window, input_dir, output_dir)
        if setup_error is not None:
            return GuiRun([], _window_texts(window), setup_error)
        previous_texts = _window_texts(window)
        _click_button(window, "开始转换")
        saw_busy = False
        error: str | None = None
        deadline = time.time() + CONVERSION_TIMEOUT
        # 合成点击偶发落空（点击后状态仍是「尚未开始」且无产物）：周期性重按
        # 「开始转换」（按钮可用且未观察到运行态时），最多 _MAX_START_RECLICKS
        # 次；按钮不可用或已见运行态即停止重按，不干扰正常转换。
        reclick_state = _ReclickState()
        while time.time() < deadline:
            busy_visible, _enabled = _button_state(window, "停止任务")
            if busy_visible:
                saw_busy = True
                if stop_after_busy:
                    _click_button(window, "停止任务")
            start_visible, start_enabled = _button_state(window, "开始转换")
            produced = any(output_dir.rglob("*.md"))
            finished_after_busy = saw_busy and start_visible and start_enabled
            finished_fast = (
                not saw_busy
                and start_visible
                and start_enabled
                and (produced or completion_confirms_new_run(_window_texts(window), previous_texts, None, None))
            )
            if finished_after_busy or finished_fast:
                break
            reclick_state.maybe_reclick(
                window, saw_busy=saw_busy, produced=produced, start_enabled=start_visible and start_enabled
            )
            time.sleep(0.5)
        else:
            error = "转换未在时限内结束（开始转换未重新可用）"
        if stop_after_busy and not saw_busy and error is None:
            error = "任务过快结束，未能观察到运行态并验证停止路径"
        if error is not None:
            return GuiRun([], _window_texts(window), error)
        outputs = sorted(path for path in output_dir.rglob("*.md") if path.is_file())
        return GuiRun(outputs, _window_texts(window), None)
    finally:
        _close_conversion_gui(proc, window, owner.close)
