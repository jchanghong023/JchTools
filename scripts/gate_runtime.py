"""本地测试门的顺序命令执行和日志记录."""

from __future__ import annotations

import dataclasses
import os
import subprocess
import tempfile
import time
import uuid
from pathlib import Path
from typing import final

from scripts.gate_summary import binding_loop_problem

STATUS_OK = "PASS"
STATUS_FAILED = "FAIL"
STATUS_UNVERIFIED = "UNVERIFIED"


@dataclasses.dataclass
class Stage:
    name: str
    argv: list[str]
    kind: str = "check"
    env: dict[str, str] | None = None


@dataclasses.dataclass
class Result:
    name: str
    status: str
    detail: str
    log: Path | None = None
    elapsed: float = 0.0
    returncode: int | None = None


@final
class GateRun:
    """顺序执行阶段，保留每次执行的独立日志与结果."""

    def __init__(self, level: str, root: Path, log_dir: Path) -> None:
        self.level = level
        self.root = root
        self.log_dir = log_dir / f"pid-{os.getpid()}-{uuid.uuid4().hex}"
        self.results: list[Result] = []
        self._started = time.monotonic()
        self._temporary: tempfile.TemporaryDirectory[str] | None = None
        self._sequence = 0
        print(f"日志目录：{self.log_dir}", flush=True)

    def prepare_temporary(self) -> None:
        """建立仓库外独占临时目录，避免触发 Git 整树保护."""
        if self._temporary is not None:
            message = "运行期临时目录已经建立。"
            raise RuntimeError(message)
        parent = Path(tempfile.gettempdir()).resolve()
        if parent.is_relative_to(self.root.resolve()):
            parent = self.root.resolve().parent
        self._temporary = tempfile.TemporaryDirectory(prefix="jchtools-gate-", dir=parent)

    def run(self, stages: list[Stage]) -> list[Result]:
        results: list[Result] = []
        for stage in stages:
            self._sequence += 1
            log = self.log_dir / f"{self._sequence:03d}-{stage.name}.log"
            started = time.monotonic()
            try:
                self.log_dir.mkdir(parents=True, exist_ok=True)
                env = os.environ | (stage.env or {})
                if self._temporary is not None:
                    env.update(TEMP=self._temporary.name, TMP=self._temporary.name)
                with log.open("xb") as sink:
                    _ = subprocess.run(
                        stage.argv, cwd=self.root, stdout=sink, stderr=subprocess.STDOUT, env=env, check=True
                    )
            except subprocess.CalledProcessError as error:
                result = Result(
                    stage.name,
                    STATUS_FAILED,
                    f"exit={error.returncode}",
                    log,
                    time.monotonic() - started,
                    error.returncode,
                )
                self.results.append(result)
                raise
            except OSError as error:
                result = Result(
                    stage.name, STATUS_FAILED, str(error), log if log.exists() else None, time.monotonic() - started
                )
                self.results.append(result)
                raise
            result = Result(stage.name, STATUS_OK, "exit=0", log, time.monotonic() - started, 0)
            if problem := binding_loop_problem(log):
                result.status = STATUS_FAILED
                result.detail = problem
            self.results.append(result)
            results.append(result)
            if result.status != STATUS_OK:
                raise subprocess.CalledProcessError(1, stage.argv)
        return results

    def finish(self, *, snapshot: list[str] | None = None, notes: list[str] | None = None) -> int:
        if self._temporary is not None:
            try:
                name = self._temporary.name
                if os.name == "nt" and not name.startswith("\\\\?\\"):
                    self._temporary.name = "\\\\?\\UNC\\" + name[2:] if name.startswith("\\\\") else "\\\\?\\" + name
                self._temporary.cleanup()
            except (OSError, KeyboardInterrupt) as error:
                self.results.append(Result("temporary-cleanup", STATUS_FAILED, str(error)))
            self._temporary = None
        for line in snapshot or []:
            print(line)
        for result in self.results:
            print(f"{result.status} {result.name}: {result.detail} stage_elapsed={result.elapsed:.6f}s")
            if result.log is not None:
                print(f"  log={result.log}")
        for note in notes or []:
            print(f"NOTE {note}")
        executed = [result for result in self.results if result.status != "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"]
        success = bool(executed) and all(result.status == STATUS_OK for result in executed)
        print(f"{self.level} {'PASS' if success else 'FAIL'} total={time.monotonic() - self._started:.1f}s")
        return 0 if success else 1
