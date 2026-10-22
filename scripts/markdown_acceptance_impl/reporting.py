"""转 Markdown 验收汇总与报告输出（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import dataclasses
import json
import time
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.markdown_acceptance_impl.items import ITEMS
from scripts.markdown_acceptance_impl.model import STATUS_FAILED, STATUS_NOT_RUN, STATUS_OK, STATUS_OPTIONAL, Outcome

if TYPE_CHECKING:
    from scripts.markdown_acceptance_impl.model import Item

__all__ = [
    "COMMON_FORMATS",
    "OPTIONAL_COMMON_ITEMS",
    "_fixture_summary",
    "_print_report",
    "print_list",
    "profile_items",
]


COMMON_FORMATS = frozenset(("doc", "docx", "xls", "xlsx", "ppt", "pptx", "pdf", "mp4"))


OPTIONAL_COMMON_ITEMS = frozenset(("A16", "A17", "A18", "A19", "A20"))


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


def profile_items(profile: str) -> list[Item]:
    """当前用户确认的必验范围；保留原完整矩阵供显式扩展使用。."""
    if profile == "full":
        return list(ITEMS)
    result: list[Item] = []
    for original in ITEMS:
        item = dataclasses.replace(original)
        if item.group == "A" and item.item_id not in OPTIONAL_COMMON_ITEMS:
            item.fixtures = tuple(
                name for name in item.fixtures if Path(name).suffix.lstrip(".").lower() in COMMON_FORMATS
            )
            if item.synth == "office":
                item.synth = ""
            if item.item_id in ("A21", "A22", "A23"):
                extension = {"A21": "DOCX", "A22": "PPTX", "A23": "XLSX"}[item.item_id]
                item.title = f"{extension}：真实正文与原文件保护（其余变体为可选扩展）"
            if item.item_id == "A24":
                item.title = "MP4：真实中文转录、时间戳与原文件保护"
                item.synth = ""
            if item.item_id == "A25":
                item.title = "旧 Office DOC/XLS/PPT：真实正文与原文件保护"
                item.fixtures = ("common/legacy.doc", "common/legacy.xls", "common/legacy.ppt")
        result.append(item)
    return result


def _print_report(results: list[tuple[Item, Outcome]], report_path: Path | None, profile: str = "full") -> int:
    # P-12/P-13：固定验收全集；--only 只控制执行，不能缩小报告分母。
    observed = {item.item_id: outcome for item, outcome in results}
    results = [
        (
            item,
            observed.get(
                item.item_id,
                Outcome(
                    STATUS_OPTIONAL
                    if profile == "common" and item.item_id in OPTIONAL_COMMON_ITEMS
                    else STATUS_NOT_RUN,
                    "保留的扩展格式测试，本次不属于必验范围"
                    if profile == "common" and item.item_id in OPTIONAL_COMMON_ITEMS
                    else "本次未选择执行",
                ),
            ),
        )
        for item in profile_items(profile)
    ]
    print()
    print("==== 转 Markdown 验收汇总 ====")
    for item, outcome in results:
        print(f"{outcome.status:8} {item.item_id}  {item.title}")
        if outcome.reason:
            print(f"         原因：{outcome.reason}")
        for detail in outcome.details:
            print(f"         {detail}")
    counts = {STATUS_OK: 0, STATUS_FAILED: 0, STATUS_NOT_RUN: 0, STATUS_OPTIONAL: 0}
    for _, outcome in results:
        counts[outcome.status] += 1
    print("-" * 118)
    total = len(results)
    line = f"PASS {counts[STATUS_OK]}| FAIL {counts[STATUS_FAILED]}| NOT RUN {counts[STATUS_NOT_RUN]}（共 {total} 条）"
    print(line)
    print(f"profile={profile}；OPTIONAL {counts[STATUS_OPTIONAL]}（保留，不计为通过）")
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
            "profile": profile,
            "summary": {
                "pass": counts[STATUS_OK],
                "fail": counts[STATUS_FAILED],
                "not_run": counts[STATUS_NOT_RUN],
                "optional": counts[STATUS_OPTIONAL],
            },
        }
        _ = report_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
        print(f"JSON 报告：{report_path}")
    if counts[STATUS_FAILED]:
        return 1
    if counts[STATUS_NOT_RUN] or counts[STATUS_OK] == 0:
        return 2
    return 0
