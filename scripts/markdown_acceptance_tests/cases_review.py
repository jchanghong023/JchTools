"""转 Markdown 验收测试用例（自 scripts/test_markdown_acceptance.py 原样搬移）."""

from __future__ import annotations
import contextlib
import hashlib
import io
import json
import os
import posixpath
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING, cast
from unittest.mock import MagicMock, patch
import pywintypes
import win32api
import win32con
import win32event
from defusedxml import ElementTree
from pywinauto import timings
if TYPE_CHECKING:
    from collections.abc import Callable

    from pywinauto.application import WindowSpecification
from scripts import gui_smoke, isolation_probe, make_tmp, markdown_acceptance, media_e2e_fixture, test_gate, test_timing
from scripts.gui_smoke import find_button as smoke_find_button
from scripts.markdown_acceptance import unique_visible_buttons, verify_common_postconditions
from scripts.test_gate import acceptance_coverage_gaps

from scripts.markdown_acceptance_tests.helpers import (
    EXPECTED_COVERAGE_GAPS,
    EXPECTED_PHYSICAL_ROWS,
    AUTO_FAST_PAGES_THRESHOLD,
    ARGUMENT_ERROR_EXIT_CODE,
    COMMAND_TIMEOUT_SECONDS,
    COMMAND_FAILURE_EXIT_CODE,
    UNVERIFIED_EXIT_CODE,
    _bundled_package_fixture,
    report_acceptance,
    _content_spec,
    _content_problems,
    _per_input_problems,
    _input_rule,
    _a25_rules,
    _run_handler,
    _FakeRect,
    _FakeButton,
    _FakeWindow,
    _FakeSmokeWindow,
    _UnreadableConfirmCheckbox,
    _UnreadableConfirmWindow,
    _digest,
)
class ReviewRemediationAcceptanceTests(unittest.TestCase):
    """P2-08/P3-36/P3-37：从实际条目分派消费产物、隔离前置及退出证据。"""

    def setUp(self) -> None:
        with patch.object(
            sys,
            "argv",
            ["make_tmp.py", "workspace", "--destination", str(markdown_acceptance.TMP_ROOT)],
        ):
            self.assertEqual(make_tmp.main(), 0)

    @staticmethod
    def _context(root: Path, item: markdown_acceptance.Item) -> markdown_acceptance.Context:
        executable = root / "target" / "debug" / "JchTools.exe"
        executable.parent.mkdir(parents=True)
        executable.write_bytes(b"test-only GUI boundary")
        fixtures = root / "fixtures"
        fixtures.mkdir()
        for name in item.fixtures:
            source = fixtures / name
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_bytes(b"public synthetic acceptance source")
        return markdown_acceptance.Context(
            executable,
            None,
            None,
            fixtures,
            markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
            stages=gui_smoke.SUPPORTED_STAGES,
        )

    @staticmethod
    def _environment(root: Path) -> dict[str, str]:
        return os.environ | {
            "JCHTOOLS_TEST_STATE_DIR": str(root / "state"),
            "JCHTOOLS_TEST_ASSET_ROOT": str(root / "assets"),
            "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT": str(root / "assets"),
            "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": str(root / "assets"),
        }

    def _assert_report(
        self,
        root: Path,
        item: markdown_acceptance.Item,
        outcome: markdown_acceptance.Outcome,
        status: str,
        exit_code: int,
    ) -> None:
        # 其余条目作为已成功的控制组，避免 --only 的 NOT RUN 掩盖被测条目的退出分类。
        results = [
            (entry, outcome if entry.item_id == item.item_id else markdown_acceptance.Outcome("PASS"))
            for entry in markdown_acceptance.ITEMS
        ]
        report = root / "report.json"
        reporter = cast(
            "Callable[[list[tuple[markdown_acceptance.Item, markdown_acceptance.Outcome]], Path | None], int]",
            getattr(markdown_acceptance, "_" + "print_report"),
        )
        with patch("sys.stdout", new=io.StringIO()):
            code = reporter(results, report)
        payload = json.loads(report.read_text(encoding="utf-8"))
        observed = next(row for row in payload["items"] if row["id"] == item.item_id)
        self.assertEqual((observed["status"], code), (status, exit_code), observed["reason"])
        self.assertEqual(payload["summary"]["fail"], int(status == "FAIL"))
        self.assertEqual(payload["summary"]["not_run"], int(status == "NOT RUN"))

    def _svg_outcome(self, body: str) -> markdown_acceptance.Outcome:
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "A07")

        def convert(_exe: Path, _inputs: Path, outputs: Path, *, stop_after_busy: bool = False) -> object:
            del stop_after_busy
            media = outputs / "pptx_svg_pptx_media"
            media.mkdir()
            token_png = cast("Callable[[str], bytes]", getattr(markdown_acceptance, "_" + "token_png"))
            (media / "svg.png").write_bytes(token_png("SVG-LOCAL"))
            (media / "neighbor.png").write_bytes(token_png("PNG-NEIGHBOR"))
            product = outputs / "pptx_svg_pptx.md"
            product.write_text(
                f"![]({media.name}/svg.png)\n![]({media.name}/neighbor.png)\n{body}\n", encoding="utf-8"
            )
            return markdown_acceptance.GuiRun([product], "成功 1，部分提取 0，失败 0", None)

        with tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary:
            root = Path(temporary)
            context = self._context(root, item)
            with (
                patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                patch.object(runner, "drive_conversion", side_effect=convert),
                patch.dict(os.environ, self._environment(root), clear=True),
            ):
                return markdown_acceptance.run_item(item, context)

    def test_review_remediation_svg_rasterized_png_with_both_ocr_markers_passes(self) -> None:
        # P2-08 / T-13/T-14：SVG 的落盘后缀可变，两个来源的正文都必须存在。
        outcome = self._svg_outcome("SVG-LOCAL-TEXT\nPNG-NEIGHBOR")
        self.assertEqual(outcome.status, "PASS", outcome.reason)

    def test_review_remediation_svg_text_missing_with_healthy_media_fails(self) -> None:
        # P2-08：两张独立可解码 PNG 及健康引用不能弥补 SVG OCR 正文丢失。
        outcome = self._svg_outcome("PNG-NEIGHBOR")
        self.assertEqual(outcome.status, "FAIL", outcome.reason)
        self.assertIn("SVG-LOCAL", outcome.reason)

    def test_review_remediation_neighbor_text_missing_with_healthy_media_fails(self) -> None:
        # P2-08：收紧 SVG 判据时不能丢掉原有邻位 PNG 的正文要求。
        outcome = self._svg_outcome("SVG-LOCAL-TEXT")
        self.assertEqual(outcome.status, "FAIL", outcome.reason)
        self.assertIn("PNG-NEIGH", outcome.reason)

    def test_review_remediation_missing_isolation_reports_not_run_for_conversion_and_c05(self) -> None:
        # P3-36 / P-13：资产、GUI、夹具有效而缺一个必需隔离键，尚未执行业务。
        for item_id in ("A26", "C05"):
            with (
                self.subTest(item=item_id),
                tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary,
            ):
                root = Path(temporary)
                item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == item_id)
                context = self._context(root, item)
                environment = self._environment(root)
                del environment["JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"]
                with (
                    patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                    patch.dict(os.environ, environment, clear=True),
                    patch.object(runner, "manifest_has_python_entries", return_value=False),
                    patch.object(runner, "scan_python_modules", return_value=([], None)),
                ):
                    outcome = markdown_acceptance.run_item(item, context)
                self._assert_report(root, item, outcome, "NOT RUN", UNVERIFIED_EXIT_CODE)
                self.assertIn("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", outcome.reason)

    def test_review_remediation_real_conversion_error_remains_failed(self) -> None:
        # P3-36：不能把所有 GuiRun.error（包括真实执行失败）统一降为前置不足。
        for item_id in ("A26", "C05"):
            with (
                self.subTest(item=item_id),
                tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary,
            ):
                root = Path(temporary)
                item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == item_id)
                context = self._context(root, item)
                with (
                    patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                    patch.dict(os.environ, self._environment(root), clear=True),
                    patch.object(runner, "manifest_has_python_entries", return_value=False),
                    patch.object(runner, "scan_python_modules", return_value=([], None)),
                    patch.object(
                        runner,
                        "drive_conversion",
                        return_value=markdown_acceptance.GuiRun([], "失败 1", "review actual conversion failure"),
                    ),
                ):
                    outcome = markdown_acceptance.run_item(item, context)
                self._assert_report(root, item, outcome, "FAIL", 1)
                self.assertIn("review actual conversion failure", outcome.reason)

    def test_review_remediation_invalid_isolation_path_stays_failed(self) -> None:
        # 缺键才是未执行；用户提供非法隔离路径时仍须 FAIL，且不得启动 GUI。
        for item_id in ("A26", "C05", "B01"):
            with (
                self.subTest(item=item_id),
                tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary,
            ):
                root = Path(temporary)
                item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == item_id)
                context = self._context(root, item)
                environment = self._environment(root)
                environment["JCHTOOLS_TEST_STATE_DIR"] = "relative-unsafe-state"
                with (
                    patch.dict(os.environ, environment, clear=True),
                    patch.object(runner, "drive_conversion") as drive,
                    patch("scripts.markdown_acceptance.subprocess.run") as delegate,
                ):
                    outcome = markdown_acceptance.run_item(item, context)
                drive.assert_not_called()
                delegate.assert_not_called()
                self._assert_report(root, item, outcome, "FAIL", 1)
                self.assertIn("绝对隔离路径", outcome.reason)

    def test_review_remediation_unknown_conversion_exception_stays_failed(self) -> None:
        # 已满足隔离前置后的未知执行异常，不得被当成 NOT RUN。
        for item_id in ("A26", "C05"):
            with (
                self.subTest(item=item_id),
                tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary,
            ):
                root = Path(temporary)
                item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == item_id)
                context = self._context(root, item)
                with (
                    patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                    patch.dict(os.environ, self._environment(root), clear=True),
                    patch.object(runner, "manifest_has_python_entries", return_value=False),
                    patch.object(runner, "scan_python_modules", return_value=([], None)),
                    patch.object(runner, "drive_conversion", side_effect=LookupError("review unknown failure")),
                ):
                    outcome = markdown_acceptance.run_item(item, context)
                self._assert_report(root, item, outcome, "FAIL", 1)
                self.assertIn("LookupError: review unknown failure", outcome.reason)

    @staticmethod
    def _delegate_consumer(root: Path, *, fail: bool = False) -> Path:
        # 仅替换桌面操作边界；子进程实际执行已有隔离守卫并以真实退出码返回。
        script = root / "delegated_consumer.py"
        script.write_text(
            "import os, sys\n"
            f"sys.path.insert(0, {str(markdown_acceptance.ROOT)!r})\n"
            "from scripts import gui_smoke\n"
            "stage = sys.argv[sys.argv.index('--stages') + 1]\n"
            "gui_smoke.require_orchestrated_isolation([stage], os.environ, allow_isolated_run=False)\n"
            f"sys.exit({COMMAND_FAILURE_EXIT_CODE if fail else 0})\n",
            encoding="utf-8",
        )
        return script

    def test_review_remediation_guarded_delegate_missing_isolation_reports_not_run(self) -> None:
        # P3-36：经 B 组真实分派、委托及最终 JSON 报告消费前置分类。
        with tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary:
            root = Path(temporary)
            item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "B01")
            context = self._context(root, item)
            environment = self._environment(root)
            del environment["JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"]
            with (
                patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                patch.object(runner, "GUI_SMOKE", self._delegate_consumer(root)),
                patch.dict(os.environ, environment, clear=True),
            ):
                outcome = markdown_acceptance.run_item(item, context)
            self._assert_report(root, item, outcome, "NOT RUN", UNVERIFIED_EXIT_CODE)
            self.assertIn("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT", outcome.reason)

    def test_review_remediation_delegate_execution_error_remains_failed(self) -> None:
        # P3-36：环境有效后的真实子进程非零退出仍是 FAIL/1。
        with tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary:
            root = Path(temporary)
            item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "B01")
            context = self._context(root, item)
            with (
                patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                patch.object(runner, "GUI_SMOKE", self._delegate_consumer(root, fail=True)),
                patch.dict(os.environ, self._environment(root), clear=True),
            ):
                outcome = markdown_acceptance.run_item(item, context)
            self._assert_report(root, item, outcome, "FAIL", 1)
            self.assertIn(str(COMMAND_FAILURE_EXIT_CODE), outcome.reason)

    def test_review_remediation_self_isolating_stages_and_c03_remain_runnable(self) -> None:
        # P3-36：S6-S9 自建环境；C03 的 env_extra 必须在委托守卫前合入有效环境。
        for item_id in ("B02", "B03", "B04", "B05", "C03"):
            with (
                self.subTest(item=item_id),
                tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary,
            ):
                root = Path(temporary)
                item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == item_id)
                context = self._context(root, item)
                environment = self._environment(root)
                for key in gui_smoke.ISOLATION_REQUIRED_ENV_KEYS:
                    del environment[key]
                with (
                    patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                    patch.object(runner, "GUI_SMOKE", self._delegate_consumer(root)),
                    patch.dict(os.environ, environment, clear=True),
                ):
                    outcome = markdown_acceptance.run_item(item, context)
                self._assert_report(root, item, outcome, "PASS", 0)

    @staticmethod
    def _start_close_process(root: Path, exit_code: int) -> tuple[subprocess.Popen[bytes], Callable[[], None], int]:
        source = (
            "import pathlib, subprocess, sys, time\n"
            "root = pathlib.Path(sys.argv[1])\n"
            "while not (root / 'start-parent').exists(): time.sleep(0.01)\n"
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(900)'])\n"
            "pending = root / 'child-pid.part'\n"
            "pending.write_text(str(child.pid), encoding='ascii')\n"
            "pending.replace(root / 'child-pid')\n"
            "while not (root / 'close-parent').exists(): time.sleep(0.01)\n"
            "sys.exit(int(sys.argv[2]))\n"
        )
        process, owner = gui_smoke.start_owned_process(
            [sys.executable, "-c", source, str(root), str(exit_code)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            (root / "start-parent").write_text("start", encoding="ascii")
            deadline = time.monotonic() + 10
            while True:
                try:
                    pid = int((root / "child-pid").read_text(encoding="ascii"))
                    break
                except (FileNotFoundError, PermissionError, ValueError):
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError("review owned GUI did not create its child") from None
                    time.sleep(0.01)
            child = win32api.OpenProcess(win32con.SYNCHRONIZE, 0, pid)
            return process, owner.close, child
        except Exception:
            try:
                owner.close()
            finally:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=10)
            raise

    def _close_outcome(self, mode: str) -> None:
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "A26")
        exit_code = COMMAND_FAILURE_EXIT_CODE if mode == "nonzero" else 0
        with tempfile.TemporaryDirectory(prefix="review-acceptance-", dir=markdown_acceptance.TMP_ROOT) as temporary:
            root = Path(temporary)
            context = self._context(root, item)
            process, close_owned, child = self._start_close_process(root, exit_code)
            original_wait = process.wait

            def bounded_wait(timeout: float | None = None) -> int:
                # 只收窄测试进程的等待预算；仍由真实 Popen 取得退出码或 TimeoutExpired。
                return original_wait(timeout=0.8 if timeout is None else min(timeout, 0.8))

            def click(_window: object, title: str) -> None:
                if title != "开始转换":
                    return
                inputs = root / "scratch" / "a26-run" / "input"
                outputs = inputs.parent / "output"
                for source in inputs.iterdir():
                    product = outputs / f"{source.stem}_{source.suffix.lstrip('.')}.md"
                    product.write_text("公开合成转换正文\n", encoding="utf-8")

            def button(_window: object, title: str) -> tuple[bool, bool]:
                return title == "开始转换", True

            def request_close(_window: object) -> None:
                if mode != "live":
                    (root / "close-parent").write_text("WM_CLOSE delivered", encoding="ascii")
                if mode == "nontransient":
                    raise RuntimeError("review nontransient close failure")
                raise timings.TimeoutError("review stale UIA after WM_CLOSE")

            def release_job() -> None:
                close_owned()
                if mode == "job-failure":
                    raise RuntimeError("review Job cleanup failure")

            # drive_conversion 自己保存并执行此 close，真实 Job 的创建、绑定与回收不替换。
            owner = MagicMock()
            owner.close = release_job
            try:
                with (
                    patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                    patch.dict(os.environ, self._environment(root), clear=True),
                    patch.object(gui, "start_owned_process", return_value=(process, owner)),
                    patch.object(gui, "_connect_window", return_value=(None, object())),
                    patch.object(isolation_probe, "_wait_process_isolation", return_value=True),
                    patch.object(gui, "_configure_conversion_paths", return_value=None),
                    patch.object(gui, "_click_button", side_effect=click),
                    patch.object(gui, "_button_state", side_effect=button),
                    patch.object(
                        gui,
                        "_window_texts",
                        side_effect=["尚未开始", "成功 3，部分提取 0，失败 0"],
                    ),
                    patch.object(gui, "_request_close", side_effect=request_close),
                    patch.object(process, "wait", side_effect=bounded_wait),
                ):
                    outcome = markdown_acceptance.run_item(item, context)
                self.assertIsNotNone(process.poll(), "条目返回前必须回收自己的 GUI")
                # Job 关闭/终止后，内核将子进程句柄置为 signaled 是异步的。
                # 只同步等待，不在断言前另行终止子进程，以免掩盖真实回收失败。
                self.assertEqual(
                    win32event.WaitForSingleObject(child, 10_000),
                    win32event.WAIT_OBJECT_0,
                    "真实 Job 所属子进程必须在有界等待内退出",
                )
                expected_status = "PASS" if mode == "clean" else "FAIL"
                self._assert_report(root, item, outcome, expected_status, 0 if mode == "clean" else 1)
                if mode != "live":
                    self.assertEqual(process.returncode, exit_code)
                if mode == "nontransient":
                    self.assertIn("review nontransient close failure", outcome.reason)
                elif mode == "job-failure":
                    self.assertIn("review Job cleanup failure", outcome.reason)
            finally:
                try:
                    close_owned()
                finally:
                    try:
                        if process.poll() is None:
                            process.kill()
                        original_wait(timeout=10)
                    finally:
                        try:
                            # 由 fixture 自有 Job 回收；SYNCHRONIZE 句柄仅用于等待，
                            # 不得用它 TerminateProcess，也不按 PID 重开并误杀无关进程。
                            self.assertEqual(
                                win32event.WaitForSingleObject(child, 10_000),
                                win32event.WAIT_OBJECT_0,
                                "fixture 自有 Job 清理后子进程仍未退出",
                            )
                        finally:
                            win32api.CloseHandle(child)

    def test_review_remediation_transient_close_with_natural_exit_zero_preserves_success(self) -> None:
        # P3-37：成功产物不能被 WM_CLOSE 后 UIA 销毁竞态覆盖为 FAIL。
        self._close_outcome("clean")

    def test_review_remediation_transient_close_with_live_process_stays_failed(self) -> None:
        # P3-37：清理强杀不是自然成功退出，且真实子进程仍必须被 Job 回收。
        self._close_outcome("live")

    def test_review_remediation_transient_close_with_nonzero_exit_stays_failed(self) -> None:
        # P3-37：只有自然 exit 0 才能忽略瞬态异常，非零退出仍为 FAIL/1。
        self._close_outcome("nonzero")

    def test_review_remediation_nontransient_close_with_exit_zero_stays_failed(self) -> None:
        # P3-37：不得以 exit 0 为由无条件 suppress 所有关闭异常。
        self._close_outcome("nontransient")

    def test_review_remediation_job_cleanup_failure_with_exit_zero_stays_failed(self) -> None:
        # P3-37：瞬态 UIA 可忽略也不能吞掉真实 Job 收尾异常。
        self._close_outcome("job-failure")

    def test_review_remediation_naturally_exited_process_skips_stale_window(self) -> None:
        # 已观察到自然 exit 0，不再访问可能已销毁的窗口句柄；仍执行所属 Job 收尾。
        process = subprocess.Popen([sys.executable, "-c", "pass"])
        try:
            self.assertEqual(process.wait(timeout=10), 0)
            release_job = MagicMock()
            with patch.object(
                gui, "_request_close", side_effect=AssertionError("stale window accessed")
            ) as request:
                markdown_acceptance._close_conversion_gui(process, cast("WindowSpecification", object()), release_job)
            request.assert_not_called()
            release_job.assert_called_once_with()
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)


class ReviewAcceptanceContentTests(unittest.TestCase):
    """P-12/P-13：验收驱动须拒绝真实公开夹具对应的缺内容产物。"""

    def setUp(self) -> None:
        workspace = markdown_acceptance.ROOT / ".tmp" / "review-repairs" / "AcceptanceRegress"
        with (
            patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def _context(self, executable: Path) -> markdown_acceptance.Context:
        executable.parent.mkdir(parents=True, exist_ok=True)
        executable.write_bytes(b"unused desktop boundary; never executed")
        return markdown_acceptance.Context(
            executable,
            None,
            None,
            markdown_acceptance.ROOT / "tests" / "markdown_fixtures",
            markdown_acceptance.AssetProbe(self.root, self.root, self.root / "xberg.exe", self.root, []),
            stages=gui_smoke.SUPPORTED_STAGES,
        )

    @staticmethod
    def _source_text(archive: zipfile.ZipFile) -> str:
        """正控只读取源 XML/嵌入容器；不调用待测 ContentSpec 构造器或读取产品输出。"""
        parts: list[str] = []
        for name in archive.namelist():
            if name.endswith(".xml"):
                tree = ElementTree.fromstring(archive.read(name))
                parts.extend(
                    node.text.strip()
                    for node in tree.iter()
                    if isinstance(node.tag, str)
                    and node.tag.rsplit("}", 1)[-1] in ("t", "v")
                    and node.text
                    and node.text.strip()
                )
            elif "/embeddings/" in name and name.endswith((".docx", ".pptx", ".xlsx")):
                with zipfile.ZipFile(io.BytesIO(archive.read(name))) as embedded:
                    parts.append(ReviewAcceptanceContentTests._source_text(embedded))
        return "\n".join(parts)

    def _healthy_product(self, source: Path, output: Path) -> Path:
        product = output / f"{source.stem}_{source.suffix[1:].lower()}.md"
        media = output / f"{product.stem}_media"
        text = [f"# {source.name}"]
        with zipfile.ZipFile(source) as archive:
            text.append(self._source_text(archive))
            for index, member in enumerate(name for name in archive.namelist() if "/media/" in name):
                media.mkdir(exist_ok=True)
                target = media / f"image-{index}{Path(member).suffix}"
                target.write_bytes(archive.read(member))
                text.append(f"![]({media.name}/{target.name})")
        if source.name in ("pptx_placeable_wmf.pptx", "pptx_standard_wmf.pptx"):
            text.append("dft_se dft_clk")
        elif source.name == "pptx_emf.pptx":
            text.append("Redundant Memory\nMBIST controller\nEFUSE")
        elif source.suffix == ".xlsb":
            text.append("Welcome to File Extension FYI Center\nActive Server Pages\nPortable Network Graphics")
        product.write_text("\n".join(text), encoding="utf-8")
        return product

    def _artifact_control(
        self,
        item_id: str,
        profile: str,
        mutate: Callable[[list[Path], Path], str] | None = None,
    ) -> markdown_acceptance.Outcome:
        item = next(entry for entry in markdown_acceptance.profile_items(profile) if entry.item_id == item_id)
        context = self._context(self.root / "cargo-cache" / "debug" / "JchTools.exe")
        completed: list[int] = []

        def collect(
            _exe: Path, inputs: Path, outputs: Path, *, stop_after_busy: bool = False
        ) -> markdown_acceptance.GuiRun:
            self.assertFalse(stop_after_busy)
            products = [self._healthy_product(source, outputs) for source in sorted(inputs.iterdir()) if source.is_file()]
            self.assertTrue(products, "正控必须采集每个源的健康内容")
            diagnostic = mutate(products, inputs) if mutate is not None else ""
            completed.append(len(products))
            partial_count = len(products) if diagnostic.startswith("部分提取：") else 0
            texts = f"成功 {len(products) - partial_count}，部分提取 {partial_count}，失败 0\n{diagnostic}"
            return markdown_acceptance.GuiRun(products, texts, None)

        with (
            patch.object(runner, "SCRATCH_ROOT", self.root / "scratch"),
            patch.dict(os.environ, ReviewRemediationAcceptanceTests._environment(self.root), clear=True),
            patch.object(runner, "drive_conversion", side_effect=collect) as capture,
        ):
            outcome = markdown_acceptance.run_item(item, context)
        capture.assert_called_once()
        self.assertEqual(len(completed), 1, "采集回调必须完整返回，不能以无关异常 FAIL 冒充内容反例")
        self.assertNotIn("源图/产物语义核对失败", outcome.reason)
        return outcome

    def _healthy_content_control(self, item_id: str) -> None:
        for profile in ("common", "full"):
            with self.subTest(item=item_id, profile=profile, control="healthy"):
                outcome = self._artifact_control(item_id, profile)
                self.assertEqual(outcome.status, "PASS", outcome.reason)


    def test_review_f58_custom_cargo_layout_still_rejects_automatic_asset_writes(self) -> None:
        # T-05/T-21/T-30/P-13：自定义 Cargo 路径不能让 C03 跳过下载反例判定。
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C03")
        context = self._context(self.root / "cargo-cache" / "debug" / "JchTools.exe")
        completed: list[Path] = []

        def unexpected_download(_item: str, _target: Path, environment: dict[str, str]) -> None:
            assets = Path(environment["JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT"])
            (assets / "unexpected-runtime.bin").write_bytes(b"not allowed during ordinary startup")
            completed.append(assets)

        with (
            patch.object(runner, "SCRATCH_ROOT", self.root / "scratch"),
            patch.object(runner, "_c03_delegate_stages", side_effect=unexpected_download) as delegate,
        ):
            outcome = markdown_acceptance.run_item(item, context)
        delegate.assert_called_once()
        self.assertEqual(len(completed), 1, "C03 必须完整执行委托采集，目录名不得使它跳过")
        self.assertEqual(outcome.status, "FAIL", outcome.reason)
        self.assertIn("unexpected-runtime.bin", outcome.reason)

    def test_review_f58_custom_cargo_layout_runs_healthy_c03_and_preserves_isolation_failure(self) -> None:
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C03")
        context = self._context(self.root / "cargo-cache" / "custom-profile" / "JchTools.exe")
        for blocked in (False, True):
            with self.subTest(blocked=blocked):
                result = markdown_acceptance.Outcome("FAIL", "本轮 GUI PID 未证明隔离") if blocked else None
                with (
                    patch.object(runner, "SCRATCH_ROOT", self.root / "scratch"),
                    patch.object(runner, "_c03_delegate_stages", return_value=result) as delegate,
                ):
                    outcome = markdown_acceptance.run_item(item, context)
                delegate.assert_called_once()
                self.assertEqual(outcome.status, "FAIL" if blocked else "PASS", outcome.reason)
                if blocked:
                    self.assertIn("本轮 GUI PID", outcome.reason)

    def _reject_missing_content(self, item_id: str) -> None:
        # T-13/T-14/T-15/T-16/附录 A：只替换桌面结果采集边界，使用在库真实合成源。
        self._healthy_content_control(item_id)
        for profile in ("common", "full"):
            for body in ("", "\nUnrelated nonempty paragraph without any source content.\n"):
                with self.subTest(item=item_id, profile=profile, unrelated_body=bool(body)):
                    item = next(
                        entry for entry in markdown_acceptance.profile_items(profile) if entry.item_id == item_id
                    )
                    context = self._context(self.root / "target" / "debug" / "JchTools.exe")
                    completed: list[int] = []

                    def collect_bad_artifacts(
                        _exe: Path, inputs: Path, outputs: Path, *, stop_after_busy: bool = False
                    ) -> markdown_acceptance.GuiRun:
                        del stop_after_busy
                        products: list[Path] = []
                        for source in sorted(inputs.iterdir()):
                            if not source.is_file():
                                continue
                            product = outputs / f"{source.stem}_{source.suffix[1:].lower()}.md"
                            product.write_text(f"# {source.name}\n{body}", encoding="utf-8")
                            products.append(product)
                        self.assertTrue(products, "负例必须包含每个健康输入对应的非空产物")
                        completed.append(len(products))
                        return markdown_acceptance.GuiRun(products, f"成功 {len(products)}，部分提取 0，失败 0", None)

                    with (
                        patch.object(runner, "SCRATCH_ROOT", self.root / "scratch"),
                        patch.dict(
                            os.environ,
                            ReviewRemediationAcceptanceTests._environment(self.root),
                            clear=True,
                        ),
                        patch.object(runner, "drive_conversion", side_effect=collect_bad_artifacts) as capture,
                    ):
                        outcome = markdown_acceptance.run_item(item, context)
                    capture.assert_called_once()
                    self.assertEqual(len(completed), 1, "负例采集回调必须完成，不能靠无关异常误绿")
                    self.assertNotIn("源图/产物语义核对失败", outcome.reason)
                    self.assertRegex(outcome.reason, "正文|源媒体|部分提取")
                    self.assertEqual(
                        outcome.status,
                        "FAIL",
                        f"{item_id}/{profile} 不得把仅文件名标题或无关段落当作源内容：{outcome}",
                    )
                    self.assertTrue(outcome.reason)

    def test_review_f59_a04_placeable_wmf_requires_source_content(self) -> None:
        self._reject_missing_content("A04")

    def test_review_f59_a05_standard_wmf_requires_source_content(self) -> None:
        self._reject_missing_content("A05")

    def test_review_f59_a06_emf_requires_source_content(self) -> None:
        self._reject_missing_content("A06")

    def test_review_f59_a21_word_requires_source_content(self) -> None:
        self._reject_missing_content("A21")

    def test_review_f59_a22_powerpoint_requires_source_content(self) -> None:
        self._reject_missing_content("A22")

    def test_review_f59_a23_spreadsheet_requires_source_content(self) -> None:
        self._reject_missing_content("A23")

    def test_review_f59_table_values_must_remain_in_their_source_rows(self) -> None:
        for item_id in ("A21", "A22", "A23"):
            with self.subTest(item=item_id):
                def swap_rows(products: list[Path], _inputs: Path) -> str:
                    changed = 0
                    for product in products:
                        text = product.read_text(encoding="utf-8")
                        if "40A" in text and "50A" in text:
                            product.write_text(
                                text.replace("40A", "SWAPPED-AMPS").replace("50A", "40A").replace("SWAPPED-AMPS", "50A"),
                                encoding="utf-8",
                            )
                            changed += 1
                    self.assertGreater(changed, 0, "反例必须包含源表格")
                    return ""

                healthy = self._artifact_control(item_id, "common")
                self.assertEqual(healthy.status, "PASS", healthy.reason)
                outcome = self._artifact_control(item_id, "common", swap_rows)
                self.assertEqual(outcome.status, "FAIL", outcome.reason)
                self.assertIn("正文", outcome.reason)

    def test_review_f59_missing_single_input_cannot_borrow_other_file_content(self) -> None:
        for item_id in ("A21", "A22", "A23"):
            with self.subTest(item=item_id):
                def move_content(products: list[Path], _inputs: Path) -> str:
                    self.assertGreater(len(products), 1, "完整矩阵必须覆盖多个独立输入")
                    lost = products[-1].read_text(encoding="utf-8")
                    products[0].write_text(products[0].read_text(encoding="utf-8") + "\n" + lost, encoding="utf-8")
                    products[-1].write_text(f"# {products[-1].stem}\nUnrelated text\n", encoding="utf-8")
                    return ""

                healthy = self._artifact_control(item_id, "full")
                self.assertEqual(healthy.status, "PASS", healthy.reason)
                outcome = self._artifact_control(item_id, "full", move_content)
                self.assertEqual(outcome.status, "FAIL", outcome.reason)
                self.assertIn("正文", outcome.reason)

    def test_review_f59_vector_media_without_ocr_is_not_content_success(self) -> None:
        for item_id in ("A04", "A05", "A06"):
            with self.subTest(item=item_id):
                def remove_ocr(products: list[Path], _inputs: Path) -> str:
                    for product in products:
                        references = [line for line in product.read_text(encoding="utf-8").splitlines() if line.startswith("![](")]
                        product.write_text(f"# {product.stem}\n" + "\n".join(references), encoding="utf-8")
                    return ""

                outcome = self._artifact_control(item_id, "common", remove_ocr)
                self.assertEqual(outcome.status, "FAIL", outcome.reason)
                self.assertRegex(outcome.reason, "正文|部分提取")

    def test_review_f59_wrong_source_media_is_rejected(self) -> None:
        for item_id in ("A04", "A05", "A06", "A21"):
            with self.subTest(item=item_id):
                def replace_media(products: list[Path], _inputs: Path) -> str:
                    pictures = sorted(products[0].parent.rglob("*_media/*"))
                    self.assertTrue(pictures, "反例必须替换真实源媒体，而非跳过媒体采集")
                    for picture in pictures:
                        picture.write_bytes(b"unrelated invalid image, not source media")
                    return ""

                healthy = self._artifact_control(item_id, "common")
                self.assertEqual(healthy.status, "PASS", healthy.reason)
                outcome = self._artifact_control(item_id, "common", replace_media)
                self.assertEqual(outcome.status, "FAIL", outcome.reason)
                self.assertIn("源媒体", outcome.reason)

    def test_review_f59_vector_partial_diagnosis_is_bound_to_host_member_and_stage(self) -> None:
        for item_id in ("A04", "A05", "A06"):
            for mismatch in ("", "index", "host", "member", "stage", "ocr_without_media"):
                with self.subTest(item=item_id, mismatch=mismatch):
                    def partial(products: list[Path], inputs: Path) -> str:
                        source = next(inputs.iterdir())
                        with zipfile.ZipFile(source) as archive:
                            member = next(name for name in archive.namelist() if "/media/" in name)
                        for product in products:
                            product.write_text(f"# {source.name}\n其他可提取内容\n", encoding="utf-8")
                        for media in products[0].parent.glob("*_media"):
                            for picture in media.iterdir():
                                picture.unlink()
                            media.rmdir()
                        host = "unrelated.pptx" if mismatch == "host" else source.name
                        name = "unrelated.emf" if mismatch == "member" else Path(member).name
                        if mismatch == "index":
                            name = "Image 0"
                        stage = "未知异常" if mismatch == "stage" else "光栅化失败"
                        if mismatch == "ocr_without_media":
                            stage = "OCR 无文字"
                        return f"部分提取：{host} · {name} · {stage}"

                    outcome = self._artifact_control(item_id, "common", partial)
                    rejected = mismatch not in ("", "index")
                    self.assertEqual(outcome.status, "FAIL" if rejected else "PASS", outcome.reason)
                    if rejected:
                        self.assertIn("部分提取", outcome.reason)

    def test_review_f59_unrelated_capture_exception_is_not_a_content_counterexample(self) -> None:
        item = next(entry for entry in markdown_acceptance.profile_items("common") if entry.item_id == "A21")
        context = self._context(self.root / "cargo-cache" / "debug" / "JchTools.exe")
        with (
            patch.object(runner, "SCRATCH_ROOT", self.root / "scratch"),
            patch.dict(os.environ, ReviewRemediationAcceptanceTests._environment(self.root), clear=True),
            patch.object(runner, "drive_conversion", side_effect=LookupError("unrelated capture failure")),
        ):
            outcome = markdown_acceptance.run_item(item, context)
        self.assertEqual(outcome.status, "FAIL")
        self.assertIn("LookupError: unrelated capture failure", outcome.reason)
        self.assertNotIn("正文缺少", outcome.reason)
