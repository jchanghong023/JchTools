"""转 Markdown 验收 CLI 参数与条目选择（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import argparse
import sys
from typing import TYPE_CHECKING, override

from scripts.markdown_acceptance_impl.items import ITEMS

if TYPE_CHECKING:
    from typing import NoReturn

    from scripts.markdown_acceptance_impl.model import Item

__all__ = [
    "_ArgumentParser",
    "_Arguments",
    "_select_items",
]


class _ArgumentParser(argparse.ArgumentParser):
    """验收入口参数错误使用文档承诺的退出码 3（测试未运行仍专用退出码 2）."""

    @override
    def error(self, message: str) -> NoReturn:
        self.print_usage(sys.stderr)
        self.exit(3, f"{self.prog}: error: {message}\n")


class _Arguments(argparse.Namespace):
    """带类型标注的解析结果；类属性即默认值（argparse 仅对缺失属性写默认值）."""

    list_only: bool = False
    only: str = ""
    profile: str = "common"
    gui_exe: str = ""
    portable_root: str = ""
    installed_root: str = ""
    fixtures: str = ""
    json_report: str = ""
    seed_state: str = ""
    seed_xberg: str = ""


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
