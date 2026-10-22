"""转 Markdown 验收共享 formats 能力查询（自 scripts/markdown_acceptance.py 原样搬移）.

A18/A25 只消费当前会话 broker 的 formats 响应：不直启引擎、不重试协议失败、
不回退静态清单；patch 目标按本模块定义处维护（Context.cached_formats 经
runner 模块绑定调用 _shared_format_rows）。
"""

from __future__ import annotations

import os
import subprocess
import time
import uuid
from pathlib import Path
from typing import TYPE_CHECKING

import pywintypes
import winerror

from scripts import isolation_probe
from scripts.gui_smoke import missing_isolation_env, start_owned_process
from scripts.markdown_acceptance_impl.broker import (
    BROKER_FORMATS_TIMEOUT,
    BROKER_PROTOCOL,
    MANDATORY_DOCUMENT_EXTENSIONS,
    _broker_pipe_identity,
    _connect_live_formats_pipe,
    _exchange_formats,
)
from scripts.markdown_acceptance_impl.gui import _resolve_gui
from scripts.markdown_acceptance_impl.items import ITEMS, _SYNTH_INPUT_EXTENSIONS
from scripts.markdown_acceptance_impl.model import (
    _is_str_obj_list,
    _is_str_obj_map,
    _isolated_environment_error,
    _str_field,
)

if TYPE_CHECKING:
    from _win32typing import PyHANDLE  # pyright: ignore[reportMissingModuleSource]  # 官方 pywin32 类型桩专用模块。

    from scripts.markdown_acceptance_impl.runner import Context


__all__ = [
    "_formats_request_inputs",
    "_matrix_input_extensions",
    "_open_formats_pipe",
    "_runtime_format_extensions",
    "_shared_format_rows",
    "xberg_format_extensions",
]


def _matrix_input_extensions() -> set[str]:
    """A25 只排除 A01-A24 已登记真实顶层夹具或合成输入的扩展名。."""
    covered: set[str] = set()
    for item in ITEMS:
        if item.group != "A" or item.item_id == "A25":
            continue
        extensions = {Path(name).suffix.lstrip(".").lower() for name in item.fixtures}
        covered.update(extensions)
        if item.synth:
            covered.update(_SYNTH_INPUT_EXTENSIONS[item.synth])
    return covered


def _open_formats_pipe(ctx: Context, state: str, deadline: float) -> PyHANDLE:
    pipe, digest = _broker_pipe_identity(state)
    launched = False
    while time.monotonic() < deadline:
        try:
            return _connect_live_formats_pipe(pipe)
        except pywintypes.error as error:
            if error.winerror not in (
                winerror.ERROR_FILE_NOT_FOUND,
                winerror.ERROR_PIPE_BUSY,
                winerror.ERROR_PIPE_NOT_CONNECTED,
                winerror.ERROR_NO_DATA,
            ):
                raise
            if error.winerror != winerror.ERROR_PIPE_BUSY and not launched:
                if (Path(state) / f"background-stopped-{digest}").exists():
                    message = "当前隔离会话已停止后台；formats 查询不得重新启动服务"
                    raise RuntimeError(message) from error
                exe, reason = _resolve_gui(ctx)
                if reason is not None or exe is None:
                    raise RuntimeError(reason or "缺少 JchTools GUI 可执行文件") from error
                root = Path(state)
                existing_logs = isolation_probe._isolation_log_snapshot(root)  # noqa: SLF001  # 固定共享隔离 API。
                proc, owner = start_owned_process(
                    [str(exe), "--xberg-broker"],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    creationflags=subprocess.CREATE_NO_WINDOW,
                )
                ctx.broker_process = proc
                ctx.broker_close = owner.close
                launched = True
                if not isolation_probe._wait_process_isolation(  # noqa: SLF001  # 固定共享隔离 API。
                    root, proc, "xberg-broker", existing_logs, deadline
                ):
                    message = "新启动 broker 未证明隔离 SQLite 与本轮 PID；请使用 test-hooks 构建"
                    raise RuntimeError(message) from error
        time.sleep(min(0.02, max(0.0, deadline - time.monotonic())))
    message = "连接共享 formats 管道超时"
    raise TimeoutError(message)


def _formats_request_inputs(ctx: Context) -> tuple[str, Path] | str:
    """准备现有 broker 请求的隔离状态与运行时输入，失败保留原具体诊断。."""
    missing = missing_isolation_env(os.environ)
    if missing:
        return f"共享 formats 查询缺少隔离环境：{missing}，拒绝启动"
    state = os.environ["JCHTOOLS_TEST_STATE_DIR"]
    environment_error = _isolated_environment_error()
    if environment_error is not None:
        return environment_error
    runtime = ctx.assets.runtime_dir
    if runtime is None:
        return "当前 Xberg 目录不可用，无法查询共享引擎能力"
    return state, runtime


def _shared_format_rows(ctx: Context) -> tuple[list[object], str | None]:
    """仅复用会话 broker 的 formats；不直启引擎，不重试协议失败、不回退静态清单。."""
    inputs = _formats_request_inputs(ctx)
    if isinstance(inputs, str):
        return [], inputs
    state, runtime = inputs
    deadline = time.monotonic() + BROKER_FORMATS_TIMEOUT
    request_id = f"acceptance-formats-{uuid.uuid4().hex}"
    try:
        pipe = _open_formats_pipe(ctx, state, deadline)
        try:
            response = _exchange_formats(pipe, runtime, request_id, deadline)
        finally:
            pipe.Close()
    except (OSError, RuntimeError, ValueError, pywintypes.error) as error:
        return [], f"共享 formats 查询失败：{error}"
    if (
        not _is_str_obj_map(response)
        or response.get("id") != request_id
        or response.get("jchtools_broker_protocol") != BROKER_PROTOCOL
    ):
        return [], "共享 formats 响应身份、协议或成功状态不可验证"
    if response.get("ok") is not True:
        error = response.get("error")
        detail = "共享 formats 响应身份、协议或成功状态不可验证"
        if isinstance(error, str) and error.strip():
            detail = f"共享 formats 查询失败：{error}"
        return [], detail
    rows = response.get("formats")
    if not _is_str_obj_list(rows):
        return [], "共享 formats 响应缺少 formats 数组"
    return rows, None


def _runtime_format_extensions(ctx: Context) -> tuple[set[str], str | None]:
    rows, error = ctx.cached_formats()
    if error is not None:
        return set(), error
    extensions: set[str] = set()
    for entry in rows:
        extension = _str_field(entry, "extension")
        mime = _str_field(entry, "mime_type")
        if extension is None or mime is None:
            return set(), "共享 formats 含不可解析条目，不据此声明覆盖"
        token = extension.strip().lstrip(".").lower()
        if not token or "." in token or not mime.strip():
            return set(), "共享 formats 含无效扩展名/MIME，不据此声明覆盖"
        extensions.add(token)
    missing = sorted(MANDATORY_DOCUMENT_EXTENSIONS - extensions)
    if missing:
        return set(), f"当前共享引擎未声明产品必需格式：{missing}，不能据旧清单声明就绪"
    return extensions, None


def xberg_format_extensions(ctx: Context) -> tuple[list[str], str | None]:
    """A25 枚举当前共享引擎能力，排除其他矩阵已登记的真实输入扩展名。."""
    extensions, error = _runtime_format_extensions(ctx)
    if error is not None:
        return [], error
    rows, _ = ctx.cached_formats()
    # A18 按其六种实际声明逐项验证；A25 只枚举其余文档/图片，不扩大媒体范围。
    for entry in rows:
        if (_str_field(entry, "mime_type") or "").startswith(("audio/", "video/")):
            token = (_str_field(entry, "extension") or "").strip().lstrip(".").lower()
            extensions.discard(token)
    return sorted(extensions - _matrix_input_extensions()), None
