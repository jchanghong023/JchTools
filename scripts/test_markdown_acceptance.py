"""A26/T-14 回归单元测试（只使用 tempfile，不写入仓库临时目录）。"""

from __future__ import annotations

import hashlib
import tempfile
import unittest
from pathlib import Path

from scripts.markdown_acceptance import verify_common_postconditions
from scripts.test_gate import _acceptance_coverage_gaps


def _digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class MediaPostconditionTests(unittest.TestCase):
    """验证 T-14 图片目录与 A26 横切断言的边界。"""

    def test_legal_media_directory_and_reference_pass(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            source.mkdir()
            output.mkdir()
            original = source / "sample.docx"
            original.write_bytes(b"source")
            media = output / "文档_docx_media" / "图片 0.png"
            media.parent.mkdir()
            media.write_bytes(b"png-bytes")
            (output / "sample_docx.md").write_text("![图片](<文档_docx_media/图片 0.png>)\n", encoding="utf-8")

            problems = verify_common_postconditions(
                source,
                output,
                {Path("sample.docx"): _digest(original)},
            )

            self.assertEqual(problems, [])

    def test_escape_reference_and_literal_code_are_handled(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            source.mkdir()
            output.mkdir()
            original = source / "sample.docx"
            original.write_bytes(b"source")
            (output / "sample_docx.md").write_text(
                "![escape](../外部_media/逃逸.png)\n"
                "![unc](//server/share/外部_media/UNC.png)\n"
                "`![literal](文档_media/字面.png)`\n"
                "```markdown\n![fenced](文档_media/围栏.png)\n```\n",
                encoding="utf-8",
            )

            problems = verify_common_postconditions(
                source,
                output,
                {Path("sample.docx"): _digest(original)},
            )

            self.assertTrue(any("图片引用逃出输出目录" in problem for problem in problems))
            self.assertTrue(any("UNC.png" in problem for problem in problems))
            self.assertFalse(any("字面.png" in problem for problem in problems))
            self.assertFalse(any("围栏.png" in problem for problem in problems))

    def test_missing_reference_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            source.mkdir()
            output.mkdir()
            original = source / "sample.docx"
            original.write_bytes(b"source")
            (output / "sample_docx.md").write_text("![image](sample_docx_media/missing.png)\n", encoding="utf-8")

            problems = verify_common_postconditions(
                source,
                output,
                {Path("sample.docx"): _digest(original)},
            )

            self.assertTrue(any("图片引用目标不存在" in problem for problem in problems))

    def test_orphan_media_temp_file_and_source_change_are_reported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            source.mkdir()
            output.mkdir()
            original = source / "sample.docx"
            original.write_bytes(b"source")
            media = output / "sample_docx_media" / "orphan.png"
            media.parent.mkdir()
            media.write_bytes(b"png-bytes")
            (output / ".jch-markdown-leftover.tmp").write_bytes(b"partial")
            original.write_bytes(b"changed")

            problems = verify_common_postconditions(
                source,
                output,
                {Path("sample.docx"): hashlib.sha256(b"source").hexdigest()},
            )

            self.assertTrue(any("图片没有正文引用" in problem for problem in problems))
            self.assertTrue(any("输出残留临时文件" in problem for problem in problems))
            self.assertTrue(any("源文件被改动" in problem for problem in problems))

    def test_acceptance_partial_and_snap_not_run_are_coverage_gaps(self) -> None:
        text = "\n".join(
            (
                "PASS  package（未执行）",
                "PARTIAL  markdown-acceptance（已执行项通过，仍有 NOT RUN）",
                "NOT RUN  snap-ocr-worker-root（缺少隔离资产）",
            )
        )

        gaps = _acceptance_coverage_gaps(text)

        self.assertEqual(len(gaps), 2)
        self.assertTrue(any("markdown-acceptance" in gap for gap in gaps))
        self.assertTrue(any("snap-ocr-worker-root" in gap for gap in gaps))

    def test_acceptance_package_not_run_is_not_a_fulltest_gap(self) -> None:
        text = "\n".join(
            (
                "PASS  markdown-acceptance",
                "PASS  snap-ocr-worker-root",
                "NOT RUN  package（加 -WithPackage）",
            )
        )

        self.assertEqual(_acceptance_coverage_gaps(text), [])


if __name__ == "__main__":
    unittest.main()
