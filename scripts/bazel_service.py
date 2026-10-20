"""工作区独占的持久 Bazel 服务；每轮只回收已验证的非持久任务进程。."""

from __future__ import annotations

import ctypes
import hashlib
import json
import msvcrt
import os
import struct
import subprocess
import sys
import time
import traceback
import uuid
from ctypes import wintypes
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

from scripts import process_runtime
from scripts.gate_runtime import read_published_text

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping

_NO_INHERIT = False
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
    runtime: str


_open_job = cast("Callable[[int, bool, str], int]", vars(win32job)["OpenJobObject"])
_create_job = cast("Callable[[object, str], int]", vars(win32job)["CreateJobObject"])
_query_job = cast("Callable[[int, int], dict[str, object]]", vars(win32job)["QueryInformationJobObject"])
terminate_job = cast("Callable[[int, int], None]", vars(win32job)["TerminateJobObject"])
_belongs = cast("Callable[[int, int], bool]", vars(win32job)["IsProcessInJob"])
_process_times = cast("Callable[[int], dict[str, object]]", vars(win32process)["GetProcessTimes"])
_process_id = cast("Callable[[int], int]", vars(win32process)["GetProcessId"])
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
    for name in ("bazel_service.py", "bazel_gate.py", "process_runtime.py"):
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
    """独占服务租约；释放、失败及取消均不终止 broker 或原生 server。."""

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
            self.close()
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
                # 不兼容的已存在 broker 可能仍运行旧版生命周期代码；绝不隐式替换或杀服。
                _verify_job_identity(root, identity, process, job, maintenance=True)
                if not maintenance and (
                    identity["version"] != _version() or identity.get("system_javabase") != _system_javabase(os.environ)
                ):
                    message = "已有 Bazel 服务实现或 Java 启动环境不兼容；请单独执行维护停止，未终止已有服务。"
                    raise RuntimeError(message)
                self.job, self.process = job, process
                job = None
        finally:
            if job is not None:
                win32api.CloseHandle(job)
            if self.process is not process:
                win32api.CloseHandle(process)

    def _start(self, root: Path) -> None:
        pid = read_server_pid(self.directory.parent / "output/base/server/server.pid.txt")
        if pid is not None:
            try:
                native = _open_process(pid)
            except pywintypes.error as error:
                if error.winerror != winerror.ERROR_INVALID_PARAMETER:
                    raise
            else:
                try:
                    if _alive(native):
                        message = "已有存活 Bazel server 无法通过兼容 broker 复用，拒绝启动或隐式重启。"
                        raise RuntimeError(message)
                finally:
                    win32api.CloseHandle(native)
        name = "Local\\JchToolsBazel-" + uuid.uuid4().hex
        self.job = _create_job(None, name)
        # broker/server 不属于本轮清理范围；broker 异常退出或最后句柄关闭也不能杀服。
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

    def close(self, *, timeout: float = 0.0) -> None:
        """任何结果均释放租约并清理任务，不决定服务存活。."""
        try:
            if self.job is not None:
                try:
                    _publish(self.lease, {"released": True})
                finally:
                    if self.process is not None and not terminate_requests(
                        self.directory, self.job, self.process, timeout=timeout
                    ):
                        message = "本轮 Bazel 任务未在清理截止前确认退出；持久服务未被终止。"
                        raise RuntimeError(message)
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


_query_job_information = cast(
    "Callable[[int, int, object, int, object], int]",
    ctypes.WINFUNCTYPE(
        wintypes.BOOL,
        wintypes.HANDLE,
        ctypes.c_int,
        ctypes.c_void_p,
        wintypes.DWORD,
        ctypes.POINTER(wintypes.DWORD),
        use_last_error=True,
    )(("QueryInformationJobObject", ctypes.WinDLL("kernel32", use_last_error=True))),
)


def job_process_ids(job: int) -> tuple[int, ...]:
    """按内核实际填写的 PID 数采样；成员增长时扩容，不截断本轮任务。."""
    capacity = 16
    pointer_size = ctypes.sizeof(ctypes.c_void_p)
    while True:
        size = 8 + capacity * pointer_size
        buffer = ctypes.create_string_buffer(size)
        written = wintypes.DWORD()
        ok = _query_job_information(
            int(job), win32job.JobObjectBasicProcessIdList, ctypes.byref(buffer), size, ctypes.byref(written)
        )
        assigned, listed = struct.unpack("<II", ctypes.string_at(buffer, 8))
        if ok:
            if listed > capacity:
                message = "Windows Job 返回的 PID 数超出接收缓冲区"
                raise OSError(message)
            data = ctypes.string_at(ctypes.addressof(buffer) + 8, listed * pointer_size)
            return cast("tuple[int, ...]", struct.unpack(f"{listed}P", data))
        error = ctypes.get_last_error()
        if error != winerror.ERROR_MORE_DATA:
            raise ctypes.WinError(error)
        capacity = max(capacity * 2, assigned + 16)


def _terminate_member(job: int, pid: int, directory: Path | None) -> int | None:
    try:
        handle = win32api.OpenProcess(
            win32con.SYNCHRONIZE | win32con.PROCESS_QUERY_INFORMATION | win32con.PROCESS_TERMINATE,
            _NO_INHERIT,
            pid,
        )
    except pywintypes.error as error:
        if error.winerror != winerror.ERROR_INVALID_PARAMETER:
            raise
        return None
    try:
        if _belongs(handle, job) and _alive(handle):
            if directory is not None:
                image = process_runtime.process_image(int(handle))
                # Job 内的 conhost 只为本轮派生的控制台进程服务；Job 归属已验证，
                # broker/server 不使用控制台，回收它不会终止持久服务或遗漏工作进程。
                reapable = image.name.lower() == "conhost.exe" or process_runtime.is_bazel_client(
                    image,
                    process_runtime.process_argv(int(handle)),
                    output_base=directory.parent / "output/base",
                    output_root=Path(__file__).resolve().parents[1] / ".tmp/bazel-gate/output",
                )
                if not reapable:
                    win32api.CloseHandle(handle)
                    return None
            try:
                win32api.TerminateProcess(handle, 1)
            except pywintypes.error as error:
                # 并行清理已开始终止时 Windows 可返回拒绝访问；不能据此冒报退出。
                # 仍持有真实进程句柄，由外层在同一截止内确认其终态。
                if error.winerror != winerror.ERROR_ACCESS_DENIED and _alive(handle):
                    raise
    except BaseException:
        win32api.CloseHandle(handle)
        raise
    return handle


def terminate_requests(directory: Path, job: int, broker: int, *, timeout: float) -> bool:
    """仅终止已验证服务 Job 中非 broker/server 的本轮成员；未知 server 身份时拒绝猜测。."""
    deadline = time.monotonic() + max(0.0, timeout)
    broker_pid = _process_id(broker)
    handles: dict[int, int] = {}
    try:
        while True:
            try:
                native = _verify_native(directory, job, required=False)
            except ServerPidPendingError:
                native = None
            protected = {broker_pid}
            if native is not None:
                protected.add(native[0])
            members = set(job_process_ids(job)) - protected
            if not members:
                return all(not _alive(handle) for handle in handles.values())
            # PID 尚未发布时也回收可证明的 client/proxy，保留潜在 Java server。
            for pid in members:
                if pid in handles:
                    continue
                handle = _terminate_member(job, pid, directory if native is None else None)
                if handle is not None:
                    handles[pid] = handle
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            time.sleep(min(0.01, remaining))
    finally:
        for handle in handles.values():
            win32api.CloseHandle(handle)


def _dispatch_request(request: Path, owner: int) -> int:
    config = cast("_Request", json.loads(read_published_text(request)))
    client = request.parent / "client.json"
    env = config["environment"] | cast("dict[str, str]", json.loads(read_published_text(client)))
    console = request.parent.parent / f"bazel-{request.name.split('.')[0]}.console.log"

    def cancelled() -> bool:
        lease_path = env.get("JCHTOOLS_GATE_BAZEL_SERVICE_LEASE")
        released = lease_path and cast("_Lease", json.loads(read_published_text(Path(lease_path)))).get("released")
        failure = env.get("JCHTOOLS_GATE_FAILURE_FILE")
        return bool(
            not _alive(owner)
            or released
            or (request.parent / "cancelled").exists()
            or (failure and Path(failure).exists())
        )

    # wrapper 仅发布运行材料；原生 client 必须由持久 broker 直接创建、持有和回收。
    # 因此首次 server PID 尚未发布时也能终止已知 client，绝不猜测哪个 Java 进程是 server。
    commands = [
        [sys.executable, "-m", "scripts.bazel_gate", "--dispatch", str(request)],
        config["argv"],
    ]
    with console.open("xb") as sink:
        for index, argv in enumerate(commands):
            if cancelled():
                return 1
            role = "prepare" if index == 0 else "client"
            _ = sink.write(f"BAZEL PROCESS start role={role}\n".encode())
            sink.flush()
            proc = subprocess.Popen(
                argv,
                cwd=Path(config["runtime"]).parents[2],
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=sink,
                stderr=subprocess.STDOUT,
                # 持久 broker 没有控制台；子进程也不分配跨轮共享的隐式控制台。
                creationflags=subprocess.CREATE_NO_WINDOW,
            )
            try:
                while proc.poll() is None:
                    if cancelled():
                        return 1
                    time.sleep(0.01)
            finally:
                if proc.poll() is None:
                    proc.terminate()
                    _ = proc.wait(timeout=1.0)
            _ = sink.write(f"BAZEL PROCESS exit role={role} pid={proc.pid} code={proc.returncode}\n".encode())
            sink.flush()
            if proc.returncode:
                return cast("int", proc.returncode)
    return 0


@final
class _Broker:
    """保留跨租约的原生服务身份，同时逐轮监督所属 CLI。."""

    def __init__(self, directory: Path, job: int) -> None:
        self.directory = directory
        self.job = job
        self.selected: Path | None = None
        self.owner: int | None = None
        self.sequence = 1
        self.startup: tuple[list[str], str, str] | None = None
        self.server: tuple[int, str] | None = None
        self.blocked = False

    def close(self) -> None:
        if self.owner is not None:
            win32api.CloseHandle(self.owner)
            self.owner = None

    def release(self) -> None:
        """封闭当前请求入口，再回收本轮任务；清理失败仍不得关闭持久服务。."""
        self.blocked = True
        try:
            if self.selected is not None:
                _publish(self.selected, {"released": True})
        finally:
            try:
                if not terminate_requests(self.directory, self.job, win32api.GetCurrentProcess(), timeout=2.0):
                    message = "本轮 Bazel 任务未全部退出，拒绝后续派发。"
                    raise RuntimeError(message)
            finally:
                self.close()
        self.blocked = False

    def request(self) -> Path | None:
        if self.blocked:
            self.release()
            return None
        active = self.directory / "active.json"
        if not active.exists():
            return None
        selected = Path(cast("dict[str, str]", json.loads(read_published_text(active)))["lease"])
        if selected != self.selected:
            self.close()
            self.selected, self.sequence = selected, 1
        lease = cast("_Lease", json.loads(read_published_text(selected)))
        if lease.get("released"):
            if self.owner is not None:
                self.release()
            return None
        if self.owner is None:
            self.owner = _lease_owner(lease)
        if not _alive(self.owner):
            message = "活动租约所属 CLI 已退出。"
            raise RuntimeError(message)
        requests = lease.get("requests")
        if not requests:
            return None
        if (Path(requests) / "cancelled").exists():
            self.release()
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
        try:
            code = _dispatch_request(request, self.owner)
            self._verify_server(required=code == 0)
        finally:
            if not terminate_requests(self.directory, self.job, win32api.GetCurrentProcess(), timeout=2.0):
                message = "Bazel 请求结束后仍有本轮任务，拒绝发布通过结果。"
                raise RuntimeError(message)
        _publish(request.with_suffix(".complete.json"), code)
        self.sequence += 1
        if code:
            self.release()
        return code

    def _verify_server(self, *, required: bool) -> None:
        observed = _verify_native(self.directory, self.job, required=required)
        if observed is None:
            return
        # 观察值已通过 Job 归属验证：身份变化只可能是本工作区 server 崩溃后由
        # 客户端在 Job 内重启的接任者；未知或外部 server 会在归属检查处拒绝。
        self.server = observed


def _broker(directory: Path, name: str) -> int:
    # 空闲期间仍持有命名 Job，不能由本轮 worker 的 Job 持有服务。
    job = _open_job(win32job.JOB_OBJECT_ALL_ACCESS, _NO_INHERIT, name)
    broker = _Broker(directory, job)
    try:
        while True:
            request: Path | None = None
            try:
                request = broker.request()
                if request is not None:
                    _ = broker.execute(request)
                    continue
            except Exception:  # noqa: BLE001 - 本轮异常必须失败关闭请求入口，而非杀死持久服务。
                traceback.print_exc()
                _ = sys.stderr.flush()
                try:
                    try:
                        if request is not None:
                            _publish(request.with_suffix(".complete.json"), 1)
                    finally:
                        broker.release()
                except Exception:  # noqa: BLE001 - 未清理完成时保持 broker 存活并禁止后续派发。
                    traceback.print_exc()
                    _ = sys.stderr.flush()
            time.sleep(0.01)
    finally:
        broker.close()
        win32api.CloseHandle(job)


def _stop_service_job(job: int) -> None:
    terminate_job(job, 1)
    deadline = time.monotonic() + 5.0
    while cast("int", _query_job(job, win32job.JobObjectBasicAccountingInformation)["ActiveProcesses"]):
        if time.monotonic() >= deadline:
            message = "维护停止未能确认服务 Job 的全部进程退出。"
            raise RuntimeError(message)
        time.sleep(0.01)


def stop(root: Path) -> None:
    """持有工作区独占租约，维护性停止已验证的服务。."""
    lease = ServiceLease(root, maintenance=True)
    native: int | None = None
    pid_file = root / ".tmp/bazel-gate/output/base/server/server.pid.txt"
    try:
        if lease.job is not None:
            # 仅显式维护命令有权终止服务 Job；普通租约 close 永远不走此路径。
            pid = read_server_pid(pid_file)
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
            if lease.job is not None:
                _stop_service_job(lease.job)
            lease.close(timeout=5.0)
            if native is not None and win32event.WaitForSingleObject(native, 5000) != win32event.WAIT_OBJECT_0:
                message = "未能确认隔离原生服务已终止。"
                raise RuntimeError(message)
            # 残留的 server PID 记录会被 Windows PID 复用冒充存活服务，阻断下一轮
            # 客户端与租约校验；维护停止确认终止后必须删除。
            pid_file.unlink(missing_ok=True)
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
