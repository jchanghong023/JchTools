"""本地测试门的顺序命令执行和日志记录."""

from __future__ import annotations

import dataclasses
import json
import os
import shutil
import subprocess
import sys
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
    """顺序执行阶段，临时捕获输出并保留失败日志."""

    def __init__(self, level: str, root: Path, log_dir: Path) -> None:
        self.level = level
        self.root = root
        self.log_dir = log_dir / f"pid-{os.getpid()}-{uuid.uuid4().hex}"
        self.results: list[Result] = []
        self._started = time.monotonic()
        self._temporary: tempfile.TemporaryDirectory[str] | None = None
        self._sequence = 0

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
            except OSError as error:
                result = Result(
                    stage.name, STATUS_FAILED, str(error), log if log.exists() else None, time.monotonic() - started
                )
            else:
                result = Result(stage.name, STATUS_OK, "exit=0", log, time.monotonic() - started, 0)
                if problem := binding_loop_problem(log):
                    result.status = STATUS_FAILED
                    result.detail = problem
            self.results.append(result)
            results.append(result)
        return results

    def _cleanup_temporary(self) -> None:
        if self._temporary is None:
            return
        try:
            name = self._temporary.name
            if os.name == "nt" and not name.startswith("\\\\?\\"):
                self._temporary.name = "\\\\?\\UNC\\" + name[2:] if name.startswith("\\\\") else "\\\\?\\" + name
            self._temporary.cleanup()
        except (OSError, KeyboardInterrupt) as error:
            self.results.append(Result("temporary-cleanup", STATUS_FAILED, str(error)))
        self._temporary = None

    def _cleanup_success_logs(self) -> set[Path]:
        private_logs: set[Path] = set()
        for result in self.results:
            if result.status == STATUS_OK and result.log is not None:
                try:
                    result.log.unlink()
                except OSError as error:
                    result.status = STATUS_FAILED
                    private_logs.add(result.log)
                    result.detail = f"成功阶段日志清理失败: {error}"
                else:
                    result.log = None
        return private_logs

    @staticmethod
    def _show_failure(result: Result, private_logs: set[Path]) -> None:
        print(f"{result.status} {result.name}: {result.detail} stage_elapsed={result.elapsed:.6f}s")
        if result.log is None:
            return
        print(f"  log={result.log}")
        if result.log in private_logs:
            return
        try:
            with result.log.open(encoding="utf-8", errors="replace") as source:
                shutil.copyfileobj(source, sys.stdout)
        except OSError as error:
            print(f"  无法读取失败日志: {error}")

    def _record_timings(self) -> None:
        # 阶段耗时是汇总数据；媒体阶段名含用例名，改为序号避免泄露成功用例身份。
        timings: list[tuple[str, str, float]] = []
        media_index = 0
        for result in self.results:
            name = result.name
            if name.startswith("real-media-e2e-"):
                media_index += 1
                name = f"real-media-e2e-{media_index}"
            timings.append((name, result.status, result.elapsed))
        try:
            self.log_dir.mkdir(parents=True, exist_ok=True)
            with (self.log_dir / "stage-timings.json").open("x", encoding="utf-8") as output:
                json.dump(
                    {
                        "total_seconds": time.monotonic() - self._started,
                        "stages": [
                            {"stage": name, "status": status, "elapsed_seconds": elapsed}
                            for name, status, elapsed in timings
                        ],
                    },
                    output,
                    ensure_ascii=False,
                    indent=2,
                )
                _ = output.write("\n")
        except OSError as error:
            self.results.append(Result("stage-timings", STATUS_FAILED, f"无法记录阶段耗时：{error}"))
        else:
            print(f"阶段耗时：{self.log_dir / 'stage-timings.json'}")
            for name, status, elapsed in timings:
                print(f"{name}: {status} {elapsed:.1f}s")

    def finish(self, *, snapshot: list[str] | None = None, notes: list[str] | None = None) -> int:
        self._cleanup_temporary()
        private_logs = self._cleanup_success_logs()
        self._record_timings()
        executed = [result for result in self.results if result.status != "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"]
        excluded = len(self.results) - len(executed)
        if excluded:
            print(f"NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED count={excluded}")
        success = bool(executed) and all(result.status == STATUS_OK for result in executed)
        if not success:
            for line in snapshot or []:
                print(line)
            for result in executed:
                if result.status != STATUS_OK:
                    self._show_failure(result, private_logs)
            for note in notes or []:
                print(f"NOTE {note}")
        print(f"{self.level} {'PASS' if success else 'FAIL'} total={time.monotonic() - self._started:.1f}s")
        return 0 if success else 1
