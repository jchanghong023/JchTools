"""转 Markdown 验收运行前置判定（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import re
from typing import TYPE_CHECKING

from scripts import isolation_probe

if TYPE_CHECKING:
    from pathlib import Path

    from scripts.markdown_acceptance_impl.model import Item

__all__ = [
    "_c03_normal_state_artifact",
    "_fixture_precondition",
]


def _fixture_precondition(item: Item, fixtures_dir: Path) -> str | None:
    if not item.fixtures:
        return None
    missing = [name for name in item.fixtures if not (fixtures_dir / name).is_file()]
    if missing:
        return f"缺夹具（{len(missing)} 个）：{missing}；构造方法见 --list 条目说明"
    return None


def _c03_normal_state_artifact(path: Path, state_root: Path) -> bool:
    """只允许源码声明的普通状态落位，不按名称豁免未知子树或下载目录。."""
    relative = path.relative_to(state_root).as_posix()
    task_directory = r"tasks/\d{8}T\d{6}-[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"
    if path.is_dir():
        return (
            relative in (*isolation_probe._ISOLATED_LOG_DIRECTORIES, "tasks")  # noqa: SLF001  # 固定共享日志目录。
            or re.fullmatch(task_directory, relative) is not None
        )
    if path.parent == state_root and path.name in ("logs.retention.lock", "logs-fallback.retention.lock"):
        return True
    if (
        path.parent.name in isolation_probe._ISOLATED_LOG_DIRECTORIES  # noqa: SLF001  # 固定共享日志目录。
        and path.parent.parent == state_root
    ):
        # 源码仅保留精确命名的历史日志；历史文件不能作为本轮启动证据。
        return (
            isolation_probe._CURRENT_PRODUCT_LOG.fullmatch(path.name) is not None  # noqa: SLF001  # 固定共享日志语法。
            or isolation_probe._HISTORICAL_PRODUCT_LOG.fullmatch(path.name) is not None  # noqa: SLF001  # 固定共享日志语法。
        )
    return any(
        re.fullmatch(pattern, relative) is not None
        for pattern in (
            r"organizer\.lock",
            r"(?:config|hash-cache)\.sqlite3(?:-(?:wal|shm|journal))?",
            r"xberg-[A-Za-z0-9_.-]+\.lock",
            task_directory + r"/task\.sqlite3(?:-(?:wal|shm|journal))?",
        )
    )
