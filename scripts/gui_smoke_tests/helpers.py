"""gui_smoke 判据单测共享常量与内存 UIA 假件（自 scripts/test_gui_smoke.py 原样搬移）."""

from __future__ import annotations
import contextlib
import io
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
import zipfile
from collections import Counter
from dataclasses import dataclass
from functools import partial
from pathlib import Path
from typing import TYPE_CHECKING, cast
from unittest.mock import Mock, patch
import win32event
if TYPE_CHECKING:
    from collections.abc import Callable

    from pywinauto.application import WindowSpecification
from scripts import gui_smoke, make_tmp, markdown_acceptance, requirement_coverage, xberg_test_engine
DONE_LINE = "成功 2 · 部分提取 0 · 失败 0 · 已有结果跳过 0 · 重复结果跳过 0 · 总耗时 0.4s"


DIFFERENT_LINE = "成功 0 · 部分提取 0 · 失败 0 · 已有结果跳过 2 · 重复结果跳过 0 · 总耗时 0.4s"


ISOLATED_ENV = {
    "JCHTOOLS_TEST_STATE_DIR": r"C:\isolated\state",
    "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": r"C:\isolated\snap-assets",
}


REQUIRED_IDLE_CHECKS = 2


def snapshot_names(snapshot: frozenset[tuple[str, int, int]]) -> set[str]:
    return {entry[0] for entry in snapshot}


def require_organize_failure(
    root: Path,
    expected: dict[str, tuple[str, int]],
    git_before: tuple[frozenset[str], dict[str, tuple[str, int]]],
) -> None:
    try:
        gui_smoke.verify_organize_witnesses(root, expected, git_before)
    except RuntimeError:
        return
    message = "整理结果判据未拒绝合同反例"
    raise AssertionError(message)


class _ReviewClock:
    def __init__(self) -> None:
        self.now = 0.0

    def read(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds


@dataclass(frozen=True)
class _ReviewRectangle:
    left: int
    top: int
    right: int
    bottom: int


class _ReviewControl:
    def __init__(
        self,
        title: str,
        rectangle: _ReviewRectangle = _ReviewRectangle(250, 80, 390, 112),
        *,
        action: Callable[[], None] | None = None,
        before_wait: Callable[[], None] | None = None,
    ) -> None:
        self.title = title
        self.bounds = rectangle
        self.action = action
        self.before_wait = before_wait

    def window_text(self) -> str:
        return self.title

    def rectangle(self) -> _ReviewRectangle:
        return self.bounds

    def is_visible(self) -> bool:
        return True

    def is_enabled(self) -> bool:
        return True

    def wait(self, _state: str, **_options: object) -> _ReviewControl:
        if self.before_wait is not None:
            self.before_wait()
        return self

    def invoke(self) -> None:
        if self.action is not None:
            self.action()

    def click_input(self) -> None:
        message = "内存 UIA 回归禁止鼠标兜底"
        raise AssertionError(message)


class _ReviewStageDriver:
    """仅替换进程/桌面边界，pre、真实阶段主体及 after 仍执行。."""

    def __init__(self, window: object) -> None:
        self.window = window

    def __call__(
        self,
        _tag: str,
        _exe: str,
        body: Callable[[WindowSpecification], None],
        *,
        pre: Callable[[], None] | None = None,
        after: Callable[[], None] | None = None,
        **_options: object,
    ) -> None:
        try:
            if pre is not None:
                pre()
            body(cast("WindowSpecification", self.window))
        finally:
            if after is not None:
                after()


class _ReviewLayoutWindow:
    handle = 0
    # 独立 UIA 场景，不从被测 LAYOUT_PAGES 生成，否则漏页会同步消失。
    pages: dict[str, tuple[str, ...]] = {
        "递归解压": ("开始解压",),
        "目录整理": ("开始分析",),
        "MD 整理": ("开始合并",),
        "转 Markdown": ("开始转换",),
        "Git 工具": ("开始",),
        "截图 OCR": (),
        "设置": ("下载 / 补齐音频转写资产", "检查已安装组件"),
        "关于": (),
        "模型服务": ("保存配置", "退出模型服务"),
        # 错误提示当前可见；正常无提示时不应无条件等待“关闭提示”。
        "CodeAgent": ("保存间隔", "关闭提示"),
    }

    def __init__(self, damaged_page: str = "", damage: str = "", *, show_hint: bool = True) -> None:
        self.page = "递归解压"
        self.bounds = _ReviewRectangle(0, 0, 960, 620)
        self.damaged_page = damaged_page
        self.damage = damage
        self.show_hint = show_hint
        self.modal_open = False
        self.service_running = True

    def rectangle(self) -> _ReviewRectangle:
        return self.bounds

    def resize(
        self, _handle: int, _after: int, _x: int, _y: int, width: int, height: int, _flags: int
    ) -> None:
        self.bounds = _ReviewRectangle(0, 0, width, height)

    def select_page(self, page: str) -> None:
        self.page = page

    def open_modal(self) -> None:
        self.modal_open = True

    def return_to_page(self) -> None:
        self.modal_open = False

    def stop_service(self) -> None:
        self.service_running = False

    def descendants(self, *, control_type: str) -> list[_ReviewControl]:
        if control_type == "Text":
            if self.modal_open:
                return [_ReviewControl("退出模型服务")]
            return [_ReviewControl("截图快捷键"), _ReviewControl("关于 JchTools")]
        if control_type != "Button":
            return []
        if self.modal_open:
            titles = ("返回检查", "确认退出模型服务")
            actions: tuple[Callable[[], None] | None, ...] = (self.return_to_page, self.stop_service)
            page = "confirm5"
            buttons: list[_ReviewControl] = []
        else:
            titles = self.pages[self.page]
            if self.page == "CodeAgent" and not self.show_hint:
                titles = ("保存间隔",)
            actions = tuple(self.open_modal if title == "退出模型服务" else None for title in titles)
            page = self.page
            buttons = [
                _ReviewControl(name, action=partial(self.select_page, name))
                for name in self.pages
            ]
        for index, (title, action) in enumerate(zip(titles, actions, strict=True)):
            left = 250 + index * 180
            bounds = _ReviewRectangle(left, 200, left + 160, 240)
            if page == self.damaged_page:
                if self.damage == "clipped" and index == 0:
                    bounds = _ReviewRectangle(250, 200, self.bounds.right + 1, 240)
                elif self.damage == "overlap" and index == 1:
                    bounds = _ReviewRectangle(370, 200, 530, 240)
            buttons.append(_ReviewControl(title, bounds, action=action))
        return buttons

    def child_window(self, *, title: str, control_type: str, found_index: int = 0) -> _ReviewControl:
        matching = [control for control in self.descendants(control_type=control_type) if control.title == title]
        if len(matching) <= found_index:
            message = f"内存 UIA 中不存在可见控件：{title}"
            raise gui_smoke.timings.TimeoutError(message)
        return matching[found_index]


class _ReviewLayoutService:
    """仅替换真实服务的进程/Job 边界，不改页面或模态行为。."""

    def __init__(self) -> None:
        self.reaped = False

    def poll(self) -> int | None:
        return 0 if self.reaped else None

    def close(self) -> None:
        self.reaped = True

    def wait(self, *, timeout: int) -> int:
        del timeout
        if not self.reaped:
            raise AssertionError("隔离服务必须先由所属 Job 收尾")
        return 0


class _ReviewValueElement:
    def GetCurrentPropertyValue(self, _identifier: int) -> bool:  # noqa: N802 - UIA 协议方法名。
        return True


class _ReviewElementInfo:
    def __init__(self) -> None:
        self.element = _ReviewValueElement()


class _ReviewLogEdit(_ReviewControl):
    def __init__(self, value: str) -> None:
        super().__init__("")
        self.value = value
        self.element_info = _ReviewElementInfo()

    def get_value(self) -> str:
        return self.value


class _ReviewUiaDll:
    UIA_IsValuePatternAvailablePropertyId = 1


class _ReviewUia:
    UIA_dll = _ReviewUiaDll()


class _ReviewConversionWindow:
    def __init__(self, media_name: str, cancellation_log: str) -> None:
        self.media_name = media_name
        self.cancellation_log = cancellation_log
        self.output: Path | None = None
        self.running = False
        self.stopped = False

    def set_directories(self, _window: WindowSpecification, _input: str, output: str) -> None:
        self.output = Path(output)

    def start(self) -> None:
        if self.output is None:
            message = "尚未选择输出目录"
            raise AssertionError(message)
        _ = (self.output / "00_complete_txt.md").write_bytes(b"completed before cancellation\n")
        self.running = True

    def stop(self) -> None:
        self.running = False
        self.stopped = True

    def descendants(self, *, control_type: str) -> list[_ReviewControl]:
        if control_type == "Button":
            if self.running:
                return [_ReviewControl("停止任务", action=self.stop)]
            return [_ReviewControl("开始转换", action=self.start)]
        if control_type == "Text":
            return [_ReviewControl("已停止" if self.stopped else f"正在处理 01_{self.media_name}")]
        if control_type == "Edit":
            return [_ReviewLogEdit(self.cancellation_log if self.stopped else "")]
        return []

    def child_window(self, *, title: str, control_type: str, found_index: int = 0) -> _ReviewControl:
        return [control for control in self.descendants(control_type=control_type) if control.title == title][found_index]


class _ReviewConfirmWindow:
    def __init__(self, observations: tuple[str, ...]) -> None:
        self.observations = iter(observations)
        self.last = "open"
        self.closed = False

    def wait_checkbox(self) -> None:
        if self.closed:
            message = "确认层已删除，旧复选框不可见"
            raise gui_smoke.timings.TimeoutError(message)

    def descendants(self, *, control_type: str) -> list[_ReviewControl]:
        if control_type == "Button":
            return [_ReviewControl("确认")]
        if control_type != "Text":
            return []
        self.last = next(self.observations, self.last)
        self.closed = self.last == "closed"
        if self.last == "error":
            message = "UIA 树正重建"
            raise gui_smoke.timings.TimeoutError(message)
        if self.last == "empty":
            return []
        return [_ReviewControl("后台任务页面" if self.closed else "确认文件处理范围")]

    def child_window(self, *, title: str, control_type: str, found_index: int = 0) -> _ReviewControl:
        if control_type == "CheckBox":
            return _ReviewControl(title, before_wait=self.wait_checkbox)
        return self.descendants(control_type=control_type)[found_index]
