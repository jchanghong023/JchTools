"""比较冻结 Python 与 Rust 在同一张合成截图上的可观察 OCR 结果.

用法：python tests/ocr_fixtures/compare.py --expected ... --manifest ...
      --python <旧版报告.json> [--python ...] --rust <新版报告.json>
      [--rust ...] --out .tmp/ocr-assets/comparison.json
      [--max-cer 0.02] [--max-anchor-delta 1]
报告由旧仓库 scripts/evaluate_ocr_fixture.py 和本仓库 ocr_compare example 产生。

退出码约定：0 = Rust 指标在阈值内通过；1 = CER 或布局锚点偏差超阈（JSON 的
failures 逐条标注原因）；命令行缺参为 argparse 的退出码 2；未捕获异常统一为
退出码 3（崩溃诊断，不产出 JSON）。阈值门只作用于 Rust（迁入版）的紧凑 CER
与锚点行列偏差；Python 是冻结参照，数值只记录不设门。紧凑 CER 即 2026-09-27
基线与附录 C「正文字符错误率 ≤ 2%」的度量口径——含空白的 exact CER 只记录
不设门（冻结 Python 基线自身约 2.2%，以其设门会把与基线逐字符一致的健康结果
误判为失败；空白回归由锚点偏差与 texts_identical 报告承接）。布局列按 East
Asian Width 半角单元计（W/F 计 2、组合字符计 0，与 SNAP2TEXT 附录 B 的 Rust
侧口径一致）。--out 只接受仓库根 .tmp/ 之下的路径（AGENTS.md §2），拒绝仓库
外同名 .tmp。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import statistics
import tempfile
import unicodedata
from pathlib import Path
from typing import Any
from unittest.mock import patch

REPO_ROOT = Path(__file__).resolve().parents[2]


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


def halfwidth_units(text: str) -> int:
    """East Asian Width 口径的半角单元数：W/F 计 2，组合字符计 0，其余计 1."""
    total = 0
    for char in text:
        if unicodedata.combining(char):
            continue
        total += 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
    return total


def anchors(text: str, names: list[str]) -> dict[str, dict[str, int] | None]:
    """定位锚点所在行及半角网格列（列按 halfwidth_units 口径，行按行号计）."""
    result: dict[str, dict[str, int] | None] = {}
    for name in names:
        hits = [
            (row, halfwidth_units(line[: line.index(name)]))
            for row, line in enumerate(text.splitlines())
            if name in line
        ]
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


def ensure_repo_tmp_output(path: Path) -> Path:
    """输出必须位于仓库根的真实 .tmp/ 之下（AGENTS.md §2），拒绝重解析点逃逸."""
    resolved = path.resolve()
    repo_root = REPO_ROOT.resolve()
    expected_tmp_root = repo_root / ".tmp"
    tmp_root = (REPO_ROOT / ".tmp").resolve()
    if tmp_root != expected_tmp_root or tmp_root not in resolved.parents:
        message = f"输出必须位于仓库 {expected_tmp_root} 之下：{resolved}"
        raise ValueError(message)
    return resolved


def evaluate_thresholds(
    names: list[str],
    reference_anchors: dict[str, dict[str, int] | None],
    rust_metrics: dict[str, Any],
    max_cer: float,
    max_anchor_delta: int,
) -> list[str]:
    """按附录 C 口径评估 Rust 指标，返回逐条失败原因（空列表 = 通过）."""
    failures: list[str] = []
    compact_cer = rust_metrics["compact_cer"]
    rust_anchors = rust_metrics["anchors"]
    if compact_cer > max_cer:
        failures.append(f"rust compact_cer {compact_cer:.6f} 超过阈值 {max_cer}")
    for name in names:
        reference = reference_anchors[name]
        actual = rust_anchors[name]
        if reference is None:
            continue
        if actual is None:
            failures.append(f"锚点 {name} 未在 Rust 输出中出现或出现多次")
            continue
        row_delta = abs(actual["row"] - reference["row"])
        column_delta = abs(actual["column"] - reference["column"])
        if row_delta > max_anchor_delta:
            failures.append(f"锚点 {name} 行偏差 {row_delta} 超过阈值 {max_anchor_delta}")
        if column_delta > max_anchor_delta:
            failures.append(f"锚点 {name} 列偏差 {column_delta} 超过阈值 {max_anchor_delta}")
    return failures


def self_test() -> None:
    """内嵌自测（--self-test）：锚点列宽度与输出路径守卫，仅在仓库 .tmp/ 下创建临时项."""
    # 全宽（F）与中文（W）各计 2：ａｂ中文 = 8 个半角单元，x 的网格列 = 8。
    assert anchors("\uff41\uff42中文x", ["x"]) == {"x": {"row": 0, "column": 8}}
    # 组合字符计 0：cafe + combining acute 后 END 起始列 = 4（不是码点数 5）。
    assert anchors("cafe\u0301END", ["END"]) == {"END": {"row": 0, "column": 4}}
    # 纯 ASCII 半宽：码点数即列数，口径不回归。
    assert anchors("abc END", ["END"]) == {"END": {"row": 0, "column": 4}}
    # 行（row）按行号计，不受宽度口径影响。
    assert anchors("前\n中文END", ["END"]) == {"END": {"row": 1, "column": 4}}
    # 锚点出现多次时不可度量，保持 None。
    assert anchors("x\nx", ["x"]) == {"x": None}
    # 阈值评估：紧凑 CER 超限、锚点缺失、行列超差逐条给出原因。
    failures = evaluate_thresholds(
        ["A", "B", "C"],
        {"A": {"row": 0, "column": 2}, "B": {"row": 1, "column": 8}, "C": {"row": 2, "column": 4}},
        {
            "compact_cer": 0.03,
            "anchors": {"A": {"row": 0, "column": 4}, "B": None, "C": {"row": 4, "column": 4}},
        },
        max_cer=0.02,
        max_anchor_delta=1,
    )
    assert failures == [
        "rust compact_cer 0.030000 超过阈值 0.02",
        "锚点 A 列偏差 2 超过阈值 1",
        "锚点 B 未在 Rust 输出中出现或出现多次",
        "锚点 C 行偏差 2 超过阈值 1",
    ], failures
    # 输出守卫：仓库根 .tmp/ 之下允许（含尚未存在的路径）。
    ensure_repo_tmp_output(REPO_ROOT / ".tmp" / "ocr-assets" / "comparison.json")
    # 仓库外系统临时目录同名 .tmp、仓库内非 .tmp、仓库旁 .tmp 均必须拒绝。
    for rejected in (
        Path(tempfile.gettempdir()) / ".tmp" / "compare.json",
        REPO_ROOT / "tests" / "compare.json",
        REPO_ROOT.parent / ".tmp" / "compare.json",
    ):
        try:
            ensure_repo_tmp_output(rejected)
        except ValueError:
            continue
        message = f"越界输出路径必须被拒绝：{rejected}"
        raise AssertionError(message)

    # 若仓库 .tmp 根本身或测试路径中的 .tmp 被重解析到其根外，都必须拒绝。
    repo_tmp = REPO_ROOT / ".tmp"
    expected_repo_tmp = REPO_ROOT.resolve() / ".tmp"
    if repo_tmp.resolve() != expected_repo_tmp:
        try:
            ensure_repo_tmp_output(repo_tmp / "escaped.json")
        except ValueError:
            return
        message = "仓库 .tmp 重解析到根外时必须拒绝输出"
        raise AssertionError(message)
    repo_tmp.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="compare-path-", dir=repo_tmp) as temporary:
        sandbox = Path(temporary)
        fake_repo = sandbox / "repo"
        outside = sandbox / "outside"
        fake_repo.mkdir()
        outside.mkdir()
        linked_tmp = fake_repo / ".tmp"
        try:
            linked_tmp.symlink_to(outside, target_is_directory=True)
        except (NotImplementedError, OSError):
            return
        with patch(f"{__name__}.REPO_ROOT", fake_repo):
            try:
                ensure_repo_tmp_output(linked_tmp / "escaped.json")
            except ValueError:
                pass
            else:
                message = "仓库 .tmp 重解析到根外时必须拒绝输出"
                raise AssertionError(message)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--expected", type=Path)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--python", dest="python_reports", type=Path, action="append")
    parser.add_argument("--rust", dest="rust_reports", type=Path, action="append")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--max-cer", type=float, default=0.02, help="Rust 端 CER 阈值（附录 C：≤ 2%%）")
    parser.add_argument(
        "--max-anchor-delta", type=int, default=1, help="锚点行/列偏差阈值（附录 C：一个半角单元或一行）"
    )
    parser.add_argument("--self-test", action="store_true", help="运行内嵌自测后退出，不读写报告")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        print("PASS compare.py 自测通过")
        return 0
    missing = [
        flag
        for flag, value in (
            ("--expected", args.expected),
            ("--manifest", args.manifest),
            ("--python", args.python_reports),
            ("--rust", args.rust_reports),
            ("--out", args.out),
        )
        if value is None
    ]
    if missing:
        parser.error("比较模式缺少必填参数：" + ", ".join(missing))
    assert args.expected is not None
    assert args.manifest is not None
    assert args.out is not None
    assert args.python_reports is not None
    assert args.rust_reports is not None

    output = ensure_repo_tmp_output(args.out)
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
    failures = evaluate_thresholds(
        names,
        reference_anchors,
        new,
        args.max_cer,
        args.max_anchor_delta,
    )
    report = {
        "image_reports": {"python": [str(p) for p in args.python_reports], "rust": [str(p) for p in args.rust_reports]},
        "reference_sha256": hashlib.sha256(expected.encode("utf-8")).hexdigest(),
        "python": {"metrics": old, "load_seconds": python_load, "ocr_seconds": python_ocr},
        "rust": {"metrics": new, "load_seconds": rust_load, "ocr_seconds": rust_ocr},
        "anchor_deltas": anchor_deltas,
        "median_load_seconds": {"python": statistics.median(python_load), "rust": statistics.median(rust_load)},
        "median_ocr_seconds": {"python": statistics.median(python_ocr), "rust": statistics.median(rust_ocr)},
        "texts_identical": python_text == rust_text,
        "thresholds": {
            "max_cer": args.max_cer,
            "max_anchor_delta": args.max_anchor_delta,
            "applies_to": "rust",
        },
        "passed": not failures,
        "failures": failures,
    }
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(
        json.dumps(
            {key: report[key] for key in ("median_load_seconds", "median_ocr_seconds", "texts_identical")},
            ensure_ascii=False,
        )
    )
    for failure in failures:
        print(f"FAIL {failure}")
    if failures:
        return 1
    print("PASS Rust 指标在阈值内通过")
    return 0


if __name__ == "__main__":
    # 复审 R4-1：未捕获异常（报告 status!=success、重复文本不一致、输出路径
    # 守卫 ValueError）原本以 traceback 按退出码 1 退出，与「1 = 超阈」语义
    # 冲突且不产出 JSON，自动化门无法区分「超阈」与「脚本报错」。崩溃路径
    # 统一收敛为退出码 3 并输出简明诊断；argparse 缺参仍为 2。
    try:
        exit_code = main()
    except Exception as error:
        print(f"CRASH compare.py 执行异常（退出码 3）：{error!r}")
        raise SystemExit(3) from error
    raise SystemExit(exit_code)
