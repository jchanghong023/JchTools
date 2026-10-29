"""转换页 UI 交互助手：目录行、等待文本/日志与完成确认机制。.

由 gui_smoke.py 按域拆出；行为与拆分前一致，入口 scripts/gui_smoke.py 负责 re-export。
"""

from __future__ import annotations

import re
import sys
import time
from pathlib import Path
from typing import TYPE_CHECKING, cast

import win32con
import win32gui
from pywinauto.uia_defines import IUIA

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

if TYPE_CHECKING:
    from pywinauto import controls
    from pywinauto.application import WindowSpecification
    from pywinauto.base_wrapper import BaseWrapper

from scripts.gui_smoke_impl import config, driver


def converter_directory_rows(window: WindowSpecification) -> list[BaseWrapper]:
    """转 Markdown 页自上而下两行「选择目录…」按钮：输入 / 输出（XB-20）."""
    buttons = [b for b in window.descendants(control_type="Button") if (b.window_text() or "") == "选择目录…"]
    unique: list[BaseWrapper] = []
    for button in buttons:
        if not button.is_visible() or any(driver.same_physical_control(button, existing) for existing in unique):
            continue
        unique.append(button)
    unique.sort(key=lambda b: (b.rectangle().top, b.rectangle().left))
    buttons = unique
    if len(buttons) < config.CONVERT_DIR_ROWS:
        msg = f"转 Markdown 页「选择目录…」按钮不足两行（实得 {len(buttons)}）"
        raise RuntimeError(msg)
    return buttons


def unique_visible_buttons(window: WindowSpecification, title: str) -> list[BaseWrapper]:
    """按矩形去除 Slint 外层/内层重复 UIA Button 节点."""
    candidates: list[BaseWrapper] = []
    for button in window.descendants(control_type="Button"):
        if (button.window_text() or "") != title or not button.is_visible():
            continue
        if any(driver.same_physical_control(button, existing) for existing in candidates):
            continue
        candidates.append(button)
    candidates.sort(key=lambda button: (button.rectangle().top, button.rectangle().left))
    return candidates


def wait_button(window: WindowSpecification, title: str, timeout: float, *, enabled: bool = False) -> BaseWrapper:
    """动态等待唯一可见按钮，避免页面切换时缓存空 WindowSpecification."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            buttons = unique_visible_buttons(window, title)
            if buttons and (not enabled or any(button.is_enabled() for button in buttons)):
                return next(button for button in buttons if not enabled or button.is_enabled())
        except driver.TRANSIENT_GUI_ERRORS:
            pass
        time.sleep(0.2)
    message = f"等待可见{('可用' if enabled else '')}按钮超时：{title}"
    raise RuntimeError(message)


def click_and_wait_text(
    window: WindowSpecification,
    button_title: str,
    needle: str,
    attempts: int = 5,
) -> str:
    """点击按钮并以 needle 文本出现为准——合成点击可能落空，未出现即重试.

    与 confirm_dialog/open_confirm 同一立场：只检查「点击没报错」会把没生效
    的点击当成功，后续等待必然超时。
    """
    for _attempt in range(attempts):
        driver.click(window, driver.find_button(window, button_title))
        try:
            return wait_text_containing(window, needle, timeout=config.ATTEMPT_TIMEOUT)
        except RuntimeError:
            continue
    msg = f"点击「{button_title}」后始终未出现「{needle}」（点击可能一直落空）"
    raise RuntimeError(msg)


def wait_text_containing(
    window: WindowSpecification,
    needle: str,
    timeout: int = config.COMPLETION_TIMEOUT,
    exclude: str = "",
) -> str:
    """等待任一 Text 控件包含 needle 并返回其全文（设置/转换页状态断言的共用判据）.

    exclude 非空时跳过与它完全相同的旧文案——重试类断言用它确保等到的是
    新一次操作的结果，而不是仍留在界面上的上一次输出。
    """
    deadline = time.time() + timeout
    last_seen = ""
    while time.time() < deadline:
        try:
            for text in window.descendants(control_type="Text"):
                value = text.window_text() or ""
                if needle in value and value != exclude:
                    return value
                if value:
                    last_seen = value
        except driver.TRANSIENT_GUI_ERRORS:
            pass
        time.sleep(0.5)
    msg = f"等待界面文本「{needle}」超时（最后可见文本：{last_seen[:200]}）"
    raise RuntimeError(msg)


def wait_log_containing(window: WindowSpecification, needle: str) -> str:
    """T-23：Slint 只读日志暴露为 Edit/Value，不能以 Text/Name 读取。."""
    deadline = time.monotonic() + config.CONVERT_STOP_TIMEOUT
    while time.monotonic() < deadline:
        try:
            for control in window.descendants(control_type="Edit"):
                editor = cast("controls.uia_controls.EditWrapper", control)
                available = IUIA().UIA_dll.UIA_IsValuePatternAvailablePropertyId
                if not editor.element_info.element.GetCurrentPropertyValue(available):
                    continue
                value = editor.get_value() or ""
                if needle in value:
                    return value
        except driver.TRANSIENT_GUI_ERRORS:
            pass
        time.sleep(0.1)
    message = f"等待真实 GUI 只读日志超时：{needle}"
    raise RuntimeError(message)


def wait_log_matching(window: WindowSpecification, pattern: str) -> str:
    """同 wait_log_containing，但按正则匹配——取消条目落在哪个媒体文件上取决于时延."""
    deadline = time.monotonic() + config.CONVERT_STOP_TIMEOUT
    while time.monotonic() < deadline:
        try:
            for control in window.descendants(control_type="Edit"):
                editor = cast("controls.uia_controls.EditWrapper", control)
                available = IUIA().UIA_dll.UIA_IsValuePatternAvailablePropertyId
                if not editor.element_info.element.GetCurrentPropertyValue(available):
                    continue
                value = editor.get_value() or ""
                if re.search(pattern, value):
                    return value
        except driver.TRANSIENT_GUI_ERRORS:
            pass
        time.sleep(0.1)
    message = f"等待真实 GUI 只读日志超时（正则）：{pattern}"
    raise RuntimeError(message)


def find_check(window: WindowSpecification, title: str) -> WindowSpecification:
    """按标题返回 CheckBox 控件规格（与 driver.find_button 同构，类型注解一致）.

    可见性由调用处的 wait 保证。
    """
    return window.child_window(title=title, control_type="CheckBox")


def check_flat_output(window: WindowSpecification) -> None:
    """勾选「平铺输出」.

    Slint 的 Check 不暴露 UIA TogglePattern，无法直读勾选状态；用随 convert-flat
    切换的提示文案（app.slint 输出目录卡片）作为状态锚：点击后等「平铺时同名结果
    只处理排序第一份」出现，未出现按点击落空重试（奇数次点击收敛到勾选态）。
    """
    _ = find_check(window, "平铺输出").wait("visible", timeout=config.TIMEOUT)
    for _attempt in range(3):
        driver.click(window, find_check(window, "平铺输出"))
        try:
            _ = wait_text_containing(window, "平铺时同名结果只处理排序第一份", timeout=8)
        except RuntimeError:
            continue
        return
    msg = "「平铺输出」未能勾选（状态提示始终未切换为平铺文案）"
    raise RuntimeError(msg)


def produced_snapshot(directory: Path) -> frozenset[tuple[str, int, int]]:
    """输出目录产物快照（相对路径、mtime_ns、大小）；目录不可读时返回空快照.

    转换页没有可观察的任务计数或时间戳文本（收尾统计行的总耗时只保留 0.1s 精度，
    两轮可以逐字相同），用「点击开始前后的产物快照变化」作任务代次锚（S11-06）。

    全有或全无语义（复审修正）：枚举或 stat 中途失败（Windows 写锁/杀软扫描
    句柄）返回空快照而不是部分集合——部分快照相对完整 before 必然「已变化」，
    会把陈旧完成行误判为新完成；空快照在 completion_confirms_new_run 侧被
    视为「无锚」（与 None 同路），宁可退回文本判据也不假锚。
    """
    snapshot: list[tuple[str, int, int]] = []
    try:
        for path in directory.rglob("*"):
            if path.is_file():
                info = path.stat()
                snapshot.append((str(path.relative_to(directory)), info.st_mtime_ns, info.st_size))
    except OSError:
        return frozenset()
    return frozenset(snapshot)


def completion_confirms_new_run(
    value: str,
    previous_done: str,
    produced_before: frozenset[tuple[str, int, int]] | None,
    produced_now: frozenset[tuple[str, int, int]] | None,
) -> bool:
    """S11-06 新一轮批次完成判据（纯函数）.

    含完成标记且统计行文本不同于上一轮 → 新完成；文本与上一轮逐字相同时，若
    调用方提供了前后两份非空输出快照且快照已变化，也判为新一轮完成。
    任一侧空快照均按「无锚」处理（与 None 同路）：它可能表示尚无产物或目录
    不可读（部分快照已被 produced_snapshot 归零），不得当「已变化」。
    纯跳过批次若统计行也逐字相同则不存在任何可观察锚点（现有各阶段的成功/跳过
    计数必然变化，不命中该死角）。
    """
    if config.CONVERT_DONE_MARKER not in value:
        return False
    if value != previous_done:
        return True
    if not produced_before or not produced_now:
        return False
    return produced_now != produced_before


def confirm_new_completion(
    value: str,
    previous_done: str,
    produced_dir: Path | None,
    before: frozenset[tuple[str, int, int]] | None,
) -> bool:
    """以 completion_confirms_new_run 判定 value；快照只在完成标记在场后才取（S11-06）."""
    if config.CONVERT_DONE_MARKER not in value:
        return False
    now = produced_snapshot(produced_dir) if produced_dir is not None else None
    return completion_confirms_new_run(value, previous_done, before, now)


def wait_completion_with_anchor(
    window: WindowSpecification,
    previous_done: str,
    produced_dir: Path | None,
    before: frozenset[tuple[str, int, int]] | None,
) -> str:
    """已确认进入运行态后等待本次收尾统计行（S11-06：快照锚兜底）.

    不能用 exclude=previous_done 的纯文本等待——统计行与上一轮逐字相同时会把
    新完成误判为旧文案残留而超时假失败，由产物快照锚兜底识别。
    """
    deadline = time.time() + config.COMPLETION_TIMEOUT
    last_seen = ""
    while time.time() < deadline:
        try:
            for text in window.descendants(control_type="Text"):
                value = text.window_text() or ""
                if confirm_new_completion(value, previous_done, produced_dir, before):
                    return value
                if value:
                    last_seen = value
        except driver.TRANSIENT_GUI_ERRORS:
            pass
        time.sleep(0.5)
    msg = f"等待新一轮转换收尾统计超时（上一轮统计：{previous_done[:120]}；最后可见文本：{last_seen[:200]}）"
    raise RuntimeError(msg)


def close_title_bar(window: WindowSpecification) -> None:
    """直接向目标 GUI 窗口发送 WM_CLOSE，并等待停止确认弹窗出现."""
    handle = window.handle
    win32gui.PostMessage(handle, win32con.WM_CLOSE, 0, 0)
    _ = wait_text_containing(window, config.STOP_AND_CLOSE_TITLE, timeout=config.TIMEOUT)


def wait_window_closed_after_cancel(window: WindowSpecification) -> None:
    """T-23/U-09：任务关闭必须在取消预算内自然生效。."""
    deadline = time.monotonic() + config.CONVERT_STOP_TIMEOUT
    while window.exists() and time.monotonic() < deadline:
        time.sleep(0.1)
    driver.require(not window.exists(), "确认关闭后不得等待当前媒体自然完成")


def set_edit_before_button(window: WindowSpecification, button: BaseWrapper, value: str) -> None:
    """填写选择按钮左侧最近的同排输入框，避免误选文件名或侧栏搜索框."""
    rect = button.rectangle()
    candidates = [
        edit
        for edit in window.descendants(control_type="Edit")
        if abs(edit.rectangle().top - rect.top) < config.EDIT_ROW_TOLERANCE_PX and edit.rectangle().right <= rect.left
    ]
    driver.require(candidates, "未找到选择按钮左侧输入框")
    _ = min(candidates, key=lambda edit: rect.left - edit.rectangle().right).set_edit_text(value)
