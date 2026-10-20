"""工作区独占的持久 Bazel 服务；有效原生 Java 启动环境变化时安全切换自有 Job。."""

from __future__ import annotations

import hashlib
import json
import msvcrt
import os
import subprocess
import sys
import time
import traceback
import uuid
from pathlib import Path
from typing import TYPE_CHECKING, NotRequired, TypedDict, cast, final

import pywintypes
import win32api
import win32con
import win32event
import win32file
import win32job
import win32process
import winerror

from scripts.gate_runtime import read_published_text

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping

_NO_INHERIT = False
_IDLE_SECONDS = 10800
_BROKER_ARGC = 4
_STOP_ARGC = 3


class _Identity(TypedDict):
    pid: int
    job: str
    root: str
    version: str
    created: str
    system_javabase: NotRequired[str]


class _Lease(TypedDict):
    owner: NotRequired[int]
    requests: NotRequired[str | None]
    released: bool
    owner_created: NotRequired[str]


class _Request(TypedDict):
    argv: list[str]
    environment: dict[str, str]


_open_job = cast("Callable[[int, bool, str], int]", vars(win32job)["OpenJobObject"])
_create_job = cast("Callable[[object, str], int]", vars(win32job)["CreateJobObject"])
_query_job = cast("Callable[[int, int], dict[str, object]]", vars(win32job)["QueryInformationJobObject"])
_set_job = cast("Callable[[int, int, dict[str, object]], None]", vars(win32job)["SetInformationJobObject"])
terminate_job = cast("Callable[[int, int], None]", vars(win32job)["TerminateJobObject"])
_belongs = cast("Callable[[int, int], bool]", vars(win32job)["IsProcessInJob"])
_process_times = cast("Callable[[int], dict[str, object]]", vars(win32process)["GetProcessTimes"])
_create_process = cast(
    "Callable[[str | None, str, object, object, bool, int, dict[str, str], str, object], tuple[int, int, int, int]]",
    vars(win32process)["CreateProcess"],
)
_resume_thread = cast("Callable[[int], int]", vars(win32process)["ResumeThread"])


def _open_process(pid: int) -> int:
    return win32api.OpenProcess(win32con.SYNCHRONIZE | win32con.PROCESS_QUERY_INFORMATION, _NO_INHERIT, pid)


def open_verified_job(root: Path, *, maintenance: bool = False) -> tuple[int, int]:
    identity = cast("_Identity", json.loads(read_published_text(root / ".tmp/bazel-gate/service/identity.json")))
    process = _open_process(identity["pid"])
    job: int | None = None
    try:
        job = _open_job(win32job.JOB_OBJECT_ALL_ACCESS, _NO_INHERIT, identity["job"])
        _verify_job_identity(root, identity, process, job, maintenance=maintenance)
    except BaseException:
        if job is not None:
            win32api.CloseHandle(job)
        win32api.CloseHandle(process)
        raise
    else:
        return job, process


def _verify_job_identity(root: Path, identity: _Identity, process: int, job: int, *, maintenance: bool) -> None:
    if not (
        _alive(process)
        and identity["root"] == str(root.resolve())
        and (maintenance or identity["version"] == _version())
        and str(_process_times(process)["CreationTime"]) == identity["created"]
        and _belongs(process, job)
    ):
        message = "已有服务的身份或版本不匹配；请单独执行维护停止。"
        raise RuntimeError(message)


def _publish(path: Path, value: object) -> None:
    temporary = path.with_name(path.name + ".pending")
    _ = temporary.write_text(json.dumps(value), encoding="utf-8")
    _ = temporary.replace(path)


def _version() -> str:
    digest = hashlib.sha256()
    for name in ("bazel_service.py", "bazel_gate.py"):
        digest.update(Path(__file__).with_name(name).read_bytes())
    return digest.hexdigest()


def _system_javabase(environment: Mapping[str, str]) -> str:
    # Windows native client 的 GetSystemJavabase 只接受含 bin/java.exe 的 JAVA_HOME，
    # 否则传递空的 --default_system_javabase；不从 PATH 选择或覆盖用户的 Java。
    home = next((value for key, value in environment.items() if key.upper() == "JAVA_HOME"), "")
    return home if home and (Path(home) / "bin/java.exe").exists() else ""


def _alive(handle: int) -> bool:
    return win32event.WaitForSingleObject(handle, 0) != win32event.WAIT_OBJECT_0


@final
class ServiceLease:
    """持有可直接终止所属进程树的真实 Job 句柄。."""

    def __init__(self, root: Path, *, maintenance: bool = False) -> None:
        self.directory = root / ".tmp/bazel-gate/service"
        self.directory.mkdir(parents=True, exist_ok=True)
        (self.directory / "jvm-tmp").mkdir(exist_ok=True)
        self.lock = (self.directory / "lease.lock").open("a+b")
        self.job: int | None = None
        self.process: int | None = None
        self.lease = self.directory / (uuid.uuid4().hex + ".lease.json")
        try:
            self._lock()
            self._reuse(root, maintenance=maintenance)
            if self.job is None and maintenance:
                return
            if self.job is None:
                self._start(root)
            _publish(
                self.lease,
                {
                    "owner": os.getpid(),
                    "owner_created": str(_process_times(win32api.GetCurrentProcess())["CreationTime"]),
                    "requests": None,
                    "released": False,
                },
            )
            _publish(self.directory / "active.json", {"lease": str(self.lease)})
        except BaseException:
            self.close(success=False)
            raise

    def _lock(self) -> None:
        _ = self.lock.seek(0, os.SEEK_END)
        if not self.lock.tell():
            _ = self.lock.write(b"\0")
            self.lock.flush()
        _ = self.lock.seek(0)
        msvcrt.locking(self.lock.fileno(), msvcrt.LK_NBLCK, 1)

    def _reuse(self, root: Path, *, maintenance: bool) -> None:
        manifest = self.directory / "identity.json"
        if not manifest.exists():
            return
        identity = cast("_Identity", json.loads(read_published_text(manifest)))
        try:
            process = _open_process(identity["pid"])
        except pywintypes.error as error:
            if error.winerror != winerror.ERROR_INVALID_PARAMETER:
                raise
        else:
            self._reuse_owned_process(root, identity, process, maintenance=maintenance)

    def _reuse_owned_process(self, root: Path, identity: _Identity, process: int, *, maintenance: bool) -> None:
        job: int | None = None
        try:
            if _alive(process) and str(_process_times(process)["CreationTime"]) == identity["created"]:
                job = _open_job(win32job.JOB_OBJECT_ALL_ACCESS, _NO_INHERIT, identity["job"])
                # 独占租约锁已持有；先证明工作区及真实 Job 归属，再决定复用或版本切换。
                # 不能仅凭旧 manifest 的 PID 就回收其他会话。
                _verify_job_identity(root, identity, process, job, maintenance=True)
                if not maintenance and (
                    identity["version"] != _version() or identity.get("system_javabase") != _system_javabase(os.environ)
                ):
                    _ = _verify_native(self.directory, job, required=False)
                    terminate_job(job, 1)
                    deadline = time.monotonic() + 5.0
                    while cast("int", _query_job(job, win32job.JobObjectBasicAccountingInformation)["ActiveProcesses"]):
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            message = "旧启动身份 Bazel 服务 Job 尚未全部退出，拒绝重建。"
                            raise RuntimeError(message)
                        time.sleep(min(0.01, remaining))
                    return
                self.job, self.process = job, process
                job = None
        finally:
            if job is not None:
                win32api.CloseHandle(job)
            if self.process is not process:
                win32api.CloseHandle(process)

    def _start(self, root: Path) -> None:
        name = "Local\\JchToolsBazel-" + uuid.uuid4().hex
        self.job = _create_job(None, name)
        limits = _query_job(self.job, win32job.JobObjectExtendedLimitInformation)
        basic = cast("dict[str, int]", limits["BasicLimitInformation"])
        basic["LimitFlags"] |= win32job.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        _set_job(self.job, win32job.JobObjectExtendedLimitInformation, limits)
        env = os.environ | {
            "PYTHONPATH": str(Path(__file__).resolve().parents[1]) + os.pathsep + os.environ.get("PYTHONPATH", "")
        }
        command = subprocess.list2cmdline(
            [sys.executable, "-m", "scripts.bazel_service", "--broker", str(self.directory), name]
        )
        self.process, thread, pid, _ = _create_process(
            None,
            command,
            None,
            None,
            _NO_INHERIT,
            win32con.CREATE_SUSPENDED | win32con.CREATE_NO_WINDOW,
            env,
            str(root),
            win32process.STARTUPINFO(),
        )
        try:
            win32job.AssignProcessToJobObject(self.job, self.process)
            _publish(
                self.directory / "identity.json",
                {
                    "pid": pid,
                    "job": name,
                    "created": str(_process_times(self.process)["CreationTime"]),
                    "root": str(root.resolve()),
                    "version": _version(),
                    "system_javabase": _system_javabase(os.environ),
                },
            )
            _ = _resume_thread(thread)
        except BaseException:
            win32api.TerminateProcess(self.process, 1)
            raise
        finally:
            win32api.CloseHandle(thread)

    def close(self, *, success: bool, timeout: float = 0.0) -> None:
        try:
            if self.job is not None:
                if success:
                    try:
                        _publish(self.lease, {"released": True})
                    except BaseException:
                        # 释放失败不再是成功轮；不能留下仍可接收请求的所属服务。
                        terminate_job(self.job, 1)
                        raise
                else:
                    terminate_job(self.job, 1)
                    deadline = time.monotonic() + max(0.0, timeout)
                    while timeout > 0 and cast(
                        "int", _query_job(self.job, win32job.JobObjectBasicAccountingInformation)["ActiveProcesses"]
                    ):
                        if time.monotonic() >= deadline:
                            message = "清理截止前未能确认 Bazel 服务 Job 的全部进程已终止。"
                            raise RuntimeError(message)
                        time.sleep(min(0.01, max(0.0, deadline - time.monotonic())))
        finally:
            if self.process is not None:
                win32api.CloseHandle(self.process)
                self.process = None
            if self.job is not None:
                win32api.CloseHandle(self.job)
                self.job = None
            self.lock.close()


def bind_requests(lease: Path, requests: Path) -> None:
    value = cast("_Lease", json.loads(read_published_text(lease)))
    if value.get("released"):
        message = "Bazel 租约已释放。"
        raise RuntimeError(message)
    value["requests"] = str(requests)
    _publish(lease, value)


_SERVER_PID_MAX_DIGITS = 10


class ServerPidPendingError(RuntimeError):
    """原生 server 已创建 PID 文件，但尚未写入内容。."""


def read_server_pid(path: Path) -> int | None:
    """读取可被 Bazel 替换的 PID 快照；共享冲突与未知权限错误必须由调用方区分。."""
    try:
        handle = win32file.CreateFile(
            str(path),
            win32con.GENERIC_READ,
            win32con.FILE_SHARE_READ | win32con.FILE_SHARE_WRITE | win32con.FILE_SHARE_DELETE,
            None,
            win32con.OPEN_EXISTING,
            win32con.FILE_ATTRIBUTE_NORMAL,
            None,
        )
    except pywintypes.error as error:
        if error.winerror in {winerror.ERROR_FILE_NOT_FOUND, winerror.ERROR_PATH_NOT_FOUND}:
            return None
        raise
    try:
        _, data = win32file.ReadFile(int(handle), 64)
    except pywintypes.error as error:
        if error.winerror != winerror.ERROR_HANDLE_EOF:
            raise
        data = b""
    finally:
        win32api.CloseHandle(int(handle))
    text = cast("bytes", data).decode("ascii").strip()
    if not text:
        message = "Bazel server PID 尚未发布"
        raise ServerPidPendingError(message)
    if not text.isdecimal() or len(text) > _SERVER_PID_MAX_DIGITS or int(text) <= 0:
        message = "Bazel server PID 文件无效"
        raise RuntimeError(message)
    return int(text)


def _verify_native(directory: Path, job: int, *, required: bool) -> tuple[int, str] | None:
    pid = read_server_pid(directory.parent / "output/base/server/server.pid.txt")
    if pid is None:
        if required:
            message = "Bazel 已完成，但未发布原生服务 PID。"
            raise RuntimeError(message)
        return None
    try:
        process = _open_process(pid)
    except pywintypes.error as error:
        if error.winerror == winerror.ERROR_INVALID_PARAMETER and not required:
            return None
        raise
    try:
        return _native_identity(pid, process, job, required=required)
    finally:
        win32api.CloseHandle(process)


def _native_identity(pid: int, process: int, job: int, *, required: bool) -> tuple[int, str] | None:
    alive = _alive(process)
    if alive and not _belongs(process, job):
        message = "拒绝使用不属于独占服务 Job 的未知存活 Bazel 服务。"
        raise RuntimeError(message)
    if required and not alive:
        message = "发布完成结果前，原生 Bazel 服务已经退出。"
        raise RuntimeError(message)
    return (pid, str(_process_times(process)["CreationTime"])) if alive else None


def _lease_owner(lease: _Lease) -> int:
    owner_pid = lease.get("owner")
    owner_created = lease.get("owner_created")
    if owner_pid is None or owner_created is None:
        message = "活动租约缺少不可变的所属进程身份。"
        raise RuntimeError(message)
    owner = _open_process(owner_pid)
    try:
        if str(_process_times(owner)["CreationTime"]) == owner_created:
            return owner
    except BaseException:
        win32api.CloseHandle(owner)
        raise
    win32api.CloseHandle(owner)
    message = "租约所属进程的 PID 身份已改变。"
    raise RuntimeError(message)


def _dispatch_request(request: Path, owner: int) -> int:
    config = cast("_Request", json.loads(read_published_text(request)))
    client = request.parent / "client.json"
    env = config["environment"] | cast("dict[str, str]", json.loads(read_published_text(client)))
    console = request.parent.parent / f"bazel-{request.name.split('.')[0]}.console.log"
    with console.open("xb") as sink:
        proc = subprocess.Popen(
            [sys.executable, "-m", "scripts.bazel_gate", "--dispatch", str(request)],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=sink,
            stderr=subprocess.STDOUT,
        )
        while proc.poll() is None:
            if not _alive(owner):
                return 1
            time.sleep(0.01)
    return cast("int", proc.returncode)


@final
class _Broker:
    """保留跨租约的原生服务身份，同时逐轮监督所属 CLI。."""

    def __init__(self, directory: Path, job: int) -> None:
        self.directory = directory
        self.job = job
        self.selected: Path | None = None
        self.owner: int | None = None
        self.sequence = 1
        self.last = time.monotonic()
        self.startup: tuple[list[str], str, str] | None = None
        self.server: tuple[int, str] | None = None

    def close(self) -> None:
        if self.owner is not None:
            win32api.CloseHandle(self.owner)
            self.owner = None

    def request(self) -> Path | None:
        active = self.directory / "active.json"
        if not active.exists():
            return None
        selected = Path(cast("dict[str, str]", json.loads(read_published_text(active)))["lease"])
        if selected != self.selected:
            self.close()
            self.selected, self.sequence = selected, 1
        lease = cast("_Lease", json.loads(read_published_text(selected)))
        if lease.get("released"):
            return None
        self.last = time.monotonic()
        if self.owner is None:
            self.owner = _lease_owner(lease)
        if not _alive(self.owner):
            message = "活动租约所属 CLI 已退出。"
            raise RuntimeError(message)
        requests = lease.get("requests")
        if not requests:
            return None
        request = Path(requests) / f"{self.sequence:03d}.request.json"
        return request if request.exists() and (request.parent / "client.json").exists() else None

    def execute(self, request: Path) -> int:
        config = cast("_Request", json.loads(read_published_text(request)))
        argv = config["argv"]
        command_index = next(i for i, value in enumerate(argv) if value in {"build", "test"})
        javabase = _system_javabase(config["environment"])
        if javabase != _system_javabase(os.environ):
            message = "请求的原生 Java 启动环境与独占服务租约不一致，拒绝派发。"
            raise RuntimeError(message)
        identity = (argv[:command_index], hashlib.sha256(Path(argv[0]).read_bytes()).hexdigest(), javabase)
        if self.startup is not None and identity != self.startup:
            message = "热服务运行期间 Bazel 的启动参数、Java 环境或二进制身份改变。"
            raise RuntimeError(message)
        self.startup = identity
        self._verify_server(required=False)
        if self.owner is None:
            message = "派发请求前缺少所属 CLI 句柄。"
            raise RuntimeError(message)
        code = _dispatch_request(request, self.owner)
        if code == 0:
            self._verify_server(required=True)
        _publish(request.with_suffix(".complete.json"), code)
        self.sequence += 1
        return code

    def _verify_server(self, *, required: bool) -> None:
        observed = _verify_native(self.directory, self.job, required=required)
        if self.server is not None and observed != self.server:
            message = "热请求之间原生 Bazel 服务身份改变。"
            raise RuntimeError(message)
        if required:
            self.server = observed


def _broker(directory: Path, name: str) -> int:
    # 空闲期间仍持有命名 Job，不能由本轮 worker 的 Job 持有服务。
    job = _open_job(win32job.JOB_OBJECT_ALL_ACCESS, _NO_INHERIT, name)
    broker = _Broker(directory, job)
    try:
        while time.monotonic() - broker.last < _IDLE_SECONDS:
            request = broker.request()
            if request is None:
                time.sleep(0.01)
                continue
            code = broker.execute(request)
            if code:
                return code
    except BaseException:
        traceback.print_exc()
        _ = sys.stderr.flush()
        raise
    finally:
        # 首错、所属进程死亡和空闲过期均终止全部继承的原生后代。
        broker.close()
        terminate_job(job, 1)
        win32api.CloseHandle(job)
    return 0


def stop(root: Path) -> None:
    """持有工作区独占租约，维护性停止已验证的服务。."""
    lease = ServiceLease(root, maintenance=True)
    native: int | None = None
    try:
        if lease.job is not None:
            pid = read_server_pid(root / ".tmp/bazel-gate/output/base/server/server.pid.txt")
            if pid is not None:
                try:
                    native = _open_process(pid)
                except pywintypes.error as error:
                    if error.winerror != winerror.ERROR_INVALID_PARAMETER:
                        raise
                if native is not None and not _belongs(native, lease.job):
                    win32api.CloseHandle(native)
                    native = None
    finally:
        try:
            lease.close(success=False, timeout=5.0)
            if native is not None and win32event.WaitForSingleObject(native, 5000) != win32event.WAIT_OBJECT_0:
                message = "未能确认隔离原生服务已终止。"
                raise RuntimeError(message)
        finally:
            if native is not None:
                win32api.CloseHandle(native)


if __name__ == "__main__":
    if len(sys.argv) == _BROKER_ARGC and sys.argv[1] == "--broker":
        log_name = f"broker-pid-{os.getpid()}-{uuid.uuid4().hex}.log"
        sys.stdout = sys.stderr = (Path(sys.argv[2]) / log_name).open("x", encoding="utf-8")
        raise SystemExit(_broker(Path(sys.argv[2]), sys.argv[3]))
    if len(sys.argv) == _STOP_ARGC and sys.argv[1] == "--stop":
        stop(Path(sys.argv[2]).resolve())
    else:
        message = "请使用 --stop ROOT 执行隔离维护。"
        raise SystemExit(message)
