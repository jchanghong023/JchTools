#!/usr/bin/env python3
"""按 T-26 从零合成 tests/markdown_fixtures 内可机械再生的公开夹具.

默认产物从零构造、无业务含义：不迁自旧项目，不含内部源文件名、正文、OCR
结果或可反查源文档的元数据。依赖仅 Pillow + 标准库（scripts/requirements-dev.txt
已提供 Pillow）；无法等价再生的夹具及残留原因见本目录 README.md。
另提供 Windows 离屏 GDI oracle：仅从既有公开 DOCX 的原 WMF 记录按请求尺寸
重绘，作为跨分辨率保真回归的独立源真值；不调用产品或引擎，不读取其图片像素。

用法：
    python tests/markdown_fixtures/generate_synthetic.py          # 重建本脚本管理的夹具
    python tests/markdown_fixtures/generate_synthetic.py --check  # 只校验在场夹具的结构断言
    python tests/markdown_fixtures/generate_synthetic.py --wmf-oracles  # 重建两张独立 WMF 源图
    python tests/markdown_fixtures/generate_synthetic.py --wmf-oracles --check
    python tests/markdown_fixtures/generate_synthetic.py --format-sweep  # 补齐 A25 的 13 种格式
    python tests/markdown_fixtures/generate_synthetic.py --format-sweep --check
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import hashlib
import io
import json
import re
import struct
import sys
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Protocol, cast

from PIL import Image, ImageDraw, ImageFont, ImageOps, PngImagePlugin

DIRECTORY = Path(__file__).resolve().parent

# T-18 修订后的引擎分流口径：auto_fast_pages 默认 500。210 页低于阈值，
# 作常规模式回归；501 页超过阈值，用于断言 auto_mode 降级披露。
NORMAL_PAGE_COUNT = 210
AUTO_MODE_PAGE_COUNT = 501
EXPECTED_PAGES_BY_NAME = {
    "large_210_pages.pdf": NORMAL_PAGE_COUNT,
    "large_501_pages.pdf": AUTO_MODE_PAGE_COUNT,
    "scanned_hello.pdf": 1,
}

# 合成 JSONL 样本（matrix/format_sweep/sample.jsonl）：无外部语料路径、
# 无真实数值语义，仅保留「每行一个合法 JSON 对象」的格式特征。
SYNTHETIC_JSONL_ROWS: tuple[dict[str, object], ...] = (
    {"kind": "synthetic-jsonl", "index": 1, "tokens": ["ALPHA", "BETA"], "note": "从零构造的合成行"},
    {"kind": "synthetic-jsonl", "index": 2, "tokens": ["GAMMA"], "nested": {"ok": True, "depth": 2}},
    {"kind": "synthetic-jsonl", "index": 3, "numbers": [1, 2, 3], "text": "第三行合成样例"},
)


class _GdiFunction(Protocol):
    """收口 ctypes 动态函数的原生签名与 HANDLE/BOOL 返回值。."""

    argtypes: list[object]
    restype: object

    def __call__(self, *args: object) -> int | None: ...


def render_wmf_gdi(source: bytes, size: tuple[int, int]) -> Image.Image:
    """独立离屏重绘 placeable WMF；源逻辑边界等比居中，尺寸仅决定采样。

    GDI 的 WindowExt/ViewportExt 映射源逻辑单位到目标像素，不经 Pillow 的
    WMF→EMF 转换或先光栅化再缩放。所有 DC、DIB 和 metafile 均局部回收。
    """
    if sys.platform != "win32":
        raise OSError("WMF GDI oracle 需要 Windows")
    width, height = size
    if width <= 0 or height <= 0 or len(source) < 40:
        raise ValueError("WMF GDI oracle 的尺寸或源头不合法")
    key, _, left, top, right, bottom, inch, _, _ = struct.unpack_from("<IHhhhhHIH", source)
    if key != 0x9AC6CDD7 or right <= left or bottom <= top or not inch:
        raise ValueError("WMF GDI oracle 需要合法 placeable 逻辑边界")
    records = source[22:]
    if struct.unpack_from("<I", records, 6)[0] * 2 != len(records):
        raise ValueError("WMF 声明长度与原始记录不符")
    source_width, source_height = right - left, bottom - top
    if width * source_height > height * source_width:
        draw_width, draw_height = height * source_width // source_height, height
    else:
        draw_width, draw_height = width, width * source_height // source_width
    x, y = (width - draw_width) // 2, (height - draw_height) // 2
    library = ctypes.WinDLL("gdi32", use_last_error=True)

    def native(name: str, result: object, *arguments: object) -> _GdiFunction:
        function = cast("_GdiFunction", getattr(library, name))
        function.restype = result
        function.argtypes = list(arguments)
        return function

    pointer, integer, unsigned = ctypes.c_void_p, ctypes.c_int, ctypes.c_uint
    create_dc = native("CreateCompatibleDC", pointer, pointer)
    delete_dc = native("DeleteDC", integer, pointer)
    delete_object = native("DeleteObject", integer, pointer)
    select_object = native("SelectObject", pointer, pointer, pointer)
    create_dib = native(
        "CreateDIBSection", pointer, pointer, pointer, unsigned,
        ctypes.POINTER(pointer), pointer, ctypes.c_ulong,
    )
    create_metafile = native("SetMetaFileBitsEx", pointer, unsigned, pointer)
    delete_metafile = native("DeleteMetaFile", integer, pointer)
    play = native("PlayMetaFile", integer, pointer, pointer)
    map_mode = native("SetMapMode", integer, pointer, integer)
    window_origin = native("SetWindowOrgEx", integer, pointer, integer, integer, pointer)
    window_extent = native("SetWindowExtEx", integer, pointer, integer, integer, pointer)
    viewport_origin = native("SetViewportOrgEx", integer, pointer, integer, integer, pointer)
    viewport_extent = native("SetViewportExtEx", integer, pointer, integer, integer, pointer)
    flush = native("GdiFlush", integer)
    info = ctypes.create_string_buffer(
        struct.pack("<IiiHHIIiiII", 40, width, -height, 1, 32, 0, width * height * 4, 0, 0, 0, 0)
    )
    dc: int | None = None
    bitmap: int | None = None
    previous: int | None = None
    metafile: int | None = None
    try:
        dc = create_dc(None)
        if not dc:
            raise ctypes.WinError(ctypes.get_last_error())
        pixels = pointer()
        bitmap = create_dib(dc, info, 0, ctypes.byref(pixels), None, 0)
        if not bitmap or not pixels.value:
            raise ctypes.WinError(ctypes.get_last_error())
        _ = ctypes.memset(pixels, 255, width * height * 4)
        previous = select_object(dc, bitmap)
        if previous in (None, ctypes.c_void_p(-1).value):
            previous = None
            raise ctypes.WinError(ctypes.get_last_error())
        buffer = ctypes.create_string_buffer(records)
        metafile = create_metafile(len(records), buffer)
        if not metafile:
            raise ctypes.WinError(ctypes.get_last_error())
        operations: tuple[tuple[_GdiFunction, tuple[object, ...]], ...] = (
            (map_mode, (dc, 8)),  # MM_ANISOTROPIC；映射由源边界与目标画布决定。
            (window_origin, (dc, left, top, None)),
            (window_extent, (dc, source_width, source_height, None)),
            (viewport_origin, (dc, x, y, None)),
            (viewport_extent, (dc, max(1, draw_width), max(1, draw_height), None)),
            (play, (dc, metafile)),
            (flush, ()),
        )
        for function, arguments in operations:
            if not function(*arguments):
                raise ctypes.WinError(ctypes.get_last_error())
        # Pillow 的 raw decoder 桩未声明尾部参数；这里收口实际使用的 BGRX 签名。
        from_bytes = cast(
            "Callable[[str, tuple[int, int], bytes, str, str], Image.Image]", Image.frombytes
        )
        return from_bytes("RGB", size, ctypes.string_at(pixels, width * height * 4), "raw", "BGRX")
    finally:
        if metafile:
            _ = delete_metafile(metafile)
        if previous and dc:
            _ = select_object(dc, previous)
        if bitmap:
            _ = delete_object(bitmap)
        if dc:
            _ = delete_dc(dc)


_WMF_GDI_SIZES = ((625, 313), (1250, 626))
_WMF_GDI_MEMBER = "docx_emf_wmf_raster.docx:word/media/healthy.wmf"


def _wmf_oracle_source() -> bytes:
    with zipfile.ZipFile(DIRECTORY / "matrix" / "docx_emf_wmf_raster.docx") as archive:
        return archive.read("word/media/healthy.wmf")


def _regenerate_wmf_oracles() -> list[str]:
    """明确请求时才重建平台 oracle，不影响默认跨平台纯合成流程。."""
    source = _wmf_oracle_source()
    report: list[str] = []
    for size in _WMF_GDI_SIZES:
        target = DIRECTORY / "matrix" / f"wmf_gdi_{size[0]}x{size[1]}.png"
        metadata = PngImagePlugin.PngInfo()
        metadata.add_text("SourceMemberSHA256", hashlib.sha256(source).hexdigest())
        metadata.add_text("SourceMember", _WMF_GDI_MEMBER)
        metadata.add_text(
            "Oracle", "Windows GDI PlayMetaFile; source-header aspect-fit; white offscreen RGB"
        )
        render_wmf_gdi(source, size).save(target, "PNG", pnginfo=metadata)
        report.append(f"重建 {target.relative_to(DIRECTORY)}：原 WMF 独立 Windows GDI 源图")
    return report


def _check_wmf_oracles() -> list[str]:
    """离线核对采样尺寸和源成员指纹；不依赖 Windows 或产品图片。."""
    digest = hashlib.sha256(_wmf_oracle_source()).hexdigest()
    problems: list[str] = []
    for size in _WMF_GDI_SIZES:
        target = DIRECTORY / "matrix" / f"wmf_gdi_{size[0]}x{size[1]}.png"
        if not target.is_file():
            problems.append(f"缺独立 WMF oracle：{target.name}")
            continue
        with Image.open(target) as image:
            if image.size != size or image.mode != "RGB":
                problems.append(f"{target.name} 的尺寸或 RGB 模式不符")
            if (
                image.info.get("SourceMemberSHA256") != digest
                or image.info.get("SourceMember") != _WMF_GDI_MEMBER
            ):
                problems.append(f"{target.name} 的原 WMF 成员或指纹不符")
    return problems


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


# Pillow 写 PDF 会把当前挂钟写进 /CreationDate 与 /ModDate。只替换日期
# 中固定宽度的 14 位数字，保留原有时区和分隔符；不能改变字符串长度，
# 否则已写出的 xref 与 startxref 偏移失效。此处不重建或更新夹具基线。
_FIXED_PDF_DATE = b"D:20260101000000"


def _pin_pdf_timestamp(target: Path) -> None:
    data = target.read_bytes()
    patched, count = re.subn(
        rb"/(CreationDate|ModDate) \(D:\d{14}",
        rb"/\1 (" + _FIXED_PDF_DATE,
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



def _zip_members(members: dict[str, bytes], *, first_stored: str | None = None) -> bytes:
    """固定 ZIP 日期与条目顺序；ODF 的 mimetype 必须是首个无压缩成员。."""
    result = io.BytesIO()
    with zipfile.ZipFile(result, "w") as archive:
        for name, content in members.items():
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_STORED if name == first_stored else zipfile.ZIP_DEFLATED
            archive.writestr(info, content)
    return result.getvalue()


def _odf_package(extension: str) -> bytes:
    """按 ODF 1.3 Part 2/3 从零构造含真实正文的文本、表格、演示包。."""
    kind = {"odt": "text", "ods": "spreadsheet", "odp": "presentation"}[extension]
    mimetype = f"application/vnd.oasis.opendocument.{kind}"
    namespaces = (
        ' xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"'
        ' xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"'
        ' xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"'
        ' xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0"'
        ' xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"'
        ' xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"'
        ' xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"'
    )
    token = f"{extension.upper()} SYNTH CHECK"
    bodies = {
        "odt": f"<office:text><text:p>{token}</text:p></office:text>",
        "ods": (
            '<office:spreadsheet><table:table table:name="Fixture"><table:table-column/><table:table-row>'
            f'<table:table-cell office:value-type="string"><text:p>{token}</text:p>'
            "</table:table-cell></table:table-row></table:table></office:spreadsheet>"
        ),
        "odp": (
            '<office:presentation><draw:page draw:name="page1" draw:master-page-name="Default">'
            '<draw:frame svg:x="2cm" svg:y="2cm" svg:width="20cm" svg:height="4cm">'
            f"<draw:text-box><text:p>{token}</text:p></draw:text-box></draw:frame>"
            "</draw:page></office:presentation>"
        ),
    }
    content = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        f'<office:document-content{namespaces} office:version="1.3">'
        f"<office:body>{bodies[extension]}</office:body></office:document-content>"
    )
    styles = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        f'<office:document-styles{namespaces} office:version="1.3">'
        "<office:styles/><office:automatic-styles>"
        '<style:page-layout style:name="PageLayout">'
        '<style:page-layout-properties fo:page-width="28cm" fo:page-height="21cm"'
        ' style:print-orientation="landscape"/></style:page-layout>'
        "</office:automatic-styles><office:master-styles>"
        '<style:master-page style:name="Default" style:page-layout-name="PageLayout"/>'
        "</office:master-styles></office:document-styles>"
    )
    manifest = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"'
        ' manifest:version="1.3">'
        f'<manifest:file-entry manifest:full-path="/" manifest:media-type="{mimetype}"'
        ' manifest:version="1.3"/>'
        '<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>'
        '<manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>'
        "</manifest:manifest>"
    )
    return _zip_members(
        {
            "mimetype": mimetype.encode("ascii"),
            "content.xml": content.encode("utf-8"),
            "styles.xml": styles.encode("utf-8"),
            "META-INF/manifest.xml": manifest.encode("utf-8"),
        },
        first_stored="mimetype",
    )


def _xltm_package() -> bytes:
    """从零构造无 VBA 工程的 macro-enabled SpreadsheetML 模板；不伪装普通 XLSX。."""
    main_type = "application/vnd.ms-excel.template.macroEnabled.main+xml"
    members = {
        "[Content_Types].xml": (
            '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
            '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
            '<Default Extension="xml" ContentType="application/xml"/>'
            f'<Override PartName="/xl/workbook.xml" ContentType="{main_type}"/>'
            '<Override PartName="/xl/worksheets/sheet1.xml"'
            ' ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
            "</Types>"
        ),
        "_rels/.rels": (
            '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
            '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/'
            'officeDocument" Target="xl/workbook.xml"/></Relationships>'
        ),
        "xl/workbook.xml": (
            '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
            ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
            '<sheets><sheet name="Fixture" sheetId="1" r:id="rId1"/></sheets></workbook>'
        ),
        "xl/_rels/workbook.xml.rels": (
            '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
            '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/'
            'worksheet" Target="worksheets/sheet1.xml"/></Relationships>'
        ),
        "xl/worksheets/sheet1.xml": (
            '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">'
            '<sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>XLTM SYNTH CHECK</t></is></c>'
            "</row></sheetData></worksheet>"
        ),
    }
    return _zip_members({name: text.encode("utf-8") for name, text in members.items()})


def _cfb_stream_layout(source: bytes, name: str) -> tuple[int, int, list[int]]:
    """定位既有 CFB v3 夹具的流块，允许等长改写而不重排目录或 FAT。."""
    if source[:8] != bytes.fromhex("d0cf11e0a1b11ae1") or struct.unpack_from("<H", source, 30)[0] != 9:
        raise ValueError("旧 Office 合成源必须是 CFB v3")
    fat_count = struct.unpack_from("<I", source, 44)[0]
    if fat_count > 109:
        raise ValueError("受管旧 Office 源不应需要 DIFAT 扩展")
    fat_ids = struct.unpack_from("<109I", source, 76)[:fat_count]
    fat: tuple[int, ...] = struct.unpack(
        f"<{128 * fat_count}I",
        b"".join(source[(sector + 1) * 512 : (sector + 2) * 512] for sector in fat_ids),
    )

    def chain(start: int, table: tuple[int, ...]) -> list[int]:
        result: list[int] = []
        seen: set[int] = set()
        while start != _CFB_END:
            if start >= len(table) or start in seen:
                raise ValueError("CFB 流链越界或重复")
            seen.add(start)
            result.append(start)
            start = table[start]
        return result

    directory_ids = chain(struct.unpack_from("<I", source, 48)[0], fat)
    directory = b"".join(source[(sector + 1) * 512 : (sector + 2) * 512] for sector in directory_ids)
    entry: bytes | None = None
    for offset in range(0, len(directory), 128):
        candidate = directory[offset : offset + 128]
        length = struct.unpack_from("<H", candidate, 64)[0]
        if candidate[66] == 2 and candidate[: length - 2].decode("utf-16le") == name:
            entry = candidate
            break
    if entry is None:
        raise ValueError(f"公开旧 Office 合成源缺少 {name} 流")
    start = struct.unpack_from("<I", entry, 116)[0]
    length = struct.unpack_from("<Q", entry, 120)[0]
    if length >= 4096:
        return length, 512, [(sector + 1) * 512 for sector in chain(start, fat)]
    root_ids = chain(struct.unpack_from("<I", directory, 116)[0], fat)
    mini_fat_ids = chain(struct.unpack_from("<I", source, 60)[0], fat)
    mini_fat_bytes = b"".join(source[(sector + 1) * 512 : (sector + 2) * 512] for sector in mini_fat_ids)
    mini_fat = struct.unpack(f"<{len(mini_fat_bytes) // 4}I", mini_fat_bytes)
    locations = [
        (root_ids[sector // 8] + 1) * 512 + (sector % 8) * 64
        for sector in chain(start, mini_fat)
    ]
    return length, 64, locations


def _cfb_stream(source: bytes, name: str) -> bytes:
    length, unit, locations = _cfb_stream_layout(source, name)
    return b"".join(source[offset : offset + unit] for offset in locations)[:length]


def _replace_cfb_stream(source: bytes, name: str, content: bytes) -> bytes:
    length, unit, locations = _cfb_stream_layout(source, name)
    if len(content) != length:
        raise ValueError("旧 Office 变体只允许等长流改写，不能破坏扇区或其他流偏移")
    result = bytearray(source)
    for index, offset in enumerate(locations):
        part = content[index * unit : (index + 1) * unit]
        result[offset : offset + len(part)] = part
    return bytes(result)


def _word_template(source: bytes) -> bytes:
    """MS-DOC FibBase.fDot 位才定义 DOT；不能仅将 DOC 改扩展名。."""
    document = bytearray(_cfb_stream(source, "WordDocument"))
    flags = struct.unpack_from("<H", document, 10)[0]
    struct.pack_into("<H", document, 10, flags | 1)
    return _replace_cfb_stream(source, "WordDocument", bytes(document))


def _excel_variant(source: bytes, marker: int) -> bytes:
    """BOF 后加入 Template(0060)/ADDIN(0087)，移除可选 Excel9File 保留所有后续偏移。."""
    workbook = _cfb_stream(source, "Workbook")
    records: list[bytes] = []
    offset = 0
    removed = False
    while offset < len(workbook):
        record_type, length = struct.unpack_from("<HH", workbook, offset)
        record = workbook[offset : offset + 4 + length]
        if record_type == 0x01C0 and length == 0 and not removed:
            removed = True
        else:
            records.append(record)
        offset += 4 + length
    if not removed or struct.unpack_from("<H", records[0])[0] != 0x0809:
        raise ValueError("公开 XLS 源的 Globals/可选 Excel9File 结构不符")
    records.insert(1, struct.pack("<HH", marker, 0))
    return _replace_cfb_stream(source, "Workbook", b"".join(records))


_FORMAT_SWEEP_EXTENSIONS = (
    "doc", "dot", "odp", "ods", "odt", "pnm", "pot", "pps", "ppt", "xla", "xls", "xlt", "xltm",
)


def _format_sweep_payloads() -> dict[str, bytes]:
    """从公开合成旧 Office 源和新构造的开放格式生成完整的 A25 补充集。."""
    common = DIRECTORY / "common"
    document = (common / "legacy.doc").read_bytes()
    workbook = (common / "legacy.xls").read_bytes()
    presentation = (common / "legacy.ppt").read_bytes()
    pnm = io.BytesIO()
    _render_text("PNM SYNTH CHECK", (320, 96)).save(pnm, "PPM")
    return {
        "doc": document,
        "dot": _word_template(document),
        "odp": _odf_package("odp"),
        "ods": _odf_package("ods"),
        "odt": _odf_package("odt"),
        "pnm": pnm.getvalue(),  # P6 是 PNM 家族的二进制 RGB 表示。
        # MS-OFFDI 明列三种扩展名都使用 MS-PPT；不伪造不存在的类型标志。
        "pot": presentation,
        "pps": presentation,
        "ppt": presentation,
        "xla": _excel_variant(workbook, 0x0087),
        "xls": workbook,
        "xlt": _excel_variant(workbook, 0x0060),
        "xltm": _xltm_package(),
    }


def _regenerate_format_sweep() -> list[str]:
    """只写 A25 缺失集，不覆盖其他格式清点样本或原始公开合成源。."""
    sweep = DIRECTORY / "matrix" / "format_sweep"
    sweep.mkdir(parents=True, exist_ok=True)
    report: list[str] = []
    for extension, payload in _format_sweep_payloads().items():
        target = sweep / f"sample.{extension}"
        _ = target.write_bytes(payload)
        report.append(f"重建 {target.relative_to(DIRECTORY)}：公开合成源／规范构造变体")
    return report


def _check_format_sweep() -> list[str]:
    """源绑定、确定性字节与关键类型判别字段；不运行转换器或办公软件。."""
    problems: list[str] = []
    for extension, expected in _format_sweep_payloads().items():
        target = DIRECTORY / "matrix" / "format_sweep" / f"sample.{extension}"
        if not target.is_file():
            problems.append(f"缺 A25 合成夹具 {target.name}")
            continue
        actual = target.read_bytes()
        if actual != expected:
            problems.append(f"{target.name} 不符合受管公开合成源／格式变换")
        if extension == "dot":
            flags = struct.unpack_from("<H", _cfb_stream(actual, "WordDocument"), 10)[0]
            if not flags & 1:
                problems.append("sample.dot 缺少 FibBase.fDot 模板标识")
        elif extension in {"xla", "xlt"}:
            stream = _cfb_stream(actual, "Workbook")
            first_length = struct.unpack_from("<H", stream, 2)[0]
            marker = struct.unpack_from("<HH", stream, first_length + 4)
            if marker != (0x0087 if extension == "xla" else 0x0060, 0):
                problems.append(f"{target.name} 缺少首 BOF 后的正确 ADDIN/Template 空记录")
            offset = 0
            while offset < len(stream):
                record, length = struct.unpack_from("<HH", stream, offset)
                if record == 0x0085:
                    sheet_start = struct.unpack_from("<I", stream, offset + 4)[0]
                    if struct.unpack_from("<H", stream, sheet_start)[0] != 0x0809:
                        problems.append(f"{target.name} BoundSheet8 不再指向真实 sheet BOF")
                offset += 4 + length
        elif extension in {"odp", "ods", "odt"}:
            with zipfile.ZipFile(io.BytesIO(actual)) as archive:
                first = archive.infolist()[0]
                if first.filename != "mimetype" or first.compress_type != zipfile.ZIP_STORED or first.extra:
                    problems.append(f"{target.name} mimetype 必须首项、无压缩且无 ZIP extra")
    return problems


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
    elif target.name == "scanned_hello.pdf":
        _render_text("SCANNED-SYNTH-TOKEN", (400, 120)).save(target, "PDF")
        _pin_pdf_timestamp(target)
    elif target.name in EXPECTED_PAGES_BY_NAME:
        _write_multipage_pdf(target, EXPECTED_PAGES_BY_NAME[target.name])
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
    """单册 PDF 的页数结构断言，扫描页同样必须恰好一页；None 即通过."""
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
        rows = [json.loads(line) for line in text.splitlines() if line.strip()]
        if rows != list(SYNTHETIC_JSONL_ROWS):
            return f"{target.name} 行数或内容不符合从零构造的合成 JSONL 样本（T-26）"
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
    if data[24:43] != struct.pack(">IIIIBH", 320, 96, 72, 72, 0, 0):
        return f"{target.name} PageInformation 载荷不完整或不符"
    # 固定前缀与 EOF 都在场仍可能中间被截断；按段声明长度检查连续边界，
    # 不执行解码器，也不把「尚能识别文件头」当作完整合成夹具。
    if len(data) < 54:
        return f"{target.name} 缺少 generic region 段头"
    number, kind, references, page, length = struct.unpack(">IBBBI", data[43:54])
    if (number, kind, references, page) != (1, 38, 0, 1):
        return f"{target.name} generic region 段头不符"
    region = data[54 : 54 + length]
    if length <= 18 or len(region) != length:
        return f"{target.name} generic region 的 MMR 载荷为空或截断"
    if region[:18] != struct.pack(">IIIIBB", 320, 96, 0, 0, 0, 1):
        return f"{target.name} generic region 尺寸或 MMR 标记不符"
    expected_tail = struct.pack(">IBBBI", 2, 49, 0, 1, 0) + struct.pack(">IBBBI", 3, 51, 0, 0, 0)
    if data[54 + length :] != expected_tail:
        return f"{target.name} 段长度、EndOfPage 或 EndOfFile 收尾不符"
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
    wmf_oracles: bool = False
    format_sweep: bool = False


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
    modes = parser.add_mutually_exclusive_group()
    _ = modes.add_argument("--wmf-oracles", action="store_true", help="仅重建/校验原 WMF 独立 GDI 源图")
    _ = modes.add_argument("--format-sweep", action="store_true", help="仅重建/校验 A25 补充的 13 种格式")
    args = parser.parse_args(namespace=_Arguments())
    if args.wmf_oracles:
        if not args.check_only:
            for line in _regenerate_wmf_oracles():
                print(line)
        problems = _check_wmf_oracles()
    elif args.format_sweep:
        if not args.check_only:
            for line in _regenerate_format_sweep():
                print(line)
        problems = _check_format_sweep()
    else:
        if not args.check_only:
            for line in regenerate():
                print(line)
        problems = check()
    prefix = "重建后校验失败" if not args.check_only else "校验失败"
    for problem in problems:
        print(f"{prefix}：{problem}")
    if args.check_only and not problems:
        if args.wmf_oracles:
            count = len(_WMF_GDI_SIZES)
        elif args.format_sweep:
            count = len(_FORMAT_SWEEP_EXTENSIONS)
        else:
            count = len(managed_files())
        print(f"校验通过：{count} 个受管夹具均满足结构断言")
    return 1 if problems else 0


if __name__ == "__main__":
    raise SystemExit(main())
