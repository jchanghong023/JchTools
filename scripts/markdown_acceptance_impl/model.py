"""转 Markdown 验收共享数据模型与纯值辅助（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import contextlib
import dataclasses
import hashlib
import io
import os
import sys
from pathlib import Path
from typing import TYPE_CHECKING

from scripts import isolation_probe

if TYPE_CHECKING:
    from collections.abc import Mapping
    from typing import TypeIs

__all__ = [
    "OLD_PROJECT_DIR",
    "STATUS_FAILED",
    "STATUS_NOT_RUN",
    "STATUS_OK",
    "STATUS_OPTIONAL",
    "AssetProbe",
    "ContentSpec",
    "GuiRun",
    "InputRule",
    "Item",
    "Outcome",
    "PreparedInputs",
    "SynthResult",
    "_file_digest",
    "_is_str_obj_list",
    "_is_str_obj_map",
    "_isolated_environment_error",
    "_old_cache_candidates",
    "_reconfigure_stdout",
    "_str_field",
]


def _isolated_environment_error(env: Mapping[str, str] | None = None) -> str | None:
    """拒绝将生产状态库或资产目录通过验收环境变量伪装成隔离根."""
    effective_env = os.environ if env is None else env
    for name in (
        "JCHTOOLS_TEST_ASSET_ROOT",
        "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT",
        "JCHTOOLS_TEST_STATE_DIR",
        "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT",
    ):
        value = effective_env.get(name)
        if not value:
            continue
        path = Path(value)
        if not path.is_absolute() or not isolation_probe._is_under_tmp(path):  # noqa: SLF001  # 固定共享隔离 API。
            return f"{name} 必须是仓库 .tmp/ 下的绝对隔离路径，拒绝读取或写入：{value}"
    return None


# 状态常量名避开 pass 字样（质量门 S105 把含该字样的变量名当疑似硬编码口令）。
STATUS_OK = "PASS"


STATUS_FAILED = "FAIL"


STATUS_NOT_RUN = "NOT RUN"


STATUS_OPTIONAL = "OPTIONAL"


# T-28 / 附录 A 的旧项目与旧缓存前置：旧目录仍在即未满足「删除旧项目后独立运行」。
OLD_PROJECT_DIR = Path(r"D:\code1111111111\all2markdown")


def _old_cache_candidates() -> list[Path]:
    local = os.environ.get("LOCALAPPDATA")
    candidates = [OLD_PROJECT_DIR / ".venv"]
    if local:
        candidates.append(Path(local) / "all2markdown")
    candidates.append(Path.home() / ".cache" / "all2markdown")
    return candidates


def _is_str_obj_map(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def _is_str_obj_list(value: object) -> TypeIs[list[object]]:
    return isinstance(value, list)


def _str_field(entry: object, key: str) -> str | None:
    if _is_str_obj_map(entry):
        got = entry.get(key)
        if isinstance(got, str):
            return got
    return None


def _reconfigure_stdout() -> None:
    # 阶段明细含中文；Windows 控制台默认代码页会把 print 变成 UnicodeEncodeError。
    with contextlib.suppress(AttributeError):
        stream = sys.stdout
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")


@dataclasses.dataclass
class Outcome:
    """单个条目的验收结论；reason 为 NOT RUN/FAIL 的必要说明."""

    status: str
    reason: str = ""
    details: list[str] = dataclasses.field(default_factory=list)


@dataclasses.dataclass
class AssetProbe:
    """只读探测本机转 Markdown 资产状态（不联网、不修改任何文件）."""

    root: Path | None
    runtime_dir: Path | None
    xberg_exe: Path | None
    inference_dir: Path | None
    missing: list[str]

    base_missing: list[str] = dataclasses.field(default_factory=list)
    media_missing: list[str] = dataclasses.field(default_factory=list)

    def xberg_ready(self) -> bool:
        return self.xberg_exe is not None and not self.base_missing

    def media_ready(self) -> bool:
        return self.xberg_ready() and self.inference_dir is not None and not self.media_missing

    def acquire_hint(self) -> str:
        return (
            "默认安装版/便携版的 xberg/ 已内置文档与截图资产，无需初始化或下载。"
            "可用 --seed-state / --seed-xberg 在隔离 SQLite 中承接有效旧运行目录；"
            "设置页「检查已安装组件」仅离线检查。音视频需用户主动点击"
            "「下载 / 补齐音频转写资产」，按内置收据固定发布安装 FFmpeg、SenseVoice/VAD；"
            "转换过程不联网补齐。媒体验收必须有实际资产和真实转录结果。"
        )


def _file_digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


# ---------------------------------------------------------------- 合成夹具构造器。


@dataclasses.dataclass
class SynthResult:
    """一次合成产物：输入文件名列表与失败原因（失败即整项 NOT RUN，不降级）."""

    files: list[str]
    error: str = ""


# ---------------------------------------------------------------- 条目登记表。


@dataclasses.dataclass
class Item:
    """一条验收条目：附录 A 矩阵 / GUI E2E / 双形态与环境前置."""

    item_id: str
    group: str
    title: str
    entry: str
    fixtures: tuple[str, ...] = ()
    fixture_note: str = ""
    synth: str = ""
    needs_assets: str = ""  # ""（无需）/ "xberg" / "xberg+media"
    stage: str = ""  # B 组引用的 gui_smoke 阶段号


# ---------------------------------------------------------------- GUI 驱动（A 组 / C 组共用）。


@dataclasses.dataclass
class GuiRun:
    """一次 GUI 转换的可观察结果；error 非空表示驱动链路本身失败."""

    outputs: list[Path]
    texts: str
    error: str | None


# ---------------------------------------------------------------- 逐输入断言（S10-02/04）。


@dataclasses.dataclass(frozen=True)
class InputRule:
    """逐输入验收期望：健康必须有产物，预期失败必须有失败诊断.

    must_produce：健康输入，必须有对应顶层 Markdown 产物；
    expect_failure：损坏/截断等预期失败输入，不得有产物且 GUI 日志须含失败诊断
    （T-16/T-24：文件、阶段与原因必须可区分，不得静默消失）；
    两者都为 False 是宽松输入（成功或失败均可接受，如空白透明图的空 OCR）。
    """

    must_produce: bool = True
    expect_failure: bool = False


# ---------------------------------------------------------------- 条目执行。


@dataclasses.dataclass
class PreparedInputs:
    """一次转换的一次性输入/输出目录与输入文件清单；error 非空表示合成失败."""

    input_dir: Path
    output_dir: Path
    files: list[str]
    error: str | None = None


# A 组内容断言表（S10-03 断言强度扩展）：require 命中、forbid 全文禁止、
# forbid_in_text_fences 仅围栏禁止、require_counts 恰好次数（A03 两处引用）、
# require_patterns 正则命中（A08 字段值存活于两段 run 之间）、ordered_tokens
# 显示顺序 + 图文邻近（A01/A14 另以独立源像素核对落盘身份）、forbid_outside_alt 只允许
# 出现在图片 alt 位置（A09 descr 不冒充正文）、require_in_texts GUI 任务文本
# 必含（A10 部分提取诊断 / A15 auto_mode 披露）。token 均避开数字（OCR 对
# 数字/字母易混，如 8→O、0→O），取稳定核心片段。


@dataclasses.dataclass(frozen=True)
class ContentSpec:
    """单条目的正文与 GUI 任务文本断言集合。."""

    require: tuple[str, ...] = ()
    forbid: tuple[str, ...] = ()
    forbid_in_text_fences: tuple[str, ...] = ()
    require_counts: tuple[tuple[str, int], ...] = ()
    require_patterns: tuple[str, ...] = ()
    ordered_tokens: tuple[str, ...] = ()
    forbid_outside_alt: tuple[str, ...] = ()
    require_in_texts: tuple[str, ...] = ()
