"""生成全部有效需求的测试引用审计；源码引用不是已执行或已覆盖的证明。."""

from __future__ import annotations

import hashlib
import io
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Iterator

ROOT = Path(__file__).resolve().parent.parent
REPORT = ROOT / ".tmp" / "test-gate" / "requirements-map.json"
_PREFIX = r"(?:AH|CA|XB|H|P|X|C|M|G|S|R|U|E|T|O)"
IDENTIFIER = re.compile(r"\b" + _PREFIX + r"-\d{2}\b")
ASSERTION = re.compile(r"^### Requirement:\s+(" + _PREFIX + r"-\d{2})(?:\s+(.+))?$")
RANGE = re.compile(r"\b(" + _PREFIX + r")-(\d{2})\s*[\uff5e\u2013\u2014~]\s*(?:\1-)?(\d{2})\b")

# 本轮审查的主要验收方式提示，不代表全部需求已完成类型审查。
# 非目标不要求行为测试；任何类别均不构成验收通过状态。
VERIFICATION_KINDS = {
    "H-08": "NON_GOAL",
    "U-04": "NON_GOAL",
    "P-14": "ARCHITECTURE",
    "O-02": "ARCHITECTURE",
    "P-05": "DELIVERY",
    "E-04": "DELIVERY",
    "U-08": "DELIVERY",
    "O-31": "DELIVERY",
    "O-04": "MEASUREMENT",
    "O-32": "MEASUREMENT",
    "XB-13": "COMPARISON",
}
SOURCE_GLOBS = (
    "scripts/*.py",
    "scripts/*.ps1",
    "scripts/*.cmd",
    ".github/workflows/*.yml",
    ".github/workflows/*.yaml",
    "installer/*.iss",
)


def referenced_ids(line: str) -> set[str]:
    """展开既有注释中的编号区间；只建立待复核引用，不推断断言强度。."""
    result = set(IDENTIFIER.findall(line))
    for match in RANGE.finditer(line):
        prefix, first, last = match.groups()
        result.update(f"{prefix}-{number:02}" for number in range(int(first), int(last) + 1))
    return result


def _outside_fences(lines: list[str]) -> Iterator[tuple[int, str]]:
    """按原始行号产出围栏代码块之外的行；围栏开闭与缩进口径与 CommonMark 对齐。."""
    fence = ""
    for number, line in enumerate(lines, 1):
        marker = re.match(r"^\s{0,3}(`{3,}|~{3,})", line)
        if marker:
            token = marker.group(1)
            if not fence:
                fence = token
            elif token[0] == fence[0] and len(token) >= len(fence) and not line[marker.end() :].strip():
                fence = ""
            continue
        if fence:
            continue
        yield number, line


def requirement_rows(directory: Path) -> list[dict[str, str | int]]:
    rows: list[dict[str, str | int]] = []
    seen: set[str] = set()
    for document in sorted(directory.glob("**/spec.md")):
        in_requirements = False
        current: dict[str, str | int] | None = None
        lines = document.read_text(encoding="utf-8-sig").splitlines()
        for number, line in _outside_fences(lines):
            if line.startswith("## "):
                in_requirements = line.strip() == "## Requirements"
                current = None
                continue
            if not in_requirements:
                continue
            match = ASSERTION.match(line)
            if match is None:
                if current is not None and not current["behavior"] and line.strip():
                    current["behavior"] = line.strip()
                continue
            identifier, behavior = match.groups()
            if identifier in seen:
                message = f"需求编号重复：{identifier}"
                raise ValueError(message)
            seen.add(identifier)
            current = {
                "id": identifier,
                "document": document.relative_to(directory).as_posix(),
                "line": number,
                "behavior": behavior or "",
            }
            rows.append(current)
    if not rows:
        message = "没有找到有效需求，不能生成空白通过报告"
        raise ValueError(message)
    return rows


def reference_rows(root: Path) -> dict[str, list[dict[str, str | int]]]:
    references: dict[str, list[dict[str, str | int]]] = {}
    # 与静态测试发现相同的手写 Rust 位置；可选组件和 build/examples/benches
    # 中的引用同样只是候选，不因所在目录而被遗漏或被当作执行证明。
    package_roots = [root, *root.glob("crates/*"), *root.glob("optional/*")]
    sources = {source for pattern in SOURCE_GLOBS for source in root.glob(pattern)}
    for package in package_roots:
        build_script = package / "build.rs"
        if build_script.is_file():
            sources.add(build_script)
        for directory in ("src", "tests", "examples", "benches"):
            sources.update((package / directory).glob("**/*.rs"))
    for source in sorted(sources):
        if source.name == "requirement_coverage.py":
            continue
        lines = source.read_text(encoding="utf-8-sig").splitlines()
        for number, line in enumerate(lines, 1):
            # 产品注释、脚本和交付配置中的编号均只是候选引用，不是测试证明。
            for identifier in sorted(referenced_ids(line)):
                references.setdefault(identifier, []).append({"file": str(source.relative_to(root)), "line": number})
    return references


def build_report(root: Path) -> dict[str, object]:
    references = reference_rows(root)
    rows = requirement_rows(root / "openspec" / "specs")
    items = [
        {
            **row,
            "verification_kind": VERIFICATION_KINDS.get(str(row["id"]), "BEHAVIOR"),
            "candidate_references": references.get(str(row["id"]), []),
            "status": "UNVERIFIED",
            "reason": "引用需要逐条复核实际断言及本轮真实入口执行证据；引用存在不代表覆盖或通过",
        }
        for row in rows
    ]
    snapshots = {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in sorted((root / "openspec" / "specs").glob("**/spec.md"))
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
