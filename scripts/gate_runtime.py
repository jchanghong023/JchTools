"""本地测试门的顺序命令执行和日志记录."""

from __future__ import annotations

import dataclasses
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path
from typing import cast, final

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
    aggregate: dict[str, object] | None = None


_COUNT = r"(?:\d{1,3}(?:,\d{3})+|\d+)"
_NEXTEST_SUMMARY = re.compile(
    "".join(
        (
            rf"^\s*Summary\s+\[\s*[\d.]+s\]\s+(?P<run>{_COUNT})(?:/(?P<selected>{_COUNT}))?\s+tests? run:\s+",
            rf"(?P<passed>{_COUNT}) passed(?: \((?:{_COUNT} (?:slow|flaky|leaky))",
            rf"(?:, {_COUNT} (?:slow|flaky|leaky))*\))?",
            rf"(?:, (?P<failed>{_COUNT}) failed(?: \({_COUNT} due to being leaky\))?)?",
            rf"(?:, (?P<exec_failed>{_COUNT}) exec failed)?",
            rf"(?:, (?P<timed_out>{_COUNT}) timed out)?, (?P<skipped>{_COUNT}) skipped\s*$",
        )
    )
)
_DOCTEST_SUFFIX = r"(\d+) ignored; (\d+) measured; (\d+) filtered out; finished in [\d.]+s\s*$"
_DOCTEST_SUMMARY = re.compile(r"^\s*test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; " + _DOCTEST_SUFFIX)


def _nextest_counts(match: re.Match[str]) -> dict[str, int]:
    # nextest 省略值为零的失败类别；括号中的状态是已计入通过数的子集。
    def number(key: str) -> int:
        return int((match.group(key) or "0").replace(",", ""))

    run = number("run")
    selected = match.group("selected")
    return {
        "run": run,
        "passed": number("passed"),
        "failed": number("failed") + number("exec_failed") + number("timed_out"),
        "skipped": number("skipped"),
        "unfinished": int(selected.replace(",", "")) - run if selected is not None else 0,
    }


def _native_aggregate(stage: Stage, log: Path | None) -> dict[str, object] | None:
    """只接受受限的原生聚合行，不复制测试名称或发现数."""
    is_nextest = "nextest" in stage.argv and "run" in stage.argv
    is_doctest = "--doc" in stage.argv and "test" in stage.argv
    if not (is_nextest or is_doctest):
        return None
    kind = "nextest" if is_nextest else "doctest"
    summaries: list[str] = []
    counts = {"run": 0, "passed": 0, "failed": 0, "skipped": 0, "unfinished": 0}
    filtered = 0
    measured = 0
    if log is None:
        return {"kind": kind, "available": False}
    try:
        with log.open(encoding="utf-8", errors="replace") as source:
            for line in source:
                match = (_NEXTEST_SUMMARY if is_nextest else _DOCTEST_SUMMARY).fullmatch(line.rstrip())
                if match is None:
                    continue
                summaries.append(line.strip())
                if is_nextest:
                    for key, value in _nextest_counts(match).items():
                        counts[key] += value
                else:
                    passed, failed, ignored, benches, excluded = map(int, match.groups())
                    counts["run"] += passed + failed
                    counts["passed"] += passed
                    counts["failed"] += failed
                    counts["skipped"] += ignored
                    measured += benches
                    filtered += excluded
    except OSError:
        return {"kind": kind, "available": False}
    if not summaries:
        return {"kind": kind, "available": False}
    result: dict[str, object] = {"kind": kind, "available": True, **counts, "native_summaries": summaries}
    if is_doctest:
        # 多个 doctest 进程中断时，无法从已完成的汇总推知未完成数。
        result.update(unfinished=None, filtered_out=filtered, measured=measured)
    return result


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
            result.aggregate = _native_aggregate(stage, result.log)
            if result.aggregate is not None and result.status != STATUS_OK:
                result.aggregate["stage_incomplete"] = True
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

    @staticmethod
    def _add_nextest_total(total: dict[str, object] | None, result: Result) -> dict[str, object]:
        if total is None:
            total = {
                "available": True,
                "run": 0,
                "passed": 0,
                "failed": 0,
                "skipped": 0,
                "unfinished": 0,
                "elapsed_seconds": 0.0,
            }
        total["elapsed_seconds"] = cast("float", total["elapsed_seconds"]) + result.elapsed
        aggregate = result.aggregate
        if aggregate is not None and aggregate["available"]:
            for key in ("run", "passed", "failed", "skipped", "unfinished"):
                total[key] = cast("int", total[key]) + cast("int", aggregate[key])
        else:
            total["available"] = False
        return total

    def _record_timings(self) -> None:
        # 阶段耗时是汇总数据；媒体阶段名含用例名，改为序号避免泄露成功用例身份。
        timings: list[dict[str, object]] = []
        nextest_total: dict[str, object] | None = None
        media_index = 0
        for result in self.results:
            name = result.name
            if name.startswith("real-media-e2e-"):
                media_index += 1
                name = f"real-media-e2e-{media_index}"
            timing: dict[str, object] = {"stage": name, "status": result.status, "elapsed_seconds": result.elapsed}
            if result.aggregate is not None:
                timing["aggregate"] = result.aggregate
                if result.aggregate["kind"] == "nextest":
                    nextest_total = self._add_nextest_total(nextest_total, result)
            timings.append(timing)
        try:
            self.log_dir.mkdir(parents=True, exist_ok=True)
            with (self.log_dir / "stage-timings.json").open("x", encoding="utf-8") as output:
                json.dump(
                    {
                        "total_seconds": time.monotonic() - self._started,
                        "stages": timings,
                        "nextest_total": nextest_total,
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
            for timing in timings:
                print(f"{timing['stage']}: {timing['status']} {timing['elapsed_seconds']:.1f}s")
                if "aggregate" in timing:
                    print(f"  aggregate={json.dumps(timing['aggregate'], ensure_ascii=False)}")
            if nextest_total is not None:
                print(f"nextest total: {json.dumps(nextest_total, ensure_ascii=False)}")

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
