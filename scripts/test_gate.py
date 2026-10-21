#!/usr/bin/env python3
"""JchTools 本地三级验证门：fastcheck / fulltest / slowtest.

fastcheck：Bazel 调度静态检查、格式、debug 编译，无测试或 release 打包；非编译预算 60 秒。
fulltest：相同检查与适用 Windows 非打包 UT / integration / doc / e2e；不要求 ISCC，预算 900 秒。
slowtest：fulltest 功能覆盖一次，加 release 目标构建、打包脚本单测与完整本地安装包 / ZIP；预算 1500 秒。
总墙钟中只扣除观测到且没有非编译工作重叠的编译区间；复用正常 target 缓存。
每条结束路径输出 total / compile_excluded / budgeted / limit 与状态；启动取消也回收已创建的 Job。
不触发 CI、发布、Computer Use、用户鼠标或抢焦点测试。
fulltest / slowtest 的 --authorized 只代表当次明确授权，不创造授权。


用法：
    python scripts/test_gate.py fastcheck
    python scripts/test_gate.py fastcheck --deadline-seconds 3   # 仅允许调低，用于验证超时路径
    python scripts/test_gate.py fulltest --authorized
    python scripts/test_gate.py slowtest --authorized
    python scripts/test_gate.py slowtest --authorized --deadline-seconds 1200
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import dataclasses
import io
import json
import math
import mmap
import os
import re
import shutil
import signal
import struct
import subprocess
import sys
import time
import traceback
from ctypes import wintypes
from pathlib import Path
from typing import TYPE_CHECKING, cast, final

import pywintypes
import win32api
import win32con
import win32event
import win32job
import winerror

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import IO


if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scripts import process_runtime
from scripts.bazel_gate import BazelSession
from scripts.bazel_service import (
    ServerPidPendingError,
    ServiceLease,
    open_verified_job,
    read_server_pid,
    terminate_requests,
)
from scripts.bazel_service import (
    job_process_ids as _job_process_ids,
)
from scripts.gate_runtime import GateRun, GateStoppedError, Result, Stage, process_log_directory
from scripts.local_gate_plan import gate_plan, run_full_coverage

ROOT = Path(__file__).resolve().parent.parent
# 与 BazelSession._dispatch_argv 一致：工具安装属于源码 checkout，不随运行 fixture ROOT 改变。
_BAZEL_TOOL_OUTPUT = ROOT / ".tmp" / "bazel-gate" / "output"
LOG_DIR = ROOT / ".tmp" / "test-gate"
FIXED_XBERG_TEST_DIR = Path(r"C:\Users\jiang\Documents\xberg-test\xberg-cli-x86_64-pc-windows-msvc")

FASTCHECK_DEADLINE_SECONDS = 60.0
FULLTEST_DEADLINE_SECONDS = 900.0
SLOWTEST_DEADLINE_SECONDS = 1500.0
PROCESS_TREE_CLEANUP_SECONDS = 5.0
SERVER_PID_PUBLICATION_SECONDS = 2.0
# 源码快照里的工作区改动只列前 N 项，dirty 时避免刷屏。
SNAPSHOT_DIRTY_SAMPLE = 10
BINDING_LOOP_SAMPLE_LIMIT = 6
BROKER_ARGUMENT_COUNT = 6
ACTION_ARGUMENT_COUNTS = frozenset({3, 4, 6})

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

# json.loads 的返回含 Any；经固定签名别名收口为 object，再用 TypeIs 守卫逐层收窄。
_parse_json: Callable[[str], object] = json.loads

# 墙钟接缝：生产即 time.monotonic；回归测试注入假时钟，确定性复现预算超限（不真实睡眠）。
_monotonic: Callable[[], float] = time.monotonic


@dataclasses.dataclass
class StageResult:
    """单个阶段的执行结果；log 为该阶段完整输出落盘位置（无则 None）."""

    name: str
    status: str
    detail: str
    log: Path | None = None


class _Arguments(argparse.Namespace):
    """带类型标注的解析结果；子命令未提供的开关保留安全默认值."""

    gate: str = ""
    deadline_seconds: float = FASTCHECK_DEADLINE_SECONDS
    authorized: bool = False
    # 参数只允许降低各级非编译预算；授权真实性仍是软约束。

    bazel_config: str | None = None


def _reconfigure_stdout() -> None:
    # 阶段明细含中文；Windows 控制台默认代码页会把 print 变成 UnicodeEncodeError。
    with contextlib.suppress(AttributeError):
        stream = sys.stdout
        if isinstance(stream, io.TextIOWrapper):
            stream.reconfigure(encoding="utf-8", errors="replace")


@final
class _CommandJob:
    """仅本次创建的 Windows 命令树，无桌面或 UI 运行依赖。."""

    def __init__(self, proc: subprocess.Popen[bytes]) -> None:
        create = cast("Callable[[object, str], int]", win32job.CreateJobObject)
        query = cast("Callable[[int, int], dict[str, object]]", vars(win32job)["QueryInformationJobObject"])
        configure = cast("Callable[[int, int, dict[str, object]], None]", vars(win32job)["SetInformationJobObject"])
        self.process = proc
        self._retained_handles: dict[int, int] = {}
        self.pid_publication_started: float | None = None
        self.service_job: int | None = None
        self.service_process: int | None = None
        self.job = create(None, "")
        try:
            information = query(self.job, win32job.JobObjectExtendedLimitInformation)
            limits = cast("dict[str, int]", information["BasicLimitInformation"])
            limits["LimitFlags"] |= win32job.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            configure(self.job, win32job.JobObjectExtendedLimitInformation, information)
            handle = win32api.OpenProcess(win32con.PROCESS_SET_QUOTA | win32con.PROCESS_TERMINATE, 0, proc.pid)
            try:
                win32job.AssignProcessToJobObject(self.job, handle)
            finally:
                win32api.CloseHandle(handle)
            if isinstance(proc.args, (list, tuple)) and "--lease-client" in proc.args:
                self.service_job, self.service_process = open_verified_job(ROOT)
        except BaseException:
            self.close()
            raise

    def process_handles(self, *, service: bool = True) -> list[int]:
        belongs = cast("Callable[[int, int], bool]", vars(win32job)["IsProcessInJob"])
        handles: list[int] = []
        try:
            observed_job = self.service_job if service and self.service_job is not None else self.job
            for pid in _job_process_ids(observed_job):
                try:
                    handle = win32api.OpenProcess(win32con.SYNCHRONIZE | win32con.PROCESS_QUERY_INFORMATION, 0, pid)
                except pywintypes.error as error:
                    if error.winerror != ERROR_INVALID_PARAMETER:
                        raise
                    continue  # 成员已退出，不对复用 PID 作终止操作。
                handles.append(handle)
                if not belongs(handle, observed_job):
                    _ = handles.pop()
                    win32api.CloseHandle(handle)
        except BaseException:
            for handle in handles:
                win32api.CloseHandle(handle)
            raise
        else:
            return handles

    def retain_process_identities(self) -> None:
        """保留所属进程对象到阶段结束；持有句柄期间 PID 不会被系统复用。."""
        observed_job = self.service_job if self.service_job is not None else self.job
        belongs = cast("Callable[[int, int], bool]", vars(win32job)["IsProcessInJob"])
        for pid in _job_process_ids(observed_job):
            if pid in self._retained_handles:
                continue
            try:
                handle = win32api.OpenProcess(win32con.SYNCHRONIZE | win32con.PROCESS_QUERY_INFORMATION, 0, pid)
            except pywintypes.error as error:
                if error.winerror != ERROR_INVALID_PARAMETER:
                    raise
                continue
            try:
                if belongs(handle, observed_job):
                    self._retained_handles[pid] = handle
                    continue
            except BaseException:
                win32api.CloseHandle(handle)
                raise
            win32api.CloseHandle(handle)

    def has_verified_live_member(self, pid: int) -> bool:
        """查询已验证 Job 归属且仍存活的缓存成员，不重新打开进程句柄。."""
        handle = self._retained_handles.get(pid)
        return handle is not None and win32event.WaitForSingleObject(handle, 0) != win32event.WAIT_OBJECT_0

    def close(self) -> None:
        for handle in self._retained_handles.values():
            win32api.CloseHandle(handle)
        self._retained_handles.clear()
        if self.service_process is not None:
            win32api.CloseHandle(self.service_process)
            self.service_process = None
        if self.service_job is not None:
            win32api.CloseHandle(self.service_job)
            self.service_job = None
        win32api.CloseHandle(self.job)

    def terminate(self, timeout: float) -> bool:
        """终止本轮命令和非持久任务，绝不把服务 Job 作为终止目标。."""
        terminate = cast("Callable[[int, int], None]", vars(win32job)["TerminateJobObject"])
        query = cast("Callable[[int, int], dict[str, int]]", vars(win32job)["QueryInformationJobObject"])
        deadline = time.monotonic() + max(0.0, timeout)
        handles: list[int] = []
        try:
            try:
                if self.service_job is not None and isinstance(self.process.args, (list, tuple)):
                    # 先封闭本轮入口；即便通知或采样失败，也必须进入任务终止路径。
                    requests = Path(str(self.process.args[-1]))
                    _ = (requests / "cancelled").touch()
                handles = self.process_handles(service=False)
            finally:
                terminate(self.job, 1)
                native_stopped = True
                if self.service_job is not None and self.service_process is not None:
                    native_stopped = terminate_requests(
                        ROOT / ".tmp/bazel-gate/service",
                        self.service_job,
                        self.service_process,
                        timeout=max(0.0, deadline - time.monotonic()),
                    )
            while query(self.job, win32job.JobObjectBasicAccountingInformation)["ActiveProcesses"]:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                time.sleep(min(0.01, remaining))
            return _wait_process_handles(handles, deadline) and native_stopped
        except pywintypes.error:
            return False
        finally:
            for handle in handles:
                win32api.CloseHandle(handle)

    def wait_for_termination(self, timeout: float) -> bool:
        query = cast("Callable[[int, int], dict[str, int]]", vars(win32job)["QueryInformationJobObject"])
        deadline = time.monotonic() + max(0.0, timeout)
        try:
            while query(self.job, win32job.JobObjectBasicAccountingInformation)["ActiveProcesses"]:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                time.sleep(min(0.01, remaining))
        except pywintypes.error:
            return False
        else:
            return True


def _wait_process_handles(handles: list[int], deadline: float) -> bool:
    # Job 活动计数归零并不等于所有进程句柄已经发出终态信号，仍须等待持有句柄。
    return all(
        win32event.WaitForSingleObject(handle, max(0, int((deadline - time.monotonic()) * 1000)))
        == win32event.WAIT_OBJECT_0
        for handle in handles
    )


_COMMAND_JOBS: dict[int, _CommandJob] = {}


def _start_owned_command(
    argv: list[str], *, stdout: IO[bytes] | int = subprocess.DEVNULL, env: dict[str, str] | None = None
) -> subprocess.Popen[bytes]:
    proc = subprocess.Popen(
        argv,
        cwd=ROOT,
        stdin=subprocess.DEVNULL,
        stdout=stdout,
        stderr=subprocess.STDOUT,
        env=env,
        creationflags=win32con.CREATE_SUSPENDED if sys.platform == "win32" else 0,
        start_new_session=sys.platform != "win32",
    )
    if sys.platform == "win32":
        owner: _CommandJob | None = None
        try:
            owner = _CommandJob(proc)
            _COMMAND_JOBS[proc.pid] = owner
            process_runtime.resume_owned_process(proc.pid)
        except BaseException:
            _ = _COMMAND_JOBS.pop(proc.pid, None)
            if owner is not None:
                owner.close()
            proc.kill()
            _ = proc.wait()
            raise
    return proc


def _close_owned_command(proc: subprocess.Popen[bytes], *, timeout: float) -> bool:
    owner = _COMMAND_JOBS.get(proc.pid)
    if owner is None or owner.process is not proc:
        return sys.platform != "win32"
    _ = _COMMAND_JOBS.pop(proc.pid)
    try:
        confirmed = owner.terminate(timeout)
        if confirmed:
            _ = proc.poll()
        return confirmed
    finally:
        owner.close()


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


def _observe_bazel_server(owner: _CommandJob, proc: subprocess.Popen[bytes]) -> None:
    if not isinstance(proc.args, (list, tuple)) or "--lease-client" not in proc.args:
        return
    if (
        owner.service_process is not None
        and win32event.WaitForSingleObject(owner.service_process, 0) == win32event.WAIT_OBJECT_0
    ):
        message = "Bazel 原生服务意外退出"
        raise RuntimeError(message)
    requests = Path(str(proc.args[-1]))
    completed = any(requests.glob("*.complete.json"))
    pid_file = ROOT / ".tmp/bazel-gate/output/base/server/server.pid.txt"
    try:
        server_pid = read_server_pid(pid_file)
    except (pywintypes.error, ServerPidPendingError) as error:
        _await_server_pid_publication(owner, proc, requests, error, completed=completed)
        return  # 不阻塞监督器；下次采样仍计入门预算，并继续保留所有 Job 成员身份。
    if server_pid is None:
        if completed:
            message = "Bazel 客户端已结束但缺少受控 server PID 证据"
            raise RuntimeError(message)
        return
    observed_job = owner.service_job if owner.service_job is not None else owner.job
    if not owner.has_verified_live_member(server_pid) and server_pid not in _job_process_ids(observed_job):
        if not _process_is_running(server_pid) and not completed:
            return
        message = "Bazel server 不属于当前派发 Job，拒绝继续"
        raise RuntimeError(message)
    owner.pid_publication_started = None


def _observe_owned_command(proc: subprocess.Popen[bytes]) -> None:
    owner = _COMMAND_JOBS.get(proc.pid)
    if owner is not None and owner.process is proc:
        try:
            owner.retain_process_identities()
            _observe_bazel_server(owner, proc)
        except pywintypes.error as error:
            message = f"保留所属进程身份失败：{error}"
            raise RuntimeError(message) from error


def _kill_tree(proc: subprocess.Popen[bytes], *, timeout: float) -> bool:
    # 超时必须终止整个进程树（cargo 会派生 rustc / 测试二进制子进程）。
    if sys.platform == "win32":
        owner = _COMMAND_JOBS.get(proc.pid)
        if owner is None or owner.process is not proc:
            return False
        return owner.terminate(timeout)
    try:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
    except ProcessLookupError:
        return True
    except PermissionError:
        return False
    return True


def _tail(log: Path, limit: int = 12) -> str:
    lines = log.read_text(encoding="utf-8", errors="replace").splitlines()
    return "\n".join(lines[-limit:])


def run_logged(
    name: str, argv: list[str], *, timeout: float | None, env_extra: dict[str, str] | None = None
) -> StageResult:
    """运行单个命令阶段：输出落独占进程目录；None 不限制命令等待时间。."""
    started = _monotonic()
    directory = process_log_directory(LOG_DIR / "commands")
    log = directory / f"{name}.log"
    directory.mkdir(parents=True, exist_ok=False)
    env = os.environ | (env_extra or {})
    cleanup_budget = (
        PROCESS_TREE_CLEANUP_SECONDS if timeout is None else min(PROCESS_TREE_CLEANUP_SECONDS, timeout * 0.25)
    )
    if timeout is not None and timeout <= cleanup_budget:
        return StageResult(name, STATUS_TIMED_OUT, f"阶段预算 {timeout:.1f}s 不足以启动并清理进程树")
    with log.open("xb") as sink:
        proc = _start_owned_command(argv, stdout=sink, env=env)
        timed_out = False
        tree_terminated = True
        # 等待窗口按绝对截止计算：进程启动与 Job 绑定的耗时从命令等待预算中扣除，
        # 否则固定等待叠加上述启动耗时会把总墙钟推过硬上限；同时保留原定 cleanup
        # 预算给进程树终止收尾，不得让等待吃满全部预算后挤压清理窗口。
        # 等待预算已被启动耗尽时 wait(timeout=0) 仍会轮询一次：只有已退出的命令才能通过。
        wait_budget = None if timeout is None else max(0.0, started + timeout - cleanup_budget - _monotonic())
        try:
            _ = proc.wait(timeout=wait_budget)
        except subprocess.TimeoutExpired:
            if timeout is None:
                raise
            timed_out = True
            cleanup_deadline = started + timeout
            kill_timeout = min(2.0, max(0.0, cleanup_deadline - _monotonic()))
            tree_terminated = _kill_tree(proc, timeout=kill_timeout)
            if not tree_terminated and proc.poll() is None:
                proc.kill()
            try:
                _ = proc.wait(timeout=max(0.0, cleanup_deadline - _monotonic()))
            except subprocess.TimeoutExpired:
                tree_terminated = False
                if proc.poll() is None:
                    proc.kill()
        finally:
            remaining = cleanup_budget if timeout is None else max(0.0, started + timeout - _monotonic())
            tree_terminated = _close_owned_command(proc, timeout=remaining) and tree_terminated
    elapsed = _monotonic() - started
    if timed_out:
        cleanup = "进程树已终止" if tree_terminated else "未能确认进程树已终止"
        detail = f"达到 {timeout:.0f}s 总预算（已耗时 {elapsed:.1f}s）；{cleanup}；完整日志：{log}"
        return StageResult(name, STATUS_TIMED_OUT, detail, log)
    if proc.returncode == 0:
        if not tree_terminated:
            return StageResult(name, STATUS_FAILED, "命令已退出，但未能确认所属子进程全部退出", log)
        return StageResult(name, STATUS_OK, f"{elapsed:.1f}s；完整日志：{log}", log)
    detail = f"退出码 {proc.returncode}（已耗时 {elapsed:.1f}s）；日志尾部：\n{_tail(log)}"
    return StageResult(name, STATUS_FAILED, detail, log)


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


def _gate_controller(image: Path, argv: tuple[str, ...]) -> bool:
    """仅承认门自身的派发、原生 Bazel 和 adapter 监督器，不豁免通用解释器。."""
    native = ROOT / ".tmp" / "bazel-gate"
    dispatch_argc = 5

    def python_controller() -> bool:
        if len(argv) == dispatch_argc and argv[1:4] == ("-m", "scripts.bazel_gate", "--dispatch"):
            request = Path(argv[4]).resolve()
            return request == native / "dispatch.json" or (
                request.name.endswith(".request.json")
                and request.parent.name == "bazel-service"
                and request.is_relative_to(LOG_DIR.resolve())
            )
        if len(argv) == dispatch_argc and argv[1:4] == ("-m", "scripts.bazel_gate", "--lease-client"):
            requests = Path(argv[4]).resolve()
            return requests.name == "bazel-service" and requests.is_relative_to(LOG_DIR.resolve())
        if len(argv) == BROKER_ARGUMENT_COUNT and argv[1:4] == ("-m", "scripts.bazel_service", "--broker"):
            return Path(argv[4]).resolve() == native / "service" and argv[5].startswith("Local\\JchToolsBazel-")
        if len(argv) in ACTION_ARGUMENT_COUNTS and Path(argv[1]).resolve() in {
            ROOT / "scripts" / "bazel_action.py",
            native / "bazel_action.py",
        }:
            # 只信任本门生成的规范短 package，不能将任意 JSON 视为监督器。
            spec = Path(argv[2]).resolve()
            return (
                spec.name == "spec.json"
                and spec.parent.parent == native / "targets"
                and re.fullmatch(r"[0-9a-f]{16}", spec.parent.name) is not None
            )
        return False

    if image.resolve() == Path(sys.executable).resolve():
        return python_controller()
    output_base = native / "output" / "base"
    if process_runtime.is_bazel_client(image, argv, output_base=output_base, output_root=_BAZEL_TOOL_OUTPUT):
        return True
    if image.name.lower() == "java.exe":
        # Windows Bazel 9.3.0 实际以 -jar 启动，不含 Java 主类参数。
        server = next(
            (
                Path(argv[index + 1]).resolve()
                for index, arg in enumerate(argv)
                if arg == "-jar" and index + 1 < len(argv)
            ),
            None,
        )
        return (
            server is not None
            and server.name == "A-server.jar"
            and server.is_relative_to(_BAZEL_TOOL_OUTPUT / "install")
            and ("--batch" in argv or any(arg.startswith("--max_idle_secs=") for arg in argv))
            and any(
                arg.startswith("--output_base=") and Path(arg.partition("=")[2]).resolve() == output_base
                for arg in argv
            )
        )
    return False


def _compiler_ids(proc: subprocess.Popen[bytes]) -> frozenset[int]:
    """只观察本次 Job 内真实编译器，不将 cargo 或检查器整体豁免."""
    owner = _COMMAND_JOBS.get(proc.pid)
    if owner is None or owner.process is not proc:
        return frozenset()
    compilers = {
        "rustc.exe",
        "link.exe",
        "lld-link.exe",
        "rc.exe",
        "cl.exe",
        "c1xx.exe",
        "c2.exe",
    }
    # conhost 是 Windows 为所属控制台程序创建的宿主，不执行检查或构建脚本。
    controllers = {"cargo.exe", "cargo-clippy.exe", "rustup.exe", "cmd.exe", "powershell.exe", "conhost.exe"}
    identifiers: set[int] = set()
    handles = owner.process_handles()
    try:
        for handle in handles:
            if win32event.WaitForSingleObject(handle, 0) == win32event.WAIT_OBJECT_0:
                continue
            buffer = ctypes.create_unicode_buffer(32768)
            size = ctypes.c_ulong(len(buffer))
            if not _query_process_image(int(handle), 0, buffer, ctypes.byref(size)):
                return frozenset()
            executable = Path(cast("str", buffer.value))
            image = executable.name.lower()
            if image in compilers:
                identifiers.add(_get_process_id(int(handle)))
            elif image not in controllers and not _gate_controller(
                executable, process_runtime.process_argv(int(handle))
            ):
                # build script、下载器或其他子进程可能与编译重叠，不豁免其工作。
                return frozenset()
    finally:
        for handle in handles:
            win32api.CloseHandle(handle)
    return frozenset(identifiers)


def _gate_plan(level: str, log_dir: Path) -> list[Stage]:
    return gate_plan(level, ROOT, log_dir, _powershell_env())


def _prepare_process_workspace(run: GateRun, results: list[Result]) -> None:
    temporary = run.supervise(
        Stage(
            "temporary-workspace",
            [
                sys.executable,
                str(ROOT / "scripts/make_tmp.py"),
                "workspace",
                "--destination",
                str(run.log_dir),
            ],
        )
    )
    results.extend(temporary)
    if len(temporary) != 1 or temporary[0].status != STATUS_OK:
        message = "仓库隔离临时目录准备失败；未启动其他检查。"
        raise RuntimeError(message)
    run.prepare_temporary()


def _execute_bazel_plan(
    session: BazelSession, level: str, plan: list[Stage], results: list[Result], snapshot: list[str]
) -> None:
    common = [
        stage
        for stage in plan
        if stage.name in {"production-build", "test-target-check", "clippy"}
        or stage.name == "xberg-latest"
        or (stage.kind in {"compile", "compiler-observed"} and not stage.name.endswith("-doctests"))
        or (stage.name.startswith(("static-", "rustfmt-", "pyquality-")) and stage.name != "pyquality-pip-audit")
    ]
    results.extend(session.run(common, parallel=False))
    if level != "fastcheck":
        run_full_coverage(session, plan, results, snapshot, ROOT, level=level, ps_env=_powershell_env())


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


def execute_gate(  # noqa: PLR0913 - 独立保留阶段、编译观测、时钟和监督进度四个运行边界。
    level: str,
    limit: float,
    *,
    stages_override: list[Stage] | None = None,
    compiler_ids_override: Callable[[subprocess.Popen[bytes]], frozenset[int]] | None = None,
    clock_override: Callable[[], float] | None = None,
    progress: Callable[[float, str], None] | None = None,
    bazel_config: str | None = None,
) -> int:
    """单一入口；override 仅供隔离自检，不暴露为 CLI 绕过开关."""
    run = GateRun(
        level,
        limit,
        ROOT,
        LOG_DIR / level,
        start_command=_start_owned_command,
        close_command=_close_owned_command,
        kill_tree=_kill_tree,
        compiler_ids=compiler_ids_override or _compiler_ids,
        clock=clock_override or _monotonic,
        observe_tree=_observe_owned_command,
        validate_result=_validate_gate_result,
        progress=progress,
    )
    results: list[Result] = []
    snapshot: list[str] = []
    notes: list[str] = []
    session: BazelSession | None = None
    try:
        if stages_override is not None:
            for stage in stages_override:
                results.extend(run.supervise(stage))
        elif sys.platform != "win32":
            results.append(Result("platform", STATUS_UNVERIFIED, "合同 P-07 仅支持 Windows。"))
        else:
            _prepare_process_workspace(run, results)
            plan = _gate_plan(level, run.log_dir)
            session = BazelSession(run, bazel_config=bazel_config)
            _execute_bazel_plan(session, level, plan, results, snapshot)
            notes.append("NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED：gui_smoke、Markdown 真实桌面验收、OCR 桌面探针。")
            notes.append("CI / 远程流水线 / 发布 / Computer Use：在三个门之外。")
        if level == "slowtest":
            notes.append("SKIPPED_NOT_APPLICABLE WSL：P-07 仅 Windows；slowtest 增加本地打包验证，不引入 WSL。")
    except GateStoppedError as error:
        results.extend(error.results)
    except KeyboardInterrupt:
        results.append(Result("interruption", STATUS_FAILED, "用户中断；不报告为通过。"))
    except Exception as error:  # noqa: BLE001 - 所有编排异常统一失败退出，不能遗漏清理。
        failure = Result("orchestration", STATUS_FAILED, f"门编排失败：{type(error).__name__}: {error}")
        results.append(failure)
        run.record_results([failure])
        traceback.print_exc()
        _ = sys.stderr.flush()
    finally:
        if session is not None:
            _finish_bazel_session(session, run, results)
    return run.finish(results, snapshot=snapshot, notes=notes)


def cmd_fastcheck(deadline_seconds: float) -> int:
    return execute_gate("fastcheck", deadline_seconds)


def _powershell_env() -> dict[str, str]:
    """Windows PowerShell 5.1 兼容环境，真实媒体 / GUI 代理与 slowtest 打包共用.

    本机执行策略全作用域 Undefined（默认 Restricted）会拒绝任何 -File 运行 .ps1；
    PSExecutionPolicyPreference 以 Process 作用域覆盖之，且随环境继承给脚本内部
    再起的 powershell 子进程，只影响本进程树。PSModulePath 修正见
    [`_ps51_module_path`]（本机实测，缺它时 PS 子进程加载不了模块）。
    """
    env = {"PSExecutionPolicyPreference": "Bypass"}
    module_path = _ps51_module_path()
    if module_path is not None:
        env["PSModulePath"] = module_path
    # 保留调用方明确选定的隔离测试引擎，即使路径无效也由 acceptance 报错，
    # 不能静默改测固定目录里的旧二进制。未指定时才回落固定测试目录。
    explicit_xberg = os.environ.get("JCHTOOLS_TEST_XBERG_DIR")
    if explicit_xberg:
        env["JCHTOOLS_TEST_XBERG_DIR"] = explicit_xberg
    elif (FIXED_XBERG_TEST_DIR / "xberg.exe").is_file():
        env["JCHTOOLS_TEST_XBERG_DIR"] = str(FIXED_XBERG_TEST_DIR)
    # test-program-build 会在真实媒体用例前构建该 EXE；保留调用方显式路径，
    # 并尊重 CARGO_TARGET_DIR，不依赖打包阶段或猜测旧产物。
    explicit_gui = os.environ.get("JCHTOOLS_TEST_GUI_EXE")
    if explicit_gui:
        env["JCHTOOLS_TEST_GUI_EXE"] = explicit_gui
    else:
        target_raw = os.environ.get("CARGO_TARGET_DIR")
        if target_raw:
            target_dir = Path(target_raw)
            if not target_dir.is_absolute():
                target_dir = ROOT / target_dir
        else:
            target_dir = ROOT / "target"
        env["JCHTOOLS_TEST_GUI_EXE"] = str(target_dir.resolve() / "debug" / "JchTools.exe")
    return env


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


def cmd_fulltest(deadline_seconds: float = FULLTEST_DEADLINE_SECONDS) -> int:
    return main(["fulltest", "--authorized", "--deadline-seconds", str(deadline_seconds)])


def _git_output(git: str, args: list[str]) -> str | None:
    done = subprocess.run([git, *args], cwd=ROOT, capture_output=True, text=True, encoding="utf-8", check=False)
    if done.returncode != 0:
        return None
    return done.stdout.strip()


def _version_line(name: str) -> str:
    exe = shutil.which(name)
    if exe is None:
        return f"{name}: 未找到（PATH 缺失）"
    done = subprocess.run([exe, "--version"], capture_output=True, text=True, check=False)
    lines = (done.stdout or done.stderr).strip().splitlines()
    return f"{name}: {lines[0] if lines else '版本未知'}"


def snapshot_lines() -> list[str]:
    """本次运行的源码快照：HEAD、工作区是否干净、关键构建配置与主要工具版本.

    fulltest / slowtest 的结论必须能绑回具体提交；工作区不干净时列出改动摘要，
    避免把「含未提交改动的本地结果」与「CI 构建的提交结果」混为同一状态。
    """
    git = shutil.which("git")
    head = _git_output(git, ["rev-parse", "HEAD"]) if git is not None else None
    lines = [f"HEAD: {head or '未知（git 不可用）'}"]
    if git is not None:
        porcelain = _git_output(git, ["status", "--porcelain"])
        if porcelain is None:
            lines.append("工作区：状态查询失败")
        elif not porcelain:
            lines.append("工作区：clean（无未提交改动）")
        else:
            changed = porcelain.splitlines()
            sample = "；".join(changed[:SNAPSHOT_DIRTY_SAMPLE])
            rest = len(changed) - SNAPSHOT_DIRTY_SAMPLE
            more = f"；另有 {rest} 项" if rest > 0 else ""
            lines.append(f"工作区：dirty（{len(changed)} 项未提交改动）：{sample}{more}")
    lines.append(_version_line("cargo"))
    lines.append(_version_line("rustc"))
    lines.append(f"python: {sys.version.split()[0]}（{sys.executable}）")
    for key in ("CARGO_TARGET_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        value = os.environ.get(key)
        if value:
            lines.append(f"{key}: {value}")
    return lines


def cmd_slowtest(deadline_seconds: float = SLOWTEST_DEADLINE_SECONDS) -> int:
    return main(["slowtest", "--authorized", "--deadline-seconds", str(deadline_seconds)])


def _gate_worker_command() -> list[str]:
    return [
        sys.executable,
        "-u",
        "-c",
        "from scripts.test_gate import _gate_worker; raise SystemExit(_gate_worker())",
    ]


def _gate_state(name: str) -> mmap.mmap:
    if sys.platform == "win32":
        # All 平台存根只暴露 Unix 构造签名；这里绑定已由 Windows 运行验证的命名映射签名。
        create = cast("Callable[[int, int, str], mmap.mmap]", vars(mmap)["mmap"])
        return create(-1, 16, name)
    message = "测试门独立监督仅支持 Windows"
    raise OSError(message)


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


def _gate_worker() -> int:  # pyright: ignore[reportUnusedFunction] # 由 _gate_worker_command 的独立解释器调用。
    """受独立进程监督；仅上报已经观测的编译扣时，不能延长其他工作预算。."""
    level = os.environ["JCHTOOLS_GATE_LEVEL"]
    limit = float(os.environ["JCHTOOLS_GATE_LIMIT"])
    started: float | None = float(os.environ["JCHTOOLS_GATE_STARTED"])

    def clock() -> float:
        nonlocal started
        if started is not None:
            value, started = started, None
            return value
        return _monotonic()

    with _gate_state(os.environ["JCHTOOLS_GATE_PROGRESS"]) as state:

        def progress(excluded: float, status: str) -> None:
            _write_compile_excluded(state, excluded)
            if status:
                struct.pack_into("<i", state, 8, _WORKER_TIMED_OUT if status == STATUS_TIMED_OUT else _WORKER_FAILED)

        code = execute_gate(
            level,
            limit,
            clock_override=clock,
            progress=progress,
            bazel_config=os.environ.get("JCHTOOLS_GATE_BAZEL_CONFIG") or None,
        )
        if struct.unpack_from("<i", state, 8)[0] == 0:
            struct.pack_into("<i", state, 8, _WORKER_OK if code == 0 else _WORKER_FAILED)
        return code


def _abort_gate_worker(proc: subprocess.Popen[bytes], *, timeout: float) -> None:
    """先立即终止监督进程树，再在剩余预算内确认其退出。."""
    owner = _COMMAND_JOBS.pop(proc.pid)
    try:
        terminate = cast("Callable[[int, int], None]", vars(win32job)["TerminateJobObject"])
        terminate(owner.job, 1)
        _ = owner.wait_for_termination(timeout)
    finally:
        owner.close()
        _ = proc.poll()


@dataclasses.dataclass
class _GateSupervision:
    """监督器独占的预算与终态，不依赖工作进程完成日志输出。."""

    limit: float
    started: float
    excluded: float = 0.0
    status: str = STATUS_FAILED
    forced: bool = False
    marker: int = 0

    @property
    def remaining(self) -> float:
        return max(0.0, self.limit - (_monotonic() - self.started - self.excluded))

    @property
    def succeeded(self) -> bool:
        return self.status == STATUS_OK and not self.forced and self.marker == _WORKER_OK

    def observe(self, state: mmap.mmap) -> float:
        observed = _read_compile_excluded(state)
        # 先读扣时再采墙钟，避免采时后被抢占、读到合法但晚于旧时钟的扣时。
        total = _monotonic() - self.started
        if not math.isfinite(observed) or observed < self.excluded or observed > total:
            message = "工作进程上报了无效的编译扣时"
            raise ValueError(message)
        self.excluded = observed
        self.marker = struct.unpack_from("<i", state, 8)[0]
        return total


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


def _cleanup_supervised_gate(
    service: ServiceLease | None, proc: subprocess.Popen[bytes] | None, outcome: _GateSupervision
) -> None:
    """先立即回收本轮 worker，再释放服务租约；任一路径异常都继续回收另一方。."""
    try:
        if proc is not None:
            _abort_gate_worker(proc, timeout=outcome.remaining if outcome.succeeded else 0.0)
    finally:
        if service is not None:
            _close_supervised_service(service, outcome, success=outcome.succeeded and sys.exception() is None)


def _supervise_gate(level: str, limit: float, started: float, *, bazel_config: str | None = None) -> int:
    """独立于工作进程的硬截止：清理、日志和汇总卡死也不能越过共享预算。."""
    name = f"Local\\JchToolsGate-{os.getpid()}-{time.monotonic_ns()}"
    outcome = _GateSupervision(limit, started)
    proc: subprocess.Popen[bytes] | None = None
    service: ServiceLease | None = None
    with _gate_state(name) as state:
        env = os.environ | {
            "JCHTOOLS_GATE_LEVEL": level,
            "JCHTOOLS_GATE_LIMIT": str(limit),
            "JCHTOOLS_GATE_STARTED": str(started),
            "JCHTOOLS_GATE_PROGRESS": name,
            "JCHTOOLS_GATE_BAZEL_CONFIG": bazel_config or "",
            "PYTHONIOENCODING": "utf-8",
        }
        try:
            service = ServiceLease(ROOT)
            env["JCHTOOLS_GATE_BAZEL_SERVICE_LEASE"] = str(service.lease)
            output = sys.stdout.fileno() if isinstance(sys.stdout, io.TextIOWrapper) else subprocess.DEVNULL
            proc = _start_owned_command(_gate_worker_command(), stdout=output, env=env)
            _monitor_gate_worker(proc, state, outcome)
        except KeyboardInterrupt:
            outcome.forced = True
            print("STOP 用户中断；立即终止本门所属进程树", flush=True)
        except Exception as error:  # noqa: BLE001 - 监督异常也必须终止工作进程，不能放任继续执行。
            outcome.forced = True
            print(f"STOP 监督失败：{type(error).__name__}: {error}", flush=True)
        finally:
            _cleanup_supervised_gate(service, proc, outcome)
        if outcome.forced or outcome.marker == 0:
            total = _monotonic() - started
            print(
                f"{level} {outcome.status} total={total:.1f}s",
                f"compile_excluded={outcome.excluded:.1f}s",
                f"budgeted={max(0.0, total - outcome.excluded):.1f}s limit={limit:.1f}s",
                flush=True,
            )
    return 0 if outcome.status == STATUS_OK else 1


def main(argv: list[str] | None = None) -> int:
    entry_started = _monotonic()
    started_at: float | None = entry_started

    def clock() -> float:
        nonlocal started_at
        if started_at is not None:
            value, started_at = started_at, None
            return value
        return _monotonic()

    _reconfigure_stdout()
    description = "JchTools 本地三级门：非编译预算 60 / 900 / 1500 秒；无 CI、鼠标或发布"
    parser = argparse.ArgumentParser(description=description)
    sub = parser.add_subparsers(dest="gate", required=True)
    for name, ceiling in (
        ("fastcheck", FASTCHECK_DEADLINE_SECONDS),
        ("fulltest", FULLTEST_DEADLINE_SECONDS),
        ("slowtest", SLOWTEST_DEADLINE_SECONDS),
    ):
        command = sub.add_parser(name, help=f"非编译预算 ≤{ceiling:.0f}s；保留缓存，观测编译独占区间扣时")
        _ = command.add_argument("--deadline-seconds", type=float, default=ceiling, help="只允许调低本级非编译预算")
        _ = command.add_argument(
            "--bazel-config", default=None, help="Use the same named Bazel RC config for every dispatch"
        )
        if name != "fastcheck":
            _ = command.add_argument("--authorized", action="store_true", help="代表当次明确授权；不可自主升级")
    arguments = parser.parse_args(argv, namespace=_Arguments())
    if arguments.gate != "fastcheck" and not arguments.authorized:
        run = GateRun(
            arguments.gate,
            arguments.deadline_seconds,
            ROOT,
            LOG_DIR,
            start_command=_start_owned_command,
            close_command=_close_owned_command,
            kill_tree=_kill_tree,
            compiler_ids=_compiler_ids,
            clock=clock,
        )
        return run.finish([Result("authorization", STATUS_UNVERIFIED, "须当次明确授权，并使用 --authorized。")])
    if (
        sys.platform == "win32"
        and math.isfinite(arguments.deadline_seconds)
        and (
            0.0
            < arguments.deadline_seconds
            <= {
                "fastcheck": FASTCHECK_DEADLINE_SECONDS,
                "fulltest": FULLTEST_DEADLINE_SECONDS,
                "slowtest": SLOWTEST_DEADLINE_SECONDS,
            }[arguments.gate]
        )
    ):
        return _supervise_gate(
            arguments.gate, arguments.deadline_seconds, entry_started, bazel_config=arguments.bazel_config
        )
    return execute_gate(
        arguments.gate, arguments.deadline_seconds, clock_override=clock, bazel_config=arguments.bazel_config
    )


if __name__ == "__main__":
    sys.exit(main())
