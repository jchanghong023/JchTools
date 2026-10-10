"""本地测试门共享调度器：保守核算真正编译之外的墙钟预算."""

from __future__ import annotations

import dataclasses
import json
import math
import os
import re
import subprocess
import tempfile
import time
from collections import deque
from typing import TYPE_CHECKING, NotRequired, Protocol, TypedDict, final

if TYPE_CHECKING:
    from collections.abc import Callable
    from pathlib import Path
    from typing import IO, TypeIs, Unpack

STATUS_OK = "PASS"
STATUS_FAILED = "FAIL"
STATUS_TIMED_OUT = "TIMEOUT"
STATUS_UNVERIFIED = "UNVERIFIED"
_CEILINGS = {"fastcheck": 60.0, "fulltest": 900.0, "slowtest": 1500.0}
_POLL_SECONDS = 0.02
_TAIL_LINES = 8
_PARALLEL_COMMANDS = 4
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


@dataclasses.dataclass
class Stage:
    name: str
    argv: list[str]
    kind: str = "check"
    env: dict[str, str] | None = None
    phase_file: Path | None = None
    exclusive_resource: str | None = None


@dataclasses.dataclass
class Result:
    name: str
    status: str
    detail: str
    log: Path | None = None
    elapsed: float = 0.0
    returncode: int | None = None


@dataclasses.dataclass
class _Running:
    stage: Stage
    process: subprocess.Popen[bytes]
    sink: IO[bytes]
    log: Path
    started: float


def _is_mapping(value: object) -> TypeIs[dict[str, object]]:
    return isinstance(value, dict)


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
        self.log_dir = log_dir
        self.compile_excluded = 0.0
        self.total = 0.0
        self._start_command = callbacks["start_command"]
        self._close_command = callbacks["close_command"]
        self._kill_tree = callbacks["kill_tree"]
        self._compiler_ids = callbacks["compiler_ids"]
        self._observe_tree = callbacks.get("observe_tree")
        self._active: list[_Running] = []
        self._observed: dict[int, frozenset[int]] = {}
        self._results: list[Result] = []
        self._timed_out = False
        self._interrupted = False
        self._finished: int | None = None
        self._temporary: tempfile.TemporaryDirectory[str] | None = None
        self._sequence = 0
        self._reserve = min(2.0, max(0.0, limit * 0.1))
        ceiling = _CEILINGS.get(level)
        self._valid = ceiling is not None and math.isfinite(limit) and 0.0 < limit <= ceiling
        if not self._valid:
            self._results.append(Result("gate-limit", STATUS_FAILED, "层级未知或预算无效；预算只能调低"))

    def prepare_temporary(self) -> None:
        """建立本门独占的仓库外 tempfile，避免触发 Git 整树保护."""
        if self._temporary is not None:
            message = "运行期临时目录已经建立。"
            raise RuntimeError(message)
        self._temporary = tempfile.TemporaryDirectory(prefix="jchtools-gate-", dir=self.root.parent)

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
            sink = log.open("wb")
        except OSError as error:
            return Result(stage.name, STATUS_FAILED, f"无法创建阶段日志：{error}")
        self._charge()
        if self._expired():
            sink.close()
            return Result(stage.name, STATUS_TIMED_OUT, "实际启动前共享非编译预算已耗尽", log)
        try:
            proc = self._start(stage, sink)
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
            reason = "共享非编译预算耗尽" if self._timed_out else "测试门被中断或终止"
            detail = f"{reason}; {detail}"
        print(f"DONE {running.stage.name} status={status} {detail}", flush=True)
        return Result(running.stage.name, status, detail, running.log, self._clock() - running.started, code)

    def _collect_completed(self) -> list[Result]:
        results: list[Result] = []
        for running in tuple(self._active):
            if running.process.poll() is not None:
                deadline = self._clock() + min(self._reserve, max(0.0, self.limit - self.budgeted))
                results.append(self._complete(running, stopped=False, deadline=deadline))
                self._active.remove(running)
        return results

    def _stop_all(self) -> list[Result]:
        self._charge()
        deadline = self._clock() + min(self._reserve, max(0.0, self.limit - self.budgeted))
        results: list[Result] = []
        for running in tuple(self._active):
            results.append(self._complete(running, stopped=True, deadline=deadline))
            self._active.remove(running)
        self._charge()
        return results

    def _next_stage(self, pending: deque[Stage]) -> Stage | None:
        held = {running.stage.exclusive_resource for running in self._active}
        for stage in pending:
            if stage.exclusive_resource is None or stage.exclusive_resource not in held:
                pending.remove(stage)
                return stage
        return None

    def _schedule(self, pending: deque[Stage], results: list[Result], *, parallel: bool) -> None:
        while pending or self._active:
            self._sample()
            if self._interrupted or self._expired():
                break
            while pending and (not self._active or (parallel and len(self._active) < _PARALLEL_COMMANDS)):
                self._charge()
                if self._expired():
                    break
                stage = self._next_stage(pending)
                if stage is None:
                    break
                result = self._launch(stage)
                self._charge()
                if result is not None:
                    results.append(result)
                if self._interrupted or self._expired():
                    break
            results.extend(self._collect_completed())
            if self._interrupted or self._expired():
                break
            if self._active:
                time.sleep(min(_POLL_SECONDS, max(0.0, self.limit - self._reserve - self.budgeted)))

    def _unstarted(self, pending: deque[Stage]) -> list[Result]:
        status = STATUS_TIMED_OUT if self._timed_out else STATUS_UNVERIFIED
        reason = "命令启动前共享预算已耗尽" if self._timed_out else "命令未执行"
        return [Result(stage.name, status, reason) for stage in pending]

    def run(self, stages: list[Stage], *, parallel: bool = True) -> list[Result]:
        """并行执行独立命令，不重置测试门的共享预算时钟."""
        if self._finished is not None:
            message = "最终汇总后不能继续执行阶段"
            raise RuntimeError(message)
        pending = deque(stages)
        results: list[Result] = []
        try:
            if self._valid and not self._interrupted and not self._timed_out:
                self._schedule(pending, results, parallel=parallel)
        except KeyboardInterrupt:
            self._interrupted = True
            results.append(Result("gate-interrupt", STATUS_FAILED, "测试门被中断"))
        except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
            self._interrupted = True
            results.append(Result("gate-runtime", STATUS_FAILED, f"测试门编排失败：{error}"))
        finally:
            results.extend(self._stop_all())
            results.extend(self._unstarted(pending))
            self._results.extend(results)
        return results

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
        print(f"{result.status} {result.name}: {result.detail}")
        if result.log is not None:
            print(f"  log={result.log}")
            if result.status != STATUS_OK:
                tail = _tail(result.log)
                if tail:
                    print(tail)

    def finish(
        self, results: list[Result], *, snapshot: list[str] | None = None, notes: list[str] | None = None
    ) -> int:
        """所有结果均报告真实汇总状态及完整运行的五项指标."""
        if self._finished is not None:
            return self._finished
        results = [*results, *self._stop_all()]
        if self._temporary is not None:
            print(f"运行期 tempfile：{self._temporary.name}（仓库外，同盘，本门回收）")
            try:
                self._temporary.cleanup()
            except OSError as error:
                results.append(Result("temporary-cleanup", STATUS_FAILED, f"运行期临时目录回收失败：{error}"))
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
