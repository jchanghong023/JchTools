"""转 Markdown 验收 Xberg 资产/运行时探测（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import contextlib
import os
import sqlite3
from pathlib import Path
from typing import cast

__all__ = [
    "BASE_COMPONENT_IDS",
    "BASE_REQUIRED",
    "COMPONENT_MANIFEST_NAME",
    "DATA_DIRECTORY",
    "DOCUMENT_MODEL_SUFFIXES",
    "LOCAL_TEST_XBERG_DIR",
    "MEDIA_REQUIRED",
    "RECEIPT_NAME",
    "RELEASE_DOWNLOAD",
    "RELEASE_SOURCE",
    "_asset_root",
    "_fetch_xberg_directory",
    "_plain_windows_path",
    "_probe_legacy_runtime_dir",
    "_probe_runtime_dir",
    "_settings_state_dir",
]


# 与 crates/markdown/src/markdown_assets.rs 同口径：资产根目录与成员相对路径。
DATA_DIRECTORY = "markdown-assets"


# 本机测试引擎固定目录（AGENTS.md §3，与 scripts/test_gate.py 的同名常量同口径）：
# 测试只用该目录下的最新版真实引擎；通过隔离旧配置夹具接入，不提供产品手工路径入口。
LOCAL_TEST_XBERG_DIR = Path(r"C:\Users\jiang\Documents\xberg-test\xberg-cli-x86_64-pc-windows-msvc")


# 可选媒体只在媒体条目前置检查；文档/截图不能被未下载音频阻塞。
MEDIA_REQUIRED = (
    "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx",
    "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt",
    "models/vad/silero_vad.onnx",
    "ffmpeg/avutil-61.dll",
    "ffmpeg/swresample-7.dll",
    "ffmpeg/avcodec-63.dll",
    "ffmpeg/avformat-63.dll",
)


BASE_REQUIRED = (
    "xberg.exe",
    "onnxruntime.dll",
    "onnxruntime_providers_shared.dll",
    "MSVCP140.dll",
    "VCRUNTIME140.dll",
    "VCRUNTIME140_1.dll",
    "models/snapshot-ocr/det.onnx",
    "models/snapshot-ocr/rec.onnx",
    "models/snapshot-ocr/dict/dict.txt",
    "sherpa-onnx/sherpa-onnx-c-api.dll",
    "sherpa-onnx/sherpa-onnx-cxx-api.dll",
    "sherpa-onnx/onnxruntime.dll",
    "sherpa-onnx/onnxruntime_providers_shared.dll",
    "sherpa-onnx/LICENSE",
)


DOCUMENT_MODEL_SUFFIXES = (
    "/v6/det/tiny/model.onnx",
    "/v6/rec/tiny/model.onnx",
    "/v6/rec/tiny/dict.txt",
    "/v2/classifiers/PP-LCNet_x1_0_textline_ori.onnx",
)


RECEIPT_NAME = "jchtools-xberg-release.json"


RELEASE_SOURCE = "https://api.github.com/repos/jchanghong023/xberg/releases/latest"


RELEASE_DOWNLOAD = "https://github.com/jchanghong023/xberg/releases/download/"


COMPONENT_MANIFEST_NAME = "xberg-components-x86_64-pc-windows-msvc.json"


BASE_COMPONENT_IDS = frozenset(("document", "snapshot"))


def _asset_root() -> Path | None:
    # 验收只能使用调用方明确布置的隔离资产根，禁止读取用户生产资产目录。
    for name in ("JCHTOOLS_TEST_ASSET_ROOT", "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT"):
        override = os.environ.get(name)
        if override and Path(override).is_absolute():
            return Path(override)
    return None


def _settings_state_dir() -> Path | None:
    """返回调用方提供的隔离设置目录，禁止读取生产 SQLite（XB-18）。."""
    override = os.environ.get("JCHTOOLS_TEST_STATE_DIR")
    if override and Path(override).is_absolute():
        return Path(override)
    return None


def _plain_windows_path(text: str) -> str:
    r"""剥除 Windows verbatim 前缀（与 src/platform.rs display_path_text 同口径）.

    应用保存目录经 Rust fs::canonicalize 落盘为 `\\?\C:\...` / `\\?\UNC\...`，
    界面显示与测试期望均为常规路径；测试链在读取侧归一，避免同一目录因
    表示形式不同而误判。
    """
    if text.startswith("\\\\?\\UNC\\"):
        return "\\\\" + text[len("\\\\?\\UNC\\") :]
    if text.startswith("\\\\?\\"):
        return text[len("\\\\?\\") :]
    return text


def _probe_legacy_runtime_dir(root: Path, missing: list[str]) -> Path | None:
    """只读旧文本指针；缺席允许内置发现，读取错误不能伪装为未配置."""
    selection = root / "xberg-runtime-path.txt"
    if not selection.exists():
        return None
    try:
        text = selection.read_text(encoding="utf-8").strip()
        return Path(_plain_windows_path(text)) if text else None
    except (OSError, ValueError) as error:
        missing.append(f"读取旧 Xberg 目录失败：{error}")
        return None


def _fetch_xberg_directory(database: Path) -> tuple[object, ...] | None:
    """同一只读 SQLite 快照选择活动来源，不吞掉未知来源或配置读取错误."""
    with contextlib.closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as conn:
        settings = cast("dict[str, str]", dict(conn.execute("SELECT key,value FROM app_settings")))
    source = settings.get("xberg_source", "custom")
    keys = {
        "custom": "xberg_custom_directory",
        "downloaded": "xberg_downloaded_directory",
        "bundled": "xberg_bundled_directory",
    }
    if source not in keys:
        message = f"未知 Xberg 来源：{source}"
        raise ValueError(message)
    stored = settings.get(keys[source])
    if stored is None and source == "custom":
        stored = settings.get("xberg_directory")
    return (stored,) if stored is not None else None


def _probe_runtime_dir(root: Path, missing: list[str]) -> Path | None:
    """复用产品有效源优先级；只发现显式隔离 bundle，不读取真实安装/注册表."""
    state = _settings_state_dir()
    database = state / "config.sqlite3" if state is not None else None
    try:
        row = _fetch_xberg_directory(database) if database is not None and database.is_file() else None
    except (sqlite3.Error, OSError, ValueError) as error:
        missing.append(f"读取应用配置 SQLite 失败：{error}")
        return None
    stored = row[0] if row else None
    runtime_dir = (
        Path(_plain_windows_path(stored.strip()))
        if isinstance(stored, str) and stored.strip()
        else _probe_legacy_runtime_dir(root, missing)
    )
    if missing:
        return None
    if runtime_dir is not None and runtime_dir.is_absolute() and (runtime_dir / "xberg.exe").is_file():
        return runtime_dir
    bundled = os.environ.get("JCHTOOLS_TEST_BUNDLED_XBERG_DIR")
    if bundled:
        candidate = Path(_plain_windows_path(bundled))
        if candidate.is_absolute() and (candidate / "xberg.exe").is_file():
            return candidate
        missing.append(f"内置 Xberg 目录无效或缺 xberg.exe：{candidate}")
    elif runtime_dir is not None:
        missing.append(f"Xberg 运行目录无效或缺 xberg.exe：{runtime_dir}")
    else:
        missing.append("未配置有效 Xberg 且未提供隔离内置目录（JCHTOOLS_TEST_BUNDLED_XBERG_DIR）")
    return None
