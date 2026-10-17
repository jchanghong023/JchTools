"""验收进程的隔离启动证据；不依赖 GUI 驱动或业务验收模块。."""

from __future__ import annotations

import re
import time
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import subprocess

__all__ = [
    "_CURRENT_PRODUCT_LOG",
    "_HISTORICAL_PRODUCT_LOG",
    "_ISOLATED_LOG_DIRECTORIES",
    "_is_under_tmp",
    "_isolated_log_paths",
    "_isolation_log_snapshot",
    "_process_isolation_ready",
    "_startup_event_matches",
    "_wait_process_isolation",
]

ROOT = Path(__file__).resolve().parent.parent
TMP_ROOT = ROOT / ".tmp"
_PRODUCT_LOG_DATE = r"[0-9]{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12][0-9]|3[01])"
_CURRENT_PRODUCT_LOG = re.compile(rf"jchtools\.{_PRODUCT_LOG_DATE}\.(?P<pid>[0-9]+)-[0-9]+-[0-9]+(?:\.[0-9]+)?\.log")
_HISTORICAL_PRODUCT_LOG = re.compile(
    rf"(?:jchtools\.{_PRODUCT_LOG_DATE}\.[0-9]+(?:\.[0-9]+)?\.log|jchtools\.log\.{_PRODUCT_LOG_DATE})"
)
_ISOLATED_LOG_DIRECTORIES = ("logs", "logs-fallback")
_LOG_FIELD = r'[A-Za-z_][A-Za-z0-9_]*=(?:"(?:[^"\\]|\\.)*"|[^\s"]+)'
_LOG_LINE = re.compile(rf"{_LOG_FIELD}(?:\s+{_LOG_FIELD})*")
_LOG_FIELDS = re.compile(r'(?P<key>[A-Za-z_][A-Za-z0-9_]*)=(?P<value>"(?:[^"\\]|\\.)*"|[^\s"]+)')


def _is_under_tmp(path: Path) -> bool:
    """解析链接后确认路径仍位于仓库 .tmp，避免验收输入/产物绕出隔离区。."""
    try:
        resolved = path.resolve()
    except (OSError, RuntimeError, ValueError):
        return False
    return resolved.is_relative_to(TMP_ROOT)


def _isolated_log_paths(state: Path) -> list[Path]:
    """仅枚举隔离主/备用目录中当前产品拥有的日志，不接受任意 .log 文件。."""
    return [
        path
        for directory in _ISOLATED_LOG_DIRECTORIES
        for path in (state / directory).glob("jchtools.*.log")
        if _CURRENT_PRODUCT_LOG.fullmatch(path.name) is not None and path.is_file() and _is_under_tmp(path)
    ]


def _isolation_log_snapshot(state: Path) -> frozenset[Path]:
    return frozenset(_isolated_log_paths(state))


def _startup_event_matches(line: str, pid: int, role: str) -> bool:
    """识别真实结构字段，消息字符串中的伪字段及重复归属字段不算启动证据。."""
    if _LOG_LINE.fullmatch(line) is None:
        return False
    expected = {"component": "logging", "event": "application_started", "pid": str(pid), "role": f'"{role}"'}
    fields: dict[str, list[str]] = {key: [] for key in expected}
    for match in _LOG_FIELDS.finditer(line):
        key = match["key"]
        if key in fields:
            fields[key].append(match["value"])
    return all(fields[key] == [value] for key, value in expected.items())


def _process_isolation_ready(state: Path, pid: int, role: str, existing_logs: frozenset[Path]) -> bool:
    """要求隔离 SQLite 与本轮新文件中的同角色/PID 启动事件；旧文件补写也不算证明。."""
    database = state / "config.sqlite3"
    if not database.is_file() or not _is_under_tmp(database):
        return False
    for path in _isolated_log_paths(state):
        if path in existing_logs:
            continue
        ownership = _CURRENT_PRODUCT_LOG.fullmatch(path.name)
        if ownership is None or ownership["pid"] != str(pid):
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if any(_startup_event_matches(line, pid, role) for line in text.splitlines()):
            return True
    return False


def _wait_process_isolation(
    state: Path, proc: subprocess.Popen[bytes], role: str, existing_logs: frozenset[Path], deadline: float
) -> bool:
    """在发起业务操作前有界等待实际隔离证据，无法证明时 fail-closed 拒绝。."""
    while time.monotonic() < deadline:
        if _process_isolation_ready(state, proc.pid, role, existing_logs):
            return True
        if proc.poll() is not None:
            return False
        time.sleep(min(0.1, max(0.0, deadline - time.monotonic())))
    return False
