"""转 Markdown 验收共享 broker formats 管道客户端（自 scripts/markdown_acceptance.py 原样搬移）."""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import time
from typing import TYPE_CHECKING, cast

import pywintypes
import win32api
import win32con
import win32event
import win32file
import win32pipe
import win32ts
import winerror

from scripts.markdown_acceptance_impl.release_scan import _parse_json

if TYPE_CHECKING:
    from collections.abc import Buffer, Callable
    from pathlib import Path

    from _win32typing import (  # pyright: ignore[reportMissingModuleSource]  # 官方 pywin32 类型桩专用模块。
        PyHANDLE,
        PyOVERLAPPED,
    )

__all__ = [
    "BROKER_FORMATS_TIMEOUT",
    "BROKER_MAX_MESSAGE",
    "BROKER_PROTOCOL",
    "BROKER_READ_CHUNK",
    "MANDATORY_DOCUMENT_EXTENSIONS",
    "_broker_pipe_identity",
    "_connect_live_formats_pipe",
    "_exchange_formats",
    "_finish_pipe_io",
    "_formats_pipe_is_live",
]


BROKER_FORMATS_TIMEOUT = 15.0


BROKER_PROTOCOL = 3


BROKER_READ_CHUNK = 64 * 1024


BROKER_MAX_MESSAGE = 512 * 1024 * 1024


MANDATORY_DOCUMENT_EXTENSIONS = frozenset(("pdf", "docx", "pptx", "xlsx"))


def _broker_pipe_identity(state: str) -> tuple[str, str]:
    """与 xberg_runtime_windows::Identity 共用原始状态路径和当前 Windows 会话身份。."""
    session_query = cast("Callable[[int], int]", vars(win32ts)["ProcessIdToSessionId"])
    digest = hashlib.sha256(f"{state}:{session_query(os.getpid())}".encode()).hexdigest()
    return rf"\\.\pipe\jchtools-xberg-{digest}", digest


def _formats_pipe_is_live(pipe: PyHANDLE) -> bool:
    """仅接受仍存活的代理连接；上个 GUI Job 的关闭端点不是新请求连接。."""
    try:
        server_query = cast("Callable[[int], int]", vars(win32pipe)["GetNamedPipeServerProcessId"])
        server_pid = server_query(pipe.handle)
        process = win32api.OpenProcess(win32con.SYNCHRONIZE, 0, server_pid)
    except pywintypes.error as error:
        if error.winerror in (
            winerror.ERROR_BROKEN_PIPE,
            winerror.ERROR_PIPE_NOT_CONNECTED,
            winerror.ERROR_NO_DATA,
            winerror.ERROR_INVALID_PARAMETER,
        ):
            return False
        raise
    try:
        status = win32event.WaitForSingleObject(process, 0)
        if status == win32event.WAIT_OBJECT_0:
            return False
        if status != win32event.WAIT_TIMEOUT:
            message = "无法核验共享 formats 管道的代理进程状态"
            raise RuntimeError(message)
        return True
    finally:
        win32api.CloseHandle(process)


def _connect_live_formats_pipe(name: str) -> PyHANDLE:
    candidate = win32file.CreateFile(
        name,
        win32con.GENERIC_READ | win32con.GENERIC_WRITE,
        0,
        None,
        win32con.OPEN_EXISTING,
        win32con.FILE_FLAG_OVERLAPPED,
        None,
    )
    accepted = False
    try:
        accepted = _formats_pipe_is_live(candidate)
        if accepted:
            return candidate
    finally:
        if not accepted:
            candidate.Close()
    # 尚未发送协议请求；连接层仅排除已经退出的旧代理。
    raise pywintypes.error(winerror.ERROR_PIPE_NOT_CONNECTED, "formats connect", "旧代理已经退出")


def _finish_pipe_io(handle: int, overlap: PyOVERLAPPED, status: int, deadline: float) -> int:
    if status == winerror.ERROR_IO_PENDING:
        remaining = max(0, int((deadline - time.monotonic()) * 1000))
        if win32event.WaitForSingleObject(overlap.hEvent, remaining) != win32event.WAIT_OBJECT_0:
            # 所有 I/O 都由本线程在独立连接发起；CancelIo 不会取消其他连接/线程。
            win32file.CancelIo(handle)
            # 取消必须到达终态才释放 OVERLAPPED/read buffer，不能遗留后台 I/O。
            with contextlib.suppress(pywintypes.error):
                _ = win32file.GetOverlappedResult(handle, overlap, 1)
            message = "共享 formats 读写超时"
            raise TimeoutError(message)
    return win32file.GetOverlappedResult(handle, overlap, 0)


def _exchange_formats(pipe: PyHANDLE, runtime: Path, request_id: str, deadline: float) -> object:
    root = str(runtime.resolve(strict=True))
    if not root.startswith("\\\\?\\"):
        root = "\\\\?\\UNC\\" + root[2:] if root.startswith("\\\\") else "\\\\?\\" + root
    envelope = {
        "runtime_dir": root,
        "request": {"id": request_id, "command": "formats", "timeout_ms": int(BROKER_FORMATS_TIMEOUT * 1000)},
    }
    payload = (json.dumps(envelope, ensure_ascii=False) + "\n").encode("utf-8")
    event = win32event.CreateEvent(None, 1, 0, None)
    try:
        if time.monotonic() >= deadline:
            message = "共享 formats 请求期限已到"
            raise TimeoutError(message)
        overlap = pywintypes.OVERLAPPED()
        overlap.hEvent = int(event)
        status, _ = win32file.WriteFile(pipe.handle, payload, overlap)
        if _finish_pipe_io(pipe.handle, overlap, status, deadline) != len(payload):
            message = "共享 formats 请求未完整发送"
            raise RuntimeError(message)
        response = bytearray()
        while time.monotonic() < deadline:
            win32event.ResetEvent(int(event))
            overlap = pywintypes.OVERLAPPED()
            overlap.hEvent = int(event)
            buffer = win32file.AllocateReadBuffer(BROKER_READ_CHUNK)
            status, _ = win32file.ReadFile(pipe.handle, buffer, overlap)
            count = _finish_pipe_io(pipe.handle, overlap, status, deadline)
            if not count:
                message = "共享 formats 管道未返回完整 JSON 行"
                raise RuntimeError(message)
            view = memoryview(cast("Buffer", cast("object", buffer)))
            response.extend(view[:count])
            if len(response) > BROKER_MAX_MESSAGE:
                message = "共享 formats 响应超过 broker 消息边界"
                raise RuntimeError(message)
            newline = response.find(b"\n")
            if newline >= 0:
                return _parse_json(response[:newline].decode("utf-8"))
        message = "共享 formats 响应超时"
        raise TimeoutError(message)
    finally:
        win32api.CloseHandle(event)
