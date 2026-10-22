"""转 Markdown 验收测试共享常量、夹具与假件（自 scripts/test_markdown_acceptance.py 原样搬移）."""

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
