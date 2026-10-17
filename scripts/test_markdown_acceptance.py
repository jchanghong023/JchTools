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


def _bundled_package_fixture(root: Path) -> Path:
    """小字节真实目录/收据夹具；仅测承接判定，不冒充真实模型或转换 E2E。"""
    runtime = root / "xberg"
    runtime.mkdir(parents=True)
    _ = (root / "JchTools.exe").write_bytes(b"synthetic GUI; never executed")
    names = set(markdown_acceptance.BASE_REQUIRED) | {
        "LICENSE", "THIRD_PARTY_LICENSES.md", "models/paddleocr-onnx-models-LICENSE.txt",
        *(f"models/models--fixture/snapshots/fixed{suffix}" for suffix in markdown_acceptance.DOCUMENT_MODEL_SUFFIXES),
    }
    records: list[dict[str, object]] = []
    for name in sorted(names):
        identity_name = name.removeprefix("sherpa-onnx/") if "onnxruntime" in name else name
        content = f"synthetic fixture: {identity_name}".encode()
        path = runtime / name
        path.parent.mkdir(parents=True, exist_ok=True)
        _ = path.write_bytes(content)
        records.append({"path": name, "size_bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()})
    manifest = root.parent / f"{root.name}-model-identities.json"
    _ = manifest.write_text(json.dumps({"xberg": {"members": records}}), encoding="utf-8")

    def component(component_id: str, members: list[dict[str, object]]) -> dict[str, object]:
        return {
            "id": component_id, "archive": f"xberg-{component_id}-x86_64-pc-windows-msvc.zip",
            "requires": [], "size_bytes": 1, "sha256": "a" * 64, "members": members,
        }

    document = [record for record in records if not str(record["path"]).startswith("models/snapshot-ocr/")]
    snapshot = [record for record in records if not str(record["path"]).startswith("models/models--")]
    audio_members = []
    for name in (*markdown_acceptance.MEDIA_REQUIRED, "ffmpeg/LICENSE.txt"):
        content = f"synthetic audio fixture: {name}".encode()
        audio_members.append({"path": name, "size_bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()})
    audio = component("audio-assets", audio_members)
    tag = "v-fixture"
    prefix = f"{markdown_acceptance.RELEASE_DOWNLOAD}{tag}/"
    audio["url"] = prefix + str(audio["archive"])
    receipt = {
        "owner": "JchTools-bundled-xberg-v1", "tag": tag, "source": markdown_acceptance.RELEASE_SOURCE,
        "manifest": {
            "archive": markdown_acceptance.COMPONENT_MANIFEST_NAME,
            "url": prefix + markdown_acceptance.COMPONENT_MANIFEST_NAME,
            "size_bytes": 1, "sha256": "b" * 64,
        },
        "components": [component("document", document), component("snapshot", snapshot)],
        "audio_assets": audio,
    }
    _ = (runtime / markdown_acceptance.RECEIPT_NAME).write_text(json.dumps(receipt), encoding="utf-8")
    return manifest


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
            patch.object(markdown_acceptance, "start_owned_process", return_value=(MagicMock(), MagicMock())),
            patch.object(markdown_acceptance, "_connect_window", return_value=(None, object())),
            patch.object(markdown_acceptance, "_click_button"),
            patch.object(markdown_acceptance, "_convert_directory_rows", return_value=[object(), object()]),
            patch.object(markdown_acceptance, "_set_row_edit"),
            patch.object(markdown_acceptance, "_wait_start_ready", return_value=True),
            patch.object(markdown_acceptance, "_button_state", side_effect=available_button),
            patch.object(markdown_acceptance, "_window_texts", side_effect=["尚未开始", completed, completed]),
            patch.object(markdown_acceptance, "_request_close"),
            patch.object(markdown_acceptance, "_terminate", return_value=(False, 0)),
            patch.object(markdown_acceptance, "CONVERSION_TIMEOUT", 1),
            patch("scripts.markdown_acceptance.time.time", side_effect=[0.0, 0.0, 0.0, 0.0, 2.0]),
            patch("scripts.markdown_acceptance.time.sleep"),
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
            patch.object(markdown_acceptance, "ITEMS", covered_matrix),
            patch.object(markdown_acceptance, "_shared_format_rows", return_value=(rows, None)),
        ):
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
                patch.object(markdown_acceptance, "xberg_format_extensions", return_value=(["rst"], None)),
                patch.object(
                    markdown_acceptance,
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
                    patch.object(markdown_acceptance, "_formats_request_inputs", return_value=(str(root), root)),
                    patch.object(markdown_acceptance, "_open_formats_pipe"),
                    patch.object(markdown_acceptance, "_exchange_formats", side_effect=reply) as exchange,
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
            patch.object(markdown_acceptance, "_formats_request_inputs", return_value=(str(root), root)),
            patch.object(markdown_acceptance, "_open_formats_pipe") as connect,
            patch.object(markdown_acceptance, "_exchange_formats", side_effect=pywintypes.error(
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
                patch.dict(
                    os.environ,
                    gui_smoke.isolated_state_env(markdown_acceptance.TMP_ROOT / "dynamic-format-tests"),
                    clear=True,
                ),
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
            patch.object(markdown_acceptance, "_resolve_gui", return_value=(root / "JchTools.exe", None)),
            patch.object(isolation_probe, "_wait_process_isolation", return_value=True),
            patch.object(markdown_acceptance, "start_owned_process", return_value=(process, MagicMock())) as start,
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
            patch.object(markdown_acceptance, "_formats_request_inputs", return_value=(str(root), root)),
            patch.object(markdown_acceptance, "_open_formats_pipe"),
            patch.object(markdown_acceptance, "_exchange_formats", side_effect=reply),
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
                patch.object(markdown_acceptance, "_formats_request_inputs", return_value=(str(root), root)),
                patch.object(markdown_acceptance, "_open_formats_pipe"),
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
            patch.object(markdown_acceptance, "scan_python_modules", side_effect=failing_scan),
            patch.object(markdown_acceptance, "drive_conversion", side_effect=fake_drive),
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
                markdown_acceptance,
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
                markdown_acceptance,
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
                patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                patch.object(markdown_acceptance, "_asset_precondition", return_value=None),
                patch.object(markdown_acceptance, "_resolve_gui", return_value=(context.gui_exe, None)),
                patch.object(markdown_acceptance, "manifest_has_python_entries", return_value=False),
                patch.object(markdown_acceptance, "scan_python_modules", side_effect=scan),
                patch.object(markdown_acceptance, "drive_conversion", side_effect=convert),
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
                    "scripts.markdown_acceptance.start_owned_process", side_effect=AssertionError("不得启动 GUI")
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
            with patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest):
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
            with patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest):
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
                    with patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest):
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
                with patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest):
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
                with patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest), self.assertRaises(ValueError):
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
                with patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest), self.assertRaises(ValueError):
                    markdown_acceptance.scan_distribution(root)

    def test_invalid_distribution_failure_and_report_details_are_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "package"
            manifest = _bundled_package_fixture(root)
            _ = (root / "xberg/xberg.exe").write_bytes(b"broken engine")
            item = next(item for item in markdown_acceptance.ITEMS if item.item_id == "C02")
            with patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest):
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
            patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest),
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
                patch.object(markdown_acceptance, "ASSET_MANIFEST", manifest),
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
                patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                patch.object(markdown_acceptance, "drive_conversion", side_effect=convert),
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
                    patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                    patch.dict(os.environ, environment, clear=True),
                    patch.object(markdown_acceptance, "manifest_has_python_entries", return_value=False),
                    patch.object(markdown_acceptance, "scan_python_modules", return_value=([], None)),
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
                    patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                    patch.dict(os.environ, self._environment(root), clear=True),
                    patch.object(markdown_acceptance, "manifest_has_python_entries", return_value=False),
                    patch.object(markdown_acceptance, "scan_python_modules", return_value=([], None)),
                    patch.object(
                        markdown_acceptance,
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
                    patch.object(markdown_acceptance, "drive_conversion") as drive,
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
                    patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                    patch.dict(os.environ, self._environment(root), clear=True),
                    patch.object(markdown_acceptance, "manifest_has_python_entries", return_value=False),
                    patch.object(markdown_acceptance, "scan_python_modules", return_value=([], None)),
                    patch.object(markdown_acceptance, "drive_conversion", side_effect=LookupError("review unknown failure")),
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
                patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                patch.object(markdown_acceptance, "GUI_SMOKE", self._delegate_consumer(root)),
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
                patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                patch.object(markdown_acceptance, "GUI_SMOKE", self._delegate_consumer(root, fail=True)),
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
                    patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                    patch.object(markdown_acceptance, "GUI_SMOKE", self._delegate_consumer(root)),
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
                    patch.object(markdown_acceptance, "SCRATCH_ROOT", root / "scratch"),
                    patch.dict(os.environ, self._environment(root), clear=True),
                    patch.object(markdown_acceptance, "start_owned_process", return_value=(process, owner)),
                    patch.object(markdown_acceptance, "_connect_window", return_value=(None, object())),
                    patch.object(isolation_probe, "_wait_process_isolation", return_value=True),
                    patch.object(markdown_acceptance, "_configure_conversion_paths", return_value=None),
                    patch.object(markdown_acceptance, "_click_button", side_effect=click),
                    patch.object(markdown_acceptance, "_button_state", side_effect=button),
                    patch.object(
                        markdown_acceptance,
                        "_window_texts",
                        side_effect=["尚未开始", "成功 3，部分提取 0，失败 0"],
                    ),
                    patch.object(markdown_acceptance, "_request_close", side_effect=request_close),
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
                markdown_acceptance, "_request_close", side_effect=AssertionError("stale window accessed")
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
            patch.object(markdown_acceptance, "SCRATCH_ROOT", self.root / "scratch"),
            patch.dict(os.environ, ReviewRemediationAcceptanceTests._environment(self.root), clear=True),
            patch.object(markdown_acceptance, "drive_conversion", side_effect=collect) as capture,
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
            patch.object(markdown_acceptance, "SCRATCH_ROOT", self.root / "scratch"),
            patch.object(markdown_acceptance, "_c03_delegate_stages", side_effect=unexpected_download) as delegate,
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
                    patch.object(markdown_acceptance, "SCRATCH_ROOT", self.root / "scratch"),
                    patch.object(markdown_acceptance, "_c03_delegate_stages", return_value=result) as delegate,
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
                        patch.object(markdown_acceptance, "SCRATCH_ROOT", self.root / "scratch"),
                        patch.dict(
                            os.environ,
                            ReviewRemediationAcceptanceTests._environment(self.root),
                            clear=True,
                        ),
                        patch.object(markdown_acceptance, "drive_conversion", side_effect=collect_bad_artifacts) as capture,
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
            patch.object(markdown_acceptance, "SCRATCH_ROOT", self.root / "scratch"),
            patch.dict(os.environ, ReviewRemediationAcceptanceTests._environment(self.root), clear=True),
            patch.object(markdown_acceptance, "drive_conversion", side_effect=LookupError("unrelated capture failure")),
        ):
            outcome = markdown_acceptance.run_item(item, context)
        self.assertEqual(outcome.status, "FAIL")
        self.assertIn("LookupError: unrelated capture failure", outcome.reason)
        self.assertNotIn("正文缺少", outcome.reason)


if __name__ == "__main__":
    _ = unittest.main()
