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
import contextlib
import io
import json
import re
import struct
import sys
from pathlib import Path
from typing import cast

from PIL import Image, ImageDraw, ImageFont, ImageOps

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
    data = target.read_bytes()
    patched, count = re.subn(
        rb"/(CreationDate|ModDate) \([^)]*\)",
        rb"/\1 (" + _FIXED_PDF_DATE + b")",
        data,
    )
    if count:
        _ = target.write_bytes(patched)


def _pdf_page_count(data: bytes) -> int:
    """按对象头粗计页数（/Type /Page 去掉 /Pages 计数）；结构断言用途足够."""
    return data.count(b"/Type /Page") - data.count(b"/Type /Pages")


def _write_jsonl(target: Path) -> None:
    lines = [json.dumps(row, ensure_ascii=False) for row in SYNTHETIC_JSONL_ROWS]
    _ = target.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _write_jbig2(target: Path) -> None:
    """从零生成单页顺序 JBIG2：PageInformation + MMR generic region + EOF.

    Pillow 的 Group4 TIFF 提供 T.6 编码，不需要 JBIG2 专用编码库。
    TIFF 默认 BlackIsZero；JBIG2 的 MMR 位极性相反，先反转编码输入，
    保证最终图仍是白底黑字。该构造已与独立 MuPDF 解码逐像素对照。
    """
    width, height = 320, 96
    picture = Image.new("1", (width, height), 1)
    draw = ImageDraw.Draw(picture)
    font = cast("ImageFont.FreeTypeFont", cast("object", ImageFont.load_default(size=24)))
    draw.text((12, 24), "JBIG2 SYNTH CHECK", fill=0, font=font)  # pyright: ignore[reportUnknownMemberType]
    encoded = io.BytesIO()
    ImageOps.invert(picture.convert("L")).convert("1").save(encoded, "TIFF", compression="group4")
    tiff_bytes = encoded.getvalue()
    with Image.open(io.BytesIO(tiff_bytes)) as tiff:
        offsets = cast("tuple[int, ...]", tiff.tag_v2[273])  # pyright: ignore[reportAttributeAccessIssue]
        lengths = cast("tuple[int, ...]", tiff.tag_v2[279])  # pyright: ignore[reportAttributeAccessIssue]
        mmr = b"".join(tiff_bytes[offset : offset + length] for offset, length in zip(offsets, lengths, strict=True))

    def segment(number: int, kind: int, page: int, payload: bytes) -> bytes:
        return struct.pack(">IBBBI", number, kind, 0, page, len(payload)) + payload

    page_info = struct.pack(">IIIIBH", width, height, 72, 72, 0, 0)
    region_info = struct.pack(">IIIIBB", width, height, 0, 0, 0, 1)
    data = (
        b"\x97JB2\r\n\x1a\n\x01"
        + struct.pack(">I", 1)
        + segment(0, 48, 1, page_info)
        + segment(1, 38, 1, region_info + mmr)
        + segment(2, 49, 1, b"")
        + segment(3, 51, 0, b"")
    )
    _ = target.write_bytes(data)


HWP_SYNTH_TEXT = "HWP SYNTH CHECK"
_CFB_FREE = 0xFFFFFFFF
_CFB_END = 0xFFFFFFFE
_HWP_DIRECTORY_MAX_ID = 5


def _hwp_record(tag: int, level: int, payload: bytes) -> bytes:
    """HWP5 记录头：tag/level/size 各占 10/10/12 位（本样本无需扩展长度）."""
    return struct.pack("<I", tag | (level << 10) | (len(payload) << 20)) + payload


def _hwp_string(text: str) -> bytes:
    return struct.pack("<H", len(text)) + text.encode("utf-16le")


def _hwp_streams() -> dict[str, bytes]:
    """从零构造无压缩 HWP 5.0.0.0，引用均指向实际定义的默认样式."""
    header = b"HWP Document File".ljust(32, b"\0") + struct.pack("<II", 0x05000000, 0) + bytes(216)
    properties = struct.pack("<7H3I", 1, 1, 1, 1, 1, 1, 1, 0, 0, 0)
    mappings = struct.pack("<15I", 0, 1, 1, 1, 1, 1, 1, 1, 0, 1, 1, 0, 0, 1, 1)
    font = b"\x01" + _hwp_string("Arial")
    char_shape = (
        bytes(14)
        + bytes([100]) * 7
        + bytes(7)
        + bytes([100]) * 7
        + bytes(7)
        + struct.pack("<iIbb4I", 1000, 0, 0, 0, 0, 0, 0xFFFFFFFF, 0)
    )
    para_shape = struct.pack("<I6i7H", 4, 0, 0, 0, 0, 0, 160, 0, 0, 0, 0, 0, 0, 0)
    style = _hwp_string("Normal") + _hwp_string("Normal") + struct.pack("<BBhHHH", 0, 0, 1033, 0, 0, 0)
    doc_info = (
        _hwp_record(16, 0, properties)
        + _hwp_record(17, 0, mappings)
        + b"".join(_hwp_record(19, 1, font) for _ in range(7))
        + _hwp_record(21, 1, char_shape)
        + _hwp_record(22, 1, bytes(8))
        + _hwp_record(25, 1, para_shape)
        + _hwp_record(26, 1, style)
    )
    # Section-definition extended control occupies eight UTF-16 code units.
    # Paragraph character count includes that control and the final CR.
    text = struct.pack("<H", 2) + b"dces" + bytes(8) + struct.pack("<H", 2)
    text += (HWP_SYNTH_TEXT + "\r").encode("utf-16le")
    para_header = struct.pack("<IIHBBHHHI", 0x80000000 | (len(text) // 2), 4, 0, 0, 1, 1, 0, 1, 1)
    section_def = b"dces" + struct.pack("<I3HI5H", 0, 0, 0, 0, 4000, 0, 1, 1, 1, 1)
    page_def = struct.pack("<10I", 59528, 84188, 8504, 8504, 5669, 4252, 4252, 4252, 0, 0)
    line_seg = struct.pack("<8iI", 0, 0, 1000, 1000, 850, 600, 0, 42520, 0x00060000)
    body = (
        _hwp_record(66, 0, para_header)
        + _hwp_record(67, 1, text)
        + _hwp_record(68, 1, struct.pack("<II", 0, 0))
        + _hwp_record(69, 1, line_seg)
        + _hwp_record(71, 1, section_def)
        + _hwp_record(73, 2, page_def)
    )
    return {"FileHeader": header, "DocInfo": doc_info, "Section0": body, "PrvText": HWP_SYNTH_TEXT.encode("utf-16le")}


def _hwp_bytes() -> bytes:
    """最小 CFB v3 容器：512 字节扇区、64 字节 mini-sector、合法目录红黑树."""
    streams = _hwp_streams()
    mini_fat: list[int] = []
    mini_data = bytearray()
    starts: dict[str, int] = {}
    for name, payload in streams.items():
        starts[name] = len(mini_fat)
        count = (len(payload) + 63) // 64
        for index in range(count):
            mini_fat.append(len(mini_fat) + 1 if index + 1 < count else _CFB_END)
        mini_data.extend(payload.ljust(count * 64, b"\0"))
    mini_size = len(mini_data)
    mini_sectors = (mini_size + 511) // 512
    mini_data.extend(bytes(mini_sectors * 512 - mini_size))
    minifat_sector = 2 + mini_sectors
    fat_sector = minifat_sector + 1

    def entry(  # noqa: PLR0913  # MS-CFB 目录项的八个固定字段，不引入重复记录模型。
        name: str, kind: int, color: int, left: int, right: int, child: int, start: int, size: int
    ) -> bytes:
        encoded = (name + "\0").encode("utf-16le")
        return (
            encoded.ljust(64, b"\0")
            + struct.pack("<HBBIII", len(encoded), kind, color, left, right, child)
            + bytes(36)
            + struct.pack("<IQ", start, size)
        )

    free = _CFB_FREE
    directory = (
        # MS-CFB §2.6.4 orders names by UTF-16 byte length FIRST, then uppercase
        # code units: DocInfo < PrvText < BodyText < FileHeader (not lexical).
        # BodyText(B) has DocInfo(B)/FileHeader(B); DocInfo has PrvText(R).
        entry("Root Entry", 5, 1, free, free, 1, 2, mini_size)
        + entry("BodyText", 1, 1, 2, 3, 5, 0, 0)
        + entry("DocInfo", 2, 1, free, 4, free, starts["DocInfo"], len(streams["DocInfo"]))
        + entry("FileHeader", 2, 1, free, free, free, starts["FileHeader"], len(streams["FileHeader"]))
        + entry("PrvText", 2, 0, free, free, free, starts["PrvText"], len(streams["PrvText"]))
        + entry("Section0", 2, 1, free, free, free, starts["Section0"], len(streams["Section0"]))
    ).ljust(1024, b"\0")
    fat = [1, _CFB_END]
    fat.extend(3 + index if index + 1 < mini_sectors else _CFB_END for index in range(mini_sectors))
    fat.extend([_CFB_END, 0xFFFFFFFD])
    fat.extend([free] * (128 - len(fat)))
    header = (
        b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"
        + bytes(16)
        + struct.pack("<HHHHH", 0x003E, 3, 0xFFFE, 9, 6)
        + bytes(6)
        + struct.pack("<9I", 0, 1, 0, 0, 4096, minifat_sector, 1, _CFB_END, 0)
        + struct.pack("<109I", fat_sector, *([free] * 108))
    )
    return (
        header
        + directory
        + bytes(mini_data)
        + struct.pack("<128I", *mini_fat, *([free] * (128 - len(mini_fat))))
        + struct.pack("<128I", *fat)
    )


def _write_hwp(target: Path) -> None:
    _ = target.write_bytes(_hwp_bytes())


def _check_hwp(target: Path) -> str | None:
    """校验确定性字节及 MS-CFB 长度优先排序、无连续红节点和等黑高."""
    data = target.read_bytes()
    if data != _hwp_bytes():
        return f"{target.name} 不符合受管 HWP5 容器/记录结构或稳定正文"
    try:
        _validate_hwp_directory(data)
    except ValueError as error:
        return f"{target.name}：{error}"
    return None


def _validate_hwp_directory(data: bytes) -> None:
    """独立遍历 MS-CFB 目录，按规范检查排序和红黑树不变量。."""
    visited: set[int] = set()

    def walk(index: int, lower: tuple[int, str] | None, upper: tuple[int, str] | None, *, red: bool) -> int:
        if index == _CFB_FREE:
            return 1
        if not 1 <= index <= _HWP_DIRECTORY_MAX_ID or index in visited:
            message = "目录节点重复或越界"
            raise ValueError(message)
        visited.add(index)
        offset = 512 + index * 128
        length = struct.unpack_from("<H", data, offset + 64)[0]
        name = data[offset : offset + length - 2].decode("utf-16le")
        # Managed names are ASCII: upper() exactly implements simple UTF-16
        # uppercase here, without Unicode expansion or surrogate ambiguity.
        if not name.isascii():
            message = "受管目录名称必须为 ASCII"
            raise ValueError(message)
        key = (length, name.upper())
        if (lower is not None and key <= lower) or (upper is not None and key >= upper):
            message = "目录名称未按长度优先的 MS-CFB 规则排序"
            raise ValueError(message)
        color = data[offset + 67]
        if color not in (0, 1) or (red and color == 0):
            message = "目录红黑颜色无效"
            raise ValueError(message)
        left, right = struct.unpack_from("<II", data, offset + 68)
        left_height = walk(left, lower, key, red=color == 0)
        right_height = walk(right, key, upper, red=color == 0)
        if left_height != right_height:
            message = "目录红黑树黑高不一致"
            raise ValueError(message)
        return left_height + color

    root_child = struct.unpack_from("<I", data, 512 + 76)[0]
    _ = walk(root_child, None, None, red=True)
    section_child = struct.unpack_from("<I", data, 512 + 128 + 76)[0]
    _ = walk(section_child, None, None, red=True)
    if visited != set(range(1, _HWP_DIRECTORY_MAX_ID + 1)):
        message = "目录含不可达节点"
        raise ValueError(message)


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
        DIRECTORY / "matrix" / "jbig2_standalone.jb2": "JBIG2 合成文字位图（A19）：完整文件头与 MMR 区域",
        sweep / "sample.hwp": "HWP5 合成正文（A25）：真实 Section0 段落与默认字体/样式",
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
    elif target.name == "jbig2_standalone.jb2":
        _write_jbig2(target)
    elif target.name == "sample.hwp":
        _write_hwp(target)


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
    if target.suffix in {".jb2", ".hwp"}:
        checker = _check_jbig2 if target.suffix == ".jb2" else _check_hwp
        return checker(target)
    return None


def _check_jbig2(target: Path) -> str | None:
    """校验合成 JBIG2 页信息与文件收尾，截断输入明确报错。."""
    data = target.read_bytes()
    if data[:13] != b"\x97JB2\r\n\x1a\n\x01\x00\x00\x00\x01":
        return f"{target.name} 缺少完整单页 JBIG2 文件头"
    if data[13:24] != struct.pack(">IBBBI", 0, 48, 0, 1, 19):
        return f"{target.name} 缺少 PageInformation 段"
    if data[24:32] != struct.pack(">II", 320, 96):
        return f"{target.name} 页面尺寸不符"
    if data[-11:] != struct.pack(">IBBBI", 3, 51, 0, 0, 0):
        return f"{target.name} 缺少 EndOfFile 段"
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
    # 中文输出走管道时默认编码是 ANSI 代码页（CI 英文 runner 为 cp1252），打印
    # 即崩（UnicodeEncodeError 掩盖真实校验结果）；与 test_gate 同口径强制 UTF-8。
    with contextlib.suppress(AttributeError):
        stream = sys.stdout
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")
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
