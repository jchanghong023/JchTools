"""转 Markdown 验收媒体与格式能力核验（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import re
from typing import TYPE_CHECKING, cast

from PIL import Image

from scripts.markdown_acceptance_impl.model import _file_digest
from scripts.markdown_acceptance_impl.synth import A02_IMAGE_COUNT, A02_IMAGE_TOKENS, A07_IMAGE_COUNT

if TYPE_CHECKING:
    from pathlib import Path

__all__ = [
    "A18_INPUT_EXTENSIONS",
    "_MEDIA_CLOCK_RADIX",
    "_MEDIA_SECTION_COUNT",
    "_media_audio_problem",
    "_media_markdown_problem",
    "_media_statistics",
    "_media_timestamp_ms",
    "_media_transcript_problem",
    "_scene_marker_problem",
    "_verify_a02_content",
    "_verify_a18_unsupported_rejected",
    "_verify_a24_scenes",
    "_verify_a24_stop",
    "_verify_a24_tone",
    "_verify_a24_video",
    "verify_svg_raster_media",
]


# A18 的合成夹具全集；支持/未支持分区只能由本轮共享引擎的实际 formats 决定。
A18_INPUT_EXTENSIONS = ("jp2", "j2k", "j2c", "jpx", "jpm", "mj2")


def verify_svg_raster_media(output: Path) -> str | None:
    """A07/T-14：SVG 和邻位 PNG 都须有独立、可解码的媒体资源."""
    media = [path for path in output.rglob("*") if path.is_file() and path.parent.name.endswith("_media")]
    if len(media) != A07_IMAGE_COUNT:
        return f"A07 应保留 SVG 栅格化结果与邻位 PNG 共 {A07_IMAGE_COUNT} 张图片，实得 {len(media)} 张"
    if len({_file_digest(path) for path in media}) != A07_IMAGE_COUNT:
        return "A07 两个不同源图片不得被同一份媒体字节替代"
    try:
        for path in media:
            with Image.open(path) as picture:
                picture.verify()
    except (OSError, ValueError) as error:
        return f"A07 落盘图片无法解码：{error}"
    return None


def _verify_a02_content(output: Path) -> str | None:
    """核对 A02 恰有合成图片及其正文 token，拒绝空正文或额外媒体."""
    media_files = [path for path in output.rglob("*") if path.is_file() and path.parent.name.endswith("_media")]
    if len(media_files) != A02_IMAGE_COUNT:
        return f"A02 应保留 {A02_IMAGE_COUNT} 张合成图片，实际媒体文件 {len(media_files)} 项"
    text = "\n".join(path.read_text(encoding="utf-8", errors="replace") for path in sorted(output.rglob("*.md")))
    if not text.strip():
        return "A02 转换产物正文为空"
    missing = [token for token in A02_IMAGE_TOKENS if token not in text]
    if missing:
        return f"A02 正文缺少合成图片内容：{missing}"
    return None


def _verify_a24_video(outputs: Path, suffixes: tuple[str, ...] = ("mp4", "m4a")) -> str | None:
    """真实中文视频产物结构断言（标题/时长/转录/时间戳）与损坏音轨失败隔离."""
    if (outputs / "damaged_mp4.md").exists():
        return "损坏音轨产出了结果文件（应为失败，不留半成品）"
    for suffix in suffixes:
        good = outputs / f"video-to-notes-intro-zh_{suffix}.md"
        if not good.is_file():
            return f"真实中文 {suffix.upper()} 未产出转录结果"
        text = good.read_text(encoding="utf-8", errors="replace")
        missing = [marker for marker in ("# video-to-notes-intro-zh", "- 音频时长: ", "## 转录") if marker not in text]
        if missing:
            return f"媒体结果缺少结构标记：{missing}"
        problem = _media_markdown_problem(text, require_chinese=True)
        if problem is not None:
            return f"真实中文 {suffix.upper()} 转录结果不完整：{problem}"
    return None


_MEDIA_CLOCK_RADIX = 60


_MEDIA_SECTION_COUNT = 2


def _media_timestamp_ms(value: str) -> int | None:
    match = re.fullmatch(r"([0-9]{2,}):([0-9]{2}):([0-9]{2})\.([0-9]{3})", value)
    if match is None:
        return None
    hours, minutes, seconds, millis = (int(part) for part in match.groups())
    if minutes >= _MEDIA_CLOCK_RADIX or seconds >= _MEDIA_CLOCK_RADIX:
        return None
    return hours * 3600000 + minutes * 60000 + seconds * 1000 + millis


def _media_statistics(metadata: str) -> tuple[str, str] | str:
    durations = [line.removeprefix("- 音频时长: ") for line in metadata.splitlines() if line.startswith("- 音频时长: ")]
    counts = [line.removeprefix("- 语音片段: ") for line in metadata.splitlines() if line.startswith("- 语音片段: ")]
    if len(durations) != 1 or len(counts) != 1:
        return "音频时长或语音片段统计行缺失/重复"
    return durations[0], counts[0]


def _media_transcript_problem(body: str, duration: int, count: int, *, require_chinese: bool) -> str | None:
    """逐段核对正文、时间范围、输出顺序和统计，保留首个失败原因."""
    last_start = 0
    transcripts: list[str] = []
    for line in body.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"\[([^]\n]+) --> ([^]\n]+)\] (.+)", line)
        if match is None:
            return "转录片段缺少合法起止时间戳或正文"
        start, end = _media_timestamp_ms(match[1]), _media_timestamp_ms(match[2])
        if start is None or end is None or start >= end or end > duration or start < last_start:
            return "片段时间戳、音频范围或输出顺序非法"
        if not match[3].strip():
            return "转录片段正文为空"
        last_start = start
        transcripts.append(match[3])
    if len(transcripts) != count:
        return "片段统计与实际正文片段数不符"
    if require_chinese and not any(re.search(r"[\u4e00-\u9fff]", transcript) for transcript in transcripts):
        return "转录片段正文没有中文（标题及元数据不能冒充转录）"
    return None


def _media_audio_problem(
    duration_text: str, count_text: str, body: str, *, require_chinese: bool, empty_scene: str | None
) -> str | None:
    duration = _media_timestamp_ms(duration_text)
    if duration is None or duration <= 0:
        return "含音轨结果必须有合法的正时长"
    if not re.fullmatch(r"[0-9]+", count_text):
        return "语音片段数不是非负整数"
    count = int(count_text)
    if empty_scene == "未检测到语音":
        return None if count == 0 and body.strip() == "（未检测到语音）" else "无语音结果缺少零片段及独立正文说明"
    if count == 0:
        return "含语音结果没有转录片段"
    return _media_transcript_problem(body, duration, count, require_chinese=require_chinese)


def _media_markdown_problem(text: str, *, require_chinese: bool = False, empty_scene: str | None = None) -> str | None:
    """T-20 消费者判据：唯一统计行、真实正文和每个片段时间范围，不只匹配标签."""
    sections = text.split("\n## 转录\n")
    if len(sections) != _MEDIA_SECTION_COUNT:
        return "缺少或重复独立转录标题行"
    metadata, body = sections
    statistics = _media_statistics(metadata)
    if isinstance(statistics, str):
        return statistics
    duration_text, count_text = statistics
    if empty_scene == "无音频轨道":
        if duration_text == empty_scene and count_text == "0" and body.strip() == "（无音频轨道）":
            return None
        return "无音轨结果必须为无音轨时长、零片段及独立正文说明"
    return _media_audio_problem(
        duration_text, count_text, body, require_chinese=require_chinese, empty_scene=empty_scene
    )


def _scene_marker_problem(outputs: Path, result_name: str, markers: tuple[str, ...], scene: str) -> str | None:
    """单场景说明文本断言：结果文件在场且包含 T-20 要求的说明标记。."""
    result = outputs / result_name
    if not result.is_file():
        return f"{scene}未产出结果文件（T-20 不能产生无解释的空文件）"
    missing = [marker for marker in markers if marker not in result.read_text(encoding="utf-8", errors="replace")]
    if missing:
        return f"{scene}缺少说明标记（T-20）：{missing}"
    return None


def _verify_a24_tone(outputs: Path) -> str | None:
    """正弦音场景：时长行必须在场；转录片段与「未检测到语音」说明取其一。."""
    problem = _scene_marker_problem(outputs, "tone_m4a.md", ("- 音频时长: ",), "正弦音场景")
    if problem is not None:
        return problem
    text = (outputs / "tone_m4a.md").read_text(encoding="utf-8", errors="replace")
    scene = "未检测到语音" if "（未检测到语音）" in text else None
    return _media_markdown_problem(text, empty_scene=scene)


def _verify_a24_scenes(outputs: Path) -> str | None:
    """S10-07：tone/silence/noaudio 三场景按 T-20 各自有明确说明文本，不能只查存在。."""
    for filename, scene in (("noaudio_mp4.md", "无音频轨道"), ("silence_m4a.md", "未检测到语音")):
        problem = _scene_marker_problem(outputs, filename, (scene,), scene)
        if problem is not None:
            return problem
        problem = _media_markdown_problem((outputs / filename).read_text(encoding="utf-8"), empty_scene=scene)
        if problem is not None:
            return f"{scene}场景结果不完整：{problem}"
    return _verify_a24_tone(outputs)


def _verify_a24_stop(produced: int, convertible: int, texts: str) -> str | None:
    """A24 补充检查停止状态及批次计数；T-23 当前文件取消与产物边界由 B01/S5 验证。."""
    if "已停止" not in texts:
        return "停止相未观察到「已停止」界面状态（T-23 须显示停止请求已生效）"
    if produced >= convertible:
        return (
            f"停止后产物 {produced} 份不少于可转换输入 {convertible} 份，"
            "停止批次未处理全部输入的计数边界未被验证；当前文件取消另须通过 B01/S5"
        )
    return None


def _verify_a18_unsupported_rejected(output_dir: Path, texts: str, supported: set[str]) -> str | None:
    """S10-06/T-08：按实际能力分区，未声明格式无产物且不进入批次统计。."""
    if not output_dir.is_dir():
        return f"混跑输出目录缺失：{output_dir}"
    unsupported_suffixes = tuple(f"_{name}.md" for name in A18_INPUT_EXTENSIONS if name not in supported)
    leftovers = sorted(path.name for path in output_dir.rglob("*.md") if path.name.endswith(unsupported_suffixes))
    if leftovers:
        return f"当前引擎未声明格式产出了结果文件（应明确跳过）：{leftovers}"
    summary = re.findall(r"成功 (\d+)，部分提取 (\d+)，失败 (\d+)", texts)
    if not summary:
        return "混跑后未观察到批次统计（成功/部分提取/失败），无法核对 unsupported 排除"
    counted = sum(int(field) for field in cast("list[str]", summary[-1]))
    if counted != len(supported):
        return (
            f"批次统计计入 {counted} 个文件，预期仅 {len(supported)} 个实际支持格式；"
            "未声明格式必须被明确跳过（T-07 未承诺类型不进入待转换列表）"
        )
    return None
