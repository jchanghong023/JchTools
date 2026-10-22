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

from scripts.markdown_acceptance_impl import release_scan, runner
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
class AcceptanceBoundaryTests(unittest.TestCase):
    def test_conversion_requires_an_explicit_gui_target(self) -> None:
        resolver = cast(
            "Callable[[markdown_acceptance.Context], tuple[Path | None, str | None]]",
            getattr(markdown_acceptance, "_" + "resolve_gui"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            assets = markdown_acceptance.AssetProbe(None, None, None, None, [])
            context = markdown_acceptance.Context(None, None, None, Path(temporary), assets)
            target, reason = resolver(context)
            assert target is None  # nosec B101: 不得猜测默认被测 GUI。
            assert reason is not None  # nosec B101: 未提供目标必须解释阻止原因。
            assert "--gui-exe" in reason  # nosec B101: 必须指引显式选择。
            explicit_gui = Path(temporary) / "JchTools.exe"
            _ = explicit_gui.write_bytes(b"synthetic executable path")
            context.gui_exe = explicit_gui
            target, reason = resolver(context)
            assert target == explicit_gui  # nosec B101: 使用显式被测 GUI 路径。
            assert reason is None  # nosec B101: 在场目标解除前置阻止。

    def test_invalid_item_selection_uses_argument_error_code(self) -> None:
        with patch.object(sys, "argv", ["markdown_acceptance.py", "--only", "UNKNOWN"]):
            assert markdown_acceptance.main() == ARGUMENT_ERROR_EXIT_CODE  # nosec B101: 参数错误专用退出码。

    def test_outside_isolation_root_is_rejected_before_use(self) -> None:
        checker = cast(
            "Callable[[], str | None]",
            getattr(markdown_acceptance, "_" + "isolated_environment_error"),
        )
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.dict(os.environ, {"JCHTOOLS_TEST_STATE_DIR": temporary}, clear=True),
        ):
            reason = checker()
        assert reason is not None  # nosec B101: 越界隔离根必须拒绝。
        assert "JCHTOOLS_TEST_STATE_DIR" in reason  # nosec B101: 拒绝原因指出变量。
        assert ".tmp" in reason  # nosec B101: 拒绝原因指出仓库隔离边界。

    def test_conversion_without_isolation_does_not_launch_gui(self) -> None:
        # 覆盖 XB-14/XB-22：缺状态或截图隔离根时，不得先启动可能触及生产配置的 GUI。
        state = str(markdown_acceptance.ROOT / ".tmp" / "parallel-review" / "conversion-state")
        snap = str(markdown_acceptance.ROOT / ".tmp" / "parallel-review" / "conversion-snap")
        for environment in (
            {},
            {"JCHTOOLS_TEST_STATE_DIR": state},
            {"JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": snap},
        ):
            with (
                self.subTest(environment=environment),
                patch.dict(os.environ, environment, clear=True),
                patch(
                    "scripts.markdown_acceptance_impl.gui.start_owned_process", side_effect=AssertionError("不得启动 GUI")
                ) as launch,
            ):
                run = markdown_acceptance.drive_conversion(Path("gui.exe"), Path("input"), Path("output"))
            launch.assert_not_called()
            assert run.error is not None  # nosec B101: 隔离前置缺失必须明确拒绝。

    def test_startup_requires_a_new_process_run_log_not_reused_pid_history(self) -> None:
        # 覆盖 P-10 / XB-18：同 PID 的历史日志也不能冒充本轮新建的启动文件。
        checker = isolation_probe._process_isolation_ready
        workspace = markdown_acceptance.ROOT / ".tmp" / "acceptance-process-logs"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            state = Path(temporary).resolve()
            (state / "config.sqlite3").write_bytes(b"seeded database")
            logs = state / "logs"
            logs.mkdir()
            started = (
                "timestamp=2026-10-17T00:00:00.000Z level=INFO pid=123 thread_id=ThreadId(1) "
                'component=logging event=application_started role="gui" message="started"\n'
            )
            historical = logs / "jchtools.2026-10-17.123.log"
            historical.write_text(started, encoding="utf-8")
            current = logs / "jchtools.2026-10-17.123-1000-1.log"
            with patch.object(isolation_probe, "TMP_ROOT", state):
                self.assertFalse(checker(state, 123, "gui", frozenset()), "旧 PID 文件被当成本轮启动证据")
                current.write_text(started, encoding="utf-8")
                self.assertTrue(checker(state, 123, "gui", frozenset()), "本轮独占启动文件未被识别")
                before = frozenset({current})
                with current.open("a", encoding="utf-8") as output:
                    output.write(started)
                self.assertFalse(
                    checker(state, 123, "gui", before),
                    "向之前已经存在的运行文件补写启动行，不能证明进程重启已换文件",
                )

    def test_conversion_isolation_requires_fresh_matching_gui_pid(self) -> None:
        # 覆盖 XB-18/XB-22：预播种数据库、旧启动记录及其他 PID 不能证明本轮隔离。
        checker = isolation_probe._process_isolation_ready
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary).resolve()
            database = state / "config.sqlite3"
            _ = database.write_bytes(b"seeded database")
            logs = state / "logs"
            _ = logs.mkdir()
            log = logs / "jchtools.2026-10-08.123-1000-1.log"
            good = (
                "timestamp=2026-10-08T00:00:00.000Z level=INFO pid=123 thread_id=ThreadId(1) "
                'component=logging event=application_started role="gui" '
                'log_file="isolated" message="应用诊断日志已启动"\n'
            )
            previous = logs / "jchtools.2026-10-08.123-999-1.log"
            _ = previous.write_text(good, encoding="utf-8")
            existing_logs = frozenset({previous})
            with patch.object(isolation_probe, "TMP_ROOT", state):
                assert not checker(state, 123, "gui", existing_logs)  # nosec B101: 旧文件不能证明当前进程。
                _ = log.write_text(good.replace("pid=123", "pid=1234"), encoding="utf-8")
                assert not checker(state, 123, "gui", existing_logs)  # nosec B101: PID 必须精确匹配。
                _ = log.write_text(good.replace("event=application_started", "event=another_event"), encoding="utf-8")
                assert not checker(state, 123, "gui", existing_logs), "同 PID 的其他事件不能证明启动隔离"
                _ = log.write_text(good.replace('role="gui"', 'role="xberg-broker"'), encoding="utf-8")
                assert not checker(state, 123, "gui", existing_logs)  # nosec B101: 代理进程不证明 GUI 的状态隔离。
                _ = log.write_text(good, encoding="utf-8")
                assert checker(state, 123, "gui", existing_logs)  # nosec B101: 本轮新文件的 GUI 启动记录有效。
                database.unlink()
                assert not checker(state, 123, "gui", existing_logs)  # nosec B101: 日志不能替代隔离 SQLite。

    def test_conversion_isolation_uses_fallback_startup_not_payload_fields(self) -> None:
        checker = isolation_probe._process_isolation_ready
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary).resolve()
            (state / "config.sqlite3").write_bytes(b"seeded database")
            logs = state / "logs-fallback"
            logs.mkdir()
            log = logs / "jchtools.2026-10-08.123-1000-1.log"
            wrong_role = (
                "timestamp=2026-10-08T00:00:00.000Z level=INFO pid=123 thread_id=ThreadId(1) "
                'component=logging event=application_started role="xberg-broker" '
                'message="payload role=\\"gui\\" pid=123"\n'
            )
            log.write_text(wrong_role, encoding="utf-8")
            with patch.object(isolation_probe, "TMP_ROOT", state):
                assert not checker(state, 123, "gui", frozenset()), "消息正文不能伪造本轮 GUI 启动字段"
                with log.open("a", encoding="utf-8") as stream:
                    stream.write(wrong_role.replace('role="xberg-broker"', 'role="gui"'))
                assert checker(state, 123, "gui", frozenset()), "受控备用目录也须承接真实 GUI 启动证明"

    def test_conversion_isolation_fixture_handles_windows_short_temp_root(self) -> None:
        # CI 的 RUNNER~1 临时目录会解析到完整用户名；夹具须遵守产品隔离根的规范路径口径。
        with (
            tempfile.TemporaryDirectory(prefix="jchtools-ci-short-path-") as temporary,
            patch("tempfile.tempdir", win32api.GetShortPathName(temporary)),
        ):
            self.test_conversion_isolation_requires_fresh_matching_gui_pid()

    def test_outside_snap_isolation_root_is_rejected(self) -> None:
        # 覆盖 XB-14/XB-22：截图资产根决定服务管道，不能继承生产位置。
        checker = cast(
            "Callable[[], str | None]",
            getattr(markdown_acceptance, "_" + "isolated_environment_error"),
        )
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.dict(os.environ, {"JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": temporary}, clear=True),
        ):
            reason = checker()
        assert reason is not None  # nosec B101: 仓库外的截图资产根必须拒绝。
        assert "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT" in reason  # nosec B101: 拒绝原因指出变量。

    def test_installed_scan_requires_executable(self) -> None:
        # 覆盖 T-02/P-12：空目录不构成安装交付，不能因无违禁资产而 PASS。
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C01")
            context = markdown_acceptance.Context(
                None, root, None, root, markdown_acceptance.AssetProbe(None, None, None, None, [])
            )
            outcome = markdown_acceptance.run_item(item, context)
        assert outcome.status == "FAIL"  # nosec B101: 安装目录必须包含主程序。
        assert "JchTools.exe" in outcome.reason  # nosec B101: 明确缺失成员。

    def test_seed_rejects_state_outside_tmp_without_writing(self) -> None:
        seed = cast(
            "Callable[[Path, Path], None]",
            getattr(markdown_acceptance, "_" + "seed_isolated_state"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary) / "state"
            message = ""
            try:
                seed(state, Path(temporary) / "runtime")
            except ValueError as error:
                message = str(error)
            assert message  # nosec B101: 越界状态根必须在副作用前失败。
            assert ".tmp" in message  # nosec B101: 拒绝原因指出仓库隔离边界。
            assert not state.exists()  # nosec B101: 被拒绝的种子操作不得创建状态目录。

    def test_invalid_release_asset_manifest_fails_package_scan(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "package"
            _ = root.mkdir()
            _ = (root / "JchTools.exe").write_bytes(b"synthetic executable fixture")
            runtime = root / "xberg"
            runtime.mkdir()
            _ = (runtime / markdown_acceptance.RECEIPT_NAME).write_text("{", encoding="utf-8")
            item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C01")
            context = markdown_acceptance.Context(
                None,
                root,
                None,
                root,
                markdown_acceptance.AssetProbe(None, None, None, None, []),
            )
            outcome = markdown_acceptance.run_item(item, context)
        assert outcome.status == "FAIL"  # nosec B101: 清单不可验证时不得报告交付扫描通过。
        assert outcome.reason  # nosec B101: 必须保留清单/收据解析错误原因。

    def test_external_report_path_is_rejected_before_write(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            report_path = Path(temporary) / "report.json"
            state_root = markdown_acceptance.ROOT / ".tmp" / "test-markdown-report-state"
            with (
                patch.dict(os.environ, {"JCHTOOLS_TEST_STATE_DIR": str(state_root)}, clear=True),
                patch.object(
                    sys,
                    "argv",
                    [
                        "markdown_acceptance.py",
                        "--only",
                        "C01",
                        "--json",
                        str(report_path),
                    ],
                ),
                patch.object(sys, "stderr"),
            ):
                error_code: int | None = None
                try:
                    _ = markdown_acceptance.main()
                except SystemExit as error:
                    if isinstance(error.code, int):
                        error_code = error.code
            assert error_code == ARGUMENT_ERROR_EXIT_CODE  # nosec B101: 越界报告参数使用专用错误码。
            assert not report_path.exists()  # nosec B101: 拒绝前不能创建外部报告。


class SyntheticFixtureTests(unittest.TestCase):
    """S10-13：夹具合成化——无外部语料路径、无来源元数据、生成脚本可校验。."""

    def test_pptx_multi_image_fixture_has_unique_nonvisual_shape_ids(self) -> None:
        # 覆盖附录 A2/T-15：组形状与图片形状不能复用同一个 cNvPr ID。
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary)
            result = markdown_acceptance.SYNTHESIZERS["pptx_multi_images"](target, markdown_acceptance.FIXTURES_DEFAULT)
            assert not result.error
            with zipfile.ZipFile(target / result.files[0]) as archive:
                slide = ElementTree.fromstring(archive.read("ppt/slides/slide1.xml"))
            presentation = "{http://schemas.openxmlformats.org/presentationml/2006/main}"
            ids = [node.attrib["id"] for node in slide.iter(f"{presentation}cNvPr")]
            assert len(ids) == markdown_acceptance.A02_IMAGE_COUNT + 1
            assert len(set(ids)) == len(ids)

    def test_pbm_binary_preserves_ascii_pixel_positions(self) -> None:
        # 覆盖附录 A17：P4 每行从最高位开始，不得把 P1 的像素整体右移。
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary)
            result = markdown_acceptance.SYNTHESIZERS["pnm"](target, markdown_acceptance.FIXTURES_DEFAULT)
            assert not result.error
            assert (target / "p4_bin.pbm").read_bytes() == b"P4\n3 3\n\x40\xa0\x40"
            with (
                markdown_acceptance.Image.open(target / "p1_ascii.pbm") as ascii_image,
                markdown_acceptance.Image.open(target / "p4_bin.pbm") as binary_image,
            ):
                assert ascii_image.size == binary_image.size
                assert ascii_image.tobytes() == binary_image.tobytes()

    def test_pptx_field_fixture_uses_drawingml_paragraph_namespace(self) -> None:
        # 覆盖 T-15/附录 A8：真实文本框必须含 a:p，不能用 p:p 让引擎接受非标准 XML。
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary)
            result = markdown_acceptance.SYNTHESIZERS["pptx_runs_fields"](target, markdown_acceptance.FIXTURES_DEFAULT)
            assert not result.error
            with zipfile.ZipFile(target / result.files[0]) as archive:
                slide = ElementTree.fromstring(archive.read("ppt/slides/slide1.xml"))
            drawing = "{http://schemas.openxmlformats.org/drawingml/2006/main}"
            presentation = "{http://schemas.openxmlformats.org/presentationml/2006/main}"
            paragraphs = slide.findall(f".//{presentation}txBody/{drawing}p")
            assert len(paragraphs) == 1
            assert "".join(node.text or "" for node in paragraphs[0].iter(f"{drawing}t")) == "RUN-AND-FIELD7-FIELD-END"

    def test_format_sweep_jsonl_is_synthetic(self) -> None:
        path = markdown_acceptance.FIXTURES_DEFAULT / "matrix" / "format_sweep" / "sample.jsonl"
        text = path.read_text(encoding="utf-8")
        assert "docs/" not in text  # nosec B101: 无外部语料路径。
        assert ".pdf" not in text  # nosec B101: 无外部语料路径。
        lines = [line for line in text.splitlines() if line.strip()]
        assert lines  # nosec B101: 回归测试断言。
        for line in lines:
            value = json.loads(line)
            assert isinstance(value, dict)  # nosec B101: cast 不校验类型；每行必须确为 JSON 对象。

    def test_fixture_readme_has_no_source_provenance(self) -> None:
        text = (markdown_acceptance.FIXTURES_DEFAULT / "README.md").read_text(encoding="utf-8")
        assert "118957872982e44b08ab430c20147f74cf3ef494" not in text  # nosec B101: 源提交号可反查来源。
        assert "逐字节迁自" not in text  # nosec B101: 迁移来源元数据。
        assert "generate_synthetic.py" in text  # nosec B101: 新的合成来源说明。

    def test_synthetic_generator_check_passes(self) -> None:
        script = markdown_acceptance.FIXTURES_DEFAULT / "generate_synthetic.py"
        assert script.is_file()  # nosec B101: 回归测试断言。
        done = subprocess.run(
            [sys.executable, str(script), "--check"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        assert done.returncode == 0, done.stderr  # nosec B101: 在场夹具必须满足结构断言。


class BundledDistributionTests(unittest.TestCase):
    """基础资产正向完整性与音频负向策略；夹具不执行 GUI/模型。"""

    @staticmethod
    def _context(root: Path) -> markdown_acceptance.Context:
        return markdown_acceptance.Context(
            None, root, root, root, markdown_acceptance.AssetProbe(None, None, None, None, [])
        )

    def test_default_base_passes_both_package_scans_without_audio(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "package"
            manifest = _bundled_package_fixture(root)
            with patch.object(release_scan, "ASSET_MANIFEST", manifest):
                for item_id in ("C01", "C02"):
                    item = next(item for item in markdown_acceptance.ITEMS if item.item_id == item_id)
                    outcome = markdown_acceptance.run_item(item, self._context(root))
                    self.assertEqual(outcome.status, markdown_acceptance.STATUS_OK, outcome.reason)
            self.assertFalse((root / "xberg/ffmpeg").exists())

    def test_missing_delivery_environment_is_not_run_not_pass(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            absent = Path(temporary) / "not-unpacked"
            for item_id in ("C01", "C02"):
                item = next(item for item in markdown_acceptance.ITEMS if item.item_id == item_id)
                outcome = markdown_acceptance.run_item(item, self._context(absent))
                self.assertEqual(outcome.status, markdown_acceptance.STATUS_NOT_RUN)
                self.assertTrue(outcome.reason)

    def test_missing_model_identity_manifest_cannot_pass_delivery(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "package"
            manifest = _bundled_package_fixture(root)
            manifest.unlink()
            item = next(item for item in markdown_acceptance.ITEMS if item.item_id == "C01")
            with patch.object(release_scan, "ASSET_MANIFEST", manifest):
                outcome = markdown_acceptance.run_item(item, self._context(root))
            self.assertEqual(outcome.status, markdown_acceptance.STATUS_FAILED)
            self.assertTrue(outcome.reason)

    def test_missing_or_tampered_base_is_not_a_passing_package(self) -> None:
        for relative in ("xberg.exe", "models/snapshot-ocr/det.onnx", "sherpa-onnx/sherpa-onnx-c-api.dll"):
            for mutation in ("missing", "tampered"):
                with self.subTest(relative=relative, mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                    root = Path(temporary) / "package"
                    manifest = _bundled_package_fixture(root)
                    path = root / "xberg" / relative
                    if mutation == "missing":
                        path.unlink()
                    else:
                        _ = path.write_bytes(b"tampered")
                    with patch.object(release_scan, "ASSET_MANIFEST", manifest):
                        problems = markdown_acceptance.scan_distribution(root)
                    self.assertTrue(any(relative in problem for problem in problems), problems)

    def test_default_distribution_rejects_audio_and_python_anywhere(self) -> None:
        leaks = (
            "xberg/ffmpeg/LICENSE.txt", "xberg/models/vad/silero_vad.onnx",
            "xberg/models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx",
            "misplaced/avcodec-63.dll", "misplaced/silero_vad.onnx",
            "samples/silence.wav", "python313.dll", "xberg-audio-assets-x86_64-pc-windows-msvc.zip",
        )
        for relative in leaks:
            with self.subTest(relative=relative), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / "package"
                manifest = _bundled_package_fixture(root)
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                _ = path.write_bytes(b"leaked optional/runtime bytes")
                with patch.object(release_scan, "ASSET_MANIFEST", manifest):
                    problems = markdown_acceptance.scan_distribution(root)
                self.assertTrue(any(relative in problem for problem in problems), problems)

    def test_receipt_rejects_unsafe_duplicate_and_conflicting_members(self) -> None:
        for relative in ("../escape.dll", "C:/escape.dll", "models\\\\escape.onnx", "/escape.dll", "xberg.exe"):
            with self.subTest(relative=relative), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / "package"
                manifest = _bundled_package_fixture(root)
                receipt_path = root / "xberg" / markdown_acceptance.RECEIPT_NAME
                receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
                receipt["components"][0]["members"].append({
                    "path": relative, "size_bytes": 1, "sha256": "f" * 64,
                })
                _ = receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
                with patch.object(release_scan, "ASSET_MANIFEST", manifest), self.assertRaises(ValueError):
                    markdown_acceptance.scan_distribution(root)

    def test_receipt_rejects_wrong_release_and_shared_member_conflict(self) -> None:
        for mutation in ("audio-url", "shared-digest", "missing-model-record"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary) / "package"
                manifest = _bundled_package_fixture(root)
                receipt_path = root / "xberg" / markdown_acceptance.RECEIPT_NAME
                receipt = json.loads(receipt_path.read_text(encoding="utf-8"))
                if mutation == "audio-url":
                    receipt["audio_assets"]["url"] = "https://github.com/untrusted/audio.zip"
                elif mutation == "shared-digest":
                    record = next(entry for entry in receipt["components"][1]["members"] if entry["path"] == "xberg.exe")
                    record["sha256"] = "f" * 64
                else:
                    receipt["components"][1]["members"] = [
                        entry for entry in receipt["components"][1]["members"]
                        if entry["path"] != "models/snapshot-ocr/rec.onnx"
                    ]
                _ = receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
                with patch.object(release_scan, "ASSET_MANIFEST", manifest), self.assertRaises(ValueError):
                    markdown_acceptance.scan_distribution(root)

    def test_invalid_distribution_failure_and_report_details_are_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "package"
            manifest = _bundled_package_fixture(root)
            _ = (root / "xberg/xberg.exe").write_bytes(b"broken engine")
            item = next(item for item in markdown_acceptance.ITEMS if item.item_id == "C02")
            with patch.object(release_scan, "ASSET_MANIFEST", manifest):
                outcome = markdown_acceptance.run_item(item, self._context(root))
            self.assertEqual(outcome.status, markdown_acceptance.STATUS_FAILED)
            self.assertIn("xberg.exe", outcome.reason)
            self.assertTrue(outcome.details)
            with patch("sys.stdout", new=io.StringIO()):
                self.assertEqual(report_acceptance([(item, outcome)]), 1)


class BundledProbeTests(unittest.TestCase):
    """有效源选择与按场景前置；基础包无音频只能阻塞媒体，不冒充转录成功。"""

    @staticmethod
    def _probe(root: Path, manifest: Path) -> markdown_acceptance.AssetProbe:
        with (
            patch.object(release_scan, "ASSET_MANIFEST", manifest),
            patch.dict(os.environ, {
                "JCHTOOLS_TEST_ASSET_ROOT": str(root / "assets"),
                "JCHTOOLS_TEST_STATE_DIR": str(root / "state"),
                "JCHTOOLS_TEST_BUNDLED_XBERG_DIR": str(root / "package/xberg"),
            }, clear=True),
        ):
            return markdown_acceptance.probe_assets()

    def test_base_qualifies_documents_but_missing_audio_is_not_run(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = _bundled_package_fixture(root / "package")
            assets = self._probe(root, manifest)
            self.assertTrue(assets.xberg_ready(), assets.missing)
            self.assertFalse(assets.media_ready())
            context = markdown_acceptance.Context(None, None, None, root, assets)
            prerequisite = getattr(markdown_acceptance, "_" + "asset_precondition")
            document = markdown_acceptance.Item("probe-doc", "C", "document", "GUI", needs_assets="xberg")
            media = markdown_acceptance.Item("probe-media", "C", "media", "GUI", needs_assets="xberg+media")
            self.assertIsNone(prerequisite(context, document))
            self.assertIn("媒体", prerequisite(context, media))
            self.assertEqual(markdown_acceptance.run_item(
                next(item for item in markdown_acceptance.ITEMS if item.item_id == "A24"), context
            ).status, markdown_acceptance.STATUS_NOT_RUN)

    def test_installed_audio_requires_each_actual_member_and_digest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = _bundled_package_fixture(root / "package")
            runtime = root / "package/xberg"
            for name in (*markdown_acceptance.MEDIA_REQUIRED, "ffmpeg/LICENSE.txt"):
                path = runtime / name
                path.parent.mkdir(parents=True, exist_ok=True)
                _ = path.write_bytes(f"synthetic audio fixture: {name}".encode())
            assets = self._probe(root, manifest)
            self.assertTrue(assets.media_ready(), assets.missing)
            _ = (runtime / markdown_acceptance.MEDIA_REQUIRED[0]).write_bytes(b"tampered")
            assets = self._probe(root, manifest)
            self.assertTrue(assets.xberg_ready(), assets.missing)
            self.assertFalse(assets.media_ready())

    def test_missing_or_tampered_base_never_qualifies_documents(self) -> None:
        for mutation in ("missing", "tampered"):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                manifest = _bundled_package_fixture(root / "package")
                model = root / "package/xberg/models/snapshot-ocr/rec.onnx"
                if mutation == "missing":
                    model.unlink()
                else:
                    _ = model.write_bytes(b"tampered")
                assets = self._probe(root, manifest)
                self.assertFalse(assets.xberg_ready())
                self.assertTrue(assets.base_missing)

    def test_effective_source_and_sqlite_failures_are_not_hidden_by_bundle(self) -> None:
        for source in ("custom", "downloaded", "bundled", "unknown", "corrupt-db"):
            with self.subTest(source=source), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                manifest = _bundled_package_fixture(root / "package")
                state = root / "state"
                state.mkdir()
                selected = root / "selected"
                selected.mkdir()
                _ = (selected / "xberg.exe").write_bytes(b"legacy never executed")
                database = state / "config.sqlite3"
                if source == "corrupt-db":
                    _ = database.write_bytes(b"not sqlite")
                else:
                    with contextlib.closing(sqlite3.connect(database)) as conn, conn:
                        _ = conn.execute("CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                        _ = conn.executemany("INSERT INTO app_settings VALUES (?,?)", [
                            ("xberg_source", source), (f"xberg_{source}_directory", str(selected)),
                            ("xberg_directory", str(root / "stale")),
                        ])
                assets = self._probe(root, manifest)
                if source in ("unknown", "corrupt-db"):
                    self.assertIsNone(assets.runtime_dir)
                    self.assertTrue(any("SQLite" in problem for problem in assets.missing))
                else:
                    self.assertEqual(assets.runtime_dir, selected)

    def test_invalid_or_missing_legacy_root_can_use_bundle_but_relative_bundle_cannot(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = _bundled_package_fixture(root / "package")
            assets = root / "assets"
            assets.mkdir()
            _ = (assets / "xberg-runtime-path.txt").write_text(str(root / "absent-legacy"), encoding="utf-8")
            self.assertEqual(self._probe(root, manifest).runtime_dir, root / "package/xberg")
            with (
                patch.dict(os.environ, {
                    "JCHTOOLS_TEST_ASSET_ROOT": str(assets), "JCHTOOLS_TEST_STATE_DIR": str(root / "state"),
                    "JCHTOOLS_TEST_BUNDLED_XBERG_DIR": "relative-bundle",
                }, clear=True),
                patch.object(release_scan, "ASSET_MANIFEST", manifest),
            ):
                probe = markdown_acceptance.probe_assets()
            self.assertIsNone(probe.runtime_dir)
            self.assertTrue(probe.missing)

    def test_legacy_pointer_read_failure_is_not_hidden_by_valid_bundle(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = _bundled_package_fixture(root / "package")
            (root / "assets/xberg-runtime-path.txt").mkdir(parents=True)
            probe = self._probe(root, manifest)
            self.assertIsNone(probe.runtime_dir)
            self.assertTrue(any("读取旧 Xberg" in problem for problem in probe.missing))


class TestGateEnvironmentTests(unittest.TestCase):
    """fulltest 保留调用方选定的引擎；固定测试目录仅作缺省值。."""

    @staticmethod
    def powershell_environment() -> dict[str, str]:
        resolver = cast("Callable[[], dict[str, str]]", getattr(test_gate, "_" + "powershell_env"))
        return resolver()

    def test_explicit_engine_is_not_replaced_by_existing_fixed_engine(self) -> None:
        selected = str(test_gate.ROOT / ".tmp" / "patched-runtime")
        with (
            patch.dict(os.environ, {"JCHTOOLS_TEST_XBERG_DIR": selected}, clear=True),
            patch.object(Path, "is_file", return_value=True),
        ):
            env = self.powershell_environment()
        assert env["JCHTOOLS_TEST_XBERG_DIR"] == selected  # nosec B101: 显式运行目录必须优先。

    def test_invalid_explicit_engine_is_not_silently_replaced(self) -> None:
        selected = str(test_gate.ROOT / ".tmp" / "missing-runtime")
        with (
            patch.dict(os.environ, {"JCHTOOLS_TEST_XBERG_DIR": selected}, clear=True),
            patch.object(Path, "is_file", return_value=False),
        ):
            env = self.powershell_environment()
        assert env["JCHTOOLS_TEST_XBERG_DIR"] == selected  # nosec B101: 无效选择不得静默换引擎。

    def test_fixed_engine_is_used_only_without_explicit_override(self) -> None:
        with (
            patch.dict(os.environ, {}, clear=True),
            patch.object(Path, "is_file", return_value=True),
        ):
            env = self.powershell_environment()
        assert env["JCHTOOLS_TEST_XBERG_DIR"] == str(test_gate.FIXED_XBERG_TEST_DIR)  # nosec B101: 缺省测试来源。


class MarkdownSummaryIntegrationTests(unittest.TestCase):
    """执行 acceptance.ps1 的实际状态分支，防止零跳过汇总被误判为部分覆盖。."""

    def classify(self, lines: list[str]) -> str:
        powershell = Path(os.environ["SYSTEMROOT"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
        script = r"""
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$payload = [Console]::In.ReadToEnd() | ConvertFrom-Json
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($payload.path, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw '验收脚本语法错误' }
$node = $ast.Find({
    param($n)
    $n -is [System.Management.Automation.Language.IfStatementAst] -and
    $n.Clauses[0].Item1.Extent.Text -eq '$mdCode -eq 0'
}, $true)
if ($null -eq $node) { throw '缺少 Markdown 退出码分类分支' }
$mdCode = 0
$mdOutput = $payload.lines
$script:Results = [System.Collections.Generic.List[string]]::new()
& ([ScriptBlock]::Create($node.Extent.Text))
[Console]::WriteLine($script:Results[0])
"""
        environment = dict(os.environ)
        provider = cast("Callable[[], dict[str, str]]", getattr(test_gate, "_" + "powershell_env"))
        environment.update(provider())
        result = subprocess.run(
            [str(powershell), "-NoProfile", "-Command", script],
            input=json.dumps({"path": str(test_gate.ROOT / "scripts/acceptance.ps1"), "lines": lines}),
            text=True,
            encoding="utf-8",
            capture_output=True,
            check=True,
            env=environment,
        )
        return result.stdout.strip()

    def test_zero_not_run_summary_is_full_pass(self) -> None:
        result = self.classify(["PASS A01 内容正确", "PASS 44| FAIL 0| NOT RUN 0（共 44 条）"])
        assert result == "PASS  markdown-acceptance"  # nosec B101: 零跳过必须全通过。

    def test_actual_skipped_item_remains_partial(self) -> None:
        result = self.classify(["PASS A01 内容正确", "NOT RUN A02 缺真实资产", "PASS 1| FAIL 0| NOT RUN 1（共 2 条）"])
        assert result.startswith("PARTIAL  markdown-acceptance")  # nosec B101: 真实未执行不得变绿。


class CommandTimeoutTests(unittest.TestCase):
    def test_test_gate_timeout_is_reported_as_failure(self) -> None:
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(test_gate, "LOG_DIR", Path(temporary)),
        ):
            result = test_gate.run_logged(
                "unit-timeout",
                [sys.executable, "-c", "import time; time.sleep(30)"],
                timeout=COMMAND_TIMEOUT_SECONDS,
            )
        assert result.status == test_gate.STATUS_TIMED_OUT  # nosec B101: 到期不得报告通过。
        assert "8s 总预算" in result.detail  # nosec B101: 消费者可见预算须与调用一致。
        assert "进程树已终止" in result.detail  # nosec B101: 清理完成状态必须可见。

    def test_timing_timeout_preserves_requested_deadline(self) -> None:
        runner = cast(
            "Callable[[list[str], float], subprocess.CompletedProcess[str]]",
            getattr(test_timing, "_" + "run_checked"),
        )
        requested_timeout: float | None = None
        try:
            _ = runner(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                COMMAND_TIMEOUT_SECONDS,
            )
        except subprocess.TimeoutExpired as error:
            requested_timeout = error.timeout
        else:
            self.fail("超时命令不得返回成功完成状态")
        assert requested_timeout == COMMAND_TIMEOUT_SECONDS  # nosec B101: 不得延长调用方预算。

    def test_timing_preserves_nonzero_exit_for_failure_classification(self) -> None:
        runner = cast(
            "Callable[[list[str], float], subprocess.CompletedProcess[str]]",
            getattr(test_timing, "_" + "run_checked"),
        )
        completed = runner([sys.executable, "-c", "raise SystemExit(7)"], COMMAND_TIMEOUT_SECONDS)
        assert completed.returncode == COMMAND_FAILURE_EXIT_CODE  # nosec B101: 套件消费者须区分失败。

    def test_gate_preserves_nonzero_exit_as_failure(self) -> None:
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(test_gate, "LOG_DIR", Path(temporary)),
        ):
            result = test_gate.run_logged(
                "unit-failure",
                [sys.executable, "-c", f"raise SystemExit({COMMAND_FAILURE_EXIT_CODE})"],
                timeout=COMMAND_TIMEOUT_SECONDS,
            )
        assert result.status == test_gate.STATUS_FAILED  # nosec B101: 非零退出不得报告通过。
        assert f"退出码 {COMMAND_FAILURE_EXIT_CODE}" in result.detail  # nosec B101: 失败原因须保留。


class ProcessTreeOwnershipTests(unittest.TestCase):
    @staticmethod
    def _start_tree(directory: Path) -> tuple[subprocess.Popen[bytes], int]:
        parent_source = (
            "import pathlib, subprocess, sys, time\n"
            "root = pathlib.Path(sys.argv[1])\n"
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(900)'])\n"
            "temporary_pid = root / 'child-pid.part'\n"
            "temporary_pid.write_text(str(child.pid), encoding='ascii')\n"
            "temporary_pid.replace(root / 'child-pid')\n"
            "time.sleep(900)\n"
        )
        starter = cast("Callable[..., subprocess.Popen[bytes]]", getattr(test_gate, "_" + "start_owned_command"))
        process = starter(
            [sys.executable, "-c", parent_source, str(directory)],
        )
        deadline = time.monotonic() + 10
        pid_file = directory / "child-pid"
        child_handle: int | None = None
        try:
            while True:
                try:
                    child_pid = int(pid_file.read_text(encoding="ascii"))
                    break
                except (FileNotFoundError, PermissionError, ValueError):
                    # 文件已改名可见但 Windows 的句柄共享状态可能尚未释放。
                    # 仍须在原有期限内取得完整 PID，不能跳过实际后代退出断言。
                    if process.poll() is not None:
                        message = "进程树父进程未能创建子进程"
                        raise RuntimeError(message) from None
                    if time.monotonic() >= deadline:
                        message = "等待可读取的完整子进程 PID 超时"
                        raise TimeoutError(message) from None
                    time.sleep(0.01)
            child_handle = win32api.OpenProcess(
                win32con.SYNCHRONIZE | win32con.PROCESS_TERMINATE,
                0,
                child_pid,
            )
            return process, child_handle
        finally:
            if child_handle is None:
                if process.poll() is None:
                    killer = cast("Callable[..., bool]", getattr(test_gate, "_" + "kill_tree"))
                    _ = killer(process, timeout=3.0)
                _ = process.wait(timeout=5)
                closer = cast("Callable[..., bool]", getattr(test_gate, "_" + "close_owned_command"))
                _ = closer(process, timeout=3.0)

    def _assert_owner_kills_only_its_tree(self, owner: object) -> None:
        killer = cast("Callable[..., bool]", getattr(owner, "_" + "kill_tree"))
        with tempfile.TemporaryDirectory(prefix="jchtools-command-tree-") as temporary:
            process, child = self._start_tree(Path(temporary))
            unrelated: subprocess.Popen[bytes] | None = None
            try:
                unrelated = subprocess.Popen(
                    [sys.executable, "-c", "import time; time.sleep(900)"],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                assert killer(process, timeout=3.0)  # nosec B101: 应终止指定所有者的整棵进程树。
                assert process.wait(timeout=5) != 0  # nosec B101: 被杀的所有者进程不能自然成功。
                assert win32event.WaitForSingleObject(child, 1000) == win32event.WAIT_OBJECT_0  # nosec B101: 子进程必须退出。
                assert unrelated.poll() is None  # nosec B101: 不得终止无关进程。
            finally:
                if process.poll() is None:
                    process.kill()
                _ = process.wait(timeout=5)
                closer = cast("Callable[..., bool]", getattr(test_gate, "_" + "close_owned_command"))
                _ = closer(process, timeout=3.0)
                if win32event.WaitForSingleObject(child, 0) == win32event.WAIT_TIMEOUT:
                    win32api.TerminateProcess(child, 0)
                win32api.CloseHandle(child)
                if unrelated is not None:
                    if unrelated.poll() is None:
                        unrelated.terminate()
                    _ = unrelated.wait(timeout=5)

    def test_gate_and_timing_terminate_only_the_owned_process_tree(self) -> None:
        for owner in (test_gate, test_timing):
            with self.subTest(owner=owner.__name__):
                self._assert_owner_kills_only_its_tree(owner)

    def test_gate_owned_tree_cleanup_does_not_depend_on_taskkill_startup(self) -> None:
        # fulltest-5：taskkill 的启动/枚举可能耗尽两秒内部预算；树终止不能依赖
        # 外部命令及时启动。仍经真实父子进程与句柄检查，并保护无关进程。
        with patch("scripts.test_gate.subprocess.run", side_effect=subprocess.TimeoutExpired("taskkill", 2.0)):
            self._assert_owner_kills_only_its_tree(test_gate)

    def test_successful_stage_reaps_owned_child_after_parent_exits(self) -> None:
        # 父命令成功退出也可能遗留后台；只有启动前绑定的 Job 可以完整回收，
        # 不能对已退出父 PID 重新枚举并据此猜测所有权。
        with (
            tempfile.TemporaryDirectory(prefix="jchtools-successful-command-tree-") as temporary,
            patch.object(test_gate, "LOG_DIR", Path(temporary)),
        ):
            root = Path(temporary)
            release = root / "release-parent"
            parent_source = (
                "import pathlib, subprocess, sys, time\n"
                "root = pathlib.Path(sys.argv[1])\n"
                "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(900)'])\n"
                "temporary_pid = root / 'child-pid.part'\n"
                "temporary_pid.write_text(str(child.pid), encoding='ascii')\n"
                "temporary_pid.replace(root / 'child-pid')\n"
                "while not (root / 'release-parent').exists(): time.sleep(0.01)\n"
            )
            results: list[test_gate.StageResult] = []

            def run() -> None:
                results.append(
                    test_gate.run_logged(
                        "unit-successful-tree", [sys.executable, "-c", parent_source, str(root)], timeout=10.0
                    )
                )

            runner = threading.Thread(target=run)
            child: int | None = None
            unrelated = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(900)"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            runner.start()
            try:
                deadline = time.monotonic() + 5
                pid_file = root / "child-pid"
                while True:
                    try:
                        child_pid = int(pid_file.read_text(encoding="ascii"))
                        break
                    except (FileNotFoundError, PermissionError, ValueError):
                        # 原子改名已可见时，写入进程的 Windows 共享句柄可能尚未释放。
                        # 仅等待原有五秒预算，不跳过后续真实进程句柄与退出断言。
                        if time.monotonic() >= deadline:
                            self.fail("阶段未创建可读取的完整子进程 PID")
                        time.sleep(0.01)
                child = win32api.OpenProcess(win32con.SYNCHRONIZE | win32con.PROCESS_TERMINATE, 0, child_pid)
                _ = release.write_text("release", encoding="ascii")
                runner.join(timeout=10)
                assert not runner.is_alive()  # nosec B101: 阶段及清理必须在预算内结束。
                assert len(results) == 1  # nosec B101: 阶段应返回唯一结果。
                assert results[0].status == test_gate.STATUS_OK  # nosec B101: 父成功且树已清理。
                assert win32event.WaitForSingleObject(child, 0) == win32event.WAIT_OBJECT_0  # nosec B101: 后台已回收。
                assert unrelated.poll() is None  # nosec B101: 不得终止无关进程。
            finally:
                _ = release.write_text("release", encoding="ascii")
                runner.join(timeout=10)
                if child is not None:
                    if win32event.WaitForSingleObject(child, 0) == win32event.WAIT_TIMEOUT:
                        win32api.TerminateProcess(child, 0)
                    win32api.CloseHandle(child)
                if unrelated.poll() is None:
                    unrelated.terminate()
                _ = unrelated.wait(timeout=5)


class MediaFixtureOutputIsolationTests(unittest.TestCase):
    """覆盖 T-26：环境 JSON 不能覆盖准备器的媒体、来源收据或已校验工具。"""

    def test_output_cannot_overwrite_owned_fixture_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            component = root / "component"
            component.mkdir()
            (component / "xberg.exe").write_bytes(b"synthetic engine identity")
            workspace = root / ".tmp" / "media-e2e-fixture"
            for relative in ("fixture-receipt.json", "speech.mp4", "spoken.wav", "tools/ffmpeg.exe"):
                with self.subTest(output=relative):
                    output = workspace / relative
                    output.parent.mkdir(parents=True, exist_ok=True)
                    original = b"previous verified artifact"
                    output.write_bytes(original)
                    with (
                        patch.object(media_e2e_fixture, "ROOT", root),
                        patch.object(media_e2e_fixture, "WORKSPACE", workspace),
                        patch.object(media_e2e_fixture, "workspace", side_effect=AssertionError("拒绝前不得准备目录")),
                    ):
                        with self.assertRaises(ValueError):
                            media_e2e_fixture.prepare(component, output)
                    assert output.read_bytes() == original
