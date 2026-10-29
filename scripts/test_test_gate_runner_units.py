"""Tests for engine identity and native unittest reports."""

from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import (
    make_tmp,
    test_gate,
    xberg_test_engine,
)


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


class NativeUnittestReportTests(unittest.TestCase):
    """Native unittest outcomes and reporting errors remain observable."""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "unittest-reports"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        _ = shutil.copyfile(test_gate.ROOT / "scripts" / "make_tmp.py", scripts / "make_tmp.py")

    def _run(self, body: str, report_error: str = "") -> subprocess.CompletedProcess[str]:
        _ = (self.root / "report_fixture.py").write_text(
            "import unittest\n" + textwrap.dedent(body), encoding="utf-8"
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
            raise SystemExit(unittest_report.main(["-f", "report_fixture"]))
            """
        )
        return subprocess.run(
            [sys.executable, "-c", bootstrap, str(self.root), report_error],
            cwd=test_gate.ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False,
        )

    def _metadata(self) -> dict:
        reports = list((self.root / ".tmp" / "test-reports").glob("*/run.json"))
        self.assertEqual(len(reports), 1)
        return json.loads(reports[0].read_text(encoding="utf-8"))

    def test_native_failure_error_and_subtests_are_reported(self) -> None:
        for expression, key, marker in (
            ("self.fail('failure-marker')", "failures", "failure-marker"),
            ("raise RuntimeError('error-marker')", "errors", "error-marker"),
            ("with self.subTest(item='child'): self.fail('subtest-marker')", "failures", "subtest-marker"),
            ("with self.subTest(item='child'): raise RuntimeError('subtest-error')", "errors", "subtest-error"),
        ):
            with self.subTest(expression=expression):
                result = self._run(f"""
                    class ReportCase(unittest.TestCase):
                        def test_result(self):
                            {expression}
                """)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                metadata = self._metadata()
                self.assertEqual(metadata["coverage"]["native_unittest"][key], 1)
                self.assertEqual(metadata["coverage"]["completed_logical_cases"], 1)
                self.assertIn(marker, result.stdout)
                shutil.rmtree(self.root / ".tmp" / "test-reports")

    def test_unexpected_success_is_failure(self) -> None:
        result = self._run("""
            class ReportCase(unittest.TestCase):
                @unittest.expectedFailure
                def test_result(self):
                    pass
        """)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual(self._metadata()["coverage"]["native_unittest"]["unexpected_successes"], 1)

    def test_report_errors_return_nonzero(self) -> None:
        for stage in ("__init__", "add_case", "finish"):
            with self.subTest(stage=stage):
                result = self._run("""
                    class ReportCase(unittest.TestCase):
                        def test_result(self):
                            pass
                """, stage)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(f"report-{stage}-broken", result.stdout)

    def test_printed_failure_and_expected_failure_succeed(self) -> None:
        result = self._run("""
            class ReportCase(unittest.TestCase):
                def test_a_print(self):
                    print("FAILED (failures=1) ERROR")
                @unittest.expectedFailure
                def test_b_expected(self):
                    self.fail("expected failure")
        """)
        self.assertEqual(result.returncode, 0, result.stdout)
        metadata = self._metadata()
        self.assertEqual(metadata["coverage"]["native_unittest"]["expected_failures"], 1)
        self.assertEqual(metadata["coverage"]["completed_logical_cases"], 2)
