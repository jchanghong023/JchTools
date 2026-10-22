"""转 Markdown 验收条目执行器（自 scripts/markdown_acceptance.py 原样搬移）.

Context 与 A/B/C 组执行、委托与进程扫描链路；测试 patch 本族函数依赖的
SCRATCH_ROOT / GUI_SMOKE / drive_conversion 等名时，目标按本模块定义处维护。
"""

from __future__ import annotations

import dataclasses
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import TYPE_CHECKING

from scripts.gui_smoke import ISOLATION_GUARDED_STAGES
from scripts.markdown_acceptance_impl.content import (
    _A25_RULES,
    _CONTENT_ASSERTS,
    _INPUT_RULES,
    _assess_conversion,
    _content_problems,
    _document_semantic_problems,
    _input_rule,
)
from scripts.markdown_acceptance_impl.formats import (
    _runtime_format_extensions,
    _shared_format_rows,
    xberg_format_extensions,
)
from scripts.markdown_acceptance_impl.gui import (
    _isolation_precondition,
    _resolve_gui,
    drive_conversion,
)
from scripts.markdown_acceptance_impl.media_verify import (
    A18_INPUT_EXTENSIONS,
    _verify_a02_content,
    _verify_a18_unsupported_rejected,
    _verify_a24_scenes,
    _verify_a24_stop,
    _verify_a24_video,
    verify_svg_raster_media,
)
from scripts.markdown_acceptance_impl.model import (
    OLD_PROJECT_DIR,
    STATUS_FAILED,
    STATUS_NOT_RUN,
    STATUS_OK,
    AssetProbe,
    GuiRun,
    InputRule,
    Item,
    Outcome,
    PreparedInputs,
    _file_digest,
)
from scripts.markdown_acceptance_impl.paths import GUI_SMOKE, SCRATCH_ROOT
from scripts.markdown_acceptance_impl.preconditions import _c03_normal_state_artifact, _fixture_precondition
from scripts.markdown_acceptance_impl.release_scan import (
    _c05_assess,
    _old_cache_hits,
    manifest_has_python_entries,
    scan_distribution,
    scan_python_modules,
)
from scripts.markdown_acceptance_impl.synth import (
    A02_IMAGE_COUNT,
    A24_MEDIA_SYNTH_FILES,
    SYNTHESIZERS,
    _synth_precondition,
)

if TYPE_CHECKING:
    from collections.abc import Callable

__all__ = [
    "Context",
    "_a18_supported_extensions",
    "_asset_precondition",
    "_c03_delegate_stages",
    "_c05_scan_during_conversion",
    "_delegate_stage",
    "_gui_smoke_stages",
    "_prepare_scratch",
    "_run_c01_installed_scan",
    "_run_c02_portable_scan",
    "_run_c03_unconfigured",
    "_run_c05_no_python",
    "_run_c06_old_dir",
    "_run_c07_old_cache",
    "_run_c08_c09",
    "_run_common_mp4",
    "_run_common_old_office",
    "_run_content_assert",
    "_run_conversion_item",
    "_run_env",
    "_run_gui_ref",
    "_run_matrix",
    "_run_matrix_a02",
    "_run_matrix_a18",
    "_run_matrix_a24",
    "_run_matrix_a25",
    "_verify_a24_outputs",
    "run_item",
]


@dataclasses.dataclass
class Context:
    """一次运行的全部环境输入与共享探测缓存."""

    gui_exe: Path | None
    installed_root: Path | None
    portable_root: Path | None
    fixtures_dir: Path
    assets: AssetProbe
    stages: tuple[str, ...] | None = None
    stages_error: str | None = None
    format_rows: list[object] | None = None
    formats_error: str | None = None
    broker_process: subprocess.Popen[bytes] | None = None
    broker_close: Callable[[], None] | None = None
    profile: str = "full"

    def cached_stages(self) -> tuple[tuple[str, ...], str | None]:
        if self.stages is None:
            self.stages, self.stages_error = _gui_smoke_stages()
        return self.stages, self.stages_error

    def cached_formats(self) -> tuple[list[object], str | None]:
        if self.format_rows is None:
            self.format_rows, self.formats_error = _shared_format_rows(self)
        return self.format_rows, self.formats_error


def _asset_precondition(ctx: Context, item: Item) -> str | None:
    if not item.needs_assets:
        return None
    assets = ctx.assets
    if not assets.xberg_ready():
        reason = "；".join(assets.base_missing or assets.missing) or "Xberg 未配置"
        return f"资产未就绪：{reason}。获取方式：{assets.acquire_hint()}"
    if item.needs_assets == "xberg+media" and not assets.media_ready():
        holes = "；".join(assets.media_missing or assets.missing) or "媒体组件缺失"
        return f"媒体资产未就绪：{holes}。获取方式：{assets.acquire_hint()}"
    return None


# ---------------------------------------------------------------- B 组：gui_smoke 阶段引用。


def _gui_smoke_stages() -> tuple[tuple[str, ...], str | None]:
    if not GUI_SMOKE.is_file():
        return (), f"gui_smoke 缺失：{GUI_SMOKE}"
    done = subprocess.run(
        [sys.executable, str(GUI_SMOKE), "--list-stages"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if done.returncode != 0:
        return (), "gui_smoke 不支持 --list-stages（S5 补丁未应用或脚本损坏）"
    stages = tuple(
        line.strip().upper() for line in (done.stdout or "").splitlines() if line.strip().upper().startswith("S")
    )
    return stages, None


def _delegate_stage(stage: str, exe: Path, env_extra: dict[str, str] | None = None) -> Outcome:
    effective_env = os.environ | (env_extra or {})
    if stage in ISOLATION_GUARDED_STAGES:
        pending = _isolation_precondition(effective_env)
        if pending is not None:
            return pending
    # gui_smoke 的 main() 无条件校验数据目录存在；本入口只委托 S1/S5 这类不读
    # 数据集的阶段，提供一次性空目录即可通过校验（S2-S4 数据阶段不经此处）。
    scratch_data = SCRATCH_ROOT / "gui-smoke-data"
    scratch_data.mkdir(parents=True, exist_ok=True)
    argv = [
        sys.executable,
        str(GUI_SMOKE),
        "--exe",
        str(exe),
        "--data",
        str(scratch_data),
        "--stages",
        stage,
    ]
    # 子进程输出按 UTF-8 强制解码：GUI 及其内部子进程会向继承的捕获管道写
    # 本地化（GBK）诊断文本，按默认码表或严格 UTF-8 都可能解码失败。
    done = subprocess.run(
        argv,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
        env=effective_env,
    )
    tail = "\n".join(((done.stdout or "") + (done.stderr or "")).splitlines()[-8:])
    if done.returncode == 0:
        return Outcome(STATUS_OK, details=[tail] if tail else [])
    return Outcome(STATUS_FAILED, f"gui_smoke {stage} 退出码 {done.returncode}", [tail] if tail else [])


def _prepare_scratch(item: Item, ctx: Context, tag: str) -> PreparedInputs:
    """为一次转换在 .tmp/markdown-acceptance/ 下准备一次性输入/输出目录."""
    base = SCRATCH_ROOT / f"{item.item_id.lower()}-{tag}"
    if base.exists():
        shutil.rmtree(base, ignore_errors=True)
    prepared = PreparedInputs(base / "input", base / "output", [])
    _ = prepared.input_dir.mkdir(parents=True)
    _ = prepared.output_dir.mkdir(parents=True)
    for name in item.fixtures:
        source = ctx.fixtures_dir / name
        if source.is_file():
            _ = shutil.copy2(source, prepared.input_dir / Path(name).name)
            prepared.files.append(Path(name).name)
    if item.synth:
        result = SYNTHESIZERS[item.synth](prepared.input_dir, ctx.fixtures_dir)
        if result.error:
            prepared.error = result.error
            return prepared
        prepared.files.extend(name for name in result.files if name not in prepared.files)
    return prepared


def _run_conversion_item(  # noqa: PLR0913  # 前置与采集参数各自独立语义，收拢成结构体反而掩盖调用契约
    item: Item,
    ctx: Context,
    exe: Path | None,
    *,
    stop_mode: bool = False,
    tag_suffix: str = "run",
    capture: list[GuiRun] | None = None,
) -> Outcome:
    """通用转换执行：前置→准备→GUI 驱动→横切断言（A 组与 C06-C09 共用）.

    capture 非空时追加本次 GuiRun，供调用方核对完整 GUI 文本（批次统计/诊断）。
    """
    target, blocked = _resolve_gui(ctx, exe)
    if blocked is not None or target is None:
        return Outcome(STATUS_NOT_RUN, blocked or "被测 GUI 缺失")
    pending = None
    for reason in (_asset_precondition(ctx, item), _fixture_precondition(item, ctx.fixtures_dir)):
        if reason:
            pending = Outcome(STATUS_NOT_RUN, reason)
            break
    if pending is None:
        pending = _isolation_precondition(os.environ)
    if pending is not None:
        return pending
    prepared = _prepare_scratch(item, ctx, tag_suffix)
    if prepared.error is not None:
        return Outcome(STATUS_NOT_RUN, f"夹具合成失败：{prepared.error}")
    if not prepared.files:
        return Outcome(STATUS_NOT_RUN, "输入目录为空：夹具与合成器均未提供文件")
    sources = {
        path.relative_to(prepared.input_dir): _file_digest(path)
        for path in prepared.input_dir.rglob("*")
        if path.is_file()
    }
    run = drive_conversion(target, prepared.input_dir, prepared.output_dir, stop_after_busy=stop_mode)
    if capture is not None:
        capture.append(run)
    if run.error is not None:
        return Outcome(STATUS_FAILED, run.error, run.texts.splitlines()[-8:])
    return _assess_conversion(item, prepared, sources, run, stop_mode=stop_mode)


def _run_matrix(item: Item, ctx: Context) -> Outcome:
    blocked = _synth_precondition(item)
    if blocked:
        return Outcome(STATUS_NOT_RUN, blocked)
    special_handlers = {
        "A25": _run_matrix_a25,
        "A02": _run_matrix_a02,
        "A24": _run_matrix_a24,
        "A18": _run_matrix_a18,
    }
    handler = special_handlers.get(item.item_id)
    if handler is not None:
        return handler(item, ctx)
    if item.item_id in _CONTENT_ASSERTS:
        return _run_content_assert(item, ctx)
    return _run_conversion_item(item, ctx, None)


def _run_content_assert(item: Item, ctx: Context) -> Outcome:
    """通用转换 + 专用正文、图像身份和失败隔离判据."""
    captured: list[GuiRun] = []
    outcome = _run_conversion_item(item, ctx, None, capture=captured)
    if outcome.status != STATUS_OK:
        return outcome
    output = SCRATCH_ROOT / f"{item.item_id.lower()}-run" / "output"
    if item.item_id == "A07":
        error = verify_svg_raster_media(output)
        if error is not None:
            return Outcome(STATUS_FAILED, error, list(outcome.details))
    text = "\n".join(path.read_text(encoding="utf-8", errors="replace") for path in sorted(output.rglob("*.md")))
    gui_texts = captured[0].texts if captured else ""
    spec = _CONTENT_ASSERTS[item.item_id]
    problems = _content_problems(text, gui_texts, spec)
    problems.extend(_document_semantic_problems(item.item_id, output.parent / "input", output, gui_texts))
    if problems:
        return Outcome(STATUS_FAILED, "；".join(problems), list(outcome.details))
    spec_summary = " ".join(
        (
            f"require={list(spec.require)}",
            f"counts={spec.require_counts}",
            f"patterns={list(spec.require_patterns)}",
            f"ordered={spec.ordered_tokens}",
            f"alt-forbid={list(spec.forbid_outside_alt)}",
            f"texts={list(spec.require_in_texts)}",
        )
    )
    details = [
        f"内容断言通过：{spec_summary}",
        *outcome.details,
    ]
    return Outcome(STATUS_OK, details=details)


def _run_matrix_a02(item: Item, ctx: Context) -> Outcome:
    first = _run_conversion_item(item, ctx, None, tag_suffix="first")
    if first.status != STATUS_OK:
        return first
    second = _run_conversion_item(item, ctx, None, tag_suffix="second")
    if second.status != STATUS_OK:
        return second
    left = SCRATCH_ROOT / "a02-first" / "output"
    right = SCRATCH_ROOT / "a02-second" / "output"
    left_files = sorted(path.relative_to(left).as_posix() for path in left.rglob("*.md"))
    right_files = sorted(path.relative_to(right).as_posix() for path in right.rglob("*.md"))
    if left_files != right_files:
        return Outcome(STATUS_FAILED, f"两次转换产物集合不一致：{left_files} vs {right_files}")
    for name in left_files:
        if (left / name).read_bytes() != (right / name).read_bytes():
            return Outcome(STATUS_FAILED, f"两次转换内容不稳定：{name}")
    problem = _verify_a02_content(left)
    if problem is not None:
        return Outcome(STATUS_FAILED, problem)
    return Outcome(
        STATUS_OK,
        details=[f"两次转换产物逐字节一致，且包含 {A02_IMAGE_COUNT} 张合成图片正文内容"],
    )


def _verify_a24_outputs() -> str | None:
    outputs = SCRATCH_ROOT / "a24-full" / "output"
    return _verify_a24_video(outputs) or _verify_a24_scenes(outputs)


def _run_common_mp4(item: Item, ctx: Context) -> Outcome:
    outcome = _run_conversion_item(item, ctx, None, tag_suffix="common-mp4")
    if outcome.status != STATUS_OK:
        return outcome
    problem = _verify_a24_video(SCRATCH_ROOT / "a24-common-mp4" / "output", ("mp4",))
    if problem is not None:
        return Outcome(STATUS_FAILED, problem, outcome.details)
    return Outcome(STATUS_OK, details=["既有 MP4 中文转录与时间戳、原文件保护通过", *outcome.details])


def _run_matrix_a24(item: Item, ctx: Context) -> Outcome:
    if ctx.profile == "common":
        return _run_common_mp4(item, ctx)
    stopped_runs: list[GuiRun] = []
    stop_phase = _run_conversion_item(item, ctx, None, stop_mode=True, tag_suffix="stop", capture=stopped_runs)
    if stop_phase.status != STATUS_OK:
        return stop_phase
    files = [Path(name).name for name in item.fixtures] + list(A24_MEDIA_SYNTH_FILES)
    convertible = sum(1 for name in files if _input_rule("A24", name).must_produce)
    stop_problem = _verify_a24_stop(
        len(stopped_runs[0].outputs) if stopped_runs else 0,
        convertible,
        stopped_runs[0].texts if stopped_runs else "",
    )
    if stop_problem is not None:
        return Outcome(STATUS_FAILED, stop_problem)
    full = _run_conversion_item(item, ctx, None, tag_suffix="full")
    if full.status != STATUS_OK:
        return full
    problem = _verify_a24_outputs()
    if problem is not None:
        return Outcome(STATUS_FAILED, problem)
    return Outcome(
        STATUS_OK,
        details=["真实中文转录、时间戳结构、无音轨/无语音/正弦音说明、停止后后续文件未处理、损坏音轨失败隔离全部符合"],
    )


def _a18_supported_extensions(item: Item, ctx: Context) -> set[str] | Outcome:
    """A18 行为执行前确认全部夹具与实际能力，不以静态清单代替。."""
    fixture_error = _fixture_precondition(item, ctx.fixtures_dir)
    if fixture_error is not None:
        return Outcome(STATUS_NOT_RUN, fixture_error)
    extensions, error = _runtime_format_extensions(ctx)
    if error is not None:
        return Outcome(STATUS_NOT_RUN, error)
    supported = {str(extension) for extension in A18_INPUT_EXTENSIONS if extension in extensions}
    if not supported:
        return Outcome(STATUS_NOT_RUN, "当前共享引擎未声明 A18 六种夹具中的任何格式，不能据静态清单声称支持")
    return supported


def _run_matrix_a18(item: Item, ctx: Context) -> Outcome:
    """A18：同一共享引擎的实际能力决定支持产物及混跑排除规则。."""
    supported = _a18_supported_extensions(item, ctx)
    if isinstance(supported, Outcome):
        return supported
    _INPUT_RULES["A18"] = {
        Path(name).name: InputRule(must_produce=Path(name).suffix.lstrip(".").lower() in supported)
        for name in item.fixtures
    }
    supported_fixtures = tuple(name for name in item.fixtures if Path(name).suffix.lstrip(".").lower() in supported)
    supported_item = dataclasses.replace(item, fixtures=supported_fixtures)
    outcome = _run_conversion_item(supported_item, ctx, None, tag_suffix="supported")
    if outcome.status != STATUS_OK:
        return outcome
    # 仅清单声明不构成行为证据：混跑六种真实夹具，逐输入核对支持产物并核对未声明排除。
    mixed_runs: list[GuiRun] = []
    mixed_outcome = _run_conversion_item(item, ctx, None, tag_suffix="mixed", capture=mixed_runs)
    if mixed_outcome.status != STATUS_OK:
        return mixed_outcome
    mixed_texts = mixed_runs[0].texts if mixed_runs else ""
    problem = _verify_a18_unsupported_rejected(SCRATCH_ROOT / "a18-mixed" / "output", mixed_texts, supported)
    if problem is not None:
        return Outcome(STATUS_FAILED, problem, list(mixed_outcome.details))
    unsupported = ", ".join(f".{name}" for name in A18_INPUT_EXTENSIONS if name not in supported) or "无"
    return Outcome(
        STATUS_OK,
        details=[
            *outcome.details,
            f"当前共享引擎未声明格式：{unsupported}；混跑确认逐输入产物与统计均符合实际能力",
        ],
    )


def _run_common_old_office(item: Item, ctx: Context) -> Outcome:
    outcome = _run_conversion_item(item, ctx, None, tag_suffix="common-office")
    if outcome.status != STATUS_OK:
        return outcome
    output = SCRATCH_ROOT / "a25-common-office" / "output"
    for suffix in ("doc", "xls", "ppt"):
        result = output / f"legacy_{suffix}.md"
        expected = f"JCHTOOLS-LEGACY-{suffix.upper()}"
        if not result.is_file() or expected not in result.read_text(encoding="utf-8"):
            return Outcome(STATUS_FAILED, f"旧 Office {suffix.upper()} 缺少合成正文标记", outcome.details)
    return outcome


def _run_matrix_a25(item: Item, ctx: Context) -> Outcome:
    if ctx.profile == "common":
        return _run_common_old_office(item, ctx)
    if ctx.assets.xberg_exe is None:
        return Outcome(STATUS_NOT_RUN, f"资产未就绪。获取方式：{ctx.assets.acquire_hint()}")
    extensions, error = xberg_format_extensions(ctx)
    if error:
        return Outcome(STATUS_NOT_RUN, error)
    sweep_dir = ctx.fixtures_dir / "matrix" / "format_sweep"
    missing = [ext for ext in extensions if next(sweep_dir.glob(f"*.{ext}"), None) is None]
    if missing:
        reason = f"Xberg 清单中还有 {len(missing)} 个未覆盖格式缺最小烟测样本：{missing}；"
        reason += f"请逐个放入 {sweep_dir}（无敏感内容的公开合成样本）"
        return Outcome(STATUS_NOT_RUN, reason)
    # sweep 样本按扩展名进入通用转换条目：注入 fixtures 让 _prepare_scratch 拷贝它们。
    # 逐输入规则按引擎实查清单收紧（S10-02 复审）：引擎声明的格式必须有产物，
    # 未声明格式（引擎回归或清单外的历史样本）保留宽松兜底。
    sweep_files = sorted(path for path in sweep_dir.iterdir() if path.is_file())
    declared = {ext.strip().lower() for ext in extensions}
    _A25_RULES.clear()
    for path in sweep_files:
        ext = path.suffix.lstrip(".").lower()
        _A25_RULES[path.name] = InputRule(must_produce=ext in declared)
    sweep_item = dataclasses.replace(
        item, fixtures=tuple(sorted(f"matrix/format_sweep/{path.name}" for path in sweep_files))
    )
    return _run_conversion_item(sweep_item, ctx, None)


def _run_gui_ref(item: Item, ctx: Context) -> Outcome:
    stages, error = ctx.cached_stages()
    if error:
        return Outcome(STATUS_NOT_RUN, error)
    if item.stage not in stages:
        return Outcome(STATUS_NOT_RUN, f"gui_smoke 未提供 {item.stage} 阶段（现有：{list(stages)}）")
    target, blocked = _resolve_gui(ctx)
    if blocked is not None or target is None:
        return Outcome(STATUS_NOT_RUN, blocked or "被测 GUI 缺失")
    pending = _asset_precondition(ctx, item)
    if pending:
        return Outcome(STATUS_NOT_RUN, pending)
    env_extra: dict[str, str] = {}
    if item.stage in ("S7", "S8") and ctx.assets.runtime_dir is not None:
        # 重启/显式音频动作必须保留同一有效根；旧外部目录仅作为迁移夹具。
        env_extra["JCHTOOLS_SMOKE_XBERG_DIR"] = str(ctx.assets.runtime_dir)
    if item.stage in ("S5", "S14") and not os.environ.get("JCHTOOLS_S5_MEDIA"):
        # S5/S14 需要真实媒体样本观察运行态；默认使用已有夹具中的真实视频。
        media = ctx.fixtures_dir / "video-to-notes-intro-zh.mp4"
        if media.is_file():
            env_extra["JCHTOOLS_S5_MEDIA"] = str(media)
    return _delegate_stage(item.stage, target, env_extra or None)


def _run_c01_installed_scan(item: Item, ctx: Context) -> Outcome:
    root = ctx.installed_root
    if root is None:
        message = "未提供安装目录（--installed-root；安装版位置见 installer/JchTools.iss 的 DefaultDirName）"
        return Outcome(STATUS_NOT_RUN, message)
    if not root.is_dir():
        return Outcome(STATUS_NOT_RUN, f"安装目录不存在：{root}")
    if not (root / "JchTools.exe").is_file():
        return Outcome(STATUS_FAILED, f"安装目录缺少 JchTools.exe：{root}")
    problems = scan_distribution(root)
    if problems:
        return Outcome(STATUS_FAILED, f"安装版基础完整性/默认资产策略失败：{problems[:8]}", problems)
    return Outcome(
        STATUS_OK,
        details=[
            f"{item.item_id} 扫描 {root}：内置收据及全部基础成员大小/SHA-256 已核对，无音频/Python 资产",
            "收据记录固定发布清单来源；本检查离线，不声称重新下载核验远端发布清单",
        ],
    )


def _run_c02_portable_scan(item: Item, ctx: Context) -> Outcome:
    root = ctx.portable_root
    if root is None:
        return Outcome(STATUS_NOT_RUN, "未提供便携目录（--portable-root；解包 package-windows.ps1 产出的便携 ZIP）")
    if not root.is_dir():
        return Outcome(STATUS_NOT_RUN, f"便携目录不存在：{root}")
    if not (root / "JchTools.exe").is_file():
        return Outcome(STATUS_FAILED, f"便携目录缺少 JchTools.exe：{root}")
    problems = scan_distribution(root)
    if problems:
        return Outcome(STATUS_FAILED, f"便携版基础完整性/默认资产策略失败：{problems[:8]}", problems)
    return Outcome(
        STATUS_OK,
        details=[
            f"{item.item_id} 扫描 {root}：内置收据及全部基础成员大小/SHA-256 已核对，无音频/Python 资产",
            "收据记录固定发布清单来源；本检查离线，不声称重新下载核验远端发布清单",
        ],
    )


def _c03_delegate_stages(item_id: str, target: Path, env: dict[str, str]) -> Outcome | None:
    """S10-11：同一隔离环境委托 S1 启动与 S15（MD 整理合并/拆分，自备数据）.

    只启动 GUI 不能证明「旧工具正常可用」；S15 用 tempfile 样本驱动旧工具的
    真实可观察操作。非 None 即未通过。
    """
    result = _delegate_stage("S1", target, env_extra=env)
    if result.status != STATUS_OK:
        result.details.insert(0, f"{item_id} 未配置状态下的 S1 启动未通过")
        return result
    legacy = _delegate_stage("S15", target, env_extra=env)
    if legacy.status != STATUS_OK:
        legacy.details.insert(0, f"{item_id} 未配置状态下旧工具（MD 整理 S15）可观察操作未通过")
        return legacy
    return None


def _run_c03_unconfigured(item: Item, ctx: Context) -> Outcome:
    target, blocked = _resolve_gui(ctx)
    if blocked is not None or target is None:
        return Outcome(STATUS_NOT_RUN, blocked or "被测 GUI 缺失")
    # 是否启用 test-hooks 由委托 GUI 的本轮 PID/角色/启动日志证明，目录名不是能力证据。
    stages, error = ctx.cached_stages()
    missing_stages = [stage for stage in ("S1", "S15") if stage not in stages]
    if error is not None or missing_stages:
        reason = error or f"gui_smoke 未提供 {missing_stages} 阶段（S1 启动/S15 旧工具操作无法委托）"
        return Outcome(STATUS_NOT_RUN, reason)
    scratch_assets = SCRATCH_ROOT / "c03-asset-root"
    scratch_state = SCRATCH_ROOT / "c03-state-root"
    for directory in (scratch_assets, scratch_state):
        shutil.rmtree(directory, ignore_errors=True)
        _ = directory.mkdir(parents=True)
    env = {
        "JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT": str(scratch_assets),
        "JCHTOOLS_TEST_ASSET_ROOT": str(scratch_assets),
        "JCHTOOLS_TEST_STATE_DIR": str(scratch_state),
        "JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT": str(scratch_assets),
        "JCHTOOLS_TEST_BUNDLED_XBERG_DIR": str(scratch_assets / "absent-bundle"),
    }
    delegated = _c03_delegate_stages(item.item_id, target, env)
    if delegated is not None:
        return delegated
    # config::state_dir 与 xberg_settings 共用隔离根，P-10 日志和普通工具任务/哈希
    # 缓存均为合法状态。资产根独立检查须为空；状态根只豁免源码明确声明的落位。
    # xberg-downloads 在状态根下，不能因状态与资产分开便漏掉自动下载反例。
    downloaded = [f"assets/{path.relative_to(scratch_assets).as_posix()}" for path in scratch_assets.rglob("*")]
    downloaded.extend(
        f"state/{path.relative_to(scratch_state).as_posix()}"
        for path in scratch_state.rglob("*")
        if not _c03_normal_state_artifact(path, scratch_state)
    )
    if downloaded:
        return Outcome(STATUS_FAILED, f"未配置启动即写入/下载资产目录：{downloaded[:8]}")
    return Outcome(
        STATUS_OK,
        details=[
            f"{item.item_id} 未配置启动正常退出，隔离设置、日志与普通工具状态落位正常，无资产下载",
            f"{item.item_id} 同环境下旧工具（MD 整理 S15 合并/拆分）可观察操作通过，未受未配置状态影响",
        ],
    )


def _c05_scan_during_conversion(exe: Path, input_dir: Path, output_dir: Path) -> tuple[GuiRun, list[str], list[str]]:
    """转换运行期间反复枚举相关进程的加载模块，捕捉对 python*.dll 的隐藏调用."""
    module_hits: list[str] = []
    scan_errors: list[str] = []
    finished = threading.Event()
    scanned = threading.Event()

    def scanner() -> None:
        while not finished.is_set():
            # S10-10：XB 进程模型下承接转换的是 JchTools.exe 与共享引擎 xberg.exe；
            # markdown-media-worker 已按 XB-12 退役，不得残留在扫描名单。
            try:
                hits, error = scan_python_modules(("JchTools.exe", "xberg.exe"))
            except Exception as exception:  # noqa: BLE001  # 线程异常必须转入未验证结论，不能丢失扫描失败。
                scan_errors.append(f"进程模块枚举异常：{type(exception).__name__}: {exception}")
                return
            module_hits.extend(hits)
            if error is not None:
                scan_errors.append(error)
                return
            scanned.set()
            time.sleep(0.5)

    thread = threading.Thread(target=scanner, daemon=True)
    thread.start()
    try:
        run = drive_conversion(exe, input_dir, output_dir)
    finally:
        finished.set()
        thread.join(timeout=5)
        if thread.is_alive():
            scan_errors.append("进程模块枚举线程未结束，不能证明转换期间无 Python")
        elif not scanned.is_set() and not scan_errors:
            scan_errors.append("进程模块枚举未完成任何一轮，不能证明转换期间无 Python")
    return run, module_hits, scan_errors


def _run_c05_no_python(item: Item, ctx: Context) -> Outcome:
    pending = _asset_precondition(ctx, item)
    if pending:
        return Outcome(STATUS_NOT_RUN, pending)
    target, blocked = _resolve_gui(ctx)
    if blocked is not None or target is None:
        return Outcome(STATUS_NOT_RUN, blocked or "被测 GUI 缺失")
    pending = _isolation_precondition(os.environ)
    if pending is not None:
        return pending
    if manifest_has_python_entries():
        return Outcome(STATUS_FAILED, "可选资产清单包含 python 字样条目（隐藏携带 Python）")
    prepared = _prepare_scratch(item, ctx, "python-scan")
    if prepared.error is not None or not prepared.files:
        return Outcome(STATUS_NOT_RUN, f"夹具准备失败：{prepared.error or '输入为空'}")
    sources = {
        path.relative_to(prepared.input_dir): _file_digest(path)
        for path in prepared.input_dir.rglob("*")
        if path.is_file()
    }
    run, module_hits, scan_errors = _c05_scan_during_conversion(target, prepared.input_dir, prepared.output_dir)
    return _c05_assess(item, prepared, sources, run, module_hits, scan_errors)


def _run_c06_old_dir(item: Item, ctx: Context) -> Outcome:
    if OLD_PROJECT_DIR.exists():
        message = f"旧项目目录仍存在：{OLD_PROJECT_DIR}（T-28 删除前置未达成；删除/改名后再验）"
        return Outcome(STATUS_NOT_RUN, message)
    return _run_conversion_item(item, ctx, None, tag_suffix="noolddir")


def _run_c07_old_cache(item: Item, ctx: Context) -> Outcome:
    hits = _old_cache_hits()
    if hits:
        return Outcome(STATUS_NOT_RUN, f"旧缓存仍在场：{hits}")
    return _run_conversion_item(item, ctx, None, tag_suffix="nooldcache")


def _run_c08_c09(item: Item, _ctx: Context, root: Path | None, label: str) -> Outcome:
    if root is None or not root.is_dir():
        switch = "--installed-root" if item.item_id == "C08" else "--portable-root"
        return Outcome(STATUS_NOT_RUN, f"未提供{label}目录（{switch}）")
    exe = root / "JchTools.exe"
    if not exe.is_file():
        return Outcome(STATUS_NOT_RUN, f"{label}目录缺少 JchTools.exe：{exe}")
    # 正式包须在独立 Windows 会话的干净配置中直接发现内置基础资产；
    # release 忽略测试环境变量，当前承接不能安全启动它或用 debug 冒充。
    reason = f"{label} release 忽略测试状态/资产根环境变量；当前驱动缺少可核对的独立 Windows "
    reason += (
        "会话，禁止启动正式 EXE 或读写生产配置。须在独立 Windows 会话的干净配置中验证内置首次转换，无需设置初始化。"
    )
    return Outcome(STATUS_NOT_RUN, reason, [f"{item.item_id} 未启动被测正式 EXE：{exe}"])


def _run_env(item: Item, ctx: Context) -> Outcome:
    handlers: dict[str, Callable[[Item, Context], Outcome]] = {
        "C01": _run_c01_installed_scan,
        "C02": _run_c02_portable_scan,
        "C03": _run_c03_unconfigured,
        "C05": _run_c05_no_python,
        "C06": _run_c06_old_dir,
        "C07": _run_c07_old_cache,
        "C08": lambda item, ctx: _run_c08_c09(item, ctx, ctx.installed_root, "安装版"),
        "C09": lambda item, ctx: _run_c08_c09(item, ctx, ctx.portable_root, "便携版"),
    }
    return handlers[item.item_id](item, ctx)


def run_item(item: Item, ctx: Context) -> Outcome:
    dispatch: dict[str, Callable[[Item, Context], Outcome]] = {
        "A": _run_matrix,
        "B": _run_gui_ref,
        "C": _run_env,
    }
    try:
        return dispatch[item.group](item, ctx)
    except Exception as exc:  # noqa: BLE001  # 单项意外错误按 FAIL 呈现，不拖垮其余条目。
        return Outcome(STATUS_FAILED, f"条目执行异常：{type(exc).__name__}: {exc}")
