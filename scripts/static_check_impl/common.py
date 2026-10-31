"""static_check 公共基础：ROOT、读取与登记助手、类型守卫及 Cargo 清单解析（自 static_check.py 原样搬移）."""

from __future__ import annotations

import json
import tomllib
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import TypeIs

# 实现包比入口脚本深一层；仓库根取 parents[2]（与入口 static_check.py 的 ROOT 同值）。
ROOT = Path(__file__).resolve().parents[2]

# json.loads / tomllib.loads / getattr 的返回含 Any；经固定签名别名收口为 object，
# 随后用 TypeIs 守卫逐层校验成精确类型。
parse_json: Callable[[str], object] = json.loads
parse_toml: Callable[[str], object] = tomllib.loads
module_attr: Callable[[object, str], object] = getattr
# rules.json 单条 choice 规则至少要有的选项数；bash「命令不存在」的退出码；
# 非 ASCII 判定阈值（0x00-0x7F 在 UTF-8 与任何 ANSI 单字节代码页下解码一致）。
MIN_CHOICE_COUNT = 2
BASH_COMMAND_NOT_FOUND = 127
ASCII_LIMIT = 0x80

PRODUCTION_CRATES = (
    "foundation",
    "file-core",
    "recursive-extract",
    "directory-organizer",
    "md-tools",
    "git-tools",
    "xberg",
    "assets",
    "markdown",
    "codeagent",
    "jchocr",
    "ui",
    "gui",
)
OPTIONAL_CRATES = ("snap-ocr-core", "snap-ocr-worker")


def is_str(value: object) -> TypeIs[str]:
    return isinstance(value, str)


def is_str_obj_map(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def is_obj_list(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


checks: list[dict[str, object]] = []


# 检查在本机不可执行（依赖缺失等）；与 PASS/FAIL 区分，避免“没跑”被记成“通过”。
class SkippedError(Exception):
    pass


def check(name: str, fn: Callable[[], object]) -> None:
    try:
        details = fn()
        checks.append({"name": name, "status": "PASS", "details": details})
    except SkippedError as exc:
        checks.append({"name": name, "status": "SKIP", "details": str(exc)})
    # BLE001：这里必须捕获任意 Exception——check() 的职责就是把单个检查的任何异常记为
    # FAIL 而不是让整个脚本崩溃；可出现的异常类型无法穷举，重抛又会改变退出行为。
    # （异常转为 FAIL 条目而非静默吞掉，属检查器逐项汇总场景。）
    except Exception as exc:  # noqa: BLE001
        checks.append({"name": name, "status": "FAIL", "details": str(exc)})


def _manifest_source(manifest: Path, kind: str, target: object) -> None:
    if not is_str_obj_map(target):
        detail = f"{manifest.relative_to(ROOT)} 的 {kind} 项必须是表"
        raise AssertionError(detail)
    target_path = target.get("path")
    if target_path is None and kind == "lib":
        target_path = "src/lib.rs"
    if not is_str(target_path) or not (manifest.parent / target_path).is_file():
        detail = f"{manifest.relative_to(ROOT)} 的 {kind} 源码路径无效：{target_path}"
        raise AssertionError(detail)


def manifest_targets(manifest: Path, cargo: dict[str, object]) -> None:
    for kind in ("lib", "bin", "test", "example", "bench"):
        value = cargo.get(kind)
        if value is None:
            continue
        targets: list[object] = [value] if kind == "lib" else value if is_obj_list(value) else []
        if not targets:
            detail = f"{manifest.relative_to(ROOT)} 的 {kind} 必须是非空目标表/数组"
            raise AssertionError(detail)
        for target in targets:
            _manifest_source(manifest, kind, target)
    package = cargo.get("package")
    if not is_str_obj_map(package):
        detail = f"{manifest.relative_to(ROOT)} 的 package 必须是表"
        raise AssertionError(detail)
    build = package.get("build")
    if build is not None and build is not False:
        build_path = "build.rs" if build is True else build
        if not is_str(build_path) or not (manifest.parent / build_path).is_file():
            detail = f"{manifest.relative_to(ROOT)} 的 build 源码路径无效：{build_path}"
            raise AssertionError(detail)
