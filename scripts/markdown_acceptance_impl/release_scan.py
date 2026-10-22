"""转 Markdown 验收发布包成员校验与进程扫描（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path
from typing import TYPE_CHECKING, cast

from scripts.markdown_acceptance_impl.assets import (
    BASE_COMPONENT_IDS,
    BASE_REQUIRED,
    COMPONENT_MANIFEST_NAME,
    DOCUMENT_MODEL_SUFFIXES,
    MEDIA_REQUIRED,
    RECEIPT_NAME,
    RELEASE_DOWNLOAD,
    RELEASE_SOURCE,
    _asset_root,
    _probe_runtime_dir,
)
from scripts.markdown_acceptance_impl.content import _assess_conversion
from scripts.markdown_acceptance_impl.model import (
    STATUS_FAILED,
    STATUS_NOT_RUN,
    STATUS_OK,
    AssetProbe,
    Outcome,
    _file_digest,
    _is_str_obj_list,
    _is_str_obj_map,
    _old_cache_candidates,
    _str_field,
)
from scripts.markdown_acceptance_impl.paths import ASSET_MANIFEST

if TYPE_CHECKING:
    from collections.abc import Callable

    from scripts.markdown_acceptance_impl.model import GuiRun, Item, PreparedInputs

__all__ = [
    "_c05_assess",
    "_is_audio_path",
    "_legacy_base_members",
    "_load_release_receipt",
    "_member_problems",
    "_member_records",
    "_merge_component_members",
    "_old_cache_hits",
    "_parse_json",
    "_release_component_members",
    "_release_component_records",
    "_release_members",
    "_tasklist",
    "_validate_base_component_members",
    "_validate_release_manifest",
    "_validate_release_member_sets",
    "manifest_has_python_entries",
    "probe_assets",
    "scan_distribution",
    "scan_python_modules",
]


# json.loads 的返回含 Any；经固定签名别名收口为 object，再以 isinstance 逐层收窄。
_parse_json: Callable[[str], object] = json.loads


# ---------------------------------------------------------------- C 组检查件。


def _member_records(entries: object, label: str) -> dict[str, tuple[int, str]]:
    """收据成员必须可安全落盘；Windows 大小写重名与逃逸均不能通过."""
    if not _is_str_obj_list(entries) or not entries:
        message = f"{label} 缺少非空 members"
        raise ValueError(message)
    members: dict[str, tuple[int, str]] = {}
    seen: set[str] = set()
    for entry in entries:
        if not _is_str_obj_map(entry):
            message = f"{label} 成员不是对象"
            raise ValueError(message)
        relative = _str_field(entry, "path")
        if (
            not relative
            or "\\" in relative
            or ":" in relative
            or relative.startswith("/")
            or re.search(r'[\x00-\x1f<>"|?*]', relative)
            or any(
                part in ("", ".", "..")
                or part.endswith((".", " "))
                or part.split(".", 1)[0].upper()
                in {
                    "CON",
                    "PRN",
                    "AUX",
                    "NUL",
                    *(f"COM{i}" for i in range(1, 10)),
                    *(f"LPT{i}" for i in range(1, 10)),
                }
                for part in relative.split("/")
            )
        ):
            message = f"{label} 成员路径不安全：{relative}"
            raise ValueError(message)
        if relative.casefold() in seen:
            message = f"{label} 成员路径重复：{relative}"
            raise ValueError(message)
        seen.add(relative.casefold())
        size, digest = entry.get("size_bytes"), entry.get("sha256")
        if (
            type(size) is not int
            or size <= 0
            or not isinstance(digest, str)
            or not re.fullmatch("[0-9a-f]{64}", digest)
        ):
            message = f"{label} 成员大小/摘要无效：{relative}"
            raise ValueError(message)
        members[relative] = (size, digest)
    return members


def _is_audio_path(relative: str) -> bool:
    parts = relative.lower().replace("\\", "/").split("/")
    return (
        "ffmpeg" in parts
        or "vad" in parts
        or "samples" in parts
        or any(part.startswith(("sense_voice", "sensevoice")) for part in parts)
        or Path(parts[-1]).stem.startswith(("avcodec", "avformat", "avutil", "swresample", "ffmpeg", "silero_vad"))
    )


def _load_release_receipt(runtime: Path) -> tuple[dict[str, object], str]:
    receipt_path = runtime / RECEIPT_NAME
    if not receipt_path.resolve().is_relative_to(runtime.resolve()):
        message = "内置收据路径逃逸"
        raise ValueError(message)
    receipt = _parse_json(receipt_path.read_text(encoding="utf-8"))
    if not _is_str_obj_map(receipt) or receipt.get("owner") != "JchTools-bundled-xberg-v1":
        message = "内置收据 owner 无效"
        raise ValueError(message)
    tag = _str_field(receipt, "tag")
    if not tag or not re.fullmatch(r"[A-Za-z0-9._-]+", tag) or receipt.get("source") != RELEASE_SOURCE:
        message = "内置收据来源/tag 无效"
        raise ValueError(message)
    return receipt, f"{RELEASE_DOWNLOAD}{tag}/"


def _validate_release_manifest(manifest: object, prefix: str) -> None:
    if (
        not _is_str_obj_map(manifest)
        or manifest.get("archive") != COMPONENT_MANIFEST_NAME
        or manifest.get("url") != prefix + COMPONENT_MANIFEST_NAME
        or type(manifest.get("size_bytes")) is not int
        or cast("int", manifest["size_bytes"]) <= 0
        or not re.fullmatch("[0-9a-f]{64}", _str_field(manifest, "sha256") or "")
    ):
        message = "内置收据 manifest 来源/大小/摘要无效"
        raise ValueError(message)


def _release_component_records(receipt: dict[str, object]) -> list[object]:
    components = receipt.get("components")
    if not _is_str_obj_list(components) or len(components) != len(BASE_COMPONENT_IDS):
        message = "内置收据 components 必须恰为 document+snapshot"
        raise ValueError(message)
    if {_str_field(record, "id") for record in components} != set(BASE_COMPONENT_IDS) or _str_field(
        receipt.get("audio_assets"), "id"
    ) != "audio-assets":
        message = "内置收据基础/音频组件位置无效"
        raise ValueError(message)
    return [*components, receipt.get("audio_assets")]


def _validate_base_component_members(component_id: str, members: dict[str, tuple[int, str]]) -> None:
    common = {name for name in BASE_REQUIRED if not name.startswith("models/")} | {
        "LICENSE",
        "THIRD_PARTY_LICENSES.md",
        "models/paddleocr-onnx-models-LICENSE.txt",
    }
    if common - members.keys():
        message = f"独立基础组件缺少共享成员：{component_id}"
        raise ValueError(message)
    owned_prefix = "models/models--" if component_id == "document" else "models/snapshot-ocr/"
    other_prefix = "models/snapshot-ocr/" if component_id == "document" else "models/models--"
    owned = [name for name in members if name.startswith(owned_prefix)]
    if not owned or any(name.startswith(other_prefix) for name in members):
        message = f"基础组件模型归属无效：{component_id}"
        raise ValueError(message)


def _release_component_members(record: object, prefix: str, ids: set[str]) -> tuple[str, dict[str, tuple[int, str]]]:
    if not _is_str_obj_map(record):
        message = "内置收据组件记录无效"
        raise ValueError(message)
    component_id = _str_field(record, "id") or ""
    if component_id not in ("document", "snapshot", "audio-assets") or component_id in ids:
        message = f"内置收据组件 id 无效/重复：{component_id}"
        raise ValueError(message)
    ids.add(component_id)
    archive = (
        "xberg-audio-assets-x86_64-pc-windows-msvc.zip"
        if component_id == "audio-assets"
        else f"xberg-{component_id}-x86_64-pc-windows-msvc.zip"
    )
    if (
        record.get("archive") != archive
        or record.get("requires") != []
        or type(record.get("size_bytes")) is not int
        or cast("int", record["size_bytes"]) <= 0
        or not re.fullmatch("[0-9a-f]{64}", _str_field(record, "sha256") or "")
        or (component_id == "audio-assets" and record.get("url") != prefix + archive)
    ):
        message = f"内置收据组件 archive/来源/大小/摘要无效：{component_id}"
        raise ValueError(message)
    members = _member_records(record.get("members"), component_id)
    if component_id != "audio-assets":
        _validate_base_component_members(component_id, members)
    return component_id, members


def _merge_component_members(
    target: dict[str, tuple[int, str]], members: dict[str, tuple[int, str]], component_id: str
) -> None:
    for name, identity in members.items():
        if _is_audio_path(name) != (component_id == "audio-assets"):
            message = f"基础/音频组件成员越界：{name}"
            raise ValueError(message)
        existing = next((old for old in target if old.casefold() == name.casefold()), None)
        if existing is not None and (existing != name or target[existing] != identity):
            message = f"共享成员身份冲突：{name}"
            raise ValueError(message)
        target[name] = identity


def _member_problems(runtime: Path, members: dict[str, tuple[int, str]]) -> list[str]:
    problems: list[str] = []
    for name, (size, digest) in members.items():
        path = runtime / name
        try:
            if not path.resolve().is_relative_to(runtime.resolve()):
                problems.append(f"成员路径逃逸：{name}")
            elif not path.is_file():
                problems.append(f"缺少成员：{name}")
            elif path.stat().st_size != size or _file_digest(path) != digest:
                problems.append(f"成员大小/摘要不匹配：{name}")
        except (OSError, ValueError, RuntimeError) as error:
            problems.append(f"成员读取失败：{name}：{error}")
    return problems


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


def _old_cache_hits() -> list[str]:
    return [str(path) for path in _old_cache_candidates() if path.exists()]


def _c05_assess(  # noqa: PLR0913, PLR0917  # 验证结果、输入快照与进程扫描证据具有独立语义
    item: Item,
    prepared: PreparedInputs,
    sources: dict[Path, str],
    run: GuiRun,
    module_hits: list[str],
    scan_errors: list[str],
) -> Outcome:
    scan_details = [f"进程模块扫描未完整完成：{error}" for error in scan_errors]
    if run.error is not None:
        return Outcome(STATUS_FAILED, f"{item.item_id} 转换链路失败：{run.error}", scan_details)
    if module_hits:
        return Outcome(STATUS_FAILED, f"转换期间进程加载了 python 模块：{module_hits[:4]}", scan_details)
    conversion = _assess_conversion(item, prepared, sources, run)
    if conversion.status != STATUS_OK:
        return Outcome(
            STATUS_FAILED,
            f"{item.item_id} 未证明真实转换成功：{conversion.reason}",
            [*conversion.details, *scan_details],
        )
    empty = [path.name for path in run.outputs if not path.read_text(encoding="utf-8", errors="replace").strip()]
    if empty:
        return Outcome(STATUS_FAILED, f"{item.item_id} 转换产物正文为空：{empty}", [*conversion.details, *scan_details])
    if scan_errors:
        return Outcome(STATUS_NOT_RUN, scan_errors[0], [*conversion.details, *scan_details])
    return Outcome(
        STATUS_OK,
        details=[
            f"{item.item_id} 转换产物与源文件断言通过，运行期间进程模块及资产清单均无 Python",
            *conversion.details,
        ],
    )


def probe_assets() -> AssetProbe:
    """只读按实际有效目录分别检查基础与可选音频；不下载、不改配置."""
    missing: list[str] = []
    root = _asset_root()
    if root is None:
        return AssetProbe(None, None, None, None, ["未提供隔离验收资产根（JCHTOOLS_TEST_ASSET_ROOT）"])
    runtime_dir = _probe_runtime_dir(root, missing)
    if runtime_dir is None:
        return AssetProbe(root, None, None, None, missing)
    base_missing: list[str] = []
    media_missing: list[str] = []
    receipt = runtime_dir / RECEIPT_NAME
    if receipt.exists():
        try:
            base, audio = _release_members(runtime_dir)
            base_missing.extend(_member_problems(runtime_dir, base))
            media_missing.extend(_member_problems(runtime_dir, audio))
        except (OSError, ValueError) as error:
            base_missing.append(f"内置收据无法验证：{error}")
    else:
        # 旧外部源仍可用；用已有固定成员清单检查基础，不强迫迁移来源制造收据。
        try:
            base = _legacy_base_members()
            base_missing.extend(
                _member_problems(
                    runtime_dir, {name: identity for name, identity in base.items() if name.startswith("models/")}
                )
            )
            base_missing.extend(f"基础资产缺失：{name}" for name in BASE_REQUIRED if not (runtime_dir / name).is_file())
        except (OSError, ValueError) as error:
            base_missing.append(f"基础资产清单无法验证：{error}")
        holes = [name for name in MEDIA_REQUIRED if not (runtime_dir / name).is_file()]
        if holes:
            media_missing.append(f"可选媒体资产缺失：{', '.join(holes)}")
    missing.extend(base_missing)
    missing.extend(media_missing)
    return AssetProbe(root, runtime_dir, runtime_dir / "xberg.exe", runtime_dir, missing, base_missing, media_missing)


def _legacy_base_members() -> dict[str, tuple[int, str]]:
    """沿用资产清单的真实模型身份；不再把这些必需模型当作违禁品."""
    parsed = _parse_json(ASSET_MANIFEST.read_text(encoding="utf-8"))
    section = parsed.get("xberg") if _is_str_obj_map(parsed) else None
    records = _member_records(section.get("members") if _is_str_obj_map(section) else None, "基础资产清单")
    base = {name: record for name, record in records.items() if not _is_audio_path(name)}
    required = set(BASE_REQUIRED) | {"LICENSE", "THIRD_PARTY_LICENSES.md", "models/paddleocr-onnx-models-LICENSE.txt"}
    missing = required - base.keys()
    missing.update(suffix for suffix in DOCUMENT_MODEL_SUFFIXES if not any(name.endswith(suffix) for name in base))
    if missing:
        message = f"基础成员清单不完整：{sorted(missing)}"
        raise ValueError(message)
    return base


def _validate_release_member_sets(base: dict[str, tuple[int, str]], audio: dict[str, tuple[int, str]]) -> None:
    expected = _legacy_base_members()
    required = (
        set(BASE_REQUIRED)
        | {"LICENSE", "THIRD_PARTY_LICENSES.md"}
        | {name for name in expected if name.startswith("models/")}
    )
    if required - base.keys() or (set(MEDIA_REQUIRED) | {"ffmpeg/LICENSE.txt"}) - audio.keys():
        message = "内置收据缺少基础模型/DLL 或音频必需成员"
        raise ValueError(message)
    for name in required:
        if name.startswith("models/") and base[name] != expected[name]:
            message = f"内置模型身份不匹配：{name}"
            raise ValueError(message)
    for name in ("onnxruntime.dll", "onnxruntime_providers_shared.dll"):
        if base[name] != base[f"sherpa-onnx/{name}"]:
            message = f"共享 ORT 身份不匹配：{name}"
            raise ValueError(message)


def _release_members(runtime: Path) -> tuple[dict[str, tuple[int, str]], dict[str, tuple[int, str]]]:
    receipt, prefix = _load_release_receipt(runtime)
    _validate_release_manifest(receipt.get("manifest"), prefix)
    records = _release_component_records(receipt)
    ids: set[str] = set()
    base: dict[str, tuple[int, str]] = {}
    audio: dict[str, tuple[int, str]] = {}
    for record in records:
        component_id, members = _release_component_members(record, prefix, ids)
        target = audio if component_id == "audio-assets" else base
        _merge_component_members(target, members, component_id)
    if ids != {"document", "snapshot", "audio-assets"}:
        message = "内置收据组件集合不完整"
        raise ValueError(message)
    _validate_release_member_sets(base, audio)
    return base, audio


def scan_distribution(root: Path) -> list[str]:
    """正向核对内置基础收据/全部成员，反向拒绝默认包的音频与 Python."""
    runtime = root / "xberg"
    if not runtime.resolve().is_relative_to(root.resolve()):
        message = "内置运行目录路径逃逸"
        raise ValueError(message)
    base, _audio = _release_members(runtime)
    problems = _member_problems(runtime, base)
    for path in root.rglob("*"):
        relative = path.relative_to(root).as_posix()
        name = path.name.lower()
        if (
            _is_audio_path(relative)
            or name.startswith(("python", "xberg-audio-assets"))
            or name == "markdown-media-worker.exe"
        ):
            problems.append(f"默认包泄漏可选音频或 Python/旧运行时：{relative}")
    return problems


def manifest_has_python_entries() -> bool:
    if ASSET_MANIFEST.is_file():
        return "python" in ASSET_MANIFEST.read_text(encoding="utf-8", errors="replace").lower()
    return True  # 清单缺失本身即无法证明未携带 Python，按可疑处理。
