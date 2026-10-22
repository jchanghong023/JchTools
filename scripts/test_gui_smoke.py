"""gui_smoke 判据及内存 UIA 阶段回归单测.

不启动真实 GUI、不操作硬件鼠标、不写仓库目录（tempfile 例外）；
受 Bandit B101 管控的 assert 逐条附 nosec 说明。
"""

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
from scripts.gui_smoke_impl import config as smoke_config, driver as smoke_driver
from scripts.gui_smoke_impl import convert_ui as smoke_convert_ui
from scripts.gui_smoke_impl import stages_convert as smoke_stages_convert
from scripts.markdown_acceptance_impl import gui as markdown_conversion_gui

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

from scripts.gui_smoke_tests.cases_smoke_logic import BundledRuntimePredicateTests, CancelledOutputTests, CompletionConfirmsNewRunTests, ExistingResultsPreservationTests, IsolationGuardTests, LatestEngineRecoveryTests, LayoutAndCoverageTests, OrganizeResultTests, OriginalDispositionTests, ProducedSnapshotTests, StageSelectionTests, StateDirectoryTests, TaskRecordTests

from scripts.gui_smoke_tests.cases_review_driver import ReviewRemediationCancellationLogTests, ReviewRemediationConfirmationTests, ReviewRemediationLayoutTests, WindowLocalClickTests


class OwnedProcessStartupTests(unittest.TestCase):
    def test_suspended_start_preserves_environment_stdio_and_flags(self) -> None:
        events: list[str] = []
        process = Mock(pid=123)
        owner = Mock()
        environment = {"isolated": "state"}

        def spawn(*args: object, **kwargs: object) -> Mock:
            events.append("spawn")
            self.assertEqual(args, (["worker.exe", "--service"],))
            self.assertIs(kwargs["env"], environment)
            self.assertEqual(kwargs["stdin"], subprocess.DEVNULL)
            self.assertEqual(kwargs["stdout"], subprocess.PIPE)
            self.assertEqual(kwargs["stderr"], subprocess.STDOUT)
            self.assertEqual(
                kwargs["creationflags"], gui_smoke.win32con.CREATE_SUSPENDED | subprocess.CREATE_NO_WINDOW
            )
            return process

        def bind(proc: object) -> Mock:
            self.assertIs(proc, process)
            events.append("bind")
            return owner

        def resume(pid: int) -> None:
            self.assertEqual(pid, 123)
            events.append("resume")

        with (
            patch.object(gui_smoke.subprocess, "Popen", side_effect=spawn),
            patch.object(smoke_driver, "OwnedProcessTree", side_effect=bind),
            patch.object(gui_smoke.process_runtime, "resume_owned_process", side_effect=resume),
        ):
            actual_process, actual_owner = gui_smoke.start_owned_process(
                ["worker.exe", "--service"], env=environment, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, creationflags=subprocess.CREATE_NO_WINDOW,
            )
        self.assertIs(actual_process, process)
        self.assertIs(actual_owner, owner)
        self.assertEqual(events, ["spawn", "bind", "resume"])
        owner.close.assert_not_called()

    def test_resume_failure_closes_owned_tree_before_root_and_pipes(self) -> None:
        events: list[str] = []
        process = Mock(pid=123)
        process.poll.return_value = None
        process.kill.side_effect = lambda: events.append("kill")
        process.wait.side_effect = lambda: events.append("wait")
        process.stdin = None
        process.stdout.close.side_effect = lambda: events.append("stdout")
        process.stderr.close.side_effect = lambda: events.append("stderr")
        owner = Mock()
        owner.close.side_effect = lambda: events.append("tree")
        with (
            patch.object(gui_smoke.subprocess, "Popen", return_value=process),
            patch.object(smoke_driver, "OwnedProcessTree", return_value=owner),
            patch.object(gui_smoke.process_runtime, "resume_owned_process", side_effect=OSError("恢复失败")),
            self.assertRaisesRegex(OSError, "恢复失败"),
        ):
            gui_smoke.start_owned_process(["worker.exe"])
        self.assertEqual(events, ["tree", "kill", "wait", "stdout", "stderr"])

    def test_binding_failure_reaps_suspended_root_without_resume(self) -> None:
        process = Mock(pid=123, stdin=None, stdout=None, stderr=None)
        process.poll.return_value = None
        with (
            patch.object(gui_smoke.subprocess, "Popen", return_value=process),
            patch.object(smoke_driver, "OwnedProcessTree", side_effect=OSError("归属失败")),
            patch.object(gui_smoke.process_runtime, "resume_owned_process") as resume,
            self.assertRaisesRegex(OSError, "归属失败"),
        ):
            gui_smoke.start_owned_process(["worker.exe"])
        resume.assert_not_called()
        process.kill.assert_called_once_with()
        process.wait.assert_called_once_with()

    def test_missing_current_isolation_evidence_prevents_pre_and_body(self) -> None:
        process = Mock(pid=123)
        process.wait.return_value = 0
        process.returncode = 0
        owner = Mock()
        before = Mock()
        body = Mock()
        after = Mock()
        with (
            patch.object(smoke_driver, "start_owned_process", return_value=(process, owner)),
            patch.object(gui_smoke.isolation_probe, "_isolation_log_snapshot", return_value=frozenset()),
            patch.object(gui_smoke.isolation_probe, "_wait_process_isolation", return_value=False),
            patch.object(smoke_driver, "wait_window", return_value=(None, Mock())),
            patch.object(smoke_driver, "close_app"),
            self.assertRaisesRegex(RuntimeError, "未证明隔离"),
        ):
            gui_smoke.run_stage("S1", "unused.exe", body, pre=before, after=after, env=ISOLATED_ENV)
        before.assert_not_called()
        body.assert_not_called()
        after.assert_called_once_with()
        owner.close.assert_called_once_with()


class ReviewAcceptanceStartupTests(unittest.TestCase):
    """P-11/P-13：真实 Python 父子进程重现 GUI 启动时的所有权及隔离边界。"""

    def setUp(self) -> None:
        workspace = Path(__file__).resolve().parent.parent / ".tmp" / "review-repairs" / "AcceptanceRegress"
        with (
            patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def _startup_result(
        self, *, create_child: bool, evidence: str, conversion: bool = False
    ) -> tuple[int, RuntimeError | None, bool, bool]:
        state = self.root / "state"
        state.mkdir()
        with contextlib.closing(sqlite3.connect(state / "config.sqlite3")) as database:
            database.execute("CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT)")
        logs = state / "logs"
        logs.mkdir()
        (logs / "jchtools.2026-10-17.1-1000-1.log").write_text(
            'component=logging event=application_started pid=1 role="gui"\n', encoding="utf-8"
        )
        source = (
            "import os, pathlib, subprocess, sys, time\n"
            "root = pathlib.Path(sys.argv[1]); state = root / 'state'\n"
            "child = None\n"
            "if sys.argv[2] == 'child':\n"
            "    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(900)'])\n"
            "mode = sys.argv[3]\n"
            "if mode != 'none':\n"
            "    role = 'gui' if mode != 'wrong-role' else 'xberg-broker'\n"
            "    pid = os.getpid() if mode != 'wrong-pid' else os.getpid() + 1\n"
            "    log = state / 'logs' / ('jchtools.2026-10-17.%s-1000-1.log' % os.getpid())\n"
            "    log.write_text('component=logging event=application_started pid=%s role=\"%s\"\\n' % (pid, role), "
            "encoding='utf-8')\n"
            "pending = root / 'ready.part'\n"
            "pending.write_text(str(child.pid if child else 0), encoding='ascii')\n"
            "pending.replace(root / 'ready')\n"
            "while not (root / 'close-parent').exists(): time.sleep(0.01)\n"
        )
        original_popen = cast("Callable[..., subprocess.Popen[bytes]]", subprocess.Popen)
        processes: list[subprocess.Popen[bytes]] = []
        child_handle: int | None = None
        body_calls: list[object] = []
        failure: RuntimeError | None = None
        unrelated = original_popen(
            [sys.executable, "-c", "import time; time.sleep(900)"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

        def wait_ready() -> int:
            deadline = time.monotonic() + 10
            while True:
                try:
                    return int((self.root / "ready").read_text(encoding="ascii"))
                except (FileNotFoundError, PermissionError, ValueError):
                    if time.monotonic() >= deadline:
                        raise TimeoutError("review startup helper did not reach its readiness barrier") from None
                    time.sleep(0.01)

        def spawn(_command: object, **kwargs: object) -> subprocess.Popen[bytes]:
            process = original_popen(
                [
                    sys.executable,
                    "-c",
                    source,
                    str(self.root),
                    "child" if create_child else "no-child",
                    evidence,
                ],
                **kwargs,
            )
            processes.append(process)
            # 允许普通 Popen 已能执行的启动代码完成，确定性模拟调用方此刻被抢占。
            # 暂停进程不等待：必须由真实启动 API 先绑定 Job 再恢复，才能到达同一屏障。
            if not cast("int", kwargs.get("creationflags", 0)) & gui_smoke.win32con.CREATE_SUSPENDED:
                wait_ready()
            return process

        def window(_pid: int) -> tuple[None, WindowSpecification]:
            nonlocal child_handle
            child_pid = wait_ready()
            if child_pid:
                child_handle = gui_smoke.win32api.OpenProcess(
                    gui_smoke.win32con.SYNCHRONIZE | gui_smoke.win32con.PROCESS_TERMINATE, 0, child_pid
                )
            return None, cast("WindowSpecification", object())

        def close(_window: object) -> None:
            (self.root / "close-parent").write_text("close", encoding="ascii")

        def configure(current_window: WindowSpecification, _inputs: Path, _outputs: Path) -> str:
            body_calls.append(current_window)
            return "review stops after real startup and isolation checks"

        environment = os.environ | {
            "JCHTOOLS_TEST_STATE_DIR": str(state),
            "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": str(self.root / "assets"),
        }

        try:
            with (
                patch("scripts.gui_smoke.subprocess.Popen", side_effect=spawn),
                patch.object(smoke_driver, "wait_window", side_effect=window),
                patch.object(smoke_driver, "close_app", side_effect=close),
                patch.object(smoke_config, "TIMEOUT", 0.2),
                patch.dict(os.environ, environment, clear=True),
                patch.object(markdown_conversion_gui, "_connect_window", side_effect=window),
                patch.object(markdown_conversion_gui, "_configure_conversion_paths", side_effect=configure),
                patch.object(markdown_conversion_gui, "_window_texts", return_value=""),
                patch.object(markdown_conversion_gui, "_request_close", side_effect=close),
            ):
                try:
                    if conversion:
                        run = markdown_acceptance.drive_conversion(
                            Path("unused-desktop-boundary.exe"), self.root / "input", self.root / "output"
                        )
                        self.assertEqual(run.error, "review stops after real startup and isolation checks")
                    else:
                        gui_smoke.run_stage(
                            "review-startup", "unused-desktop-boundary.exe", body_calls.append, env=environment
                        )
                except RuntimeError as error:
                    failure = error
            child_reaped = (
                child_handle is None
                or win32event.WaitForSingleObject(child_handle, 1000) == win32event.WAIT_OBJECT_0
            )
            return len(body_calls), failure, child_reaped, unrelated.poll() is None
        finally:
            # 保留断言前观测结果，再回收故障版本泄漏的 fixture 子进程，不污染下一用例。
            for process in processes:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=10)
            if child_handle is not None:
                try:
                    if win32event.WaitForSingleObject(child_handle, 0) == win32event.WAIT_TIMEOUT:
                        gui_smoke.win32api.TerminateProcess(child_handle, 0)
                    self.assertEqual(
                        win32event.WaitForSingleObject(child_handle, 10_000), win32event.WAIT_OBJECT_0
                    )
                finally:
                    gui_smoke.win32api.CloseHandle(child_handle)
            if unrelated.poll() is None:
                unrelated.terminate()
            unrelated.wait(timeout=10)

    def test_review_f56_startup_child_is_owned_before_first_parent_instruction(self) -> None:
        # P-13：子进程在 GUI 首条启动代码就派生；收尾必须回收它且不触碰其他实例。
        calls, failure, child_reaped, unrelated_alive = self._startup_result(create_child=True, evidence="fresh")
        self.assertIsNone(failure)
        self.assertEqual(calls, 1)
        self.assertTrue(unrelated_alive, "Job 清理不得终止其他实例")
        self.assertTrue(child_reaped, "Popen 返回到 Job 绑定之间启动的后台泄漏")

    def test_review_f56_conversion_startup_child_is_reaped_by_its_owner(self) -> None:
        # P-13：另一真实消费者也不能在首次执行之后才补绑 Job。
        calls, failure, child_reaped, unrelated_alive = self._startup_result(
            create_child=True, evidence="fresh", conversion=True
        )
        self.assertIsNone(failure)
        self.assertEqual(calls, 1)
        self.assertTrue(unrelated_alive)
        self.assertTrue(child_reaped, "转换驱动启动窗口内派生的后台必须被收尾")

    def test_review_f57_seeded_database_does_not_authorize_unisolated_gui(self) -> None:
        # P-11/P-13：旧 SQLite + 旧启动日志不能证明当前普通构建服从隔离环境。
        calls, failure, _child_reaped, unrelated_alive = self._startup_result(create_child=False, evidence="none")
        self.assertTrue(unrelated_alive)
        self.assertEqual(calls, 0, "无法证明当前 GUI PID 隔离前，不得执行业务阶段")
        self.assertIsNotNone(failure)

    def test_review_f57_current_pid_with_wrong_role_is_not_gui_isolation(self) -> None:
        # P-13/P-10：当前 PID 的 broker 角色日志也不能授权 GUI 操作。
        calls, failure, _child_reaped, unrelated_alive = self._startup_result(
            create_child=False, evidence="wrong-role"
        )
        self.assertTrue(unrelated_alive)
        self.assertEqual(calls, 0)
        self.assertIsNotNone(failure)

    def test_review_f57_wrong_pid_in_fresh_gui_log_does_not_authorize_business(self) -> None:
        # P-13/P-10：文件名属于当前进程，但启动事件 PID 不同，仍不能进入 GUI 业务。
        calls, failure, _child_reaped, unrelated_alive = self._startup_result(
            create_child=False, evidence="wrong-pid"
        )
        self.assertTrue(unrelated_alive)
        self.assertEqual(calls, 0)
        self.assertIsNotNone(failure)

    def test_review_f57_fresh_matching_gui_startup_evidence_remains_usable(self) -> None:
        # P-13：拒绝旧库不能变成无条件拒绝，真实新 PID/角色证据须允许阶段执行。
        calls, failure, _child_reaped, unrelated_alive = self._startup_result(create_child=False, evidence="fresh")
        self.assertIsNone(failure)
        self.assertEqual(calls, 1)
        self.assertTrue(unrelated_alive)
