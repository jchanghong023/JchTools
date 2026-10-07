"""生成全部有效需求的测试引用审计；源码引用不是已执行或已覆盖的证明。."""

from __future__ import annotations

import hashlib
import io
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPORT = ROOT / ".tmp" / "test-gate" / "requirements-map.json"
IDENTIFIER = re.compile(r"\b(?:XB|H|P|X|C|M|G|S|R|U|E|T|O)-\d{2}\b")
ASSERTION = re.compile(r"^- \*\*((?:XB|H|P|X|C|M|G|S|R|U|E|T|O)-\d{2})\*\*\s+(.+)$")
RANGE = re.compile(r"\b((?:XB|H|P|X|C|M|G|S|R|U|E|T|O))-(\d{2})\s*[\uff5e\u2013\u2014~]\s*(?:\1-)?(\d{2})\b")


def referenced_ids(line: str) -> set[str]:
    """展开既有注释中的编号区间；只建立待复核引用，不推断断言强度。."""
    result = set(IDENTIFIER.findall(line))
    for match in RANGE.finditer(line):
        prefix, first, last = match.groups()
        result.update(f"{prefix}-{number:02}" for number in range(int(first), int(last) + 1))
    return result


def requirement_rows(directory: Path) -> list[dict[str, str | int]]:
    rows: list[dict[str, str | int]] = []
    seen: set[str] = set()
    for document in sorted(directory.glob("*.md")):
        for number, line in enumerate(document.read_text(encoding="utf-8").splitlines(), 1):
            match = ASSERTION.match(line)
            if match is None:
                continue
            identifier, behavior = match.groups()
            if identifier in seen:
                message = f"需求编号重复：{identifier}"
                raise ValueError(message)
            seen.add(identifier)
            rows.append({"id": identifier, "document": document.name, "line": number, "behavior": behavior})
    if not rows:
        message = "没有找到有效需求，不能生成空白通过报告"
        raise ValueError(message)
    return rows


def reference_rows(root: Path) -> dict[str, list[dict[str, str | int]]]:
    references: dict[str, list[dict[str, str | int]]] = {}
    sources = [
        *root.glob("tests/**/*.rs"),
        *root.glob("src/**/*.rs"),
        *root.glob("optional/*/src/**/*.rs"),
        *root.glob("scripts/*.py"),
    ]
    for source in sorted(sources):
        if source.name == "requirement_coverage.py":
            continue
        lines = source.read_text(encoding="utf-8").splitlines()
        for number, line in enumerate(lines, 1):
            # Rust 产品注释和 Python 文档文字也可能引用编号；明确标为候选而非测试证明。
            for identifier in sorted(referenced_ids(line)):
                references.setdefault(identifier, []).append({"file": str(source.relative_to(root)), "line": number})
    return references


def build_report(root: Path) -> dict[str, object]:
    references = reference_rows(root)
    rows = requirement_rows(root / "docs" / "requirements")
    items = [
        {
            **row,
            "candidate_references": references.get(str(row["id"]), []),
            "status": "UNVERIFIED",
            "reason": "引用需要逐条复核实际断言及本轮真实入口执行证据；引用存在不代表覆盖或通过",
        }
        for row in rows
    ]
    snapshots = {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted((root / "docs" / "requirements").glob("*.md"))
    }
    return {
        "kind": "需求与测试引用审计（不是验收通过证明）",
        "requirement_documents": snapshots,
        "items": items,
        "summary": {
            "total": len(items),
            "without_reference": sum(not item["candidate_references"] for item in items),
            "verified": 0,
            "unverified": len(items),
        },
    }


def main() -> int:
    if isinstance(sys.stdout, io.TextIOWrapper):
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    _ = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "make_tmp.py"), "workspace", "--destination", str(REPORT.parent)],
        check=True,
    )
    report = build_report(ROOT)
    _ = REPORT.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"需求引用清单已生成：{REPORT}")
    print(f"UNVERIFIED：完整覆盖及无人值守目标须以逐条断言和真实执行证据判定；清单统计 {report['summary']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
