"""比较冻结 Python 与 Rust 在同一张合成截图上的可观察 OCR 结果.

用法：python tests/ocr_fixtures/compare.py --expected ... --manifest ...
      --python <旧版报告.json> [--python ...] --rust <新版报告.json>
      [--rust ...] --out .tmp/ocr-assets/comparison.json
报告由旧仓库 scripts/evaluate_ocr_fixture.py 和本仓库 ocr_compare example 产生。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import statistics
from pathlib import Path
from typing import Any


def distance(left: str, right: str) -> int:
    """逐字符 Levenshtein 距离."""
    if len(left) < len(right):
        left, right = right, left
    previous = list(range(len(right) + 1))
    for row, char in enumerate(left, 1):
        current = [row]
        for column, other in enumerate(right, 1):
            current.append(min(current[-1] + 1, previous[column] + 1, previous[column - 1] + (char != other)))
        previous = current
    return previous[-1]


def anchors(text: str, names: list[str]) -> dict[str, dict[str, int] | None]:
    """定位锚点所在行及半角网格列."""
    result: dict[str, dict[str, int] | None] = {}
    for name in names:
        hits = [(row, line.index(name)) for row, line in enumerate(text.splitlines()) if name in line]
        result[name] = {"row": hits[0][0], "column": hits[0][1]} if len(hits) == 1 else None
    return result


def metrics(reference: str, actual: str, names: list[str]) -> dict[str, Any]:
    compact_reference = "".join(reference.split())
    compact_actual = "".join(actual.split())
    return {
        "exact_cer": distance(reference, actual) / max(1, len(reference)),
        "compact_cer": distance(compact_reference, compact_actual) / max(1, len(compact_reference)),
        "lines": len(actual.splitlines()),
        "blank_lines": sum(not line.strip() for line in actual.splitlines()),
        "anchors": anchors(actual, names),
    }


def load_reports(paths: list[Path]) -> tuple[str, list[float], list[float]]:
    texts: list[str] = []
    initialization: list[float] = []
    recognition: list[float] = []
    for path in paths:
        report = json.loads(path.read_text(encoding="utf-8"))
        if report["status"] != "success":
            message = f"{path}: OCR 未成功"
            raise ValueError(message)
        texts.append(str(report["actual_text"]))
        initialization.append(float(report["initialization_seconds"]))
        value = report["recognition_seconds"]
        recognition.extend(float(item) for item in (value if isinstance(value, list) else [value]))
    if len(set(texts)) != 1:
        message = "同一实现的重复识别文本不一致"
        raise ValueError(message)
    return texts[0], initialization, recognition


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--python", dest="python_reports", type=Path, action="append", required=True)
    parser.add_argument("--rust", dest="rust_reports", type=Path, action="append", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    expected = args.expected.read_text(encoding="utf-8").rstrip("\n").replace("\r\n", "\n")
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    names: list[str] = manifest["rendering"]["geometry_anchors"]
    python_text, python_load, python_ocr = load_reports(args.python_reports)
    rust_text, rust_load, rust_ocr = load_reports(args.rust_reports)
    old = metrics(expected, python_text, names)
    new = metrics(expected, rust_text, names)
    reference_anchors = anchors(expected, names)
    anchor_deltas: dict[str, Any] = {}
    for name in names:
        ref = reference_anchors[name]
        before = old["anchors"][name]
        after = new["anchors"][name]
        anchor_deltas[name] = {
            "reference": ref,
            "python": before,
            "rust": after,
            "python_error": None if ref is None or before is None else [before[k] - ref[k] for k in ("row", "column")],
            "rust_error": None if ref is None or after is None else [after[k] - ref[k] for k in ("row", "column")],
        }
    report = {
        "image_reports": {"python": [str(p) for p in args.python_reports], "rust": [str(p) for p in args.rust_reports]},
        "reference_sha256": hashlib.sha256(expected.encode("utf-8")).hexdigest(),
        "python": {"metrics": old, "load_seconds": python_load, "ocr_seconds": python_ocr},
        "rust": {"metrics": new, "load_seconds": rust_load, "ocr_seconds": rust_ocr},
        "anchor_deltas": anchor_deltas,
        "median_load_seconds": {"python": statistics.median(python_load), "rust": statistics.median(rust_load)},
        "median_ocr_seconds": {"python": statistics.median(python_ocr), "rust": statistics.median(rust_ocr)},
        "texts_identical": python_text == rust_text,
    }
    args.out.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(
        json.dumps(
            {key: report[key] for key in ("median_load_seconds", "median_ocr_seconds", "texts_identical")},
            ensure_ascii=False,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
