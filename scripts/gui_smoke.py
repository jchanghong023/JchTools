#!/usr/bin/env python3
"""JchTools GUI OS 级冒烟测试（pywinauto / UIA）.

关键路径冒烟（2026-09-18 两工具拆分后；S6-S14 为转 Markdown GUI E2E 扩展）：
  S1 启动并正常退出；
  S2 目录整理：选择目录 → 开始分析（只读，无确认框）→ 计划生成（不执行）；
  S3 目录整理全链路：开始分析 → 确认执行 → 整理完成；
  S4 递归解压全链路：开始解压 → 一段确认 → 解压结束；完整成功的原包/分卷删除，既有内容保留，
     冲突自动改名、嵌套内容落盘，失败包保留在「解压失败」（H-07/X-05/X-06）。
  S5 转 Markdown 基本链路：启动 → 切到「转 Markdown」→ 选输入/输出 → 开始 → 停止 → 关闭
     （T 分区附录 A GUI E2E 最小段；需 Xberg 已配置且组件就绪，默认序列不含 S5，须显式 --stages 请求）。
  S6 随包基础就绪：文档/截图已安装、音频未下载，核对发布收据、实际文件与 SQLite 活动来源。
  S7 重启恢复：完全退出后仍使用同一随包目录，旧路径原件与来源记录保留。
  S8 音频显式下载边界：受控 test-hooks 下载屏障下验证无隐式请求、主动下载、取消与重试，
     不连接真实网络，不改变基础文件或活动目录。
  S9 基础包缺失：转换禁用、音频下载禁用，离线检查给出修复安装/重新解压指引，不自动下载。
  S10 输出层级与同名策略：保留层级两份同名输入各自成文；平铺只处理排序第一份、
     其余按重复跳过计数（T-11；需真实组件，Xberg 目录可真实，状态目录仍须隔离）。
     所有非自隔离阶段受统一隔离守卫：独立调用需验收编排布置的隔离环境变量在场
     （JCHTOOLS_TEST_STATE_DIR 与 JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT），缺则拒绝
     启动并提示经 acceptance.ps1 入口执行；公共启动入口始终要求状态与资产根隔离。
  S11 已有结果跳过：重复运行不覆盖既有产物，跳过数量如实显示（T-12）。
  S12 输出子树排除：输出目录位于输入内时整棵输出子树不作为新输入（T-09）。
  S13 部分失败与完成统计：单文件失败不终止批次，成功/失败可区分（T-16/T-24）。
  S14 运行中关闭确认与安全停止：转换运行中点标题栏「关闭」弹出「停止任务并关闭」，
     确认后取消当前文件、清理未完成产物、进程退出码 0（U-09/T-23；需真实媒体样本）。
  S15 MD 合并/拆分真实 GUI：验证标题下移、原文件不变与 UTF-8 分片无损还原。
  S16 Git 真实 GUI：使用一次性仓库和本地 bare 远端，验证逐文件提交及推送结果。
  S17 预置真实最新 Xberg 后，经设置页离线检查补写 notice，核对实际转换就绪状态。
  S18 在两档窗口尺寸下逐页检查主要操作控件及退出模型服务确认层的边界与重叠；
     独占隔离状态、资产、CodeAgent 凭据与注册表，真实空配置服务仅用于打开确认层，
     随后返回检查，不确认退出；阶段收尾只回收本轮拥有的后台进程。

用法：
    python scripts/gui_smoke.py --exe target/debug/JchTools.exe --data <已生成的测试数据目录>
    非自隔离阶段独立调用时须已布置隔离环境变量；推荐经 scripts/acceptance.ps1
    编排执行。--allow-isolated-run 仅跳过 CLI 前置检查，不能绕过公共启动入口的隔离。

CLI 实际执行自动写入独立 `.tmp/test-reports/<run-id>/junit.xml` 与 `run.json`；
逐 S 阶段实测，数据集阶段计入夹具复制与清理，运行墙钟包含共享收尾。
`--list-stages` / 帮助及直接调用阶段函数不创建报告；失败仍按原顺序立即停止。
依赖：pip install pywinauto（需要可交互桌面会话）。
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path
from typing import TYPE_CHECKING

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scripts.test_reports import Case, TimingReport

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping


# 测试与外部脚本经 gui_smoke.<名> 访问以下 re-export、模块锚点与入口函数（mock.patch 的模块对象目标经入口导入解析）。
__all__ = [
    "ARCHIVE_SUFFIXES",
    "ATTEMPT_TIMEOUT",
    "COMPLETION_TIMEOUT",
    "CONFIRM_CLOSED_OBSERVATIONS",
    "CONVERT_BUSY_TIMEOUT",
    "CONVERT_DIR_ROWS",
    "CONVERT_DONE_MARKER",
    "CONVERT_NEED_RUNTIME_TEXT",
    "CONVERT_STOP_TIMEOUT",
    "DEFAULT_DATA",
    "DEFAULT_EXE",
    "DEFAULT_STAGES",
    "EDIT_ROW_TOLERANCE_PX",
    "EXTRACT_ACK",
    "ISOLATION_GUARDED_STAGES",
    "ISOLATION_REQUIRED_ENV_KEYS",
    "LAYOUT_PAGES",
    "MD_SPLIT_LIMIT_BYTES",
    "ORGANIZE_ACK",
    "PHYSICAL_OVERLAP_THRESHOLD",
    "S5_LAST_ATTEMPT",
    "S11_SOURCE_FILES",
    "SETTINGS_BASE_LABELS",
    "SETTINGS_CHECK_BUTTON",
    "SETTINGS_DOWNLOAD_BUTTON",
    "SETTINGS_RETRY_BUTTON",
    "STOP_AND_CLOSE_BUTTON",
    "STOP_AND_CLOSE_TITLE",
    "STOP_MEDIA_FILE_COUNT",
    "STREAM_PAYLOAD_CASES",
    "SUPPORTED_STAGES",
    "TASK_RECORD_ERRORS",
    "TIMEOUT",
    "TRANSIENT_GUI_ERRORS",
    "Case",
    "NoPatternInterfaceError",
    "OwnedProcessTree",
    "TimingReport",
    "_CliArgs",
    "_GuiTiming",
    "_run_cli_stages",
    "activate",
    "active_xberg_settings",
    "analyze_until_ready",
    "argparse",
    "assert_clean_exit",
    "check_flat_output",
    "click",
    "click_and_wait_text",
    "close_app",
    "close_title_bar",
    "completion_confirms_new_run",
    "confirm_dialog",
    "confirm_new_completion",
    "converter_directory_rows",
    "ensure_smoke_target_is_disposable",
    "escalate_close",
    "file_digest",
    "find_button",
    "find_check",
    "goto_converter",
    "goto_organizer",
    "goto_settings",
    "isolated_state_env",
    "isolation_probe",
    "main",
    "missing_isolation_env",
    "open_confirm",
    "optional_audio_member",
    "os",
    "parse_stages",
    "plain_windows_path",
    "post_control_click",
    "prepare_organize_witnesses",
    "process_runtime",
    "produced_snapshot",
    "provision_bundled_runtime",
    "read_xberg_settings",
    "require",
    "require_orchestrated_isolation",
    "run_dataset_stages",
    "run_stage",
    "s1_launch_and_exit",
    "s2_analyze_only",
    "s3_full_organize",
    "s4_full_extract",
    "s5_markdown_basic_chain",
    "s6_bundled_readiness_and_active_source",
    "s7_restart_restores_active_runtime",
    "s8_explicit_audio_download_cancel_and_retry",
    "s9_missing_base_requires_install_repair",
    "s10_output_layout_and_flat_duplicate_policy",
    "s11_existing_results_are_skipped_untouched",
    "s12_output_subtree_is_excluded_from_scan",
    "s13_partial_failure_isolated_with_counts",
    "s14_close_during_conversion_confirms_and_stops",
    "s15_markdown_merge_and_split",
    "s16_git_commit_and_push",
    "s17_check_configured_components_offline",
    "s18_layout_at_two_window_sizes",
    "same_physical_control",
    "same_runtime_path",
    "seed_legacy_runtime",
    "set_converter_dirs",
    "set_directory",
    "set_edit_before_button",
    "settings_stage_workspace",
    "setup_directory",
    "shutil",
    "snapshot_output_bytes",
    "sqlite3",
    "start_conversion_and_wait_done",
    "start_owned_process",
    "state_dir",
    "subprocess",
    "sys",
    "task_ids",
    "task_records",
    "tempfile",
    "time",
    "timings",
    "tiny_png",
    "tree_snapshot",
    "unique_visible_buttons",
    "verify_bundled_package",
    "verify_bundled_settings",
    "verify_cancelled_outputs",
    "verify_control_rectangles",
    "verify_extracted_outputs",
    "verify_extraction_results",
    "verify_layout_at_two_sizes",
    "verify_layout_buttons",
    "verify_organize_witnesses",
    "verify_original_disposition",
    "wait_base_ready",
    "wait_bundled_settings",
    "wait_button",
    "wait_completion_with_anchor",
    "wait_download_barrier",
    "wait_exit_or_kill",
    "wait_legacy_runtime",
    "wait_log_containing",
    "wait_log_matching",
    "wait_task_status",
    "wait_text_containing",
    "wait_window",
    "wait_window_closed_after_cancel",
    "win32api",
    "win32con",
    "win32gui",
    "win32process",
]

# 以下为拆分后的实现 re-export：外部脚本与测试仍经 scripts.gui_smoke.<名> 访问；
# 超时常量的测试补丁口径是 scripts.gui_smoke_impl.config（各模块以 config.<名> 属性式读取）。

# 测试与外部脚本经 gui_smoke.<模块名> 访问这些导入（补丁锚点/工具对象）；
# 实现拆分后入口仍保留显式 re-export。
import sqlite3
import subprocess

import win32api
import win32con
import win32gui
import win32process
from pywinauto import timings
from pywinauto.uia_defines import NoPatternInterfaceError

from scripts import isolation_probe, process_runtime
from scripts.gui_smoke_impl.config import (
    ARCHIVE_SUFFIXES,
    ATTEMPT_TIMEOUT,
    COMPLETION_TIMEOUT,
    CONFIRM_CLOSED_OBSERVATIONS,
    CONVERT_BUSY_TIMEOUT,
    CONVERT_DIR_ROWS,
    CONVERT_DONE_MARKER,
    CONVERT_NEED_RUNTIME_TEXT,
    CONVERT_STOP_TIMEOUT,
    DEFAULT_DATA,
    DEFAULT_EXE,
    DEFAULT_STAGES,
    EDIT_ROW_TOLERANCE_PX,
    EXTRACT_ACK,
    ISOLATION_GUARDED_STAGES,
    ISOLATION_REQUIRED_ENV_KEYS,
    MD_SPLIT_LIMIT_BYTES,
    ORGANIZE_ACK,
    PHYSICAL_OVERLAP_THRESHOLD,
    S5_LAST_ATTEMPT,
    S11_SOURCE_FILES,
    SETTINGS_BASE_LABELS,
    SETTINGS_CHECK_BUTTON,
    SETTINGS_DOWNLOAD_BUTTON,
    SETTINGS_RETRY_BUTTON,
    STOP_AND_CLOSE_BUTTON,
    STOP_AND_CLOSE_TITLE,
    STOP_MEDIA_FILE_COUNT,
    STREAM_PAYLOAD_CASES,
    SUPPORTED_STAGES,
    TASK_RECORD_ERRORS,
    TIMEOUT,
)
from scripts.gui_smoke_impl.convert_ui import (
    check_flat_output,
    click_and_wait_text,
    close_title_bar,
    completion_confirms_new_run,
    confirm_new_completion,
    converter_directory_rows,
    find_check,
    produced_snapshot,
    set_edit_before_button,
    unique_visible_buttons,
    wait_button,
    wait_completion_with_anchor,
    wait_log_containing,
    wait_log_matching,
    wait_text_containing,
    wait_window_closed_after_cancel,
)
from scripts.gui_smoke_impl.driver import (
    TRANSIENT_GUI_ERRORS,
    OwnedProcessTree,
    activate,
    assert_clean_exit,
    click,
    close_app,
    confirm_dialog,
    escalate_close,
    find_button,
    missing_isolation_env,
    open_confirm,
    post_control_click,
    require,
    run_stage,
    same_physical_control,
    set_directory,
    setup_directory,
    start_owned_process,
    wait_exit_or_kill,
    wait_window,
)
from scripts.gui_smoke_impl.postconditions import (
    LAYOUT_PAGES,
    file_digest,
    optional_audio_member,
    prepare_organize_witnesses,
    snapshot_output_bytes,
    tiny_png,
    tree_snapshot,
    verify_bundled_package,
    verify_cancelled_outputs,
    verify_control_rectangles,
    verify_extracted_outputs,
    verify_extraction_results,
    verify_layout_at_two_sizes,
    verify_layout_buttons,
    verify_organize_witnesses,
    verify_original_disposition,
)
from scripts.gui_smoke_impl.records import (
    active_xberg_settings,
    isolated_state_env,
    plain_windows_path,
    read_xberg_settings,
    same_runtime_path,
    seed_legacy_runtime,
    settings_stage_workspace,
    state_dir,
    task_ids,
    task_records,
    verify_bundled_settings,
    wait_task_status,
)
from scripts.gui_smoke_impl.stages_convert import (
    goto_converter,
    s5_markdown_basic_chain,
    s10_output_layout_and_flat_duplicate_policy,
    s11_existing_results_are_skipped_untouched,
    s12_output_subtree_is_excluded_from_scan,
    s13_partial_failure_isolated_with_counts,
    s14_close_during_conversion_confirms_and_stops,
    set_converter_dirs,
    start_conversion_and_wait_done,
)
from scripts.gui_smoke_impl.stages_misc import (
    s15_markdown_merge_and_split,
    s16_git_commit_and_push,
    s17_check_configured_components_offline,
    s18_layout_at_two_window_sizes,
)
from scripts.gui_smoke_impl.stages_organize import (
    analyze_until_ready,
    goto_organizer,
    s1_launch_and_exit,
    s2_analyze_only,
    s3_full_organize,
    s4_full_extract,
)
from scripts.gui_smoke_impl.stages_settings import (
    goto_settings,
    provision_bundled_runtime,
    s6_bundled_readiness_and_active_source,
    s7_restart_restores_active_runtime,
    s8_explicit_audio_download_cancel_and_retry,
    s9_missing_base_requires_install_repair,
    wait_base_ready,
    wait_bundled_settings,
    wait_download_barrier,
    wait_legacy_runtime,
)


class _CliArgs(argparse.Namespace):
    """命令行参数容器：以固定字段取代 Namespace 的动态属性（运行期行为与默认 Namespace 一致）."""

    exe: str
    data: str
    stages: str
    list_stages: bool
    allow_isolated_run: bool

    def __init__(self) -> None:
        super().__init__()
        self.exe = DEFAULT_EXE
        self.data = DEFAULT_DATA
        self.stages = DEFAULT_STAGES
        self.list_stages = False
        self.allow_isolated_run = False


def parse_stages(stages_arg: str) -> list[str]:
    """解析并校验 --stages：逗号分隔、大小写不敏感、未知阶段立即失败."""
    stages = [token.strip().upper() for token in stages_arg.split(",") if token.strip()]
    if not stages:
        message = "阶段清单不能为空，未执行任何阶段不能报告成功"
        raise RuntimeError(message)
    unknown = [stage for stage in stages if stage not in SUPPORTED_STAGES]
    if unknown:
        msg = f"未知阶段：{unknown}（可选：{list(SUPPORTED_STAGES)}）"
        raise RuntimeError(msg)
    return stages


def require_orchestrated_isolation(stages: list[str], env: Mapping[str, str], *, allow_isolated_run: bool) -> None:
    """S11-07 统一隔离守卫：受守卫阶段未经编排隔离不得独立运行.

    受守卫的非自隔离阶段不自建隔离环境，独立调用会读写真实
    用户状态目录并可能连接常驻服务；在启动任何 GUI 之前整批拒绝，避免半程执行。
    经 acceptance.ps1 编排（或已手工布置同等隔离变量）时两个变量在场，不受影响；
    组件目录（Xberg 运行目录）允许为真实引擎目录，不做隔离检查。
    """
    guarded = [stage for stage in stages if stage in ISOLATION_GUARDED_STAGES]
    if not guarded or allow_isolated_run:
        return
    missing = missing_isolation_env(env)
    if not missing:
        return
    msg = (
        f"阶段 {','.join(guarded)} 独立运行会读写真实应用状态目录并可能连接常驻服务，"
        f"当前缺少隔离环境变量：{','.join(missing)}。"
        "请经 scripts/acceptance.ps1 执行（-WithGuiSmoke / -WithMarkdownAcceptance 会布置隔离"
        "状态目录与资产根）；--allow-isolated-run 仅跳过本项前置检查，公共启动入口仍要求隔离。"
    )
    raise RuntimeError(msg)


def run_dataset_stages(
    exe: str,
    data: Path,
    stages: list[str],
    *,
    run_case: Callable[[str, Callable[[], None]], None] | None = None,
) -> None:
    """S4/S2/S3 数据集阶段：每阶段前从旁路副本恢复，保证“干净语料上的完整链路”.

    H-06/附录 E：所选根的任一祖先直接含 .git 时两工具拒绝整次处理。仓库根本身
    带 .git，冒烟副本若继续放在仓库 .tmp/ 下会整次被拒（S4 确认框不再出现）。
    改放系统临时目录（AGENTS §2 允许的 tempfile 例外；语料源目录不受影响）。
    """
    scratch = Path(tempfile.mkdtemp(prefix="jchtools-gui-smoke-"))
    try:
        # 顺序 S4→S2→S3 固定（元组序即原顺序）；S3 为末段，随后的清理由
        # finally 的 scratch 删除兜底（循环末尾多出的一次 fresh 删除无输出、
        # 无异常，可观察行为不变）。
        fresh = scratch / "data"
        for stage, runner in (("S4", s4_full_extract), ("S2", s2_analyze_only), ("S3", s3_full_organize)):
            if stage not in stages:
                continue
            if run_case is None:
                _ = shutil.copytree(data, fresh)
                runner(str(exe), str(fresh))
                shutil.rmtree(fresh, ignore_errors=True)
            else:
                # 仅 CLI 报告路径计入复制与清理；直接调用仍无报告副作用。
                def invoke(runner: Callable[[str, str], None] = runner) -> None:
                    try:
                        _ = shutil.copytree(data, fresh)
                        runner(str(exe), str(fresh))
                    finally:
                        shutil.rmtree(fresh, ignore_errors=True)

                run_case(stage, invoke)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def ensure_smoke_target_is_disposable(data: Path) -> None:
    """整理/解压阶段会真实改动数据：只允许对一次性测试副本操作.

    仓库内仅放行 .tmp/（默认 .tmp/gui-smoke/data 就在这里）；用户主目录与
    盘符根一律拒绝。
    """
    repo = Path(__file__).resolve().parent.parent
    home = Path.home().resolve()
    under_repo_tmp = repo / ".tmp" in (data, *data.parents)
    if (data == repo or repo in data.parents) and not under_repo_tmp:
        msg = "拒绝在仓库目录内执行 GUI 整理冒烟（请使用 .tmp/gui-smoke/data 或其它副本）"
        raise RuntimeError(msg)
    if not under_repo_tmp and (data == home or home in data.parents):
        msg = "拒绝在用户主目录内执行 GUI 整理冒烟（请使用一次性测试副本）"
        raise RuntimeError(msg)
    # 盘符根：parts 只有 ('D:\\',) 一层；'D:\\foo' 是 2 层，不得误杀。
    if data.drive and len(data.parts) <= 1:
        msg = f"拒绝在盘符根目录执行整理冒烟：{data}"
        raise RuntimeError(msg)


class _GuiTiming:
    """CLI 专用报告生命周期；阶段函数的直接调用不产生报告."""

    def __init__(self, stages: list[str], stage_functions: dict[str, str]) -> None:
        self.stages: list[str] = stages
        self.stage_functions: dict[str, str] = stage_functions
        self.completed: set[str] = set()
        self.report: TimingReport | None = None
        self.report_failed: bool = False
        try:
            self.report = TimingReport(
                "gui_smoke",
                [sys.executable, *sys.argv],
                scope={
                    "selected_stages": stages,
                    "case_functions": {stage: stage_functions[stage] for stage in stages},
                },
                timing_basis=(
                    "单调时钟实测每个 S 阶段调用；数据集阶段包含夹具复制及成功/失败后的夹具清理；"
                    "共享临时根创建/删除、CLI 前置检查与报告写入不计入单例，运行墙钟另记"
                ),
                concurrency={"mode": "sequential", "workers": 1},
            )
        except Exception as error:  # noqa: BLE001  # 报告故障不改变原阶段执行。
            print(f"测试报告创建失败：{error}", file=sys.stderr)
            self.report_failed = True

    def record(self, stage: str, duration: float | None, status: str, detail: str = "") -> None:
        if self.report is None:
            return
        try:
            self.report.add_case(
                Case(
                    name=f"{stage}:{self.stage_functions[stage]}",
                    classname="scripts.gui_smoke",
                    duration=duration,
                    status=status,
                    detail=detail,
                    file="scripts/gui_smoke.py",
                )
            )
        except Exception as error:  # noqa: BLE001  # 保留原测试结果，报告故障另行可见。
            print(f"测试报告写入失败：{error}", file=sys.stderr)
            self.report_failed = True

    def run_case(self, stage: str, invoke: Callable[[], None]) -> None:
        started = time.perf_counter()
        try:
            invoke()
        except BaseException as error:
            duration = time.perf_counter() - started
            self.completed.add(stage)
            self.record(
                stage,
                duration,
                "failure" if isinstance(error, AssertionError) else "error",
                f"{type(error).__name__}: {error}",
            )
            raise
        else:
            duration = time.perf_counter() - started
            self.completed.add(stage)
            self.record(stage, duration, "passed")

    def finish(self, exit_code: int, completion: str) -> int:
        for stage in self.stages:
            if stage not in self.completed:
                self.record(stage, None, "skipped", "前一阶段失败或运行中断，本阶段未执行")
        exit_code = exit_code or int(self.report_failed)
        if self.report is not None:
            try:
                exit_code = self.report.finish(
                    exit_code,
                    completion=completion,
                    coverage={
                        "selected_stages": self.stages,
                        "completed_stages": [stage for stage in self.stage_functions if stage in self.completed],
                        "never_started_stages": [stage for stage in self.stages if stage not in self.completed],
                    },
                )
            except Exception as error:  # noqa: BLE001  # 不覆盖正在传播的阶段异常。
                print(f"测试报告收尾失败：{error}", file=sys.stderr)
                self.report_failed = True
        return exit_code or int(self.report_failed)


def _run_cli_stages(exe: Path, data: Path, stages: list[str]) -> int:
    """按原调度顺序执行，报告只包裹既有阶段边界."""
    stage_runners = {
        "S5": s5_markdown_basic_chain,
        "S6": s6_bundled_readiness_and_active_source,
        "S7": s7_restart_restores_active_runtime,
        "S8": s8_explicit_audio_download_cancel_and_retry,
        "S9": s9_missing_base_requires_install_repair,
        "S10": s10_output_layout_and_flat_duplicate_policy,
        "S11": s11_existing_results_are_skipped_untouched,
        "S12": s12_output_subtree_is_excluded_from_scan,
        "S13": s13_partial_failure_isolated_with_counts,
        "S14": s14_close_during_conversion_confirms_and_stops,
        "S15": s15_markdown_merge_and_split,
        "S16": s16_git_commit_and_push,
        "S17": s17_check_configured_components_offline,
        "S18": s18_layout_at_two_window_sizes,
    }
    stage_functions = {
        "S1": s1_launch_and_exit.__name__,
        "S4": s4_full_extract.__name__,
        "S2": s2_analyze_only.__name__,
        "S3": s3_full_organize.__name__,
        **{stage: runner.__name__ for stage, runner in stage_runners.items()},
    }
    timing = _GuiTiming(stages, stage_functions)
    exit_code = 0
    completion = "complete"
    try:
        if "S1" in stages:
            timing.run_case("S1", lambda: s1_launch_and_exit(str(exe)))
        run_dataset_stages(str(exe), data, stages, run_case=timing.run_case)
        for stage, runner in stage_runners.items():
            if stage in stages:
                timing.run_case(stage, lambda runner=runner: runner(str(exe)))
    except BaseException as error:
        exit_code = 130 if isinstance(error, KeyboardInterrupt) else 1
        if isinstance(error, SystemExit):
            exit_code = error.code if isinstance(error.code, int) else 1
        completion = "interrupted" if isinstance(error, (KeyboardInterrupt, SystemExit)) else "incomplete"
        raise
    finally:
        exit_code = timing.finish(exit_code, completion)
    return exit_code


def main() -> int:
    parser = argparse.ArgumentParser(description="JchTools GUI 冒烟测试")
    _ = parser.add_argument("--exe", default=DEFAULT_EXE)
    _ = parser.add_argument("--data", default=DEFAULT_DATA)
    _ = parser.add_argument(
        "--stages", default=DEFAULT_STAGES, help=f"逗号分隔阶段清单（可选：{','.join(SUPPORTED_STAGES)}）"
    )
    _ = parser.add_argument("--list-stages", action="store_true", help="逐行打印支持的阶段号后退出")
    _ = parser.add_argument(
        "--allow-isolated-run",
        action="store_true",
        help="跳过 CLI 编排前置检查；公共启动入口仍必须具有隔离状态目录与资产根（推荐 acceptance.ps1）",
    )
    args = parser.parse_args(namespace=_CliArgs())
    if args.list_stages:
        for stage in SUPPORTED_STAGES:
            print(stage)
        return 0
    stages = parse_stages(args.stages)
    # S11-07：受守卫阶段在启动任何 GUI 之前统一校验隔离环境，缺则拒绝并指引入口。
    require_orchestrated_isolation(stages, os.environ, allow_isolated_run=args.allow_isolated_run)
    exe = Path(args.exe).resolve()
    data = Path(args.data).resolve()
    if not exe.is_file():
        msg = f"找不到 {exe}"
        raise RuntimeError(msg)
    if not data.is_dir():
        msg = f"找不到测试数据目录 {data}"
        raise RuntimeError(msg)
    ensure_smoke_target_is_disposable(data)

    return _run_cli_stages(exe, data, stages)


if __name__ == "__main__":
    sys.exit(main())
