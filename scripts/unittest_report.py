"""沿用 unittest 命令行和文本诊断，自动保存逻辑用例耗时报告.

用法：python -m scripts.unittest_report <原 unittest 参数>，也支持直接运行脚本。
只扩展公开的 TextTestResult 回调和 TextTestRunner.run；发现、选择器、缓冲、
failfast、catchbreak 和退出码仍交给 TestProgram。帮助和参数错误不创建报告。
用例耗时是 startTest 到 stopTest 的单调墙钟，包含 setup、测试、teardown 和清理；
子测试的失败名称与诊断归入父用例，不增加用例数。expectedFailure 映射为 skipped，
unexpectedSuccess 映射为 failure，原生控制台诊断不变。
没有 startTest 的类/模块夹具事件写入 run.json 的 coverage.untimed_events，
中断用例写入 coverage.incomplete_cases，均不伪造零耗时或成功的 XML 用例。
报告错误保留原始摘要，同时输出完整 traceback 和底层报告操作路径.
"""

from __future__ import annotations

import sys
import time
import traceback
import unittest
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, ClassVar, override

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scripts.test_reports import ROOT, Case, TimingReport

if TYPE_CHECKING:
    from types import TracebackType

    _ExcInfo = tuple[type[BaseException], BaseException, TracebackType] | tuple[None, None, None]


@dataclass
class _ActiveTest:
    test: unittest.TestCase
    started: float
    status: str | None = None
    details: list[str] = field(default_factory=list)


class _ReportSession:
    def __init__(self, arguments: list[str], *, command: list[str] | None = None) -> None:
        invocation = ["-m", "scripts.unittest_report"] if __package__ else [sys.argv[0]]
        self.command: list[str] = command if command is not None else [sys.executable, *invocation, *arguments]
        self.arguments: list[str] = arguments
        self.report: TimingReport | None = None
        self.result: _ReportingResult | None = None
        self.reporting_errors: list[str] = []
        self.untimed_events: list[dict[str, object]] = []
        self.incomplete_cases: list[dict[str, object]] = []
        self.completed_cases: int = 0

    def report_error(self, error: Exception) -> None:
        detail = f"{type(error).__name__}: {error}"
        self.reporting_errors.append(detail)
        print(f"unittest 报告写入失败：{detail}", file=sys.stderr, flush=True)
        traceback.print_exception(type(error), error, error.__traceback__, file=sys.stderr)
        _ = sys.stderr.flush()

    def start(self) -> None:
        try:
            self.report = TimingReport(
                "unittest",
                self.command,
                scope={"arguments": self.arguments},
                timing_basis="monotonic startTest-stopTest; includes setup, call, teardown and cleanup",
                concurrency={"test_workers": 1},
            )
        except Exception as error:  # noqa: BLE001 - 报告失败不能覆盖原始测试结果。
            self.report_error(error)

    def add_case(self, case: Case) -> None:
        self.completed_cases += 1
        if self.report is not None:
            try:
                self.report.add_case(case)
            except Exception as error:  # noqa: BLE001 - 继续原生测试执行与诊断。
                self.report_error(error)

    def finish(self, exit_code: int, completion: str) -> int:
        result = self.result
        coverage: dict[str, object] = {
            "completed_logical_cases": self.completed_cases,
            "untimed_events": self.untimed_events,
            "incomplete_cases": self.incomplete_cases,
            "reporting_errors": self.reporting_errors,
            "subtests": "aggregated into their logical parent case",
            "fixture_timing": "unsupported: no startTest/stopTest interval",
        }
        if result is not None:
            coverage["native_unittest"] = {
                "tests_run": result.testsRun,
                "failures": len(result.failures),
                "errors": len(result.errors),
                "skipped": len(result.skipped),
                "expected_failures": len(result.expectedFailures),
                "unexpected_successes": len(result.unexpectedSuccesses),
                "stopped_early": result.shouldStop,
            }
        if self.reporting_errors and exit_code == 0:
            exit_code = 1
        if self.report is not None:
            try:
                exit_code = self.report.finish(exit_code, completion=completion, coverage=coverage)
            except Exception as error:  # noqa: BLE001 - 原始非零退出码优先。
                self.report_error(error)
        if self.reporting_errors and exit_code == 0:
            return 1
        return exit_code


class _ReportingResult(unittest.TextTestResult):
    session: ClassVar[_ReportSession]
    active: _ActiveTest | None = None

    @override
    def startTestRun(self) -> None:
        super().startTestRun()
        self.session.result = self

    @override
    def startTest(self, test: unittest.TestCase) -> None:
        self.active = _ActiveTest(test, time.perf_counter())
        super().startTest(test)

    def _record(self, test: unittest.TestCase, status: str, detail: str = "") -> None:
        active = self.active
        if active is None:
            self.session.untimed_events.append(
                {
                    "id": test.id(),
                    "status": status,
                    "detail": detail,
                    "duration": None,
                    "timing": "unsupported: fixture event outside startTest/stopTest",
                }
            )
            return
        # 子测试只补充当前逻辑测试的状态和名称；错误优先于失败、跳过、成功。
        priorities = {"passed": 0, "skipped": 1, "failure": 2, "error": 3}
        if active.status is None or priorities[status] > priorities[active.status]:
            active.status = status
        if detail:
            active.details.append(f"{test.id()}\n{detail}")

    @override
    def addSuccess(self, test: unittest.TestCase) -> None:
        super().addSuccess(test)
        self._record(test, "passed")

    @override
    def addFailure(self, test: unittest.TestCase, err: _ExcInfo) -> None:
        super().addFailure(test, err)
        self._record(test, "failure", self.failures[-1][1])

    @override
    def addError(self, test: unittest.TestCase, err: _ExcInfo) -> None:
        super().addError(test, err)
        self._record(test, "error", self.errors[-1][1])

    @override
    def addSkip(self, test: unittest.TestCase, reason: str) -> None:
        super().addSkip(test, reason)
        # 跳过一个子测试不是跳过已执行的父用例；父用例的最终回调仍决定结果。
        status = "passed" if self.active is not None and test is not self.active.test else "skipped"
        self._record(test, status, reason)

    @override
    def addExpectedFailure(self, test: unittest.TestCase, err: _ExcInfo) -> None:
        super().addExpectedFailure(test, err)
        self._record(test, "skipped", "expectedFailure\n" + self.expectedFailures[-1][1])

    @override
    def addUnexpectedSuccess(self, test: unittest.TestCase) -> None:
        super().addUnexpectedSuccess(test)
        self._record(test, "failure", "unexpectedSuccess: 预期失败的测试意外成功")

    @override
    def addSubTest(
        self,
        test: unittest.TestCase,
        subtest: unittest.TestCase,
        err: _ExcInfo | None,
    ) -> None:
        super().addSubTest(test, subtest, err)
        if err is not None:
            if err[0] is not None and issubclass(err[0], test.failureException):
                self._record(subtest, "failure", self.failures[-1][1])
            else:
                self._record(subtest, "error", self.errors[-1][1])

    @override
    def stopTest(self, test: unittest.TestCase) -> None:
        stopped = time.perf_counter()
        active = self.active
        self.active = None
        super().stopTest(test)
        if active is None:
            return
        duration = stopped - active.started
        if active.status is None or isinstance(sys.exc_info()[1], KeyboardInterrupt):
            self.session.incomplete_cases.append(
                {
                    "id": test.id(),
                    "status": "incomplete",
                    "observed_duration": duration,
                    "detail": "\n\n".join(active.details),
                    "timing": "partial startTest-stopTest interval; no completed outcome",
                }
            )
            return
        module = sys.modules.get(type(test).__module__)
        module_file = getattr(module, "__file__", None)
        filename: str | None = None
        if isinstance(module_file, str):
            path = Path(module_file).resolve()
            if path.is_relative_to(ROOT):
                filename = path.relative_to(ROOT).as_posix()
        self.session.add_case(
            Case(
                name=test.id(),
                classname=f"{type(test).__module__}.{type(test).__qualname__}",
                duration=duration,
                status=active.status,
                detail="\n\n".join(active.details),
                file=filename,
            )
        )


def main(
    arguments: list[str] | None = None,
    *,
    test_program: type[unittest.TestProgram] = unittest.TestProgram,
    command: list[str] | None = None,
) -> int:
    args = list(sys.argv[1:] if arguments is None else arguments)
    report_session = _ReportSession(args, command=command)

    class ReportingResult(_ReportingResult):
        session: ClassVar[_ReportSession] = report_session

    class ReportingRunner(unittest.TextTestRunner):
        @override
        def _makeResult(self) -> _ReportingResult:
            return ReportingResult(self.stream, self.descriptions, self.verbosity, durations=self.durations)

        @override
        def run(self, test: unittest.TestSuite | unittest.TestCase) -> unittest.TextTestResult:
            report_session.start()
            return super().run(test)

    try:
        # exit=True 保留当前 Python 的原生退出码，包括未发现测试的特殊退出码。
        _ = test_program(
            module=None,
            argv=[sys.argv[0], *args],
            testRunner=ReportingRunner,
        )
    except SystemExit as exit_signal:
        code = exit_signal.code
        if code is None:
            exit_code = 0
        elif isinstance(code, int):
            exit_code = code
        else:
            print(code, file=sys.stderr)
            exit_code = 1
        completion = "complete"
        if report_session.result is not None and report_session.result.shouldStop:
            completion = "stopped_early"
        return report_session.finish(exit_code, completion)
    except KeyboardInterrupt:
        traceback.print_exc()
        return report_session.finish(130, "interrupted")
    except BaseException:
        # 自定义 suite/加载器的异常仍按原样传播，已有用例报告标记为不完整。
        _ = report_session.finish(1, "aborted")
        raise
    return report_session.finish(0, "complete")


if __name__ == "__main__":
    sys.exit(main())
