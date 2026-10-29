"""pytest gate reporting; pytest/testmon own test selection."""

from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path
from typing import TYPE_CHECKING, Protocol, cast, final

import pytest

from scripts.test_reports import Case, TimingReport

if TYPE_CHECKING:
    from collections.abc import Generator


_EMPTY_RESULT_LENGTH = 2


class _TestmonConfig(Protocol):
    select: bool


class _ConfigWithTestmon(Protocol):
    testmon_config: _TestmonConfig


class _TestmonSelect(Protocol):
    deselected_files: list[str]
    deselected_tests: list[str]


class _WorkerConfig(Protocol):
    workerinput: dict[str, object]
    workeroutput: dict[str, object]


class _WorkerNode(Protocol):
    workeroutput: dict[str, object]


def _is_worker(config: pytest.Config) -> bool:
    return hasattr(config, "workerinput")


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.getgroup("jchtools").addoption("--jchtools-report-dir", default=None)


@pytest.hookimpl(tryfirst=True)
def pytest_configure(config: pytest.Config) -> None:
    datafile = os.environ.get("TESTMON_DATAFILE")
    if datafile:
        Path(datafile).parent.mkdir(parents=True, exist_ok=True)
    _ = config.pluginmanager.register(_GatePlugin(config), "jchtools-gate")


@final
class _GatePlugin:
    def __init__(self, config: pytest.Config) -> None:
        self.config = config
        self.report: TimingReport | None = None
        self.phases: dict[str, list[pytest.TestReport]] = {}
        self.testmon_deselected = 0
        self.other_deselected = 0
        self.ignored_testmon_files: set[str] = set()
        self.reporting_failed = False
        self.worker = _is_worker(config)
        self.worker_empty_results: list[tuple[bool, bool]] = []
        self.concurrency: dict[str, int] = {"test_workers": 0 if config.getoption("numprocesses", default=0) else 1}
        directory = cast("object", config.getoption("--jchtools-report-dir"))
        if not self.worker and isinstance(directory, str) and directory:
            try:
                report = TimingReport(
                    "pytest",
                    [sys.executable, "-m", "pytest", *config.invocation_params.args],
                    scope={"arguments": list(config.invocation_params.args)},
                    timing_basis="pytest setup, call and teardown durations",
                    concurrency=self.concurrency,
                )
                parent = Path(directory).resolve()
                parent.mkdir(parents=True, exist_ok=True)
                destination = parent / report.run_id
                _ = shutil.move(str(report.directory), str(destination))
                report.directory = destination
                print(f"pytest report: {destination}", flush=True)
                self.report = report
            except Exception as error:  # noqa: BLE001 - report errors must cause a nonzero exit.
                self._report_error(error)

    def _report_error(self, error: Exception) -> None:
        self.reporting_failed = True
        print(f"pytest report error: {error}", file=sys.stderr, flush=True)

    @pytest.hookimpl(optionalhook=True)
    def pytest_testnodeready(self, node: _WorkerNode) -> None:
        del node
        self.concurrency["test_workers"] += 1

    @pytest.hookimpl(optionalhook=True)
    def pytest_testnodedown(self, node: _WorkerNode, error: object) -> None:
        result = node.workeroutput.get("jchtools_empty")
        if error is None and isinstance(result, tuple):
            values = cast("tuple[object, ...]", result)
            if len(values) == _EMPTY_RESULT_LENGTH:
                evidence, other = values
                if isinstance(evidence, bool) and isinstance(other, bool):
                    self.worker_empty_results.append((evidence, other))

    def pytest_deselected(self, items: list[pytest.Item]) -> None:
        for item in items:
            if type(item).__module__ == "testmon.pytest_testmon" and type(item).__name__ == "FakeItemFromTestmon":
                self.testmon_deselected += 1
            else:
                self.other_deselected += 1

    @pytest.hookimpl(wrapper=True)
    def pytest_ignore_collect(
        self, collection_path: Path, config: pytest.Config
    ) -> Generator[None, bool | None, bool | None]:
        result = yield
        select = cast("_TestmonSelect | None", config.pluginmanager.get_plugin("TestmonSelect"))
        if result and select is not None and cast("_ConfigWithTestmon", cast("object", config)).testmon_config.select:
            try:
                relative = collection_path.relative_to(config.rootpath).as_posix()
            except ValueError:
                pass
            else:
                if relative in select.deselected_files and any(
                    name == relative or name.startswith(f"{relative}::") for name in select.deselected_tests
                ):
                    self.ignored_testmon_files.add(relative)
        return result

    def pytest_runtest_logreport(self, report: pytest.TestReport) -> None:
        if self.worker:
            return
        phases = self.phases.setdefault(report.nodeid, [])
        phases.append(report)
        if report.when != "teardown":
            return
        reports = self.phases.pop(report.nodeid)
        failures = [part for part in reports if part.failed]
        skips = [part for part in reports if part.skipped]
        status = (
            "error"
            if failures and any(part.when != "call" for part in failures)
            else ("failure" if failures else "skipped" if skips else "passed")
        )
        detail = "\n".join(str(part.longrepr) for part in failures or skips)
        path, _, name = report.nodeid.partition("::")
        if self.report is not None:
            try:
                self.report.add_case(
                    Case(name or path, path, sum(part.duration for part in reports), status, detail, path)
                )
            except Exception as error:  # noqa: BLE001 - report errors must cause a nonzero exit.
                self._report_error(error)

    def _finish_report(self, session: pytest.Session) -> None:
        if self.report is not None:
            try:
                previous_status = session.exitstatus
                session.exitstatus = self.report.finish(int(previous_status))
            except Exception as error:  # noqa: BLE001 - report errors must cause a nonzero exit.
                self._report_error(error)
                if session.exitstatus == pytest.ExitCode.OK:
                    session.exitstatus = pytest.ExitCode.TESTS_FAILED

    @pytest.hookimpl(wrapper=True, tryfirst=True)
    def pytest_sessionfinish(self, session: pytest.Session, exitstatus: int) -> Generator[None, object]:
        if self.worker:
            cast("_WorkerConfig", cast("object", self.config)).workeroutput["jchtools_empty"] = (
                bool(self.testmon_deselected or self.ignored_testmon_files),
                bool(self.other_deselected),
            )
        yield
        if self.worker:
            return
        select = cast("_TestmonSelect | None", self.config.pluginmanager.get_plugin("TestmonSelect"))
        evidence = bool(self.testmon_deselected or self.ignored_testmon_files)
        other = bool(self.other_deselected)
        if self.worker_empty_results:
            evidence = any(item[0] for item in self.worker_empty_results)
            other = any(item[1] for item in self.worker_empty_results)
        if exitstatus == pytest.ExitCode.NO_TESTS_COLLECTED:
            # On the controller testmon selection happens in workers, not locally.
            selection_enabled = (
                bool(self.worker_empty_results)
                if not self.worker and self.concurrency["test_workers"] > 0 and select is None
                else select is not None
                and cast("_ConfigWithTestmon", cast("object", self.config)).testmon_config.select
            )
            valid_empty = selection_enabled and not session.testsfailed and not other and evidence
            session.exitstatus = pytest.ExitCode.OK if valid_empty else pytest.ExitCode.NO_TESTS_COLLECTED
        if self.reporting_failed and session.exitstatus == pytest.ExitCode.OK:
            session.exitstatus = pytest.ExitCode.TESTS_FAILED
        self._finish_report(session)
