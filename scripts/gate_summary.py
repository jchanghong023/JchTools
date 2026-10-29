"""门结果校验、验收覆盖与 PowerShell 兼容辅助."""

from __future__ import annotations

import contextlib
import io
import os
import re
import shutil
import subprocess
import sys
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pathlib import Path


BINDING_LOOP_SAMPLE_LIMIT = 6


# 常量名避开 pass 字样（S105 会把含 pass 的变量名当作疑似硬编码口令）。
STATUS_OK = "PASS"
STATUS_FAILED = "FAIL"
STATUS_UNVERIFIED = "UNVERIFIED"
STATUS_NOT_RUN = "NOT RUN"

# 独立旧桌面验收的覆盖判据保持原样；这些驱动不属于三个门的鼠标隔离范围。
ENVIRONMENT_BLOCKED_ACCEPTANCE_ITEMS: frozenset[str] = frozenset({"C08", "C09"})
# 从 acceptance 汇总行提取未执行条目号（acceptance.ps1 在汇总行携带条目号清单；
# 逐项行与汇总行两种形态都按「字母+两位数字」在行内任意位置提取）。
_NOT_RUN_ITEM_ID_PATTERN = re.compile(r"\b([A-Z]\d{2})\b")


def reconfigure_stdout() -> None:
    # 阶段明细含中文；Windows 控制台默认代码页会把 print 变成 UnicodeEncodeError。
    with contextlib.suppress(AttributeError):
        stream = sys.stdout
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")


def binding_loop_problem(log: Path) -> str | None:
    # 产品合同禁止 binding loop，即使命令退出码为零也不得通过。
    text = log.read_text(encoding="utf-8", errors="replace")
    hits = [line.strip() for line in text.splitlines() if "binding loop" in line]
    if hits:
        return "构建输出出现 binding loop 警告：\n" + "\n".join(hits[:BINDING_LOOP_SAMPLE_LIMIT])
    return None


def acceptance_coverage_gaps(text: str) -> list[str]:
    """提取 acceptance 汇总中的必要覆盖缺口；package 的 NOT RUN 不在本级范围.

    markdown-acceptance 的 NOT RUN 仅在可提取条目号、且条目号全部属于环境受限
    集合（C08/C09，须独立 Windows 用户会话驱动正式包 GUI；AGENTS.md 3.4 例外）
    时放行；PARTIAL 始终阻塞。提取不到条目号一律按缺口处理（fail-closed），
    放行不改变条目本身的 NOT RUN 事实，不表述为已验证。
    """
    required = ("markdown-acceptance", "snap-ocr-worker-root")
    gaps: list[str] = []
    lines = text.splitlines()
    for name in required:
        summaries = [line.strip() for line in lines if name in line and line.strip()]
        if not summaries:
            gaps.append(f"缺少必要覆盖汇总：{name}")
            continue
        blocking = [line for line in summaries if line.startswith(("NOT RUN ", "PARTIAL "))]
        if not blocking:
            continue
        if name == "markdown-acceptance" and all(line.startswith("NOT RUN ") for line in blocking):
            item_ids: set[str] = set()
            for line in blocking:
                item_ids.update(_NOT_RUN_ITEM_ID_PATTERN.findall(line))
            if item_ids and item_ids <= ENVIRONMENT_BLOCKED_ACCEPTANCE_ITEMS:
                continue
        gaps.append(blocking[0])
    return gaps


def ps51_module_path() -> str | None:
    r"""给 Windows PowerShell 5.1 子进程用的 PSModulePath：剔除 PowerShell 7 的模块目录.

    调用方本身跑在 PowerShell 7 会话里时，进程继承的 PSModulePath 以
    `c:\program files\powershell\7\Modules` 开头；5.1 会优先自动加载该目录下标注
    CompatiblePSEditions=Core 的 Microsoft.PowerShell.Utility，加载失败后 Get-FileHash
    等内置 cmdlet 变成 CommandNotFoundException，acceptance.ps1 的打包阶段随即失败
    （本机实测反证：PSModulePath 收窄到 Windows PowerShell 自身目录后 Get-FileHash 立即可用）。
    只影响本进程树继承的环境，不改系统设置。
    """
    # Windows 环境变量名大小写不敏感，官方拼写是 PSModulePath；此处按现有值读取。
    # [quality-baseline approved 2026-10-03] 官方拼写误报，经用户裁定保留
    value = os.environ.get("PSModulePath")  # noqa: SIM112
    if not value:
        return None
    keep = [
        part
        for part in value.split(os.pathsep)
        if part
        and "\\powershell\\7\\" not in f"{part.lower()}\\"
        and "\\documents\\powershell\\" not in f"{part.lower()}\\"
    ]
    return os.pathsep.join(keep) or None


def version_line(name: str) -> str:
    exe = shutil.which(name)
    if exe is None:
        return f"{name}: 未找到（PATH 缺失）"
    done = subprocess.run([exe, "--version"], capture_output=True, text=True, check=False)
    lines = (done.stdout or done.stderr).strip().splitlines()
    return f"{name}: {lines[0] if lines else '版本未知'}"
