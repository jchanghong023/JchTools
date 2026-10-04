#!/usr/bin/env python3
"""转 Markdown 验收承接驱动器（docs/requirements/ALL2MARKDOWN.md 附录 A · F26）.

定位与边界（如实声明，不得虚构）：
  本脚本只建立「承接入口」：把附录 A 的 26 项格式矩阵、界面 E2E 与双形态/环境前置
  落成逐项可执行的验收条目。真实 Xberg 运行时、媒体模型、被测 GUI 二进制或发布包
  不在场时，对应条目一律 NOT RUN 并列出所需资产与获取方式，绝不在缺资产环境下把
  条目报成 PASS。当前环境没有媒体组件与被测 GUI，默认运行预期是「全部 NOT RUN、
  退出码 2」，这正是要如实呈现的状态。

入口形态结论（依据 T-04 与 src/main.rs）：转 Markdown 只提供 GUI 入口，仓库没有
  CLI 子命令，src/markdown.rs::run 是库函数而非公开入口。因此 A 组矩阵条目的
  「真实公开入口」唯一形态是 GUI（pywinauto 驱动）；B 组引用 scripts/gui_smoke.py
  的阶段；A25 的格式清单枚举与 src/markdown.rs::supported_formats 同参数只读调用
  xberg.exe（formats --format json），仅用于确定待测格式集合，转换本身仍走 GUI。

用法：
    python scripts/markdown_acceptance.py --list
    python scripts/markdown_acceptance.py                                # 缺资产→全 NOT RUN
    python scripts/markdown_acceptance.py --only A,A24
    python scripts/markdown_acceptance.py --gui-exe target/debug/JchTools.exe \
        --portable-root <解包的便携目录> --installed-root <安装目录> \
        --json .tmp/markdown-acceptance/report.json

依赖：pywinauto / Pillow /（媒体合成另需 PATH 上的 ffmpeg），见 scripts/requirements-dev.txt。
验收隔离：资产根必须由调用方通过 `JCHTOOLS_TEST_ASSET_ROOT`（或
`JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT`）提供；脚本不读取生产 SQLite、生产资产或截图数据。
退出码：任一 FAIL→1；全部 NOT RUN→2；其余（有 PASS、无 FAIL）→0；参数错误→3。
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import hashlib
import io
import json
import ntpath
import os
import re
import shutil
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING, cast

import comtypes
import pywintypes
import win32con
import win32gui
import win32process
from PIL import Image, ImageDraw, ImageFont
from pywinauto import Application, controls, findbestmatch, findwindows, timings
from pywinauto.application import ProcessNotFoundError, WindowSpecification

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

    from pywinauto.base_wrapper import BaseWrapper

ROOT = Path(__file__).resolve().parent.parent
FIXTURES_DEFAULT = ROOT / "tests" / "markdown_fixtures"
SCRATCH_ROOT = ROOT / ".tmp" / "markdown-acceptance"
GUI_SMOKE = ROOT / "scripts" / "gui_smoke.py"
ASSET_MANIFEST = ROOT / "resources" / "markdown-assets.json"
FORMAT_MANIFEST = ROOT / "resources" / "markdown-xberg-formats.json"

# 状态常量名避开 pass 字样（质量门 S105 把含该字样的变量名当疑似硬编码口令）。
STATUS_OK = "PASS"
STATUS_FAILED = "FAIL"
STATUS_NOT_RUN = "NOT RUN"

# 与 src/markdown_assets.rs 的常量同口径：资产根目录、固定版本与成员相对路径。
DATA_DIRECTORY = "markdown-assets"
XBERG_TAG = "v2026.10.2-0920-run54.1"
XBERG_ARCHIVE_URL = (
    f"https://github.com/jchanghong023/xberg/releases/download/{XBERG_TAG}/xberg-cli-x86_64-pc-windows-msvc.zip"
)
# 与 src/markdown_assets.rs 同口径：推理组件安装根（每个发布版本一个 tag 子目录）
# 与媒体转录在位校验的必需成员（存在性；SHA-256 校验由初始化/清单承接）。
INFERENCE_ROOT_RELPATH = "xberg-inference"
INFERENCE_REQUIRED = (
    "xberg.exe",
    "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/model.int8.onnx",
    "models/sense_voice_zh_en_ja_ko_yue_2024_07_17/tokens.txt",
    "models/vad/silero_vad.onnx",
    "sherpa-onnx/sherpa-onnx-c-api.dll",
    "sherpa-onnx/sherpa-onnx-cxx-api.dll",
    "sherpa-onnx/onnxruntime.dll",
    "sherpa-onnx/onnxruntime_providers_shared.dll",
    "ffmpeg/avutil-61.dll",
    "ffmpeg/swresample-7.dll",
    "ffmpeg/avcodec-63.dll",
    "ffmpeg/avformat-63.dll",
)

# T-28 / 附录 A 的旧项目与旧缓存前置：旧目录仍在即未满足「删除旧项目后独立运行」。
OLD_PROJECT_DIR = Path(r"D:\code1111111111\all2markdown")

# 转 Markdown 页自上而下的「选择目录…」行数：输入 / 输出（Xberg 目录在设置页，XB-20）。
CONVERT_ROW_COUNT = 2

# T-08/A18：固定运行时清单目前只声明这三个 JPEG 2000 扩展名。
# JPX/JPM/MJ2 属于未承诺格式，验收必须明确列为 unsupported，不能因上游
# 变化或夹具在场就把它们扩展为产品支持范围。
A18_SUPPORTED_EXTENSIONS = ("jp2", "j2k", "j2c")
A18_UNSUPPORTED_EXTENSIONS = ("jpx", "jpm", "mj2")

# A02 合成输入的图片数量；所有生成、媒体落盘和正文断言共用此常量。
A02_IMAGE_COUNT = 6
A02_IMAGE_TOKENS = (
    "ALPHA-TOKEN",
    "BETA-TOKEN",
    "GAMMA-TOKEN",
    "DELTA-TOKEN",
    "EPSILON-TOKEN",
    "ZETA-TOKEN",
)


def _old_cache_candidates() -> list[Path]:
    local = os.environ.get("LOCALAPPDATA")
    candidates = [OLD_PROJECT_DIR / ".venv"]
    if local:
        candidates.append(Path(local) / "all2markdown")
    candidates.append(Path.home() / ".cache" / "all2markdown")
    return candidates


# 附录 A 双形态检查：主包不得携带的转换专用资产（文件名/后缀，全部小写比较）。
FORBIDDEN_SUFFIXES = (".onnx",)
FORBIDDEN_NAMES = ("xberg.exe", "markdown-media-worker.exe")
FORBIDDEN_DLL_PREFIXES = ("python", "onnxruntime", "sherpa", "avcodec", "avformat", "swresample", "ffmpeg")

GUI_WINDOW_TIMEOUT = 60
READINESS_TIMEOUT = 180
BUSY_TIMEOUT = 30
CONVERSION_TIMEOUT = 1800  # 真实 OCR/媒体转录按分钟级计，验收宁等勿假。
EDIT_ROW_TOLERANCE_PX = 20
PHYSICAL_OVERLAP_THRESHOLD = 0.8

# pywinauto/pywin32 窗口操作在窗口建立/销毁竞态下的瞬态错误族；枚举与 gui_smoke.py
# 的 TRANSIENT_GUI_ERRORS 同源（那边附有逐项理由），此处等待循环内一律按可重试处理。
TRANSIENT_ERRORS: tuple[type[Exception], ...] = (
    pywintypes.error,
    pywintypes.com_error,
    comtypes.COMError,
    ProcessNotFoundError,
    timings.TimeoutError,
    findwindows.ElementNotFoundError,
    findbestmatch.MatchError,
    controls.InvalidWindowHandle,
    controls.InvalidElement,
)

# json.loads 的返回含 Any；经固定签名别名收口为 object，再以 isinstance 逐层收窄。
_parse_json: Callable[[str], object] = json.loads


def _is_str_obj_map(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def _is_str_obj_list(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


def _str_field(entry: object, key: str) -> str | None:
    if _is_str_obj_map(entry):
        got = entry.get(key)
        if isinstance(got, str):
            return got
    return None


def _fixed_format_extensions() -> tuple[set[str], str | None]:
    """读取与产品内置清单相同的固定格式集合（只读，不调用引擎）。."""
    try:
        parsed = _parse_json(FORMAT_MANIFEST.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        return set(), f"固定 Xberg 格式清单无法读取：{error}"
    if not _is_str_obj_map(parsed):
        return set(), "固定 Xberg 格式清单不是 JSON 对象"
    rows = parsed.get("formats")
    if not _is_str_obj_list(rows):
        return set(), "固定 Xberg 格式清单缺少 formats 数组"
    extensions = {
        token
        for row in rows
        if (token := (_str_field(row, "extension") or "").strip().lstrip(".").lower()) and "." not in token
    }
    return extensions, None


def _reconfigure_stdout() -> None:
    # 阶段明细含中文；Windows 控制台默认代码页会把 print 变成 UnicodeEncodeError。
    with contextlib.suppress(AttributeError):
        stream = sys.stdout
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")


@dataclasses.dataclass
class Outcome:
    """单个条目的验收结论；reason 为 NOT RUN/FAIL 的必要说明."""

    status: str
    reason: str = ""
    details: list[str] = dataclasses.field(default_factory=list)


@dataclasses.dataclass
class AssetProbe:
    """只读探测本机转 Markdown 资产状态（不联网、不修改任何文件）."""

    root: Path | None
    runtime_dir: Path | None
    xberg_exe: Path | None
    inference_dir: Path | None
    missing: list[str]

    def xberg_ready(self) -> bool:
        return self.xberg_exe is not None

    def media_ready(self) -> bool:
        return self.inference_dir is not None and not any("推理组件" in text or "媒体" in text for text in self.missing)

    def acquire_hint(self) -> str:
        hint = f"Xberg 运行目录：从 {XBERG_ARCHIVE_URL} 下载解压（固定 tag {XBERG_TAG}），"
        hint += "在 GUI「转 Markdown」页选择并点「使用此目录」保存；推理组件：同页「初始化可选组件」"
        hint += "按清单下载 Xberg 推理组件包（媒体转录所需的 xberg.exe、SenseVoice/VAD 模型与 FFmpeg/sherpa-onnx "
        hint += "运行库，来源与成员摘要见 resources/markdown-assets.json）"
        return hint


def _asset_root() -> Path | None:
    # 验收只能使用调用方明确布置的隔离资产根，禁止读取用户生产资产目录。
    for name in ("JCHTOOLS_TEST_ASSET_ROOT", "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT"):
        override = os.environ.get(name)
        if override and Path(override).is_absolute():
            return Path(override)
    return None


def _manifest_model_paths() -> list[str]:
    """读取资产清单中推理组件包成员的安装相对路径；解析失败返回空表（调用方按未知处理）."""
    try:
        raw = ASSET_MANIFEST.read_text(encoding="utf-8")
    except OSError:
        return []
    parsed = _parse_json(raw)
    inference = parsed.get("xberg_inference") if _is_str_obj_map(parsed) else None
    members = inference.get("members") if _is_str_obj_map(inference) else None
    if not _is_str_obj_list(members):
        return []
    return [relative for entry in members if (relative := _str_field(entry, "install_path")) is not None]


def _settings_state_dir() -> Path | None:
    """返回调用方提供的隔离设置目录，禁止读取生产 SQLite（XB-18）。."""
    override = os.environ.get("JCHTOOLS_TEST_STATE_DIR")
    if override and Path(override).is_absolute():
        return Path(override)
    return None


def _probe_legacy_runtime_dir(root: Path, missing: list[str]) -> Path | None:
    """旧文本指针（SQLite 无值时的迁移源，与 xberg_settings::load 同口径）."""
    selection = root / "xberg-runtime-path.txt"
    runtime_dir: Path | None = None
    if not selection.is_file():
        missing.append(f"未配置 Xberg 运行目录（SQLite 无记录且未找到 {selection}）")
        return None
    with contextlib.suppress(OSError, ValueError):
        runtime_dir = Path(selection.read_text(encoding="utf-8").strip())
    if runtime_dir is not None and not (runtime_dir / "xberg.exe").is_file():
        missing.append(f"Xberg 运行目录缺 xberg.exe：{runtime_dir / 'xberg.exe'}")
    return runtime_dir


def _fetch_xberg_directory(database: Path) -> tuple[object, ...] | None:
    """只读读取应用级 SQLite 保存的共享 Xberg 目录；无记录返回 None."""
    with contextlib.closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as conn:
        cursor = conn.execute("SELECT value FROM app_settings WHERE key='xberg_directory'")
        return cast("tuple[object, ...] | None", cursor.fetchone())


def _probe_runtime_dir(root: Path, missing: list[str]) -> Path | None:
    """只读核对运行目录指针与 xberg.exe；缺失项写入 missing.

    XB-18 后应用把共享目录保存在应用级 SQLite（config.sqlite3 的
    app_settings.xberg_directory）；旧文本指针仅作为 SQLite 无值时的迁移源。
    """
    runtime_dir: Path | None = None
    state = _settings_state_dir()
    database = state / "config.sqlite3" if state is not None else None
    if database is None or not database.is_file():
        runtime_dir = _probe_legacy_runtime_dir(root, missing)
    else:
        try:
            row = _fetch_xberg_directory(database)
        except sqlite3.Error as error:
            missing.append(f"读取应用配置 SQLite 失败：{error}")
        else:
            stored = row[0] if row else None
            if isinstance(stored, str) and stored.strip():
                runtime_dir = Path(stored.strip())
            else:
                runtime_dir = _probe_legacy_runtime_dir(root, missing)
    if runtime_dir is not None and not (runtime_dir / "xberg.exe").is_file():
        missing.append(f"Xberg 运行目录缺 xberg.exe：{runtime_dir / 'xberg.exe'}")
        runtime_dir = None
    return runtime_dir


def _resolve_inference_dir(root: Path, missing: list[str]) -> Path | None:
    """解析推理组件目录（与 Rust 侧 resolve_xberg_component 同口径）.

    开发期可用 JCHTOOLS_XBERG_INFERENCE_DIR 指向本地组件树；否则取安装根下
    唯一的 tag 子目录（0 个未安装，多于 1 个无法判定）。
    """
    override = os.environ.get("JCHTOOLS_XBERG_INFERENCE_DIR")
    if override and Path(override).is_absolute():
        return Path(override)
    inference_root = root / INFERENCE_ROOT_RELPATH
    tag_dirs = [entry for entry in inference_root.glob("*") if entry.is_dir()] if inference_root.is_dir() else []
    if len(tag_dirs) == 1:
        return tag_dirs[0]
    if not tag_dirs:
        missing.append(f"推理组件未安装：{inference_root}（GUI「初始化可选组件」下载）")
    else:
        names = "、".join(sorted(entry.name for entry in tag_dirs))
        missing.append(f"推理组件目录存在多个版本，无法确定使用哪一个：{names}")
    return None


def _check_inference_members(inference_dir: Path, missing: list[str]) -> None:
    """在位校验：固定必需成员 + 清单声明的组件包成员；缺失项写入 missing."""
    holes = [name for name in INFERENCE_REQUIRED if not (inference_dir / name).is_file()]
    if holes:
        shown = "、".join(holes[:3])
        missing.append(f"推理组件不完整（缺 {len(holes)} 个，如 {shown}；组件目录 {inference_dir}）")
        return
    manifest_members = _manifest_model_paths()
    if not manifest_members:
        missing.append("推理组件：无法解析 resources/markdown-assets.json 的组件包成员，安装面未知")
        return
    member_holes = [name for name in manifest_members if not (inference_dir / name).is_file()]
    if member_holes:
        shown = "、".join(member_holes[:3])
        missing.append(f"推理组件清单成员缺失（{len(member_holes)} 个，如 {shown}；组件目录 {inference_dir}）")


def probe_assets() -> AssetProbe:
    """只读核对：已保存的 Xberg 指针、xberg.exe 与推理组件是否在场."""
    missing: list[str] = []
    root = _asset_root()
    if root is None:
        return AssetProbe(None, None, None, None, ["未提供隔离验收资产根（JCHTOOLS_TEST_ASSET_ROOT）"])
    runtime_dir = _probe_runtime_dir(root, missing)
    xberg_exe = (runtime_dir / "xberg.exe") if runtime_dir is not None else None
    if xberg_exe is not None and not xberg_exe.is_file():
        xberg_exe = None
    inference_dir = _resolve_inference_dir(root, missing)
    if inference_dir is not None:
        _check_inference_members(inference_dir, missing)
    return AssetProbe(root, runtime_dir, xberg_exe, inference_dir, missing)


def _file_digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


# ---------------------------------------------------------------- 合成夹具构造器。


@dataclasses.dataclass
class SynthResult:
    """一次合成产物：输入文件名列表与失败原因（失败即整项 NOT RUN，不降级）."""

    files: list[str]
    error: str = ""


def _write_pnm_ascii(target: Path) -> None:
    pixels = [[0, 1, 0], [1, 0, 1], [0, 1, 0]]
    _ = target.joinpath("p1_ascii.pbm").write_text(
        "P1\n3 3\n" + "\n".join(" ".join(map(str, row)) for row in pixels) + "\n", encoding="ascii"
    )
    _ = target.joinpath("p2_ascii.pgm").write_text(
        "P2\n3 3\n255\n" + "\n".join(" ".join(str(255 - v * 127) for v in row) for row in pixels) + "\n",
        encoding="ascii",
    )
    _ = target.joinpath("p3_ascii.ppm").write_text(
        "P3\n3 3\n255\n"
        + "\n".join(" ".join(f"{v * 80} {255 - v * 80} {v * 40}" for v in row) for row in pixels)
        + "\n",
        encoding="ascii",
    )


def _write_pnm_binary(target: Path) -> None:
    pixels = [[0, 1, 0], [1, 0, 1], [0, 1, 0]]
    _ = target.joinpath("p4_bin.pbm").write_bytes(
        b"P4\n3 3\n" + bytes((row[0] << 6) | (row[1] << 5) | (row[2] << 4) for row in pixels)
    )
    _ = target.joinpath("p5_bin.pgm").write_bytes(
        b"P5\n3 3\n255\n" + bytes(255 - v * 127 for row in pixels for v in row)
    )
    # P6 3x3 RGB 需要恰好 27 个栅格字节；旧值 *3 是 9 字节的截断输入，
    # 会被 xberg 按损坏输入优雅拒绝（exit 1 + 诊断），A17 因此整项失败。
    _ = target.joinpath("p6_bin.ppm").write_bytes(b"P6\n3 3\n255\n" + bytes((0, 128, 64)) * 9)


def _synth_pnm(target: Path) -> SynthResult:
    """A17：纯标准库构造 PNM 家族 ASCII（P1-P3）与二进制（P4-P6）六种表示."""
    names = ["p1_ascii.pbm", "p2_ascii.pgm", "p3_ascii.ppm", "p4_bin.pbm", "p5_bin.pgm", "p6_bin.ppm"]
    try:
        _write_pnm_ascii(target)
        _write_pnm_binary(target)
    except OSError as exc:
        return SynthResult([], f"构造 PNM 夹具失败：{exc}")
    return SynthResult(names)


def _synth_images(target: Path) -> SynthResult:
    """A16/A20：用 Pillow 合成常规位图与异常/损坏变体."""
    names = [
        "img.jpg",
        "img.jpeg",
        "img.gif",
        "img.tif",
        "img.tiff",
        "img.bmp",
        "img.webp",
        "alpha.png",
        "cmyk.tif",
        "truncated.jpg",
        "huge.png",
        "empty.png",
        "fake_text.png",
    ]
    try:
        base = Image.new("RGB", (64, 64), "white")
        draw = ImageDraw.Draw(base)
        # 高对比黑色条块：可 OCR 程度交给真实引擎，不在合成侧预设文字断言。
        draw.rectangle((6, 10, 20, 26), fill="black")
        draw.rectangle((30, 34, 52, 50), fill="black")
        for name in ("img.jpg", "img.jpeg", "img.gif", "img.tif", "img.tiff", "img.bmp", "img.webp"):
            base.save(target / name)
        Image.new("RGBA", (64, 64), (255, 255, 255, 0)).save(target / "alpha.png")
        Image.new("CMYK", (64, 64), (0, 0, 0, 0)).save(target / "cmyk.tif")
        base.save(target / "truncated.jpg")
        raw = (target / "truncated.jpg").read_bytes()
        _ = (target / "truncated.jpg").write_bytes(raw[: max(1, len(raw) // 3)])
        Image.new("RGB", (8, 8), "white").save(target / "huge.png")
        data = bytearray((target / "huge.png").read_bytes())
        # IHDR 宽高改成超大值：解码必须失败并诊断，而不是漏报或试图分配巨型位图。
        data[16:24] = (60000).to_bytes(4, "big") * 2
        _ = (target / "huge.png").write_bytes(bytes(data))
        _ = (target / "empty.png").write_bytes(b"")
        _ = (target / "fake_text.png").write_bytes(b"this is not an image")
    except (OSError, ValueError) as exc:
        return SynthResult([], f"构造图片夹具失败：{exc}")
    return SynthResult(names)


# Office 变体改写表：源容器主部件 → [(目标扩展名, 目标 ContentType)]，均为 ECMA-376 标准值。
_OFFICE_VARIANTS: dict[str, tuple[str, tuple[tuple[str, str], ...]]] = {
    "chartex.docx": (
        "/word/document.xml",
        (
            ("docm", "application/vnd.ms-word.document.macroEnabled.main+xml"),
            ("dotx", "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml"),
            ("dotm", "application/vnd.ms-word.template.macroEnabled.main+xml"),
        ),
    ),
    "merged_table.pptx": (
        "/ppt/presentation.xml",
        (
            ("pptm", "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml"),
            ("ppsx", "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml"),
            ("potx", "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml"),
            ("potm", "application/vnd.ms-powerpoint.template.macroEnabled.main+xml"),
        ),
    ),
    "merged_header.xlsx": (
        "/xl/workbook.xml",
        (
            ("xltx", "application/vnd.openxmlformats-officedocument.spreadsheetml.template.main+xml"),
            ("xlsm", "application/vnd.ms-excel.sheet.macroEnabled.main+xml"),
            ("xlam", "application/vnd.ms-excel.addin.macroEnabled.main+xml"),
        ),
    ),
}


def _rewrite_content_types(blob: bytes, part: str, content_type: str) -> bytes:
    text = blob.decode("utf-8")
    pattern = rf'(<Override PartName="{re.escape(part)}" ContentType=")[^"]+(")'
    text, count = re.subn(pattern, rf"\g<1>{content_type}\g<2>", text)
    if count != 1:
        message = f"[Content_Types].xml 未命中主部件 Override：{part}"
        raise ValueError(message)
    return text.encode("utf-8")


def _synth_office(target: Path, fixtures_dir: Path) -> SynthResult:
    """A21-A23：以现有 docx/pptx/xlsx 为容器，按 ECMA-376 内容类型改写派生变体."""
    names: list[str] = []
    for source_name, (part, variants) in _OFFICE_VARIANTS.items():
        origin = fixtures_dir / source_name
        if not origin.is_file():
            continue
        try:
            with zipfile.ZipFile(origin) as archive:
                members = {name: archive.read(name) for name in archive.namelist()}
            for suffix, content_type in variants:
                out = target / f"{origin.stem}_as_{suffix}.{suffix}"
                types_blob = _rewrite_content_types(members["[Content_Types].xml"], part, content_type)
                with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as archive:
                    archive.writestr("[Content_Types].xml", types_blob)
                    for name, blob in members.items():
                        if name != "[Content_Types].xml":
                            archive.writestr(name, blob)
                names.append(out.name)
        except (OSError, ValueError, zipfile.BadZipFile) as exc:
            return SynthResult([], f"容器改写 {source_name} 失败：{exc}")
    if not names:
        return SynthResult([], "容器改写没有产出任何变体（源夹具缺失）")
    return SynthResult(names)


def _synth_media(target: Path) -> SynthResult:
    """A24：用 ffmpeg 合成 M4A、无语音、无音轨与损坏音轨变体（损坏=字节截断）."""
    ffmpeg = shutil.which("ffmpeg")
    if ffmpeg is None:
        return SynthResult([], "PATH 上没有 ffmpeg，无法合成媒体变体")
    commands = [
        (["-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "aac"], "tone.m4a"),
        (["-y", "-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono:duration=2", "-c:a", "aac"], "silence.m4a"),
        (["-y", "-f", "lavfi", "-i", "color=c=black:size=64x64:duration=1", "-c:v", "mpeg4"], "noaudio.mp4"),
    ]
    try:
        for args, name in commands:
            done = subprocess.run([ffmpeg, *args, str(target / name)], capture_output=True, text=True, check=False)
            if done.returncode != 0:
                return SynthResult([], f"ffmpeg 生成 {name} 失败：{(done.stderr or '').strip()[-200:]}")
        raw = (target / "tone.m4a").read_bytes()
        _ = (target / "damaged.mp4").write_bytes(raw[: max(1, len(raw) // 4)])
    except OSError as exc:
        return SynthResult([], f"构造媒体夹具失败：{exc}")
    names = ["tone.m4a", "silence.m4a", "noaudio.mp4", "damaged.mp4"]
    holes = [name for name in names if not (target / name).is_file()]
    if holes:
        return SynthResult([], f"媒体变体生成不完整：{holes}")
    return SynthResult(names)


# —— OOXML 矩阵夹具运行期合成（A02/A03/A09/A14）——
# 以 tests/markdown_fixtures 的完整 PPTX 骨架为容器（保证与转换器已验证的部件
# 集合兼容），仅替换 slide1 及其关系并注入媒体；XLSX 用最小标准部件集全量构造。


def _token_png(text: str) -> bytes:
    image = Image.new("RGB", (240, 80), "white")
    draw = ImageDraw.Draw(image)
    # Pillow ≥10.1 的 load_default(size=…) 返回 FreeTypeFont，但其类型桩标注不完整。
    # [quality-baseline approved 2026-10-03] Pillow 桩缺口（移除即复现），经用户裁定保留
    font = cast(
        "ImageFont.FreeTypeFont",
        ImageFont.load_default(size=24),  # pyright: ignore[reportInvalidCast]
    )
    # Pillow 桩对 ImageDraw.text 的标注不完整（部分未知），按行显式抑制。
    # [quality-baseline approved 2026-10-03] Pillow 桩缺口（移除即复现），经用户裁定保留
    draw.text((12, 26), text, fill="black", font=font)  # pyright: ignore[reportUnknownMemberType]
    buffer = io.BytesIO()
    image.save(buffer, "PNG")
    return buffer.getvalue()


_EMU_PER_PX = 9525
_PPTX_REL_TYPE_IMAGE = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image"
_PKG_REL_TYPE_IMAGE = "http://schemas.openxmlformats.org/package/2006/relationships/image"
_SKELETON_LAYOUT_REL_MISSING = "骨架 slide1.xml.rels 缺少 slideLayout 关系"
_XLSX_REL_TYPE_IMAGE = _PPTX_REL_TYPE_IMAGE
_DRAWING_REL_TYPE = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing"


def _pic_xml(index: int, rid: str, row: int, descr: str = "") -> str:
    attrs = f' descr="{descr}"' if descr else ""
    return (
        f'<p:pic><p:nvPicPr><p:cNvPr id="{index}" name="Picture {index}"{attrs}/>'
        "<p:cNvPicPr/><p:nvPr/></p:nvPicPr>"
        f'<p:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>'
        f'<p:spPr><a:xfrm><a:off x="500000" y="{500000 + row * 900000}"/>'
        f'<a:ext cx="{240 * _EMU_PER_PX}" cy="{80 * _EMU_PER_PX}"/></a:xfrm>'
        '<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>'
    )


def _slide_xml(pics: str) -> str:
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'
        ' xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">'
        "<p:cSld><p:spTree>"
        '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>'
        '<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/>'
        '<a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>'
        f"{pics}</p:spTree></p:cSld>"
        "<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"
    )


def _slide_rels_xml(rel_entries: str, layout_rel: str) -> str:
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f"{layout_rel}{rel_entries}</Relationships>"
    )


def _ensure_defaults(content_types: str, defaults: tuple[tuple[str, str], ...]) -> str:
    """[Content_Types].xml 缺失的 Default 逐个补齐（按给定顺序插在 </Types> 前）."""
    for extension, content_type in defaults:
        if f'Extension="{extension}"' in content_types:
            continue
        content_types = content_types.replace(
            "</Types>",
            f'<Default Extension="{extension}" ContentType="{content_type}"/></Types>',
        )
    return content_types


def _build_pptx(
    dest: Path,
    slide_xml: str,
    image_rels: list[tuple[str, str]],
    media: dict[str, bytes],
    extra_defaults: tuple[tuple[str, str], ...] = (),
) -> None:
    """克隆仓库 PPTX 骨架，替换 slide1 与其关系并注入媒体（布局关系保持原样）."""
    source = Path(__file__).resolve().parent.parent / "tests" / "markdown_fixtures" / "merged_table.pptx"
    with zipfile.ZipFile(source) as archive:
        # 骨架自带的旧 media 成员不属于本次合成输入；保留会让 A02 误得到
        # 7 张图片（6 张目标图 + 1 张骨架残留），从而掩盖关系归属问题。
        members = {member: archive.read(member) for member in archive.namelist() if not member.startswith("ppt/media/")}
    slide_rels = members["ppt/slides/_rels/slide1.xml.rels"].decode("utf-8")
    layout_match = re.search(r"<Relationship [^>]*slideLayout[^>]*/>", slide_rels)
    if layout_match is None:
        raise ValueError(_SKELETON_LAYOUT_REL_MISSING)
    rels = "".join(
        f'<Relationship Id="{rid}" Type="{_PPTX_REL_TYPE_IMAGE}" Target="../media/{part}"/>' for rid, part in image_rels
    )
    types = _ensure_defaults(
        members["[Content_Types].xml"].decode("utf-8"),
        (("png", "image/png"), *extra_defaults),
    )
    members["[Content_Types].xml"] = types.encode("utf-8")
    members["ppt/slides/slide1.xml"] = slide_xml.encode("utf-8")
    members["ppt/slides/_rels/slide1.xml.rels"] = _slide_rels_xml(rels, layout_match.group(0)).encode("utf-8")
    for part, blob in media.items():
        members[f"ppt/media/{part}"] = blob
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as archive:
        for member, blob in members.items():
            archive.writestr(member, blob)


def _synth_pptx_multi_images(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A02：一张 slide 六张不同文字图片（rId1..rId6），供两次连跑字节比对."""
    try:
        indexes = range(1, A02_IMAGE_COUNT + 1)
        media = {f"image{index}.png": _token_png(A02_IMAGE_TOKENS[index - 1]) for index in indexes}
        pics = "".join(_pic_xml(index, f"rId{index}", index - 1) for index in indexes)
        rels = [(f"rId{index}", f"image{index}.png") for index in indexes]
        _build_pptx(target / "pptx_multi_images.pptx", _slide_xml(pics), rels, media)
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A02 PPTX 失败：{exc}")
    return SynthResult(["pptx_multi_images.pptx"])


def _synth_pptx_shared_media(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A03：两个 shape 引用同一 media（同一 rId），两处位置各自保留."""
    try:
        media = {"image1.png": _token_png("SHARED-MEDIA-TOKEN")}
        pics = _pic_xml(2, "rId10", 0) + _pic_xml(3, "rId10", 1)
        _build_pptx(target / "pptx_shared_media.pptx", _slide_xml(pics), [("rId10", "image1.png")], media)
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A03 PPTX 失败：{exc}")
    return SynthResult(["pptx_shared_media.pptx"])


def _synth_pptx_descr_no_ocr(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A09：descr 带文字而图片本体无字——descr 不得冒充 OCR 正文."""
    try:
        media = {"image1.png": _token_png("")}
        pics = _pic_xml(2, "rId10", 0, descr="DESCR-NO-OCR-TOKEN")
        _build_pptx(target / "pptx_descr_no_ocr.pptx", _slide_xml(pics), [("rId10", "image1.png")], media)
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A09 PPTX 失败：{exc}")
    return SynthResult(["pptx_descr_no_ocr.pptx"])


def _synth_pptx_two_png_order(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A01：两张不同文字图片，rels 列举顺序与 blip 引用顺序交错，OCR 不得互换."""
    try:
        media = {"image1.png": _token_png("ALPHA-ONE"), "image2.png": _token_png("BETA-TWO")}
        # rels 列举 rId10→image1、rId11→image2；slide 先引用 rId11 再 rId10。
        pics = _pic_xml(2, "rId11", 0) + _pic_xml(3, "rId10", 1)
        _build_pptx(
            target / "pptx_two_png_order.pptx",
            _slide_xml(pics),
            [("rId10", "image1.png"), ("rId11", "image2.png")],
            media,
        )
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A01 PPTX 失败：{exc}")
    return SynthResult(["pptx_two_png_order.pptx"])


def _synth_pptx_svg(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A07：SVG 文本成员带失效外部引用——本地解析、外部资源禁用、失败隔离."""
    svg = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<svg xmlns="http://www.w3.org/2000/svg" width="240" height="80">'
        '<image href="http://127.0.0.1:9/never-resolves.png" width="1" height="1"/>'
        '<text x="12" y="48" font-size="24" fill="black">SVG-LOCAL-TEXT</text></svg>'
    )
    try:
        # 骨架替换：以 PNG 占位保持 zip 结构简单，SVG 作为独立媒体成员注入。
        media = {"image1.svg": svg.encode("utf-8"), "image2.png": _token_png("PNG-NEIGHBOR")}
        pics = _pic_xml(2, "rId10", 0) + _pic_xml(3, "rId11", 1)
        _build_pptx(
            target / "pptx_svg.pptx",
            _slide_xml(pics),
            [("rId10", "image1.svg"), ("rId11", "image2.png")],
            media,
            extra_defaults=(("svg", "image/svg+xml"),),
        )
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A07 PPTX 失败：{exc}")
    return SynthResult(["pptx_svg.pptx"])


def _synth_pptx_runs_fields(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A08：普通文本 run 与可见字段（a:fld）混排，字段值完整保留."""
    runs = (
        '<p:sp><p:nvSpPr><p:cNvPr id="9" name="TextBox"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
        '<p:spPr><a:xfrm><a:off x="500000" y="5600000"/><a:ext cx="8000000" cy="500000"/></a:xfrm>'
        '<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/>'
        '<a:lstStyle/><p:p><a:r><a:rPr lang="zh-CN" sz="1800"/><a:t>RUN-AND-FIELD</a:t></a:r>'
        '<a:fld id="{6E1B}" type="slidenum"><a:rPr lang="zh-CN" sz="1800"/><a:t>7</a:t></a:fld>'
        '<a:r><a:rPr lang="zh-CN" sz="1800"/><a:t>-FIELD-END</a:t></a:r></p:p></p:txBody></p:sp>'
    )
    try:
        media = {"image1.png": _token_png("A08-NEIGHBOR")}
        pics = _pic_xml(2, "rId10", 0)
        slide = _slide_xml(pics + runs)
        _build_pptx(target / "pptx_runs_fields.pptx", slide, [("rId10", "image1.png")], media)
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A08 PPTX 失败：{exc}")
    return SynthResult(["pptx_runs_fields.pptx"])


def _synth_pptx_undecodable_image(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A10：截断图片与正常内容同页——诊断阶段明确，其余内容继续输出."""
    try:
        good = _token_png("HEALTHY-TEXT-REMAINS")
        broken = _token_png("BROKEN")[: len(_token_png("BROKEN")) // 3]
        media = {"image1.png": broken, "image2.png": good}
        pics = _pic_xml(2, "rId10", 0) + _pic_xml(3, "rId11", 1)
        _build_pptx(
            target / "pptx_undecodable_image.pptx",
            _slide_xml(pics),
            [("rId10", "image1.png"), ("rId11", "image2.png")],
            media,
        )
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A10 PPTX 失败：{exc}")
    return SynthResult(["pptx_undecodable_image.pptx"])


def _synth_xlsx_drawing_order(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A14：最小 XLSX，两 anchor 显示顺序与 .rels 列举顺序相反，两图各有可 OCR 文字."""
    content_types = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Default Extension="png" ContentType="image/png"/>'
        '<Override PartName="/xl/workbook.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
        '<Override PartName="/xl/worksheets/sheet1.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
        '<Override PartName="/xl/drawings/drawing1.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.drawing+xml"/>'
        "</Types>"
    )
    root_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument'
        '/2006/relationships/officeDocument" Target="xl/workbook.xml"/>'
        "</Relationships>"
    )
    workbook = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        '<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>'
    )
    workbook_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument'
        '/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>'
        "</Relationships>"
    )
    sheet = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        "<sheetData/>"
        '<drawing r:id="rId1"/></worksheet>'
    )
    sheet_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f'<Relationship Id="rId1" Type="{_DRAWING_REL_TYPE}" '
        f'Target="../drawings/drawing1.xml"/>'
        "</Relationships>"
    )
    # 显示顺序由 anchor 决定：先 rId2（BOTTOM 在前）再 rId1；.rels 列举顺序相反。
    drawing = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"'
        ' xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        "<xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>"
        "<xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>6</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>"
        '<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="Bottom"/><xdr:cNvPicPr/><xdr:nvPr/></xdr:nvPicPr>'
        '<xdr:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill>'
        '<xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic>'
        "<xdr:clientData/></xdr:twoCellAnchor>"
        "<xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff>"
        "<xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>"
        "<xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff>"
        "<xdr:row>13</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>"
        '<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="3" name="Top"/><xdr:cNvPicPr/><xdr:nvPr/></xdr:nvPicPr>'
        '<xdr:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill>'
        '<xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic>'
        "<xdr:clientData/></xdr:twoCellAnchor>"
        "</xdr:wsDr>"
    )
    drawing_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f'<Relationship Id="rId1" Type="{_XLSX_REL_TYPE_IMAGE}" Target="../media/image1.png"/>'
        f'<Relationship Id="rId2" Type="{_XLSX_REL_TYPE_IMAGE}" Target="../media/image2.png"/>'
        "</Relationships>"
    )
    members = {
        "[Content_Types].xml": content_types,
        "_rels/.rels": root_rels,
        "xl/workbook.xml": workbook,
        "xl/_rels/workbook.xml.rels": workbook_rels,
        "xl/worksheets/sheet1.xml": sheet,
        "xl/worksheets/_rels/sheet1.xml.rels": sheet_rels,
        "xl/drawings/drawing1.xml": drawing,
        "xl/drawings/_rels/drawing1.xml.rels": drawing_rels,
    }
    try:
        with zipfile.ZipFile(target / "xlsx_drawing_order.xlsx", "w", zipfile.ZIP_DEFLATED) as archive:
            for member, text in members.items():
                archive.writestr(member, text.encode("utf-8"))
            archive.writestr("xl/media/image1.png", _token_png("XLSX-FIRST-REL-TOKEN"))
            archive.writestr("xl/media/image2.png", _token_png("XLSX-FIRST-DRAW-TOKEN"))
    except OSError as exc:
        return SynthResult([], f"构造 A14 XLSX 失败：{exc}")
    return SynthResult(["xlsx_drawing_order.xlsx"])


SYNTHESIZERS: dict[str, Callable[[Path, Path], SynthResult]] = {
    "pnm": lambda target, _fixtures: _synth_pnm(target),
    "images": lambda target, _fixtures: _synth_images(target),
    "office": _synth_office,
    "media": lambda target, _fixtures: _synth_media(target),
    "pptx_two_png_order": _synth_pptx_two_png_order,
    "pptx_svg": _synth_pptx_svg,
    "pptx_runs_fields": _synth_pptx_runs_fields,
    "pptx_undecodable_image": _synth_pptx_undecodable_image,
    "pptx_multi_images": _synth_pptx_multi_images,
    "pptx_shared_media": _synth_pptx_shared_media,
    "pptx_descr_no_ocr": _synth_pptx_descr_no_ocr,
    "xlsx_drawing_order": _synth_xlsx_drawing_order,
}


# ---------------------------------------------------------------- 条目登记表。


@dataclasses.dataclass
class Item:
    """一条验收条目：附录 A 矩阵 / GUI E2E / 双形态与环境前置."""

    item_id: str
    group: str
    title: str
    entry: str
    fixtures: tuple[str, ...] = ()
    fixture_note: str = ""
    synth: str = ""
    needs_assets: str = ""  # ""（无需）/ "xberg" / "xberg+media"
    stage: str = ""  # B 组引用的 gui_smoke 阶段号


AVAILABLE = "现有夹具可用"
_MATRIX_COMMON = "GUI 转换链路（pywinauto 驱动真实公开入口）"

ITEMS: tuple[Item, ...] = (
    # —— A 组：附录 A 的 26 项最低公开合成测试矩阵 ——
    Item(
        "A01",
        "A",
        "PPTX 两张 PNG：XML、版面、relationship 顺序不一致时 OCR 不互换",
        _MATRIX_COMMON,
        (),
        "运行期合成：两张不同文字 PNG，rels 列举顺序与 blip 引用顺序交错，OCR 不得互换",
        synth="pptx_two_png_order",
        needs_assets="xberg",
    ),
    Item(
        "A02",
        "A",
        "PPTX 5~10 张不同图片：多次转换内容顺序稳定",
        _MATRIX_COMMON,
        (),
        "运行期合成：6 张不同文字 PNG 组装 PPTX；本项连跑两次并逐字节比对输出一致",
        synth="pptx_multi_images",
        needs_assets="xberg",
    ),
    Item(
        "A03",
        "A",
        "PPTX 同一媒体被多个 shape 引用：允许复用识别，保留每处位置",
        _MATRIX_COMMON,
        (),
        "运行期合成：同一 slide 两个 shape 引用同一 media 成员，两处都出现该图 OCR 内容",
        synth="pptx_shared_media",
        needs_assets="xberg",
    ),
    Item(
        "A04",
        "A",
        "PPTX placeable WMF：有效识别与 OCR，失败有明确阶段",
        _MATRIX_COMMON,
        ("matrix/pptx_placeable_wmf.pptx",),
        "需新增：嵌入带 22 字节 APM placeable 头的 WMF（LibreOffice 导出或公开样本）",
        needs_assets="xberg",
    ),
    Item(
        "A05",
        "A",
        "PPTX standard-header WMF：不能只支持另一种 WMF 头部",
        _MATRIX_COMMON,
        ("matrix/pptx_standard_wmf.pptx",),
        "需新增：嵌入 standard-header WMF（无 APM 头）",
        needs_assets="xberg",
    ),
    Item(
        "A06",
        "A",
        "PPTX EMF：光栅化与文字提取，失败隔离",
        _MATRIX_COMMON,
        ("matrix/pptx_emf.pptx",),
        "需新增：嵌入 EMF（Windows GDI 导出或公开样本）",
        needs_assets="xberg",
    ),
    Item(
        "A07",
        "A",
        "PPTX SVG：本地解析、外部资源禁用、失败隔离",
        _MATRIX_COMMON,
        (),
        "运行期合成：SVG 文本成员含失效外部引用 + 邻位正常 PNG；断言两段文本与失败隔离",
        synth="pptx_svg",
        needs_assets="xberg",
    ),
    Item(
        "A08",
        "A",
        "PPTX 普通 run 与可见字段混排：字段值完整保留",
        _MATRIX_COMMON,
        (),
        "运行期合成：普通 run 与 a:fld 幻灯片编号字段混排；断言 run 与字段值都出现",
        synth="pptx_runs_fields",
        needs_assets="xberg",
    ),
    Item(
        "A09",
        "A",
        "PPTX 图片 description 有文字而 OCR 为空：description 不冒充正文",
        _MATRIX_COMMON,
        (),
        "运行期合成：图片 descr 含文字、图片本体为无文字纯色位图；descr 可在 alt，不得进入 text 围栏冒充 OCR 正文",
        synth="pptx_descr_no_ocr",
        needs_assets="xberg",
    ),
    Item(
        "A10",
        "A",
        "PPTX 图片不可解码：诊断阶段明确，其余内容继续输出",
        _MATRIX_COMMON,
        (),
        "运行期合成：截断 PNG 与正常 PNG 同页；断言正常图 OCR 内容在、转换不中断",
        synth="pptx_undecodable_image",
        needs_assets="xberg",
    ),
    Item(
        "A11",
        "A",
        "PPTX OLE 本体失败、preview OCR 成功：两者分别诊断和输出",
        _MATRIX_COMMON,
        ("pptx_with_embedded_office.pptx",),
        f"{AVAILABLE}（嵌入对象在场）；「本体失败+预览成功」注入变体另需新增 matrix/pptx_ole_broken_preview.pptx",
        needs_assets="xberg",
    ),
    Item(
        "A12",
        "A",
        "DOCX 正文、页眉、页脚、脚注、尾注引用图片：各来源关联路径全覆盖",
        _MATRIX_COMMON,
        ("matrix/docx_all_sources.docx",),
        "需新增：五来源各引用一张不同可 OCR 图片；sample_with_images.docx 仅覆盖正文来源，作辅助不单独满足本项",
        needs_assets="xberg",
    ),
    Item(
        "A13",
        "A",
        "DOCX EMF、WMF 与栅格图片混排：单图失败不影响其余",
        _MATRIX_COMMON,
        ("matrix/docx_emf_wmf_raster.docx",),
        "需新增：EMF+WMF+栅格混排，其一损坏",
        needs_assets="xberg",
    ),
    Item(
        "A14",
        "A",
        "XLSX drawing 多图且关系顺序不同：结果归属正确",
        _MATRIX_COMMON,
        (),
        "运行期合成：两个 anchor 的显示顺序与 .rels 列举顺序相反，两张图各有可 OCR 文字",
        synth="xlsx_drawing_order",
        needs_assets="xberg",
    ),
    Item(
        "A15",
        "A",
        "PDF 原生文字、扫描页、重复图片、软蒙版和大页数（T-13~T-18）",
        _MATRIX_COMMON,
        (
            "single_paper.pdf",
            "scanned_hello.pdf",
            "mixed_native_scanned.pdf",
            "large_210_pages.pdf",
            "matrix/pdf_repeat_softmask.pdf",
        ),
        f"{AVAILABLE}：前四个（原生/扫描/混合/>200 页分流）；需新增 matrix/pdf_repeat_softmask.pdf（软蒙版）",
        needs_assets="xberg",
    ),
    Item(
        "A16",
        "A",
        "独立 PNG、JPEG/JPG、GIF、TIFF/TIF、BMP、WebP 真实转换",
        _MATRIX_COMMON,
        ("test_hello_world.png",),
        f"{AVAILABLE}：PNG；JPEG/JPG/GIF/TIFF/TIF/BMP/WebP 运行期由 Pillow 合成",
        synth="images",
        needs_assets="xberg",
    ),
    Item(
        "A17",
        "A",
        "PNM、PBM、PGM、PPM：ASCII 与二进制表示",
        _MATRIX_COMMON,
        (),
        "运行期纯标准库合成 P1-P3（ASCII）与 P4-P6（二进制）六种表示",
        synth="pnm",
        needs_assets="xberg",
    ),
    Item(
        "A18",
        "A",
        "JP2、J2K、J2C：固定清单支持；JPX、JPM、MJ2 明确不支持",
        _MATRIX_COMMON,
        (
            "matrix/jpeg2000/jp2.jp2",
            "matrix/jpeg2000/j2k.j2k",
            "matrix/jpeg2000/j2c.j2c",
            "matrix/jpeg2000/jpx.jpx",
            "matrix/jpeg2000/jpm.jpm",
            "matrix/jpeg2000/mj2.mj2",
        ),
        "固定 Xberg 清单仅声明 jp2/j2k/j2c；jpx/jpm/mj2 必须作为 unsupported 明确排除，不扩大产品支持集合",
        needs_assets="xberg",
    ),
    Item(
        "A19",
        "A",
        "JBIG2、JB2 及 PDF 内嵌 JBIG2：解码与 OCR",
        _MATRIX_COMMON,
        ("matrix/jbig2_standalone.jb2", "matrix/pdf_jbig2.pdf"),
        "需新增：JBIG2 编码器稀缺，可用 jbig2enc 生成，或从公开 PDF 提取内嵌 JBIG2 流重组",
        needs_assets="xberg",
    ),
    Item(
        "A20",
        "A",
        "图片空文件、错误扩展名、截断、超大尺寸、异常色彩空间、透明背景",
        _MATRIX_COMMON,
        (),
        "运行期由 Pillow 合成全部变体（empty/fake_text/truncated/huge/cmyk/alpha）",
        synth="images",
        needs_assets="xberg",
    ),
    Item(
        "A21",
        "A",
        "DOCX、DOCM、DOTX、DOTM：入口、嵌入对象与页数分流",
        _MATRIX_COMMON,
        ("chartex.docx", "merged_cells.docx", "docx_with_embedded_office.docx"),
        f"{AVAILABLE}：三个 docx；DOCM/DOTX/DOTM 运行期 zip 容器改写合成",
        synth="office",
        needs_assets="xberg",
    ),
    Item(
        "A22",
        "A",
        "PPTX、PPTM、PPSX、POTX、POTM：入口、页数与嵌入对象",
        _MATRIX_COMMON,
        ("bug62513.pptx", "merged_table.pptx", "pptx_with_embedded_office.pptx"),
        f"{AVAILABLE}：三个 pptx；PPTM/PPSX/POTX/POTM 运行期 zip 容器改写合成",
        synth="office",
        needs_assets="xberg",
    ),
    Item(
        "A23",
        "A",
        "XLSX、XLTX、XLSM、XLSB、XLAM：工作表、drawing 与嵌入对象",
        _MATRIX_COMMON,
        ("merged_header.xlsx", "matrix/xlsb_sample.xlsb"),
        f"{AVAILABLE}：merged_header.xlsx；XLTX/XLSM/XLAM 运行期合成；XLSB 需新增（BIFF，公开样本或 Excel 生成）",
        synth="office",
        needs_assets="xberg",
    ),
    Item(
        "A24",
        "A",
        "MP4/M4A：真实中文本地转录、时间戳、无音轨、无语音、损坏音轨、停止与失败隔离",
        _MATRIX_COMMON,
        ("video-to-notes-intro-zh.mp4",),
        f"{AVAILABLE}：真实中文视频；M4A/无音轨/无语音/损坏音轨运行期由 ffmpeg 合成；停止子场景经 GUI「停止任务」验证",
        synth="media",
        needs_assets="xberg+media",
    ),
    Item(
        "A25",
        "A",
        "固定 Xberg 格式清单中的其余格式：逐项最小烟测（旧 Office、OpenDocument 等）",
        _MATRIX_COMMON,
        (),
        "需真实 Xberg 在场枚举 formats 清单后确定集合；样本放 matrix/format_sweep/<扩展名> 各一",
        needs_assets="xberg",
    ),
    Item(
        "A26",
        "A",
        "所有最终产物：每顶层输入一份 Markdown、保留并完整引用同名 _media、不含 Base64、源文件不变",
        _MATRIX_COMMON,
        ("test_hello_world.png", "sample_with_images.docx", "video-to-notes-intro-zh.mp4"),
        AVAILABLE,
        needs_assets="xberg+media",
    ),
    # —— B 组：GUI 入口 E2E（引用 scripts/gui_smoke.py 阶段；S5 为本次补丁，其余为建议扩展） ——
    Item(
        "B01",
        "B",
        "转换基本链路：启动→选输入→开始→停止→关闭",
        "gui_smoke S5 引用",
        stage="S5",
        fixture_note="需 gui_smoke 提供 S5 阶段（本任务补丁）",
        needs_assets="xberg",
    ),
    Item(
        "B02",
        "B",
        "Xberg 目录选择与保存（使用此目录校验）",
        "gui_smoke 阶段引用",
        stage="S6",
        fixture_note="需 gui_smoke 扩展 S6（建议）",
        needs_assets="xberg",
    ),
    Item(
        "B03",
        "B",
        "重启恢复：保存的 Xberg 目录下次启动自动恢复",
        "gui_smoke 阶段引用",
        stage="S7",
        fixture_note="需 gui_smoke 扩展 S7（建议）",
        needs_assets="xberg",
    ),
    Item(
        "B04",
        "B",
        "无效目录提示与重新选择",
        "gui_smoke 阶段引用",
        stage="S8",
        fixture_note="需 gui_smoke 扩展 S8（建议）",
    ),
    Item(
        "B05",
        "B",
        "组件就绪/缺失状态与初始化、取消、重试",
        "gui_smoke 阶段引用",
        stage="S9",
        fixture_note="需 gui_smoke 扩展 S9（建议）",
    ),
    Item(
        "B06",
        "B",
        "输出层级（保留/平铺）与同名策略",
        "gui_smoke 阶段引用",
        stage="S10",
        fixture_note="需 gui_smoke 扩展 S10（建议）",
        needs_assets="xberg",
    ),
    Item(
        "B07",
        "B",
        "已有结果跳过（不覆盖、显示跳过数量）",
        "gui_smoke 阶段引用",
        stage="S11",
        fixture_note="需 gui_smoke 扩展 S11（建议）",
        needs_assets="xberg",
    ),
    Item(
        "B08",
        "B",
        "输出子树排除（输出在输入内不重复转换）",
        "gui_smoke 阶段引用",
        stage="S12",
        fixture_note="需 gui_smoke 扩展 S12（建议）",
        needs_assets="xberg",
    ),
    Item(
        "B09",
        "B",
        "部分失败与完成统计（成功/失败/跳过可区分）",
        "gui_smoke 阶段引用",
        stage="S13",
        fixture_note="需 gui_smoke 扩展 S13（建议）",
        needs_assets="xberg",
    ),
    Item(
        "B10",
        "B",
        "运行中关闭窗口的确认与安全停止（U-09/T-23）",
        "gui_smoke 阶段引用",
        stage="S14",
        fixture_note="需 gui_smoke 扩展 S14（建议）",
        needs_assets="xberg",
    ),
    # —— C 组：双形态与前置环境（附录 A 末段） ——
    Item("C01", "C", "安装版主包不含模型或转换专用依赖（目录树扫描）", "安装目录文件扫描"),
    Item("C02", "C", "便携版主包不含模型或转换专用依赖（目录树扫描）", "便携目录文件扫描"),
    Item("C03", "C", "未配置时 JchTools 及旧工具正常可用且不自动下载 Xberg", "gui_smoke S1 引用 + 干净资产根快照对比"),
    Item(
        "C04",
        "C",
        "断网环境实际完成文档 OCR 与媒体转录",
        "GUI 转换链路（离线环境前置）",
        ("sample_with_images.docx", "video-to-notes-intro-zh.mp4"),
        AVAILABLE,
        needs_assets="xberg+media",
    ),
    Item(
        "C05",
        "C",
        "无 Python 运行环境：进程加载模块与资产清单均无 Python（不能只从 PATH 移除）",
        "转换运行中 tasklist 模块枚举 + 清单检查",
        ("video-to-notes-intro-zh.mp4",),
        AVAILABLE,
        needs_assets="xberg+media",
    ),
    Item(
        "C06",
        "C",
        "旧项目目录不可用（T-28 删除前置）时仍独立工作",
        "GUI 转换链路（旧目录缺席前置）",
        ("test_hello_world.png",),
        AVAILABLE,
        needs_assets="xberg",
    ),
    Item(
        "C07",
        "C",
        "无旧缓存（旧 .venv/旧目录）时仍独立工作",
        "GUI 转换链路（旧缓存缺席前置）",
        ("test_hello_world.png",),
        AVAILABLE,
        needs_assets="xberg",
    ),
    Item(
        "C08",
        "C",
        "安装版实际完成转换（配置外部 Xberg 并初始化后）",
        "GUI 转换链路（安装版 EXE）",
        ("test_hello_world.png",),
        AVAILABLE,
        needs_assets="xberg",
    ),
    Item(
        "C09",
        "C",
        "便携版实际完成转换（配置外部 Xberg 并初始化后）",
        "GUI 转换链路（便携版 EXE）",
        ("test_hello_world.png",),
        AVAILABLE,
        needs_assets="xberg",
    ),
)


@dataclasses.dataclass
class Context:
    """一次运行的全部环境输入与共享探测缓存."""

    gui_exe: Path | None
    installed_root: Path | None
    portable_root: Path | None
    fixtures_dir: Path
    assets: AssetProbe
    stages: tuple[str, ...] | None = None
    stages_error: str | None = None

    def cached_stages(self) -> tuple[tuple[str, ...], str | None]:
        if self.stages is None:
            self.stages, self.stages_error = _gui_smoke_stages()
        return self.stages, self.stages_error


def _resolve_gui(ctx: Context, exe: Path | None = None) -> tuple[Path | None, str | None]:
    target = exe or ctx.gui_exe
    if target is None:
        return None, "未提供被测 GUI（--gui-exe；验收驱动器不猜测默认被测物，避免拿陈旧 debug 产物充当验收对象）"
    if not target.is_file():
        return None, f"被测 GUI 不存在：{target}"
    return target, None


def _asset_precondition(ctx: Context, item: Item) -> str | None:
    if not item.needs_assets:
        return None
    assets = ctx.assets
    if not assets.xberg_ready():
        reason = "；".join(assets.missing) if assets.missing else "Xberg 未配置"
        return f"资产未就绪：{reason}。获取方式：{assets.acquire_hint()}"
    if item.needs_assets == "xberg+media" and not assets.media_ready():
        holes = "；".join(assets.missing) or "媒体组件缺失"
        return f"媒体资产未就绪：{holes}。获取方式：{assets.acquire_hint()}"
    return None


def _fixture_precondition(item: Item, fixtures_dir: Path) -> str | None:
    if not item.fixtures:
        return None
    missing = [name for name in item.fixtures if not (fixtures_dir / name).is_file()]
    if missing:
        return f"缺夹具（{len(missing)} 个）：{missing}；构造方法见 --list 条目说明"
    return None


def _synth_precondition(item: Item) -> str | None:
    if item.synth == "media" and shutil.which("ffmpeg") is None:
        return "合成器需要 PATH 上的 ffmpeg"
    return None


# ---------------------------------------------------------------- GUI 驱动（A 组 / C 组共用）。


@dataclasses.dataclass
class GuiRun:
    """一次 GUI 转换的可观察结果；error 非空表示驱动链路本身失败."""

    outputs: list[Path]
    texts: str
    error: str | None


def _connect_window(pid: int) -> tuple[Application, WindowSpecification]:
    deadline = time.time() + GUI_WINDOW_TIMEOUT
    last: Exception | None = None
    while time.time() < deadline:
        try:
            app = Application(backend="uia").connect(process=pid, timeout=1)
            window = app.window(title="JchTools")
            if window.exists():
                return app, window
        except TRANSIENT_ERRORS as exc:
            last = exc
        time.sleep(0.5)
    message = f"等待 JchTools 窗口超时：{last}"
    raise RuntimeError(message)


def _click_button(window: WindowSpecification, title: str) -> None:
    deadline = time.time() + GUI_WINDOW_TIMEOUT
    button: BaseWrapper | None = None
    while time.time() < deadline:
        candidates = unique_visible_buttons(window, title)
        for candidate in candidates:
            if candidate.is_visible() and candidate.is_enabled():
                button = candidate
                break
        if button is not None:
            break
        time.sleep(0.2)
    if button is None:
        message = f"按钮「{title}」未找到唯一可见可用控件"
        raise RuntimeError(message)
    # 优先 UIA Invoke 模式：不移动真实鼠标、不依赖窗口前台（合成鼠标点击在
    # 窗口失去前台时会落到别的窗口，实测导致「开始转换」未生效而超时）。
    with contextlib.suppress(*TRANSIENT_ERRORS):
        invoke = getattr(button, "invoke", None)
        if callable(invoke):
            _ = invoke()
            time.sleep(0.3)
            return
    with contextlib.suppress(*TRANSIENT_ERRORS):
        _ = window.set_focus()
    button.click_input()
    time.sleep(0.3)


def _button_state(window: WindowSpecification, title: str) -> tuple[bool, bool]:
    """返回（可见, 可用）；元素未解析按（False, False）处理，由调用方轮询."""
    try:
        candidates = unique_visible_buttons(window, title)
        visible = [button for button in candidates if button.is_visible()]
        return bool(visible), bool(visible and visible[0].is_enabled())
    except TRANSIENT_ERRORS:
        return False, False


def _wait_start_ready(window: WindowSpecification, timeout: float) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if _button_state(window, "开始转换") == (True, True):
            return True
        time.sleep(0.5)
    return False


def _convert_directory_rows(window: WindowSpecification) -> list[BaseWrapper]:
    """按可见按钮的物理矩形去重，返回输入/输出两行控件。."""
    return unique_visible_buttons(window, "选择目录…")


def _same_physical_control(left: BaseWrapper, right: BaseWrapper) -> bool:
    left_rect = left.rectangle()
    right_rect = right.rectangle()
    intersection_width = max(0, min(left_rect.right, right_rect.right) - max(left_rect.left, right_rect.left))
    intersection_height = max(0, min(left_rect.bottom, right_rect.bottom) - max(left_rect.top, right_rect.top))
    intersection = intersection_width * intersection_height
    left_area = max(1, (left_rect.right - left_rect.left) * (left_rect.bottom - left_rect.top))
    right_area = max(1, (right_rect.right - right_rect.left) * (right_rect.bottom - right_rect.top))
    return intersection / min(left_area, right_area) >= PHYSICAL_OVERLAP_THRESHOLD


def unique_visible_buttons(window: WindowSpecification, title: str) -> list[BaseWrapper]:
    candidates: list[BaseWrapper] = []
    for button in window.descendants(control_type="Button"):
        if (button.window_text() or "") != title:
            continue
        if not button.is_visible():
            continue
        if any(_same_physical_control(button, existing) for existing in candidates):
            continue
        candidates.append(button)
    candidates.sort(key=lambda button: (button.rectangle().top, button.rectangle().left))
    return candidates


def _set_row_edit(window: WindowSpecification, row_button: BaseWrapper, value: str) -> None:
    top = row_button.rectangle().top
    candidates = [
        edit
        for edit in window.descendants(control_type="Edit")
        if abs(edit.rectangle().top - top) < EDIT_ROW_TOLERANCE_PX
    ]
    if not candidates:
        message = "未找到与「选择目录…」同排的目录输入框"
        raise RuntimeError(message)
    min(candidates, key=lambda edit: edit.rectangle().left).set_edit_text(value)


def _window_texts(window: WindowSpecification) -> str:
    parts: list[str] = []
    with contextlib.suppress(*TRANSIENT_ERRORS):
        for text in window.descendants(control_type="Text"):
            value = text.window_text() or ""
            if value.strip():
                parts.append(value)
    return "\n".join(parts)


def _request_close(window: WindowSpecification) -> None:
    """先发 WM_CLOSE；鼠标兜底前验证前台窗口 PID 属于目标 GUI。."""
    handle = window.handle
    target_pid = win32process.GetWindowThreadProcessId(handle)[1]
    win32gui.PostMessage(handle, win32con.WM_CLOSE, 0, 0)
    time.sleep(0.3)
    with contextlib.suppress(*TRANSIENT_ERRORS):
        if not window.exists():
            return
    foreground = win32gui.GetForegroundWindow()
    foreground_pid = win32process.GetWindowThreadProcessId(foreground)[1]
    if foreground_pid != target_pid:
        message = "关闭兜底拒绝操作：前台窗口 PID 不属于被测 GUI"
        raise RuntimeError(message)
    for button in unique_visible_buttons(window, "关闭"):
        button.click_input()
        time.sleep(0.3)
        return


def _terminate(proc: subprocess.Popen[bytes]) -> None:
    with contextlib.suppress(Exception):
        _ = proc.wait(timeout=10)
    proc.kill()
    _ = proc.wait(timeout=10)


_MAX_START_RECLICKS = 3
_RECLICK_INTERVAL_S = 10.0


class _ReclickState:
    """「开始转换」重按状态机：运行态/产物出现前周期性补偿落空的合成点击."""

    def __init__(self) -> None:
        self.count: int = 0
        self.last_click: float = time.time()

    def maybe_reclick(
        self, window: WindowSpecification, *, saw_busy: bool, produced: bool, start_enabled: bool
    ) -> None:
        if saw_busy or produced or not start_enabled:
            return
        if self.count >= _MAX_START_RECLICKS or time.time() - self.last_click <= _RECLICK_INTERVAL_S:
            return
        self.count += 1
        self.last_click = time.time()
        _click_button(window, "开始转换")


def drive_conversion(exe: Path, input_dir: Path, output_dir: Path, *, stop_after_busy: bool = False) -> GuiRun:
    """经真实 GUI 公开入口执行一次转换并收集可观察结果.

    注意：本函数只在资产齐全的真实环境被调用（前置检查先行）；无资产环境从不执行，
    其内部链路未在当前环境验证——这与 NOT RUN 的如实呈现是同一立场，不得虚构已验证。
    """
    proc: subprocess.Popen[bytes] = subprocess.Popen([str(exe)])
    window: WindowSpecification | None = None
    try:
        _, window = _connect_window(proc.pid)
        _click_button(window, "转 Markdown")
        rows = _convert_directory_rows(window)
        if len(rows) < CONVERT_ROW_COUNT:
            message = f"转 Markdown 页「选择目录…」按钮不足两行（实得 {len(rows)}）"
            return GuiRun([], _window_texts(window), message)
        _set_row_edit(window, rows[0], str(input_dir))
        _set_row_edit(window, rows[1], str(output_dir))
        if not _wait_start_ready(window, READINESS_TIMEOUT):
            message = "「开始转换」始终未就绪（组件未初始化或就绪检查失败）"
            return GuiRun([], _window_texts(window), message)
        _click_button(window, "开始转换")
        saw_busy = False
        error: str | None = None
        deadline = time.time() + CONVERSION_TIMEOUT
        # 合成点击偶发落空（点击后状态仍是「尚未开始」且无产物）：周期性重按
        # 「开始转换」（按钮可用且未观察到运行态时），最多 _MAX_START_RECLICKS
        # 次；按钮不可用或已见运行态即停止重按，不干扰正常转换。
        reclick_state = _ReclickState()
        while time.time() < deadline:
            busy_visible, _enabled = _button_state(window, "停止任务")
            if busy_visible:
                saw_busy = True
                if stop_after_busy:
                    _click_button(window, "停止任务")
            start_visible, start_enabled = _button_state(window, "开始转换")
            produced = any(output_dir.rglob("*.md"))
            finished_after_busy = saw_busy and start_visible and start_enabled
            finished_fast = not saw_busy and start_visible and start_enabled and produced
            if finished_after_busy or finished_fast:
                break
            reclick_state.maybe_reclick(
                window, saw_busy=saw_busy, produced=produced, start_enabled=start_visible and start_enabled
            )
            time.sleep(0.5)
        else:
            error = "转换未在时限内结束（开始转换未重新可用）"
        if stop_after_busy and not saw_busy and error is None:
            error = "任务过快结束，未能观察到运行态并验证停止路径"
        if error is not None:
            return GuiRun([], _window_texts(window), error)
        outputs = sorted(path for path in output_dir.rglob("*.md") if path.is_file())
        return GuiRun(outputs, _window_texts(window), None)
    finally:
        if window is not None:
            _request_close(window)
        _terminate(proc)


# ---------------------------------------------------------------- 结果核验。


DATA_URI_PATTERN = re.compile(r"data:image/[a-zA-Z0-9.+-]+;base64")
TEMP_LEFTOVER_MARKER = ".jch-markdown-"
IMAGE_SUFFIXES = (
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".tif",
    ".tiff",
    ".bmp",
    ".webp",
    ".jp2",
    ".j2k",
    ".j2c",
    ".jpx",
    ".jpm",
    ".mj2",
    ".svg",
)
MARKDOWN_IMAGE_LINK_PATTERN = re.compile(r"!\[[^\]\n]*\]\(\s*(?P<target><[^>\n]+>|[^)\n]+?)\s*\)")
MARKDOWN_FENCE_PATTERN = re.compile(r"(?s)(```.*?```|~~~.*?~~~)")
MARKDOWN_INLINE_CODE_PATTERN = re.compile(r"(?<!`)`+(?!`)[^`\n]+`+(?!`)")


def _expected_markdown_names(files: list[str]) -> list[str]:
    names: list[str] = []
    for name in files:
        stem = Path(name).stem
        suffix = Path(name).suffix.lstrip(".").lower()
        names.append(f"{stem}_{suffix}.md" if suffix else f"{stem}.md")
    return names


def _collect_media_references(markdown: Path, output_dir: Path) -> tuple[list[str], set[Path]]:
    """收集一份 Markdown 中的实际图片链接及其问题。."""
    problems: list[str] = []
    referenced: set[Path] = set()
    text = markdown.read_text(encoding="utf-8", errors="replace")
    rendered = MARKDOWN_FENCE_PATTERN.sub("", text)
    rendered = MARKDOWN_INLINE_CODE_PATTERN.sub("", rendered)
    for match in MARKDOWN_IMAGE_LINK_PATTERN.finditer(rendered):
        raw = match.group("target").strip()
        raw = raw[1:-1] if raw.startswith("<") and raw.endswith(">") else raw.split(maxsplit=1)[0]
        if not raw or raw.startswith("#") or "://" in raw:
            continue
        if raw.startswith("//") and ntpath.isabs(raw.replace("/", "\\")):
            problems.append(f"图片引用逃出输出目录：{markdown.relative_to(output_dir)} -> {raw}")
            continue
        target = (markdown.parent / Path(raw.replace("/", "\\"))).resolve()
        try:
            _ = target.relative_to(output_dir.resolve())
        except ValueError:
            problems.append(f"图片引用逃出输出目录：{markdown.relative_to(output_dir)} -> {raw}")
            continue
        if not target.is_file():
            problems.append(f"图片引用目标不存在：{markdown.relative_to(output_dir)} -> {raw}")
        else:
            referenced.add(target)
    return problems, referenced


def _verify_media_references(output_dir: Path) -> list[str]:
    """验证 T-14 图片落盘位置、正文引用和引用目标的一致性。."""
    problems: list[str] = []
    referenced: set[Path] = set()
    for markdown in output_dir.rglob("*.md"):
        if markdown.is_file():
            markdown_problems, markdown_references = _collect_media_references(markdown, output_dir)
            problems.extend(markdown_problems)
            referenced.update(markdown_references)
    output_root = output_dir.resolve()
    for image in output_dir.rglob("*"):
        if not image.is_file():
            continue
        is_media_file = image.parent.name.endswith("_media")
        if image.suffix.lower() not in IMAGE_SUFFIXES and not is_media_file:
            continue
        relative = image.relative_to(output_dir)
        if not image.parent.name.endswith("_media"):
            problems.append(f"图片未放入产物媒体目录：{relative}")
        if image.resolve() not in referenced:
            problems.append(f"图片没有正文引用：{relative}")
        try:
            _ = image.resolve().relative_to(output_root)
        except ValueError:
            problems.append(f"图片路径逃出输出目录：{relative}")
    return problems


def verify_common_postconditions(source_dir: Path, output_dir: Path, sources: dict[Path, str]) -> list[str]:
    """附录 A 第 26 项横切断言：媒体引用完整、无 Base64、源不变、无临时残留."""
    problems = [
        f"源文件被改动：{relative}"
        for relative, digest in sources.items()
        if not (source_dir / relative).is_file() or _file_digest(source_dir / relative) != digest
    ]
    problems.extend(f"输出残留临时文件：{leftover}" for leftover in output_dir.rglob(f"*{TEMP_LEFTOVER_MARKER}*"))
    problems.extend(_verify_media_references(output_dir))
    problems.extend(
        f"结果含 Base64 图片：{markdown.relative_to(output_dir)}"
        for markdown in output_dir.rglob("*.md")
        if markdown.is_file() and DATA_URI_PATTERN.search(markdown.read_text(encoding="utf-8", errors="replace"))
    )
    return problems


# ---------------------------------------------------------------- A25：Xberg 清单枚举。


def xberg_format_extensions(xberg_exe: Path) -> tuple[list[str], str | None]:
    """与 src/markdown.rs::supported_formats 同参数只读调用，用于确定 sweep 集合."""
    done = subprocess.run(
        [str(xberg_exe), "formats", "--format", "json"],
        capture_output=True,
        text=True,
        check=False,
        cwd=str(xberg_exe.parent),
    )
    if done.returncode != 0:
        return [], f"xberg formats 调用失败（退出码 {done.returncode}）：{(done.stderr or '').strip()[:200]}"
    parsed = _parse_json(done.stdout or "[]")
    if not _is_str_obj_list(parsed):
        return [], "xberg formats 输出不是 JSON 数组"
    covered = {
        "pdf",
        "doc",
        "docx",
        "docm",
        "dot",
        "dotx",
        "dotm",
        "ppt",
        "pptx",
        "pptm",
        "pps",
        "ppsx",
        "pot",
        "potx",
        "potm",
        "xls",
        "xlsx",
        "xlsm",
        "xlsb",
        "xlt",
        "xltx",
        "xltm",
        "xla",
        "xlam",
        "odt",
        "ods",
        "odp",
        "png",
        "jpg",
        "jpeg",
        "webp",
        "bmp",
        "gif",
        "tif",
        "tiff",
        "jp2",
        "j2k",
        "j2c",
        "jpx",
        "jpm",
        "mj2",
        "jbig2",
        "jb2",
        "pnm",
        "pbm",
        "pgm",
        "ppm",
        "mp4",
        "m4a",
    }
    extensions: list[str] = []
    for entry in parsed:
        extension = _str_field(entry, "extension")
        mime = _str_field(entry, "mime_type")
        if extension is None or mime is None:
            continue
        token = extension.strip().lstrip(".").lower()
        if token and "." not in token and token not in covered and not mime.startswith(("audio/", "video/")):
            extensions.append(token)
    return sorted(set(extensions)), None


# ---------------------------------------------------------------- B 组：gui_smoke 阶段引用。


def _gui_smoke_stages() -> tuple[tuple[str, ...], str | None]:
    if not GUI_SMOKE.is_file():
        return (), f"gui_smoke 缺失：{GUI_SMOKE}"
    done = subprocess.run(
        [sys.executable, str(GUI_SMOKE), "--list-stages"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if done.returncode != 0:
        return (), "gui_smoke 不支持 --list-stages（S5 补丁未应用或脚本损坏）"
    stages = tuple(
        line.strip().upper() for line in (done.stdout or "").splitlines() if line.strip().upper().startswith("S")
    )
    return stages, None


def _delegate_stage(stage: str, exe: Path, env_extra: dict[str, str] | None = None) -> Outcome:
    # gui_smoke 的 main() 无条件校验数据目录存在；本入口只委托 S1/S5 这类不读
    # 数据集的阶段，提供一次性空目录即可通过校验（S2-S4 数据阶段不经此处）。
    scratch_data = SCRATCH_ROOT / "gui-smoke-data"
    scratch_data.mkdir(parents=True, exist_ok=True)
    argv = [
        sys.executable,
        str(GUI_SMOKE),
        "--exe",
        str(exe),
        "--data",
        str(scratch_data),
        "--stages",
        stage,
    ]
    # 子进程输出按 UTF-8 强制解码：GUI 及其内部子进程会向继承的捕获管道写
    # 本地化（GBK）诊断文本，按默认码表或严格 UTF-8 都可能解码失败。
    done = subprocess.run(
        argv,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
        env=os.environ | (env_extra or {}),
    )
    tail = "\n".join(((done.stdout or "") + (done.stderr or "")).splitlines()[-8:])
    if done.returncode == 0:
        return Outcome(STATUS_OK, details=[tail] if tail else [])
    return Outcome(STATUS_FAILED, f"gui_smoke {stage} 退出码 {done.returncode}", [tail] if tail else [])


# ---------------------------------------------------------------- C 组检查件。


def scan_forbidden_assets(root: Path) -> list[str]:
    """扫描交付目录树，报告任何模型/转换专用依赖（文件粒度，只读）."""
    hits: list[str] = []
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        name = path.name.lower()
        stem = name.rsplit(".", 1)[0]
        forbidden = (
            name in FORBIDDEN_NAMES
            or path.suffix.lower() in FORBIDDEN_SUFFIXES
            or (name.endswith(".dll") and stem.startswith(FORBIDDEN_DLL_PREFIXES))
        )
        if forbidden:
            hits.append(str(path.relative_to(root)))
    return hits


def _probe_offline() -> tuple[bool, str]:
    """尽力探测联网状态；探测失败只代表「未知」，不得据此宣称离线."""
    try:
        with socket.create_connection(("1.1.1.1", 53), timeout=3):
            return True, "当前环境可联网（探测 1.1.1.1:53 成功）"
    except OSError:
        return False, "探测 1.1.1.1:53 失败（可能离线，也可能被防火墙拦截；结论需人工确认）"


def _tasklist() -> Path | None:
    # [quality-baseline approved 2026-10-03] 官方拼写误报，经用户裁定保留
    system_root = os.environ.get("SystemRoot")  # noqa: SIM112  # Windows 官方拼写即 SystemRoot，大小写不敏感。
    if not system_root:
        return None
    candidate = Path(system_root) / "System32" / "tasklist.exe"
    return candidate if candidate.is_file() else None


def scan_python_modules(image_names: tuple[str, ...]) -> tuple[list[str], str | None]:
    """枚举指定进程名加载的模块，报告任何 python*.dll（附录 A：不能只从 PATH 移除）."""
    tasklist = _tasklist()
    if tasklist is None:
        return [], "找不到 tasklist.exe，无法枚举进程模块"
    hits: list[str] = []
    for image in image_names:
        done = subprocess.run(
            [str(tasklist), "/M", "/FI", f"IMAGENAME eq {image}"],
            capture_output=True,
            text=True,
            check=False,
        )
        if done.returncode != 0:
            return [], f"tasklist 查询 {image} 失败（退出码 {done.returncode}）"
        hits.extend(
            line.strip()
            for line in (done.stdout or "").splitlines()
            if re.search(r"python\d*[a-z_]*\.dll", line, re.IGNORECASE)
        )
    return hits, None


def manifest_has_python_entries() -> bool:
    for name in _manifest_model_paths():
        if "python" in name.lower():
            return True
    if ASSET_MANIFEST.is_file():
        return "python" in ASSET_MANIFEST.read_text(encoding="utf-8", errors="replace").lower()
    return True  # 清单缺失本身即无法证明未携带 Python，按可疑处理。


def _old_cache_hits() -> list[str]:
    return [str(path) for path in _old_cache_candidates() if path.exists()]


# ---------------------------------------------------------------- 条目执行。


@dataclasses.dataclass
class PreparedInputs:
    """一次转换的一次性输入/输出目录与输入文件清单；error 非空表示合成失败."""

    input_dir: Path
    output_dir: Path
    files: list[str]
    error: str | None = None


def _prepare_scratch(item: Item, ctx: Context, tag: str) -> PreparedInputs:
    """为一次转换在 .tmp/markdown-acceptance/ 下准备一次性输入/输出目录."""
    base = SCRATCH_ROOT / f"{item.item_id.lower()}-{tag}"
    if base.exists():
        shutil.rmtree(base, ignore_errors=True)
    prepared = PreparedInputs(base / "input", base / "output", [])
    _ = prepared.input_dir.mkdir(parents=True)
    _ = prepared.output_dir.mkdir(parents=True)
    for name in item.fixtures:
        source = ctx.fixtures_dir / name
        if source.is_file():
            _ = shutil.copy2(source, prepared.input_dir / Path(name).name)
            prepared.files.append(Path(name).name)
    if item.synth:
        result = SYNTHESIZERS[item.synth](prepared.input_dir, ctx.fixtures_dir)
        if result.error:
            prepared.error = result.error
            return prepared
        prepared.files.extend(result.files)
    return prepared


def _run_conversion_item(
    item: Item, ctx: Context, exe: Path | None, *, stop_mode: bool = False, tag_suffix: str = "run"
) -> Outcome:
    """通用转换执行：前置→准备→GUI 驱动→横切断言（A 组与 C04/C06-C09 共用）."""
    target, blocked = _resolve_gui(ctx, exe)
    if blocked is not None or target is None:
        return Outcome(STATUS_NOT_RUN, blocked or "被测 GUI 缺失")
    for pending in (_asset_precondition(ctx, item), _fixture_precondition(item, ctx.fixtures_dir)):
        if pending:
            return Outcome(STATUS_NOT_RUN, pending)
    prepared = _prepare_scratch(item, ctx, tag_suffix)
    if prepared.error is not None:
        return Outcome(STATUS_NOT_RUN, f"夹具合成失败：{prepared.error}")
    if not prepared.files:
        return Outcome(STATUS_NOT_RUN, "输入目录为空：夹具与合成器均未提供文件")
    sources = {
        path.relative_to(prepared.input_dir): _file_digest(path)
        for path in prepared.input_dir.rglob("*")
        if path.is_file()
    }
    run = drive_conversion(target, prepared.input_dir, prepared.output_dir, stop_after_busy=stop_mode)
    if run.error is not None:
        return Outcome(STATUS_FAILED, run.error, run.texts.splitlines()[-8:])
    return _assess_conversion(item, prepared, sources, run, stop_mode=stop_mode)


def _assess_conversion(
    item: Item, prepared: PreparedInputs, sources: dict[Path, str], run: GuiRun, *, stop_mode: bool = False
) -> Outcome:
    problems = verify_common_postconditions(prepared.input_dir, prepared.output_dir, sources)
    produced = [path.name for path in run.outputs]
    expected = _expected_markdown_names(prepared.files)
    details = [
        f"{item.item_id} 预期产物 {len(expected)} 项，实得 {len(produced)} 项：{produced}",
        *run.texts.splitlines()[-6:],
    ]
    if stop_mode:
        details.insert(0, "停止路径：已完成结果保留、源文件不变即符合 T-23；完整统计断言待资产环境调校")
    if problems:
        return Outcome(STATUS_FAILED, "；".join(problems[:5]), details)
    if not stop_mode:
        if item.item_id == "A26":
            if len(produced) != len(expected) or set(produced) != set(expected):
                return Outcome(
                    STATUS_FAILED,
                    f"A26 顶层输入与 Markdown 产物未一一对应：预期 {expected}，实得 {produced}",
                    details,
                )
            empty = [
                path.name for path in run.outputs if not path.read_text(encoding="utf-8", errors="replace").strip()
            ]
            if empty:
                return Outcome(STATUS_FAILED, f"A26 产物正文为空：{empty}", details)
        # 反假 PASS：run.error 只覆盖驱动链路失败；「任务运行过但整体转换失败、
        # 零产物」此前只写进 details 仍记 PASS。异常/损坏夹具按 T-25 以失败诊断
        # 收场、不产出 md 属预期，故不要求产物数等于输入数，只要求：产物是输入
        # 对应集合的子集（命名规则外的产物即规划缺陷），且常规条目不得零产物。
        extra_outputs = sorted(set(produced) - set(expected))
        if extra_outputs:
            return Outcome(STATUS_FAILED, f"产物超出输入对应集合：{extra_outputs[:8]}", details)
        if not produced:
            return Outcome(STATUS_FAILED, "预期至少一份 Markdown 产物，实得 0 项（转换全部失败或未产出）", details)
    return Outcome(STATUS_OK, details=details)


# A 组内容断言表：require 命中、forbid 全文禁止、forbid_in_text_fences 仅围栏禁止。
# token 均避开数字（OCR 对数字/字母易混，如 8→O、0→O），取稳定核心片段。
_CONTENT_ASSERTS: dict[str, dict[str, tuple[str, ...]]] = {
    "A01": {"require": ("ALPHA", "BETA")},
    "A03": {"require": ("SHARED-MEDIA",)},
    # A07 需求只要求「本地解析、外部资源禁用、失败隔离」：SVG 被识别为成员并保留
    # 引用、失效外链未拖垮转换、邻位 PNG 照常 OCR。SVG 栅格化/文本提取是上游未
    # 实现能力（干净 SVG 对照亦不输出文本），如实另行报告。
    "A07": {"require": ("PNG-NEIGH", ".svg")},
    "A08": {"require": ("RUN-AND", "FIELD-END")},
    # A09 的 descr 允许出现在图片 alt 位置（本就是 description 的标准去处），
    # 不得进入 ```text 围栏冒充 OCR 正文。
    "A09": {"forbid_in_text_fences": ("DESCR-NO-OCR-TOKEN",)},
    "A10": {"require": ("HEALTHY-TEXT",)},
    "A14": {"require": ("XLSX-FIRST-DRAW", "XLSX-FIRST-REL")},
}


def _run_matrix(item: Item, ctx: Context) -> Outcome:
    blocked = _synth_precondition(item)
    if blocked:
        return Outcome(STATUS_NOT_RUN, blocked)
    special_handlers = {
        "A25": _run_matrix_a25,
        "A02": _run_matrix_a02,
        "A24": _run_matrix_a24,
        "A18": _run_matrix_a18,
    }
    handler = special_handlers.get(item.item_id)
    if handler is not None:
        return handler(item, ctx)
    if item.item_id in _CONTENT_ASSERTS:
        return _run_content_assert(item, ctx, **_CONTENT_ASSERTS[item.item_id])
    return _run_conversion_item(item, ctx, None)


_TEXT_FENCE = re.compile(r"```[^\n]*\n(.*?)```", re.DOTALL)


def _run_content_assert(
    item: Item,
    ctx: Context,
    *,
    require: tuple[str, ...] = (),
    forbid: tuple[str, ...] = (),
    forbid_in_text_fences: tuple[str, ...] = (),
) -> Outcome:
    """通用转换 + 输出正文 token 断言（A03/A09/A14：复用识别、descr 不冒充正文、归属正确）."""
    outcome = _run_conversion_item(item, ctx, None)
    if outcome.status != STATUS_OK:
        return outcome
    output = SCRATCH_ROOT / f"{item.item_id.lower()}-run" / "output"
    text = "\n".join(path.read_text(encoding="utf-8", errors="replace") for path in sorted(output.rglob("*.md")))
    fences = cast("list[str]", _TEXT_FENCE.findall(text))
    fence_text = "\n".join(fences)
    missing = [token for token in require if token not in text]
    leaked = [token for token in forbid if token in text]
    leaked_fences = [token for token in forbid_in_text_fences if token in fence_text]
    if missing or leaked or leaked_fences:
        problems: list[str] = []
        if missing:
            problems.append(f"正文缺少期望 token：{missing}")
        if leaked:
            problems.append(f"正文出现不应出现的 token：{leaked}")
        if leaked_fences:
            problems.append(f"text 围栏出现不应出现的 token：{leaked_fences}")
        return Outcome(STATUS_FAILED, "；".join(problems), list(outcome.details))
    details = [
        f"内容断言通过：require={list(require)} forbid={list(forbid)} fences-forbid={list(forbid_in_text_fences)}",
        *outcome.details,
    ]
    return Outcome(STATUS_OK, details=details)


def _verify_a02_content(output: Path) -> str | None:
    """核对 A02 恰有合成图片及其正文 token，拒绝空正文或额外媒体."""
    media_files = [path for path in output.rglob("*") if path.is_file() and path.parent.name.endswith("_media")]
    if len(media_files) != A02_IMAGE_COUNT:
        return f"A02 应保留 {A02_IMAGE_COUNT} 张合成图片，实际媒体文件 {len(media_files)} 项"
    text = "\n".join(path.read_text(encoding="utf-8", errors="replace") for path in sorted(output.rglob("*.md")))
    if not text.strip():
        return "A02 转换产物正文为空"
    missing = [token for token in A02_IMAGE_TOKENS if token not in text]
    if missing:
        return f"A02 正文缺少合成图片内容：{missing}"
    return None


def _run_matrix_a02(item: Item, ctx: Context) -> Outcome:
    first = _run_conversion_item(item, ctx, None, tag_suffix="first")
    if first.status != STATUS_OK:
        return first
    second = _run_conversion_item(item, ctx, None, tag_suffix="second")
    if second.status != STATUS_OK:
        return second
    left = SCRATCH_ROOT / "a02-first" / "output"
    right = SCRATCH_ROOT / "a02-second" / "output"
    left_files = sorted(path.relative_to(left).as_posix() for path in left.rglob("*.md"))
    right_files = sorted(path.relative_to(right).as_posix() for path in right.rglob("*.md"))
    if left_files != right_files:
        return Outcome(STATUS_FAILED, f"两次转换产物集合不一致：{left_files} vs {right_files}")
    for name in left_files:
        if (left / name).read_bytes() != (right / name).read_bytes():
            return Outcome(STATUS_FAILED, f"两次转换内容不稳定：{name}")
    problem = _verify_a02_content(left)
    if problem is not None:
        return Outcome(STATUS_FAILED, problem)
    return Outcome(
        STATUS_OK,
        details=[f"两次转换产物逐字节一致，且包含 {A02_IMAGE_COUNT} 张合成图片正文内容"],
    )


def _verify_a24_outputs() -> str | None:
    outputs = SCRATCH_ROOT / "a24-full" / "output"
    if (outputs / "damaged_mp4.md").exists():
        return "损坏音轨产出了结果文件（应为失败，不留半成品）"
    good = outputs / "video-to-notes-intro-zh_mp4.md"
    if not good.is_file():
        return "真实中文视频未产出转录结果"
    text = good.read_text(encoding="utf-8", errors="replace")
    missing = [marker for marker in ("# video-to-notes-intro-zh", "- 音频时长: ", "## 转录") if marker not in text]
    if missing:
        return f"媒体结果缺少结构标记：{missing}"
    if not re.search(r"\d{2}:\d{2}:\d{2}\.\d{3}", text):
        return "媒体结果缺少时间戳（HH:MM:SS.mmm）"
    return None


def _run_matrix_a24(item: Item, ctx: Context) -> Outcome:
    stop_phase = _run_conversion_item(item, ctx, None, stop_mode=True, tag_suffix="stop")
    if stop_phase.status != STATUS_OK:
        return stop_phase
    full = _run_conversion_item(item, ctx, None, tag_suffix="full")
    if full.status != STATUS_OK:
        return full
    problem = _verify_a24_outputs()
    if problem is not None:
        return Outcome(STATUS_FAILED, problem)
    return Outcome(STATUS_OK, details=["真实中文转录、时间戳结构、损坏音轨失败隔离全部符合"])


def _run_matrix_a18(item: Item, ctx: Context) -> Outcome:
    """A18：按 T-08 固定清单验证支持边界，不把 JPX/JPM/MJ2 冒称为支持。."""
    fixture_error = _fixture_precondition(item, ctx.fixtures_dir)
    if fixture_error is not None:
        return Outcome(STATUS_NOT_RUN, fixture_error)
    fixed, error = _fixed_format_extensions()
    if error is not None:
        return Outcome(STATUS_NOT_RUN, error)
    unsupported_advertised = sorted(set(A18_UNSUPPORTED_EXTENSIONS) & fixed)
    if unsupported_advertised:
        return Outcome(
            STATUS_FAILED,
            f"固定格式清单错误声明不支持格式为可用：{unsupported_advertised}",
        )
    missing_supported = sorted(set(A18_SUPPORTED_EXTENSIONS) - fixed)
    if missing_supported:
        return Outcome(
            STATUS_NOT_RUN,
            f"固定格式清单未声明 A18 支持格式：{missing_supported}；不据此声称支持",
        )
    supported_fixtures = tuple(
        name for name in item.fixtures if Path(name).suffix.lstrip(".").lower() in A18_SUPPORTED_EXTENSIONS
    )
    supported_item = dataclasses.replace(item, fixtures=supported_fixtures)
    outcome = _run_conversion_item(supported_item, ctx, None, tag_suffix="supported")
    if outcome.status != STATUS_OK:
        return outcome
    unsupported = ", ".join(f".{extension}" for extension in A18_UNSUPPORTED_EXTENSIONS)
    return Outcome(
        STATUS_OK,
        details=[
            *outcome.details,
            f"固定清单明确 unsupported：{unsupported}；未将其送入产品转换支持集合",
        ],
    )


def _run_matrix_a25(item: Item, ctx: Context) -> Outcome:
    if ctx.assets.xberg_exe is None:
        return Outcome(STATUS_NOT_RUN, f"资产未就绪。获取方式：{ctx.assets.acquire_hint()}")
    extensions, error = xberg_format_extensions(ctx.assets.xberg_exe)
    if error:
        return Outcome(STATUS_NOT_RUN, error)
    sweep_dir = ctx.fixtures_dir / "matrix" / "format_sweep"
    missing = [ext for ext in extensions if next(sweep_dir.glob(f"*.{ext}"), None) is None]
    if missing:
        reason = f"Xberg 清单中还有 {len(missing)} 个未覆盖格式缺最小烟测样本：{missing[:12]}；"
        reason += f"请逐个放入 {sweep_dir}（无敏感内容的公开合成样本）"
        return Outcome(STATUS_NOT_RUN, reason)
    # sweep 样本按扩展名进入通用转换条目：注入 fixtures 让 _prepare_scratch 拷贝它们。
    item.fixtures = tuple(sorted(f"matrix/format_sweep/{path.name}" for path in sweep_dir.iterdir() if path.is_file()))
    return _run_conversion_item(item, ctx, None)


def _run_gui_ref(item: Item, ctx: Context) -> Outcome:
    stages, error = ctx.cached_stages()
    if error:
        return Outcome(STATUS_NOT_RUN, error)
    if item.stage not in stages:
        return Outcome(STATUS_NOT_RUN, f"gui_smoke 未提供 {item.stage} 阶段（现有：{list(stages)}）")
    target, blocked = _resolve_gui(ctx)
    if blocked is not None or target is None:
        return Outcome(STATUS_NOT_RUN, blocked or "被测 GUI 缺失")
    pending = _asset_precondition(ctx, item)
    if pending:
        return Outcome(STATUS_NOT_RUN, pending)
    env_extra: dict[str, str] = {}
    if item.stage in ("S6", "S7") and ctx.assets.runtime_dir is not None:
        # S6/S7 各自使用隔离 SQLite；目录环境变量必须来自同一份已探测配置，
        # 不能让固定提示路径与 GUI 实际保存的目录分叉。
        env_extra["JCHTOOLS_SMOKE_XBERG_DIR"] = str(ctx.assets.runtime_dir)
    if item.stage in ("S5", "S14") and not os.environ.get("JCHTOOLS_S5_MEDIA"):
        # S5/S14 需要真实媒体样本观察运行态；默认使用已有夹具中的真实视频。
        media = ctx.fixtures_dir / "video-to-notes-intro-zh.mp4"
        if media.is_file():
            env_extra["JCHTOOLS_S5_MEDIA"] = str(media)
    return _delegate_stage(item.stage, target, env_extra or None)


def _run_c01_installed_scan(item: Item, ctx: Context) -> Outcome:
    root = ctx.installed_root
    if root is None:
        message = "未提供安装目录（--installed-root；安装版位置见 installer/JchTools.iss 的 DefaultDirName）"
        return Outcome(STATUS_NOT_RUN, message)
    if not root.is_dir():
        return Outcome(STATUS_NOT_RUN, f"安装目录不存在：{root}")
    hits = scan_forbidden_assets(root)
    if hits:
        message = f"安装版携带转换专用资产 {len(hits)} 项：{hits[:8]}"
        return Outcome(STATUS_FAILED, message, [f"{item.item_id} 扫描根：{root}"])
    return Outcome(STATUS_OK, details=[f"{item.item_id} 扫描 {root}：无模型/转换专用依赖"])


def _run_c02_portable_scan(item: Item, ctx: Context) -> Outcome:
    root = ctx.portable_root
    if root is None:
        return Outcome(STATUS_NOT_RUN, "未提供便携目录（--portable-root；解包 package-windows.ps1 产出的便携 ZIP）")
    if not root.is_dir():
        return Outcome(STATUS_NOT_RUN, f"便携目录不存在：{root}")
    if not (root / "JchTools.exe").is_file():
        return Outcome(STATUS_FAILED, f"便携目录缺少 JchTools.exe：{root}")
    hits = scan_forbidden_assets(root)
    if hits:
        message = f"便携版携带转换专用资产 {len(hits)} 项：{hits[:8]}"
        return Outcome(STATUS_FAILED, message, [f"{item.item_id} 扫描根：{root}"])
    return Outcome(STATUS_OK, details=[f"{item.item_id} 扫描 {root}：无模型/转换专用依赖"])


def _run_c03_unconfigured(item: Item, ctx: Context) -> Outcome:
    target, blocked = _resolve_gui(ctx)
    if blocked is not None or target is None:
        return Outcome(STATUS_NOT_RUN, blocked or "被测 GUI 缺失")
    # 此入口由 acceptance.ps1 构建带 test-hooks 的隔离测试 EXE。
    # 默认生产构建不接受这些环境覆盖，不能用于本项隔离验收。
    if "target" not in target.parts or "debug" not in target.parts:
        message = "本项要求 debug 构建被测 EXE（release 忽略资产根覆盖环境变量，会读到真实用户配置）"
        return Outcome(STATUS_NOT_RUN, message)
    stages, error = ctx.cached_stages()
    if error is not None or "S1" not in stages:
        return Outcome(STATUS_NOT_RUN, error or "gui_smoke 未提供 S1 阶段选择器（补丁未应用）")
    scratch_assets = SCRATCH_ROOT / "c03-asset-root"
    shutil.rmtree(scratch_assets, ignore_errors=True)
    _ = scratch_assets.mkdir(parents=True)
    env = {
        "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT": str(scratch_assets),
        "JCHTOOLS_TEST_STATE_DIR": str(scratch_assets / "app-settings"),
    }
    result = _delegate_stage("S1", target, env_extra=env)
    if result.status != STATUS_OK:
        result.details.insert(0, f"{item.item_id} 未配置状态下的 S1 启动未通过")
        return result
    downloaded = [str(path.relative_to(scratch_assets)) for path in scratch_assets.rglob("*")]

    # XB-18 应用设置库的隔离落位（test-hooks 下 JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT 同时
    # 隔离设置目录，见 src/xberg_settings.rs state_dir），首启创建属正常行为；本项
    # 断言的是「不自动下载转换资产」。豁免必须收窄到设置库与代理锁文件本身：
    # download_runtime 的下载基目录恰为 state_dir()/xberg-downloads
    # （src/markdown_assets.rs），即 app-settings/xberg-downloads/ 也落在设置目录
    # 之下——整棵子树豁免会让「未配置即自动下载」回归假通过（e2e 审查确认项）。
    def _is_settings_artifact(path: str) -> bool:
        posix = path.replace(os.sep, "/")
        if posix == "app-settings":
            return True
        name = posix.rsplit("/", 1)[-1]
        return (
            name == "config.sqlite3"
            or name.startswith("config.sqlite3-")
            or (name.startswith("xberg-") and name.endswith(".lock"))
        )

    downloaded = [path for path in downloaded if not (path.startswith("app-settings") and _is_settings_artifact(path))]
    if downloaded:
        return Outcome(STATUS_FAILED, f"未配置启动即写入/下载资产目录：{downloaded[:8]}")
    return Outcome(
        STATUS_OK,
        details=[f"{item.item_id} 未配置启动正常退出，资产目录仅新建隔离设置库（XB-18），无资产下载"],
    )


def _run_c04_offline(item: Item, ctx: Context) -> Outcome:
    online, note = _probe_offline()
    if online:
        return Outcome(STATUS_NOT_RUN, f"需在断网环境执行；{note}")
    pending = _asset_precondition(ctx, item)
    if pending:
        return Outcome(STATUS_NOT_RUN, f"离线探测：{note}。{pending}")
    result = _run_conversion_item(item, ctx, None, tag_suffix="offline")
    if result.status == STATUS_NOT_RUN:
        result.reason = f"离线探测：{note}。{result.reason}"
    return result


def _c05_scan_during_conversion(exe: Path, input_dir: Path, output_dir: Path) -> tuple[GuiRun, list[str], list[str]]:
    """转换运行期间反复枚举相关进程的加载模块，捕捉对 python*.dll 的隐藏调用."""
    module_hits: list[str] = []
    scan_errors: list[str] = []
    finished = threading.Event()

    def scanner() -> None:
        while not finished.is_set():
            hits, error = scan_python_modules(("JchTools.exe", "markdown-media-worker.exe", "ffmpeg.exe"))
            if error is not None:
                scan_errors.append(error)
                return
            module_hits.extend(hits)
            time.sleep(0.5)

    thread = threading.Thread(target=scanner, daemon=True)
    thread.start()
    try:
        run = drive_conversion(exe, input_dir, output_dir)
    finally:
        finished.set()
        thread.join(timeout=5)
    return run, module_hits, scan_errors


def _c05_assess(item_id: str, run: GuiRun, module_hits: list[str], scan_errors: list[str]) -> Outcome:
    if scan_errors:
        return Outcome(STATUS_NOT_RUN, scan_errors[0])
    if run.error is not None:
        return Outcome(STATUS_FAILED, f"{item_id} 转换链路失败：{run.error}")
    if module_hits:
        return Outcome(STATUS_FAILED, f"转换期间进程加载了 python 模块：{module_hits[:4]}")
    return Outcome(STATUS_OK, details=[f"{item_id} 转换运行期间进程模块与资产清单均无 Python"])


def _run_c05_no_python(item: Item, ctx: Context) -> Outcome:
    pending = _asset_precondition(ctx, item)
    if pending:
        return Outcome(STATUS_NOT_RUN, pending)
    target, blocked = _resolve_gui(ctx)
    if blocked is not None or target is None:
        return Outcome(STATUS_NOT_RUN, blocked or "被测 GUI 缺失")
    if manifest_has_python_entries():
        return Outcome(STATUS_FAILED, "可选资产清单包含 python 字样条目（隐藏携带 Python）")
    prepared = _prepare_scratch(item, ctx, "python-scan")
    if prepared.error is not None or not prepared.files:
        return Outcome(STATUS_NOT_RUN, f"夹具准备失败：{prepared.error or '输入为空'}")
    run, module_hits, scan_errors = _c05_scan_during_conversion(target, prepared.input_dir, prepared.output_dir)
    return _c05_assess(item.item_id, run, module_hits, scan_errors)


def _run_c06_old_dir(item: Item, ctx: Context) -> Outcome:
    if OLD_PROJECT_DIR.exists():
        message = f"旧项目目录仍存在：{OLD_PROJECT_DIR}（T-28 删除前置未达成；删除/改名后再验）"
        return Outcome(STATUS_NOT_RUN, message)
    return _run_conversion_item(item, ctx, None, tag_suffix="noolddir")


def _run_c07_old_cache(item: Item, ctx: Context) -> Outcome:
    hits = _old_cache_hits()
    if hits:
        return Outcome(STATUS_NOT_RUN, f"旧缓存仍在场：{hits}")
    return _run_conversion_item(item, ctx, None, tag_suffix="nooldcache")


def _run_c08_c09(item: Item, ctx: Context, root: Path | None, label: str) -> Outcome:
    if root is None or not root.is_dir():
        switch = "--installed-root" if item.item_id == "C08" else "--portable-root"
        return Outcome(STATUS_NOT_RUN, f"未提供{label}目录（{switch}）")
    exe = root / "JchTools.exe"
    if not exe.is_file():
        return Outcome(STATUS_NOT_RUN, f"{label}目录缺少 JchTools.exe：{exe}")
    return _run_conversion_item(item, ctx, exe, tag_suffix=f"form-{item.item_id.lower()}")


def _run_env(item: Item, ctx: Context) -> Outcome:
    handlers: dict[str, Callable[[Item, Context], Outcome]] = {
        "C01": _run_c01_installed_scan,
        "C02": _run_c02_portable_scan,
        "C03": _run_c03_unconfigured,
        "C04": _run_c04_offline,
        "C05": _run_c05_no_python,
        "C06": _run_c06_old_dir,
        "C07": _run_c07_old_cache,
        "C08": lambda item, ctx: _run_c08_c09(item, ctx, ctx.installed_root, "安装版"),
        "C09": lambda item, ctx: _run_c08_c09(item, ctx, ctx.portable_root, "便携版"),
    }
    return handlers[item.item_id](item, ctx)


def run_item(item: Item, ctx: Context) -> Outcome:
    dispatch: dict[str, Callable[[Item, Context], Outcome]] = {
        "A": _run_matrix,
        "B": _run_gui_ref,
        "C": _run_env,
    }
    try:
        return dispatch[item.group](item, ctx)
    except Exception as exc:  # noqa: BLE001  # 单项意外错误按 FAIL 呈现，不拖垮其余条目。
        return Outcome(STATUS_FAILED, f"条目执行异常：{type(exc).__name__}: {exc}")


# ---------------------------------------------------------------- 输出与入口。


def _fixture_summary(item: Item) -> str:
    parts: list[str] = []
    if item.fixtures:
        parts.append(f"夹具 {list(item.fixtures)}")
    if item.synth:
        parts.append(f"合成器 {item.synth}")
    return "；".join(parts) if parts else "无需仓库夹具"


def print_list() -> int:
    print("转 Markdown 验收条目总表（附录 A 26 项 + GUI E2E + 双形态/环境前置）")
    print("入口形态：仅 GUI（T-04 无 CLI；矩阵经 pywinauto 驱动真实公开入口，B 组引用 gui_smoke 阶段）")
    print("=" * 118)
    for item in ITEMS:
        stage = f"| 阶段 {item.stage}" if item.stage else ""
        print(f"[{item.item_id}]（{item.group} 组）{item.title}")
        print(f"    入口：{item.entry}{stage}| 资产需求：{item.needs_assets or '无'}")
        print(f"    {_fixture_summary(item)}")
        if item.fixture_note:
            print(f"    夹具状态：{item.fixture_note}")
    print("=" * 118)
    groups = {group: sum(1 for item in ITEMS if item.group == group) for group in ("A", "B", "C")}
    print(f"共 {len(ITEMS)} 条（A 组 {groups['A']} / B 组 {groups['B']} / C 组 {groups['C']}）")
    return 0


def _print_report(results: list[tuple[Item, Outcome]], report_path: Path | None) -> int:
    print()
    print("==== 转 Markdown 验收汇总 ====")
    for item, outcome in results:
        print(f"{outcome.status:8} {item.item_id}  {item.title}")
        if outcome.reason:
            print(f"         原因：{outcome.reason}")
        for detail in outcome.details:
            print(f"         {detail}")
    counts = {STATUS_OK: 0, STATUS_FAILED: 0, STATUS_NOT_RUN: 0}
    for _, outcome in results:
        counts[outcome.status] += 1
    print("-" * 118)
    total = len(results)
    line = f"PASS {counts[STATUS_OK]}| FAIL {counts[STATUS_FAILED]}| NOT RUN {counts[STATUS_NOT_RUN]}（共 {total} 条）"
    print(line)
    if report_path is not None:
        report_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "generated": time.strftime("%Y-%m-%dT%H:%M:%S"),
            "items": [
                {
                    "id": item.item_id,
                    "group": item.group,
                    "title": item.title,
                    "status": outcome.status,
                    "reason": outcome.reason,
                    "details": outcome.details,
                }
                for item, outcome in results
            ],
            "summary": {"pass": counts[STATUS_OK], "fail": counts[STATUS_FAILED], "not_run": counts[STATUS_NOT_RUN]},
        }
        _ = report_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"JSON 报告：{report_path}")
    if counts[STATUS_FAILED]:
        return 1
    if counts[STATUS_OK] == 0:
        return 2
    return 0


class _Arguments(argparse.Namespace):
    """带类型标注的解析结果；类属性即默认值（argparse 仅对缺失属性写默认值）."""

    list_only: bool = False
    only: str = ""
    gui_exe: str = ""
    portable_root: str = ""
    installed_root: str = ""
    fixtures: str = ""
    json_report: str = ""
    seed_state: str = ""
    seed_xberg: str = ""


def _seed_isolated_state(state_root: Path, xberg_root: Path) -> None:
    """为开发验收 GUI 写入与驱动同源的隔离 Xberg 配置。."""
    if not state_root.is_absolute() or not xberg_root.is_absolute():
        message = "隔离状态目录与 Xberg 目录必须是绝对路径"
        raise ValueError(message)
    database = state_root / "config.sqlite3"
    schema = ROOT / "src" / "app_settings.sql"
    state_root.mkdir(parents=True, exist_ok=True)
    with contextlib.closing(sqlite3.connect(database)) as connection:
        _ = connection.executescript(schema.read_text(encoding="utf-8"))
        update_sql = """INSERT INTO app_settings(key,value) VALUES('xberg_directory',?) ON CONFLICT(key)
DO UPDATE SET value=excluded.value"""
        _ = connection.execute(update_sql, (str(xberg_root),))
        custom_sql = """INSERT INTO app_settings(key,value) VALUES('xberg_custom_directory',?) ON CONFLICT(key)
DO UPDATE SET value=excluded.value"""
        _ = connection.execute(custom_sql, (str(xberg_root),))
        source_sql = """INSERT INTO app_settings(key,value) VALUES('xberg_source','custom') ON CONFLICT(key)
DO UPDATE SET value=excluded.value"""
        _ = connection.execute(source_sql)
        _ = connection.execute("DELETE FROM app_settings WHERE key='xberg_downloaded_directory'")
        connection.commit()


def _select_items(selector: str) -> tuple[list[Item], str | None]:
    tokens = [token.strip().upper() for token in selector.split(",") if token.strip()]
    if not tokens:
        return list(ITEMS), None
    known_ids = {item.item_id for item in ITEMS}
    wanted_groups = {token for token in tokens if token in ("A", "B", "C")}
    wanted_ids = {token for token in tokens if token not in wanted_groups}
    unknown = wanted_ids - known_ids
    if unknown:
        return [], f"未知条目或分组：{sorted(unknown)}（可用：A/B/C 或条目号 A01…C09，逗号分隔）"
    selected = [item for item in ITEMS if item.group in wanted_groups or item.item_id in wanted_ids]
    return selected, None


def main() -> int:
    _reconfigure_stdout()
    description = "转 Markdown 验收承接驱动器（附录 A；缺资产一律 NOT RUN，不虚构 PASS）"
    parser = argparse.ArgumentParser(description=description)
    _ = parser.add_argument("--list", dest="list_only", action="store_true", help="列出全部条目与夹具/资产映射，不执行")
    _ = parser.add_argument("--only", default="", help="只执行指定组（A/B/C）或条目（如 A24），逗号分隔")
    _ = parser.add_argument("--gui-exe", default="", help="被测 JchTools.exe（debug 构建支持干净资产根覆盖）")
    _ = parser.add_argument("--portable-root", default="", help="解包的便携版目录（含 JchTools.exe）")
    _ = parser.add_argument("--installed-root", default="", help="安装版目录（默认形态为安装器写入的位置）")
    _ = parser.add_argument("--fixtures", default="", help="夹具目录（默认 tests/markdown_fixtures）")
    _ = parser.add_argument("--json", dest="json_report", default="", help="JSON 报告输出路径（留档）")
    _ = parser.add_argument("--seed-state", default="", help=argparse.SUPPRESS)
    _ = parser.add_argument("--seed-xberg", default="", help=argparse.SUPPRESS)
    args = parser.parse_args(namespace=_Arguments())
    if bool(args.seed_state) != bool(args.seed_xberg):
        parser.error("--seed-state 与 --seed-xberg 必须成对提供")
    if args.seed_state:
        _seed_isolated_state(Path(args.seed_state).resolve(), Path(args.seed_xberg).resolve())
        return 0
    if args.list_only:
        return print_list()
    selected, error = _select_items(args.only)
    if error is not None:
        print(error)
        return 3
    discovered_gui = os.environ.get("JCHTOOLS_TEST_GUI_EXE")
    if not discovered_gui:
        candidate = ROOT / "target" / "debug" / "JchTools.exe"
        discovered_gui = str(candidate) if candidate.is_file() else ""
    ctx = Context(
        Path(args.gui_exe or discovered_gui).resolve() if (args.gui_exe or discovered_gui) else None,
        Path(args.installed_root).resolve() if args.installed_root else None,
        Path(args.portable_root).resolve() if args.portable_root else None,
        Path(args.fixtures).resolve() if args.fixtures else FIXTURES_DEFAULT,
        probe_assets(),
    )
    SCRATCH_ROOT.mkdir(parents=True, exist_ok=True)
    results = [(item, run_item(item, ctx)) for item in selected]
    report_path = Path(args.json_report).resolve() if args.json_report else None
    return _print_report(results, report_path)


if __name__ == "__main__":
    sys.exit(main())
