"""Windows 所属进程的低层启动原语，不依赖验收或门调度模块。."""

from __future__ import annotations

import ctypes
from ctypes import wintypes
from pathlib import Path
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


def process_argv(handle: int) -> tuple[str, ...]:
    """从已验证 Job 身份的句柄读取命令行，不按可复用 PID 查询 WMI。."""
    native_query = ctypes.WinDLL("ntdll").NtQueryInformationProcess
    native_query.argtypes = [wintypes.HANDLE, wintypes.ULONG, ctypes.c_void_p, wintypes.ULONG, ctypes.c_void_p]
    native_query.restype = wintypes.LONG
    query = cast("Callable[[int, int, object, int, object], int]", native_query)
    required = wintypes.ULONG()
    _ = query(handle, 60, None, 0, ctypes.byref(required))
    if not required.value:
        return ()
    buffer = ctypes.create_string_buffer(required.value)
    if query(handle, 60, buffer, len(buffer), ctypes.byref(required)) < 0:
        return ()

    @final
    class UnicodeString(ctypes.Structure):
        _fields_ = [("length", wintypes.USHORT), ("maximum", wintypes.USHORT), ("buffer", ctypes.c_void_p)]

    value = UnicodeString.from_buffer(buffer)
    address = cast("int | None", value.buffer)
    length = cast("int", value.length)
    if not address or not length:
        return ()
    command = ctypes.wstring_at(address, length // ctypes.sizeof(ctypes.c_wchar))
    split = ctypes.WinDLL("shell32").CommandLineToArgvW
    split.argtypes = [wintypes.LPCWSTR, ctypes.POINTER(ctypes.c_int)]
    split.restype = ctypes.c_void_p
    count = ctypes.c_int()
    split_command = cast("Callable[[str, object], int | None]", split)
    argv_address = split_command(command, ctypes.byref(count))
    if not argv_address:
        return ()
    arguments = (wintypes.LPWSTR * count.value).from_address(argv_address)
    try:
        return tuple(cast("str", arguments[index]) for index in range(count.value))
    finally:
        free = ctypes.WinDLL("kernel32").LocalFree
        free.argtypes = [ctypes.c_void_p]
        free.restype = ctypes.c_void_p
        release = cast("Callable[[object], int | None]", free)
        _ = release(argv_address)


def process_image(handle: int) -> Path:
    """从持有句柄查询实际映像，不按 PID 或命令行中的可伪装程序名判断。."""
    query = cast(
        "Callable[[int, int, object, object], int]",
        ctypes.WINFUNCTYPE(
            wintypes.BOOL,
            wintypes.HANDLE,
            wintypes.DWORD,
            wintypes.LPWSTR,
            ctypes.POINTER(wintypes.DWORD),
            use_last_error=True,
        )(("QueryFullProcessImageNameW", ctypes.WinDLL("kernel32", use_last_error=True))),
    )
    buffer = ctypes.create_unicode_buffer(32768)
    size = wintypes.DWORD(len(buffer))
    if not query(int(handle), 0, buffer, ctypes.byref(size)):
        raise ctypes.WinError(ctypes.get_last_error())
    return Path(cast("str", buffer.value))


def is_bazel_client(image: Path, argv: tuple[str, ...], *, output_base: Path, output_root: Path) -> bool:
    """仅识别精确工作区参数的原生 client/proxy；Java server 与未知程序永不匹配。."""
    return (
        image.name.lower() in {"bazel.exe", "bazelisk.exe"}
        and ("--batch" in argv or "--nobatch" in argv)
        and any(
            arg.startswith("--output_user_root=") and Path(arg.partition("=")[2]).resolve() == output_root
            for arg in argv
        )
        and any(
            arg.startswith("--output_base=") and Path(arg.partition("=")[2]).resolve() == output_base for arg in argv
        )
    )
