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
class AcceptanceReportCompletenessTests(unittest.TestCase):
    """覆盖 P-12/P-13：部分执行和缺资产不能冒充完整验收。."""

    def test_any_unexecuted_item_prevents_success_exit(self) -> None:
        results = [
            (markdown_acceptance.ITEMS[0], markdown_acceptance.Outcome(markdown_acceptance.STATUS_OK)),
            (markdown_acceptance.ITEMS[1], markdown_acceptance.Outcome(markdown_acceptance.STATUS_NOT_RUN, "缺资产")),
        ]
        with patch("sys.stdout", new=io.StringIO()):
            code = report_acceptance(results)
        assert code == UNVERIFIED_EXIT_CODE, "NOT RUN 必须使验收返回未验证退出码"  # nosec B101: 验收回归断言。

    def test_omitted_items_prevent_success_exit(self) -> None:
        with patch("sys.stdout", new=io.StringIO()):
            code = report_acceptance(
                [(markdown_acceptance.ITEMS[0], markdown_acceptance.Outcome(markdown_acceptance.STATUS_OK))]
            )
        assert code == UNVERIFIED_EXIT_CODE, "--only 的子集成功不能冒充完整验收"  # nosec B101: 验收回归断言。


class CommonFormatAcceptanceTests(unittest.TestCase):
    """覆盖 P-12/T-18/T-19：用户确认的常用格式范围不豁免结果及安全判据。."""

    def test_common_profile_keeps_office_pdf_mp4_without_media_synthesis(self) -> None:
        items = {item.item_id: item for item in markdown_acceptance.profile_items("common")}
        required = [item for key, item in items.items() if key not in markdown_acceptance.OPTIONAL_COMMON_ITEMS]
        formats = {Path(name).suffix.lstrip(".") for item in required if item.group == "A" for name in item.fixtures}
        assert formats == markdown_acceptance.COMMON_FORMATS  # nosec B101: 验收判据回归断言。
        assert items["A25"].fixtures == ("common/legacy.doc", "common/legacy.xls", "common/legacy.ppt")  # nosec B101: 验收判据回归断言。
        assert items["A24"].synth == ""  # nosec B101: 验收判据回归断言。
        assert all(item.synth != "office" for item in required)  # nosec B101: 验收判据回归断言。
        assert {item.item_id for item in required if item.group != "A"} == {  # nosec B101: 验收判据回归断言。
            item.item_id for item in markdown_acceptance.ITEMS if item.group != "A"
        }

    def test_full_profile_preserves_existing_matrix_and_synthesis(self) -> None:
        items = markdown_acceptance.profile_items("full")
        assert items == list(markdown_acceptance.ITEMS)  # nosec B101: 验收判据回归断言。
        assert next(item for item in items if item.item_id == "A24").synth == "media"  # nosec B101: 验收判据回归断言。

    def test_mp4_duration_without_transcript_is_rejected(self) -> None:
        verifier = cast(
            "Callable[[Path, tuple[str, ...]], str | None]",
            getattr(markdown_acceptance, "_" + "verify_a24_video"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            result = output / "video-to-notes-intro-zh_mp4.md"
            header = "# video-to-notes-intro-zh\n- 音频时长: 00:01:19.125\n- 语音片段: 1\n## 转录\n"
            _ = result.write_text(header, encoding="utf-8")
            assert verifier(output, ("mp4",)) is not None  # nosec B101: 验收判据回归断言。
            _ = result.write_text(header + "[00:00:00.108 --> 00:00:02.508] 长视频想快速获取要点。\n", encoding="utf-8")
            assert verifier(output, ("mp4",)) is None  # nosec B101: 验收判据回归断言。

    def test_all_media_segments_and_unique_metadata_are_checked(self) -> None:
        checker = getattr(markdown_acceptance, "_" + "media_markdown_problem")
        healthy = (
            "# 合成媒体\n- 音频时长: 00:00:03.000\n- 语音片段: 2\n## 转录\n"
            "[00:00:00.100 --> 00:00:01.000] 第一段\n"
            "[00:00:01.500 --> 00:00:02.500] 第二段\n"
        )
        assert checker(healthy, require_chinese=True) is None
        broken = (
            healthy.replace("00:00:03.000", "00:00:00.000"),
            healthy.replace("00:00:03.000", "00:60:03.000"),
            healthy.replace("- 语音片段: 2", "- 语音片段: 1"),
            healthy.replace("- 语音片段: 2", "- 语音片段: 2\n- 语音片段: 2"),
            healthy.replace("## 转录\n", "- 音频时长: 00:00:03.000\n## 转录\n"),
            healthy.replace("00:00:02.500", "00:00:04.000"),
            healthy.replace("00:00:02.500", "00:00:01.500"),
            healthy.replace("00:00:01.500", "00:00:00.050"),
            healthy.replace("第二段", " "),
            healthy.replace("第一段", "English").replace("第二段", "English"),
            healthy + "## 转录\n",
            healthy.replace("[00:00:01.500 --> 00:00:02.500] 第二段", "无时间戳的伪正文"),
        )
        for text in broken:
            with self.subTest(text=text):
                assert checker(text, require_chinese=True) is not None
        trackless = "# empty\n- 音频时长: 无音频轨道\n- 语音片段: 0\n## 转录\n（无音频轨道）\n"
        assert checker(trackless, empty_scene="无音频轨道") is None
        assert checker(trackless.replace("0\n", "1\n"), empty_scene="无音频轨道") is not None
        assert checker(trackless.replace("（无音频轨道）", "（未检测到语音）"), empty_scene="无音频轨道") is not None

    def test_old_office_outputs_cannot_exchange_document_markers(self) -> None:
        item = next(item for item in markdown_acceptance.profile_items("common") if item.item_id == "A25")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "a25-common-office" / "output"
            output.mkdir(parents=True)
            for extension in ("doc", "xls", "ppt"):
                _ = (output / f"legacy_{extension}.md").write_text("JCHTOOLS-LEGACY-DOC", encoding="utf-8")
            with (
                patch("scripts.markdown_acceptance_impl.runner.SCRATCH_ROOT", root),
                patch(
                    "scripts.markdown_acceptance_impl.runner._run_conversion_item",
                    return_value=markdown_acceptance.Outcome(markdown_acceptance.STATUS_OK),
                ),
            ):
                outcome = _run_handler("run_common_old_office", item, MagicMock(spec=markdown_acceptance.Context))
                assert outcome.status == markdown_acceptance.STATUS_FAILED  # nosec B101: 验收判据回归断言。

    def test_optional_items_do_not_count_as_pass_or_hide_missing_required_item(self) -> None:
        reporter = cast(
            "Callable[[list[tuple[markdown_acceptance.Item, markdown_acceptance.Outcome]], Path | None, str], int]",
            getattr(markdown_acceptance, "_" + "print_report"),
        )
        required = [
            (item, markdown_acceptance.Outcome(markdown_acceptance.STATUS_OK))
            for item in markdown_acceptance.profile_items("common")
            if item.item_id not in markdown_acceptance.OPTIONAL_COMMON_ITEMS
        ]
        with tempfile.TemporaryDirectory() as temporary, patch("sys.stdout", new=io.StringIO()):
            report = Path(temporary) / "report.json"
            assert reporter(required, report, "common") == 0  # nosec B101: 验收判据回归断言。
            payload = cast("dict[str, dict[str, int]]", json.loads(report.read_text(encoding="utf-8")))
            assert payload["summary"]["pass"] == len(required)  # nosec B101: 验收判据回归断言。
            assert payload["summary"]["optional"] == len(markdown_acceptance.OPTIONAL_COMMON_ITEMS)  # nosec B101: 验收判据回归断言。
            assert (  # nosec B101: 验收判据回归断言。
                reporter([(item, result) for item, result in required if item.item_id != "A25"], None, "common")
                == UNVERIFIED_EXIT_CODE
            )
            failed = [
                (
                    item,
                    markdown_acceptance.Outcome(markdown_acceptance.STATUS_FAILED) if item.item_id == "A24" else result,
                )
                for item, result in required
            ]
            assert reporter(failed, None, "common") == 1  # nosec B101: 验收判据回归断言。


class FormatSweepCoverageTests(unittest.TestCase):
    """覆盖 T-08 / 附录 A25：未实际纳入矩阵输入的格式必须进入 sweep。."""

    def test_uncovered_declared_formats_are_not_excluded(self) -> None:
        uncovered = (
            "doc",
            "dot",
            "ppt",
            "pps",
            "pot",
            "xls",
            "xlt",
            "xltm",
            "xla",
            "odt",
            "ods",
            "odp",
            "pnm",
            "jbig2",
        )
        rows: list[object] = [
            {"extension": extension, "mime_type": "application/test"}
            for extension in (*uncovered, "pdf", "docx", "pptx", "xlsx", "pbm")
        ]
        context = markdown_acceptance.Context(None, None, None, Path(), markdown_acceptance.probe_assets())
        covered_matrix = (
            markdown_acceptance.Item(
                "A01",
                "A",
                "受控格式覆盖",
                "合成入口",
                fixtures=("input.pdf", "input.docx", "input.pptx", "input.xlsx", "input.pbm"),
            ),
        )
        with (
            patch.object(formats, "ITEMS", covered_matrix),
            patch.object(runner, "_shared_format_rows", return_value=(rows, None)),
        ):
            extensions, error = markdown_acceptance.xberg_format_extensions(context)
        assert error is None  # nosec B101: 格式清单解析回归断言。
        assert extensions == sorted(uncovered)  # nosec B101: 漏覆盖格式不能被硬编码名单排除。

    def test_covered_formats_follow_registered_matrix_inputs(self) -> None:
        declared = ("pdf", "docx", "pptx", "xlsx", "pbm")
        rows: list[object] = [{"extension": extension, "mime_type": "application/test"} for extension in declared]
        context = markdown_acceptance.Context(None, None, None, Path(), markdown_acceptance.probe_assets())
        with (
            patch.object(formats, "ITEMS", ()),
            patch.object(runner, "_shared_format_rows", return_value=(rows, None)),
        ):
            extensions, error = markdown_acceptance.xberg_format_extensions(context)
        assert error is None  # nosec B101: 格式清单解析回归断言。
        assert extensions == sorted(declared)  # nosec B101: 矩阵不含输入时不得宣称已有覆盖。

    def test_synthetic_format_table_matches_real_inputs(self) -> None:
        table = cast(
            "dict[str, frozenset[str]]",
            getattr(markdown_acceptance, "_" + "SYNTH_INPUT_EXTENSIONS"),
        )
        assert set(table) == set(markdown_acceptance.SYNTHESIZERS)  # nosec B101: 新合成器不能遗漏格式映射。
        with tempfile.TemporaryDirectory() as temporary:
            for name, synthesize in markdown_acceptance.SYNTHESIZERS.items():
                if name == "media":
                    continue
                target = Path(temporary) / name
                _ = target.mkdir()
                result = synthesize(target, markdown_acceptance.FIXTURES_DEFAULT)
                assert not result.error, result.error  # nosec B101: 真实合成器必须构造成功。
                actual = {Path(filename).suffix.lstrip(".").lower() for filename in result.files}
                assert actual == table[name], name  # nosec B101: 覆盖表不能凭空增加实际合成器未产出的格式。
        media_files = markdown_acceptance.A24_MEDIA_SYNTH_FILES
        actual_media = {Path(filename).suffix.lstrip(".").lower() for filename in media_files}
        assert actual_media == table["media"]  # nosec B101: 媒体格式采用同源产物清单，不启动实际解码。

    def test_missing_legacy_fixture_is_not_run(self) -> None:
        item = markdown_acceptance.Item("A25", "A", "格式清点", "GUI", needs_assets="xberg")
        rows: list[object] = [
            {"extension": extension, "mime_type": "application/msword"}
            for extension in ("pdf", "docx", "pptx", "xlsx", "doc")
        ]
        with tempfile.TemporaryDirectory() as temporary:
            _ = (Path(temporary) / "matrix" / "format_sweep").mkdir(parents=True)
            context = markdown_acceptance.Context(
                None,
                None,
                None,
                Path(temporary),
                markdown_acceptance.AssetProbe(None, None, Path("xberg.exe"), None, []),
                format_rows=rows,
            )
            with (
                patch.object(
                    runner, "_run_conversion_item", return_value=markdown_acceptance.Outcome("PASS")
                ) as convert,
            ):
                outcome = _run_handler("run_matrix_a25", item, context)
        assert outcome.status == "NOT RUN"  # nosec B101: 缺健康样本不得执行或报告通过。
        assert "doc" in outcome.reason  # nosec B101: 报告必须列出缺失扩展名。
        convert.assert_not_called()

    def test_sweep_does_not_mutate_registered_item(self) -> None:
        # 覆盖 P-12/P-13：运行时夹具清点不能改写用于后续选择和报告的登记表。
        item = markdown_acceptance.Item("A25", "A", "格式清点", "GUI", needs_assets="xberg")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sweep = root / "matrix" / "format_sweep"
            sweep.mkdir(parents=True)
            (sweep / "sample.rst").write_text("公开合成正文", encoding="utf-8")
            context = markdown_acceptance.Context(
                None,
                None,
                None,
                root,
                markdown_acceptance.AssetProbe(None, None, Path("xberg.exe"), None, []),
            )
            with (
                patch.dict(_a25_rules(), clear=True),
                patch.object(runner, "xberg_format_extensions", return_value=(["rst"], None)),
                patch.object(
                    runner,
                    "_run_conversion_item",
                    return_value=markdown_acceptance.Outcome("PASS"),
                ) as convert,
            ):
                outcome = _run_handler("run_matrix_a25", item, context)
                assert outcome.status == "PASS"
                converted_item = convert.call_args.args[0]
                assert converted_item.fixtures == ("matrix/format_sweep/sample.rst",)
                assert item.fixtures == ()


class PerInputRuleTests(unittest.TestCase):
    """S10-02/04：逐输入断言——整族失败、缺诊断、夹具不足都不得假 PASS。."""

    def test_a25_dynamic_rules_tighten_engine_declared_formats(self) -> None:
        # S10-02 复审：引擎声明的格式必须有产物（防引擎回归假 PASS）；未声明
        # 格式保留宽松兜底；动态规则缺席时回落静态默认（全宽松）。
        rules = _a25_rules()
        # 缺席规则的前置由本例建立，不能把恢复的调用方规则误当成空表。
        with patch.dict(rules, clear=True):
            rules["sample_odt.odt"] = markdown_acceptance.InputRule(must_produce=True)
            rules["sample_legacy.wpd"] = markdown_acceptance.InputRule(must_produce=False)
            assert _input_rule("A25", "sample_odt.odt").must_produce  # nosec B101: 回归测试断言。
            assert not _input_rule("A25", "sample_legacy.wpd").must_produce  # nosec B101: 回归测试断言。
            rules.clear()
            assert not _input_rule("A25", "sample_odt.odt").must_produce  # nosec B101: 回归测试断言。

    def test_healthy_family_all_failing_is_reported(self) -> None:
        # A21：即使 DOCM/DOTX/DOTM 全失败（只产出一份 docx），也必须报问题。
        files = ["chartex.docx", "chartex_as_docm.docm", "chartex_as_dotx.dotx", "chartex_as_dotm.dotm"]
        assert _per_input_problems("A21", files, ["chartex_docx.md"], "")  # nosec B101: 回归测试断言。

    def test_expected_failure_requires_diagnosis(self) -> None:
        assert _per_input_problems("A20", ["empty.png"], [], "")  # nosec B101: 无产物且无失败诊断。
        diagnosed = "失败：empty.png · 解码失败"
        assert _per_input_problems("A20", ["empty.png"], [], diagnosed) == []  # nosec B101
        assert _per_input_problems("A20", ["empty.png"], ["empty_png.md"], diagnosed)  # nosec B101: 不留半成品。

    def test_expected_failure_requires_failure_diagnostic(self) -> None:
        # 覆盖 T-24/P-12：当前文件、统计或无原因的失败行不证明该输入已失败。
        for texts in ("当前文件：empty.png", "empty.png\n失败 1", "失败：empty.png"):
            with self.subTest(texts=texts):
                assert _per_input_problems("A20", ["empty.png"], [], texts)  # nosec B101: 必须有该文件的失败原因。
        assert _per_input_problems("A20", ["empty.png"], [], "失败：empty.png · 解码失败") == []  # nosec B101

    def test_optional_and_sweep_inputs_do_not_require_product(self) -> None:
        assert _per_input_problems("A16", ["alpha.png"], [], "") == []  # nosec B101: 宽松输入。
        with patch.dict(_a25_rules(), clear=True):
            assert _per_input_problems("A25", ["sample.rst"], [], "") == []  # nosec B101: 未声明时保留子集语义。
            _a25_rules()["sample.rst"] = markdown_acceptance.InputRule(must_produce=True)
            assert _per_input_problems("A25", ["sample.rst"], [], "")  # nosec B101: 本轮声明支持则缺产物必须失败。
            assert _per_input_problems("A25", ["sample.rst"], ["sample_rst.md"], "") == []  # nosec B101: 有对应产物才通过。

    def test_each_top_level_input_rejects_duplicate_result_in_another_directory(self) -> None:
        # 覆盖 T-10/A26：不能只比较文件名集合，把一份顶层输入的两份产物判为通过。
        assessor = getattr(markdown_acceptance, "_" + "assess_conversion")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "input", root / "output"
            source.mkdir()
            output.mkdir()
            original = source / "sample.docx"
            original.write_bytes(b"source")
            first = output / "sample_docx.md"
            second = output / "duplicate" / first.name
            second.parent.mkdir()
            for result in (first, second):
                result.write_text("公开合成正文", encoding="utf-8")
            item = markdown_acceptance.Item("A21", "A", "逐输入唯一产物", "GUI")
            prepared = markdown_acceptance.PreparedInputs(source, output, [original.name])
            run = markdown_acceptance.GuiRun([first, second], "成功 1，失败 0", None)
            for stop_mode in (False, True):
                with self.subTest(stop_mode=stop_mode):
                    outcome = assessor(
                        item, prepared, {Path(original.name): _digest(original)}, run, stop_mode=stop_mode
                    )
                    assert outcome.status == markdown_acceptance.STATUS_FAILED
                    assert "重复" in outcome.reason

    def test_empty_healthy_output_cannot_pass_non_a26_item(self) -> None:
        # 覆盖 P-12/T-25：任何健康文档条目和停止后的成品都不能把空文件计为通过。
        assessor = getattr(markdown_acceptance, "_" + "assess_conversion")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "input", root / "output"
            source.mkdir()
            output.mkdir()
            original = source / "sample.docx"
            original.write_bytes(b"source")
            result = output / "sample_docx.md"
            result.write_text(" \n", encoding="utf-8")
            item = markdown_acceptance.Item("A21", "A", "健康输入正文", "GUI")
            prepared = markdown_acceptance.PreparedInputs(source, output, [original.name])
            run = markdown_acceptance.GuiRun([result], "成功 1，失败 0", None)
            for stop_mode in (False, True):
                with self.subTest(stop_mode=stop_mode):
                    outcome = assessor(
                        item, prepared, {Path(original.name): _digest(original)}, run, stop_mode=stop_mode
                    )
                    assert outcome.status == markdown_acceptance.STATUS_FAILED
                    assert "正文为空" in outcome.reason

    def test_a11_missing_broken_preview_fixture_is_not_run(self) -> None:
        # 缺少注入变体时必须由实际执行前置返回 NOT RUN，不能只锁定仓库当前缺少夹具。
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "A11")
        runner = cast(
            "Callable[..., markdown_acceptance.Outcome]",
            getattr(markdown_acceptance, "_" + "run_conversion_item"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            context = markdown_acceptance.Context(
                None,
                None,
                None,
                Path(temporary),
                markdown_acceptance.AssetProbe(None, None, None, None, []),
            )
            with (
                patch.object(runner, "_resolve_gui", return_value=(Path("JchTools.exe"), None)),
                patch.object(runner, "_asset_precondition", return_value=None),
                patch.object(
                    runner,
                    "_prepare_scratch",
                    side_effect=AssertionError("缺夹具时不得准备或运行转换"),
                ),
            ):
                outcome = runner(item, context, Path("JchTools.exe"))
        assert outcome.status == "NOT RUN"  # nosec B101: 缺夹具必须作为未执行项报告。
        assert "matrix/pptx_ole_broken_preview.pptx" in outcome.reason  # nosec B101: 必须指出具体缺失变体。


class ContentAssertStrengthTests(unittest.TestCase):
    """S10-03/05：A01/A03/A08/A09/A14 的次数、顺序、邻近、字段值与 alt 断言。."""

    def test_a03_requires_exact_shared_media_count(self) -> None:
        assert _content_spec("A03").require_counts == (("SHARED-MEDIA", 2),)  # nosec B101
        assert _content_problems("SHARED-MEDIA 只出现一次", "", "A03")  # nosec B101
        assert _content_problems("SHARED-MEDIA\nSHARED-MEDIA", "", "A03") == []  # nosec B101

    def test_a01_token_image_order_detects_swap(self) -> None:
        good = "![a](m/image_0.png)\nBETA-TWO\n![b](m/image_1.png)\nALPHA-ONE\n"
        assert _content_problems(good, "", "A01") == []  # nosec B101
        swapped = "![a](m/image_0.png)\nALPHA-ONE\n![b](m/image_1.png)\nBETA-TWO\n"
        assert _content_problems(swapped, "", "A01")  # nosec B101: 换图后必须可检出。

    def test_a08_field_value_must_survive_between_runs(self) -> None:
        assert _content_problems("RUN-AND-FIELD7-FIELD-END", "", "A08") == []  # nosec B101
        assert _content_problems("RUN-AND-FIELD 7 -FIELD-END", "", "A08") == []  # nosec B101
        assert _content_problems("RUN-AND-FIELD-FIELD-END", "", "A08")  # nosec B101: 字段值丢失。

    def test_a08_wrong_field_value_cannot_pass_as_preserved(self) -> None:
        # 覆盖 T-15：夹具的可见字段固定为 7，任意非空字符不证明字段值完整保留。
        for value in ("8", "X", "?", "77"):
            with self.subTest(value=value):
                assert _content_problems(f"RUN-AND-FIELD{value}-FIELD-END", "", "A08")

    def test_a09_description_only_allowed_in_alt(self) -> None:
        ok = "![DESCR-NO-OCR-TOKEN](m/image_0.png)\n"
        assert _content_problems(ok, "", "A09") == []  # nosec B101: alt 位置合法。
        leaked = "![DESCR-NO-OCR-TOKEN](m/image_0.png)\n正文 DESCR-NO-OCR-TOKEN\n"
        assert _content_problems(leaked, "", "A09")  # nosec B101: 围栏外正文禁止。

    def test_a10_requires_partial_diagnosis_in_texts(self) -> None:
        text = "HEALTHY-TEXT-REMAINS"
        diagnosed = "部分提取：pptx_undecodable_image.pptx · 部分内容未提取：解码失败"
        assert _content_problems(text, diagnosed, "A10") == []  # nosec B101
        assert _content_problems(text, "一切正常，无诊断", "A10")  # nosec B101: T-16 诊断必须在场。

    def test_a14_belongs_to_display_order(self) -> None:
        good = "![a](m/image_0.png)\nXLSX-FIRST-DRAW-TOKEN\n![b](m/image_1.png)\nXLSX-FIRST-REL-TOKEN\n"
        assert _content_problems(good, "", "A14") == []  # nosec B101
        bad = "XLSX-FIRST-REL-TOKEN\nXLSX-FIRST-DRAW-TOKEN\n"
        assert _content_problems(bad, "", "A14")  # nosec B101: 显示顺序错配必须可检出。


class AcceptanceBoundaryRegressionTests(unittest.TestCase):
    """真实 A 组红证据对应的驱动边界，不运行 GUI 或引擎。"""

    def test_partial_diagnosis_accepts_explicit_errors_before_member(self) -> None:
        check = getattr(markdown_acceptance, "_partial_stage_problems")
        host, member = "pptx_ole_broken_preview.pptx", "ole_body_broken.xlsx"
        diagnostic = (
            f"部分提取：{host} · 部分内容未提取：pptx_embedded_objects："
            f"Failed to extract embedded '{member}': Parsing error: Failed to read ZIP archive: invalid Zip archive"
        )
        stages = r"embedded.*(?:fail|error)|(?:fail|error).*embedded|解析失败"
        self.assertEqual(check(diagnostic, host, member, stages), [])
        self.assertTrue(check(diagnostic.replace(member, "other.xlsx"), host, member, stages))
        self.assertTrue(check(
            f"部分提取：{host} · {member}: OCR 无文字；other.xlsx: embedded extraction failed",
            host, member, stages,
        ))

    def test_a13_format_detection_is_member_specific_failure(self) -> None:
        check = getattr(markdown_acceptance, "_partial_stage_problems")
        diagnostic = (
            "部分提取：docx_emf_wmf_raster.docx · 部分内容未提取：image_ocr："
            "Image 0 OCR failed: OCR error: Parsing error: Could not determine image format；"
            "image_encoder：cannot re-encode format 'unknown': no decoder available"
        )
        stages = r"decode.*(?:fail|error)|Could not determine image format"
        self.assertEqual(check(diagnostic, "docx_emf_wmf_raster.docx", "broken.emf", stages, 0), [])
        self.assertTrue(check(diagnostic, "docx_emf_wmf_raster.docx", "healthy.wmf", stages, 1))
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            markdown = output / "docx_emf_wmf_raster_docx.md"
            markdown.write_text("MIXED-MEDIA\n", encoding="utf-8")
            problems: list[str] = []
            getattr(markdown_acceptance, "_mixed_docx_problems")(
                markdown, output, diagnostic, None, problems
            )
        self.assertEqual(problems, [])

    def test_single_image_ocr_may_precede_reference_without_cross_image_fallback(self) -> None:
        checker = getattr(markdown_acceptance, "_image_binding_problems")
        token_png = getattr(markdown_acceptance, "_token_png")
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            first, second = token_png("FIRST-TOKEN"), token_png("SECOND-TOKEN")
            (output / "first.png").write_bytes(first)
            (output / "second.png").write_bytes(second)
            markdown = output / "sample.md"
            markdown.write_text("FIRST-TOKEN\n\n![](first.png)\n", encoding="utf-8")
            self.assertEqual(checker(markdown, (("FIRST-TOKEN", first),)), [])
            markdown.write_text("FIRST-TOKEN\n\n![](second.png)\n", encoding="utf-8")
            self.assertTrue(checker(markdown, (("FIRST-TOKEN", first),)))
            markdown.write_text(
                "![](second.png)\nFIRST-TOKEN\n![](first.png)\nSECOND-TOKEN\n", encoding="utf-8"
            )
            self.assertTrue(checker(markdown, (("FIRST-TOKEN", first), ("SECOND-TOKEN", second))))

    def test_wmf_source_geometry_is_preserved_at_both_independent_canvas_sizes(self) -> None:
        checker = getattr(markdown_acceptance, "_wmf_content_preserved")
        fixture = markdown_acceptance.FIXTURES_DEFAULT / "matrix/docx_emf_wmf_raster.docx"
        with zipfile.ZipFile(fixture) as archive:
            wmf = archive.read("word/media/healthy.wmf")
        with tempfile.TemporaryDirectory() as temporary:
            candidate = Path(temporary) / "candidate.png"
            for width, height in ((625, 313), (1250, 626)):
                with self.subTest(size=(width, height)):
                    reference = markdown_acceptance.FIXTURES_DEFAULT / f"matrix/wmf_gdi_{width}x{height}.png"
                    with markdown_acceptance.Image.open(reference) as oracle:
                        self.assertEqual(oracle.info["SourceMemberSHA256"], hashlib.sha256(wmf).hexdigest())
                    candidate.write_bytes(reference.read_bytes())
                    self.assertTrue(checker(wmf, {candidate}))

    def test_wmf_missing_gate_register_wrong_image_or_unreferenced_file_is_rejected(self) -> None:
        checker = getattr(markdown_acceptance, "_wmf_content_preserved")
        fixture = markdown_acceptance.FIXTURES_DEFAULT / "matrix/docx_emf_wmf_raster.docx"
        with zipfile.ZipFile(fixture) as archive:
            wmf = archive.read("word/media/healthy.wmf")
        with tempfile.TemporaryDirectory() as temporary:
            candidate = Path(temporary) / "candidate.png"
            for width, height in ((625, 313), (1250, 626)):
                reference = markdown_acceptance.FIXTURES_DEFAULT / f"matrix/wmf_gdi_{width}x{height}.png"
                with markdown_acceptance.Image.open(reference) as original:
                    for label, box in (
                        ("gate", (2200, 0, 2800, 500)),
                        ("register", (2800, 100, 3300, 700)),
                    ):
                        with self.subTest(size=(width, height), missing=label):
                            damaged = original.copy()
                            # 源 WMF placeable bbox (0,0,3771,1142)，独立逻辑区域变异。
                            draw_height = width * 1142 // 3771
                            offset_y = (height - draw_height) // 2
                            left, top, right, bottom = box
                            markdown_acceptance.ImageDraw.Draw(damaged).rectangle(
                                (
                                    left * width // 3771, offset_y + top * draw_height // 1142,
                                    right * width // 3771, offset_y + bottom * draw_height // 1142,
                                ),
                                fill="white",
                            )
                            damaged.save(candidate)
                            self.assertFalse(checker(wmf, {candidate}))
                    markdown_acceptance.Image.new("RGB", original.size, "blue").save(candidate)
                    self.assertFalse(checker(wmf, {candidate}))
                    candidate.write_bytes(reference.read_bytes())
                    self.assertFalse(checker(wmf, set()), "未引用的正确图片不能抵账")

    def test_formats_business_rejection_preserves_only_its_verified_request_diagnosis(self) -> None:
        root = markdown_acceptance.ROOT
        diagnosis = "发现已有 Xberg 进程，拒绝启动第二个共享引擎"
        for stale in (False, True):
            with self.subTest(stale=stale):
                context = markdown_acceptance.Context(
                    None, None, None, markdown_acceptance.FIXTURES_DEFAULT,
                    markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
                )

                def reply(_pipe: object, _runtime: Path, request_id: str, _deadline: float) -> object:
                    return {
                        "id": "unrelated-request" if stale else request_id,
                        "ok": False, "jchtools_broker_protocol": 3,
                        "error_kind": "shared_runtime", "error": diagnosis,
                    }

                with (
                    patch.object(formats, "_formats_request_inputs", return_value=(str(root), root)),
                    patch.object(formats, "_open_formats_pipe"),
                    patch.object(formats, "_exchange_formats", side_effect=reply) as exchange,
                ):
                    rows, error = context.cached_formats()
                self.assertEqual(rows, [])
                if stale:
                    self.assertNotIn(diagnosis, error or "", "其他请求的原因不能归入本轮")
                else:
                    self.assertIn(diagnosis, error or "")
                exchange.assert_called_once()

    def test_broken_formats_exchange_is_not_replayed_or_replaced_by_static_rows(self) -> None:
        root = markdown_acceptance.ROOT
        context = markdown_acceptance.Context(
            None, None, None, markdown_acceptance.FIXTURES_DEFAULT,
            markdown_acceptance.AssetProbe(root, root, root / "xberg.exe", root, []),
        )
        with (
            patch.object(formats, "_formats_request_inputs", return_value=(str(root), root)),
            patch.object(formats, "_open_formats_pipe") as connect,
            patch.object(formats, "_exchange_formats", side_effect=pywintypes.error(
                109, "GetOverlappedResult", "管道已结束"
            )) as exchange,
        ):
            rows, error = context.cached_formats()
            self.assertEqual(context.cached_formats(), (rows, error))
        self.assertEqual(rows, [])
        self.assertIn("109", error or "")
        connect.assert_called_once()
        exchange.assert_called_once()
        connect.return_value.Close.assert_called_once()
