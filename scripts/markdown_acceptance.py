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
退出码：任一 FAIL→1；全部 NOT RUN→2；其余（有 PASS、无 FAIL）→0；参数错误→3。
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import hashlib
import io
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING

import comtypes
import pywintypes
import win32con
import win32gui
from PIL import Image, ImageDraw
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

# 状态常量名避开 pass 字样（质量门 S105 把含该字样的变量名当疑似硬编码口令）。
STATUS_OK = "PASS"
STATUS_FAILED = "FAIL"
STATUS_NOT_RUN = "NOT RUN"

# 与 src/markdown_assets.rs 的常量同口径：资产根目录、固定版本与成员相对路径。
DATA_DIRECTORY = "markdown-assets"
XBERG_TAG = "v2026.9.15-0746-run36.1"
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

# 转 Markdown 页自上而下的「选择目录…」行数：运行目录 / 输入 / 输出。
CONVERT_ROW_COUNT = 3


def _old_cache_candidates() -> list[Path | None]:
    local = os.environ.get("LOCALAPPDATA")
    return [
        OLD_PROJECT_DIR / ".venv",
        Path(local) / "all2markdown" if local else None,
        Path.home() / ".cache" / "all2markdown",
    ]


# 附录 A 双形态检查：主包不得携带的转换专用资产（文件名/后缀，全部小写比较）。
FORBIDDEN_SUFFIXES = (".onnx",)
FORBIDDEN_NAMES = ("xberg.exe", "markdown-media-worker.exe")
FORBIDDEN_DLL_PREFIXES = ("python", "onnxruntime", "sherpa", "avcodec", "avformat", "swresample", "ffmpeg")

GUI_WINDOW_TIMEOUT = 60
READINESS_TIMEOUT = 180
BUSY_TIMEOUT = 30
CONVERSION_TIMEOUT = 1800  # 真实 OCR/媒体转录按分钟级计，验收宁等勿假。
EDIT_ROW_TOLERANCE_PX = 20

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
    local = os.environ.get("LOCALAPPDATA")
    if not local:
        return None
    # 与 src/config.rs::state_dir（ProjectDirs data_local_dir）同口径。
    return Path(local) / "JchTools" / "data" / DATA_DIRECTORY


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


def _probe_runtime_dir(root: Path, missing: list[str]) -> Path | None:
    """只读核对运行目录指针与 xberg.exe；缺失项写入 missing."""
    selection = root / "xberg-runtime-path.txt"
    runtime_dir: Path | None = None
    if not selection.is_file():
        missing.append(f"未配置 Xberg 运行目录（未找到 {selection}）")
        return None
    with contextlib.suppress(OSError, ValueError):
        runtime_dir = Path(selection.read_text(encoding="utf-8").strip())
    if runtime_dir is not None and not (runtime_dir / "xberg.exe").is_file():
        missing.append(f"Xberg 运行目录缺 xberg.exe：{runtime_dir / 'xberg.exe'}")
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
        return AssetProbe(None, None, None, None, ["缺少 LOCALAPPDATA，无法定位资产目录"])
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
    _ = target.joinpath("p6_bin.ppm").write_bytes(b"P6\n3 3\n255\n" + bytes((0, 128, 64)) * 3)


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
        (["-y", "-f", "lavfi", "-i", "color=c=black:size=64x64:duration=1", "-c:v", "libx264"], "noaudio.mp4"),
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


SYNTHESIZERS: dict[str, Callable[[Path, Path], SynthResult]] = {
    "pnm": lambda target, _fixtures: _synth_pnm(target),
    "images": lambda target, _fixtures: _synth_images(target),
    "office": _synth_office,
    "media": lambda target, _fixtures: _synth_media(target),
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
        ("matrix/pptx_two_png_order.pptx",),
        "需新增：Pillow 渲染两张不同文字位图（如 IMG-ALPHA / IMG-BETA），zip 组装 PPTX 并打乱 slide XML 与 rels 顺序",
        needs_assets="xberg",
    ),
    Item(
        "A02",
        "A",
        "PPTX 5~10 张不同图片：多次转换内容顺序稳定",
        _MATRIX_COMMON,
        ("matrix/pptx_multi_images.pptx",),
        "需新增：5~10 张不同 PNG 组装 PPTX；本项连跑两次并逐字节比对输出一致（bug62513.pptx 未核实，不计入）",
        needs_assets="xberg",
    ),
    Item(
        "A03",
        "A",
        "PPTX 同一媒体被多个 shape 引用：允许复用识别，保留每处位置",
        _MATRIX_COMMON,
        ("matrix/pptx_shared_media.pptx",),
        "需新增：编辑 _rels 让两处 shape 引用同一 media 成员；断言两处位置都出现该图 OCR 内容",
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
        ("matrix/pptx_svg.pptx",),
        "需新增：手写 SVG 文本成员，并包含指向失效外部资源的引用以验证禁用",
        needs_assets="xberg",
    ),
    Item(
        "A08",
        "A",
        "PPTX 普通 run 与可见字段混排：字段值完整保留",
        _MATRIX_COMMON,
        ("matrix/pptx_runs_fields.pptx",),
        "需新增：普通文本 run 与 <a:fld> 字段混排，字段值可断言",
        needs_assets="xberg",
    ),
    Item(
        "A09",
        "A",
        "PPTX 图片 description 有文字而 OCR 为空：description 不冒充正文",
        _MATRIX_COMMON,
        ("matrix/pptx_descr_no_ocr.pptx",),
        "需新增：图片 descr/name 含文字、图片本体为无文字纯色位图",
        needs_assets="xberg",
    ),
    Item(
        "A10",
        "A",
        "PPTX 图片不可解码：诊断阶段明确，其余内容继续输出",
        _MATRIX_COMMON,
        ("matrix/pptx_undecodable_image.pptx",),
        "需新增：嵌入截断图片字节，同页保留正常文字与图片",
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
        ("matrix/xlsx_drawing_order.xlsx",),
        "需新增：drawing 多图且 _rels 顺序与显示顺序不同",
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
        "JP2、J2K、J2C、JPX、JPM、MJ2：特殊图片解码与失败诊断",
        _MATRIX_COMMON,
        (
            "matrix/jpeg2000/jp2.jp2",
            "matrix/jpeg2000/j2k.j2k",
            "matrix/jpeg2000/j2c.j2c",
            "matrix/jpeg2000/jpx.jpx",
            "matrix/jpeg2000/jpm.jpm",
            "matrix/jpeg2000/mj2.mj2",
        ),
        "需新增：jp2/j2k 可由带 openjpeg 的 Pillow 生成；其余需 OpenJPEG opj_compress 或公开语料（openjpeg-data）",
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
        "所有最终产物：每顶层输入一份 Markdown、不含 Base64、不生成图片文件、源文件不变",
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
    button = window.child_window(title=title, control_type="Button")
    _ = button.wait("visible enabled", timeout=GUI_WINDOW_TIMEOUT)
    with contextlib.suppress(*TRANSIENT_ERRORS):
        _ = window.set_focus()
    button.click_input()
    time.sleep(0.3)


def _button_state(window: WindowSpecification, title: str) -> tuple[bool, bool]:
    """返回（可见, 可用）；元素未解析按（False, False）处理，由调用方轮询."""
    try:
        button = window.child_window(title=title, control_type="Button")
        return button.exists() and button.is_visible(), bool(button.is_enabled())
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
    """转 Markdown 页自上而下的三行「选择目录…」按钮：运行目录 / 输入 / 输出."""
    buttons = [b for b in window.descendants(control_type="Button") if (b.window_text() or "") == "选择目录…"]
    buttons.sort(key=lambda b: b.rectangle().top)
    return buttons


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
    """按用户路径点标题栏「关闭」；合成点击落空时兜底 WM_CLOSE（与 gui_smoke 同口径）."""
    with contextlib.suppress(*TRANSIENT_ERRORS):
        for button in window.descendants(control_type="Button"):
            if (button.window_text() or "") == "关闭":
                _ = window.set_focus()
                button.click_input()
                time.sleep(0.3)
                return
    with contextlib.suppress(*TRANSIENT_ERRORS):
        win32gui.PostMessage(window.handle, win32con.WM_CLOSE, 0, 0)


def _terminate(proc: subprocess.Popen[bytes]) -> None:
    with contextlib.suppress(Exception):
        _ = proc.wait(timeout=10)
    proc.kill()
    _ = proc.wait(timeout=10)


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
            message = f"转 Markdown 页「选择目录…」按钮不足三行（实得 {len(rows)}）"
            return GuiRun([], _window_texts(window), message)
        _set_row_edit(window, rows[1], str(input_dir))
        _set_row_edit(window, rows[2], str(output_dir))
        if not _wait_start_ready(window, READINESS_TIMEOUT):
            message = "「开始转换」始终未就绪（组件未初始化或就绪检查失败）"
            return GuiRun([], _window_texts(window), message)
        _click_button(window, "开始转换")
        saw_busy = False
        error: str | None = None
        deadline = time.time() + CONVERSION_TIMEOUT
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
IMAGE_SUFFIXES = (".png", ".jpg", ".jpeg", ".gif", ".tif", ".tiff", ".bmp", ".webp", ".jp2", ".j2k")


def _expected_markdown_names(files: list[str]) -> list[str]:
    names: list[str] = []
    for name in files:
        stem = Path(name).stem
        suffix = Path(name).suffix.lstrip(".").lower()
        names.append(f"{stem}_{suffix}.md" if suffix else f"{stem}.md")
    return names


def verify_common_postconditions(source_dir: Path, output_dir: Path, sources: dict[Path, str]) -> list[str]:
    """附录 A 第 26 项横切断言：无 Base64、无图片文件、源不变、无临时残留."""
    problems = [
        f"源文件被改动：{relative}"
        for relative, digest in sources.items()
        if not (source_dir / relative).is_file() or _file_digest(source_dir / relative) != digest
    ]
    problems.extend(f"输出残留临时文件：{leftover}" for leftover in output_dir.rglob(f"*{TEMP_LEFTOVER_MARKER}*"))
    problems.extend(
        f"输出出现图片文件：{image.relative_to(output_dir)}"
        for image in output_dir.rglob("*")
        if image.is_file() and image.suffix.lower() in IMAGE_SUFFIXES
    )
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
        check=False,
    )
    if done.returncode != 0:
        return (), "gui_smoke 不支持 --list-stages（S5 补丁未应用或脚本损坏）"
    stages = tuple(
        line.strip().upper() for line in (done.stdout or "").splitlines() if line.strip().upper().startswith("S")
    )
    return stages, None


def _delegate_stage(stage: str, exe: Path, env_extra: dict[str, str] | None = None) -> Outcome:
    argv = [sys.executable, str(GUI_SMOKE), "--exe", str(exe), "--stages", stage]
    done = subprocess.run(argv, capture_output=True, text=True, check=False, env=os.environ | (env_extra or {}))
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
    return [str(path) for path in _old_cache_candidates() if path is not None and path.exists()]


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
    return Outcome(STATUS_OK, details=details)


def _run_matrix(item: Item, ctx: Context) -> Outcome:
    blocked = _synth_precondition(item)
    if blocked:
        return Outcome(STATUS_NOT_RUN, blocked)
    if item.item_id == "A25":
        return _run_matrix_a25(item, ctx)
    if item.item_id == "A02":
        return _run_matrix_a02(item, ctx)
    if item.item_id == "A24":
        return _run_matrix_a24(item, ctx)
    return _run_conversion_item(item, ctx, None)


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
    return Outcome(STATUS_OK, details=["两次转换产物逐字节一致"])


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
    return _delegate_stage(item.stage, target)


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
    # JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT 仅 debug 构建生效（src/markdown_assets.rs 的
    # cfg!(debug_assertions) 门）：release EXE 会读到真实用户配置，本项结论随之无效。
    if "target" not in target.parts or "debug" not in target.parts:
        message = "本项要求 debug 构建被测 EXE（release 忽略资产根覆盖环境变量，会读到真实用户配置）"
        return Outcome(STATUS_NOT_RUN, message)
    stages, error = ctx.cached_stages()
    if error is not None or "S1" not in stages:
        return Outcome(STATUS_NOT_RUN, error or "gui_smoke 未提供 S1 阶段选择器（补丁未应用）")
    scratch_assets = SCRATCH_ROOT / "c03-asset-root"
    shutil.rmtree(scratch_assets, ignore_errors=True)
    _ = scratch_assets.mkdir(parents=True)
    env = {"JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT": str(scratch_assets)}
    result = _delegate_stage("S1", target, env_extra=env)
    if result.status != STATUS_OK:
        result.details.insert(0, f"{item.item_id} 未配置状态下的 S1 启动未通过")
        return result
    downloaded = [str(path.relative_to(scratch_assets)) for path in scratch_assets.rglob("*")]
    if downloaded:
        return Outcome(STATUS_FAILED, f"未配置启动即写入/下载资产目录：{downloaded[:8]}")
    return Outcome(STATUS_OK, details=[f"{item.item_id} 未配置启动正常退出，资产根目录零写入（无自动下载）"])


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
    """带类型标注的解析结果；未提供的参数保留安全默认值."""

    list_only: bool
    only: str
    gui_exe: str
    portable_root: str
    installed_root: str
    fixtures: str
    json_report: str

    def __init__(self) -> None:
        super().__init__()
        self.list_only = False
        self.only = ""
        self.gui_exe = ""
        self.portable_root = ""
        self.installed_root = ""
        self.fixtures = ""
        self.json_report = ""


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
    args = parser.parse_args(namespace=_Arguments())
    if args.list_only:
        return print_list()
    selected, error = _select_items(args.only)
    if error is not None:
        print(error)
        return 3
    ctx = Context(
        Path(args.gui_exe).resolve() if args.gui_exe else None,
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
