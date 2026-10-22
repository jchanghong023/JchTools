"""test_test_gate 按主题拆分的用例模块：计时预算 / 引擎身份 / unittest 失败信号与逐用例协议.

类经 scripts/test_test_gate.py 根模块回接并钉回 `scripts.test_test_gate`
模块路径；用例 ID 与拆分前的 `scripts.test_test_gate.<Class>.<method>`
完全一致，断言与夹具逐字保留。
"""

from __future__ import annotations

import contextlib
import dataclasses
import hashlib
import io
import json
import mmap
import os
import shutil
import stat
import struct
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
import zipfile
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

from scripts import (
    bazel_action,
    bazel_gate,
    bazel_service,
    gate_runtime,
    local_gate_plan,
    make_tmp,
    test_gate,
    test_timing,
    xberg_test_engine,
)
from scripts.bazel_gate import BazelSession, _label


class TimingBudgetAndEncodingTests(unittest.TestCase):
    """覆盖 AGENTS.md 3.4：计时预算不扩张，中文用例名原样交给 --exact。"""

    def test_process_startup_is_charged_to_requested_timeout(self) -> None:
        process = MagicMock(spec=subprocess.Popen)
        process.communicate.return_value = ("", "")
        process.wait.return_value = 0
        process.__enter__.return_value = process
        with (
            patch.object(test_timing.subprocess, "Popen", return_value=process),
            patch.object(test_timing.time, "monotonic", side_effect=[100.0, 103.0]),
        ):
            result = test_timing._run_checked(["synthetic.exe"], timeout=10.0)
        process.communicate.assert_called_once_with(timeout=2.0)
        self.assertEqual(result.returncode, 0)

    def test_remaining_budget_bounds_single_test_timeout(self) -> None:
        suite = test_timing.Suite("synthetic", Path("never-run.exe"), seconds=6.0, test_names=["first", "second"])
        with (
            patch.object(
                test_timing, "_run_checked", side_effect=subprocess.TimeoutExpired("never-run.exe", 7.0)
            ) as run,
            patch.object(test_timing.time, "perf_counter", side_effect=[0.0, 7.0]),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            rows = test_timing.time_individual_tests([suite], 7.0)
        self.assertEqual(run.call_args.kwargs["timeout"], 7.0)
        self.assertEqual(run.call_count, 1)
        self.assertEqual(rows[0].note, "单用例超时")
        self.assertIsNone(rows[1].seconds)
        self.assertEqual(rows[1].note, "预算截停未测")

    def test_cleanup_only_budget_does_not_start_a_test(self) -> None:
        suite = test_timing.Suite("synthetic", Path("never-run.exe"), seconds=6.0, test_names=["case"])
        with (
            patch.object(test_timing, "_run_checked", side_effect=AssertionError("清理预算不足时不得启动")),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            rows = test_timing.time_individual_tests([suite], test_timing.PROCESS_TREE_CLEANUP_SECONDS)
        self.assertIsNone(rows[0].seconds)
        self.assertEqual(rows[0].note, "预算截停未测")

    def test_utf8_test_names_survive_gbk_host(self) -> None:
        with patch("subprocess._text_encoding", return_value="gbk"):
            result = test_timing._run_checked(
                [sys.executable, "-c", "import sys; sys.stdout.buffer.write('中文用例: test\\n'.encode('utf-8'))"],
                timeout=10.0,
            )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "中文用例: test\n")


class TimingResultIntegrityTests(unittest.TestCase):
    """覆盖 P-12/P-13：枚举失败不缩小范围，计时发现的失败须返回非零。"""

    def test_failed_binary_listing_is_not_silently_omitted(self) -> None:
        cargo_listing = subprocess.CompletedProcess(
            [], 0, "", "Running first (target/first.exe)\nRunning second (target/second.exe)\n"
        )
        with (
            patch.object(
                test_timing,
                "_run_checked",
                side_effect=[
                    cargo_listing,
                    subprocess.CompletedProcess([], 7, "", "listing failed"),
                    subprocess.CompletedProcess([], 0, "case: test\n", ""),
                ],
            ),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            with self.assertRaises(SystemExit) as stopped:
                test_timing.list_suites(None)
        self.assertEqual(stopped.exception.code, 1)

    def test_main_propagates_suite_and_individual_failures(self) -> None:
        for suite_failed, note, expected_code in (
            (False, "", 0),
            (True, "", 1),
            (False, "失败", 1),
            (False, "单用例超时", 1),
            (False, "预算截停未测", 0),
        ):
            with self.subTest(suite_failed=suite_failed, note=note):
                suite = test_timing.Suite("synthetic", Path("never-run.exe"), failed=suite_failed)
                rows = [test_timing.PerTestRow("synthetic", "case", None if note == "预算截停未测" else 0.1, note)]
                with (
                    patch.object(test_timing, "_reconfigure_stdout"),
                    patch.object(test_timing, "list_suites", return_value=[suite]),
                    patch.object(test_timing, "time_suites"),
                    patch.object(test_timing, "time_individual_tests", return_value=rows),
                    patch.object(test_timing, "write_report") as report,
                ):
                    code = test_timing.main(["--skip-build"])
                self.assertEqual(code, expected_code)
                report.assert_called_once_with([suite], rows, 600.0)


class XbergCachedIdentityTests(unittest.TestCase):
    """覆盖 XB-10：损坏的本机收据不能阻止最新版本重新获取。"""

    def test_invalid_receipt_requests_verified_reinstallation(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "engine-identity-regression"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            engine = root / "engine"
            engine.mkdir()
            (engine / "xberg.exe").write_bytes(b"inert engine; never execute")
            receipt = engine / xberg_test_engine.RECEIPT_NAME
            manifest = root / "resources"
            manifest.mkdir()
            (manifest / "markdown-assets.json").write_text('{"xberg": {}}', encoding="utf-8")
            release = xberg_test_engine.Release("v-latest", "0" * 64, 1)
            with (
                patch.object(xberg_test_engine, "ROOT", root),
                patch.object(xberg_test_engine, "TEST_ENGINE", engine),
            ):
                for content in ("", "{", "not json", "\ufeff{}"):
                    with self.subTest(content=content):
                        receipt.write_text(content, encoding="utf-8")
                        self.assertFalse(xberg_test_engine.current_engine_matches(release))

    def test_redirected_fixed_directory_is_rejected_before_cached_reuse(self) -> None:
        with (
            patch.object(xberg_test_engine.shutil, "which", return_value="never-run-gh"),
            patch.object(xberg_test_engine.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "{}")),
            patch.object(
                xberg_test_engine, "release_from_metadata", return_value=xberg_test_engine.Release("v", "0" * 64, 1)
            ),
            patch.object(xberg_test_engine, "ensure_test_engine_target", side_effect=OSError("重定向目录")),
            patch.object(xberg_test_engine, "current_engine_matches", return_value=True) as cached,
            patch.dict(os.environ, {"JCHTOOLS_TEST_XBERG_DIR": ""}),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(xberg_test_engine.main(), 1)
        cached.assert_not_called()


class UnittestFailureSignalTests(unittest.TestCase):
    """真实结果回调必须在卡住的清理结束前通知门；文本输出不构成失败。"""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "unittest-signals"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        # 真实 TimingReport 仍经 workspace 工厂落盘，所有报告随夹具目录一起清理。
        scripts = self.root / "scripts"
        scripts.mkdir()
        _ = shutil.copyfile(test_gate.ROOT / "scripts" / "make_tmp.py", scripts / "make_tmp.py")

    def _start(
        self,
        body: str,
        *,
        report_error: str | None = None,
        signal_enabled: bool = True,
        broken_signal: bool = False,
    ) -> tuple[subprocess.Popen[bytes], Path, Path]:
        fixture = self.root / "signal_fixture.py"
        _ = fixture.write_text(
            "import threading\nimport unittest\nfrom pathlib import Path\n"
            "ROOT = Path(__file__).resolve().parent\n"
            "def wait_forever():\n"
            "    (ROOT / 'waiting').touch()\n"
            "    threading.Event().wait()\n" + textwrap.dedent(body),
            encoding="utf-8",
        )
        bootstrap = textwrap.dedent(
            """
            import sys
            from pathlib import Path
            from scripts import test_reports, unittest_report
            root = Path(sys.argv[1])
            sys.path.insert(0, str(root))
            sys.dont_write_bytecode = True
            test_reports.ROOT = root
            test_reports._source_identity = lambda: {}
            error_stage = sys.argv[2]
            if error_stage:
                def broken_report(*args, **kwargs):
                    raise OSError("report-" + error_stage + "-broken")
                setattr(test_reports.TimingReport, error_stage, broken_report)
            raise SystemExit(unittest_report.main(["-f", "signal_fixture"]))
            """
        )
        signal = self.root / "failure.jsonl"
        if broken_signal:
            signal.mkdir()
        environment = dict(os.environ)
        environment.pop("JCHTOOLS_GATE_FAILURE_FILE", None)
        if signal_enabled:
            environment["JCHTOOLS_GATE_FAILURE_FILE"] = str(signal)
        output = self.root / "runner.log"
        with output.open("wb") as stream:
            process = subprocess.Popen(
                [sys.executable, "-c", bootstrap, str(self.root), report_error or ""],
                cwd=test_gate.ROOT,
                env=environment,
                stdout=stream,
                stderr=subprocess.STDOUT,
            )
        self.addCleanup(self._stop, process)
        return process, signal, output

    @staticmethod
    def _stop(process: subprocess.Popen[bytes]) -> None:
        if process.poll() is None:
            process.kill()
        _ = process.wait(timeout=5)

    def _details(self, signal: Path) -> list[str]:
        if not signal.is_file():
            return []
        contents = signal.read_text(encoding="utf-8")
        lines = contents.splitlines(keepends=True)
        # 只消费完整 JSON 行，不把仍在写入的半行当作损坏的事件。
        details: list[str] = []
        for line in lines:
            if not line.endswith("\n"):
                continue
            event = json.loads(line)
            self.assertEqual(set(event), {"detail"})
            detail = event["detail"]
            self.assertIsInstance(detail, str)
            self.assertTrue(detail)
            self.assertLessEqual(len(detail), 1024)
            self.assertEqual(len(detail.splitlines()), 1)
            details.append(detail)
        return details

    def _assert_signal_while_waiting(self, body: str, expected: str) -> None:
        process, signal, output = self._start(body)
        try:
            deadline = time.monotonic() + 10
            details: list[str] = []
            while time.monotonic() < deadline:
                details = self._details(signal)
                if details and (self.root / "waiting").exists():
                    break
                if process.poll() is not None:
                    break
                time.sleep(0.01)
            self.assertIsNone(process.poll(), output.read_text(encoding="utf-8", errors="replace"))
            self.assertTrue((self.root / "waiting").exists(), "夹具未进入永久清理等待")
            self.assertTrue(details, "真实失败后 runner 卡在清理时，门仍收不到失败事件")
            self.assertIn(expected, details[0])
        finally:
            self._stop(process)
            signal.unlink(missing_ok=True)
            (self.root / "waiting").unlink(missing_ok=True)

    def test_failure_and_error_signal_before_blocked_teardown(self) -> None:
        for expression, expected in (
            ("self.fail('failure-marker')", "failure-marker"),
            ("raise RuntimeError('error-marker')", "error-marker"),
        ):
            with self.subTest(expected=expected):
                self._assert_signal_while_waiting(
                    f"""
                    class SignalCase(unittest.TestCase):
                        def test_result(self):
                            {expression}
                        def tearDown(self):
                            wait_forever()
                    """,
                    expected,
                )

    def test_subtest_signals_before_blocked_teardown(self) -> None:
        for expression, expected in (
            ("self.fail('subtest-failure-marker')", "subtest-failure-marker"),
            ("raise RuntimeError('subtest-error-marker')", "subtest-error-marker"),
        ):
            with self.subTest(expected=expected):
                self._assert_signal_while_waiting(
                    f"""
                    class SignalCase(unittest.TestCase):
                        def test_result(self):
                            with self.subTest(item="child"):
                                {expression}
                        def tearDown(self):
                            wait_forever()
                    """,
                    expected,
                )

    def test_class_and_module_fixture_errors_signal_before_blocked_cleanup(self) -> None:
        for body, expected in (
            (
                """
                class SignalCase(unittest.TestCase):
                    @classmethod
                    def setUpClass(cls):
                        cls.addClassCleanup(wait_forever)
                        raise RuntimeError("class-fixture-marker")
                    def test_result(self):
                        pass
                """,
                "class-fixture-marker",
            ),
            (
                """
                def setUpModule():
                    unittest.addModuleCleanup(wait_forever)
                    raise RuntimeError("module-fixture-marker")
                class SignalCase(unittest.TestCase):
                    def test_result(self):
                        pass
                """,
                "module-fixture-marker",
            ),
        ):
            with self.subTest(expected=expected):
                self._assert_signal_while_waiting(body, expected)

    def test_unexpected_success_signals_before_blocked_class_cleanup(self) -> None:
        self._assert_signal_while_waiting(
            """
            class SignalCase(unittest.TestCase):
                @classmethod
                def setUpClass(cls):
                    cls.addClassCleanup(wait_forever)
                @unittest.expectedFailure
                def test_result(self):
                    pass
            """,
            "unexpectedSuccess",
        )

    def test_report_errors_signal_and_failfast_skips_later_cases(self) -> None:
        for stage in ("__init__", "add_case", "finish"):
            with self.subTest(stage=stage):
                process, signal, output = self._start(
                    """
                    class SignalCase(unittest.TestCase):
                        def test_a_first(self):
                            pass
                        def test_b_later(self):
                            (ROOT / "later").touch()
                    """,
                    report_error=stage,
                )
                # 初始化失败可保留 unittest 的“未运行用例”退出码，不强制改写为 1。
                self.assertNotEqual(process.wait(timeout=10), 0, output.read_text(encoding="utf-8", errors="replace"))
                details = self._details(signal)
                self.assertTrue(details, f"{stage} 报告错误未通知父门")
                self.assertIn(f"report-{stage}-broken", details[0])
                if stage != "finish":
                    self.assertFalse((self.root / "later").exists(), "报告失败后仍执行下一用例")
                else:
                    self.assertTrue((self.root / "later").exists())
                (self.root / "later").unlink(missing_ok=True)
                signal.unlink(missing_ok=True)

    def test_printed_failure_text_and_expected_failure_do_not_signal(self) -> None:
        process, signal, output = self._start(
            """
            class SignalCase(unittest.TestCase):
                def test_a_print(self):
                    print("FAILED (failures=1) ERROR failure-marker")
                @unittest.expectedFailure
                def test_b_expected(self):
                    self.fail("expected failure")
            """
        )
        self.assertEqual(process.wait(timeout=10), 0, output.read_text(encoding="utf-8", errors="replace"))
        self.assertEqual(self._details(signal), [])
        reports = list((self.root / ".tmp" / "test-reports").glob("*/run.json"))
        self.assertEqual(len(reports), 1)
        metadata = json.loads(reports[0].read_text(encoding="utf-8"))
        self.assertEqual(metadata["coverage"]["native_unittest"]["expected_failures"], 1)
        self.assertEqual(metadata["coverage"]["completed_logical_cases"], 2)

    def test_missing_or_unwritable_sidecar_preserves_native_failure_and_report(self) -> None:
        for enabled, broken in ((False, False), (True, True)):
            with self.subTest(signal_enabled=enabled):
                process, _signal, output = self._start(
                    """
                    class SignalCase(unittest.TestCase):
                        def test_result(self):
                            self.fail("native-failure-marker")
                    """,
                    signal_enabled=enabled,
                    broken_signal=broken,
                )
                self.assertEqual(process.wait(timeout=10), 1)
                text = output.read_text(encoding="utf-8", errors="replace")
                self.assertIn("native-failure-marker", text)
                self.assertIn("FAILED (failures=1)", text)
                reports = sorted((self.root / ".tmp" / "test-reports").glob("*/run.json"))
                self.assertEqual(len(reports), 1 if not enabled else 2)
                metadata = json.loads(reports[-1].read_text(encoding="utf-8"))
                self.assertEqual(metadata["coverage"]["native_unittest"]["failures"], 1)
                self.assertEqual(metadata["coverage"]["completed_logical_cases"], 1)


class GateUnittestCasesTests(unittest.TestCase):
    """逐 ID 协议只加载；精确执行保留原始实例、夹具和 subTest。"""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "unittest-cases"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        _ = shutil.copyfile(test_gate.ROOT / "scripts" / "make_tmp.py", scripts / "make_tmp.py")

    def _invoke(self, body: str, *arguments: str) -> subprocess.CompletedProcess[str]:
        _ = (self.root / "case_fixture.py").write_text(
            "import unittest\nfrom pathlib import Path\nROOT = Path(__file__).resolve().parent\n"
            + textwrap.dedent(body),
            encoding="utf-8",
        )
        bootstrap = textwrap.dedent(
            """
            import sys
            from pathlib import Path
            from scripts import gate_unittest, test_reports
            root = Path(sys.argv[1])
            sys.path.insert(0, str(root))
            sys.dont_write_bytecode = True
            test_reports.ROOT = root
            test_reports._source_identity = lambda: {}
            raise SystemExit(gate_unittest.main(sys.argv[2:]))
            """
        )
        environment = dict(os.environ)
        environment["JCHTOOLS_GATE_FAILURE_FILE"] = str(self.root / "failure.jsonl")
        return subprocess.run(
            [sys.executable, "-c", bootstrap, str(self.root), *arguments],
            cwd=test_gate.ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=15,
            check=False,
        )

    def test_list_does_not_execute_fixtures_or_subtests_and_deduplicates_ids(self) -> None:
        result = self._invoke(
            """
            def setUpModule():
                raise AssertionError("must not run module setup")
            class Cases(unittest.TestCase):
                @classmethod
                def setUpClass(cls):
                    raise AssertionError("must not run class setup")
                def setUp(self):
                    raise AssertionError("must not run setup")
                def test_parent(self):
                    for item in range(2):
                        with self.subTest(item=item):
                            self.fail("must not run test")
                def tearDown(self):
                    raise AssertionError("must not run teardown")
            def load_tests(loader, tests, pattern):
                return unittest.TestSuite([tests, Cases("test_parent")])
            """,
            "--list",
            "--",
            "case_fixture",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout.splitlines()[-1].removeprefix("JCHTOOLS_TEST_CASES=")),
            ["case_fixture.Cases.test_parent"],
        )
        self.assertFalse((self.root / "failure.jsonl").exists())
        self.assertEqual(list((self.root / ".tmp").glob("test-reports/*/run.json")), [])

    def test_exact_case_retains_duplicates_subtests_and_real_fixtures(self) -> None:
        result = self._invoke(
            """
            def record(value):
                with (ROOT / "events").open("a", encoding="utf-8") as stream:
                    stream.write(value + "\\n")
            def setUpModule():
                record("module setup")
            def tearDownModule():
                record("module teardown")
            class Cases(unittest.TestCase):
                @classmethod
                def setUpClass(cls):
                    record("class setup")
                @classmethod
                def tearDownClass(cls):
                    record("class teardown")
                def setUp(self):
                    record("setup")
                def tearDown(self):
                    record("teardown")
                def test_parent(self):
                    for item in range(2):
                        with self.subTest(item=item):
                            record(str(item))
                            self.assertLess(item, 2)
                def test_parent_extra(self):
                    self.fail("prefix matching must not execute this")
            def load_tests(loader, tests, pattern):
                return unittest.TestSuite([tests, Cases("test_parent")])
            """,
            "--case",
            "case_fixture.Cases.test_parent",
            "--",
            "-f",
            "case_fixture",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.root / "events").read_text(encoding="utf-8").splitlines(),
            [
                "module setup",
                "class setup",
                "setup",
                "0",
                "1",
                "teardown",
                "setup",
                "0",
                "1",
                "teardown",
                "class teardown",
                "module teardown",
            ],
        )
        reports = list((self.root / ".tmp" / "test-reports").glob("*/run.json"))
        self.assertEqual(len(reports), 1)
        report = json.loads(reports[0].read_text(encoding="utf-8"))
        self.assertEqual(report["coverage"]["native_unittest"]["tests_run"], 2)
        self.assertEqual(report["coverage"]["completed_logical_cases"], 2)
        self.assertEqual(
            report["command"],
            [
                sys.executable,
                "-m",
                "scripts.gate_unittest",
                "--case",
                "case_fixture.Cases.test_parent",
                "--",
                "-f",
                "case_fixture",
            ],
        )

    def test_loading_errors_never_emit_valid_list_and_notify_failure(self) -> None:
        for body in (
            'raise ImportError("broken-import-marker")',
            "def load_tests(loader, tests, pattern):\n    raise RuntimeError('broken-load-marker')",
        ):
            with self.subTest(body=body):
                result = self._invoke(body, "--list", "--", "case_fixture")
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("JCHTOOLS_TEST_CASES=", result.stdout)
                signal = self.root / "failure.jsonl"
                self.assertTrue(signal.exists())
                self.assertTrue(json.loads(signal.read_text(encoding="utf-8").splitlines()[0])["detail"])
                signal.unlink()

    def test_missing_exact_case_fails_instead_of_zero_case_pass(self) -> None:
        result = self._invoke(
            """
            class Cases(unittest.TestCase):
                def test_present(self):
                    (ROOT / "unexpected-case").touch()
                    self.fail("must not execute unrelated case")
            """,
            "--case",
            "case_fixture.Cases.test_missing",
            "--",
            "case_fixture",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.root / "failure.jsonl").exists())
        self.assertFalse((self.root / "unexpected-case").exists())
