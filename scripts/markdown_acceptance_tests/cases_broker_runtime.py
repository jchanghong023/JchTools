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

from scripts.markdown_acceptance_impl import formats, runner
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
class DynamicFormatAcceptanceTests(unittest.TestCase):
    """覆盖 T-08/XB-14/XB-26：实际引擎能力决定产物规则，静态清单不能代替查询。."""

    @staticmethod
    def _evaluate(
        declared: tuple[str, ...], *, failed: tuple[str, ...] = (), error: str | None = None, item_id: str = "A18"
    ) -> markdown_acceptance.Outcome:
        rows: list[object] = [
            {"extension": name, "mime_type": "video/mj2" if name == "mj2" else "application/test"}
            for name in (*declared, "pdf", "docx", "pptx", "xlsx")
        ]
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == item_id)
        # A25 会改 fixtures；本用例使用独立表项，避免影响其他矩阵消费者。
        item = markdown_acceptance.Item(
            item.item_id, item.group, item.title, item.entry, item.fixtures, needs_assets=item.needs_assets
        )

        def convert(_exe: Path, inputs: Path, outputs: Path, *, stop_after_busy: bool = False) -> object:
            del stop_after_busy
            products: list[Path] = []
            failures = 0
            for source in inputs.iterdir():
                extension = source.suffix.lstrip(".").lower()
                if extension in failed:
                    failures += 1
                    continue
                if extension not in declared:
                    continue
                product = outputs / f"{source.stem}_{extension}.md"
                _ = product.write_text("公开合成转换正文\n", encoding="utf-8")
                products.append(product)
            summary = f"转换完成：成功 {len(products)}，部分提取 0，失败 {failures}"
            return markdown_acceptance.GuiRun(products, summary, None)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            context = markdown_acceptance.Context(
                root / "JchTools.exe",
                None,
                None,
                markdown_acceptance.FIXTURES_DEFAULT,
                markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
            )
            with (
                patch.dict(
                    os.environ,
                    gui_smoke.isolated_state_env(markdown_acceptance.TMP_ROOT / "dynamic-format-tests"),
                    clear=True,
                ),
                patch.dict(_a25_rules(), clear=True),
                patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                patch.object(runner, "_resolve_gui", return_value=(context.gui_exe, None)),
                patch.object(runner, "_asset_precondition", return_value=None),
                patch.object(runner, "_shared_format_rows", return_value=(rows, error)),
                patch.object(runner, "drive_conversion", side_effect=convert),
            ):
                return markdown_acceptance.run_item(item, context)

    def test_runtime_supported_jpx_is_verified_as_supported(self) -> None:
        outcome = self._evaluate(("jp2", "j2k", "j2c", "jpx", "mj2"))
        assert outcome.status == "PASS", outcome.reason  # nosec B101: 当前引擎支持 JPX 时须验其真实产物。

    def test_runtime_supported_jpx_without_product_fails(self) -> None:
        outcome = self._evaluate(("jp2", "j2k", "j2c", "jpx"), failed=("jpx",))
        assert outcome.status == "FAIL"  # nosec B101: 声明支持却转换失败不能冒充 unsupported 跳过。

    def test_undeclared_jpx_counted_as_failure_is_rejected(self) -> None:
        outcome = self._evaluate(("jp2", "j2k", "j2c"), failed=("jpx",))
        assert outcome.status == "FAIL"  # nosec B101: T-07 未声明格式不可进入失败计数冒充已排除。
        assert "批次统计计入 4" in outcome.reason  # nosec B101: 核对实际统计，不仅核对有无文件。

    def test_missing_runtime_formats_cannot_use_stale_static_manifest(self) -> None:
        outcome = self._evaluate(("jp2", "j2k", "j2c"), error="当前共享引擎清单不可验证")
        assert outcome.status == "NOT RUN"  # nosec B101: 旧固定清单不得替代缺失的当前能力证明。

    def test_a25_queries_shared_broker_without_starting_another_engine(self) -> None:
        with patch(
            "scripts.markdown_acceptance.subprocess.run",
            side_effect=AssertionError("不得单独启动 xberg formats"),
        ) as direct_query:
            outcome = self._evaluate(("rst",), item_id="A25")
        assert outcome.status == "PASS", outcome.reason  # nosec B101: 消费共享清单并实际核对 rst 产物。
        direct_query.assert_not_called()


class ClosedFormatConnectionTests(unittest.TestCase):
    def test_closed_previous_broker_is_replaced_before_formats_request(self) -> None:
        """覆盖 P-13/XB-14：已退出代理的遗留端点不能冒充有效连接。."""
        root = markdown_acceptance.ROOT / ".tmp" / "release-validation" / "pipe-regression"
        state = root / "state"
        context = markdown_acceptance.Context(
            root / "JchTools.exe",
            None,
            None,
            markdown_acceptance.FIXTURES_DEFAULT,
            markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
        )
        old_pipe, live_pipe = MagicMock(), MagicMock()
        close_old = MagicMock()
        old_pipe.Close = close_old
        old_pipe.handle, live_pipe.handle = 1, 2
        process = MagicMock()
        process.pid = 987
        expected = [
            {"extension": extension, "mime_type": "application/test"} for extension in ("pdf", "docx", "pptx", "xlsx")
        ]

        def exchange(pipe: object, _runtime: Path, request_id: str, _deadline: float) -> object:
            if pipe is old_pipe:
                raise pywintypes.error(232, "WriteFile", "管道正在被关闭")
            return {"id": request_id, "ok": True, "jchtools_broker_protocol": 3, "formats": expected}

        with (
            patch.dict(
                os.environ,
                {
                    "JCHTOOLS_TEST_STATE_DIR": str(state),
                    "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT": str(root / "assets"),
                    "JCHTOOLS_TEST_ASSET_ROOT": str(root / "assets"),
                    "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": str(root / "snap"),
                },
            ),
            patch("win32file.CreateFile", side_effect=[old_pipe, live_pipe]),
            patch("win32pipe.GetNamedPipeServerProcessId", side_effect=[111, 987]),
            patch("win32api.OpenProcess", return_value=123),
            patch("win32api.CloseHandle"),
            patch("win32event.WaitForSingleObject", side_effect=[win32event.WAIT_OBJECT_0, win32event.WAIT_TIMEOUT]),
            patch.object(formats, "_resolve_gui", return_value=(root / "JchTools.exe", None)),
            patch.object(isolation_probe, "_wait_process_isolation", return_value=True),
            patch.object(formats, "start_owned_process", return_value=(process, MagicMock())) as start,
            patch.object(formats, "_exchange_formats", side_effect=exchange) as request,
        ):
            rows, error = context.cached_formats()
        assert error is None, error  # nosec B101: 关闭的旧端点必须在请求前被识别。
        assert rows == expected  # nosec B101: 必须消费本轮有效代理的协议响应。
        start.assert_called_once()
        request.assert_called_once()
        close_old.assert_called_once()


class SharedFormatProtocolTests(unittest.TestCase):
    """失效/异版本 broker 响应必须使真实 A18 消费者 NOT RUN，不能使用旧固定清单。."""

    def test_current_broker_protocol_accepts_formats(self) -> None:
        """覆盖 T-08/XB-05：当前协议 3 的真实终态结构不能被旧版本判据拒绝。"""
        root = markdown_acceptance.ROOT / ".tmp" / "acceptance-protocol-regression"
        context = markdown_acceptance.Context(
            None, None, None, markdown_acceptance.FIXTURES_DEFAULT,
            markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
        )
        expected = [{"extension": "pdf", "mime_type": "application/pdf"}]

        def reply(_pipe: object, _runtime: Path, request_id: str, _deadline: float) -> object:
            return {"id": request_id, "ok": True, "jchtools_broker_protocol": 3, "formats": expected}

        with (
            patch.object(formats, "_formats_request_inputs", return_value=(str(root), root)),
            patch.object(formats, "_open_formats_pipe"),
            patch.object(formats, "_exchange_formats", side_effect=reply),
        ):
            rows, error = context.cached_formats()
        self.assertIsNone(error)
        self.assertEqual(rows, expected)

    def test_protocol3_bytes_use_real_formats_parser(self) -> None:
        """覆盖 XB-05：从管道字节经真实 JSON 解码和身份判据接受协议 3。"""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            context = markdown_acceptance.Context(
                None, None, None, markdown_acceptance.FIXTURES_DEFAULT,
                markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
            )
            expected = [{"extension": "pdf", "mime_type": "application/pdf"}]
            response = (json.dumps({
                "id": "acceptance-formats-protocol3", "ok": True,
                "jchtools_broker_protocol": 3, "formats": expected,
            }) + "\n").encode("utf-8")
            sizes: list[int] = []
            sent: list[bytes] = []

            def write_pipe(_handle: object, payload: bytes, _overlap: object) -> tuple[int, None]:
                sent.append(payload)
                sizes.append(len(payload))
                return 0, None

            def read_pipe(_handle: object, _buffer: object, _overlap: object) -> tuple[int, None]:
                sizes.append(len(response))
                return 0, None

            with (
                patch.object(formats, "_formats_request_inputs", return_value=(str(root), root)),
                patch.object(formats, "_open_formats_pipe"),
                patch.object(markdown_acceptance.uuid, "uuid4", return_value=MagicMock(hex="protocol3")),
                patch("win32event.CreateEvent", return_value=321),
                patch("win32event.ResetEvent"),
                patch("win32api.CloseHandle"),
                patch("win32file.WriteFile", side_effect=write_pipe),
                patch("win32file.AllocateReadBuffer", return_value=response),
                patch("win32file.ReadFile", side_effect=read_pipe),
                patch("win32file.GetOverlappedResult", side_effect=lambda *_args: sizes.pop(0)),
            ):
                rows, error = context.cached_formats()
            self.assertIsNone(error)
            self.assertEqual(rows, expected)
            self.assertEqual(json.loads(sent[0])["request"]["command"], "formats")

    @staticmethod
    def _protocol_outcome(fields: dict[str, object]) -> markdown_acceptance.Outcome:
        def reply(_pipe: object, _runtime: Path, request_id: str, _deadline: float) -> object:
            response: dict[str, object] = {
                "id": request_id,
                "ok": True,
                "jchtools_broker_protocol": 3,
                "formats": [
                    {"extension": extension, "mime_type": "application/test"}
                    for extension in ("pdf", "docx", "pptx", "xlsx", "jp2", "j2k", "j2c")
                ],
            }
            response.update(fields)
            return response

        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "A18")
        state = markdown_acceptance.ROOT / ".tmp" / "parallel-review" / "protocol-state"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            context = markdown_acceptance.Context(
                root / "JchTools.exe",
                None,
                None,
                markdown_acceptance.FIXTURES_DEFAULT,
                markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
            )
            with (
                patch.dict(
                    os.environ,
                    {
                        "JCHTOOLS_TEST_STATE_DIR": str(state),
                        "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": str(state),
                    },
                    clear=True,
                ),
                patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                patch.object(formats, "_open_formats_pipe"),
                patch.object(formats, "_exchange_formats", side_effect=reply),
                patch.object(runner, "_asset_precondition", return_value=None),
                patch.object(
                    runner,
                    "drive_conversion",
                    side_effect=AssertionError("无有效当前能力证明不得启动 GUI 转换"),
                ),
            ):
                return markdown_acceptance.run_item(item, context)

    def test_stale_response_id_cannot_pass_a18(self) -> None:
        outcome = self._protocol_outcome({"id": "stale-request"})
        assert outcome.status == "NOT RUN"  # nosec B101: 其他请求的旧清单不能证明本轮能力。

    def test_incompatible_broker_protocol_cannot_pass_a18(self) -> None:
        outcome = self._protocol_outcome({"jchtools_broker_protocol": 1})
        assert outcome.status == "NOT RUN"  # nosec B101: 不兼容协议不可当作当前会话。

    def test_previous_broker_protocol_cannot_pass_a18(self) -> None:
        outcome = self._protocol_outcome({"jchtools_broker_protocol": 2})
        self.assertEqual(outcome.status, "NOT RUN")

    def test_worker_failure_cannot_fall_back_to_manifest(self) -> None:
        outcome = self._protocol_outcome({"ok": False, "error_kind": "backend_error", "error": "formats unavailable"})
        assert outcome.status == "NOT RUN"  # nosec B101: 实际查询失败不能以仓库清单冒充能力。

    def test_missing_required_documents_cannot_claim_current_engine_ready(self) -> None:
        outcome = self._protocol_outcome(
            {
                "formats": [{"extension": extension, "mime_type": "image/test"} for extension in ("jp2", "j2k", "j2c")],
            }
        )
        assert outcome.status == "NOT RUN"  # nosec B101: 缺四类必需文档支持的引擎不得声明就绪。


class PythonProcessScanTests(unittest.TestCase):
    """S10-10：进程名单必须覆盖实际承接转换的 xberg.exe（XB 进程模型）。."""

    def test_c05_scans_xberg_and_drops_retired_worker(self) -> None:
        recorded: list[tuple[str, ...]] = []
        first_round = threading.Event()

        def fake_scan(names: tuple[str, ...]) -> tuple[list[str], None]:
            recorded.append(names)
            first_round.set()
            return [], None

        def fake_drive(_exe: Path, _input: Path, _output: Path, *, stop_after_busy: bool = False) -> object:
            del stop_after_busy
            assert first_round.wait(timeout=10)  # nosec B101: 扫描至少完成一轮再返回。
            return markdown_acceptance.GuiRun([], "", None)

        with (
            patch.object(runner, "scan_python_modules", side_effect=fake_scan),
            patch.object(runner, "drive_conversion", side_effect=fake_drive),
        ):
            scanner = cast(
                "Callable[[Path, Path, Path], tuple[object, list[str], list[str]]]",
                getattr(markdown_acceptance, "_" + "c05_scan_during_conversion"),
            )
            _run, hits, errors = scanner(Path("exe"), Path("in"), Path("out"))
        assert errors == []  # nosec B101: 回归测试断言。
        assert hits == []  # nosec B101: 回归测试断言。
        assert recorded  # nosec B101: 回归测试断言。
        assert recorded[0] == ("JchTools.exe", "xberg.exe")  # nosec B101: XB 进程模型，退役 worker 不得残留。

    def test_c05_scanner_exception_is_reported_not_silently_lost(self) -> None:
        # 覆盖 T-27/P-13：线程中的模块枚举异常不能使「未发现 Python」伪装为已验证。
        first_round = threading.Event()

        def failing_scan(_names: tuple[str, ...]) -> tuple[list[str], str | None]:
            first_round.set()
            raise OSError("synthetic module query failure")

        def fake_drive(_exe: Path, _input: Path, _output: Path) -> markdown_acceptance.GuiRun:
            assert first_round.wait(timeout=10)
            return markdown_acceptance.GuiRun([], "", None)

        with (
            patch.object(runner, "scan_python_modules", side_effect=failing_scan),
            patch.object(runner, "drive_conversion", side_effect=fake_drive),
        ):
            scanner = getattr(markdown_acceptance, "_" + "c05_scan_during_conversion")
            _run, hits, errors = scanner(Path("exe"), Path("in"), Path("out"))
        assert hits == []
        assert errors and "synthetic module query failure" in errors[0]

    def test_c05_unfinished_scanner_cannot_report_verified_modules(self) -> None:
        # 覆盖 P-13：五秒收尾期限后仍运行的枚举线程不能留下空结果冒充验证。
        with (
            patch("scripts.markdown_acceptance.threading.Thread") as thread,
            patch.object(
                runner,
                "drive_conversion",
                return_value=markdown_acceptance.GuiRun([], "", None),
            ),
        ):
            thread.return_value.is_alive.return_value = True
            scanner = getattr(markdown_acceptance, "_" + "c05_scan_during_conversion")
            _run, hits, errors = scanner(Path("exe"), Path("in"), Path("out"))
        assert hits == []
        assert errors and "线程未结束" in errors[0]

    def test_c05_scanner_without_completed_round_cannot_report_verified_modules(self) -> None:
        # 覆盖 P-13：线程未获调度而直接收尾时，没有查询结果就没有验证证据。
        with (
            patch("scripts.markdown_acceptance.threading.Thread") as thread,
            patch.object(
                runner,
                "drive_conversion",
                return_value=markdown_acceptance.GuiRun([], "", None),
            ),
        ):
            thread.return_value.is_alive.return_value = False
            scanner = getattr(markdown_acceptance, "_" + "c05_scan_during_conversion")
            _run, hits, errors = scanner(Path("exe"), Path("in"), Path("out"))
        assert hits == []
        assert errors and "未完成任何一轮" in errors[0]

    def test_c05_without_outputs_cannot_pass(self) -> None:
        assessor = cast(
            "Callable[..., markdown_acceptance.Outcome]",
            getattr(markdown_acceptance, "_" + "c05_assess"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            input_dir = root / "input"
            output_dir = root / "output"
            _ = input_dir.mkdir()
            _ = output_dir.mkdir()
            source = input_dir / "video-to-notes-intro-zh.mp4"
            _ = source.write_bytes(b"synthetic media input")
            item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C05")
            outcome = assessor(
                item,
                markdown_acceptance.PreparedInputs(input_dir, output_dir, [source.name]),
                {Path(source.name): _digest(source)},
                markdown_acceptance.GuiRun([], "成功 0，部分提取 0，失败 0", None),
                [],
                [],
            )
        assert outcome.status == "FAIL"  # nosec B101: 无产物不得冒充 C05 实际转换成功。
        assert "实得 0 项：[]" in outcome.details[0]  # nosec B101: 结论须基于观察到的零产物。

    @staticmethod
    def _review_c05_outcome(
        gap: str, *, python_hit: bool = False, conversion: str = "healthy"
    ) -> markdown_acceptance.Outcome:
        """保留真实 C05 分派、扫描线程、输入快照与落盘核验，仅替换外部 GUI/模块查询。"""
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C05")
        second_scan = threading.Event()
        rounds = 0

        def scan(_names: tuple[str, ...]) -> tuple[list[str], str | None]:
            nonlocal rounds
            rounds += 1
            if rounds == 1:
                return (["xberg.exe 4242 python311.dll"] if python_hit else []), None
            second_scan.set()
            if gap == "exception":
                raise OSError("review module query failure")
            return [], "review module query failure"

        def convert(_exe: Path, inputs: Path, outputs: Path) -> markdown_acceptance.GuiRun:
            if not second_scan.wait(timeout=10):
                raise AssertionError("第二轮模块查询未完成")
            if conversion == "driver":
                return markdown_acceptance.GuiRun([], "转换失败", "review conversion driver failure")
            if conversion == "missing":
                return markdown_acceptance.GuiRun([], "成功 0，部分提取 0，失败 1", None)
            products: list[Path] = []
            for source in inputs.iterdir():
                product = outputs / f"{source.stem}_{source.suffix.lstrip('.')}.md"
                body = "公开合成转换正文\n"
                if conversion == "empty":
                    body = " \n"
                elif conversion == "media":
                    body += "![](review-missing.png)\n"
                product.write_text(body, encoding="utf-8")
                products.append(product)
                if conversion == "source":
                    source.write_bytes(b"unexpected source mutation")
            return markdown_acceptance.GuiRun(products, "成功 1，部分提取 0，失败 0", None)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixtures = root / "fixtures"
            for name in item.fixtures:
                source = fixtures / name
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_bytes(b"public synthetic C05 source")
            context = markdown_acceptance.Context(
                root / "JchTools.exe",
                None,
                None,
                fixtures,
                markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
            )
            with (
                patch.dict(
                    os.environ,
                    gui_smoke.isolated_state_env(markdown_acceptance.TMP_ROOT / "c05-scan-tests"),
                    clear=True,
                ),
                patch.object(runner, "SCRATCH_ROOT", root / "scratch"),
                patch.object(runner, "_asset_precondition", return_value=None),
                patch.object(runner, "_resolve_gui", return_value=(context.gui_exe, None)),
                patch.object(runner, "manifest_has_python_entries", return_value=False),
                patch.object(runner, "scan_python_modules", side_effect=scan),
                patch.object(runner, "drive_conversion", side_effect=convert),
                patch("scripts.markdown_acceptance.time.sleep"),
            ):
                return markdown_acceptance.run_item(item, context)

    def test_review_fix_c05_confirmed_python_hit_survives_later_scan_gap(self) -> None:
        # 覆盖 T-27/P-13、附录 A：已观察到的 Python DLL 不能被后续未知结果降级。
        for gap in ("exception", "reported-error"):
            with self.subTest(gap=gap):
                outcome = self._review_c05_outcome(gap, python_hit=True)
                self.assertEqual(outcome.status, markdown_acceptance.STATUS_FAILED)
                self.assertIn("python311.dll", outcome.reason)

    def test_review_fix_c05_confirmed_conversion_failure_survives_scan_gap(self) -> None:
        # 覆盖 T-09/T-14/T-25/T-27：驱动、缺产物、空正文、坏引用与源改动均是确定失败。
        evidence = {
            "driver": "review conversion driver failure",
            "missing": "未产出对应 Markdown",
            "empty": "正文为空",
            "media": "review-missing.png",
            "source": "源文件被改动",
        }
        for gap in ("exception", "reported-error"):
            for conversion, expected in evidence.items():
                with self.subTest(gap=gap, conversion=conversion):
                    outcome = self._review_c05_outcome(gap, conversion=conversion)
                    self.assertEqual(outcome.status, markdown_acceptance.STATUS_FAILED)
                    self.assertIn(expected, outcome.reason)

    def test_review_fix_c05_scan_gap_without_confirmed_failure_stays_not_run(self) -> None:
        # 覆盖 T-27/P-13：落盘结果健康不等于已证明无 Python，扫描缺口仍须诚实报告。
        for gap in ("exception", "reported-error"):
            with self.subTest(gap=gap):
                outcome = self._review_c05_outcome(gap)
                self.assertEqual(outcome.status, markdown_acceptance.STATUS_NOT_RUN)
                self.assertIn("review module query failure", outcome.reason)


class FormalDeliveryIsolationTests(unittest.TestCase):
    """覆盖 T-02 / XB-18：正式包不能用测试环境变量冒充隔离配置。."""

    def test_release_conversion_is_not_started_without_verified_isolation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            package = root / "package"
            _ = package.mkdir()
            _ = (package / "JchTools.exe").write_bytes(b"formal executable fixture")
            source_dir = root / "input"
            output_dir = root / "output"
            _ = source_dir.mkdir()
            _ = output_dir.mkdir()
            _ = (source_dir / "test_hello_world.png").write_bytes(b"synthetic source")
            prepared = markdown_acceptance.PreparedInputs(source_dir, output_dir, ["test_hello_world.png"])
            assets = markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, [])
            context = markdown_acceptance.Context(None, package, package, source_dir, assets)
            for item_id in ("C08", "C09"):
                with self.subTest(item=item_id):
                    item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == item_id)
                    with (
                        patch.dict(os.environ, {"JCHTOOLS_TEST_STATE_DIR": str(root / "state")}, clear=True),
                        patch.object(runner, "_prepare_scratch", return_value=prepared),
                        patch.object(
                            runner,
                            "drive_conversion",
                            return_value=markdown_acceptance.GuiRun([], "", None),
                        ) as drive,
                    ):
                        outcome = _run_handler("run_env", item, context)
                    drive.assert_not_called()
                    assert outcome.status == "NOT RUN"  # nosec B101: 缺独立会话不能启动正式包。
                    assert "release" in outcome.reason  # nosec B101: 须解释正式包忽略隔离环境变量。
                    assert "Windows" in outcome.reason  # nosec B101: 须说明真实独立配置前置。


class UnconfiguredStateBoundaryTests(unittest.TestCase):
    """覆盖 T-02 / T-21 / P-10：合法状态可写，未配置不能下载组件。."""

    def test_unconfigured_launch_isolates_screenshot_service(self) -> None:
        # 覆盖 XB-14/XB-22：未配置启动仍会连接后台，必须隔离截图管道身份。
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C03")
        context = markdown_acceptance.Context(
            Path("target/debug/JchTools.exe"),
            None,
            None,
            Path("fixtures"),
            markdown_acceptance.AssetProbe(None, None, None, None, []),
        )
        context.stages = ("S1", "S15")
        environments: list[dict[str, str]] = []

        def delegate(_item_id: str, target: Path, environment: dict[str, str]) -> None:
            environments.append(environment)

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(runner, "SCRATCH_ROOT", Path(temporary)),
            patch.object(runner, "_resolve_gui", return_value=(context.gui_exe, None)),
            patch.object(runner, "_c03_delegate_stages", side_effect=delegate),
        ):
            outcome = _run_handler("run_c03_unconfigured", item, context)
        assert outcome.status == "PASS", outcome.reason  # nosec B101: 合法隔离启动保持可用。
        assert environments  # nosec B101: 必须委托真实阶段。
        assert (
            environments[0].get("JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT")
            == environments[0]["JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT"]
        )  # nosec B101: 截图服务与其他资产根均隔离到同一临时根。

    @staticmethod
    def _run_with_artifacts(artifacts: tuple[str, ...], *, asset_file: str = "") -> markdown_acceptance.Outcome:
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C03")
        assets = markdown_acceptance.AssetProbe(None, None, None, None, [])
        context = markdown_acceptance.Context(Path("target/debug/JchTools.exe"), None, None, Path("fixtures"), assets)
        context.stages = ("S1", "S15")

        def delegate(_item_id: str, target: Path, environment: dict[str, str]) -> None:
            state = Path(environment["JCHTOOLS_TEST_STATE_DIR"])
            asset = Path(environment["JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT"])
            for name in artifacts:
                target = state / name
                _ = target.parent.mkdir(parents=True, exist_ok=True)
                _ = target.write_bytes(b"synthetic artifact")
            if asset_file:
                target = asset / asset_file
                _ = target.parent.mkdir(parents=True, exist_ok=True)
                _ = target.write_bytes(b"unexpected asset")

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(runner, "SCRATCH_ROOT", Path(temporary)),
            patch.object(runner, "_resolve_gui", return_value=(context.gui_exe, None)),
            patch.object(runner, "_c03_delegate_stages", side_effect=delegate),
        ):
            return _run_handler("run_c03_unconfigured", item, context)

    def test_normal_state_artifacts_do_not_count_as_conversion_downloads(self) -> None:
        outcome = self._run_with_artifacts(
            (
                "config.sqlite3",
                "config.sqlite3-wal",
                "hash-cache.sqlite3",
                "organizer.lock",
                "logs/jchtools.2026-10-07.123.0.log",
                "logs/jchtools.2026-10-07.123-1000-1.0.log",
                "logs.retention.lock",
                "logs-fallback.retention.lock",
                "logs-fallback/jchtools.2026-10-07.123.log",
                "logs-fallback/jchtools.2026-10-07.123-1000-1.log",
                "tasks/20261007T120000-12345678-1234-1234-1234-123456789abc/task.sqlite3",
            )
        )
        assert outcome.status == "PASS", outcome.reason  # nosec B101: 普通工具合法状态和本地日志不是转换组件下载。

    def test_state_download_path_is_still_rejected(self) -> None:
        outcome = self._run_with_artifacts(("config.sqlite3", "xberg-downloads/runtime/xberg.exe"))
        assert outcome.status == "FAIL"  # nosec B101: 分离状态根后仍检查主动下载目录。
        assert "xberg-downloads" in outcome.reason  # nosec B101: 精确呈现意外组件落位。

    def test_nested_settings_name_cannot_hide_a_download(self) -> None:
        outcome = self._run_with_artifacts(("unexpected/config.sqlite3",))
        assert outcome.status == "FAIL"  # nosec B101: 不按basename豁免整个状态树。

    def test_asset_root_write_is_still_rejected(self) -> None:
        outcome = self._run_with_artifacts(("config.sqlite3",), asset_file="xberg.exe")
        assert outcome.status == "FAIL"  # nosec B101: 未配置资产根须保持为空。

    def test_unrecognized_log_name_cannot_hide_a_download(self) -> None:
        outcome = self._run_with_artifacts(("logs/jchtools.2026-10-07.not-a-pid.log",))
        assert outcome.status == "FAIL"
        assert "not-a-pid.log" in outcome.reason
