"""转 Markdown 验收文档/图源语义核对（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import io
import ntpath
import re
import struct
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING, cast
from urllib.parse import unquote

from defusedxml import ElementTree
from PIL import Image, ImageChops, ImageDraw, ImageFont, ImageStat

from scripts.markdown_acceptance_impl.model import (
    STATUS_FAILED,
    STATUS_OK,
    ContentSpec,
    InputRule,
    Outcome,
    _file_digest,
)
from tests.markdown_fixtures.generate_synthetic import render_wmf_gdi

if TYPE_CHECKING:
    from collections.abc import Callable

    from scripts.markdown_acceptance_impl.model import GuiRun, Item, PreparedInputs

__all__ = [
    "DATA_URI_PATTERN",
    "IMAGE_SUFFIXES",
    "MARKDOWN_IMAGE_LINK_PATTERN",
    "MARKDOWN_INLINE_CODE_PATTERN",
    "TEMP_LEFTOVER_MARKER",
    "_A25_RULES",
    "_BROKEN_IMAGE_INPUTS",
    "_CONTENT_ASSERTS",
    "_DOCUMENT_IMAGE_BINDINGS",
    "_EMBEDDED_FAILURE_STAGES",
    "_IMAGE_INK_LOOKUP",
    "_IMAGE_INK_THRESHOLD",
    "_IMAGE_MAX_INK_MEAN_ERROR",
    "_INPUT_RULES",
    "_ITEM_DEFAULT_RULES",
    "_MEDIA_FAILURE_STAGES",
    "_MEMBER_FAILURE_STAGES",
    "_OPTIONAL_IMAGE_INPUTS",
    "_SOURCE_CONTENT_ITEMS",
    "_SOURCE_TEXT_PART",
    "_TEXT_FENCE",
    "_VECTOR_SOURCE_SPECS",
    "_WMF_HEADER_BYTES",
    "_WMF_RECORD_HEADER_BYTES",
    "_WMF_RECORD_MIN_WORDS",
    "_WMF_WINDOW_EXTENT",
    "_WMF_WINDOW_ORIGIN",
    "_assess_conversion",
    "_collect_media_references",
    "_content_problems",
    "_document_image_problems",
    "_document_semantic_problems",
    "_expected_markdown_name",
    "_expected_markdown_names",
    "_image_binding_problems",
    "_image_point",
    "_input_rule",
    "_jbig2_semantic_problems",
    "_markdown_container_content",
    "_markdown_open_containers",
    "_mask_markdown_fences",
    "_mixed_docx_problems",
    "_partial_stage_problems",
    "_per_input_problems",
    "_per_source_content_problems",
    "_same_image_content",
    "_same_image_pixels",
    "_source_content_spec",
    "_source_embedded_problems",
    "_source_input_spec",
    "_source_media_preserved",
    "_source_member_problems",
    "_source_wmf_with_bounds",
    "_token_image_order_problems",
    "_verify_media_references",
    "_wmf_content_preserved",
    "verify_common_postconditions",
]


# ---------------------------------------------------------------- 结果核验。


DATA_URI_PATTERN = re.compile(r"data:image/[a-zA-Z0-9.+-]+;base64")


TEMP_LEFTOVER_MARKER = ".jch-markdown-"


IMAGE_SUFFIXES = (
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".tif",
    ".tiff",
    ".bmp",
    ".webp",
    ".jp2",
    ".j2k",
    ".j2c",
    ".jpx",
    ".jpm",
    ".mj2",
    ".svg",
)


MARKDOWN_IMAGE_LINK_PATTERN = re.compile(r"!\[[^\]\n]*\]\(\s*(?P<target><[^>\n]+>|[^)\n]+?)\s*\)")


MARKDOWN_INLINE_CODE_PATTERN = re.compile(r"(?<!`)`+(?!`)[^`\n]+`+(?!`)")


def _markdown_container_content(line: str, containers: list[tuple[str, int]]) -> tuple[str, list[tuple[str, int]]]:
    """仅剥除仍在延续的引用块/列表前缀，不把代码正文当作新容器。."""
    continued: list[tuple[str, int]] = []
    for kind, width in containers:
        if kind == "quote":
            prefix = re.match(r"^[ ]{0,3}>[ \t]?", line)
            if prefix is None:
                break
            line = line[prefix.end() :]
        elif line.strip():
            if not line.startswith(" " * width):
                break
            line = line[width:]
        continued.append((kind, width))
    return line, continued


def _markdown_open_containers(line: str, containers: list[tuple[str, int]]) -> tuple[str, list[tuple[str, int]]]:
    """记录当前行新增的引用块及列表内容缩进，供后续围栏行延续。."""
    while True:
        quote = re.match(r"^[ ]{0,3}>[ \t]?", line)
        if quote is not None:
            containers.append(("quote", 0))
            line = line[quote.end() :]
            continue
        item = re.match(r"^[ ]{0,3}(?:[-+*]|[0-9]{1,9}[.)])([ ]{1,4})(?=\S|[\r\n]|$)", line)
        if item is None:
            return line, containers
        containers.append(("list", item.end()))
        line = line[item.end() :]


def _mask_markdown_fences(text: str) -> str:
    """按行屏蔽围栏并保留偏移；容器结束及合法闭合标记才结束围栏。."""
    masked: list[str] = []
    fence: tuple[str, int] | None = None
    containers: list[tuple[str, int]] = []
    for line in text.splitlines(keepends=True):
        content, continued = _markdown_container_content(line, containers)
        if fence is not None and continued == containers:
            marker, length = fence
            masked.append(re.sub(r"[^\r\n]", " ", line))
            if re.fullmatch(rf"[ ]{{0,3}}{re.escape(marker)}{{{length},}}[ \t]*(?:\r?\n)?", content):
                fence = None
            continue
        # 未闭合围栏到所属容器结束即止；同一行仍可能包含外层的真实引用。
        fence = None
        content, containers = _markdown_open_containers(content, continued)
        opening = re.match(r"^[ ]{0,3}(`{3,}|~{3,})([^\r\n]*)", content)
        if opening is not None and not (opening[1][0] == "`" and "`" in opening[2]):
            fence = (opening[1][0], len(opening[1]))
            masked.append(re.sub(r"[^\r\n]", " ", line))
        else:
            masked.append(line)
    return "".join(masked)


def _expected_markdown_name(name: str) -> str:
    stem = Path(name).stem
    suffix = Path(name).suffix.lstrip(".").lower()
    return f"{stem}_{suffix}.md" if suffix else f"{stem}.md"


def _expected_markdown_names(files: list[str]) -> list[str]:
    return [_expected_markdown_name(name) for name in files]


# 图像合成器（A16/A20 共用）产出的变体分类：截断/超大/空文件/错误扩展名
# 按 T-25 预期为失败——A20 对这四项强制 expect_failure（必须有失败诊断），
# A16 登记为宽松（不索产物也不索诊断，保守不假 PASS）；透明背景与异常色彩
# 空间成功（空 OCR）或失败均可。
_BROKEN_IMAGE_INPUTS = ("truncated.jpg", "huge.png", "empty.png", "fake_text.png")


_OPTIONAL_IMAGE_INPUTS = ("alpha.png", "cmyk.tif")


_INPUT_RULES: dict[str, dict[str, InputRule]] = {
    "A16": {name: InputRule(must_produce=False) for name in (*_BROKEN_IMAGE_INPUTS, *_OPTIONAL_IMAGE_INPUTS)},
    "A20": {name: InputRule(must_produce=False, expect_failure=True) for name in _BROKEN_IMAGE_INPUTS}
    | {name: InputRule(must_produce=False) for name in _OPTIONAL_IMAGE_INPUTS},
    "A24": {"damaged.mp4": InputRule(must_produce=False, expect_failure=True)},
}


# 条目级默认规则：A25 的格式清点集合由引擎清单运行期决定，保留子集语义
# （未验证不得列为通过，由「至少一份产物」兜底；逐格式行为不强断言）。
_ITEM_DEFAULT_RULES: dict[str, InputRule] = {"A25": InputRule(must_produce=False)}


# A25 动态规则（S10-02 复审）：拿到引擎实查清单后，由 _run_matrix_a25 按
# 「引擎声明的格式必须有产物、未声明格式保留宽松兜底」重写——静态默认的
# 全宽松会让「引擎停止转换某格式」这类回归以假 PASS 溜过。验收单线程，
# 模块级重写安全；单测直接构造该表锁定判定。
_A25_RULES: dict[str, InputRule] = {}


def _input_rule(item_id: str, filename: str) -> InputRule:
    """查单个输入的验收期望；未登记的健康输入默认必须有产物。."""
    if item_id == "A25":
        return _A25_RULES.get(filename, _ITEM_DEFAULT_RULES.get(item_id, InputRule()))
    return _INPUT_RULES.get(item_id, {}).get(filename, _ITEM_DEFAULT_RULES.get(item_id, InputRule()))


def _per_input_problems(item_id: str, files: list[str], produced: list[str], texts: str) -> list[str]:
    """逐输入断言：健康输入必须有产物；预期失败输入不得有产物且须有失败诊断。."""
    problems: list[str] = []
    produced_names = set(produced)
    for name in files:
        rule = _input_rule(item_id, name)
        expected = _expected_markdown_name(name)
        if rule.must_produce and expected not in produced_names:
            problems.append(f"健康输入未产出对应 Markdown：{name}（预期 {expected}）")
        if rule.expect_failure:
            if expected in produced_names:
                problems.append(f"预期失败输入产出了结果（应失败不留半成品，T-25）：{name}")
            elif re.search(rf"(?m)^失败：{re.escape(name)} · \S[^\n]*$", texts) is None:
                problems.append(f"预期失败输入缺少失败诊断（T-16/T-24 须显示文件与原因）：{name}")
    return problems


def _collect_media_references(markdown: Path, output_dir: Path) -> tuple[list[str], set[Path]]:
    """收集一份 Markdown 中的实际图片链接及其问题。."""
    problems: list[str] = []
    referenced: set[Path] = set()
    text = markdown.read_text(encoding="utf-8", errors="replace")
    rendered = _mask_markdown_fences(text)
    rendered = MARKDOWN_INLINE_CODE_PATTERN.sub("", rendered)
    for match in MARKDOWN_IMAGE_LINK_PATTERN.finditer(rendered):
        raw = match.group("target").strip()
        raw = raw[1:-1] if raw.startswith("<") and raw.endswith(">") else raw.split(maxsplit=1)[0]
        if not raw or raw.startswith("#"):
            # 「# 开头」是合法锚点语义，跳过；空目标无媒体落盘要求。
            continue
        if "://" in raw:
            # T-14 最新零改写协议保留引擎最终正文中的原有外链（如源 HTML）。
            # 外链不是本地落盘媒体，不据此要求转换联网补图；本地资源仍逐项核对。
            continue
        local_reference = unquote(raw)
        if ntpath.isabs(local_reference.replace("/", "\\")):
            problems.append(f"图片引用逃出输出目录：{markdown.relative_to(output_dir)} -> {raw}")
            continue
        target = (markdown.parent / Path(local_reference.replace("/", "\\"))).resolve()
        try:
            _ = target.relative_to(output_dir.resolve())
        except ValueError:
            problems.append(f"图片引用逃出输出目录：{markdown.relative_to(output_dir)} -> {raw}")
            continue
        if not target.is_file():
            problems.append(f"图片引用目标不存在：{markdown.relative_to(output_dir)} -> {raw}")
        else:
            referenced.add(target)
    return problems, referenced


def _verify_media_references(output_dir: Path) -> list[str]:
    """验证 T-14 图片落盘位置、正文引用和引用目标的一致性。."""
    problems: list[str] = []
    referenced: set[Path] = set()
    for markdown in output_dir.rglob("*.md"):
        if markdown.is_file():
            markdown_problems, markdown_references = _collect_media_references(markdown, output_dir)
            problems.extend(markdown_problems)
            referenced.update(markdown_references)
    output_root = output_dir.resolve()
    for image in output_dir.rglob("*"):
        if not image.is_file():
            continue
        is_media_file = image.parent.name.endswith("_media")
        if image.suffix.lower() not in IMAGE_SUFFIXES and not is_media_file:
            continue
        relative = image.relative_to(output_dir)
        if not image.parent.name.endswith("_media"):
            problems.append(f"图片未放入产物媒体目录：{relative}")
        if image.resolve() not in referenced:
            problems.append(f"图片没有正文引用：{relative}")
        try:
            _ = image.resolve().relative_to(output_root)
        except ValueError:
            problems.append(f"图片路径逃出输出目录：{relative}")
    return problems


def verify_common_postconditions(source_dir: Path, output_dir: Path, sources: dict[Path, str]) -> list[str]:
    """附录 A 第 26 项横切断言：媒体引用完整、无 Base64、源不变、无临时残留."""
    problems = [
        f"源文件被改动：{relative}"
        for relative, digest in sources.items()
        if not (source_dir / relative).is_file() or _file_digest(source_dir / relative) != digest
    ]
    problems.extend(f"输出残留临时文件：{leftover}" for leftover in output_dir.rglob(f"*{TEMP_LEFTOVER_MARKER}*"))
    problems.extend(_verify_media_references(output_dir))
    problems.extend(
        f"结果含 Base64 图片：{markdown.relative_to(output_dir)}"
        for markdown in output_dir.rglob("*.md")
        if markdown.is_file() and DATA_URI_PATTERN.search(markdown.read_text(encoding="utf-8", errors="replace"))
    )
    return problems


def _assess_conversion(  # noqa: PLR0911  # 前置失败类别各自独立呈现，合并会吞掉具体原因
    item: Item, prepared: PreparedInputs, sources: dict[Path, str], run: GuiRun, *, stop_mode: bool = False
) -> Outcome:
    problems = verify_common_postconditions(prepared.input_dir, prepared.output_dir, sources)
    produced = [path.name for path in run.outputs]
    expected = _expected_markdown_names(prepared.files)
    details = [
        f"{item.item_id} 预期产物 {len(expected)} 项，实得 {len(produced)} 项：{produced}",
        *run.texts.splitlines()[-6:],
    ]
    if problems:
        return Outcome(STATUS_FAILED, "；".join(problems[:5]), details)
    if len(produced) != len(set(produced)):
        return Outcome(STATUS_FAILED, "同一顶层输入存在重复 Markdown 产物（T-10 每个输入最多一份）", details)
    empty = [path.name for path in run.outputs if not path.read_text(encoding="utf-8", errors="replace").strip()]
    if empty:
        return Outcome(STATUS_FAILED, f"产物正文为空：{empty}", details)
    if stop_mode:
        # 停止语义（T-23）只断言已完成结果保留与源不变；「后续文件未处理」由
        # 调用方按条目补充（如 A24 的 _verify_a24_stop）。
        details.insert(0, "停止路径：已完成结果保留、源文件不变即符合 T-23；完整统计断言待资产环境调校")
        return Outcome(STATUS_OK, details=details)
    if item.item_id == "A26" and (len(produced) != len(expected) or set(produced) != set(expected)):
        return Outcome(
            STATUS_FAILED,
            f"A26 顶层输入与 Markdown 产物未一一对应：预期 {expected}，实得 {produced}",
            details,
        )
    # 反假 PASS（S10-02）：健康输入逐个必须有对应产物（此前「子集 + 至少一份」
    # 允许任一家族全失败仍 PASS）；损坏/预期失败输入按 T-25 不得留半成品且须有
    # 失败诊断；命名规则外的产物仍视为规划缺陷。
    extra_outputs = sorted(set(produced) - set(expected))
    if extra_outputs:
        return Outcome(STATUS_FAILED, f"产物超出输入对应集合：{extra_outputs[:8]}", details)
    input_problems = _per_input_problems(item.item_id, prepared.files, produced, run.texts)
    if input_problems:
        return Outcome(STATUS_FAILED, "；".join(input_problems[:6]), details)
    if not produced:
        return Outcome(STATUS_FAILED, "预期至少一份 Markdown 产物，实得 0 项（转换全部失败或未产出）", details)
    return Outcome(STATUS_OK, details=details)


_CONTENT_ASSERTS: dict[str, ContentSpec] = {
    # A01 夹具 slide 显示顺序为先 rId11（BETA）后 rId10（ALPHA）且与 rels 列举
    # 顺序交错：ordered_tokens 锁定显示顺序，邻近断言锁定图文归属（互换可检出）。
    "A01": ContentSpec(ordered_tokens=("BETA", "ALPHA")),
    # A03 同一 media 被两个 shape 引用：T-14 要求保留每处位置，正文恰出现两次。
    "A03": ContentSpec(require_counts=(("SHARED-MEDIA", 2),)),
    # 这六项使用逐输入源真值，不能用整批拼接正文替另一文件抵账。
    "A04": ContentSpec(),
    "A05": ContentSpec(),
    "A06": ContentSpec(),
    "A21": ContentSpec(),
    "A22": ContentSpec(),
    "A23": ContentSpec(),
    # SVG 栅格化后可按 PNG 落盘；源扩展名字面量不是 T-14 的产物要求。
    # 两个来源的 OCR 正文都须保留；独立可解码媒体由 verify_svg_raster_media 核对。
    "A07": ContentSpec(require=("SVG-LOCAL", "PNG-NEIGH")),
    # A08 字段值（幻灯片编号一位字符）必须存活在两段 run 之间，不能只查首尾。
    "A08": ContentSpec(
        require=("RUN-AND", "FIELD-END"),
        require_patterns=(r"RUN-AND-FIELD\s{0,3}7\s{0,3}-FIELD-END",),
    ),
    # A09 的 descr 允许出现在图片 alt 位置（本就是 description 的标准去处），
    # 围栏内与围栏外正文都不得冒充 OCR 正文。
    "A09": ContentSpec(
        forbid_in_text_fences=("DESCR-NO-OCR-TOKEN",),
        forbid_outside_alt=("DESCR-NO-OCR-TOKEN",),
    ),
    # A10 截断图按 T-16 必须留下「部分提取 + 文件名 + 原因」的界面诊断。
    "A10": ContentSpec(require=("HEALTHY-TEXT",), require_in_texts=("部分提取", "pptx_undecodable_image")),
    # 专用语义判据还逐文件核对图像身份、来源路径和分阶段诊断。
    "A11": ContentSpec(),
    "A12": ContentSpec(),
    "A13": ContentSpec(),
    "A19": ContentSpec(),
    # A14 两个 anchor 的显示顺序（rId2/rId1）与 .rels 列举顺序相反，归属不得互换。
    "A14": ContentSpec(ordered_tokens=("XLSX-FIRST-DRAW", "XLSX-FIRST-REL")),
    # A15（S10-08）：>500 页夹具必须经 processing_warnings 披露 auto_mode 降级
    # （T-18 修订：阈值默认 500，JchTools 只转达引擎警告并按文件呈现）。
    "A15": ContentSpec(require_in_texts=("auto_mode", "large_501_pages")),
}


_TEXT_FENCE = re.compile(r"```[^\n]*\n(.*?)```", re.DOTALL)


def _token_image_order_problems(text: str, ordered_tokens: tuple[str, ...]) -> list[str]:
    """图文归属断言（A01/A14）：token 按显示顺序出现，且各自邻近不同的图片引用.

    换图/错配可检出：token 顺序颠倒、或多个 token 邻近同一引用、或邻近引用
    顺序与显示顺序不一致，都判为归属错误（T-15 对应关系稳定）。
    """
    problems: list[str] = []
    positions: list[int] = []
    for token in ordered_tokens:
        index = text.find(token)
        if index < 0:
            return [f"正文缺少期望 token：{token}"]
        positions.append(index)
    if positions != sorted(positions):
        return [f"token 顺序与显示顺序不一致（可能互换）：{list(zip(ordered_tokens, positions, strict=True))}"]
    references = [match.start() for match in MARKDOWN_IMAGE_LINK_PATTERN.finditer(text)]
    if len(references) < len(ordered_tokens):
        return [f"图片引用数（{len(references)}）少于 token 数（{len(ordered_tokens)}），无法核对图文归属"]
    nearest: list[int] = []
    for token, position in zip(ordered_tokens, positions, strict=True):
        before = [start for start in references if start < position]
        if not before:
            return [f"token {token} 之前没有任何图片引用，图文归属无法核对"]
        nearest.append(max(before))
    if len(set(nearest)) != len(nearest):
        problems.append(f"多个 token 邻近同一图片引用，图文归属不可区分（可能互换）：{ordered_tokens}")
    elif nearest != sorted(nearest):
        problems.append(f"token 与各自邻近图片引用的顺序不一致（可能互换）：{ordered_tokens}")
    return problems


def _content_problems(text: str, gui_texts: str, spec: ContentSpec) -> list[str]:
    """按 ContentSpec 汇总正文与 GUI 任务文本的内容问题（纯函数，供单测锁定）."""
    problems: list[str] = []
    fences = cast("list[str]", _TEXT_FENCE.findall(text))
    fence_text = "\n".join(fences)
    missing = [token for token in spec.require if token not in text]
    if missing:
        problems.append(f"正文缺少期望 token：{missing}")
    leaked = [token for token in spec.forbid if token in text]
    if leaked:
        problems.append(f"正文出现不应出现的 token：{leaked}")
    leaked_fences = [token for token in spec.forbid_in_text_fences if token in fence_text]
    if leaked_fences:
        problems.append(f"text 围栏出现不应出现的 token：{leaked_fences}")
    wrong_counts = [
        f"{token} 出现 {text.count(token)} 次（预期 {expected} 次）"
        for token, expected in spec.require_counts
        if text.count(token) != expected
    ]
    if wrong_counts:
        problems.append(f"token 出现次数不符合预期：{wrong_counts}")
    missing_patterns = [pattern for pattern in spec.require_patterns if not re.search(pattern, text)]
    if missing_patterns:
        problems.append(f"正文未命中期望模式（如字段值丢失）：{missing_patterns}")
    if spec.ordered_tokens:
        problems.extend(_token_image_order_problems(text, spec.ordered_tokens))
    # 图片引用整段剔除后，剩余正文不得再出现 descr（alt 是唯一合法去处）。
    outside_alt = MARKDOWN_IMAGE_LINK_PATTERN.sub("", text)
    leaked_alt = [token for token in spec.forbid_outside_alt if token in outside_alt]
    if leaked_alt:
        problems.append(f"token 出现在 alt 之外冒充正文：{leaked_alt}")
    missing_texts = [token for token in spec.require_in_texts if token not in gui_texts]
    if missing_texts:
        problems.append(f"GUI 任务文本缺少期望诊断/披露：{missing_texts}")
    return problems


_IMAGE_INK_THRESHOLD = 245


_IMAGE_MAX_INK_MEAN_ERROR = 12


_IMAGE_INK_LOOKUP = [255 if value < _IMAGE_INK_THRESHOLD else 0 for value in range(256)]


# Pillow 桩漏标 point 的 lut；只收口本判据实际使用的 8 位查找表调用，不改变映射。
_image_point = cast("Callable[[Image.Image, list[int]], Image.Image]", vars(Image.Image)["point"])


def _same_image_pixels(left: Image.Image, right: Image.Image) -> bool:
    """同采样画布的 RGB 源图比较；沿用文字区域误差，不以白边稀释差异."""
    if left.size != right.size:
        return False
    ink = _image_point(ImageChops.darker(left.convert("L"), right.convert("L")), _IMAGE_INK_LOOKUP)
    difference = ImageChops.difference(left, right)
    if ink.getbbox() is None:
        return difference.getbbox() is None
    return max(ImageStat.Stat(difference, ink).mean) <= _IMAGE_MAX_INK_MEAN_ERROR


def _same_image_content(source: bytes, actual: Path) -> bool:
    """独立源像素 oracle：允许编码变化，白底不稀释文字区域的错配误差."""
    if actual.read_bytes() == source:
        return True
    with Image.open(io.BytesIO(source)) as expected, Image.open(actual) as produced:
        return _same_image_pixels(expected.convert("RGB"), produced.convert("RGB"))


def _image_binding_problems(markdown: Path, bindings: tuple[tuple[str, bytes], ...]) -> list[str]:
    """OCR token 与本地源图绑定；单图允许前后布局，多图仍按相邻前置引用核对."""
    text = markdown.read_text(encoding="utf-8")
    rendered = _mask_markdown_fences(text)
    rendered = MARKDOWN_INLINE_CODE_PATTERN.sub(lambda match: " " * len(match.group(0)), rendered)
    references = list(MARKDOWN_IMAGE_LINK_PATTERN.finditer(rendered))
    outside_alt = MARKDOWN_IMAGE_LINK_PATTERN.sub(lambda match: " " * len(match.group(0)), text)
    problems: list[str] = []
    for token, source in bindings:
        positions = [match.start() for match in re.finditer(re.escape(token), outside_alt)]
        if len(positions) != 1:
            problems.append(f"{markdown.name} OCR token {token} 应在正文出现一次，实得 {len(positions)} 次")
            continue
        before = [reference for reference in references if reference.end() <= positions[0]]
        if before:
            reference = before[-1]
        elif len(references) == 1 and len(bindings) == 1:
            # PDF 等单图产物可先输出 OCR 正文；单图仍须通过独立源像素校验。
            reference = references[0]
        else:
            problems.append(f"{markdown.name} OCR token {token} 缺少对应图片引用")
            continue
        raw = reference.group("target").strip()
        raw = raw[1:-1] if raw.startswith("<") and raw.endswith(">") else raw.split(maxsplit=1)[0]
        local_reference = unquote(raw)
        if ntpath.isabs(local_reference.replace("/", "\\")):
            problems.append(f"{markdown.name} OCR token {token} 对应的本地媒体不可达：{raw}")
            continue
        actual = (markdown.parent / local_reference.replace("\\", "/")).resolve()
        if not actual.is_relative_to(markdown.parent.resolve()) or not actual.is_file():
            problems.append(f"{markdown.name} OCR token {token} 对应的本地媒体不可达：{raw}")
            continue
        try:
            matched = _same_image_content(source, actual)
        except (OSError, ValueError) as error:
            problems.append(f"{markdown.name} OCR token {token} 图像无法解码：{error}")
            continue
        if not matched:
            problems.append(f"{markdown.name} OCR token {token} 对应落盘图片内容错配")
    return problems


def _partial_stage_problems(
    gui_texts: str, filename: str, member: str, stages: str, image_index: int | None = None
) -> list[str]:
    """同宿主、同成员的明确失败语义；阶段可在成员前，不跨诊断成员抵账."""
    # 只读 oracle image_ocr.rs 使用 Image {idx}；A13 无页眉/页脚，三个正文 blip
    # 的确定源顺序为 EMF/WMF/PNG，故 0/1 是相应失败成员，不是任意 OCR 失败。
    member_pattern = rf"(?<![\w.-]){re.escape(member)}(?![\w.-])"
    if image_index is not None:
        member_pattern += rf"|(?i:\bImage {image_index}\b)"
    # 先剔除宿主（其文件名可能含 raster），再按成员记录划界；不能将 Image 1
    # 的失败归给 Image 0，也不能借另一分号诊断中的阶段标签抵账。
    host_pattern = rf"(?<![\w.-]){re.escape(filename)}(?![\w.-])"
    clauses = [
        clause
        for row in gui_texts.splitlines()
        if re.search(host_pattern, row) is not None and "部分" in row
        for clause in re.split(r"[；;]|(?=\bImage \d+\b)", row.split(filename, 1)[1])
        if re.search(member_pattern, clause) is not None
    ]
    if not any(re.search(stages, clause, re.IGNORECASE) for clause in clauses):
        return [f"{filename} 缺少 {member} 的部分提取及明确失败阶段诊断"]
    return []


_DOCUMENT_IMAGE_BINDINGS = {
    "A01": ("pptx_two_png_order.pptx", "ppt/media/", (("BETA", "image2.png"), ("ALPHA", "image1.png"))),
    "A14": (
        "xlsx_drawing_order.xlsx",
        "xl/media/",
        (("XLSX-FIRST-DRAW", "image2.png"), ("XLSX-FIRST-REL", "image1.png")),
    ),
    "A11": ("pptx_ole_broken_preview.pptx", "ppt/media/", (("OLE-PREVIEW-OK", "preview_ok.png"),)),
    "A12": (
        "docx_all_sources.docx",
        "word/media/",
        tuple(
            (f"{name}-IMAGE-TOKEN", f"{name.lower()}.png")
            for name in ("BODY", "HEADER", "FOOTER", "FOOTNOTE", "ENDNOTE")
        ),
    ),
    "A13": ("docx_emf_wmf_raster.docx", "word/media/", (("RASTER-HEALTHY", "healthy.png"),)),
}


def _wmf_content_preserved(wmf: bytes, references: set[Path]) -> bool:
    """原字节保留或按原始 WMF 逻辑坐标离屏重放后保真；不拟合产物像素."""
    if any(path.read_bytes() == wmf for path in references):
        return True
    for path in references:
        try:
            # Pillow 将 WMF 转为 EMF 后播放，未应用原始逻辑 bbox；其默认
            # DPI/映射不能作为引擎 raster 的独立源真值。复用夹具的 classic GDI oracle。
            with Image.open(path) as produced, render_wmf_gdi(wmf, produced.size) as expected:
                canvas = Image.new("RGBA", produced.size, "white")
                canvas.alpha_composite(produced.convert("RGBA"))
                matched = _same_image_pixels(expected.convert("RGB"), canvas.convert("RGB"))
        except (OSError, ValueError):
            continue
        if matched:
            return True
    return False


def _mixed_docx_problems(markdown: Path, output: Path, gui_texts: str, wmf: bytes | None, problems: list[str]) -> None:
    filename = "docx_emf_wmf_raster.docx"
    if "MIXED-MEDIA" not in markdown.read_text(encoding="utf-8"):
        problems.append("A13 单图失败丢失宿主原生正文")
    problems.extend(
        _partial_stage_problems(
            gui_texts,
            filename,
            "broken.emf",
            (
                r"decode.*(?:fail|error)|raster.*(?:fail|error)|(?:fail|error).*decod|"
                r"(?:fail|error).*raster|could not determine image format|解码失败|光栅化失败"
            ),
            image_index=0,
        )
    )
    _, references = _collect_media_references(markdown, output)
    if wmf is not None and not _wmf_content_preserved(wmf, references):
        problems.extend(
            _partial_stage_problems(
                gui_texts,
                filename,
                "healthy.wmf",
                (
                    r"decode.*(?:fail|error)|raster.*(?:fail|error)|ocr.*(?:fail|error)|"
                    r"(?:fail|error).*(?:decod|raster|ocr)|解码失败|光栅化失败|OCR 执行失败"
                ),
                image_index=1,
            )
        )


def _document_image_problems(item_id: str, source_dir: Path, output: Path, gui_texts: str, problems: list[str]) -> None:
    filename, member_root, tokens = _DOCUMENT_IMAGE_BINDINGS[item_id]
    with zipfile.ZipFile(source_dir / filename) as archive:
        bindings = tuple((token, archive.read(member_root + member)) for token, member in tokens)
        wmf = archive.read("word/media/healthy.wmf") if item_id == "A13" else None
    markdown = (
        next(iter(output.rglob("*.md")), None)
        if item_id in ("A01", "A14")
        else next(iter(output.rglob(_expected_markdown_name(filename))), None)
    )
    if markdown is None:
        problems.append(f"{item_id} 缺少语义断言所需的 {filename} 产物")
        return
    problems.extend(_image_binding_problems(markdown, bindings))
    if item_id == "A11":
        problems.extend(
            _partial_stage_problems(
                gui_texts,
                filename,
                "ole_body_broken.xlsx",
                (
                    r"embedded.*(?:fail|error)|(?:fail|error).*embedded|"
                    r"OLE.*(?:失败|错误)|嵌入.*(?:失败|错误)|解析失败"
                ),
            )
        )
    elif item_id == "A13":
        _mixed_docx_problems(markdown, output, gui_texts, wmf, problems)


def _jbig2_semantic_problems(output: Path, problems: list[str]) -> None:
    # 真值来自固定生成规范的独立二值文字位图（不是 Xberg 输出）。
    expected = Image.new("1", (320, 96), 1)
    draw = ImageDraw.Draw(expected)
    # Pillow text 桩的尾部参数未知；此精确签名只描述实际使用的固定参数子集。
    draw_text = cast(
        "Callable[[ImageDraw.ImageDraw, tuple[float, float], str, int, ImageFont.ImageFont], None]",
        vars(ImageDraw.ImageDraw)["text"],
    )
    draw_text(draw, (12, 24), "JBIG2 SYNTH CHECK", 0, ImageFont.load_default(size=24))
    buffer = io.BytesIO()
    expected.save(buffer, "PNG")
    for filename in ("jbig2_standalone.jb2", "jbig2_standalone.jbig2", "pdf_jbig2.pdf"):
        markdown = next(iter(output.rglob(_expected_markdown_name(filename))), None)
        if markdown is None:
            problems.append(f"A19 缺少 {filename} 独立转换产物")
        else:
            problems.extend(_image_binding_problems(markdown, (("JBIG2 SYNTH CHECK", buffer.getvalue()),)))


_SOURCE_CONTENT_ITEMS = frozenset(("A04", "A05", "A06", "A21", "A22", "A23"))


_MEMBER_FAILURE_STAGES = (
    r"decode.*(?:fail|error)|raster.*(?:fail|error)|ocr.*(?:fail|error)|"
    r"(?:fail|error).*(?:decod|raster|ocr|extract|pars)|"
    r"解码失败|光栅化失败|OCR 执行失败|OCR 无文字|未识别到文字|解析失败|提取失败"
)


_MEDIA_FAILURE_STAGES = (
    r"(?:decod|raster|encod).*(?:fail|error)|(?:fail|error).*(?:decod|raster|encod)|"
    r"could not determine image format|no decoder|unknown.*format|"
    r"资源缺失|格式未知|解码失败|光栅化失败|编码失败|结果无法关联"
)


_EMBEDDED_FAILURE_STAGES = (
    r"(?:embedded|extract|pars).*(?:fail|error)|(?:fail|error).*(?:embedded|extract|pars)|"
    r"嵌入.*(?:失败|错误)|解析失败|提取失败"
)


_VECTOR_SOURCE_SPECS = {
    # 来自夹具 WMF ExtTextOut / EMF ExtTextOutW 原始文字记录，不取文件名或产物。
    "A04": ContentSpec(require_patterns=(r"dft[\s_-]*se", r"dft[\s_-]*clk")),
    "A05": ContentSpec(require_patterns=(r"dft[\s_-]*se", r"dft[\s_-]*clk")),
    "A06": ContentSpec(require=("Redundant Memory", "MBIST", "EFUSE")),
}


_SOURCE_TEXT_PART = re.compile(
    r"""
    (?:word/(?:document|header\d+|footer\d+|footnotes|endnotes)
    |ppt/slides/slide\d+
    |xl/(?:sharedStrings|worksheets/sheet\d+)
    |(?:word|ppt)/charts/chart\d+)\.xml
    """,
    re.VERBOSE,
)


_WMF_HEADER_BYTES = 18


_WMF_RECORD_HEADER_BYTES = 6


_WMF_RECORD_MIN_WORDS = 3


_WMF_WINDOW_ORIGIN = 0x020B


_WMF_WINDOW_EXTENT = 0x020C


def _source_content_spec(archive: zipfile.ZipFile) -> ContentSpec:
    """只从源容器可见正文/表格/图表取真值，排除属性、关系、文件名和样式。."""
    tokens: set[str] = set()
    for member in archive.namelist():
        if _SOURCE_TEXT_PART.fullmatch(member) is None:
            continue
        tree = ElementTree.fromstring(archive.read(member))
        for node in tree.iter():
            if node.tag.rsplit("}", 1)[-1] not in ("t", "v") or not node.text:
                continue
            token = node.text.strip()
            if token and (node.tag.rsplit("}", 1)[-1] == "t" or any(character.isalpha() for character in token)):
                tokens.add(token)
    # 现有三种 Office 合并表格夹具具有相同源行，值存在还不够，行内关联也不能丢。
    table = {"Fuse", "Circuit", "40A", "Blower", "50A", "Cooling"}
    table_patterns = (r"101[\s|]*40A[\s|]*Blower", r"102[\s|]*50A[\s|]*Cooling") if table.issubset(tokens) else ()
    patterns = tuple(r"\s+".join(re.escape(part) for part in token.split()) for token in sorted(tokens))
    return ContentSpec(require_patterns=(*patterns, *table_patterns))


def _source_wmf_with_bounds(source: bytes) -> bytes:
    """仅取源 WindowOrg/WindowExt 边界，为标准 WMF 补齐 GDI oracle 所需 APM 头。."""
    if source[:4] == b"\xd7\xcd\xc6\x9a":
        return source
    position = _WMF_HEADER_BYTES
    x = y = width = height = 0
    while position + _WMF_RECORD_HEADER_BYTES <= len(source):
        words, operation = struct.unpack_from("<IH", source, position)
        if words < _WMF_RECORD_MIN_WORDS or position + words * 2 > len(source):
            message = "源 WMF 记录长度非法"
            raise ValueError(message)
        if operation == _WMF_WINDOW_ORIGIN:
            y, x = struct.unpack_from("<hh", source, position + _WMF_RECORD_HEADER_BYTES)
        elif operation == _WMF_WINDOW_EXTENT:
            height, width = struct.unpack_from("<hh", source, position + _WMF_RECORD_HEADER_BYTES)
        position += words * 2
    if width <= 0 or height <= 0:
        message = "源 standard WMF 缺少逻辑边界"
        raise ValueError(message)
    header = struct.pack("<IHhhhhHI", 0x9AC6CDD7, 0, x, y, x + width, y + height, 1440, 0)
    checksum = 0
    for word in cast("tuple[int, ...]", struct.unpack("<10H", header)):
        checksum ^= word
    return header + struct.pack("<H", checksum) + source


def _source_media_preserved(member: str, source: bytes, references: set[Path]) -> bool:
    """逐源成员核对已引用媒体；缺图不能靠其他成员的文字或无关媒体抵账。."""
    if any(path.read_bytes() == source for path in references):
        return True
    if "wmf" in member.lower():
        try:
            return _wmf_content_preserved(_source_wmf_with_bounds(source), references)
        except (OSError, ValueError):
            return False
    for actual in references:
        try:
            if _same_image_content(source, actual):
                return True
            if member.lower().endswith(".emf"):
                with Image.open(io.BytesIO(source)) as expected, Image.open(actual) as produced:
                    if _same_image_pixels(
                        expected.convert("RGB").resize(produced.size, Image.Resampling.LANCZOS),
                        produced.convert("RGB"),
                    ):
                        return True
        except (OSError, ValueError):
            continue
    return False


def _source_input_spec(source: Path, archive: zipfile.ZipFile, item_id: str) -> ContentSpec:
    if item_id in _VECTOR_SOURCE_SPECS:
        if not any("/media/" in member for member in archive.namelist()):
            message = f"{source.name} 源容器缺少向量图片，无法核对 OCR 真值"
            raise ValueError(message)
        return _VECTOR_SOURCE_SPECS[item_id]
    if source.suffix.lower() == ".xlsb":
        # 已有公开 BIFF12 sharedStrings.bin 的固定真值，独立于输出和二进制路径。
        return ContentSpec(
            require=("Welcome to File Extension FYI Center", "Active Server Pages", "Portable Network Graphics")
        )
    return _source_content_spec(archive)


def _source_embedded_problems(
    source: Path, archive: zipfile.ZipFile, member: str, body: str, gui_texts: str
) -> list[str]:
    """核对嵌入正文真值，并保留同宿主、同成员和对应阶段的失败诊断。."""
    problems: list[str] = []
    with zipfile.ZipFile(io.BytesIO(archive.read(member))) as embedded:
        embedded_content = _content_problems(body, gui_texts, _source_content_spec(embedded))
    if embedded_content:
        diagnostics = _partial_stage_problems(gui_texts, source.name, Path(member).name, _EMBEDDED_FAILURE_STAGES)
        if diagnostics:
            problems.extend(f"{source.name}/{member}：{problem}" for problem in embedded_content)
            problems.extend(diagnostics)
    return problems


def _source_member_problems(source: Path, markdown: Path, gui_texts: str, item_id: str) -> list[str]:
    """每个顶层输入单独核对正文、嵌入正文、媒体和真实部分提取诊断。."""
    text = markdown.read_text(encoding="utf-8")
    # 图片 alt/name/description 不能冒充 OCR 或源正文。
    body = MARKDOWN_IMAGE_LINK_PATTERN.sub("", text)
    _, references = _collect_media_references(markdown, markdown.parent)
    problems: list[str] = []
    with zipfile.ZipFile(source) as archive:
        spec = _source_input_spec(source, archive, item_id)
        content = _content_problems(body, gui_texts, spec)
        if not spec.require and not spec.require_patterns:
            problems.append(f"{source.name} 源正文真值为空，不能验证内容")
        if item_id not in _VECTOR_SOURCE_SPECS:
            problems.extend(f"{source.name}：{problem}" for problem in content)
        for member in archive.namelist():
            if "/media/" in member:
                preserved = _source_media_preserved(member, archive.read(member), references)
                if not preserved or (item_id in _VECTOR_SOURCE_SPECS and content):
                    diagnostics = _partial_stage_problems(
                        gui_texts,
                        source.name,
                        Path(member).name,
                        _MEMBER_FAILURE_STAGES if preserved else _MEDIA_FAILURE_STAGES,
                        image_index=0 if item_id in _VECTOR_SOURCE_SPECS else None,
                    )
                    if diagnostics:
                        problems.extend(f"{source.name}：{problem}" for problem in content)
                        problems.extend(diagnostics)
                        if not preserved:
                            problems.append(f"{source.name} 缺少源媒体本体/对应关系：{member}")
            elif "/embeddings/" in member and Path(member).suffix.lower() in (".docx", ".pptx", ".xlsx"):
                problems.extend(_source_embedded_problems(source, archive, member, body, gui_texts))
    return problems


def _per_source_content_problems(item_id: str, source_dir: Path, output: Path, gui_texts: str) -> list[str]:
    problems: list[str] = []
    for source in sorted(path for path in source_dir.iterdir() if path.is_file()):
        markdown = output / _expected_markdown_name(source.name)
        if not markdown.is_file():
            problems.append(f"{source.name} 缺少逐输入内容核对产物")
            continue
        problems.extend(_source_member_problems(source, markdown, gui_texts, item_id))
    return problems


def _document_semantic_problems(item_id: str, source_dir: Path, output: Path, gui_texts: str) -> list[str]:
    """附录 A 的独立源图/固定文字真值，不把被测引擎自己的输出当 oracle."""
    problems: list[str] = []
    try:
        if item_id in _DOCUMENT_IMAGE_BINDINGS:
            _document_image_problems(item_id, source_dir, output, gui_texts, problems)
        elif item_id == "A19":
            _jbig2_semantic_problems(output, problems)
        elif item_id in _SOURCE_CONTENT_ITEMS:
            problems.extend(_per_source_content_problems(item_id, source_dir, output, gui_texts))
    except (OSError, ValueError, KeyError, zipfile.BadZipFile) as error:
        problems.append(f"{item_id} 源图/产物语义核对失败：{error}")
    return problems
