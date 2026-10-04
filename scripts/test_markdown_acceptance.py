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

from pywinauto import timings

if TYPE_CHECKING:
    from collections.abc import Callable

    from pywinauto.application import WindowSpecification

from scripts import gui_smoke, markdown_acceptance
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


class _UnreadableConfirmCheckbox:
    def wait(self, _condition: str, *, timeout: int) -> None:
        del timeout

    def is_visible(self) -> bool:
        message = "UIA 暂时不可读"
        raise timings.TimeoutError(message)


class _UnreadableConfirmWindow:
    def child_window(self, **_kwargs: object) -> _UnreadableConfirmCheckbox:
        return _UnreadableConfirmCheckbox()

    def descendants(self, *, control_type: str) -> list[object]:
        del control_type
        message = "UIA 暂时不可读"
        raise timings.TimeoutError(message)


def _digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class MediaPostconditionTests(unittest.TestCase):
    """验证 T-14 图片目录与 A26 横切断言的边界。."""

    def test_svg_rasterization_does_not_require_svg_suffix_in_markdown(self) -> None:
        # 覆盖 T-13/T-14：SVG 可由引擎栅格化为 PNG，必须验证真实媒体而非源扩展名字面量。
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary)
            output = scratch / "a07-run" / "output"
            media = output / "sample_media"
            media.mkdir(parents=True)
            token_png = cast("Callable[[str], bytes]", getattr(markdown_acceptance, "_" + "token_png"))
            _ = (media / "image_0.png").write_bytes(token_png("SVG-LOCAL"))
            _ = (media / "image_1.png").write_bytes(token_png("PNG-NEIGHBOR"))
            _ = (output / "sample.md").write_text(
                "![](sample_media/image_0.png)\n![](sample_media/image_1.png)\nPNG-NEIGHBOR\n",
                encoding="utf-8",
            )
            item = next(item for item in markdown_acceptance.ITEMS if item.item_id == "A07")
            run_matrix = cast(
                "Callable[[markdown_acceptance.Item, markdown_acceptance.Context], markdown_acceptance.Outcome]",
                getattr(markdown_acceptance, "_" + "run_matrix"),
            )
            with (
                patch.object(markdown_acceptance, "SCRATCH_ROOT", scratch),
                patch(
                    "scripts.markdown_acceptance._run_conversion_item", return_value=markdown_acceptance.Outcome("PASS")
                ),
            ):
                outcome = run_matrix(item, cast("markdown_acceptance.Context", object()))
            assert outcome.status == "PASS", outcome.reason  # nosec B101: 回归测试断言。

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
            patch("scripts.gui_smoke.subprocess.Popen"),
            patch.object(gui_smoke, "_OwnedProcessTree"),
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
