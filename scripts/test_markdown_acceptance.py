"""转 Markdown 验收回归单元测试（临时夹具与真实进程仅用于隔离边界）。."""

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
from scripts.markdown_acceptance_impl import gui
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

from scripts.markdown_acceptance_tests.cases_report import AcceptanceBoundaryRegressionTests, AcceptanceReportCompletenessTests, CommonFormatAcceptanceTests, ContentAssertStrengthTests, FormatSweepCoverageTests, PerInputRuleTests

from scripts.markdown_acceptance_tests.cases_media_semantics import A15AutoModeTests, A15WordingTests, A18UnsupportedBoundaryTests, A24SceneNoteTests, DocumentSemanticPredicateTests

from scripts.markdown_acceptance_tests.cases_broker_runtime import ClosedFormatConnectionTests, DynamicFormatAcceptanceTests, FormalDeliveryIsolationTests, PythonProcessScanTests, SharedFormatProtocolTests, UnconfiguredStateBoundaryTests

from scripts.markdown_acceptance_tests.cases_boundary_bundled import AcceptanceBoundaryTests, BundledDistributionTests, BundledProbeTests, CommandTimeoutTests, MarkdownSummaryIntegrationTests, MediaFixtureOutputIsolationTests, ProcessTreeOwnershipTests, SyntheticFixtureTests, TestGateEnvironmentTests

from scripts.markdown_acceptance_tests.cases_review import ReviewAcceptanceContentTests, ReviewRemediationAcceptanceTests


class MediaPostconditionTests(unittest.TestCase):
    """验证 T-14 图片目录与 A26 横切断言的边界。."""

    def test_docx_mixed_media_fixture_relationships_resolve_to_real_members(self) -> None:
        # 覆盖 T-14/T-15：A13 的健康图片必须真实可达，不能以坏夹具制造产品失败。
        path = markdown_acceptance.FIXTURES_DEFAULT / "matrix/docx_emf_wmf_raster.docx"
        with zipfile.ZipFile(path) as archive:
            relationships = ElementTree.fromstring(archive.read("word/_rels/document.xml.rels"))
            names = set(archive.namelist())
            for relationship in relationships:
                if relationship.get("Type", "").endswith("/image"):
                    member = posixpath.normpath("word/" + relationship.attrib["Target"])
                    assert member in names  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_svg_case_rejects_missing_or_duplicated_media(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            media = root / "sample_media"
            _ = media.mkdir()
            token_png = cast("Callable[[str], bytes]", getattr(markdown_acceptance, "_" + "token_png"))
            data = token_png("PNG-NEIGHBOR")
            _ = (media / "image_0.png").write_bytes(data)
            assert markdown_acceptance.verify_svg_raster_media(root) is not None  # nosec B101: 回归测试断言。
            _ = (media / "image_1.png").write_bytes(data)
            assert markdown_acceptance.verify_svg_raster_media(root) is not None  # nosec B101: 回归测试断言。

    def test_stop_fixture_contains_work_after_current_file(self) -> None:
        # 覆盖 T-23：单个文件自然结束不能冒充已请求停止并阻止后续文件。
        with tempfile.TemporaryDirectory() as temporary:
            media = Path(temporary) / "synthetic.mp4"
            _ = media.write_bytes(b"synthetic fixture")

            def inspect_batch(
                _tag: str,
                _exe: str,
                _body: object,
                *,
                pre: Callable[[], None],
                after: Callable[[], None],
            ) -> None:
                try:
                    pre()
                    inputs = [path for path in Path(temporary).rglob("*.mp4") if path != media]
                    assert len(inputs) > 1  # nosec B101: 停止必须面对尚未开始的后续工作。
                finally:
                    after()

            scratch = Path(temporary) / "batch"
            _ = scratch.mkdir()
            with (
                patch.dict(os.environ, {"JCHTOOLS_S5_MEDIA": str(media)}),
                patch("scripts.gui_smoke.tempfile.mkdtemp", return_value=str(scratch)),
                patch.object(gui_smoke, "run_stage", side_effect=inspect_batch),
            ):
                gui_smoke.s5_markdown_basic_chain("synthetic.exe")

    def test_confirm_read_failure_does_not_report_success(self) -> None:
        # 覆盖 X-02：读取确认框的瞬态异常不能冒充用户已确认并启动任务。
        with (
            patch.object(gui_smoke, "find_button"),
            patch.object(gui_smoke, "click"),
            patch("scripts.gui_smoke.time.sleep"),
            patch("scripts.gui_smoke.time.time", side_effect=[0, 0, 100]),
        ):
            try:
                gui_smoke.confirm_dialog(
                    cast("WindowSpecification", cast("object", _UnreadableConfirmWindow())),
                    timeout=1,
                )
            except RuntimeError:
                pass
            else:
                self.fail("UIA 异常不得冒充确认成功")

    def test_isolated_gui_rejects_binary_without_test_hooks_before_body(self) -> None:
        # 覆盖 XB-18/XB-21：测试配置尚未隔离时不得写入真实用户设置。
        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(gui_smoke, "start_owned_process", return_value=(MagicMock(), MagicMock())),
            patch.object(gui_smoke, "wait_window", return_value=(None, None)),
            patch.object(gui_smoke, "_wait_exit_or_kill", return_value=(False, 0)),
            patch("scripts.gui_smoke.time.sleep"),
            patch("scripts.gui_smoke.time.time", side_effect=[0, 100]),
            patch.object(gui_smoke, "assert_clean_exit"),
        ):
            try:
                gui_smoke.run_stage(
                    "isolation",
                    "synthetic.exe",
                    lambda _window: None,
                    env={"JCHTOOLS_TEST_STATE_DIR": temporary},
                )
            except RuntimeError:
                pass
            else:
                self.fail("未隔离的 GUI 不得执行测试写入")

    def test_owned_gui_exit_reaps_child_without_ending_unrelated_process(self) -> None:
        # 覆盖 XB-14/XB-22：测试 GUI 正常退出后只回收自己创建的后台，不能占用后续验收。
        parent_source = (
            "import pathlib, subprocess, sys, time\n"
            "root = pathlib.Path(sys.argv[1])\n"
            "while not (root / 'start').exists(): time.sleep(0.01)\n"
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(900)'])\n"
            "(root / 'child-pid').write_text(str(child.pid), encoding='ascii')\n"
        )
        with tempfile.TemporaryDirectory(prefix="jchtools-owned-tree-") as temporary:
            proc, owner = gui_smoke.start_owned_process([sys.executable, "-c", parent_source, temporary])
            unrelated: subprocess.Popen[bytes] | None = None
            try:
                # 从首个进程启动起覆盖清场；第二次启动失败也不能遗留等待信号的父进程。
                unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(900)"])
                _ = (Path(temporary) / "start").write_text("go", encoding="ascii")
                assert proc.wait(timeout=10) == 0  # nosec B101: 验证父进程自然成功退出。
                child_pid = int((Path(temporary) / "child-pid").read_text(encoding="ascii"))
                child = win32api.OpenProcess(win32con.SYNCHRONIZE | win32con.PROCESS_TERMINATE, 0, child_pid)
                try:
                    owner.close()
                    status = win32event.WaitForSingleObject(child, 1000)
                    assert status == win32event.WAIT_OBJECT_0, "GUI 已退出但所属后台仍存活"  # nosec B101: 实际子进程必须退出。
                    assert unrelated.poll() is None, "不得结束无关实例"  # nosec B101: 所有权边界。
                finally:
                    try:
                        if win32event.WaitForSingleObject(child, 0) == win32event.WAIT_TIMEOUT:
                            win32api.TerminateProcess(child, 0)
                    finally:
                        win32api.CloseHandle(child)
            finally:
                # 每个资源独立清场；所有权回收失败也不能跳过父进程和无关见证进程。
                try:
                    if owner is not None:
                        owner.close()
                finally:
                    try:
                        if proc.poll() is None:
                            proc.kill()
                        _ = proc.wait(timeout=10)
                    finally:
                        if unrelated is not None:
                            if unrelated.poll() is None:
                                unrelated.terminate()
                            _ = unrelated.wait(timeout=10)

    def test_fast_failed_conversion_finishes_without_outputs(self) -> None:
        # 覆盖 T-24：未观察到忙态且全部瞬间失败时，应收集失败诊断而不是空等到转换超时。
        completed = "成功 0 · 部分提取 0 · 失败 1 · 已有结果跳过 0 · 重复结果跳过 0 · 总耗时 0.1s"

        def available_button(_window: object, title: str) -> tuple[bool, bool]:
            return title == "开始转换", True

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.dict(
                os.environ,
                {
                    "JCHTOOLS_TEST_STATE_DIR": str(markdown_acceptance.ROOT / ".tmp" / "parallel-review" / "state"),
                    "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": str(
                        markdown_acceptance.ROOT / ".tmp" / "parallel-review" / "snap"
                    ),
                },
                clear=True,
            ),
            patch.object(isolation_probe, "_wait_process_isolation", return_value=True),
            patch.object(gui, "start_owned_process", return_value=(MagicMock(), MagicMock())),
            patch.object(gui, "_connect_window", return_value=(None, object())),
            patch.object(gui, "_click_button"),
            patch.object(gui, "_convert_directory_rows", return_value=[object(), object()]),
            patch.object(gui, "_set_row_edit"),
            patch.object(gui, "_wait_start_ready", return_value=True),
            patch.object(gui, "_button_state", side_effect=available_button),
            patch.object(gui, "_window_texts", side_effect=["尚未开始", completed, completed]),
            patch.object(gui, "_request_close"),
            patch.object(gui, "_terminate", return_value=(False, 0)),
            patch.object(gui, "CONVERSION_TIMEOUT", 1),
            patch("scripts.markdown_acceptance_impl.gui.time.time", side_effect=[0.0, 0.0, 0.0, 0.0, 2.0]),
            patch("scripts.markdown_acceptance_impl.gui.time.sleep"),
        ):
            root = Path(temporary)
            source, output = root / "input", root / "output"
            source.mkdir()
            output.mkdir()
            # T-09 要求输入输出不同；本例只模拟瞬时失败，不混入非法目录前置。
            run = markdown_acceptance.drive_conversion(root / "gui.exe", source, output)
        assert run.error is None, "收尾状态不能误判为驱动超时"  # nosec B101: 瞬间失败的状态转换回归。
        assert run.outputs == [], "失败批次不得伪造产物"  # nosec B101: 失败产物边界。
        assert "失败 1" in run.texts  # nosec B101: 必须保留真实失败诊断。

    def test_legal_media_directory_and_reference_pass(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            _ = source.mkdir()
            _ = output.mkdir()
            original = source / "文档.docx"
            _ = original.write_bytes(b"source")
            media = output / "文档_docx_media" / "图片 0.png"
            _ = media.parent.mkdir()
            _ = media.write_bytes(b"png-bytes")
            _ = (output / "文档_docx.md").write_text("![图片](<文档_docx_media/图片 0.png>)\n", encoding="utf-8")

            problems = verify_common_postconditions(
                source,
                output,
                {Path("文档.docx"): _digest(original)},
            )

            assert problems == []  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_percent_encoded_media_reference_resolves_to_original_filename(self) -> None:
        # 覆盖 T-14：引擎前缀中的 &、百分号和括号经 URL 转义后仍指向原落盘文件。
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "input", root / "output"
            source.mkdir()
            output.mkdir()
            original = source / "中文&copy;% (图).docx"
            original.write_bytes(b"source")
            media = output / "中文&copy;%_(图)_docx_media" / "image_0.png"
            media.parent.mkdir()
            media.write_bytes(b"png-bytes")
            (output / "中文&copy;% (图)_docx.md").write_text(
                "![](中文%26copy;%25_%28图%29_docx_media/image_0.png)\n", encoding="utf-8"
            )
            assert verify_common_postconditions(source, output, {Path(original.name): _digest(original)}) == []

    def test_encoded_absolute_media_reference_is_rejected_before_path_resolution(self) -> None:
        # 覆盖 T-14/T-21：不能解析本地或 UNC 绝对引用，尤其不能访问远程共享再判越界。
        collector = getattr(markdown_acceptance, "_" + "collect_media_references")
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            markdown = output / "sample.md"
            markdown.write_text("![](%2F%2Fserver/share/image.png)\n![](C%3A%2Foutside/image.png)\n", encoding="utf-8")
            with patch.object(Path, "resolve", side_effect=AssertionError("绝对引用不得解析")):
                problems, referenced = collector(markdown, output)
            assert len(problems) == 2
            assert not referenced

    def test_duplicate_uia_button_nodes_collapse_to_physical_rows(self) -> None:
        buttons = [
            _FakeButton(_FakeRect(10, 100, 110, 140), "选择目录…"),
            _FakeButton(_FakeRect(10, 100, 110, 140), "选择目录…"),
            _FakeButton(_FakeRect(10, 180, 110, 220), "选择目录…"),
            _FakeButton(_FakeRect(10, 180, 110, 220), "选择目录…"),
        ]

        rows = unique_visible_buttons(cast("WindowSpecification", cast("object", _FakeWindow(buttons))), "选择目录…")

        assert len(rows) == EXPECTED_PHYSICAL_ROWS  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
        assert [row.rectangle().top for row in rows] == [100, 180]  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_missing_smoke_button_returns_pollable_spec(self) -> None:
        window = cast("WindowSpecification", cast("object", _FakeSmokeWindow()))

        result = smoke_find_button(window, "停止任务")

        assert result is not None  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_escape_reference_and_literal_code_are_handled(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            _ = source.mkdir()
            _ = output.mkdir()
            original = source / "sample.docx"
            _ = original.write_bytes(b"source")
            _ = (output / "sample_docx.md").write_text(
                """![escape](../外部_media/逃逸.png)
![unc](//server/share/外部_media/UNC.png)
`![literal](文档_media/字面.png)`
```markdown
![fenced](文档_media/围栏.png)
```
""",
                encoding="utf-8",
            )

            problems = verify_common_postconditions(
                source,
                output,
                {Path("sample.docx"): _digest(original)},
            )

            assert any("图片引用逃出输出目录" in problem for problem in problems)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            assert any("UNC.png" in problem for problem in problems)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            assert not any("字面.png" in problem for problem in problems)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            assert not any("围栏.png" in problem for problem in problems)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_shorter_fence_inside_long_code_block_is_not_a_media_reference(self) -> None:
        # 覆盖 T-14/T-17：四字符围栏内的三字符字面围栏不能提前结束代码块。
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            for marker in ("`", "~"):
                with self.subTest(marker=marker):
                    (output / "sample.md").write_text(
                        f"{marker * 4}markdown\n{marker * 3}\n![](missing.png)\n{marker * 4}\n",
                        encoding="utf-8",
                    )
                    assert verify_common_postconditions(output, output, {}) == []

    def test_longer_closing_fence_and_unclosed_fence_preserve_literal_links(self) -> None:
        # 覆盖 T-17：合法加长闭合与文件尾未闭合的代码围栏都不产生图片资源。
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            for text in ("~~~markdown\n![](missing.png)\n~~~~\n", "```markdown\n![](missing.png)\n"):
                with self.subTest(text=text):
                    (output / "sample.md").write_text(text, encoding="utf-8")
                    assert verify_common_postconditions(output, output, {}) == []

    def test_review_fix_container_fences_ignore_literal_media_but_check_real_references(self) -> None:
        # 覆盖 T-14/T-17、附录 A26：引用块及已有两空格列表续行中的代码不是图片。
        containers = (
            ("quote", "", "> "),
            ("nested-quote", "", "> > "),
            ("indented-quote", "", "  > "),
            ("list-continuation", "- 项目\n", "  "),
            ("quote-in-list", "- 项目\n", "  > "),
        )
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            for name, lead, prefix in containers:
                for marker in ("`", "~"):
                    for closing in ("same", "longer", "shorter-then-close"):
                        with self.subTest(container=name, marker=marker, closing=closing):
                            body = [prefix + marker * 4 + "markdown\n"]
                            if closing == "shorter-then-close":
                                body.append(prefix + marker * 3 + "\n")
                            body.append(prefix + "![](literal-missing.png)\n")
                            body.append(prefix + marker * (5 if closing == "longer" else 4) + "\n")
                            (output / "sample.md").write_text(
                                "![](before-missing.png)\n\n"
                                + lead
                                + "".join(body)
                                + prefix
                                + "![](container-real-missing.png)\n\n![](after-missing.png)\n",
                                encoding="utf-8",
                            )
                            self.assertEqual(
                                verify_common_postconditions(output, output, {}),
                                [
                                    "图片引用目标不存在：sample.md -> before-missing.png",
                                    "图片引用目标不存在：sample.md -> container-real-missing.png",
                                    "图片引用目标不存在：sample.md -> after-missing.png",
                                ],
                            )

    def test_review_fix_unclosed_container_fence_ends_with_its_container(self) -> None:
        # 覆盖 T-14/T-17：未闭合围栏到引用块/列表结束即止，不能吞掉后续真实图片。
        containers = (("", "> "), ("", "> > "), ("- 项目\n", "  "), ("- 项目\n", "  > "))
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            for lead, prefix in containers:
                for marker in ("`", "~"):
                    for ending in ("", "\n![](outside-missing.png)\n"):
                        with self.subTest(prefix=prefix, marker=marker, ending=ending):
                            (output / "sample.md").write_text(
                                lead
                                + prefix
                                + marker * 3
                                + "markdown\n"
                                + prefix
                                + "![](literal-missing.png)\n"
                                + ending,
                                encoding="utf-8",
                            )
                            self.assertEqual(
                                verify_common_postconditions(output, output, {}),
                                ["图片引用目标不存在：sample.md -> outside-missing.png"] if ending else [],
                            )

    def test_review_fix_container_fence_mask_preserves_offsets_and_line_endings(self) -> None:
        # 覆盖 T-17：屏蔽只替换围栏字符，不移动围栏外文字、Unicode 或 CRLF 的位置。
        masker = getattr(markdown_acceptance, "_" + "mask_markdown_fences")
        for prefix in ("> ", "> > ", "  > ", "  "):
            with self.subTest(prefix=prefix):
                before = "前文 ![](before.png)\r\n"
                literal = prefix + "~~~~markdown\r\n" + prefix + "![](字面.png)\r\n" + prefix + "~~~~~\r\n"
                after = prefix + "后文 ![](after.png)\r\n"
                text = before + literal + after
                expected_literal = "".join(character if character in "\r\n" else " " for character in literal)
                masked = masker(text)
                self.assertEqual(masked, before + expected_literal + after)
                self.assertEqual(len(masked), len(text))
                self.assertEqual(masked.index("![](after.png)"), text.index("![](after.png)"))

    def test_missing_reference_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            _ = source.mkdir()
            _ = output.mkdir()
            original = source / "sample.docx"
            _ = original.write_bytes(b"source")
            _ = (output / "sample_docx.md").write_text("![image](sample_docx_media/missing.png)\n", encoding="utf-8")

            problems = verify_common_postconditions(
                source,
                output,
                {Path("sample.docx"): _digest(original)},
            )

            assert any("图片引用目标不存在" in problem for problem in problems)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_orphan_media_temp_file_and_source_change_are_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            _ = source.mkdir()
            _ = output.mkdir()
            original = source / "sample.docx"
            _ = original.write_bytes(b"source")
            media = output / "sample_docx_media" / "orphan.png"
            _ = media.parent.mkdir()
            _ = media.write_bytes(b"png-bytes")
            _ = (output / ".jch-markdown-leftover.tmp").write_bytes(b"partial")
            _ = original.write_bytes(b"changed")

            problems = verify_common_postconditions(
                source,
                output,
                {Path("sample.docx"): hashlib.sha256(b"source").hexdigest()},
            )

            assert any("图片没有正文引用" in problem for problem in problems)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            assert any("输出残留临时文件" in problem for problem in problems)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            assert any("源文件被改动" in problem for problem in problems)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_acceptance_partial_and_snap_not_run_are_coverage_gaps(self) -> None:
        text = (
            "PASS  package（未执行）\n"
            "PARTIAL  markdown-acceptance（已执行项通过，仍有 NOT RUN）\n"
            "NOT RUN  snap-ocr-worker-root（缺少隔离资产）"
        )

        gaps = acceptance_coverage_gaps(text)

        assert len(gaps) == EXPECTED_COVERAGE_GAPS  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
        assert any("markdown-acceptance" in gap for gap in gaps)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
        assert any("snap-ocr-worker-root" in gap for gap in gaps)  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_acceptance_package_not_run_is_not_a_fulltest_gap(self) -> None:
        text = "PASS  markdown-acceptance\nPASS  snap-ocr-worker-root\nNOT RUN  package（加 -WithPackage）"

        assert acceptance_coverage_gaps(text) == []  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_runtime_probe_uses_isolated_sqlite_over_fixed_env_hint(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            state = root / "state"
            asset_root = root / "assets"
            selected = root / "selected-xberg"
            fixed_hint = root / "stale-fixed-hint"
            _ = state.mkdir()
            _ = asset_root.mkdir()
            _ = selected.mkdir()
            _ = fixed_hint.mkdir()
            _ = (selected / "xberg.exe").write_bytes(b"selected")
            _ = (fixed_hint / "xberg.exe").write_bytes(b"stale")
            database = state / "config.sqlite3"
            connection = sqlite3.connect(database)
            try:
                _ = connection.execute("CREATE TABLE app_settings (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL)")
                _ = connection.execute(
                    "INSERT INTO app_settings(key,value) VALUES('xberg_directory',?)",
                    (str(selected),),
                )
                connection.commit()
            finally:
                connection.close()

            with patch.dict(
                os.environ,
                {
                    "JCHTOOLS_TEST_STATE_DIR": str(state),
                    "JCHTOOLS_TEST_XBERG_DIR": str(fixed_hint),
                },
                clear=False,
            ):
                missing: list[str] = []
                probe_runtime = cast(
                    "Callable[[Path, list[str]], Path | None]",
                    getattr(markdown_acceptance, "_" + "probe_runtime_dir"),
                )
                runtime = probe_runtime(asset_root, missing)

            assert runtime == selected  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
            assert missing == []  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_acceptance_forces_utf8_for_python_children(self) -> None:
        acceptance = Path(__file__).with_name("acceptance.ps1").read_text(encoding="utf-8")

        assert "PYTHONIOENCODING" in acceptance  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
        assert "PYTHONUTF8" in acceptance  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
        assert "Set-Content -LiteralPath $log -Encoding UTF8" in acceptance  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

    def test_markdown_acceptance_isolates_snap_assets(self) -> None:
        # 覆盖 XB-14：转换 GUI 初始化不能唤起生产截图服务，抢占用户会话引擎。
        acceptance = Path(__file__).with_name("acceptance.ps1").read_text(encoding="utf-8")
        assert "Set-Item -Path Env:JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT -Value $mdSnapAssetRoot" in acceptance  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

if __name__ == "__main__":
    _ = unittest.main()
