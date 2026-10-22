"""gui_smoke 判据单测用例（自 scripts/test_gui_smoke.py 原样搬移）."""

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

from scripts.gui_smoke_tests.helpers import (
    DONE_LINE,
    DIFFERENT_LINE,
    ISOLATED_ENV,
    REQUIRED_IDLE_CHECKS,
    snapshot_names,
    require_organize_failure,
    _ReviewClock,
    _ReviewRectangle,
    _ReviewControl,
    _ReviewStageDriver,
    _ReviewLayoutWindow,
    _ReviewLayoutService,
    _ReviewValueElement,
    _ReviewElementInfo,
    _ReviewLogEdit,
    _ReviewUiaDll,
    _ReviewUia,
    _ReviewConversionWindow,
    _ReviewConfirmWindow,
)
class WindowLocalClickTests(unittest.TestCase):
    """局部点击必须命中所属 HWND 的真实控件坐标，拒绝错误归属及不可用控件."""

    @staticmethod
    def control() -> Mock:
        control = Mock()
        control.handle = 0
        control.process_id.return_value = 501
        control.invoke.side_effect = gui_smoke.NoPatternInterfaceError
        control.rectangle.return_value = _ReviewRectangle(260, 180, 340, 220)
        control.is_visible.return_value = True
        control.is_enabled.return_value = True
        return control

    @staticmethod
    def dispatch(control: Mock) -> list[tuple[int, int, int, int]]:
        window = Mock(handle=100)
        events: list[tuple[int, int, int, int]] = []

        def post_message(handle: int, message: int, keys: int, point: int) -> None:
            events.append((handle, message, keys, point))

        with (
            patch("scripts.gui_smoke.time.sleep"),
            patch("scripts.gui_smoke.win32gui.ShowWindow"),
            patch("scripts.gui_smoke.win32process.GetWindowThreadProcessId", return_value=(1, 501)),
            patch("scripts.gui_smoke.win32gui.IsChild", return_value=False),
            patch("scripts.gui_smoke.win32gui.IsWindowVisible", return_value=True),
            patch("scripts.gui_smoke.win32gui.IsWindowEnabled", return_value=True),
            patch("scripts.gui_smoke.win32gui.GetClientRect", return_value=(0, 0, 640, 480)),
            patch(
                "scripts.gui_smoke.win32gui.ScreenToClient",
                side_effect=lambda _handle, point: (point[0] - 200, point[1] - 100),
            ),
            patch("scripts.gui_smoke.win32gui.PostMessage", side_effect=post_message),
        ):
            gui_smoke.click(cast("WindowSpecification", window), cast("WindowSpecification", control))
        return events

    def test_unavailable_invoke_targets_control_center_without_foreground_input(self) -> None:
        events = self.dispatch(self.control())
        point = gui_smoke.win32api.MAKELONG(100, 100)
        self.assertEqual(
            events,
            [
                (100, gui_smoke.win32con.WM_MOUSEMOVE, 0, point),
                (100, gui_smoke.win32con.WM_LBUTTONDOWN, gui_smoke.win32con.MK_LBUTTON, point),
                (100, gui_smoke.win32con.WM_LBUTTONUP, 0, point),
            ],
        )

    def test_invalid_owner_visibility_enablement_and_bounds_reject_local_click(self) -> None:
        for invalid in ("process", "foreign-hwnd", "hidden", "disabled", "outside", "empty"):
            control = self.control()
            if invalid == "process":
                control.process_id.return_value = 999
            elif invalid == "foreign-hwnd":
                control.handle = 200
            elif invalid == "hidden":
                control.is_visible.return_value = False
            elif invalid == "disabled":
                control.is_enabled.return_value = False
            elif invalid == "outside":
                control.rectangle.return_value = _ReviewRectangle(190, 180, 340, 220)
            else:
                control.rectangle.return_value = _ReviewRectangle(260, 180, 260, 220)
            with self.subTest(invalid=invalid), self.assertRaises(RuntimeError):
                _ = self.dispatch(control)


class ReviewRemediationLayoutTests(unittest.TestCase):
    def run_layout(self, window: _ReviewLayoutWindow) -> None:
        clock = _ReviewClock()
        service = _ReviewLayoutService()

        def spawn(argv: list[str], *, env: dict[str, str]) -> tuple[_ReviewLayoutService, _ReviewLayoutService]:
            self.assertIs(env, stage.call_args.kwargs["env"], "GUI 与服务必须使用同一完整隔离环境")
            self.assertEqual(argv, ["unused.exe", "--acp-http-service"], "不允许启动真实 GUI 或 Agent")
            state = Path(env["JCHTOOLS_TEST_STATE_DIR"])
            self.assertTrue(state.is_absolute())
            self.assertNotEqual(str(state), ISOLATED_ENV["JCHTOOLS_TEST_STATE_DIR"])
            for key in (
                "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
                "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
                "JCHTOOLS_TEST_ASSET_ROOT",
                "JCHTOOLS_TEST_CODEAGENT_CREDENTIALS",
            ):
                self.assertTrue(Path(env[key]).is_relative_to(state), key)
            self.assertFalse(Path(env["JCHTOOLS_TEST_CODEAGENT_CREDENTIALS"]).exists(), "不得读取真实 Token")
            self.assertEqual(
                env["JCHTOOLS_TEST_CODEAGENT_REGISTRY"],
                rf"Software\JchTools\Tests\CodeAgent\{state.name}",
            )
            self.assertEqual(env["JCHTOOLS_TEST_ACP_SERVICE_EXE"], str(Path("unused.exe").resolve()))
            return service, service
        with (
            patch.object(gui_smoke, "run_stage", side_effect=_ReviewStageDriver(window)) as stage,
            patch.object(gui_smoke, "activate"),
            patch("scripts.gui_smoke.win32gui.SetWindowPos", side_effect=window.resize),
            patch("scripts.gui_smoke.time.time", side_effect=clock.read),
            patch("scripts.gui_smoke.time.sleep", side_effect=clock.sleep),
            patch.dict(
                os.environ,
                {
                    **ISOLATED_ENV,
                    "JCHTOOLS_TEST_CODEAGENT_CREDENTIALS": r"C:\real-profile\.cac\.credentials.json",
                    "JCHTOOLS_TEST_CODEAGENT_REGISTRY": "Environment",
                    "JCHTOOLS_TEST_ACP_SERVICE_EXE": r"C:\real-production\JchTools.exe",
                },
            ),
            patch.object(gui_smoke, "start_owned_process", side_effect=spawn) as popen,
        ):
            try:
                gui_smoke.s18_layout_at_two_window_sizes("unused.exe")
            finally:
                self.assertTrue(service.reaped, "成功与几何失败都必须回收本轮独占服务")
                popen.assert_called_once()
                self.assertTrue(window.service_running, "布局验收不得确认退出模型服务")
                self.assertFalse(window.modal_open, "布局验收必须通过返回检查关闭 kind 5")

    def test_review_remediation_s18_rejects_clipping_on_new_pages_and_exit_modal(self) -> None:
        # P2-07 / U-05 / P-13：真正执行 S18，遗漏页面/模态将无法拒绝这些几何反例。
        for page in ("模型服务", "CodeAgent", "confirm5"):
            with self.subTest(page=page), self.assertRaisesRegex(RuntimeError, "裁切|移出窗口"):
                self.run_layout(_ReviewLayoutWindow(page, "clipped"))

    def test_review_remediation_s18_rejects_overlap_on_new_pages_and_exit_modal(self) -> None:
        # P2-07：CodeAgent 的关闭提示已可见；不要求无提示状态出现此按钮。
        for page in ("模型服务", "CodeAgent", "confirm5"):
            with self.subTest(page=page), self.assertRaisesRegex(RuntimeError, "重叠"):
                self.run_layout(_ReviewLayoutWindow(page, "overlap"))

    def test_review_remediation_s18_accepts_normal_geometry_without_stopping_service(self) -> None:
        for show_hint in (False, True):
            with self.subTest(show_hint=show_hint):
                self.run_layout(_ReviewLayoutWindow(show_hint=show_hint))


class ReviewRemediationCancellationLogTests(unittest.TestCase):
    def run_s5(self, log: str) -> None:
        # P3-35 / T-23 / P-12：执行真实 S5 主体、产物判据及 wait_log_matching；
        # 只替换桌面/进程边界，不在测试中复制正则。
        media_name = "中文 示例 clip.mp4"
        clock = _ReviewClock()
        window = _ReviewConversionWindow(media_name, log)
        with tempfile.TemporaryDirectory(prefix="jchtools-review-s5-") as temporary:
            media = Path(temporary) / media_name
            _ = media.write_bytes(b"synthetic media; no decoder is launched")
            with (
                patch.dict(os.environ, {"JCHTOOLS_S5_MEDIA": str(media)}),
                patch.object(gui_smoke, "run_stage", side_effect=_ReviewStageDriver(window)),
                patch.object(gui_smoke, "goto_converter"),
                patch.object(gui_smoke, "set_converter_dirs", side_effect=window.set_directories),
                patch.object(gui_smoke, "activate"),
                patch.object(gui_smoke, "IUIA", side_effect=_ReviewUia),
                patch("scripts.gui_smoke.time.time", side_effect=clock.read),
                patch("scripts.gui_smoke.time.monotonic", side_effect=clock.read),
                patch("scripts.gui_smoke.time.sleep", side_effect=clock.sleep),
                patch("scripts.gui_smoke.subprocess.Popen", side_effect=AssertionError("禁止启动真实 GUI")),
            ):
                gui_smoke.s5_markdown_basic_chain("unused.exe")

    def test_review_remediation_s5_accepts_cancelled_media_with_chinese_and_spaces(self) -> None:
        self.run_s5("已取消：01_中文 示例 clip.mp4；未完成结果不提交\n已停止")

    def test_review_remediation_s5_rejects_stopped_without_a_cancellation_entry(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "只读日志超时"):
            self.run_s5("已停止\n没有文件被确认取消")

    def test_review_remediation_s5_rejects_cancellation_suffix_on_another_line(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "只读日志超时"):
            self.run_s5("已取消：01_中文 示例 clip.mp4\n其他事件；未完成结果不提交")


class ReviewRemediationConfirmationTests(unittest.TestCase):
    def confirm(self, window: _ReviewConfirmWindow, *, extraction: bool = False) -> None:
        clock = _ReviewClock()
        with (
            patch.object(gui_smoke, "activate"),
            patch("scripts.gui_smoke.time.time", side_effect=clock.read),
            patch("scripts.gui_smoke.time.sleep", side_effect=clock.sleep),
            patch("scripts.gui_smoke.subprocess.Popen", side_effect=AssertionError("禁止启动真实 GUI")),
        ):
            gui_smoke.confirm_dialog(cast("WindowSpecification", window), timeout=15, extraction=extraction)

    def test_review_remediation_confirm_closure_observations_cross_batch_boundary(self) -> None:
        # P3-38 / P-13：第 19 次仍 open，20、21 次 closed；第 20 次后旧 checkbox 消失。
        for extraction in (False, True):
            window = _ReviewConfirmWindow(("open",) * 19 + ("closed", "closed"))
            with self.subTest(extraction=extraction):
                self.confirm(window, extraction=extraction)
                self.assertTrue(window.closed)

    def test_review_remediation_confirm_unreadable_or_open_tree_cannot_report_success(self) -> None:
        # P3-38：空树、瞬态异常、始终打开均不能作为正常关闭的有效观察。
        for observation in ("empty", "error", "open"):
            with self.subTest(observation=observation), self.assertRaises(RuntimeError):
                self.confirm(_ReviewConfirmWindow((observation,)))

    def test_review_remediation_confirm_invalid_reading_breaks_consecutive_closure(self) -> None:
        # P3-38：单次有效关闭观察不得穿过空树、异常或重新出现的标题累积成成功。
        for observation in ("empty", "error", "open"):
            with self.subTest(observation=observation), self.assertRaises(RuntimeError):
                self.confirm(_ReviewConfirmWindow(("closed", observation) * 40))
