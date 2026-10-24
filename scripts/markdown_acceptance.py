#!/usr/bin/env python3
"""转 Markdown 验收承接驱动器（openspec/specs/markdown-conversion/spec.md 附录 A · F26）.

定位与边界（如实声明，不得虚构）：
  本脚本只建立「承接入口」：把附录 A 的 26 项格式矩阵、界面 E2E 与双形态/环境前置
  落成逐项可执行的验收条目。真实 Xberg 运行时、媒体模型、被测 GUI 二进制或发布包
  缺失时，对应条目一律 NOT RUN 并列出所需前置，不得报为 PASS。未提供被测 GUI、
  隔离资产根及发布目录参数时，默认运行预期全部 NOT RUN、退出码 2。

入口形态结论（依据 T-04 与 src/main.rs）：转 Markdown 只提供 GUI 入口，仓库没有
  CLI 子命令，src/markdown.rs::run 是库函数而非公开入口。因此 A 组矩阵条目的
  「真实公开入口」唯一形态是 GUI（pywinauto 驱动）；B 组引用 scripts/gui_smoke.py
  的阶段；A18/A25 通过现有会话 broker 只读查询同一共享引擎的 formats，
  不单独启动 xberg.exe，不以仓库内固定清单代替实际能力；转换本身仍走 GUI。

用法：
    python scripts/markdown_acceptance.py --list
    python scripts/markdown_acceptance.py                                # 缺资产→全 NOT RUN
    python scripts/markdown_acceptance.py --only A,A24
    python scripts/markdown_acceptance.py --gui-exe target/debug/JchTools.exe \
        --portable-root <解包的便携目录> --installed-root <安装目录> \
        --json .tmp/markdown-acceptance/report.json

依赖：pywinauto / Pillow /（媒体合成另需 PATH 上的 ffmpeg），见 scripts/requirements-dev.txt。
验收隔离：资产根与 `JCHTOOLS_TEST_STATE_DIR` 必须位于仓库 `.tmp/` 下；脚本拒绝读取或
写入仓库外状态库/资产根，不自动猜测 `target/debug` 中可能陈旧的 GUI。被测 GUI 须由
`--gui-exe` 或 `JCHTOOLS_TEST_GUI_EXE` 显式指定。
语义验收按夹具构造、源图绑定和单成员失败阶段分层；媒体验收分别核对统计行与逐段正文，
各层保留独立真值、原阈值和首个失败原因，不以被测引擎输出替代源图。
诊断以同宿主、同成员的明确失败语义核对，不钉死引擎阶段标签；单图产物允许正文先于
图片引用，但仍独立核对源图像素，多图产物不跨图片寻找可抵账的引用。
WMF 栅格保真以原始记录和逻辑坐标的独立离屏 GDI 重放核对，不使用默认 DPI 的
WMF→EMF 渲染充当真值；仅采样画布取自候选尺寸，文字区域误差阈值保持不变。
围栏中的字面图片引用按 Markdown 围栏字符与长度排除；格式清点不改写条目登记表，
模块枚举线程失败如实列为未验证，不以线程异常丢失冒充未加载 Python。
formats 响应先验证请求身份与协议，再原样保留业务拒绝原因；失败不重发、不静态兜底。
退出码：任一 FAIL→1；任一 NOT RUN（包括未选择的条目）→2；全部 PASS→0；参数错误→3。
CLI 实际执行另自动写入独立 `.tmp/test-reports/<run-id>/junit.xml` 与 `run.json`；
仅所选条目进入单例报告，PASS/FAIL 使用实测 run_item 墙钟，NOT RUN 无时长。
原生 JSON/控制台固定全集分母保持不变；帮助、--list 与 --seed-state 不创建测试报告。

实现拆分：具体实现位于 scripts/markdown_acceptance_impl/ 包（paths/assets/broker/
cli/content/formats/gui/items/media_verify/model/preconditions/release_scan/
reporting/runner/synth）；本模块只保留 CLI 引导、逐项计时与全部原顶层名的 re-export，
保证 `markdown_acceptance.<名>` 与 patch 目标语义（函数读定义处模块全局）不变。
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import ntpath
import os
import re
import sqlite3
import struct
import subprocess
import sys
import threading
import time
import uuid
import zipfile
from pathlib import Path
from typing import TYPE_CHECKING, cast, override
from urllib.parse import unquote

import comtypes
import win32api
import win32con
import win32event
import win32file
import win32gui
import win32pipe
import win32ts
from defusedxml import ElementTree
from defusedxml.ElementTree import tostring
from PIL import Image, ImageChops, ImageDraw, ImageFont, ImageStat
from pywinauto import Application, controls, findbestmatch, findwindows, timings
from pywinauto.application import ProcessNotFoundError, WindowSpecification
from pywinauto.uia_defines import IUIA

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scripts import isolation_probe
from scripts.gui_smoke import (
    click,
)

# 测试经 markdown_acceptance.<名> 访问以下 re-export 与补丁锚点（含 getattr 动态取用）。
__all__ = [
    "A02_IMAGE_COUNT",
    "A02_IMAGE_TOKENS",
    "A07_IMAGE_COUNT",
    "A18_INPUT_EXTENSIONS",
    "A24_MEDIA_SYNTH_FILES",
    "ASSET_MANIFEST",
    "AVAILABLE",
    "BASE_COMPONENT_IDS",
    "BASE_REQUIRED",
    "BROKER_FORMATS_TIMEOUT",
    "BROKER_MAX_MESSAGE",
    "BROKER_PROTOCOL",
    "BROKER_READ_CHUNK",
    "BUSY_TIMEOUT",
    "COMMON_FORMATS",
    "COMPONENT_MANIFEST_NAME",
    "CONVERSION_TIMEOUT",
    "CONVERT_ROW_COUNT",
    "DATA_DIRECTORY",
    "DATA_URI_PATTERN",
    "DOCUMENT_MODEL_SUFFIXES",
    "EDIT_ROW_TOLERANCE_PX",
    "FIXTURES_DEFAULT",
    "GUI_SMOKE",
    "GUI_WINDOW_TIMEOUT",
    "IMAGE_SUFFIXES",
    "ITEMS",
    "IUIA",
    "LOCAL_TEST_XBERG_DIR",
    "MANDATORY_DOCUMENT_EXTENSIONS",
    "MARKDOWN_IMAGE_LINK_PATTERN",
    "MARKDOWN_INLINE_CODE_PATTERN",
    "MEDIA_REQUIRED",
    "OLD_PROJECT_DIR",
    "OPTIONAL_COMMON_ITEMS",
    "PHYSICAL_OVERLAP_THRESHOLD",
    "READINESS_TIMEOUT",
    "RECEIPT_NAME",
    "RELEASE_DOWNLOAD",
    "RELEASE_SOURCE",
    "ROOT",
    "SCRATCH_ROOT",
    "STATUS_FAILED",
    "STATUS_NOT_RUN",
    "STATUS_OK",
    "STATUS_OPTIONAL",
    "SYNTHESIZERS",
    "TEMP_LEFTOVER_MARKER",
    "TMP_ROOT",
    "TRANSIENT_ERRORS",
    "TYPE_CHECKING",
    "_A25_RULES",
    "_BROKEN_IMAGE_INPUTS",
    "_CONTENT_ASSERTS",
    "_DOCUMENT_IMAGE_BINDINGS",
    "_DOCUMENT_PREDICATE_FILES",
    "_DRAWING_NAMESPACE",
    "_DRAWING_REL_TYPE",
    "_EMBEDDED_FAILURE_STAGES",
    "_EMU_PER_PX",
    "_IGNORABLE_ATTRIBUTE",
    "_IMAGE_INK_LOOKUP",
    "_IMAGE_INK_THRESHOLD",
    "_IMAGE_MAX_INK_MEAN_ERROR",
    "_INPUT_RULES",
    "_ITEM_DEFAULT_RULES",
    "_MATRIX_COMMON",
    "_MAX_START_RECLICKS",
    "_MEDIA_CLOCK_RADIX",
    "_MEDIA_FAILURE_STAGES",
    "_MEDIA_SECTION_COUNT",
    "_MEMBER_FAILURE_STAGES",
    "_OFFICE_REL_NAMESPACE",
    "_OFFICE_VARIANTS",
    "_OPTIONAL_IMAGE_INPUTS",
    "_PKG_REL_TYPE_IMAGE",
    "_PPTX_REL_TYPE_IMAGE",
    "_RECLICK_INTERVAL_S",
    "_SKELETON_LAYOUT_REL_MISSING",
    "_SOURCE_CONTENT_ITEMS",
    "_SOURCE_TEXT_PART",
    "_SYNTH_INPUT_EXTENSIONS",
    "_TEXT_FENCE",
    "_VECTOR_SOURCE_SPECS",
    "_WMF_HEADER_BYTES",
    "_WMF_RECORD_HEADER_BYTES",
    "_WMF_RECORD_MIN_WORDS",
    "_WMF_WINDOW_EXTENT",
    "_WMF_WINDOW_ORIGIN",
    "_WORD_NAMESPACE",
    "_XLSX_REL_TYPE_IMAGE",
    "Application",
    "AssetProbe",
    "ContentSpec",
    "Context",
    "ElementTree",
    "GuiRun",
    "Image",
    "ImageChops",
    "ImageDraw",
    "ImageFont",
    "ImageStat",
    "InputRule",
    "Item",
    "Outcome",
    "PreparedInputs",
    "ProcessNotFoundError",
    "SynthResult",
    "WindowSpecification",
    "_ArgumentParser",
    "_Arguments",
    "_ReclickState",
    "_a18_supported_extensions",
    "_add_broken_ole_body",
    "_add_docx_body_relationships",
    "_add_docx_semantic_sources",
    "_assess_conversion",
    "_asset_precondition",
    "_asset_root",
    "_broker_pipe_identity",
    "_build_ole_preview_predicate",
    "_build_pptx",
    "_button_state",
    "_c03_delegate_stages",
    "_c03_normal_state_artifact",
    "_c05_assess",
    "_c05_scan_during_conversion",
    "_click_button",
    "_close_conversion_gui",
    "_collect_media_references",
    "_configure_conversion_paths",
    "_connect_live_formats_pipe",
    "_connect_window",
    "_content_problems",
    "_conversion_preflight_error",
    "_convert_directory_rows",
    "_delegate_stage",
    "_document_image_problems",
    "_document_semantic_problems",
    "_ensure_defaults",
    "_exchange_formats",
    "_expected_markdown_name",
    "_expected_markdown_names",
    "_fetch_xberg_directory",
    "_file_digest",
    "_finish_pipe_io",
    "_fixture_precondition",
    "_fixture_summary",
    "_formats_pipe_is_live",
    "_formats_request_inputs",
    "_gui_smoke_stages",
    "_image_binding_problems",
    "_image_point",
    "_input_rule",
    "_is_audio_path",
    "_is_str_obj_list",
    "_is_str_obj_map",
    "_isolated_environment_error",
    "_isolation_precondition",
    "_jbig2_semantic_problems",
    "_legacy_base_members",
    "_load_release_receipt",
    "_markdown_container_content",
    "_markdown_open_containers",
    "_mask_markdown_fences",
    "_matrix_input_extensions",
    "_media_audio_problem",
    "_media_markdown_problem",
    "_media_statistics",
    "_media_timestamp_ms",
    "_media_transcript_problem",
    "_member_problems",
    "_member_records",
    "_merge_component_members",
    "_mixed_docx_problems",
    "_old_cache_candidates",
    "_old_cache_hits",
    "_open_formats_pipe",
    "_parse_json",
    "_partial_stage_problems",
    "_per_input_problems",
    "_per_source_content_problems",
    "_pic_xml",
    "_plain_windows_path",
    "_prepare_scratch",
    "_print_report",
    "_probe_legacy_runtime_dir",
    "_probe_runtime_dir",
    "_reconfigure_stdout",
    "_release_component_members",
    "_release_component_records",
    "_release_members",
    "_request_close",
    "_resolve_gui",
    "_rewrite_content_types",
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
    "_runtime_format_extensions",
    "_same_image_content",
    "_same_image_pixels",
    "_same_physical_control",
    "_scene_marker_problem",
    "_select_items",
    "_set_row_edit",
    "_settings_state_dir",
    "_shared_format_rows",
    "_slide_rels_xml",
    "_slide_xml",
    "_source_content_spec",
    "_source_embedded_problems",
    "_source_input_spec",
    "_source_media_preserved",
    "_source_member_problems",
    "_source_wmf_with_bounds",
    "_str_field",
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
    "_tasklist",
    "_terminate",
    "_token_image_order_problems",
    "_token_png",
    "_validate_base_component_members",
    "_validate_release_manifest",
    "_validate_release_member_sets",
    "_verify_a02_content",
    "_verify_a18_unsupported_rejected",
    "_verify_a24_outputs",
    "_verify_a24_scenes",
    "_verify_a24_stop",
    "_verify_a24_tone",
    "_verify_a24_video",
    "_verify_media_references",
    "_wait_start_ready",
    "_window_texts",
    "_wmf_content_preserved",
    "_write_pnm_ascii",
    "_write_pnm_binary",
    "cast",
    "click",
    "comtypes",
    "controls",
    "drive_conversion",
    "findbestmatch",
    "findwindows",
    "hashlib",
    "io",
    "json",
    "manifest_has_python_entries",
    "ntpath",
    "override",
    "print_list",
    "probe_assets",
    "profile_items",
    "re",
    "render_wmf_gdi",
    "run_item",
    "scan_distribution",
    "scan_python_modules",
    "struct",
    "subprocess",
    "threading",
    "time",
    "timings",
    "tostring",
    "unique_visible_buttons",
    "unquote",
    "uuid",
    "verify_common_postconditions",
    "verify_svg_raster_media",
    "win32api",
    "win32con",
    "win32event",
    "win32file",
    "win32gui",
    "win32pipe",
    "win32ts",
    "xberg_format_extensions",
    "zipfile",
]

from scripts.markdown_acceptance_impl.assets import (
    BASE_COMPONENT_IDS,
    BASE_REQUIRED,
    COMPONENT_MANIFEST_NAME,
    DATA_DIRECTORY,
    DOCUMENT_MODEL_SUFFIXES,
    LOCAL_TEST_XBERG_DIR,
    MEDIA_REQUIRED,
    RECEIPT_NAME,
    RELEASE_DOWNLOAD,
    RELEASE_SOURCE,
    _asset_root,
    _fetch_xberg_directory,
    _plain_windows_path,
    _probe_legacy_runtime_dir,
    _probe_runtime_dir,
    _settings_state_dir,
)
from scripts.markdown_acceptance_impl.broker import (
    BROKER_FORMATS_TIMEOUT,
    BROKER_MAX_MESSAGE,
    BROKER_PROTOCOL,
    BROKER_READ_CHUNK,
    MANDATORY_DOCUMENT_EXTENSIONS,
    _broker_pipe_identity,
    _connect_live_formats_pipe,
    _exchange_formats,
    _finish_pipe_io,
    _formats_pipe_is_live,
)
from scripts.markdown_acceptance_impl.cli import _ArgumentParser, _Arguments, _select_items
from scripts.markdown_acceptance_impl.content import (
    _A25_RULES,
    _BROKEN_IMAGE_INPUTS,
    _CONTENT_ASSERTS,
    _DOCUMENT_IMAGE_BINDINGS,
    _EMBEDDED_FAILURE_STAGES,
    _IMAGE_INK_LOOKUP,
    _IMAGE_INK_THRESHOLD,
    _IMAGE_MAX_INK_MEAN_ERROR,
    _INPUT_RULES,
    _ITEM_DEFAULT_RULES,
    _MEDIA_FAILURE_STAGES,
    _MEMBER_FAILURE_STAGES,
    _OPTIONAL_IMAGE_INPUTS,
    _SOURCE_CONTENT_ITEMS,
    _SOURCE_TEXT_PART,
    _TEXT_FENCE,
    _VECTOR_SOURCE_SPECS,
    _WMF_HEADER_BYTES,
    _WMF_RECORD_HEADER_BYTES,
    _WMF_RECORD_MIN_WORDS,
    _WMF_WINDOW_EXTENT,
    _WMF_WINDOW_ORIGIN,
    DATA_URI_PATTERN,
    IMAGE_SUFFIXES,
    MARKDOWN_IMAGE_LINK_PATTERN,
    MARKDOWN_INLINE_CODE_PATTERN,
    TEMP_LEFTOVER_MARKER,
    _assess_conversion,
    _collect_media_references,
    _content_problems,
    _document_image_problems,
    _document_semantic_problems,
    _expected_markdown_name,
    _expected_markdown_names,
    _image_binding_problems,
    _image_point,
    _input_rule,
    _jbig2_semantic_problems,
    _markdown_container_content,
    _markdown_open_containers,
    _mask_markdown_fences,
    _mixed_docx_problems,
    _partial_stage_problems,
    _per_input_problems,
    _per_source_content_problems,
    _same_image_content,
    _same_image_pixels,
    _source_content_spec,
    _source_embedded_problems,
    _source_input_spec,
    _source_media_preserved,
    _source_member_problems,
    _source_wmf_with_bounds,
    _token_image_order_problems,
    _verify_media_references,
    _wmf_content_preserved,
    verify_common_postconditions,
)
from scripts.markdown_acceptance_impl.formats import (
    _formats_request_inputs,
    _matrix_input_extensions,
    _open_formats_pipe,
    _runtime_format_extensions,
    _shared_format_rows,
    xberg_format_extensions,
)
from scripts.markdown_acceptance_impl.gui import (
    _MAX_START_RECLICKS,
    _RECLICK_INTERVAL_S,
    BUSY_TIMEOUT,
    CONVERSION_TIMEOUT,
    CONVERT_ROW_COUNT,
    EDIT_ROW_TOLERANCE_PX,
    GUI_WINDOW_TIMEOUT,
    PHYSICAL_OVERLAP_THRESHOLD,
    READINESS_TIMEOUT,
    TRANSIENT_ERRORS,
    _button_state,
    _click_button,
    _close_conversion_gui,
    _configure_conversion_paths,
    _connect_window,
    _conversion_preflight_error,
    _convert_directory_rows,
    _isolation_precondition,
    _ReclickState,
    _request_close,
    _resolve_gui,
    _same_physical_control,
    _set_row_edit,
    _terminate,
    _wait_start_ready,
    _window_texts,
    drive_conversion,
    unique_visible_buttons,
)
from scripts.markdown_acceptance_impl.items import _MATRIX_COMMON, _SYNTH_INPUT_EXTENSIONS, AVAILABLE, ITEMS
from scripts.markdown_acceptance_impl.media_verify import (
    _MEDIA_CLOCK_RADIX,
    _MEDIA_SECTION_COUNT,
    A18_INPUT_EXTENSIONS,
    _media_audio_problem,
    _media_markdown_problem,
    _media_statistics,
    _media_timestamp_ms,
    _media_transcript_problem,
    _scene_marker_problem,
    _verify_a02_content,
    _verify_a18_unsupported_rejected,
    _verify_a24_scenes,
    _verify_a24_stop,
    _verify_a24_tone,
    _verify_a24_video,
    verify_svg_raster_media,
)
from scripts.markdown_acceptance_impl.model import (
    OLD_PROJECT_DIR,
    STATUS_FAILED,
    STATUS_NOT_RUN,
    STATUS_OK,
    STATUS_OPTIONAL,
    AssetProbe,
    ContentSpec,
    GuiRun,
    InputRule,
    Item,
    Outcome,
    PreparedInputs,
    SynthResult,
    _file_digest,
    _is_str_obj_list,
    _is_str_obj_map,
    _isolated_environment_error,
    _old_cache_candidates,
    _reconfigure_stdout,
    _str_field,
)
from scripts.markdown_acceptance_impl.paths import (
    ASSET_MANIFEST,
    FIXTURES_DEFAULT,
    GUI_SMOKE,
    ROOT,
    SCRATCH_ROOT,
    TMP_ROOT,
)
from scripts.markdown_acceptance_impl.preconditions import _c03_normal_state_artifact, _fixture_precondition
from scripts.markdown_acceptance_impl.release_scan import (
    _c05_assess,
    _is_audio_path,
    _legacy_base_members,
    _load_release_receipt,
    _member_problems,
    _member_records,
    _merge_component_members,
    _old_cache_hits,
    _parse_json,
    _release_component_members,
    _release_component_records,
    _release_members,
    _tasklist,
    _validate_base_component_members,
    _validate_release_manifest,
    _validate_release_member_sets,
    manifest_has_python_entries,
    probe_assets,
    scan_distribution,
    scan_python_modules,
)
from scripts.markdown_acceptance_impl.reporting import (
    COMMON_FORMATS,
    OPTIONAL_COMMON_ITEMS,
    _fixture_summary,
    _print_report,
    print_list,
    profile_items,
)
from scripts.markdown_acceptance_impl.runner import (
    Context,
    _a18_supported_extensions,
    _asset_precondition,
    _c03_delegate_stages,
    _c05_scan_during_conversion,
    _delegate_stage,
    _gui_smoke_stages,
    _prepare_scratch,
    _run_c01_installed_scan,
    _run_c02_portable_scan,
    _run_c03_unconfigured,
    _run_c05_no_python,
    _run_c06_old_dir,
    _run_c07_old_cache,
    _run_c08_c09,
    _run_common_mp4,
    _run_common_old_office,
    _run_content_assert,
    _run_conversion_item,
    _run_env,
    _run_gui_ref,
    _run_matrix,
    _run_matrix_a02,
    _run_matrix_a18,
    _run_matrix_a24,
    _run_matrix_a25,
    _verify_a24_outputs,
    run_item,
)
from scripts.markdown_acceptance_impl.synth import (
    _DOCUMENT_PREDICATE_FILES,
    _DRAWING_NAMESPACE,
    _DRAWING_REL_TYPE,
    _EMU_PER_PX,
    _IGNORABLE_ATTRIBUTE,
    _OFFICE_REL_NAMESPACE,
    _OFFICE_VARIANTS,
    _PKG_REL_TYPE_IMAGE,
    _PPTX_REL_TYPE_IMAGE,
    _SKELETON_LAYOUT_REL_MISSING,
    _WORD_NAMESPACE,
    _XLSX_REL_TYPE_IMAGE,
    A02_IMAGE_COUNT,
    A02_IMAGE_TOKENS,
    A07_IMAGE_COUNT,
    A24_MEDIA_SYNTH_FILES,
    SYNTHESIZERS,
    _add_broken_ole_body,
    _add_docx_body_relationships,
    _add_docx_semantic_sources,
    _build_ole_preview_predicate,
    _build_pptx,
    _ensure_defaults,
    _pic_xml,
    _rewrite_content_types,
    _slide_rels_xml,
    _slide_xml,
    _synth_document_predicates,
    _synth_images,
    _synth_jbig2_predicates,
    _synth_media,
    _synth_office,
    _synth_pnm,
    _synth_pptx_descr_no_ocr,
    _synth_pptx_multi_images,
    _synth_pptx_runs_fields,
    _synth_pptx_shared_media,
    _synth_pptx_svg,
    _synth_pptx_two_png_order,
    _synth_pptx_undecodable_image,
    _synth_precondition,
    _synth_xlsx_drawing_order,
    _token_png,
    _write_pnm_ascii,
    _write_pnm_binary,
)
from scripts.test_reports import Case, TimingReport
from tests.markdown_fixtures.generate_synthetic import render_wmf_gdi


def _seed_isolated_state(state_root: Path, xberg_root: Path) -> None:
    """播种旧来源配置供真实最新版引擎验收；不恢复已移除的产品手工目录入口."""
    if not state_root.is_absolute() or not xberg_root.is_absolute():
        message = "隔离状态目录与 Xberg 目录必须是绝对路径"
        raise ValueError(message)
    state_root = state_root.resolve()
    xberg_root = xberg_root.resolve()
    if not isolation_probe._is_under_tmp(state_root):  # noqa: SLF001  # 固定共享隔离 API。
        message = f"隔离状态目录必须位于仓库 .tmp/ 下，拒绝写入：{state_root}"
        raise ValueError(message)
    if not (xberg_root / "xberg.exe").is_file():
        message = f"隔离 Xberg 目录缺少 xberg.exe：{xberg_root}"
        raise ValueError(message)
    database = state_root / "config.sqlite3"
    if not isolation_probe._is_under_tmp(database):  # noqa: SLF001  # 固定共享隔离 API。
        message = f"隔离设置库目标必须位于仓库 .tmp/ 下，拒绝写入：{database}"
        raise ValueError(message)
    schema = ROOT / "crates" / "foundation" / "src" / "app_settings.sql"
    state_root.mkdir(parents=True, exist_ok=True)
    with contextlib.closing(sqlite3.connect(database)) as connection:
        _ = connection.executescript(schema.read_text(encoding="utf-8"))
        update_sql = """INSERT INTO app_settings(key,value) VALUES('xberg_directory',?) ON CONFLICT(key)
DO UPDATE SET value=excluded.value"""
        _ = connection.execute(update_sql, (str(xberg_root),))
        custom_sql = """INSERT INTO app_settings(key,value) VALUES('xberg_custom_directory',?) ON CONFLICT(key)
DO UPDATE SET value=excluded.value"""
        _ = connection.execute(custom_sql, (str(xberg_root),))
        source_sql = """INSERT INTO app_settings(key,value) VALUES('xberg_source','custom') ON CONFLICT(key)
DO UPDATE SET value=excluded.value"""
        _ = connection.execute(source_sql)
        _ = connection.execute("DELETE FROM app_settings WHERE key='xberg_downloaded_directory'")
        connection.commit()


def _cli_execution_context(args: _Arguments, parser: _ArgumentParser) -> tuple[Context, Path | None]:
    """准备 CLI 执行环境和报告路径，保留参数错误及路径隔离边界。."""
    environment_error = _isolated_environment_error()
    if environment_error is not None:
        parser.error(environment_error)
    if args.json_report:
        try:
            report_path = Path(args.json_report).resolve()
        except (OSError, RuntimeError, ValueError) as error:
            parser.error(f"--json 输出路径无效：{error}")
        if not isolation_probe._is_under_tmp(report_path):  # noqa: SLF001  # 固定共享隔离 API。
            parser.error(f"--json 输出路径必须位于仓库 .tmp/ 下，拒绝写入：{report_path}")
    else:
        report_path = None
    explicit_gui = os.environ.get("JCHTOOLS_TEST_GUI_EXE")
    gui_source = args.gui_exe or explicit_gui
    ctx = Context(
        Path(gui_source).resolve() if gui_source else None,
        Path(args.installed_root).resolve() if args.installed_root else None,
        Path(args.portable_root).resolve() if args.portable_root else None,
        Path(args.fixtures).resolve() if args.fixtures else FIXTURES_DEFAULT,
        probe_assets(),
    )
    return ctx, report_path


class _MarkdownTiming:
    """CLI 专用逐项计时及报告；不改变 run_item 的异常转换语义."""

    def __init__(self, selected: list[Item], profile: str, selector: str) -> None:
        self.selected: list[Item] = selected
        self.profile: str = profile
        self.completed: set[str] = set()
        self.not_run: set[str] = set()
        self.report: TimingReport | None = None
        self.report_failed: bool = False
        try:
            self.report = TimingReport(
                "markdown_acceptance",
                [sys.executable, *sys.argv],
                scope={
                    "profile": profile,
                    "selector": selector,
                    "selected_items": [{"id": item.item_id, "group": item.group} for item in selected],
                    "native_report_denominator": len(ITEMS),
                    "unselected_items_are_not_executed": True,
                },
                timing_basis=(
                    "单调时钟实测 CLI 中每个所选条目的 run_item 调用，包含其前置探测、夹具准备、"
                    "GUI/模型执行及条目内清理；NOT RUN 无执行时长；共享 broker 收尾与报告写入"
                    "不计入单例，运行墙钟另记"
                ),
                concurrency={"mode": "sequential", "workers": 1},
            )
        except Exception as error:  # noqa: BLE001  # 报告故障不改变全体所选条目的原有执行。
            print(f"测试报告创建失败：{error}", file=sys.stderr)
            self.report_failed = True

    def record(self, item: Item, duration: float | None, status: str, detail: str) -> None:
        if self.report is None:
            return
        try:
            self.report.add_case(
                Case(
                    name=item.item_id,
                    classname=f"scripts.markdown_acceptance.{self.profile}.{item.group}",
                    duration=duration,
                    status=status,
                    detail=detail,
                    file="scripts/markdown_acceptance.py",
                )
            )
        except Exception as error:  # noqa: BLE001  # 原生结论保留，报告失败另行可见。
            print(f"测试报告写入失败：{error}", file=sys.stderr)
            self.report_failed = True

    def run_item(self, item: Item, ctx: Context) -> Outcome:
        started = time.perf_counter()
        try:
            outcome = run_item(item, ctx)
        except BaseException as error:
            duration = time.perf_counter() - started
            self.completed.add(item.item_id)
            self.record(item, duration, "error", f"{type(error).__name__}: {error}")
            raise
        duration = time.perf_counter() - started
        self.completed.add(item.item_id)
        status = {
            STATUS_OK: "passed",
            STATUS_FAILED: "failure",
            STATUS_NOT_RUN: "skipped",
            STATUS_OPTIONAL: "skipped",
        }[outcome.status]
        if status == "skipped":
            self.not_run.add(item.item_id)
        self.record(
            item,
            None if status == "skipped" else duration,
            status,
            "\n".join([outcome.reason, *outcome.details]),
        )
        return outcome

    def finish(self, exit_code: int, completion: str) -> int:
        for item in self.selected:
            if item.item_id not in self.completed:
                self.record(item, None, "skipped", "运行中断，本条目未执行")
        exit_code = exit_code or int(self.report_failed)
        if self.not_run and completion == "complete":
            completion = "partial"
        if self.report is not None:
            try:
                exit_code = self.report.finish(exit_code, completion=completion, coverage=self._coverage())
            except Exception as error:  # noqa: BLE001  # 不覆盖验收或 broker 清理原有异常。
                print(f"测试报告收尾失败：{error}", file=sys.stderr)
                self.report_failed = True
        return exit_code or int(self.report_failed)

    def _coverage(self) -> dict[str, list[str]]:
        return {
            "selected_items": [item.item_id for item in self.selected],
            "dispatch_completed_items": [item.item_id for item in self.selected if item.item_id in self.completed],
            "executed_items": [
                item.item_id
                for item in self.selected
                if item.item_id in self.completed and item.item_id not in self.not_run
            ],
            "selected_not_run_items": [item.item_id for item in self.selected if item.item_id in self.not_run],
            "never_started_items": [item.item_id for item in self.selected if item.item_id not in self.completed],
        }


def _close_context_broker(ctx: Context) -> None:
    """保留原 CLI 对本轮 broker 的精确回收逻辑."""
    if ctx.broker_close is not None:
        ctx.broker_close()
    elif ctx.broker_process is not None and ctx.broker_process.poll() is None:
        # Job 归属建立失败时尚未发送 formats；只回收自己刚启动的精确进程句柄。
        ctx.broker_process.kill()
        _ = ctx.broker_process.wait(timeout=15.0)
    if ctx.broker_process is not None:
        _ = ctx.broker_process.poll()


def _execute_acceptance(selected: list[Item], ctx: Context, report_path: Path | None, selector: str) -> int:
    """执行全部所选条目，原生汇总和共享报告保持各自口径."""
    timing = _MarkdownTiming(selected, ctx.profile, selector)
    exit_code = 0
    completion = "complete"
    try:
        try:
            results = [(item, timing.run_item(item, ctx)) for item in selected]
            exit_code = _print_report(results, report_path, ctx.profile)
        finally:
            _close_context_broker(ctx)
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
    _reconfigure_stdout()
    description = "转 Markdown 验收承接驱动器（附录 A；缺资产一律 NOT RUN，不虚构 PASS）"
    parser = _ArgumentParser(description=description)
    _ = parser.add_argument("--list", dest="list_only", action="store_true", help="列出全部条目与夹具/资产映射，不执行")
    _ = parser.add_argument(
        "--profile", choices=("common", "full"), default="common", help="默认常用格式；full 保留原扩展矩阵"
    )
    _ = parser.add_argument("--only", default="", help="只执行指定组（A/B/C）或条目（如 A24），逗号分隔")
    _ = parser.add_argument(
        "--gui-exe",
        default="",
        help="被测 JchTools.exe（必须显式提供，或由 JCHTOOLS_TEST_GUI_EXE 指定）",
    )
    _ = parser.add_argument("--portable-root", default="", help="解包的便携版目录（含 JchTools.exe）")
    _ = parser.add_argument("--installed-root", default="", help="安装版目录（默认形态为安装器写入的位置）")
    _ = parser.add_argument("--fixtures", default="", help="夹具目录（默认 tests/markdown_fixtures）")
    _ = parser.add_argument("--json", dest="json_report", default="", help="JSON 报告输出路径（必须位于仓库 .tmp/）")
    _ = parser.add_argument("--seed-state", default="", help=argparse.SUPPRESS)
    _ = parser.add_argument("--seed-xberg", default="", help=argparse.SUPPRESS)
    args = parser.parse_args(namespace=_Arguments())
    if bool(args.seed_state) != bool(args.seed_xberg):
        parser.error("--seed-state 与 --seed-xberg 必须成对提供")
    if args.seed_state:
        try:
            _seed_isolated_state(Path(args.seed_state), Path(args.seed_xberg))
        except (OSError, RuntimeError, ValueError, sqlite3.Error) as error:
            parser.error(str(error))
        return 0
    if args.list_only:
        return print_list()
    selected, error = _select_items(args.only)
    if error is not None:
        print(error)
        return 3
    ctx, report_path = _cli_execution_context(args, parser)
    ctx.profile = args.profile
    selected_ids = {item.item_id for item in selected}
    selected = [
        item
        for item in profile_items(args.profile)
        if item.item_id in selected_ids and not (args.profile == "common" and item.item_id in OPTIONAL_COMMON_ITEMS)
    ]
    SCRATCH_ROOT.mkdir(parents=True, exist_ok=True)
    return _execute_acceptance(selected, ctx, report_path, args.only)


if __name__ == "__main__":
    sys.exit(main())
