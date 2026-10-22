"""门汇总、状态常量与监督辅助原语（从 test_gate 根模块拆出）.

本模块是无状态叶子：状态常量、阶段结果与汇总打印、acceptance 覆盖缺口
提取、binding loop 扫描、PowerShell 5.1 环境与版本行、独立监督器的进度
发布与观察辅助。不回读 scripts.test_gate，`python scripts/test_gate.py`
与 `python -m scripts.bazel_gate` 的双重执行语义不受影响；可补丁接缝
（_monotonic、_COMMAND_JOBS、execute_gate、LOG_DIR、ROOT 等）保留在
根模块，根模块按调用点绑定本模块的实现。
"""

from __future__ import annotations

import contextlib
import ctypes
import dataclasses
import io
import os
import re
import shutil
import struct
import subprocess
import sys
import time
from ctypes import wintypes
from typing import TYPE_CHECKING, cast

import pywintypes
import win32api
import win32con
import win32event
import winerror

from scripts.bazel_service import ServerPidPendingError
from scripts.gate_runtime import Result

if TYPE_CHECKING:
    from collections.abc import Callable
    from mmap import mmap
    from pathlib import Path

    from scripts.bazel_gate import BazelSession
    from scripts.bazel_service import ServiceLease
    from scripts.gate_runtime import GateRun
    from scripts.test_gate import _CommandJob, _GateSupervision


SERVER_PID_PUBLICATION_SECONDS = 2.0


BINDING_LOOP_SAMPLE_LIMIT = 6


# 常量名避开 pass 字样（S105 会把含 pass 的变量名当作疑似硬编码口令）。
STATUS_OK = "PASS"
STATUS_FAILED = "FAIL"
STATUS_TIMED_OUT = "TIMEOUT"
STATUS_UNVERIFIED = "UNVERIFIED"
STATUS_NOT_RUN = "NOT RUN"
ERROR_INVALID_PARAMETER = 87

_WORKER_OK = 1
_WORKER_FAILED = 2
_WORKER_TIMED_OUT = 3

# 独立旧桌面验收的覆盖判据保持原样；这些驱动不属于三个门的鼠标隔离范围。
ENVIRONMENT_BLOCKED_ACCEPTANCE_ITEMS: frozenset[str] = frozenset({"C08", "C09"})
# 从 acceptance 汇总行提取未执行条目号（acceptance.ps1 在汇总行携带条目号清单；
# 逐项行与汇总行两种形态都按「字母+两位数字」在行内任意位置提取）。
_NOT_RUN_ITEM_ID_PATTERN = re.compile(r"\b([A-Z]\d{2})\b")


@dataclasses.dataclass
class StageResult:
    """单个阶段的执行结果；log 为该阶段完整输出落盘位置（无则 None）."""

    name: str
    status: str
    detail: str
    log: Path | None = None


def _reconfigure_stdout() -> None:
    # 阶段明细含中文；Windows 控制台默认代码页会把 print 变成 UnicodeEncodeError。
    with contextlib.suppress(AttributeError):
        stream = sys.stdout
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")


def _wait_process_handles(handles: list[int], deadline: float) -> bool:
    # Job 活动计数归零并不等于所有进程句柄已经发出终态信号，仍须等待持有句柄。
    return all(
        win32event.WaitForSingleObject(handle, max(0, int((deadline - time.monotonic()) * 1000)))
        == win32event.WAIT_OBJECT_0
        for handle in handles
    )


def _process_is_running(pid: int) -> bool:
    inherit_handle = False
    try:
        handle = win32api.OpenProcess(win32con.SYNCHRONIZE, inherit_handle, pid)
    except pywintypes.error as error:
        if error.winerror != ERROR_INVALID_PARAMETER:
            raise
        return False
    else:
        try:
            return win32event.WaitForSingleObject(handle, 0) != win32event.WAIT_OBJECT_0
        finally:
            win32api.CloseHandle(handle)


def _await_server_pid_publication(
    owner: _CommandJob,
    proc: subprocess.Popen[bytes],
    requests: Path,
    error: pywintypes.error | ServerPidPendingError,
    *,
    completed: bool,
) -> None:
    sharing = isinstance(error, pywintypes.error) and error.winerror == winerror.ERROR_SHARING_VIOLATION
    # 只允许首次客户端启动期间的独占写/空发布；拒绝 ACL 拒绝及完成后的未知归属。
    publishing = (
        (sharing or isinstance(error, ServerPidPendingError))
        and not completed
        and any(requests.glob("*.request.json"))
        and proc.poll() is None
    )
    if not publishing:
        raise error
    now = time.monotonic()
    if owner.pid_publication_started is None:
        owner.pid_publication_started = now
    if now - owner.pid_publication_started >= SERVER_PID_PUBLICATION_SECONDS:
        message = "Bazel server PID 发布超时，无法证明 Job 归属"
        raise RuntimeError(message) from error


def _tail(log: Path, limit: int = 12) -> str:
    lines = log.read_text(encoding="utf-8", errors="replace").splitlines()
    return "\n".join(lines[-limit:])


def _scan_binding_loop(result: StageResult) -> StageResult:
    # AGENTS.md 第 4 节禁止布局绑定环；acceptance.ps1 对 cargo test 输出做同样扫描。
    if result.status != STATUS_OK or result.log is None:
        return result
    text = result.log.read_text(encoding="utf-8", errors="replace")
    hits = [line.strip() for line in text.splitlines() if "binding loop" in line]
    if not hits:
        return result
    detail = "构建输出出现 binding loop 警告（AGENTS.md 第 4 节禁止）：\n" + "\n".join(hits[:BINDING_LOOP_SAMPLE_LIMIT])
    return StageResult(result.name, STATUS_FAILED, detail, result.log)


def _validate_gate_result(result: Result) -> Result:
    checked = _scan_binding_loop(StageResult(result.name, result.status, result.detail, result.log))
    result.status, result.detail = checked.status, checked.detail
    return result


def acceptance_coverage_gaps(text: str) -> list[str]:
    """提取 acceptance 汇总中的必要覆盖缺口；package 的 NOT RUN 不在本级范围.

    markdown-acceptance 的 NOT RUN 仅在可提取条目号、且条目号全部属于环境受限
    集合（C08/C09，须独立 Windows 用户会话驱动正式包 GUI；AGENTS.md 3.4 例外）
    时放行；PARTIAL 始终阻塞。提取不到条目号一律按缺口处理（fail-closed），
    放行不改变条目本身的 NOT RUN 事实，不表述为已验证。
    """
    required = ("markdown-acceptance", "snap-ocr-worker-root")
    gaps: list[str] = []
    lines = text.splitlines()
    for name in required:
        summaries = [line.strip() for line in lines if name in line and line.strip()]
        if not summaries:
            gaps.append(f"缺少必要覆盖汇总：{name}")
            continue
        blocking = [line for line in summaries if line.startswith(("NOT RUN ", "PARTIAL "))]
        if not blocking:
            continue
        if name == "markdown-acceptance" and all(line.startswith("NOT RUN ") for line in blocking):
            item_ids: set[str] = set()
            for line in blocking:
                item_ids.update(_NOT_RUN_ITEM_ID_PATTERN.findall(line))
            if item_ids and item_ids <= ENVIRONMENT_BLOCKED_ACCEPTANCE_ITEMS:
                continue
        gaps.append(blocking[0])
    return gaps


def print_summary(
    title: str,
    results: list[StageResult],
    notes: list[str],
    *,
    snapshot: list[str] | None = None,
    wall_note: str | None = None,
) -> int:
    print(f"\n==== {title} ====")
    for line in snapshot or []:
        print(f"快照       {line}")
    for result in results:
        print(f"{result.status:10} {result.name}")
        for line in result.detail.splitlines():
            print(f"           {line}")
    for note in notes:
        print(f"{STATUS_NOT_RUN:10} {note}")
    unverified = [result.name for result in results if result.status == STATUS_UNVERIFIED]
    if all(result.status == STATUS_OK for result in results):
        print(f"Overall: PASS（{len(results)} 个阶段全部通过）")
        code = 0
    elif unverified:
        names = ", ".join(unverified)
        print(f"Overall: NOT FULLY VERIFIED（存在 UNVERIFIED 阶段：{names}；不得报告为通过）")
        code = 1
    else:
        print("Overall: FAIL")
        code = 1
    # 墙钟耗时：fastcheck 用于判定 60 秒硬上限，fulltest/slowtest 用于如实记录本次运行规模。
    if wall_note is not None:
        print(wall_note)
    return code


_query_process_image = cast(
    "Callable[[int, int, object, object], int]",
    ctypes.WINFUNCTYPE(
        wintypes.BOOL,
        wintypes.HANDLE,
        wintypes.DWORD,
        wintypes.LPWSTR,
        ctypes.POINTER(wintypes.DWORD),
    )(("QueryFullProcessImageNameW", ctypes.windll.kernel32)),
)
_get_process_id = cast(
    "Callable[[int], int]",
    ctypes.WINFUNCTYPE(wintypes.DWORD, wintypes.HANDLE)(("GetProcessId", ctypes.windll.kernel32)),
)


def _finish_bazel_session(session: BazelSession, run: GateRun, results: list[Result]) -> None:
    """诊断与会话回收独立失败，均保留到最终统一汇总。."""
    statuses = {item.status for item in results}
    passed = STATUS_OK in statuses and statuses <= {STATUS_OK, "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"}
    try:
        _ = run.remaining
        session.diagnostics.finish(
            STATUS_OK if passed else STATUS_FAILED,
            run.total,
            run.compile_excluded,
        )
    except (Exception, KeyboardInterrupt) as error:  # noqa: BLE001 - 报告失败不能产生通过结果。
        passed = False
        failure = Result("bazel-diagnostics", STATUS_FAILED, f"{type(error).__name__}: {error}")
        results.append(failure)
        run.record_results([failure])
    try:
        session.close(success=passed)
    except (Exception, KeyboardInterrupt) as error:  # noqa: BLE001 - 会话回收失败仍进入统一计时汇总。
        failure = Result("bazel-session-close", STATUS_FAILED, f"Bazel 会话回收失败：{type(error).__name__}: {error}")
        results.append(failure)
        run.record_results([failure])


def _ps51_module_path() -> str | None:
    r"""给 Windows PowerShell 5.1 子进程用的 PSModulePath：剔除 PowerShell 7 的模块目录.

    调用方本身跑在 PowerShell 7 会话里时，进程继承的 PSModulePath 以
    `c:\program files\powershell\7\Modules` 开头；5.1 会优先自动加载该目录下标注
    CompatiblePSEditions=Core 的 Microsoft.PowerShell.Utility，加载失败后 Get-FileHash
    等内置 cmdlet 变成 CommandNotFoundException，acceptance.ps1 的打包阶段随即失败
    （本机实测反证：PSModulePath 收窄到 Windows PowerShell 自身目录后 Get-FileHash 立即可用）。
    只影响本进程树继承的环境，不改系统设置。
    """
    # Windows 环境变量名大小写不敏感，官方拼写是 PSModulePath；此处按现有值读取。
    # [quality-baseline approved 2026-10-03] 官方拼写误报，经用户裁定保留
    value = os.environ.get("PSModulePath")  # noqa: SIM112
    if not value:
        return None
    keep = [
        part
        for part in value.split(os.pathsep)
        if part
        and "\\powershell\\7\\" not in f"{part.lower()}\\"
        and "\\documents\\powershell\\" not in f"{part.lower()}\\"
    ]
    return os.pathsep.join(keep) or None


def _version_line(name: str) -> str:
    exe = shutil.which(name)
    if exe is None:
        return f"{name}: 未找到（PATH 缺失）"
    done = subprocess.run([exe, "--version"], capture_output=True, text=True, check=False)
    lines = (done.stdout or done.stderr).strip().splitlines()
    return f"{name}: {lines[0] if lines else '版本未知'}"


def _gate_worker_command() -> list[str]:
    return [
        sys.executable,
        "-u",
        "-c",
        "from scripts.test_gate import _gate_worker; raise SystemExit(_gate_worker())",
    ]


def _write_compile_excluded(state: mmap.mmap, excluded: float) -> None:
    """仅用于 CPython/Windows x64：按自然对齐的机器字原子发布 double 位模式。."""
    # 映射起点按页对齐；c_void_p.value 在 CPython 中直接读写机器字，不解引用指针。
    # pack/unpack 只操作本地 bytes；共享映射不能 pack_into，它会先清零。
    # from_buffer 视图不跨调用保留，因此不会阻止映射关闭，也没有阻塞监督器的锁。
    bits = struct.unpack("<Q", struct.pack("<d", excluded))[0]
    ctypes.c_void_p.from_buffer(state).value = bits


def _read_compile_excluded(state: mmap.mmap) -> float:
    """以相同的 Win64 机器字访问读取完整位模式；空指针表示全零位模式。."""
    bits = ctypes.c_void_p.from_buffer(state).value or 0
    return struct.unpack("<d", struct.pack("<Q", bits))[0]


def _monitor_gate_worker(proc: subprocess.Popen[bytes], state: mmap.mmap, outcome: _GateSupervision) -> None:
    """同时观察不可变编译扣时、失败标记与硬截止，不等待最终汇总。."""
    while True:
        total = outcome.observe(state)
        if total - outcome.excluded >= outcome.limit or outcome.marker == _WORKER_TIMED_OUT:
            outcome.status, outcome.forced = STATUS_TIMED_OUT, True
            return
        if outcome.marker == _WORKER_FAILED:
            outcome.forced = True
            return
        code = proc.poll()
        if code is not None:
            outcome.status = STATUS_OK if code == 0 and outcome.marker == _WORKER_OK else STATUS_FAILED
            return
        time.sleep(min(0.02, max(0.0, outcome.limit - total + outcome.excluded)))


def _close_supervised_service(service: ServiceLease, outcome: _GateSupervision, *, success: bool) -> None:
    """所有终态只释放租约和回收本轮任务，绝不终止持久服务。."""
    try:
        service.close(timeout=0.0 if success else min(2.0, outcome.remaining))
    except Exception as error:  # noqa: BLE001 - 本轮任务回收或租约发布失败不能产生通过结果。
        if outcome.status != STATUS_TIMED_OUT:
            outcome.status = STATUS_FAILED
        outcome.forced = True
        print(f"STOP Bazel 本轮任务释放失败：{type(error).__name__}: {error}", flush=True)
