"""A26/T-14 回归单元测试（只使用 tempfile，不写入仓库临时目录）。."""

from __future__ import annotations

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

from scripts import gui_smoke, markdown_acceptance, test_gate, test_timing
from scripts.gui_smoke import find_button as smoke_find_button
from scripts.markdown_acceptance import unique_visible_buttons, verify_common_postconditions
from scripts.test_gate import acceptance_coverage_gaps

EXPECTED_COVERAGE_GAPS = 2
EXPECTED_PHYSICAL_ROWS = 2
AUTO_FAST_PAGES_THRESHOLD = 500
ARGUMENT_ERROR_EXIT_CODE = 3
COMMAND_TIMEOUT_SECONDS = 8.0
COMMAND_FAILURE_EXIT_CODE = 7
UNVERIFIED_EXIT_CODE = 2


def report_acceptance(results: list[tuple[markdown_acceptance.Item, markdown_acceptance.Outcome]]) -> int:
    reporter = cast(
        "Callable[[list[tuple[markdown_acceptance.Item, markdown_acceptance.Outcome]], Path | None], int]",
        getattr(markdown_acceptance, "_" + "print_report"),
    )
    return reporter(results, None)


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
            header = "# video-to-notes-intro-zh\n- 音频时长: 00:01:19.125\n## 转录\n"
            _ = result.write_text(header, encoding="utf-8")
            assert verifier(output, ("mp4",)) is not None  # nosec B101: 验收判据回归断言。
            _ = result.write_text(header + "[00:00:00.108 --> 00:00:02.508] 长视频想快速获取要点。\n", encoding="utf-8")
            assert verifier(output, ("mp4",)) is None  # nosec B101: 验收判据回归断言。

    def test_old_office_outputs_cannot_exchange_document_markers(self) -> None:
        item = next(item for item in markdown_acceptance.profile_items("common") if item.item_id == "A25")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "a25-common-office" / "output"
            output.mkdir(parents=True)
            for extension in ("doc", "xls", "ppt"):
                _ = (output / f"legacy_{extension}.md").write_text("JCHTOOLS-LEGACY-DOC", encoding="utf-8")
            with (
                patch("scripts.markdown_acceptance.SCRATCH_ROOT", root),
                patch(
                    "scripts.markdown_acceptance._run_conversion_item",
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


def _content_spec(item_id: str) -> markdown_acceptance.ContentSpec:
    """经 getattr 取私有断言表项（沿用本文件的私有成员访问惯例，避开 SLF001）。."""
    table = cast(
        "dict[str, markdown_acceptance.ContentSpec]",
        getattr(markdown_acceptance, "_" + "CONTENT_ASSERTS"),
    )
    return table[item_id]


def _content_problems(text: str, gui_texts: str, item_id: str) -> list[str]:
    checker = cast(
        "Callable[[str, str, markdown_acceptance.ContentSpec], list[str]]",
        getattr(markdown_acceptance, "_" + "content_problems"),
    )
    return checker(text, gui_texts, _content_spec(item_id))


def _per_input_problems(item_id: str, files: list[str], produced: list[str], texts: str) -> list[str]:
    checker = cast(
        "Callable[[str, list[str], list[str], str], list[str]]",
        getattr(markdown_acceptance, "_" + "per_input_problems"),
    )
    return checker(item_id, files, produced, texts)


def _input_rule(item_id: str, filename: str) -> markdown_acceptance.InputRule:
    checker = cast(
        "Callable[[str, str], markdown_acceptance.InputRule]",
        getattr(markdown_acceptance, "_" + "input_rule"),
    )
    return checker(item_id, filename)


def _a25_rules() -> dict[str, markdown_acceptance.InputRule]:
    return cast(
        "dict[str, markdown_acceptance.InputRule]",
        getattr(markdown_acceptance, "_" + "A25_RULES"),
    )


def _run_handler(name: str, item: object, ctx: object) -> markdown_acceptance.Outcome:
    runner = cast(
        "Callable[[object, object], markdown_acceptance.Outcome]",
        getattr(markdown_acceptance, "_" + name),
    )
    return runner(item, ctx)


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
            proc: subprocess.Popen[bytes] = subprocess.Popen([sys.executable, "-c", parent_source, temporary])
            unrelated: subprocess.Popen[bytes] = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(900)"]
            )
            owner = None
            try:
                owner = gui_smoke.own_process_tree(proc)
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
                    if win32event.WaitForSingleObject(child, 0) == win32event.WAIT_TIMEOUT:
                        win32api.TerminateProcess(child, 0)
                    win32api.CloseHandle(child)
            finally:
                if owner is not None:
                    owner.close()
                if proc.poll() is None:
                    proc.kill()
                _ = proc.wait(timeout=10)
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
            patch.object(markdown_acceptance, "_wait_process_isolation", return_value=True),
            patch("scripts.markdown_acceptance.subprocess.Popen"),
            patch("scripts.markdown_acceptance.own_process_tree", create=True),
            patch.object(markdown_acceptance, "_connect_window", return_value=(None, object())),
            patch.object(markdown_acceptance, "_click_button"),
            patch.object(markdown_acceptance, "_convert_directory_rows", return_value=[object(), object()]),
            patch.object(markdown_acceptance, "_set_row_edit"),
            patch.object(markdown_acceptance, "_wait_start_ready", return_value=True),
            patch.object(markdown_acceptance, "_button_state", side_effect=available_button),
            patch.object(markdown_acceptance, "_window_texts", side_effect=["尚未开始", completed, completed]),
            patch.object(markdown_acceptance, "_request_close"),
            patch.object(markdown_acceptance, "_terminate"),
            patch.object(markdown_acceptance, "CONVERSION_TIMEOUT", 1),
            patch("scripts.markdown_acceptance.time.time", side_effect=[0.0, 0.0, 0.0, 0.0, 2.0]),
            patch("scripts.markdown_acceptance.time.sleep"),
        ):
            root = Path(temporary)
            run = markdown_acceptance.drive_conversion(root / "gui.exe", root, root)
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

    def test_markdown_acceptance_isolates_snap_assets(self) -> None:
        # 覆盖 XB-14：转换 GUI 初始化不能唤起生产截图服务，抢占用户会话引擎。
        acceptance = Path(__file__).with_name("acceptance.ps1").read_text(encoding="utf-8")
        assert "Set-Item -Path Env:JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT -Value $mdSnapAssetRoot" in acceptance  # nosec B101: 回归测试断言，不用于产品权限或输入校验。


class AcquireHintTests(unittest.TestCase):
    """S10-01：资产获取提示必须区分测试引擎与产品钉死 tag，并指向设置页。."""

    def test_hint_guides_settings_page_and_test_engine_paths(self) -> None:
        probe = markdown_acceptance.AssetProbe(None, None, None, None, ["未提供隔离验收资产根"])
        hint = probe.acquire_hint()
        # 测试引擎口径优先：固定测试目录 + 环境变量入口。
        assert "JCHTOOLS_TEST_XBERG_DIR" in hint  # nosec B101: 回归测试断言，不用于产品权限或输入校验。
        assert str(markdown_acceptance.LOCAL_TEST_XBERG_DIR) in hint  # nosec B101: 回归测试断言。
        # 配置入口在设置页（XB-20），不得再引导到「转 Markdown」页选择运行目录。
        assert "「设置」页" in hint  # nosec B101: 回归测试断言。
        assert "「转 Markdown」页选择并点" not in hint  # nosec B101: 回归测试断言。
        # 产品钉死 tag 只读自清单，两个口径不得混写。
        manifest = cast(
            "dict[str, object]",
            json.loads(markdown_acceptance.ASSET_MANIFEST.read_text(encoding="utf-8")),
        )
        section = cast("dict[str, object]", manifest["xberg"])
        pinned = section["tag"]
        assert isinstance(pinned, str)  # nosec B101: 回归测试断言。
        assert pinned in hint  # nosec B101: 回归测试断言。
        assert "resources/markdown-assets.json" in hint  # nosec B101: 回归测试断言。


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
        with patch.object(markdown_acceptance, "_shared_format_rows", return_value=(rows, None)):
            extensions, error = markdown_acceptance.xberg_format_extensions(context)
        assert error is None  # nosec B101: 格式清单解析回归断言。
        assert extensions == sorted(uncovered)  # nosec B101: 漏覆盖格式不能被硬编码名单排除。

    def test_covered_formats_follow_registered_matrix_inputs(self) -> None:
        declared = ("pdf", "docx", "pptx", "xlsx", "pbm")
        rows: list[object] = [{"extension": extension, "mime_type": "application/test"} for extension in declared]
        context = markdown_acceptance.Context(None, None, None, Path(), markdown_acceptance.probe_assets())
        with (
            patch.object(markdown_acceptance, "ITEMS", ()),
            patch.object(markdown_acceptance, "_shared_format_rows", return_value=(rows, None)),
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
                    markdown_acceptance, "_run_conversion_item", return_value=markdown_acceptance.Outcome("PASS")
                ) as convert,
            ):
                outcome = _run_handler("run_matrix_a25", item, context)
        assert outcome.status == "NOT RUN"  # nosec B101: 缺健康样本不得执行或报告通过。
        assert "doc" in outcome.reason  # nosec B101: 报告必须列出缺失扩展名。
        convert.assert_not_called()


class PerInputRuleTests(unittest.TestCase):
    """S10-02/04：逐输入断言——整族失败、缺诊断、夹具不足都不得假 PASS。."""

    def test_a25_dynamic_rules_tighten_engine_declared_formats(self) -> None:
        # S10-02 复审：引擎声明的格式必须有产物（防引擎回归假 PASS）；未声明
        # 格式保留宽松兜底；动态规则缺席时回落静态默认（全宽松）。
        rules = _a25_rules()
        old_rules = dict(rules)
        try:
            rules["sample_odt.odt"] = markdown_acceptance.InputRule(must_produce=True)
            rules["sample_legacy.wpd"] = markdown_acceptance.InputRule(must_produce=False)
            assert _input_rule("A25", "sample_odt.odt").must_produce  # nosec B101: 回归测试断言。
            assert not _input_rule("A25", "sample_legacy.wpd").must_produce  # nosec B101: 回归测试断言。
        finally:
            rules.clear()
            rules.update(old_rules)
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
                patch.object(markdown_acceptance, "_resolve_gui", return_value=(Path("JchTools.exe"), None)),
                patch.object(markdown_acceptance, "_asset_precondition", return_value=None),
                patch.object(
                    markdown_acceptance,
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
                patch.object(markdown_acceptance, "SCRATCH_ROOT", Path(temporary)),
                patch("scripts.markdown_acceptance._run_conversion_item", side_effect=fake_run),
                patch.object(
                    markdown_acceptance, "_runtime_format_extensions", return_value=({"jp2", "j2k", "j2c", "png"}, None)
                ),
                patch.object(markdown_acceptance, "_fixture_precondition", return_value=None),
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
                patch.dict(_a25_rules(), clear=True),
                patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                patch.object(markdown_acceptance, "_resolve_gui", return_value=(context.gui_exe, None)),
                patch.object(markdown_acceptance, "_asset_precondition", return_value=None),
                patch.object(markdown_acceptance, "_shared_format_rows", return_value=(rows, error)),
                patch.object(markdown_acceptance, "drive_conversion", side_effect=convert),
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
            return {"id": request_id, "ok": True, "jchtools_broker_protocol": 2, "formats": expected}

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
            patch.object(markdown_acceptance, "_resolve_gui", return_value=(root / "JchTools.exe", None)),
            patch.object(markdown_acceptance, "_wait_process_isolation", return_value=True),
            patch.object(markdown_acceptance, "own_process_tree"),
            patch("scripts.markdown_acceptance.subprocess.Popen", return_value=process) as start,
            patch.object(markdown_acceptance, "_exchange_formats", side_effect=exchange) as request,
        ):
            rows, error = context.cached_formats()
        assert error is None, error  # nosec B101: 关闭的旧端点必须在请求前被识别。
        assert rows == expected  # nosec B101: 必须消费本轮有效代理的协议响应。
        start.assert_called_once()
        request.assert_called_once()
        close_old.assert_called_once()


class SharedFormatProtocolTests(unittest.TestCase):
    """失效/异版本 broker 响应必须使真实 A18 消费者 NOT RUN，不能使用旧固定清单。."""

    @staticmethod
    def _protocol_outcome(fields: dict[str, object]) -> markdown_acceptance.Outcome:
        def reply(_pipe: object, _runtime: Path, request_id: str, _deadline: float) -> object:
            response: dict[str, object] = {
                "id": request_id,
                "ok": True,
                "jchtools_broker_protocol": 2,
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
                patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                patch.object(markdown_acceptance, "_open_formats_pipe"),
                patch.object(markdown_acceptance, "_exchange_formats", side_effect=reply),
                patch.object(markdown_acceptance, "_asset_precondition", return_value=None),
                patch.object(
                    markdown_acceptance,
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


class A24SceneNoteTests(unittest.TestCase):
    """S10-07：tone/silence/noaudio 说明文本与停止相「后续未处理」断言。."""

    @staticmethod
    def _write_scene_outputs(root: Path, *, noaudio_body: str) -> None:
        outputs = root / "a24-full" / "output"
        _ = outputs.mkdir(parents=True)
        for suffix in ("mp4", "m4a"):
            _ = (outputs / f"video-to-notes-intro-zh_{suffix}.md").write_text(
                "# video-to-notes-intro-zh\n- 音频时长: 00:00:02.000\n## 转录\n[00:00:00.000 -> 00:00:01.000] 中文\n",
                encoding="utf-8",
            )
        _ = (outputs / "tone_m4a.md").write_text("# tone\n- 音频时长: 00:00:02.000\n## 转录\n片段\n", encoding="utf-8")
        _ = (outputs / "silence_m4a.md").write_text(
            "# silence\n- 音频时长: 00:00:02.000\n（未检测到语音）\n", encoding="utf-8"
        )
        _ = (outputs / "noaudio_mp4.md").write_text(noaudio_body, encoding="utf-8")

    @staticmethod
    def _verify_a24_outputs() -> str | None:
        checker = cast("Callable[[], str | None]", getattr(markdown_acceptance, "_" + "verify_a24_outputs"))
        return checker()

    def test_scene_notes_are_asserted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            self._write_scene_outputs(Path(temporary), noaudio_body="# noaudio\n- 音频时长: 无音频轨道\n")
            with patch.object(markdown_acceptance, "SCRATCH_ROOT", Path(temporary)):
                assert self._verify_a24_outputs() is None  # nosec B101: 三场景说明齐全。
        with tempfile.TemporaryDirectory() as temporary:
            # 无音轨场景缺少「无音频轨道」说明：T-20 要求不能产生无解释的空文件。
            self._write_scene_outputs(Path(temporary), noaudio_body="# noaudio\n（空白）\n")
            with patch.object(markdown_acceptance, "SCRATCH_ROOT", Path(temporary)):
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
            patch.object(markdown_acceptance, "scan_python_modules", side_effect=fake_scan),
            patch.object(markdown_acceptance, "drive_conversion", side_effect=fake_drive),
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
                        patch.object(markdown_acceptance, "_prepare_scratch", return_value=prepared),
                        patch.object(
                            markdown_acceptance,
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

        def delegate(_item_id: str, _target: Path, environment: dict[str, str]) -> None:
            environments.append(environment)

        with (
            tempfile.TemporaryDirectory() as temporary,
            patch.object(markdown_acceptance, "SCRATCH_ROOT", Path(temporary)),
            patch.object(markdown_acceptance, "_resolve_gui", return_value=(context.gui_exe, None)),
            patch.object(markdown_acceptance, "_c03_delegate_stages", side_effect=delegate),
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

        def delegate(_item_id: str, _target: Path, environment: dict[str, str]) -> None:
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
            patch.object(markdown_acceptance, "SCRATCH_ROOT", Path(temporary)),
            patch.object(markdown_acceptance, "_resolve_gui", return_value=(context.gui_exe, None)),
            patch.object(markdown_acceptance, "_c03_delegate_stages", side_effect=delegate),
        ):
            return _run_handler("run_c03_unconfigured", item, context)

    def test_normal_state_artifacts_do_not_count_as_conversion_downloads(self) -> None:
        outcome = self._run_with_artifacts(
            (
                "config.sqlite3",
                "config.sqlite3-wal",
                "hash-cache.sqlite3",
                "organizer.lock",
                "logs/jchtools.log.2026-10-07",
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
                    "scripts.markdown_acceptance.subprocess.Popen", side_effect=AssertionError("不得启动 GUI")
                ) as launch,
            ):
                run = markdown_acceptance.drive_conversion(Path("gui.exe"), Path("input"), Path("output"))
            launch.assert_not_called()
            assert run.error is not None  # nosec B101: 隔离前置缺失必须明确拒绝。

    def test_conversion_isolation_requires_fresh_matching_gui_pid(self) -> None:
        # 覆盖 XB-18/XB-22：预播种数据库、旧启动记录及其他 PID 不能证明本轮隔离。
        checker = cast(
            "Callable[[Path, int, str, dict[Path, int]], bool]",
            getattr(markdown_acceptance, "_" + "process_isolation_ready"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            database = state / "config.sqlite3"
            _ = database.write_bytes(b"seeded database")
            logs = state / "logs"
            _ = logs.mkdir()
            log = logs / "jchtools.log.2026-10-08"
            good = '诊断日志已初始化（P-10） role="gui" pid=123 log_dir="isolated"\n'
            _ = log.write_text(good, encoding="utf-8")
            offsets = {log: log.stat().st_size}
            with patch.object(markdown_acceptance, "TMP_ROOT", state):
                assert not checker(state, 123, "gui", offsets)  # nosec B101: 旧记录不能证明当前进程。
                with log.open("a", encoding="utf-8") as stream:
                    _ = stream.write(good.replace("pid=123", "pid=1234"))
                assert not checker(state, 123, "gui", offsets)  # nosec B101: PID 必须精确匹配。
                with log.open("a", encoding="utf-8") as stream:
                    _ = stream.write(good.replace('role="gui"', 'role="xberg-broker"'))
                assert not checker(state, 123, "gui", offsets)  # nosec B101: 代理进程不证明 GUI 的状态隔离。
                with log.open("a", encoding="utf-8") as stream:
                    _ = stream.write(good)
                assert checker(state, 123, "gui", offsets)  # nosec B101: 本轮同 PID 的 GUI 启动记录有效。
                database.unlink()
                assert not checker(state, 123, "gui", offsets)  # nosec B101: 日志不能替代隔离 SQLite。

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
            manifest = Path(temporary) / "markdown-assets.json"
            _ = manifest.write_text("{", encoding="utf-8")
            item = next(entry for entry in markdown_acceptance.ITEMS if entry.item_id == "C01")
            context = markdown_acceptance.Context(
                None,
                root,
                None,
                root,
                markdown_acceptance.AssetProbe(None, None, None, None, []),
            )
            with patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest):
                outcome = markdown_acceptance.run_item(item, context)
        assert outcome.status == "FAIL"  # nosec B101: 清单不可验证时不得报告交付扫描通过。
        assert "无法读取或解析" in outcome.reason  # nosec B101: 必须保留清单错误原因。

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
                        "--json-report",
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

    def test_format_sweep_jsonl_is_synthetic(self) -> None:
        path = markdown_acceptance.FIXTURES_DEFAULT / "matrix" / "format_sweep" / "sample.jsonl"
        text = path.read_text(encoding="utf-8")
        assert "docs/" not in text  # nosec B101: 无外部语料路径。
        assert ".pdf" not in text  # nosec B101: 无外部语料路径。
        lines = [line for line in text.splitlines() if line.strip()]
        assert lines  # nosec B101: 回归测试断言。
        for line in lines:
            _ = cast("dict[str, object]", json.loads(line))  # 每行必须是合法 JSON 对象。

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


class ManifestDrivenScanTests(unittest.TestCase):
    """S10-14：禁止资产扫描按清单成员驱动，文档类成员不得误报。."""

    def test_manifest_member_names_are_forbidden(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            _ = (root / "MSVCP140.dll").write_bytes(b"x")  # 清单成员、静态模式抓不到。
            _ = (root / "LICENSE").write_text("product license", encoding="utf-8")
            hits = markdown_acceptance.scan_forbidden_assets(root)
        assert any("MSVCP140.dll" in hit for hit in hits)  # nosec B101: 清单驱动命中。
        assert not any("LICENSE" in hit for hit in hits)  # nosec B101: 文档/许可成员不误报。


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


if __name__ == "__main__":
    _ = unittest.main()
