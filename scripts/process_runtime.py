"""Windows 所属进程的低层启动原语，不依赖验收或门调度模块。."""

from __future__ import annotations

import ctypes
from typing import TYPE_CHECKING, cast, final

import win32api

if TYPE_CHECKING:
    from collections.abc import Callable

_ERROR_NO_MORE_FILES = 18


@final
class _ThreadEntry(ctypes.Structure):
    """Toolhelp 的 THREADENTRY32 布局，用于恢复尚未执行的所属初始线程。."""

    _fields_ = (
        ("size", ctypes.c_uint32),
        ("usage", ctypes.c_uint32),
        ("thread_id", ctypes.c_uint32),
        ("owner_pid", ctypes.c_uint32),
        ("priority", ctypes.c_int32),
        ("delta_priority", ctypes.c_int32),
        ("flags", ctypes.c_uint32),
    )

    def __init__(self) -> None:
        super().__init__()
        self.size: int = ctypes.sizeof(self)
        self.thread_id: int = 0
        self.owner_pid: int = 0


def resume_owned_process(pid: int) -> None:
    """进程先以 CREATE_SUSPENDED 启动并绑定 Job，之后才允许派生任何后代。."""
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    create_snapshot = cast(
        "Callable[[int, int], int]",
        ctypes.WINFUNCTYPE(ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, use_last_error=True)(
            ("CreateToolhelp32Snapshot", kernel)
        ),
    )
    signature = ctypes.WINFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p, use_last_error=True)
    first = cast("Callable[[int, object], int]", signature(("Thread32First", kernel)))
    next_entry = cast("Callable[[int, object], int]", signature(("Thread32Next", kernel)))
    open_thread = cast(
        "Callable[[int, int, int], int]",
        ctypes.WINFUNCTYPE(ctypes.c_void_p, ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32, use_last_error=True)(
            ("OpenThread", kernel)
        ),
    )
    resume_thread = cast(
        "Callable[[int], int]",
        ctypes.WINFUNCTYPE(ctypes.c_uint32, ctypes.c_void_p, use_last_error=True)(("ResumeThread", kernel)),
    )
    snapshot = create_snapshot(4, 0)  # TH32CS_SNAPTHREAD
    if snapshot == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        entry = _ThreadEntry()
        found = first(snapshot, ctypes.byref(entry))
        while found:
            if entry.owner_pid == pid:
                handle = open_thread(2, 0, entry.thread_id)  # THREAD_SUSPEND_RESUME
                if not handle:
                    raise ctypes.WinError(ctypes.get_last_error())
                try:
                    if resume_thread(handle) == ctypes.c_uint32(-1).value:
                        raise ctypes.WinError(ctypes.get_last_error())
                finally:
                    win32api.CloseHandle(handle)
                return
            found = next_entry(snapshot, ctypes.byref(entry))
        if ctypes.get_last_error() != _ERROR_NO_MORE_FILES:
            raise ctypes.WinError(ctypes.get_last_error())
        message = "所属暂停进程没有可恢复的初始线程"
        raise OSError(message)
    finally:
        win32api.CloseHandle(snapshot)
