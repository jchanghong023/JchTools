"""A26/T-14 回归单元测试（只使用 tempfile，不写入仓库临时目录）。."""

from __future__ import annotations

import hashlib
import os
import sqlite3
import tempfile
import unittest
from pathlib import Path
from typing import TYPE_CHECKING, cast
from unittest.mock import patch

if TYPE_CHECKING:
    from collections.abc import Callable

    from pywinauto.application import WindowSpecification

from scripts import markdown_acceptance
from scripts.gui_smoke import find_button as smoke_find_button
from scripts.markdown_acceptance import unique_visible_buttons, verify_common_postconditions
from scripts.test_gate import acceptance_coverage_gaps

EXPECTED_COVERAGE_GAPS = 2
EXPECTED_PHYSICAL_ROWS = 2


class _FakeRect:
    def __init__(self, left: int, top: int, right: int, bottom: int) -> None:
        self.left: int = left
        self.top: int = top
        self.right: int = right
        self.bottom: int = bottom


class _FakeButton:
    def __init__(self, rect: _FakeRect, title: str) -> None:
        self._rect: _FakeRect = rect
        self._title: str = title

    def rectangle(self) -> _FakeRect:
        return self._rect

    def window_text(self) -> str:
        return self._title

    def is_visible(self) -> bool:
        return True


class _FakeWindow:
    def __init__(self, buttons: list[_FakeButton]) -> None:
        self._buttons: list[_FakeButton] = buttons

    def descendants(self, *, control_type: str) -> list[_FakeButton]:
        assert control_type == "Button"  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
        return self._buttons


class _FakeSmokeWindow:
    def descendants(self, *, control_type: str) -> list[object]:
        assert control_type == "Button"  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
        return []

    def child_window(self, **_kwargs: object) -> object:
        return self


def _digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class MediaPostconditionTests(unittest.TestCase):
    """验证 T-14 图片目录与 A26 横切断言的边界。."""

    def test_legal_media_directory_and_reference_pass(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "input"
            output = root / "output"
            _ = source.mkdir()
            _ = output.mkdir()
            original = source / "sample.docx"
            _ = original.write_bytes(b"source")
            media = output / "文档_docx_media" / "图片 0.png"
            _ = media.parent.mkdir()
            _ = media.write_bytes(b"png-bytes")
            _ = (output / "sample_docx.md").write_text("![图片](<文档_docx_media/图片 0.png>)\n", encoding="utf-8")

            problems = verify_common_postconditions(
                source,
                output,
                {Path("sample.docx"): _digest(original)},
            )

            assert problems == []  # nosec B101: 回归测试断言，不用于产品权限或输入校验。

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


if __name__ == "__main__":
    _ = unittest.main()
