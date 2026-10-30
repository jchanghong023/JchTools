"""使用上游 cargo-delta 影响分析选择工作区包."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, TypeIs

if TYPE_CHECKING:
    from collections.abc import Callable

_parse_json: Callable[[bytes], object] = json.loads


@dataclass(frozen=True)
class ImpactSelection:
    packages: frozenset[str]
    all_packages: frozenset[str]
    default_packages: frozenset[str]
    features: dict[str, frozenset[str]]
    baseline: str
    reason: str


def _run(root: Path, args: list[str]) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(args, cwd=root, capture_output=True, check=False)


def _mapping(value: object) -> TypeIs[dict[object, object]]:
    return isinstance(value, dict)


def _list(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


def _strings(value: object) -> list[str]:
    if not _list(value) or not all(isinstance(item, str) for item in value):
        message = "工作区元数据的字符串列表无效"
        raise ValueError(message)
    return [item for item in value if isinstance(item, str)]


def _package_details(package: object, members: set[str]) -> tuple[str, str, str, dict[object, object]] | None:
    if not _mapping(package) or not isinstance(package.get("id"), str):
        message = "工作区包元数据无效"
        raise ValueError(message)
    package_id = package["id"]
    if not isinstance(package_id, str) or package_id not in members:
        return None
    name = package["name"]
    manifest = package["manifest_path"]
    declared_features = package["features"]
    if not isinstance(name, str) or not name or not isinstance(manifest, str) or not _mapping(declared_features):
        message = "工作区包名称、清单或特性无效"
        raise ValueError(message)
    return package_id, name, manifest, declared_features


def _metadata_packages(
    root: Path, data: dict[object, object]
) -> tuple[frozenset[str], frozenset[str], dict[str, frozenset[str]]]:
    members = set(_strings(data["workspace_members"]))
    defaults = set(_strings(data["workspace_default_members"]))
    if not members or not defaults or not defaults <= members:
        message = "工作区成员无效"
        raise ValueError(message)
    package_data = data["packages"]
    if not _list(package_data):
        message = "工作区包列表无效"
        raise ValueError(message)
    names: set[str] = set()
    default_names: set[str] = set()
    features: dict[str, frozenset[str]] = {}
    seen_ids: set[str] = set()
    for package in package_data:
        details = _package_details(package, members)
        if details is None:
            continue
        package_id, name, manifest, declared_features = details
        if package_id in seen_ids or name in names or not all(isinstance(key, str) for key in declared_features):
            message = "工作区包名称、标识或特性重复或无效"
            raise ValueError(message)
        seen_ids.add(package_id)
        names.add(name)
        if package_id in defaults:
            default_names.add(name)
        features[name] = frozenset(key for key in declared_features if isinstance(key, str))
        _ = Path(manifest).resolve().parent.relative_to(root)
    if seen_ids != members:
        message = "工作区包元数据不完整"
        raise ValueError(message)
    return frozenset(names), frozenset(default_names), features


def _metadata_input(root: Path) -> dict[object, object]:
    result = _run(root, ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"])
    if result.returncode:
        message = "cargo metadata 执行失败"
        raise ValueError(message)
    data = _parse_json(result.stdout)
    if not _mapping(data):
        message = "工作区元数据不是对象"
        raise ValueError(message)
    return data


def _metadata(root: Path) -> tuple[frozenset[str], frozenset[str], dict[str, frozenset[str]]]:
    try:
        return _metadata_packages(root, _metadata_input(root))
    except (OSError, ValueError, KeyError, TypeError) as error:
        message = "无法枚举 Cargo 工作区，拒绝使用不完整的测试范围"
        raise RuntimeError(message) from error


def _delta_binary(root: Path) -> str | None:
    executable = "cargo-delta.exe" if sys.platform == "win32" else "cargo-delta"
    local = root / ".tmp" / "native-tools" / "cargo-delta" / "bin" / executable
    candidates = ([str(local)] if local.is_file() else []) + ([found] if (found := shutil.which("cargo-delta")) else [])
    for candidate in candidates:
        try:
            result = _run(root, [candidate, "delta", "--version"])
            if result.returncode == 0 and re.fullmatch(rb"cargo-delta 0\.5\.0\s*", result.stdout):
                return candidate
        except OSError:
            continue
    return None


def _native_reason(root: Path, stderr: bytes, selected: frozenset[str], all_packages: frozenset[str]) -> str:
    headers = {
        "Cargo input changes require a full workspace build due to:": "cargo-delta 全局 Cargo 输入",
        "WARNING: Trip wire activated due to changes in the following file(s):": "cargo-delta tripwire",
    }
    lines = stderr.decode("utf-8").splitlines()
    for index, line in enumerate(lines):
        if line not in headers:
            continue
        reported: list[str] = []
        for entry in lines[index + 1 :]:
            if not entry.startswith("- "):
                break
            candidate = Path(entry[2:])
            if candidate.is_absolute():
                try:
                    candidate = candidate.relative_to(root)
                except ValueError:
                    continue
            relative = candidate.as_posix()
            if ".." not in candidate.parts:
                reported.append(relative)
        detail = ", ".join(repr(path) for path in sorted(set(reported))) or "原生诊断未提供可显示的相对路径"
        return f"{headers[line]}：{detail}"
    if selected == all_packages:
        return "cargo-delta 受影响包覆盖整个工作区（未报告全局输入或 tripwire）"
    return "cargo-delta 受影响的包" if selected else "cargo-delta 未选中工作区包；本次未运行 Cargo 测试"


def _delta_impact(root: Path, binary: str, all_packages: frozenset[str]) -> tuple[frozenset[str], str]:
    try:
        env = os.environ.copy()
        if sys.platform == "win32":
            count_text = env.get("GIT_CONFIG_COUNT", "0")
            if not count_text.isascii() or not count_text.isdecimal():
                message = "GIT_CONFIG_COUNT 无效，无法追加 Git 子进程配置"
                raise RuntimeError(message)
            count = int(count_text)
            key, value = f"GIT_CONFIG_KEY_{count}", f"GIT_CONFIG_VALUE_{count}"
            if key in env or value in env:
                message = "Git 子进程配置存在未计入的配置项，无法安全追加"
                raise RuntimeError(message)
            env["GIT_CONFIG_COUNT"] = str(count + 1)
            env[key] = "core.longpaths"
            env[value] = "true"
        result = subprocess.run(
            [binary, "delta", "impact", "--base-ref", "origin/main", "--affected", "-f", "names"],
            cwd=root,
            env=env,
            capture_output=True,
            check=False,
        )
        if result.returncode:
            detail = result.stderr.decode("utf-8", errors="replace").strip()
            message = f"cargo-delta impact 执行失败（退出码 {result.returncode}）：{detail}"
            raise RuntimeError(message)
        lines = result.stdout.decode("utf-8").splitlines()
        if any(not line or line != line.strip() or line not in all_packages for line in lines):
            message = "cargo-delta 返回了无效或未知包名"
            raise RuntimeError(message)
        selected = frozenset(lines)
        if len(selected) != len(lines):
            message = "cargo-delta 返回了重复包名"
            raise RuntimeError(message)
        return selected, _native_reason(root, result.stderr, selected, all_packages)
    except (OSError, UnicodeError) as error:
        message = "cargo-delta 无法执行或返回无效 UTF-8"
        raise RuntimeError(message) from error


def select_impact(root: Path) -> ImpactSelection:
    root = root.resolve()
    all_packages, default_packages, features = _metadata(root)
    binary = _delta_binary(root)
    if binary is None:
        message = "cargo-delta 0.5.0 不可用；请运行 scripts/cargo_delta.py --install"
        raise RuntimeError(message)
    selected, reason = _delta_impact(root, binary, all_packages)
    return ImpactSelection(
        selected,
        all_packages,
        default_packages,
        features,
        "cargo-delta base-ref origin/main",
        reason,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    _ = parser.add_argument(
        "--install", action="store_true", required=True, help="将上游 cargo-delta 0.5.0 安装到 .tmp"
    )
    _ = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    destination = root / ".tmp" / "native-tools" / "cargo-delta"
    factory = subprocess.run(
        [sys.executable, str(root / "scripts" / "make_tmp.py"), "workspace", "--destination", str(destination)],
        cwd=root,
        check=False,
    )
    if factory.returncode:
        return factory.returncode
    cargo = shutil.which("cargo")
    if cargo is None:
        print("未找到 cargo，无法安装 cargo-delta", file=sys.stderr)
        return 1
    return subprocess.run(
        [
            str(Path(cargo).resolve()),
            "install",
            "cargo-delta",
            "--version",
            "=0.5.0",
            "--locked",
            "--root",
            str(destination),
        ],
        cwd=root,
        check=False,
    ).returncode


if __name__ == "__main__":
    sys.exit(main())
