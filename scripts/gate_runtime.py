"""本地测试门命令监督器：Bazel 负责调度，本层保留所属进程树和硬预算."""

from __future__ import annotations

import dataclasses
import json
import math
import os
import re
import subprocess
import tempfile
import time
import uuid
from collections import deque
from pathlib import Path
from typing import TYPE_CHECKING, NotRequired, Protocol, TypedDict, cast, final

import pywintypes
import win32api
import win32con
import win32file
import winerror

if TYPE_CHECKING:
    from collections.abc import Callable
    from typing import IO, TypeIs, Unpack

STATUS_OK = "PASS"
STATUS_FAILED = "FAIL"
STATUS_TIMED_OUT = "TIMEOUT"
STATUS_UNVERIFIED = "UNVERIFIED"
_CEILINGS = {"fastcheck": 60.0, "fulltest": 900.0, "slowtest": 1500.0}
_POLL_SECONDS = 0.02
_TAIL_LINES = 8
_TEST_FAILURE_SUMMARY_LIMIT = 1024
_parse_json: Callable[[str], object] = json.loads


class _StartCommand(Protocol):
    def __call__(
        self, argv: list[str], *, stdout: IO[bytes] | int, env: dict[str, str] | None
    ) -> subprocess.Popen[bytes]: ...


class _StopCommand(Protocol):
    def __call__(self, proc: subprocess.Popen[bytes], *, timeout: float) -> bool: ...


class _Callbacks(TypedDict):
    start_command: _StartCommand
    close_command: _StopCommand
    kill_tree: _StopCommand
    compiler_ids: Callable[[subprocess.Popen[bytes]], frozenset[int]]
    clock: NotRequired[Callable[[], float]]
    observe_tree: NotRequired[Callable[[subprocess.Popen[bytes]], None]]
    validate_result: NotRequired[Callable[[Result], Result]]
    progress: NotRequired[Callable[[float, str], None] | None]


@dataclasses.dataclass
class Stage:
    name: str
    argv: list[str]
    kind: str = "check"
    env: dict[str, str] | None = None
    phase_file: Path | None = None
    exclusive_resource: str | None = None
    watch_test_failures: bool = False


@dataclasses.dataclass
class Result:
    name: str
    status: str
    detail: str
    log: Path | None = None
    elapsed: float = 0.0
    returncode: int | None = None


class GateStoppedError(RuntimeError):
    """阶段报错后的控制流出口；携带原始结果供统一清理与汇总。."""

    def __init__(self, results: list[Result]) -> None:
        super().__init__("阶段报错，已停止测试门")
        self.results: list[Result] = results


@dataclasses.dataclass
class _Running:
    stage: Stage
    process: subprocess.Popen[bytes]
    sink: IO[bytes]
    log: Path
    started: float
    output_offset: int = 0
    test_failure: str | None = None


def _is_mapping(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


def read_published_text(path: Path) -> str:
    """读取已关闭写方并原子发布的 UTF-8 快照；允许仍存活的 Windows 删除/写共享句柄。.

    共享标志不改变发布协议：写方必须先关闭 pending，再 replace；缺失、权限、
    编码及内容错误仍交由消费者 fail closed，不将它们当作可重试的 Bazel 命令。
    """
    handle = win32file.CreateFile(
        str(path),
        win32con.GENERIC_READ,
        win32con.FILE_SHARE_READ | win32con.FILE_SHARE_WRITE | win32con.FILE_SHARE_DELETE,
        None,
        win32con.OPEN_EXISTING,
        win32con.FILE_ATTRIBUTE_NORMAL,
        None,
    )
    chunks: list[bytes] = []
    try:
        while True:
            try:
                _, payload = win32file.ReadFile(int(handle), 64 * 1024)
            except pywintypes.error as error:
                if error.winerror != winerror.ERROR_HANDLE_EOF:
                    raise
                break
            data = cast("bytes", cast("object", payload))
            if not data:
                break
            chunks.append(data)
    finally:
        win32api.CloseHandle(int(handle))
    return b"".join(chunks).decode("utf-8")


def _compiling(path: Path | None) -> bool:
    if path is None:
        return False
    try:
        value = _parse_json(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False
    return _is_mapping(value) and value.get("compiling") is True


def _tail(path: Path) -> str:
    try:
        with path.open(encoding="utf-8", errors="replace") as source:
            return "".join(deque(source, maxlen=_TAIL_LINES)).rstrip()
    except OSError as error:
        return f"无法读取完整日志：{error}"


def process_log_directory(base: Path) -> Path:
    """每次调用分配新的进程级日志命名空间，不复用或删除历史证据。."""
    return base / f"pid-{os.getpid()}-{uuid.uuid4().hex}"


@final
class GateRun:
    """从初始化到最终汇总共享一个时钟，包含准备、各次 run、清理与报告."""

    def __init__(
        self,
        level: str,
        limit: float,
        root: Path,
        log_dir: Path,
        **callbacks: Unpack[_Callbacks],
    ) -> None:
        self._clock = callbacks.get("clock", time.monotonic)
        self._started = self._clock()
        self._last = self._started
        self.level = level
        self.limit = limit
        self.root = root
        self.log_dir = process_log_directory(log_dir)
        self.compile_excluded = 0.0
        self.total = 0.0
        self._start_command = callbacks["start_command"]
        self._close_command = callbacks["close_command"]
        self._kill_tree = callbacks["kill_tree"]
        self._compiler_ids = callbacks["compiler_ids"]
        self._observe_tree = callbacks.get("observe_tree")
        self._validate_result = callbacks.get("validate_result")
        self._progress = callbacks.get("progress")
        self._active: list[_Running] = []
        self._persistent: _Running | None = None
        self._observed: dict[int, frozenset[int]] = {}
        self._results: list[Result] = []
        self._timed_out = False
        self._interrupted = False
        self._failure: str | None = None
        self._finished: int | None = None
        self._temporary: tempfile.TemporaryDirectory[str] | None = None
        self._sequence = 0
        self._reserve = min(2.0, max(0.0, limit * 0.1))
        ceiling = _CEILINGS.get(level)
        self._valid = ceiling is not None and math.isfinite(limit) and 0.0 < limit <= ceiling
        if not self._valid:
            self._results.append(Result("gate-limit", STATUS_FAILED, "层级未知或预算无效；预算只能调低"))
        print(f"日志目录：{self.log_dir}", flush=True)

    def _cleanup_temporary(self) -> None:
        if self._temporary is None:
            return
        # 仅在回收时用原生路径；TEMP 保持普通路径，不改变 Git 等消费者。
        # 普通 Win32 路径会折叠尾点/尾空格，无法删除 AH-04 回归的真实目录。
        name = self._temporary.name
        if os.name == "nt" and not name.startswith("\\\\?\\"):
            self._temporary.name = "\\\\?\\UNC\\" + name[2:] if name.startswith("\\\\") else "\\\\?\\" + name
        self._temporary.cleanup()

    def prepare_temporary(self) -> None:
        """建立本门独占的仓库外 tempfile，避免触发 Git 整树保护."""
        if self._temporary is not None:
            message = "运行期临时目录已经建立。"
            raise RuntimeError(message)
        parent = Path(tempfile.gettempdir()).resolve()
        if parent.is_relative_to(self.root.resolve()):
            parent = self.root.parent
        self._temporary = tempfile.TemporaryDirectory(prefix="jchtools-gate-", dir=parent)

    @property
    def budgeted(self) -> float:
        return max(0.0, self.total - self.compile_excluded)

    @property
    def remaining(self) -> float:
        self._charge()
        return max(0.0, self.limit - self.budgeted)

    def _account(self, observed: dict[int, frozenset[int]]) -> None:
        now = self._clock()
        interval = max(0.0, now - self._last)
        continuous = (
            bool(observed)
            and observed.keys() == self._observed.keys()
            and all(ids & self._observed[pid] for pid, ids in observed.items())
        )
        if continuous:
            self.compile_excluded += interval
        self.total = max(0.0, now - self._started)
        self._last = now
        self._observed = observed
        if self._progress is not None:
            status = STATUS_TIMED_OUT if self._timed_out else STATUS_FAILED if self._interrupted else ""
            self._progress(self.compile_excluded, status)

    def _charge(self) -> None:
        # 准备、活跃阶段切换、清理和空闲区间均收费，不沿用上次编译豁免。
        self._account({})

    def _probe(self, proc: subprocess.Popen[bytes]) -> frozenset[int]:
        try:
            return self._compiler_ids(proc)
        except Exception as error:
            message = f"观察所属编译器进程失败：{error}"
            raise RuntimeError(message) from error

    def _retain_process_identities(self) -> None:
        if self._observe_tree is not None:
            for running in self._active:
                self._observe_tree(running.process)

    def _sample(self) -> None:
        self._retain_process_identities()
        observed: dict[int, frozenset[int]] = {}
        eligible = bool(self._active)
        for running in self._active:
            proc = running.process
            phase = running.stage.phase_file
            if phase is not None and not phase.is_absolute():
                phase = self.root / phase
            candidate = running.stage.kind in {"compile", "compiler-observed"} or (
                running.stage.kind == "mixed" and _compiling(phase)
            )
            if not candidate:
                self._account({})
                return
            ids = self._probe(proc)
            if not ids or proc.poll() is not None:
                eligible = False
            observed[proc.pid] = ids
        self._account(observed if eligible else {})

    def _expired(self) -> bool:
        exhausted = self.budgeted >= self.limit
        cleanup_due = self.budgeted >= self.limit - self._reserve and any(
            running.process.poll() is None for running in self._active
        )
        if exhausted or cleanup_due:
            self._timed_out = True
        return self._timed_out

    def _log_path(self, stage: Stage) -> Path:
        self._sequence += 1
        safe_name = re.sub(r"[^\w.-]", "_", stage.name)
        return self.log_dir / f"{self._sequence:03d}-{safe_name}.log"

    def _start(self, stage: Stage, sink: IO[bytes]) -> subprocess.Popen[bytes]:
        try:
            env = os.environ | (stage.env or {})
            if self._temporary is not None:
                env.update(TEMP=self._temporary.name, TMP=self._temporary.name)
            return self._start_command(stage.argv, stdout=sink, env=env)
        except OSError:
            raise
        except Exception as error:
            message = f"所属命令启动失败：{error}"
            raise RuntimeError(message) from error

    def _launch(self, stage: Stage) -> Result | None:
        print(f"RUN {stage.name} kind={stage.kind}", flush=True)
        self._charge()
        log = self._log_path(stage)
        started = self._clock()
        try:
            self.log_dir.mkdir(parents=True, exist_ok=True)
            sink = log.open("xb")
        except OSError as error:
            return Result(stage.name, STATUS_FAILED, f"无法创建阶段日志：{error}")
        self._charge()
        if self._expired():
            sink.close()
            return Result(stage.name, STATUS_TIMED_OUT, "实际启动前共享非编译预算已耗尽", log)
        try:
            command = dataclasses.replace(
                stage, env=(stage.env or {}) | {"JCHTOOLS_GATE_FAILURE_FILE": str(log.with_suffix(".failure.jsonl"))}
            )
            if stage.watch_test_failures:
                command = dataclasses.replace(
                    command, argv=[*stage.argv, "--logfile", str(log.with_suffix(".test-results"))]
                )
            proc = self._start(command, sink)
        except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
            _ = sink.write(f"命令启动失败：{error}\n".encode("utf-8", errors="replace"))
            sink.close()
            status = STATUS_UNVERIFIED if isinstance(error, FileNotFoundError) else STATUS_FAILED
            return Result(stage.name, status, f"命令启动失败：{error}", log)
        except KeyboardInterrupt:
            _ = sink.write("命令启动被中断\n".encode())
            sink.close()
            self._interrupted = True
            return Result(stage.name, STATUS_FAILED, "命令启动被中断", log)
        self._active.append(_Running(stage, proc, sink, log, started))
        self._charge()
        return None

    def _cleanup_timeout(self, deadline: float) -> float:
        self._charge()
        return max(0.0, min(deadline - self._clock(), self.limit - self.budgeted))

    @staticmethod
    def _stop_command(callback: _StopCommand, proc: subprocess.Popen[bytes], timeout: float) -> bool:
        try:
            return callback(proc, timeout=timeout)
        except Exception as error:
            message = f"所属命令终止回调失败：{error}"
            raise RuntimeError(message) from error

    def _terminate(self, running: _Running, deadline: float) -> tuple[bool, str]:
        proc = running.process
        confirmed = False
        detail = ""
        try:
            confirmed = self._stop_command(self._kill_tree, proc, self._cleanup_timeout(deadline))
        except RuntimeError as error:
            detail = str(error)
        except KeyboardInterrupt:
            self._interrupted = True
            detail = "终止所属进程树时被中断"
        try:
            if proc.poll() is None:
                proc.kill()
            _ = proc.wait(timeout=self._cleanup_timeout(deadline))
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            return False, f"所属进程树终止失败：{error}"
        except KeyboardInterrupt:
            self._interrupted = True
            return False, "等待所属进程树退出时被中断"
        return confirmed, detail or ("" if confirmed else "无法确认所属进程树已全部终止")

    def _close(self, running: _Running, deadline: float) -> tuple[bool, str]:
        try:
            confirmed = self._stop_command(self._close_command, running.process, self._cleanup_timeout(deadline))
        except RuntimeError as error:
            return False, f"所属命令清理失败：{error}"
        except KeyboardInterrupt:
            self._interrupted = True
            return False, "清理所属命令时被中断"
        finally:
            running.sink.close()
            self._charge()
        return confirmed, "" if confirmed else "无法确认所属子进程已全部退出"

    def _complete(self, running: _Running, *, stopped: bool, deadline: float) -> Result:
        self._charge()
        terminated, termination = self._terminate(running, deadline) if stopped else (True, "")
        closed, cleanup = self._close(running, deadline)
        if not terminated or not closed:
            self._interrupted = True  # 未确认旧进程树收尾时，不再启动可能共享资源的下一项。
        code = running.process.poll()
        status = STATUS_OK if code == 0 and terminated and closed else STATUS_FAILED
        if stopped:
            status = STATUS_TIMED_OUT if self._timed_out else STATUS_FAILED
        detail = "; ".join(part for part in (f"exit={code}", termination, cleanup) if part)
        if stopped:
            reason = (
                "共享非编译预算耗尽"
                if self._timed_out
                else (f"{self._failure} 报错，终止并行命令" if self._failure else "测试门被中断或终止")
            )
            detail = f"{reason}; {detail}"
        if running.test_failure is not None:
            detail = f"嵌套测试首次失败：{running.test_failure}; {detail}"
        result = Result(running.stage.name, status, detail, running.log, self._clock() - running.started, code)
        if self._validate_result is not None:
            try:
                result = self._validate_result(result)
            except Exception as error:  # noqa: BLE001 - 任何验证异常都必须停止调度并回收其他进程。
                result.status = STATUS_FAILED
                result.detail = f"阶段结果校验失败：{type(error).__name__}: {error}"
        self._halt_on_error(result)
        print(f"DONE {running.stage.name} status={result.status} {result.detail}", flush=True)
        return result

    def _halt_on_error(self, result: Result) -> None:
        if result.status not in {STATUS_OK, "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"}:
            first_failure = self._failure is None
            if first_failure:
                self._failure = result.name
            # 输出端可能永久阻塞；先中断调度并让独立监督器看到失败。
            self._interrupted = True
            if self._progress is not None:
                self._progress(self.compile_excluded, STATUS_TIMED_OUT if self._timed_out else STATUS_FAILED)
            if first_failure:
                print(
                    f"STOP {result.name} status={result.status}：{result.detail}；",
                    f"停止后续阶段并清理所属进程树；log={result.log}",
                    sep="",
                    flush=True,
                )

    def _observe_python_failure(self, running: _Running) -> None:
        signal = running.log.with_suffix(".failure.jsonl")
        if not signal.exists():
            return
        with signal.open("rb") as source:
            line = source.readline(8192)
        if not line.endswith(b"\n") and running.process.poll() is None:
            return
        message = _parse_json(line.decode("utf-8"))
        if not _is_mapping(message) or not isinstance(message.get("detail"), str):
            error = "Python 测试失败通知格式无效"
            raise ValueError(error)
        detail = " ".join(str(message["detail"]).split())[:_TEST_FAILURE_SUMMARY_LIMIT]
        if not detail:
            error = "Python 测试失败通知缺少诊断"
            raise ValueError(error)
        running.test_failure = detail
        self._halt_on_error(Result(running.stage.name, STATUS_FAILED, detail, running.log))

    def _observe_test_failure(self, running: _Running) -> None:
        if not running.stage.watch_test_failures:
            return
        # stable libtest 的 fail-fast 开关仍需 nightly；只观察原生结果文件，不解析测试输出。
        # failed <name> 是测试名，failed: <message> 是失败消息；摘要只保留有界单行。
        result_file = running.log.with_suffix(".test-results")
        if not result_file.exists():
            return
        with result_file.open("rb") as source:
            _ = source.seek(running.output_offset)
            while line := source.readline():
                if not line.endswith(b"\n") and running.process.poll() is None:
                    break
                running.output_offset = source.tell()
                if line.startswith((b"failed ", b"failed:")):
                    summary = " ".join(line.decode("utf-8", errors="replace").split())
                    if line.startswith(b"failed "):
                        summary = f"test {summary.removeprefix('failed ')} ... FAILED"
                    if len(summary) > _TEST_FAILURE_SUMMARY_LIMIT:
                        summary = summary[: _TEST_FAILURE_SUMMARY_LIMIT - 3] + "..."
                    running.test_failure = summary
                    self._halt_on_error(Result(running.stage.name, STATUS_FAILED, running.test_failure, running.log))
                    break
                self._charge()
                if self._expired():
                    break

    def _collect_completed(self) -> list[Result]:
        results: list[Result] = []
        for running in tuple(self._active):
            self._observe_python_failure(running)
            self._observe_test_failure(running)
            if self._interrupted or self._timed_out:
                break
            code = running.process.poll()
            if code is not None:
                if code != 0:
                    self._halt_on_error(Result(running.stage.name, STATUS_FAILED, f"exit={code}", running.log))
                    break
                deadline = self._clock() + min(self._reserve, max(0.0, self.limit - self.budgeted))
                results.append(self._complete(running, stopped=False, deadline=deadline))
                self._active.remove(running)
                if self._interrupted:
                    break
        return results

    def _stop_all(self) -> list[Result]:
        self._charge()
        if self._persistent is not None and self._persistent not in self._active:
            self._active.append(self._persistent)
        self._persistent = None
        deadline = self._clock() + min(self._reserve, max(0.0, self.limit - self.budgeted))
        results: list[Result] = []
        for running in tuple(self._active):
            results.append(self._complete(running, stopped=True, deadline=deadline))
            self._active.remove(running)
        self._charge()
        return results

    def _wait_for_command(self, results: list[Result]) -> None:
        while self._active and not self._interrupted:
            self._sample()
            results.extend(self._collect_completed())
            if self._interrupted or self._expired():
                break
            if self._active:
                time.sleep(min(_POLL_SECONDS, max(0.0, self.limit - self._reserve - self.budgeted)))

    def supervise(self, stage: Stage) -> list[Result]:
        """监督一个 Bazel 命令或隔离自检命令，不提供第二套生产调度器."""
        if self._finished is not None:
            message = "最终汇总后不能继续执行阶段"
            raise RuntimeError(message)
        results: list[Result] = []
        launched = False
        try:
            if self._valid and not self._interrupted and not self._timed_out:
                self._charge()
                if not self._expired():
                    launched = True
                    result = self._launch(stage)
                    if result is not None:
                        results.append(result)
                        self._halt_on_error(result)
                    self._wait_for_command(results)
        except KeyboardInterrupt:
            self._interrupted = True
            results.append(Result("gate-interrupt", STATUS_FAILED, "测试门被中断"))
        except Exception as error:  # noqa: BLE001 - 未预料的监督错误也必须终止整个门。
            result = Result("gate-runtime", STATUS_FAILED, f"测试门监督失败：{type(error).__name__}: {error}")
            results.append(result)
            self._halt_on_error(result)
        finally:
            results.extend(self._stop_all())
            if not launched:
                status = STATUS_TIMED_OUT if self._timed_out else STATUS_UNVERIFIED
                results.append(Result(stage.name, status, "前置失败或共享预算耗尽，命令未执行"))
            self._results.extend(results)
        if self._interrupted or self._timed_out:
            raise GateStoppedError(results)
        return results

    def _validate_persistent_start(self) -> None:
        self._charge()
        if self._finished is not None or self._interrupted or self._expired() or not self._valid:
            message = "持久命令启动前门已停止或预算耗尽"
            raise RuntimeError(message)

    def _persistent_completion_code(self, running: _Running, completion: Path) -> int | None:
        if running.process.poll() is not None:
            message = f"持久派发器意外退出：{running.process.returncode}"
            raise RuntimeError(message)
        if not completion.exists():
            return None
        self._retain_process_identities()
        code = _parse_json(read_published_text(completion))
        if type(code) is not int:
            message = "持久派发完成通知不是整数退出码"
            raise ValueError(message)
        return code

    def _wait_for_persistent_command(self, stage: Stage, completion: Path, results: list[Result]) -> None:
        while self._active and not self._interrupted:
            self._sample()
            running = self._active[0]
            self._observe_python_failure(running)
            if self._interrupted or self._expired():
                break
            code = self._persistent_completion_code(running, completion)
            if code is not None:
                if code != 0:
                    self._halt_on_error(Result(stage.name, STATUS_FAILED, f"exit={code}", running.log))
                    break
                results.append(Result(stage.name, STATUS_OK, "真实 Bazel 客户端 exit=0；保留所属服务 Job", running.log))
                self._active.remove(running)
                break
            time.sleep(min(_POLL_SECONDS, max(0.0, self.limit - self._reserve - self.budgeted)))

    def supervise_persistent(self, stage: Stage, completion: Path) -> list[Result]:
        """监督连续真实命令；观察桥与跨 CLI 原生服务分别持有受控 Job."""
        results: list[Result] = []
        try:
            self._validate_persistent_start()
            if self._persistent is None:
                failure = self._launch(stage)
                if failure is not None:
                    self._halt_on_error(failure)
                    results.append(failure)
                else:
                    self._persistent = self._active[-1]
            else:
                self._persistent.stage = stage
                self._active.append(self._persistent)
            self._wait_for_persistent_command(stage, completion, results)
        except KeyboardInterrupt:
            self._interrupted = True
            results.append(Result(stage.name, STATUS_FAILED, "持久派发被中断"))
        except Exception as error:  # noqa: BLE001 - 持久服务异常也必须终止整个所属 Job。
            failure = Result(stage.name, STATUS_FAILED, f"持久派发监督失败：{type(error).__name__}: {error}")
            results.append(failure)
            self._halt_on_error(failure)
        finally:
            self._charge()
            if self._interrupted or self._timed_out:
                results.extend(self._stop_all())
            self._results.extend(results)
        if self._interrupted or self._timed_out:
            raise GateStoppedError(results)
        return results

    def close_persistent(self) -> None:
        """回收本轮观察桥 Job；原生服务 lease 由外层独立监督器释放或终止."""
        if self._persistent is None:
            return
        running = self._persistent
        self._persistent = None
        self._charge()
        deadline = self._clock() + min(self._reserve, max(0.0, self.limit - self.budgeted))
        confirmed, detail = self._close(running, deadline)
        if not confirmed:
            failure = Result("bazel-service-cleanup", STATUS_FAILED, detail, running.log)
            self.record_results([failure])
            raise GateStoppedError([failure])

    def validate_result(self, result: Result) -> Result:
        """对 Bazel 原生测试结果应用原门判据，不把聚合命令成功当作测试成功."""
        return self._validate_result(result) if self._validate_result is not None else result

    def record_results(self, results: list[Result]) -> None:
        """保留原生阶段结果，并向独立硬预算监督器发布首个失败."""
        self._results.extend(results)
        for result in results:
            self._halt_on_error(result)

    def _status(self, results: list[Result]) -> str:
        if self._timed_out or self.budgeted >= self.limit:
            return STATUS_TIMED_OUT
        if self._interrupted or any(result.status == STATUS_FAILED for result in results):
            return STATUS_FAILED
        if any(result.status == STATUS_TIMED_OUT for result in results):
            return STATUS_TIMED_OUT
        executed = [result for result in results if result.status != "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"]
        if not executed or any(result.status != STATUS_OK for result in executed):
            return STATUS_UNVERIFIED
        return STATUS_OK

    @staticmethod
    def _print_result(result: Result) -> None:
        elapsed = f"{result.elapsed:.6f}s" if result.log is not None and result.returncode is not None else "unknown"
        print(f"{result.status} {result.name}: {result.detail} stage_elapsed={elapsed}")
        if result.log is not None:
            print(f"  log={result.log}")
            if result.status != STATUS_OK:
                tail = _tail(result.log)
                if tail:
                    print(tail)

    def _notify_external_failure(self, results: list[Result]) -> None:
        if self._progress is None or self._interrupted:
            return
        for result in results:
            if result.status not in {STATUS_OK, "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"}:
                # finish 已停止调度；保留原状态分类，先通知监督器再写可能阻塞的日志。
                self._progress(self.compile_excluded, result.status)
                print(f"STOP {result.name}: {result.detail}; log={result.log}", flush=True)
                break

    def finish(
        self, results: list[Result], *, snapshot: list[str] | None = None, notes: list[str] | None = None
    ) -> int:
        """所有结果均报告真实汇总状态及完整运行的五项指标."""
        if self._finished is not None:
            return self._finished
        self._notify_external_failure(results)
        results = [*results, *self._stop_all()]
        if self._temporary is not None:
            print(f"运行期 tempfile：{self._temporary.name}（仓库外，本门独占并回收）")
            try:
                self._cleanup_temporary()
            except OSError as error:
                results.append(Result("temporary-cleanup", STATUS_FAILED, f"运行期临时目录回收失败：{error}"))
            except KeyboardInterrupt:
                self._interrupted = True
                results.append(Result("temporary-cleanup", STATUS_FAILED, "运行期临时目录回收被中断；不报告为通过。"))
            self._temporary = None
        if not self._valid:
            results.extend(self._results[:1])
        for line in snapshot or []:
            print(line)
        for result in results:
            self._print_result(result)
        for note in notes or []:
            print(f"NOTE {note}")
        self._charge()
        internal = [result for result in self._results if result.status != STATUS_OK]
        status = self._status([*results, *internal])
        print(
            f"{self.level} {status} total={self.total:.1f}s",
            f"compile_excluded={self.compile_excluded:.1f}s budgeted={self.budgeted:.1f}s limit={self.limit:.1f}s",
        )
        self._charge()
        if self.budgeted >= self.limit and status != STATUS_TIMED_OUT:
            status = STATUS_TIMED_OUT
            print(
                f"{self.level} {status} total={self.total:.1f}s",
                f"compile_excluded={self.compile_excluded:.1f}s budgeted={self.budgeted:.1f}s limit={self.limit:.1f}s",
            )
        self._finished = 0 if status == STATUS_OK else 1
        return self._finished
