"""转 Markdown 验收合成夹具生成器（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import io
import re
import shutil
import subprocess
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING, cast

from defusedxml import ElementTree
from defusedxml.ElementTree import tostring
from PIL import Image, ImageDraw, ImageFont

from scripts.markdown_acceptance_impl.model import SynthResult

if TYPE_CHECKING:
    from collections.abc import Callable

    from scripts.markdown_acceptance_impl.model import Item

__all__ = [
    "A02_IMAGE_COUNT",
    "A02_IMAGE_TOKENS",
    "A07_IMAGE_COUNT",
    "A24_MEDIA_SYNTH_FILES",
    "SYNTHESIZERS",
    "_DOCUMENT_PREDICATE_FILES",
    "_DRAWING_NAMESPACE",
    "_DRAWING_REL_TYPE",
    "_EMU_PER_PX",
    "_IGNORABLE_ATTRIBUTE",
    "_OFFICE_REL_NAMESPACE",
    "_OFFICE_VARIANTS",
    "_PKG_REL_TYPE_IMAGE",
    "_PPTX_REL_TYPE_IMAGE",
    "_SKELETON_LAYOUT_REL_MISSING",
    "_WORD_NAMESPACE",
    "_XLSX_REL_TYPE_IMAGE",
    "_add_broken_ole_body",
    "_add_docx_body_relationships",
    "_add_docx_semantic_sources",
    "_build_ole_preview_predicate",
    "_build_pptx",
    "_ensure_defaults",
    "_pic_xml",
    "_rewrite_content_types",
    "_slide_rels_xml",
    "_slide_xml",
    "_synth_document_predicates",
    "_synth_images",
    "_synth_jbig2_predicates",
    "_synth_media",
    "_synth_office",
    "_synth_pnm",
    "_synth_pptx_descr_no_ocr",
    "_synth_pptx_multi_images",
    "_synth_pptx_runs_fields",
    "_synth_pptx_shared_media",
    "_synth_pptx_svg",
    "_synth_pptx_two_png_order",
    "_synth_pptx_undecodable_image",
    "_synth_precondition",
    "_synth_xlsx_drawing_order",
    "_token_png",
    "_write_pnm_ascii",
    "_write_pnm_binary",
]


# A02 合成输入的图片数量；所有生成、媒体落盘和正文断言共用此常量。
A02_IMAGE_COUNT = 6


A07_IMAGE_COUNT = 2


A02_IMAGE_TOKENS = (
    "ALPHA-TOKEN",
    "BETA-TOKEN",
    "GAMMA-TOKEN",
    "DELTA-TOKEN",
    "EPSILON-TOKEN",
    "ZETA-TOKEN",
)


def _write_pnm_ascii(target: Path) -> None:
    pixels = [[0, 1, 0], [1, 0, 1], [0, 1, 0]]
    _ = target.joinpath("p1_ascii.pbm").write_text(
        "P1\n3 3\n" + "\n".join(" ".join(map(str, row)) for row in pixels) + "\n", encoding="ascii"
    )
    _ = target.joinpath("p2_ascii.pgm").write_text(
        "P2\n3 3\n255\n" + "\n".join(" ".join(str(255 - v * 127) for v in row) for row in pixels) + "\n",
        encoding="ascii",
    )
    _ = target.joinpath("p3_ascii.ppm").write_text(
        "P3\n3 3\n255\n"
        + "\n".join(" ".join(f"{v * 80} {255 - v * 80} {v * 40}" for v in row) for row in pixels)
        + "\n",
        encoding="ascii",
    )


def _write_pnm_binary(target: Path) -> None:
    pixels = [[0, 1, 0], [1, 0, 1], [0, 1, 0]]
    _ = target.joinpath("p4_bin.pbm").write_bytes(
        b"P4\n3 3\n" + bytes((row[0] << 7) | (row[1] << 6) | (row[2] << 5) for row in pixels)
    )
    _ = target.joinpath("p5_bin.pgm").write_bytes(
        b"P5\n3 3\n255\n" + bytes(255 - v * 127 for row in pixels for v in row)
    )
    # P6 3x3 RGB 需要恰好 27 个栅格字节；旧值 *3 是 9 字节的截断输入，
    # 会被 xberg 按损坏输入优雅拒绝（exit 1 + 诊断），A17 因此整项失败。
    _ = target.joinpath("p6_bin.ppm").write_bytes(b"P6\n3 3\n255\n" + bytes((0, 128, 64)) * 9)


def _synth_pnm(target: Path) -> SynthResult:
    """A17：纯标准库构造 PNM 家族 ASCII（P1-P3）与二进制（P4-P6）六种表示."""
    names = ["p1_ascii.pbm", "p2_ascii.pgm", "p3_ascii.ppm", "p4_bin.pbm", "p5_bin.pgm", "p6_bin.ppm"]
    try:
        _write_pnm_ascii(target)
        _write_pnm_binary(target)
    except OSError as exc:
        return SynthResult([], f"构造 PNM 夹具失败：{exc}")
    return SynthResult(names)


def _synth_images(target: Path) -> SynthResult:
    """A16/A20：用 Pillow 合成常规位图与异常/损坏变体."""
    names = [
        "img.jpg",
        "img.jpeg",
        "img.gif",
        "img.tif",
        "img.tiff",
        "img.bmp",
        "img.webp",
        "alpha.png",
        "cmyk.tif",
        "truncated.jpg",
        "huge.png",
        "empty.png",
        "fake_text.png",
    ]
    try:
        base = Image.new("RGB", (64, 64), "white")
        draw = ImageDraw.Draw(base)
        # 高对比黑色条块：可 OCR 程度交给真实引擎，不在合成侧预设文字断言。
        draw.rectangle((6, 10, 20, 26), fill="black")
        draw.rectangle((30, 34, 52, 50), fill="black")
        for name in ("img.jpg", "img.jpeg", "img.gif", "img.tif", "img.tiff", "img.bmp", "img.webp"):
            base.save(target / name)
        Image.new("RGBA", (64, 64), (255, 255, 255, 0)).save(target / "alpha.png")
        Image.new("CMYK", (64, 64), (0, 0, 0, 0)).save(target / "cmyk.tif")
        base.save(target / "truncated.jpg")
        raw = (target / "truncated.jpg").read_bytes()
        _ = (target / "truncated.jpg").write_bytes(raw[: max(1, len(raw) // 3)])
        Image.new("RGB", (8, 8), "white").save(target / "huge.png")
        data = bytearray((target / "huge.png").read_bytes())
        # IHDR 宽高改成超大值：解码必须失败并诊断，而不是漏报或试图分配巨型位图。
        data[16:24] = (60000).to_bytes(4, "big") * 2
        _ = (target / "huge.png").write_bytes(bytes(data))
        _ = (target / "empty.png").write_bytes(b"")
        _ = (target / "fake_text.png").write_bytes(b"this is not an image")
    except (OSError, ValueError) as exc:
        return SynthResult([], f"构造图片夹具失败：{exc}")
    return SynthResult(names)


# Office 变体改写表：源容器主部件 → [(目标扩展名, 目标 ContentType)]，均为 ECMA-376 标准值。
_OFFICE_VARIANTS: dict[str, tuple[str, tuple[tuple[str, str], ...]]] = {
    "chartex.docx": (
        "/word/document.xml",
        (
            ("docm", "application/vnd.ms-word.document.macroEnabled.main+xml"),
            ("dotx", "application/vnd.openxmlformats-officedocument.wordprocessingml.template.main+xml"),
            ("dotm", "application/vnd.ms-word.template.macroEnabled.main+xml"),
        ),
    ),
    "merged_table.pptx": (
        "/ppt/presentation.xml",
        (
            ("pptm", "application/vnd.ms-powerpoint.presentation.macroEnabled.main+xml"),
            ("ppsx", "application/vnd.openxmlformats-officedocument.presentationml.slideshow.main+xml"),
            ("potx", "application/vnd.openxmlformats-officedocument.presentationml.template.main+xml"),
            ("potm", "application/vnd.ms-powerpoint.template.macroEnabled.main+xml"),
        ),
    ),
    "merged_header.xlsx": (
        "/xl/workbook.xml",
        (
            ("xltx", "application/vnd.openxmlformats-officedocument.spreadsheetml.template.main+xml"),
            ("xlsm", "application/vnd.ms-excel.sheet.macroEnabled.main+xml"),
            ("xlam", "application/vnd.ms-excel.addin.macroEnabled.main+xml"),
        ),
    ),
}


def _rewrite_content_types(blob: bytes, part: str, content_type: str) -> bytes:
    text = blob.decode("utf-8")
    pattern = rf'(<Override PartName="{re.escape(part)}" ContentType=")[^"]+(")'
    text, count = re.subn(pattern, rf"\g<1>{content_type}\g<2>", text)
    if count != 1:
        message = f"[Content_Types].xml 未命中主部件 Override：{part}"
        raise ValueError(message)
    return text.encode("utf-8")


def _synth_office(target: Path, fixtures_dir: Path) -> SynthResult:
    """A21-A23：以现有 docx/pptx/xlsx 为容器，按 ECMA-376 内容类型改写派生变体."""
    names: list[str] = []
    for source_name, (part, variants) in _OFFICE_VARIANTS.items():
        origin = fixtures_dir / source_name
        if not origin.is_file():
            continue
        try:
            with zipfile.ZipFile(origin) as archive:
                members = {name: archive.read(name) for name in archive.namelist()}
            for suffix, content_type in variants:
                out = target / f"{origin.stem}_as_{suffix}.{suffix}"
                types_blob = _rewrite_content_types(members["[Content_Types].xml"], part, content_type)
                with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as archive:
                    archive.writestr("[Content_Types].xml", types_blob)
                    for name, blob in members.items():
                        if name != "[Content_Types].xml":
                            archive.writestr(name, blob)
                names.append(out.name)
        except (OSError, ValueError, zipfile.BadZipFile) as exc:
            return SynthResult([], f"容器改写 {source_name} 失败：{exc}")
    if not names:
        return SynthResult([], "容器改写没有产出任何变体（源夹具缺失）")
    return SynthResult(names)


def _synth_media(target: Path) -> SynthResult:
    """A24：保留真实中文音轨的 M4A，以及无语音、无音轨、损坏音轨变体."""
    ffmpeg = shutil.which("ffmpeg")
    if ffmpeg is None:
        return SynthResult([], "PATH 上没有 ffmpeg，无法合成媒体变体")
    commands = [
        (["-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "aac"], "tone.m4a"),
        (["-y", "-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono:duration=2", "-c:a", "aac"], "silence.m4a"),
        (["-y", "-f", "lavfi", "-i", "color=c=black:size=64x64:duration=1", "-c:v", "mpeg4"], "noaudio.mp4"),
        # 音轨来自真实中文 MP4，不以正弦音冒充 M4A 转录。此文件按稳定排序
        # 先于原 MP4，停止当前真实转录后仍有后续文件，避免只在最后一项停止。
        (
            ["-y", "-i", str(target / "video-to-notes-intro-zh.mp4"), "-map", "0:a:0", "-vn", "-c:a", "copy"],
            "video-to-notes-intro-zh.m4a",
        ),
    ]
    try:
        for args, name in commands:
            done = subprocess.run(
                [ffmpeg, *args, str(target / name)], capture_output=True, text=True, check=False, timeout=900
            )
            if done.returncode != 0:
                return SynthResult([], f"ffmpeg 生成 {name} 失败：{(done.stderr or '').strip()[-200:]}")
        raw = (target / "tone.m4a").read_bytes()
        _ = (target / "damaged.mp4").write_bytes(raw[: max(1, len(raw) // 4)])
    except OSError as exc:
        return SynthResult([], f"构造媒体夹具失败：{exc}")
    names: list[str] = list(A24_MEDIA_SYNTH_FILES)
    holes = [name for name in names if not (target / name).is_file()]
    if holes:
        return SynthResult([], f"媒体变体生成不完整：{holes}")
    return SynthResult(names)


# —— OOXML 矩阵夹具运行期合成（A02/A03/A09/A14）——
# 以 tests/markdown_fixtures 的完整 PPTX 骨架为容器（保证与转换器已验证的部件
# 集合兼容），仅替换 slide1 及其关系并注入媒体；XLSX 用最小标准部件集全量构造。


def _token_png(text: str) -> bytes:
    # 长 token 也须完整进入源图；截断的合成真值不能要求引擎补出不可见文字。
    # Pillow ≥10.1 的 load_default(size=…) 返回 FreeTypeFont，但其类型桩标注不完整。
    # [quality-baseline approved 2026-10-03] Pillow 桩缺口（移除即复现），经用户裁定保留
    font = cast(
        "ImageFont.FreeTypeFont",
        ImageFont.load_default(size=24),  # pyright: ignore[reportInvalidCast]
    )
    image = Image.new("RGB", (max(240, int(font.getlength(text)) + 24), 80), "white")
    draw = ImageDraw.Draw(image)
    # Pillow 桩对 ImageDraw.text 的标注不完整（部分未知），按行显式抑制。
    # [quality-baseline approved 2026-10-03] Pillow 桩缺口（移除即复现），经用户裁定保留
    draw.text((12, 26), text, fill="black", font=font)  # pyright: ignore[reportUnknownMemberType]
    buffer = io.BytesIO()
    image.save(buffer, "PNG")
    return buffer.getvalue()


_EMU_PER_PX = 9525


_PPTX_REL_TYPE_IMAGE = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image"


_PKG_REL_TYPE_IMAGE = "http://schemas.openxmlformats.org/package/2006/relationships/image"


_SKELETON_LAYOUT_REL_MISSING = "骨架 slide1.xml.rels 缺少 slideLayout 关系"


_XLSX_REL_TYPE_IMAGE = _PPTX_REL_TYPE_IMAGE


_DRAWING_REL_TYPE = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing"


def _pic_xml(index: int, rid: str, row: int, descr: str = "") -> str:
    attrs = f' descr="{descr}"' if descr else ""
    return (
        f'<p:pic><p:nvPicPr><p:cNvPr id="{index}" name="Picture {index}"{attrs}/>'
        "<p:cNvPicPr/><p:nvPr/></p:nvPicPr>"
        f'<p:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>'
        f'<p:spPr><a:xfrm><a:off x="500000" y="{500000 + row * 900000}"/>'
        f'<a:ext cx="{240 * _EMU_PER_PX}" cy="{80 * _EMU_PER_PX}"/></a:xfrm>'
        '<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>'
    )


def _slide_xml(pics: str) -> str:
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"'
        ' xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">'
        "<p:cSld><p:spTree>"
        '<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>'
        '<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/>'
        '<a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>'
        f"{pics}</p:spTree></p:cSld>"
        "<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"
    )


def _slide_rels_xml(rel_entries: str, layout_rel: str) -> str:
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f"{layout_rel}{rel_entries}</Relationships>"
    )


def _ensure_defaults(content_types: str, defaults: tuple[tuple[str, str], ...]) -> str:
    """[Content_Types].xml 缺失的 Default 逐个补齐（按给定顺序插在 </Types> 前）."""
    for extension, content_type in defaults:
        if f'Extension="{extension}"' in content_types:
            continue
        content_types = content_types.replace(
            "</Types>",
            f'<Default Extension="{extension}" ContentType="{content_type}"/></Types>',
        )
    return content_types


def _build_pptx(
    dest: Path,
    slide_xml: str,
    image_rels: list[tuple[str, str]],
    media: dict[str, bytes],
    extra_defaults: tuple[tuple[str, str], ...] = (),
) -> None:
    """克隆仓库 PPTX 骨架，替换 slide1 与其关系并注入媒体（布局关系保持原样）."""
    from scripts.markdown_acceptance_impl.paths import FIXTURES_DEFAULT

    source = FIXTURES_DEFAULT / "merged_table.pptx"
    with zipfile.ZipFile(source) as archive:
        # 骨架自带的旧 media 成员不属于本次合成输入；保留会让 A02 误得到
        # 7 张图片（6 张目标图 + 1 张骨架残留），从而掩盖关系归属问题。
        members = {member: archive.read(member) for member in archive.namelist() if not member.startswith("ppt/media/")}
    slide_rels = members["ppt/slides/_rels/slide1.xml.rels"].decode("utf-8")
    layout_match = re.search(r"<Relationship [^>]*slideLayout[^>]*/>", slide_rels)
    if layout_match is None:
        raise ValueError(_SKELETON_LAYOUT_REL_MISSING)
    rels = "".join(
        f'<Relationship Id="{rid}" Type="{_PPTX_REL_TYPE_IMAGE}" Target="../media/{part}"/>' for rid, part in image_rels
    )
    types = _ensure_defaults(
        members["[Content_Types].xml"].decode("utf-8"),
        (("png", "image/png"), *extra_defaults),
    )
    members["[Content_Types].xml"] = types.encode("utf-8")
    members["ppt/slides/slide1.xml"] = slide_xml.encode("utf-8")
    members["ppt/slides/_rels/slide1.xml.rels"] = _slide_rels_xml(rels, layout_match.group(0)).encode("utf-8")
    for part, blob in media.items():
        members[f"ppt/media/{part}"] = blob
    with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as archive:
        for member, blob in members.items():
            archive.writestr(member, blob)


def _synth_pptx_multi_images(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A02：一张 slide 六张不同文字图片（rId1..rId6），供两次连跑字节比对."""
    try:
        indexes = range(1, A02_IMAGE_COUNT + 1)
        media = {f"image{index}.png": _token_png(A02_IMAGE_TOKENS[index - 1]) for index in indexes}
        pics = "".join(_pic_xml(index + 1, f"rId{index}", index - 1) for index in indexes)
        rels = [(f"rId{index}", f"image{index}.png") for index in indexes]
        _build_pptx(target / "pptx_multi_images.pptx", _slide_xml(pics), rels, media)
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A02 PPTX 失败：{exc}")
    return SynthResult(["pptx_multi_images.pptx"])


def _synth_pptx_shared_media(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A03：两个 shape 引用同一 media（同一 rId），两处位置各自保留."""
    try:
        media = {"image1.png": _token_png("SHARED-MEDIA-TOKEN")}
        pics = _pic_xml(2, "rId10", 0) + _pic_xml(3, "rId10", 1)
        _build_pptx(target / "pptx_shared_media.pptx", _slide_xml(pics), [("rId10", "image1.png")], media)
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A03 PPTX 失败：{exc}")
    return SynthResult(["pptx_shared_media.pptx"])


def _synth_pptx_descr_no_ocr(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A09：descr 带文字而图片本体无字——descr 不得冒充 OCR 正文."""
    try:
        media = {"image1.png": _token_png("")}
        pics = _pic_xml(2, "rId10", 0, descr="DESCR-NO-OCR-TOKEN")
        _build_pptx(target / "pptx_descr_no_ocr.pptx", _slide_xml(pics), [("rId10", "image1.png")], media)
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A09 PPTX 失败：{exc}")
    return SynthResult(["pptx_descr_no_ocr.pptx"])


def _synth_pptx_two_png_order(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A01：两张不同文字图片，rels 列举顺序与 blip 引用顺序交错，OCR 不得互换."""
    try:
        media = {"image1.png": _token_png("ALPHA-ONE"), "image2.png": _token_png("BETA-TWO")}
        # rels 列举 rId10→image1、rId11→image2；slide 先引用 rId11 再 rId10。
        pics = _pic_xml(2, "rId11", 0) + _pic_xml(3, "rId10", 1)
        _build_pptx(
            target / "pptx_two_png_order.pptx",
            _slide_xml(pics),
            [("rId10", "image1.png"), ("rId11", "image2.png")],
            media,
        )
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A01 PPTX 失败：{exc}")
    return SynthResult(["pptx_two_png_order.pptx"])


def _synth_pptx_svg(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A07：SVG 文本成员带失效外部引用——本地解析、外部资源禁用、失败隔离."""
    svg = (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<svg xmlns="http://www.w3.org/2000/svg" width="240" height="80">'
        '<image href="http://127.0.0.1:9/never-resolves.png" width="1" height="1"/>'
        '<text x="12" y="48" font-size="24" fill="black">SVG-LOCAL-TEXT</text></svg>'
    )
    try:
        # 骨架替换：以 PNG 占位保持 zip 结构简单，SVG 作为独立媒体成员注入。
        media = {"image1.svg": svg.encode("utf-8"), "image2.png": _token_png("PNG-NEIGHBOR")}
        pics = _pic_xml(2, "rId10", 0) + _pic_xml(3, "rId11", 1)
        _build_pptx(
            target / "pptx_svg.pptx",
            _slide_xml(pics),
            [("rId10", "image1.svg"), ("rId11", "image2.png")],
            media,
            extra_defaults=(("svg", "image/svg+xml"),),
        )
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A07 PPTX 失败：{exc}")
    return SynthResult(["pptx_svg.pptx"])


def _synth_pptx_runs_fields(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A08：普通文本 run 与可见字段（a:fld）混排，字段值完整保留."""
    runs = (
        '<p:sp><p:nvSpPr><p:cNvPr id="9" name="TextBox"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
        '<p:spPr><a:xfrm><a:off x="500000" y="5600000"/><a:ext cx="8000000" cy="500000"/></a:xfrm>'
        '<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/>'
        '<a:lstStyle/><a:p><a:r><a:rPr lang="zh-CN" sz="1800"/><a:t>RUN-AND-FIELD</a:t></a:r>'
        '<a:fld id="{00000000-0000-0000-0000-000000006E1B}" type="slidenum">'
        '<a:rPr lang="zh-CN" sz="1800"/><a:t>7</a:t></a:fld>'
        '<a:r><a:rPr lang="zh-CN" sz="1800"/><a:t>-FIELD-END</a:t></a:r></a:p></p:txBody></p:sp>'
    )
    try:
        media = {"image1.png": _token_png("A08-NEIGHBOR")}
        pics = _pic_xml(2, "rId10", 0)
        slide = _slide_xml(pics + runs)
        _build_pptx(target / "pptx_runs_fields.pptx", slide, [("rId10", "image1.png")], media)
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A08 PPTX 失败：{exc}")
    return SynthResult(["pptx_runs_fields.pptx"])


def _synth_pptx_undecodable_image(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A10：截断图片与正常内容同页——诊断阶段明确，其余内容继续输出."""
    try:
        good = _token_png("HEALTHY-TEXT-REMAINS")
        broken = _token_png("BROKEN")[: len(_token_png("BROKEN")) // 3]
        media = {"image1.png": broken, "image2.png": good}
        pics = _pic_xml(2, "rId10", 0) + _pic_xml(3, "rId11", 1)
        _build_pptx(
            target / "pptx_undecodable_image.pptx",
            _slide_xml(pics),
            [("rId10", "image1.png"), ("rId11", "image2.png")],
            media,
        )
    except (OSError, ValueError, zipfile.BadZipFile) as exc:
        return SynthResult([], f"构造 A10 PPTX 失败：{exc}")
    return SynthResult(["pptx_undecodable_image.pptx"])


def _synth_xlsx_drawing_order(target: Path, _fixtures_dir: Path) -> SynthResult:
    """A14：最小 XLSX，两 anchor 显示顺序与 .rels 列举顺序相反，两图各有可 OCR 文字."""
    content_types = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Default Extension="png" ContentType="image/png"/>'
        '<Override PartName="/xl/workbook.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>'
        '<Override PartName="/xl/worksheets/sheet1.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>'
        '<Override PartName="/xl/drawings/drawing1.xml" ContentType='
        '"application/vnd.openxmlformats-officedocument.drawing+xml"/>'
        "</Types>"
    )
    root_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument'
        '/2006/relationships/officeDocument" Target="xl/workbook.xml"/>'
        "</Relationships>"
    )
    workbook = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        '<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>'
    )
    workbook_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument'
        '/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>'
        "</Relationships>"
    )
    sheet = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        "<sheetData/>"
        '<drawing r:id="rId1"/></worksheet>'
    )
    sheet_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f'<Relationship Id="rId1" Type="{_DRAWING_REL_TYPE}" '
        f'Target="../drawings/drawing1.xml"/>'
        "</Relationships>"
    )
    # 显示顺序由 anchor 决定：先 rId2（BOTTOM 在前）再 rId1；.rels 列举顺序相反。
    drawing = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing"'
        ' xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        "<xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>"
        "<xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>6</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>"
        '<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="Bottom"/><xdr:cNvPicPr/><xdr:nvPr/></xdr:nvPicPr>'
        '<xdr:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill>'
        '<xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic>'
        "<xdr:clientData/></xdr:twoCellAnchor>"
        "<xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff>"
        "<xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>"
        "<xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff>"
        "<xdr:row>13</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>"
        '<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="3" name="Top"/><xdr:cNvPicPr/><xdr:nvPr/></xdr:nvPicPr>'
        '<xdr:blipFill><a:blip r:embed="rId1"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill>'
        '<xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic>'
        "<xdr:clientData/></xdr:twoCellAnchor>"
        "</xdr:wsDr>"
    )
    drawing_rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f'<Relationship Id="rId1" Type="{_XLSX_REL_TYPE_IMAGE}" Target="../media/image1.png"/>'
        f'<Relationship Id="rId2" Type="{_XLSX_REL_TYPE_IMAGE}" Target="../media/image2.png"/>'
        "</Relationships>"
    )
    members = {
        "[Content_Types].xml": content_types,
        "_rels/.rels": root_rels,
        "xl/workbook.xml": workbook,
        "xl/_rels/workbook.xml.rels": workbook_rels,
        "xl/worksheets/sheet1.xml": sheet,
        "xl/worksheets/_rels/sheet1.xml.rels": sheet_rels,
        "xl/drawings/drawing1.xml": drawing,
        "xl/drawings/_rels/drawing1.xml.rels": drawing_rels,
    }
    try:
        with zipfile.ZipFile(target / "xlsx_drawing_order.xlsx", "w", zipfile.ZIP_DEFLATED) as archive:
            for member, text in members.items():
                archive.writestr(member, text.encode("utf-8"))
            archive.writestr("xl/media/image1.png", _token_png("XLSX-FIRST-REL-TOKEN"))
            archive.writestr("xl/media/image2.png", _token_png("XLSX-FIRST-DRAW-TOKEN"))
    except OSError as exc:
        return SynthResult([], f"构造 A14 XLSX 失败：{exc}")
    return SynthResult(["xlsx_drawing_order.xlsx"])


_DOCUMENT_PREDICATE_FILES = {
    "A11": "pptx_ole_broken_preview.pptx",
    "A12": "docx_all_sources.docx",
    "A13": "docx_emf_wmf_raster.docx",
}


_WORD_NAMESPACE = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"


_DRAWING_NAMESPACE = "{http://schemas.openxmlformats.org/drawingml/2006/main}"


_OFFICE_REL_NAMESPACE = "{http://schemas.openxmlformats.org/officeDocument/2006/relationships}"


_IGNORABLE_ATTRIBUTE = "{http://schemas.openxmlformats.org/markup-compatibility/2006}Ignorable"


def _synth_jbig2_predicates(target: Path, fixtures_dir: Path) -> SynthResult:
    """以固定单页 JBIG2 合成独立图片与仅含图像流的 PDF."""
    # 复用从零构造的固定 JBIG2；PDF 只有图像流，无原生 token 可冒充 OCR。
    if not (target / "jbig2_standalone.jb2").is_file():
        _ = shutil.copy2(fixtures_dir / "matrix/jbig2_standalone.jb2", target / "jbig2_standalone.jb2")
    data = (target / "jbig2_standalone.jb2").read_bytes()
    if data[:13] != b"\x97JB2\r\n\x1a\n\x01\x00\x00\x00\x01":
        return SynthResult([], "A19 缺少固定单页 JBIG2 合成源，不能使用未知样本作真值")
    _ = (target / "jbig2_standalone.jbig2").write_bytes(data)
    stream = data[13:-11]  # 删除文件头和 EndOfFile；保留 EndOfPage。
    content = b"q 320 0 0 96 0 0 cm /Im0 Do Q"
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"".join(
            (
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 96] ",
                b"/Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>",
            )
        ),
        b"".join(
            (
                b"<< /Type /XObject /Subtype /Image /Width 320 /Height 96 ",
                b"/ColorSpace /DeviceGray /BitsPerComponent 1 /Filter /JBIG2Decode /Length ",
                str(len(stream)).encode("ascii"),
                b" >>\nstream\n",
                stream,
                b"\nendstream",
            )
        ),
        b"<< /Length " + str(len(content)).encode("ascii") + b" >>\nstream\n" + content + b"\nendstream",
    ]
    pdf = bytearray(b"%PDF-1.4\n")
    offsets = [0]
    for number, blob in enumerate(objects, 1):
        offsets.append(len(pdf))
        pdf.extend(f"{number} 0 obj\n".encode("ascii") + blob + b"\nendobj\n")
    xref = len(pdf)
    pdf.extend(f"xref\n0 {len(offsets)}\n0000000000 65535 f \n".encode("ascii"))
    for offset in offsets[1:]:
        pdf.extend(f"{offset:010d} 00000 n \n".encode("ascii"))
    pdf.extend(f"trailer\n<< /Size {len(offsets)} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode("ascii"))
    _ = (target / "pdf_jbig2.pdf").write_bytes(pdf)
    return SynthResult(["jbig2_standalone.jb2", "jbig2_standalone.jbig2", "pdf_jbig2.pdf"])


def _build_ole_preview_predicate(dest: Path) -> None:
    preview = _pic_xml(2, "rId10", 0)
    ole = "".join(
        (
            '<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="3" name="OLE"/>',
            "<p:cNvGraphicFramePr/><p:nvPr/></p:nvGraphicFramePr>",
            '<p:xfrm><a:off x="0" y="0"/><a:ext cx="2286000" cy="762000"/></p:xfrm>',
            '<a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/presentationml/2006/ole">',
            '<p:oleObj r:id="rIdOle" progId="Excel.Sheet.12" name="Broken body">',
            f"<p:embed/>{preview}</p:oleObj></a:graphicData></a:graphic></p:graphicFrame>",
        )
    )
    _build_pptx(dest, _slide_xml(ole), [("rId10", "preview_ok.png")], {"preview_ok.png": _token_png("OLE-PREVIEW-OK")})


def _add_broken_ole_body(members: dict[str, bytes]) -> None:
    rel = "ppt/slides/_rels/slide1.xml.rels"
    members[rel] = members[rel].replace(
        b"</Relationships>",
        b"".join(
            (
                b'<Relationship Id="rIdOle" Type="http://schemas.openxmlformats.org/officeDocument/',
                b'2006/relationships/oleObject" Target="../embeddings/ole_body_broken.xlsx"/></Relationships>',
            )
        ),
    )
    # ZIP 签名保留、中央目录缺失：不能被 MIME sniff 当普通文本成功解析。
    members["ppt/embeddings/ole_body_broken.xlsx"] = b"PK\x03\x04" + bytes(26)
    members["[Content_Types].xml"] = _ensure_defaults(
        members["[Content_Types].xml"].decode("utf-8"),
        (("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),),
    ).encode("utf-8")


def _add_docx_body_relationships(members: dict[str, bytes], rel: bytes) -> None:
    original = members["word/_rels/document.xml.rels"].decode("utf-8")
    additions = rel.decode("utf-8").split("<Relationships", 1)[1].split(">", 1)[1]
    additions = additions.removesuffix("</Relationships>")
    for note in ("footnotes", "endnotes"):
        additions += "".join(
            (
                f'<Relationship Id="rId{note}" Type="http://schemas.openxmlformats.org/',
                f'officeDocument/2006/relationships/{note}" Target="{note}.xml"/>',
            )
        )
    members["word/_rels/document.xml.rels"] = original.replace(
        "</Relationships>", additions + "</Relationships>"
    ).encode("utf-8")


def _add_docx_semantic_sources(members: dict[str, bytes]) -> str | None:
    # 每个来源独立图片/token；rId 局部作用域复用，关系解析错用正文表可检出。
    document = ElementTree.fromstring(members["word/document.xml"])
    # ElementTree 重命名前缀后，原 mc:Ignorable 的前缀列表已不再可解析。
    _ = document.attrib.pop(_IGNORABLE_ATTRIBUTE, None)
    body = document.find(f"{_WORD_NAMESPACE}body")
    if body is None:
        return "A12 固定骨架缺少正文"
    picture = next(paragraph for paragraph in body if paragraph.find(f".//{_DRAWING_NAMESPACE}blip") is not None)
    picture_xml = tostring(picture)

    # 节点由安全解析器推导类型，并保持在拥有正文树的局部作用域内。
    def add_source(source: str) -> None:
        token = f"{source}-IMAGE-TOKEN"
        member = f"media/{source.lower()}.png"
        members[f"word/{member}"] = _token_png(token)
        part = ElementTree.fromstring(picture_xml)
        for blip in part.iter(f"{_DRAWING_NAMESPACE}blip"):
            blip.set(f"{_OFFICE_REL_NAMESPACE}embed", "rIdSemantic")
        for text in part.iter(f"{_WORD_NAMESPACE}t"):
            text.text = ""
        rel = _slide_rels_xml(
            f'<Relationship Id="rIdSemantic" Type="{_PPTX_REL_TYPE_IMAGE}" Target="{member}"/>', ""
        ).encode("utf-8")
        if source == "BODY":
            picture = next(
                paragraph for paragraph in body if paragraph.find(f".//{_DRAWING_NAMESPACE}blip") is not None
            )
            body.remove(picture)
            body.insert(1, part)
            _add_docx_body_relationships(members, rel)
        elif source in ("HEADER", "FOOTER"):
            filename = f"{source.lower()}1.xml"
            root = ElementTree.fromstring(members[f"word/{filename}"])
            _ = root.attrib.pop(_IGNORABLE_ATTRIBUTE, None)
            for child in list(root):
                root.remove(child)
            root.append(part)
            members[f"word/{filename}"] = tostring(root)
            members[f"word/_rels/{filename}.rels"] = rel
        else:
            note = source.lower()
            word_namespace = _WORD_NAMESPACE[1:-1]
            root = ElementTree.fromstring(f'<w:{note}s xmlns:w="{word_namespace}"><w:{note} w:id="2"/></w:{note}s>')
            root[0].append(part)
            members[f"word/{note}s.xml"] = tostring(root)
            members[f"word/_rels/{note}s.xml.rels"] = rel
            paragraph = ElementTree.fromstring(
                f'<w:p xmlns:w="{word_namespace}"><w:r><w:{note}Reference w:id="2"/></w:r></w:p>'
            )
            body.insert(2, paragraph)
            types = members["[Content_Types].xml"].decode("utf-8")
            members["[Content_Types].xml"] = types.replace(
                "</Types>",
                "".join(
                    (
                        f'<Override PartName="/word/{note}s.xml" ContentType="application/vnd.',
                        f'openxmlformats-officedocument.wordprocessingml.{note}s+xml"/></Types>',
                    )
                ),
            ).encode("utf-8")

    for source in ("BODY", "HEADER", "FOOTER", "FOOTNOTE", "ENDNOTE"):
        add_source(source)
    members["word/document.xml"] = tostring(document)
    return None


def _synth_document_predicates(target: Path, fixtures_dir: Path, kind: str) -> SynthResult:
    """在既有隔离合成流程内补真实关联；固定仓库夹具只读，不生成永久二进制."""
    try:
        if kind == "A19":
            return _synth_jbig2_predicates(target, fixtures_dir)
        dest = target / _DOCUMENT_PREDICATE_FILES[kind]
        if kind == "A11":
            _build_ole_preview_predicate(dest)
        elif not dest.is_file():
            _ = shutil.copy2(fixtures_dir / "matrix" / _DOCUMENT_PREDICATE_FILES[kind], dest)
        with zipfile.ZipFile(dest) as archive:
            members = {name: archive.read(name) for name in archive.namelist()}
        if kind == "A11":
            _add_broken_ole_body(members)
        elif kind == "A13":
            # 保留 EMF 签名及声明尺寸，只截断记录，保证是解码/光栅化失败而非未知格式。
            members["word/media/broken.emf"] = members["word/media/broken.emf"][:88]
            members["word/media/healthy.png"] = _token_png("RASTER-HEALTHY")
        else:
            error = _add_docx_semantic_sources(members)
            if error is not None:
                return SynthResult([], error)
        with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as archive:
            for name, blob in members.items():
                archive.writestr(name, blob)
    except (OSError, ValueError, KeyError, StopIteration, zipfile.BadZipFile) as error:
        return SynthResult([], f"{kind} 语义夹具合成失败：{error}")
    return SynthResult([_DOCUMENT_PREDICATE_FILES[kind]])


SYNTHESIZERS: dict[str, Callable[[Path, Path], SynthResult]] = {
    "pnm": lambda target, _fixtures: _synth_pnm(target),
    "images": lambda target, _fixtures: _synth_images(target),
    "office": _synth_office,
    "media": lambda target, _fixtures: _synth_media(target),
    "pptx_two_png_order": _synth_pptx_two_png_order,
    "pptx_svg": _synth_pptx_svg,
    "pptx_runs_fields": _synth_pptx_runs_fields,
    "pptx_undecodable_image": _synth_pptx_undecodable_image,
    "pptx_multi_images": _synth_pptx_multi_images,
    "pptx_shared_media": _synth_pptx_shared_media,
    "pptx_descr_no_ocr": _synth_pptx_descr_no_ocr,
    "xlsx_drawing_order": _synth_xlsx_drawing_order,
    "document_a11": lambda target, fixtures: _synth_document_predicates(target, fixtures, "A11"),
    "document_a12": lambda target, fixtures: _synth_document_predicates(target, fixtures, "A12"),
    "document_a13": lambda target, fixtures: _synth_document_predicates(target, fixtures, "A13"),
    "document_a19": lambda target, fixtures: _synth_document_predicates(target, fixtures, "A19"),
}


def _synth_precondition(item: Item) -> str | None:
    if item.synth == "media" and shutil.which("ffmpeg") is None:
        return "合成器需要 PATH 上的 ffmpeg"
    return None


# A24 媒体合成变体（与 _synth_media 的产出同名；damaged 为字节截断，必须失败）。
A24_MEDIA_SYNTH_FILES = (
    "tone.m4a",
    "silence.m4a",
    "noaudio.mp4",
    "damaged.mp4",
    "video-to-notes-intro-zh.m4a",
)
