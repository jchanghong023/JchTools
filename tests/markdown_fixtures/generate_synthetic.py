#!/usr/bin/env python3
"""按 T-26 从零合成 tests/markdown_fixtures 内可机械再生的公开夹具.

全部产物从零构造、无业务含义：不迁自旧项目，不含内部源文件名、正文、OCR
结果或可反查源文档的元数据。依赖仅 Pillow + 标准库（scripts/requirements-dev.txt
已提供 Pillow）；无法等价再生的夹具及残留原因见本目录 README.md。

用法：
    python tests/markdown_fixtures/generate_synthetic.py          # 重建本脚本管理的夹具
    python tests/markdown_fixtures/generate_synthetic.py --check  # 只校验在场夹具的结构断言
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import cast

from PIL import Image, ImageDraw, ImageFont

DIRECTORY = Path(__file__).resolve().parent

# T-18 修订后的引擎分流口径：auto_fast_pages 默认 500。210 页低于阈值，
# 作常规模式回归；501 页超过阈值，用于断言 auto_mode 降级披露。
NORMAL_PAGE_COUNT = 210
AUTO_MODE_PAGE_COUNT = 501
EXPECTED_PAGES_BY_NAME = {"large_210_pages.pdf": NORMAL_PAGE_COUNT, "large_501_pages.pdf": AUTO_MODE_PAGE_COUNT}

# 合成 JSONL 样本（matrix/format_sweep/sample.jsonl）：无外部语料路径、
# 无真实数值语义，仅保留「每行一个合法 JSON 对象」的格式特征。
SYNTHETIC_JSONL_ROWS: tuple[dict[str, object], ...] = (
    {"kind": "synthetic-jsonl", "index": 1, "tokens": ["ALPHA", "BETA"], "note": "从零构造的合成行"},
    {"kind": "synthetic-jsonl", "index": 2, "tokens": ["GAMMA"], "nested": {"ok": True, "depth": 2}},
    {"kind": "synthetic-jsonl", "index": 3, "numbers": [1, 2, 3], "text": "第三行合成样例"},
)


def _render_text(text: str, size: tuple[int, int]) -> Image.Image:
    """渲染一张白底黑字位图（夹具只要求稳定可渲染的文字，不追求字形质量）."""
    image = Image.new("RGB", size, "white")
    draw = ImageDraw.Draw(image)
    font = cast(
        "ImageFont.FreeTypeFont",
        ImageFont.load_default(size=24),  # pyright: ignore[reportInvalidCast]
    )
    draw.text((8, size[1] // 2 - 14), text, fill="black", font=font)  # pyright: ignore[reportUnknownMemberType]
    return image


def _blank_page(*, marked: bool) -> Image.Image:
    """一页 64x64 白底（marked 时含黑色小块，避免整册全空白退化为空内容）."""
    image = Image.new("RGB", (64, 64), "white")
    if marked:
        draw = ImageDraw.Draw(image)
        draw.rectangle((24, 24, 40, 40), fill="black")
    return image


def _write_multipage_pdf(target: Path, pages: int) -> None:
    """写一册 pages 页的合成 PDF（首页含黑色小块）."""
    frames = [_blank_page(marked=index == 0) for index in range(pages)]
    frames[0].save(target, "PDF", save_all=True, append_images=frames[1:])


# Pillow 写 PDF 会把当前挂钟写进 /CreationDate 与 /ModDate（复审 R7-2）：
# 重跑即改变字节，使 SHA256SUMS.txt 的钉扎失效。生成后把两个日期字段改写为
# 定长固定串（格式 D:YYYYMMDDHHmmSS 共 17 字节，长度不变、xref 偏移不受
# 影响），保证重建可字节复现；重建受管夹具后仍须按 AGENTS §3.3 重建
# SHA256SUMS.txt。
_FIXED_PDF_DATE = b"D:20260101000000Z"


def _pin_pdf_timestamp(target: Path) -> None:
    import re

    data = target.read_bytes()
    patched, count = re.subn(
        rb"/(CreationDate|ModDate) \([^)]*\)",
        rb"/\1 (" + _FIXED_PDF_DATE + b")",
        data,
    )
    if count:
        target.write_bytes(patched)


def _pdf_page_count(data: bytes) -> int:
    """按对象头粗计页数（/Type /Page 去掉 /Pages 计数）；结构断言用途足够."""
    return data.count(b"/Type /Page") - data.count(b"/Type /Pages")


def _write_jsonl(target: Path) -> None:
    lines = [json.dumps(row, ensure_ascii=False) for row in SYNTHETIC_JSONL_ROWS]
    _ = target.write_text("\n".join(lines) + "\n", encoding="utf-8")


def managed_files() -> dict[Path, str]:
    """本脚本管理的夹具与其用途说明（重建与 --check 的公共清单）."""
    sweep = DIRECTORY / "matrix" / "format_sweep"
    return {
        DIRECTORY / "test_hello_world.png": "PNG 文字样本（A16/A26/C06-C09）",
        DIRECTORY / "large_210_pages.pdf": f"{NORMAL_PAGE_COUNT} 页 PDF：常规模式回归（低于 500 阈值）",
        DIRECTORY / "large_501_pages.pdf": (
            f"{AUTO_MODE_PAGE_COUNT} 页 PDF：超过引擎 auto_fast_pages=500，断言 auto_mode 披露（A15）"
        ),
        DIRECTORY / "scanned_hello.pdf": "单页图像型 PDF（扫描页形态：正文即整页位图）",
        sweep / "sample.jsonl": "合成 JSONL 样本（A25 格式清点；无外部语料路径）",
    }


def _regenerate_one(target: Path) -> None:
    """按文件名分发重建单个受管夹具."""
    if target.name == "test_hello_world.png":
        _render_text("hello world", (200, 60)).save(target, "PNG")
    elif target.name in EXPECTED_PAGES_BY_NAME:
        _write_multipage_pdf(target, EXPECTED_PAGES_BY_NAME[target.name])
        _pin_pdf_timestamp(target)
    elif target.name == "scanned_hello.pdf":
        _render_text("SCANNED-SYNTH-TOKEN", (400, 120)).save(target, "PDF")
        _pin_pdf_timestamp(target)
    elif target.name == "sample.jsonl":
        _write_jsonl(target)


def regenerate() -> list[str]:
    """重建全部受管夹具；返回重建报告行。."""
    report: list[str] = []
    for target, purpose in managed_files().items():
        target.parent.mkdir(parents=True, exist_ok=True)
        _regenerate_one(target)
        report.append(f"重建 {target.relative_to(DIRECTORY)}：{purpose}")
    return report


def _check_pdf(target: Path) -> str | None:
    """单册 PDF 的页数结构断言；None 即通过（无页数期望的单页 PDF 跳过）."""
    expected = EXPECTED_PAGES_BY_NAME.get(target.name)
    if expected is None:
        return None
    pages = _pdf_page_count(target.read_bytes())
    if pages != expected:
        return f"{target.name} 页数 {pages} != 预期 {expected}"
    return None


def _check_plain(target: Path) -> str | None:
    """PNG 尺寸与 JSONL 纯合成内容断言；None 即通过."""
    if target.suffix == ".png":
        with Image.open(target) as picture:
            if picture.size != (200, 60):
                return f"{target.name} 尺寸 {picture.size} != 预期 (200, 60)"
        return None
    if target.suffix == ".jsonl":
        text = target.read_text(encoding="utf-8")
        if "docs/" in text or ".pdf" in text:
            return f"{target.name} 含外部语料路径，不是纯合成内容"
        for line in text.splitlines():
            if line.strip() and not isinstance(json.loads(line), dict):
                return f"{target.name} 存在非 JSON 对象行"
    return None


def check() -> list[str]:
    """校验受管夹具在场且满足结构断言；返回问题清单（空即通过）。."""
    problems: list[str] = []
    for target, purpose in managed_files().items():
        if not target.is_file():
            problems.append(f"缺夹具 {target.relative_to(DIRECTORY)}（{purpose}）")
            continue
        problems.extend(problem for problem in (_check_pdf(target), _check_plain(target)) if problem is not None)
    return problems


class _Arguments(argparse.Namespace):
    """带类型标注的解析结果；类属性即默认值。."""

    check_only: bool = False


def main() -> int:
    """入口：默认重建受管夹具，--check 只做结构校验。."""
    parser = argparse.ArgumentParser(description="合成/校验 tests/markdown_fixtures 的可再生公开夹具（T-26）")
    _ = parser.add_argument("--check", dest="check_only", action="store_true", help="只校验在场夹具，不重写文件")
    args = parser.parse_args(namespace=_Arguments())
    if not args.check_only:
        for line in regenerate():
            print(line)
    problems = check()
    prefix = "重建后校验失败" if not args.check_only else "校验失败"
    for problem in problems:
        print(f"{prefix}：{problem}")
    if args.check_only and not problems:
        print(f"校验通过：{len(managed_files())} 个受管夹具均满足结构断言")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
