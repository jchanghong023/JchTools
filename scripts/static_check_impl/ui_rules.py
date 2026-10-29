"""static_check 界面规则检查：Slint 块解析、布局宽度绑定、配色、产品命名（自 static_check.py 原样搬移）.

说明：slint_modal_gating 留在入口 static_check.py。
"""

from __future__ import annotations

import re
from typing import TYPE_CHECKING

from scripts.static_check_impl.common import ROOT, read_text

if TYPE_CHECKING:
    from collections.abc import Callable
    from pathlib import Path

# 清单来源函数定义在入口 static_check.py（AST 回归按入口命名空间覆盖 ROOT），
# 由入口在导入后注入本属性。
project_manifests: Callable[[], list[Path]]


def slint_blocks(ui: str) -> list[tuple[int, int, str]]:
    # 列表元素为 (open_pos, close_pos, 块头标识符)：块头是紧贴 { 前的最后一个标识符（如 Rectangle）。
    pairs: list[tuple[int, int, str]] = []
    stack: list[tuple[int, str]] = []
    for i, ch in enumerate(ui):
        if ch == "{":
            head = re.search(r"([A-Za-z_][\w-]*)\s*$", ui[max(0, i - 80) : i])
            stack.append((i, head.group(1) if head else ""))
        elif ch == "}":
            open_pos, head = stack.pop()
            pairs.append((open_pos, i, head))
    return pairs


def enclosing(pairs: list[tuple[int, int, str]], pos: int) -> tuple[int, int, str] | None:
    best: tuple[int, int, str] | None = None
    for open_pos, close_pos, head in pairs:
        if open_pos < pos < close_pos and (best is None or close_pos - open_pos < best[1] - best[0]):
            best = (open_pos, close_pos, head)
    return best


LAYOUTS = {"HorizontalLayout", "VerticalLayout", "GridLayout"}


def ui_slint_files() -> list[Path]:
    # UI 源按主题/控件/页面拆分为 ui/ 下多个 .slint 文件；界面规则覆盖全部文件。
    files = sorted((ROOT / "ui").rglob("*.slint"))
    if not files:
        detail = "ui/ 下没有任何 .slint 文件"
        raise AssertionError(detail)
    return files


def slint_layout_width() -> str:
    # AGENTS.md 第 4 节：布局容器直接子项不得用 root/parent.width 绑定自身宽度（绑定环）。
    # 匹配位置在元素 E 自身块内，E 的父块（再上溯一层）是布局时才违规；绝对定位下的填充用法不受影响。
    bad: list[tuple[str, int, str]] = []
    for path in ui_slint_files():
        ui = read_text(path)
        pairs = slint_blocks(ui)
        for match in re.finditer(r"(?:preferred-|min-|max-)?width\s*:\s*(root|parent)\.width\b", ui):
            inner = enclosing(pairs, match.start())
            outer = enclosing(pairs, inner[0]) if inner else None
            if outer and outer[2] in LAYOUTS:
                bad.append((path.name, ui.count("\n", 0, match.start()) + 1, match.group(0).strip()))
    if bad:
        detail = f"布局直接子项的宽度绑定了 root/parent.width（文件, 行, 表达式）：{bad}"
        raise AssertionError(detail)
    return "布局容器内没有用 root/parent.width 绑定子项自身宽度。"


def slint_colors() -> str:
    # AGENTS.md 第 4 节：十六进制颜色只允许出现在 Design 全局内（浅深主题由 Design.dark 切换）。
    # Design 全局可定义在 ui/ 下任一 .slint 文件（如 theme.slint）；颜色检查逐文件执行。
    bad: list[tuple[str, int, str]] = []
    design_found = False
    for path in ui_slint_files():
        ui = read_text(path)
        pairs = slint_blocks(ui)
        match = re.search(r"global\s+Design\s*\{", ui)
        open_pos = close_pos = -1
        if match is not None:
            design_found = True
            design = [pair for pair in pairs if pair[0] == match.end() - 1]
            if not design:
                detail = f"{path.name}：Design 块花括号配对异常"
                raise AssertionError(detail)
            open_pos, close_pos, _ = design[0]
        for hex_match in re.finditer(r"#[0-9a-fA-F]{3,8}\b", ui):
            if open_pos < hex_match.start() < close_pos:
                continue
            bad.append((path.name, ui.count("\n", 0, hex_match.start()) + 1, hex_match.group(0)))
    if not design_found:
        detail = "ui/ 下找不到 Design 全局定义"
        raise AssertionError(detail)
    if bad:
        detail = f"Design 全局之外出现硬编码十六进制颜色（文件, 行, 颜色）：{bad}"
        raise AssertionError(detail)
    return "十六进制颜色只出现在 Design 全局内。"


def product_naming() -> str:
    # AGENTS.md 第 1/5 节的产品名同步下界：标题与状态目录必须是 JchTools，且无旧名残留。
    ui = chr(10).join(read_text(path) for path in ui_slint_files())
    config = read_text(ROOT / "crates/file-core/src/config.rs")
    if not re.search(r'title:\s*"JchTools"', ui):
        detail = "窗口标题必须是产品名 JchTools"
        raise AssertionError(detail)
    # 状态定位可由 config 委托给应用共享设置；检查实际定位方，不能要求
    # 调用方为满足字符串检查重复维护产品目录名。
    shared_settings = read_text(ROOT / "crates/foundation/src/xberg_settings.rs")
    locator = shared_settings if "jchtools_foundation::xberg_settings::state_dir()" in config else config
    if not re.search(r'ProjectDirs::from\(\s*""\s*,\s*""\s*,\s*"JchTools"\s*\)', locator):
        detail = '实际状态定位方的用户数据目录名必须是 "JchTools"'
        raise AssertionError(detail)
    ui_rels = [path.relative_to(ROOT).as_posix() for path in ui_slint_files()]
    for rel in [
        *ui_rels,
        "crates/file-core/src/config.rs",
        "crates/foundation/src/xberg_settings.rs",
        "crates/gui/src/registry.rs",
        *(path.relative_to(ROOT).as_posix() for path in project_manifests()),  # noqa: F821  # 入口注入。
    ]:
        if "MyTools" in read_text(ROOT / rel):
            detail = f"{rel} 残留旧产品名 MyTools"
            raise AssertionError(detail)
    return "窗口标题、状态目录与 Cargo 清单中的产品名一致（JchTools），无旧名残留。"
