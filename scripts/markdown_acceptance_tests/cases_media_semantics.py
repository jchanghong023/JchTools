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
from scripts import gui_smoke, isolation_probe, make_tmp, markdown_acceptance, media_e2e_fixture, test_gate
from scripts.gui_smoke import find_button as smoke_find_button
from scripts.markdown_acceptance import unique_visible_buttons, verify_common_postconditions
from scripts.gate_summary import acceptance_coverage_gaps

from scripts.markdown_acceptance_impl import runner
from scripts.markdown_acceptance_tests.helpers import (EXPECTED_COVERAGE_GAPS,
EXPECTED_PHYSICAL_ROWS,
AUTO_FAST_PAGES_THRESHOLD,
ARGUMENT_ERROR_EXIT_CODE, COMMAND_FAILURE_EXIT_CODE,
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
_digest,)
class DocumentSemanticPredicateTests(unittest.TestCase):
    """覆盖附录 A11/A12/A13/A19 与 T-14～T-16；真值来自源图，不来自引擎输出。"""

    def check(self, item_id: str, source: Path, output: Path, texts: str = "") -> list[str]:
        checker = getattr(markdown_acceptance, "_" + "document_semantic_problems")
        return checker(item_id, source, output, texts)

    def test_image_binding_resolves_percent_encoded_media_directory(self) -> None:
        # 覆盖 T-14/T-15：URL 转义不能使正确的源图与 OCR 绑定误报不可达。
        checker = getattr(markdown_acceptance, "_" + "image_binding_problems")
        token_png = getattr(markdown_acceptance, "_" + "token_png")
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            media = output / "sample&copy;%_media"
            media.mkdir()
            image = token_png("SOURCE-TOKEN")
            (media / "image_0.png").write_bytes(image)
            markdown = output / "sample.md"
            markdown.write_text("![](sample%26copy;%25_media/image_0.png)\nSOURCE-TOKEN\n", encoding="utf-8")
            assert checker(markdown, (("SOURCE-TOKEN", image),)) == []

    def test_a01_and_a14_detect_swapped_bytes_with_unchanged_text(self) -> None:
        cases = (
            ("A01", "pptx_two_png_order.pptx", "ppt/media/", "BETA-TWO", "ALPHA-ONE"),
            ("A14", "xlsx_drawing_order.xlsx", "xl/media/", "XLSX-FIRST-DRAW-TOKEN", "XLSX-FIRST-REL-TOKEN"),
        )
        token_png = getattr(markdown_acceptance, "_" + "token_png")
        for item_id, filename, member_root, first, second in cases:
            with self.subTest(item=item_id), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                source, output = root / "input", root / "output"
                source.mkdir()
                media = output / "sample_media"
                media.mkdir(parents=True)
                first_bytes, second_bytes = token_png(first), token_png(second)
                with zipfile.ZipFile(source / filename, "w") as archive:
                    archive.writestr(member_root + "image1.png", second_bytes)
                    archive.writestr(member_root + "image2.png", first_bytes)
                (media / "first.png").write_bytes(first_bytes)
                (media / "second.png").write_bytes(second_bytes)
                (output / "sample.md").write_text(
                    f"![](sample_media/first.png)\n{first}\n![](sample_media/second.png)\n{second}\n",
                    encoding="utf-8",
                )
                assert self.check(item_id, source, output) == []
                with markdown_acceptance.Image.open(io.BytesIO(first_bytes)) as image:
                    image.save(media / "first.png", "JPEG", quality=95)
                assert self.check(item_id, source, output) == [], "轻微有损重编码不应按字节差异判错"
                # 无损 BMP 重编码允许，不要求 PNG 字节完全一致。
                with markdown_acceptance.Image.open(io.BytesIO(first_bytes)) as image:
                    image.save(media / "first.png", "BMP")
                assert self.check(item_id, source, output) == []
                (media / "first.png").write_bytes(second_bytes)
                (media / "second.png").write_bytes(first_bytes)
                assert self.check(item_id, source, output), "正文未变、只互换图片字节也必须失败"
                (media / "first.png").write_bytes(b"truncated")
                assert self.check(item_id, source, output), "损坏落盘图片不得通过"

    def test_a11_requires_reachable_ole_and_distinct_body_diagnosis(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "input", root / "output"
            source.mkdir()
            output.mkdir()
            fixture = markdown_acceptance.FIXTURES_DEFAULT / "matrix/pptx_ole_broken_preview.pptx"
            (source / fixture.name).write_bytes(fixture.read_bytes())
            synth = markdown_acceptance.SYNTHESIZERS["document_a11"]
            _ = synth(source, markdown_acceptance.FIXTURES_DEFAULT)
            with zipfile.ZipFile(source / fixture.name) as archive:
                slide = ElementTree.fromstring(archive.read("ppt/slides/slide1.xml"))
                assert any(node.tag.endswith("}oleObj") for node in slide.iter())
                preview = archive.read("ppt/media/preview_ok.png")
            media = output / "sample_media"
            media.mkdir()
            (media / "preview.png").write_bytes(preview)
            (output / "pptx_ole_broken_preview_pptx.md").write_text(
                "![](sample_media/preview.png)\nOLE-PREVIEW-OK\n", encoding="utf-8"
            )
            diagnostic = (
                "部分提取：pptx_ole_broken_preview.pptx · 部分内容未提取："
                "ole_body_broken.xlsx embedded_extract_failed: invalid ZIP"
            )
            assert self.check("A11", source, output, diagnostic) == []
            actual_diagnostic = (
                "部分提取：pptx_ole_broken_preview.pptx · 部分内容未提取：pptx_embedded_objects："
                "Failed to extract embedded 'ole_body_broken.xlsx': "
                "Parsing error: Failed to read ZIP archive: invalid Zip archive: Could not find EOCD"
            )
            self.assertEqual(self.check("A11", source, output, actual_diagnostic), [])
            self.assertTrue(self.check("A11", source, output, actual_diagnostic.replace(
                "ole_body_broken.xlsx", "unrelated.xlsx"
            )))
            assert self.check("A11", source, output, diagnostic.replace("embedded_extract_failed", "OCR 无文字"))
            assert self.check("A11", source, output, "")

    def test_a12_all_five_sources_need_distinct_images_and_ocr(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "input", root / "output"
            source.mkdir()
            output.mkdir()
            fixture = markdown_acceptance.FIXTURES_DEFAULT / "matrix/docx_all_sources.docx"
            (source / fixture.name).write_bytes(fixture.read_bytes())
            _ = markdown_acceptance.SYNTHESIZERS["document_a12"](source, markdown_acceptance.FIXTURES_DEFAULT)
            media = output / "sample_media"
            media.mkdir()
            segments = []
            with zipfile.ZipFile(source / fixture.name) as archive:
                assert "word/footnotes.xml" in archive.namelist()
                assert "word/endnotes.xml" in archive.namelist()
                w = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"
                document = ElementTree.fromstring(archive.read("word/document.xml"))
                for note in ("footnote", "endnote"):
                    assert any(
                        node.tag == f"{w}{note}Reference" and node.get(f"{w}id") == "2" for node in document.iter()
                    )
                for part in ("document", "header1", "footer1", "footnotes", "endnotes"):
                    rels = ElementTree.fromstring(archive.read(f"word/_rels/{part}.xml.rels"))
                    semantic = [rel for rel in rels if rel.get("Id") == "rIdSemantic"]
                    assert len(semantic) == 1
                    assert "word/" + semantic[0].attrib["Target"] in archive.namelist()
                for name in ("BODY", "HEADER", "FOOTER", "FOOTNOTE", "ENDNOTE"):
                    (media / f"{name}.png").write_bytes(archive.read(f"word/media/{name.lower()}.png"))
                    segments.append(f"![](sample_media/{name}.png)\n{name}-IMAGE-TOKEN\n")
            result = output / "docx_all_sources_docx.md"
            result.write_text("\n".join(segments), encoding="utf-8")
            assert self.check("A12", source, output) == []
            result.write_text("\n".join(segments[:-1]), encoding="utf-8")
            assert self.check("A12", source, output), "漏尾注不得通过"
            result.write_text("\n".join(segments).replace("FOOTNOTE-IMAGE-TOKEN", ""), encoding="utf-8")
            assert self.check("A12", source, output), "只有图片或 alt 不能冒充 OCR"

    def test_a13_requires_healthy_content_and_specific_broken_image_stage(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "input", root / "output"
            source.mkdir()
            output.mkdir()
            fixture = markdown_acceptance.FIXTURES_DEFAULT / "matrix/docx_emf_wmf_raster.docx"
            (source / fixture.name).write_bytes(fixture.read_bytes())
            _ = markdown_acceptance.SYNTHESIZERS["document_a13"](source, markdown_acceptance.FIXTURES_DEFAULT)
            media = output / "sample_media"
            media.mkdir()
            with zipfile.ZipFile(source / fixture.name) as archive:
                (media / "healthy.png").write_bytes(archive.read("word/media/healthy.png"))
                (media / "healthy.wmf").write_bytes(archive.read("word/media/healthy.wmf"))
            result = output / "docx_emf_wmf_raster_docx.md"
            healthy = "MIXED-MEDIA\n![](sample_media/healthy.png)\nRASTER-HEALTHY\n![](sample_media/healthy.wmf)\n"
            result.write_text(healthy, encoding="utf-8")
            diagnosed = "部分提取：docx_emf_wmf_raster.docx · 部分内容未提取：broken.emf rasterize_failed: invalid EMF"
            assert self.check("A13", source, output, diagnosed) == []
            assert self.check("A13", source, output, diagnosed.replace("rasterize_failed", "OCR 无文字"))
            assert self.check("A13", source, output, "")
            indexed = diagnosed.replace(
                "broken.emf rasterize_failed", "Image 0 OCR failed: metafile rasterization failed"
            )
            assert self.check("A13", source, output, indexed) == []
            assert self.check("A13", source, output, indexed.replace("Image 0", "Image 1"))
            crossed = indexed.replace("metafile rasterization failed", "no text")
            crossed += "；Image 1 OCR failed: metafile rasterization failed at rasterize_decode"
            assert self.check("A13", source, output, crossed), "别的成员失败阶段不能归给当前图片"
            result.write_text(healthy.replace("![](sample_media/healthy.wmf)\n", ""), encoding="utf-8")
            assert self.check("A13", source, output, diagnosed), "WMF 丢失无诊断不能算部分成功"
            assert (
                self.check("A13", source, output, diagnosed + "；healthy.wmf OCR execution failed: unavailable backend")
                == []
            )
            result.write_text("MIXED-MEDIA", encoding="utf-8")
            assert self.check("A13", source, output, diagnosed), "任意单图失败不能掩盖健康图片丢失"

    def test_a19_each_extension_requires_its_own_ocr_not_native_pdf_text(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "input", root / "output"
            source.mkdir()
            output.mkdir()
            fixture = markdown_acceptance.FIXTURES_DEFAULT / "matrix/jbig2_standalone.jb2"
            (source / fixture.name).write_bytes(fixture.read_bytes())
            _ = markdown_acceptance.SYNTHESIZERS["document_a19"](source, markdown_acceptance.FIXTURES_DEFAULT)
            media = output / "sample_media"
            media.mkdir()
            picture = markdown_acceptance.Image.new("1", (320, 96), 1)
            markdown_acceptance.ImageDraw.Draw(picture).text(
                (12, 24),
                "JBIG2 SYNTH CHECK",
                fill=0,
                font=markdown_acceptance.ImageFont.load_default(size=24),
            )
            picture.save(media / "jbig2.png", "PNG")
            for name in ("jbig2_standalone_jb2.md", "jbig2_standalone_jbig2.md", "pdf_jbig2_pdf.md"):
                (output / name).write_text("![](sample_media/jbig2.png)\nJBIG2 SYNTH CHECK", encoding="utf-8")
            assert self.check("A19", source, output) == []
            (media / "jbig2.png").write_bytes(getattr(markdown_acceptance, "_" + "token_png")("OTHER"))
            assert self.check("A19", source, output), "正确 token 不能掩盖落盘 JBIG2 内容被换掉"
            picture.save(media / "jbig2.png", "PNG")
            (output / "pdf_jbig2_pdf.md").write_text("JBIG2 native text only", encoding="utf-8")
            assert self.check("A19", source, output), "另一输入的正确 OCR 不可覆盖 PDF 失败"


class A18UnsupportedBoundaryTests(unittest.TestCase):
    """S10-06：实际引擎未声明的格式必须经混跑排除，不得只在清单层声明。."""

    def test_mixed_conversion_run_is_executed_and_verified(self) -> None:
        calls: list[str] = []
        summary = "转 Markdown 转换完成：成功 3，部分提取 0，失败 0，已有结果跳过 0，重复结果跳过 0"

        def fake_run(
            _item: object,
            _ctx: object,
            _exe: object,
            *,
            stop_mode: bool = False,
            tag_suffix: str = "run",
            capture: list[markdown_acceptance.GuiRun] | None = None,
        ) -> markdown_acceptance.Outcome:
            del stop_mode
            calls.append(tag_suffix)
            if capture is not None:
                capture.append(markdown_acceptance.GuiRun([], summary, None))
            return markdown_acceptance.Outcome("PASS")

        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "A18")
        with tempfile.TemporaryDirectory() as temporary:
            context = markdown_acceptance.Context(None, None, None, Path(temporary), markdown_acceptance.probe_assets())
            _ = (Path(temporary) / "a18-mixed" / "output").mkdir(parents=True)
            with (
                patch.object(runner, "SCRATCH_ROOT", Path(temporary)),
                patch("scripts.markdown_acceptance_impl.runner._run_conversion_item", side_effect=fake_run),
                patch.object(
                    runner, "_runtime_format_extensions", return_value=({"jp2", "j2k", "j2c", "png"}, None)
                ),
                patch.object(runner, "_fixture_precondition", return_value=None),
            ):
                outcome = _run_handler("run_matrix_a18", item, context)
        assert outcome.status == "PASS", outcome.reason  # nosec B101: 回归测试断言。
        assert "supported" in calls  # nosec B101: 支持格式先行转换。
        assert "mixed" in calls  # nosec B101: 必须追加混跑转换。

    def test_unsupported_products_or_missing_summary_are_rejected(self) -> None:
        verify = cast(
            "Callable[[Path, str, set[str]], str | None]",
            getattr(markdown_acceptance, "_" + "verify_a18_unsupported_rejected"),
        )
        supported = {"jp2", "j2k", "j2c"}
        with tempfile.TemporaryDirectory() as temporary:
            outputs = Path(temporary)
            summary = "转 Markdown 转换完成：成功 3，部分提取 0，失败 0，已有结果跳过 0，重复结果跳过 0"
            assert verify(outputs, summary, supported) is None  # nosec B101: 统计只计实际支持格式。
            _ = (outputs / "jpx_jpx.md").write_text("leak", encoding="utf-8")
            assert verify(outputs, summary, supported) is not None  # nosec B101: 未声明格式产出即失败。
            _ = (outputs / "jpx_jpx.md").unlink()
            assert verify(outputs, "没有任何统计文本", supported) is not None  # nosec B101: 缺批次统计不可核对。
            counted = "转 Markdown 转换完成：成功 6，部分提取 0，失败 0，已有结果跳过 0，重复结果跳过 0"
            assert verify(outputs, counted, supported) is not None  # nosec B101: 未声明格式计入批次即失败。


class A24SceneNoteTests(unittest.TestCase):
    """S10-07：tone/silence/noaudio 说明文本与停止相「后续未处理」断言。."""

    @staticmethod
    def _write_scene_outputs(root: Path, *, noaudio_body: str) -> None:
        outputs = root / "a24-full" / "output"
        _ = outputs.mkdir(parents=True)
        for suffix in ("mp4", "m4a"):
            _ = (outputs / f"video-to-notes-intro-zh_{suffix}.md").write_text(
                "# video-to-notes-intro-zh\n- 音频时长: 00:00:02.000\n- 语音片段: 1\n## 转录\n"
                "[00:00:00.000 --> 00:00:01.000] 中文\n",
                encoding="utf-8",
            )
        _ = (outputs / "tone_m4a.md").write_text(
            "# tone\n- 音频时长: 00:00:02.000\n- 语音片段: 1\n## 转录\n[00:00:00.000 --> 00:00:01.000] 合成片段\n",
            encoding="utf-8",
        )
        _ = (outputs / "silence_m4a.md").write_text(
            "# silence\n- 音频时长: 00:00:02.000\n- 语音片段: 0\n## 转录\n（未检测到语音）\n", encoding="utf-8"
        )
        _ = (outputs / "noaudio_mp4.md").write_text(noaudio_body, encoding="utf-8")

    @staticmethod
    def _verify_a24_outputs() -> str | None:
        checker = cast("Callable[[], str | None]", getattr(markdown_acceptance, "_" + "verify_a24_outputs"))
        return checker()

    def test_scene_notes_are_asserted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            self._write_scene_outputs(
                Path(temporary),
                noaudio_body="# noaudio\n- 音频时长: 无音频轨道\n- 语音片段: 0\n## 转录\n（无音频轨道）\n",
            )
            with patch.object(runner, "SCRATCH_ROOT", Path(temporary)):
                assert self._verify_a24_outputs() is None  # nosec B101: 三场景说明齐全。
        with tempfile.TemporaryDirectory() as temporary:
            # 无音轨场景缺少「无音频轨道」说明：T-20 要求不能产生无解释的空文件。
            self._write_scene_outputs(Path(temporary), noaudio_body="# noaudio\n（空白）\n")
            with patch.object(runner, "SCRATCH_ROOT", Path(temporary)):
                assert self._verify_a24_outputs() is not None  # nosec B101

    def test_stop_phase_requires_unprocessed_followups(self) -> None:
        verify_stop = cast(
            "Callable[[int, int, str], str | None]",
            getattr(markdown_acceptance, "_" + "verify_a24_stop"),
        )
        assert verify_stop(3, 4, "已停止") is None  # nosec B101: 后续文件未处理。
        assert verify_stop(4, 4, "已停止") is not None  # nosec B101: 全部完成即停止语义未验证。
        assert verify_stop(0, 4, "") is not None  # nosec B101: 未观察到已停止状态。


class A15AutoModeTests(unittest.TestCase):
    """S10-08：>500 页夹具在场，A15 断言 auto_mode 披露（T-18 修订口径）。."""

    def test_over_500_pages_fixture_and_assertions_are_registered(self) -> None:
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "A15")
        assert "large_501_pages.pdf" in item.fixtures  # nosec B101: 回归测试断言。
        assert "auto_mode" in _content_spec("A15").require_in_texts  # nosec B101: 回归测试断言。
        assert "large_501_pages" in _content_spec("A15").require_in_texts  # nosec B101: 回归测试断言。
        fixture = markdown_acceptance.FIXTURES_DEFAULT / "large_501_pages.pdf"
        assert fixture.is_file()  # nosec B101: 回归测试断言。
        data = fixture.read_bytes()
        pages = data.count(b"/Type /Page") - data.count(b"/Type /Pages")
        assert pages > AUTO_FAST_PAGES_THRESHOLD  # nosec B101: 必须超过引擎 auto_fast_pages。


class A15WordingTests(unittest.TestCase):
    """S10-08：210 页场景保留为常规模式回归，旧 200 页口径措辞清理。."""

    def test_a15_keeps_210_pages_as_normal_mode_regression(self) -> None:
        item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "A15")
        assert "large_210_pages.pdf" in item.fixtures  # nosec B101: 回归测试断言。
        assert "200 页" not in item.fixture_note  # nosec B101: 旧口径措辞清理。
        assert ">200 页" not in item.fixture_note  # nosec B101: 旧口径措辞清理。
