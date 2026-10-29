"""Integration tests for pytest/testmon selection and reporting."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch
from xml.etree import ElementTree

from scripts import make_tmp

ROOT = Path(__file__).resolve().parents[1]


class PytestGateIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.root = ROOT / ".tmp" / "pytest-gate-tests" / uuid.uuid4().hex
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(self.root)]):
            self.assertEqual(make_tmp.main(), 0)
        self.addCleanup(shutil.rmtree, self.root)
        self.env = os.environ.copy()
        self.env["PYTHONPATH"] = os.pathsep.join(filter(None, [str(ROOT), self.env.get("PYTHONPATH", "")]))
        self.env["TESTMON_DATAFILE"] = str(self.root / "cache" / "testmondata")
        self.env.pop("PYTEST_ADDOPTS", None)
        (self.root / "pytest.ini").write_text("[pytest]\ntestpaths = tests\n", encoding="utf-8")
        (self.root / "tests").mkdir()

    def run_pytest(self, *args, report=None):
        command = [sys.executable, "-m", "pytest", "-p", "scripts.pytest_gate", "-x",
                   "-n", "2", "--dist", "loadscope", "--max-worker-restart", "0"]
        if report is not None:
            command += ["--jchtools-report-dir", str(report)]
        return subprocess.run(command + list(args), cwd=self.root, env=self.env, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=120, check=False)

    def test_testmon_selects_changed_dependencies_and_full_runs_all(self):
        # Testmon records executed dependency code, not constants imported before tracing.
        (self.root / "a.py").write_text("def value():\n    return 1\n", encoding="utf-8")
        (self.root / "b.py").write_text("def value():\n    return 2\n", encoding="utf-8")
        for name in ("a", "b"):
            (self.root / "tests" / f"test_{name}.py").write_text(
                f"import {name}, os, time\nfrom pathlib import Path\n"
                f"def test_{name}():\n"
                f"    if os.environ.get('GATE_HANDSHAKE'):\n"
                f"        Path('pid-{name}').write_text(str(os.getpid()))\n"
                f"        deadline = time.monotonic() + 20\n"
                f"        while not Path('pid-{'b' if name == 'a' else 'a'}').exists():\n"
                f"            assert time.monotonic() < deadline, 'workers did not overlap'\n"
                f"            time.sleep(0.05)\n"
                f"    assert {name}.value() > 0\n"
                f"    Path('executed-{name}').write_text('{name}', encoding='utf-8')\n",
                encoding="utf-8",
            )
        self.env["GATE_HANDSHAKE"] = "1"
        result = self.run_pytest("--testmon", "--testmon-env=integration", "tests", report=self.root / "report-first")
        self.assertEqual(result.returncode, 0, result.stdout)
        del self.env["GATE_HANDSHAKE"]
        self.assertNotEqual((self.root / "pid-a").read_text(), (self.root / "pid-b").read_text())
        def executed_tests():
            return [path.read_text(encoding="utf-8") for path in self.root.glob("executed-*")]
        def clear_executions():
            for path in self.root.glob("executed-*"):
                path.unlink()
        self.assertCountEqual(executed_tests(), ["a", "b"])
        report = ElementTree.parse(next((self.root / "report-first").glob("*/junit.xml"))).getroot()
        self.assertEqual(report.attrib["tests"], "2")
        self.assertTrue(all(float(case.attrib["time"]) >= 0 for case in report.findall("testcase")))
        metadata = json.loads(next((self.root / "report-first").glob("*/run.json")).read_text(encoding="utf-8"))
        self.assertEqual(metadata["concurrency"]["test_workers"], 2)
        self.assertEqual(len(list((self.root / "report-first").glob("*/junit.xml"))), 1)
        clear_executions()
        result = self.run_pytest("--testmon", "--testmon-env=integration", "tests")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(executed_tests(), [], result.stdout)
        (self.root / "a.py").write_text("def value():\n    return 3\n", encoding="utf-8")
        result = self.run_pytest("--testmon", "--testmon-env=integration", "tests")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(executed_tests(), ["a"], result.stdout)
        clear_executions()
        result = self.run_pytest("--no-testmon", "tests")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertCountEqual(executed_tests(), ["a", "b"])

    def test_zero_collection_and_configuration_errors_are_not_cache_hits(self):
        empty = self.run_pytest("--testmon", "tests")
        self.assertEqual(empty.returncode, 5, empty.stdout)
        invalid = self.run_pytest("--testmon", "--not-a-real-option", "tests")
        self.assertNotEqual(invalid.returncode, 0, invalid.stdout)
        (self.root / "tests" / "test_one.py").write_text("def test_one(): pass\n", encoding="utf-8")
        first = self.run_pytest("--testmon", "tests")
        self.assertEqual(first.returncode, 0, first.stdout)
        (self.root / "tests" / "test_one.py").unlink()
        empty_after_cache = self.run_pytest("--testmon", "tests")
        self.assertEqual(empty_after_cache.returncode, 5, empty_after_cache.stdout)

    def test_failure_is_reported_and_stops_the_run(self):
        (self.root / "tests" / "test_failure.py").write_text(
            "def test_a_failure(): assert False\n"
            "def test_b_not_run(): assert False\n",
            encoding="utf-8",
        )
        result = self.run_pytest("--no-testmon", "tests", report=self.root / "report-failure")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        report = ElementTree.parse(next((self.root / "report-failure").glob("*/junit.xml"))).getroot()
        self.assertEqual(report.attrib["tests"], "1")
        self.assertEqual(report.attrib["failures"], "1")

    def test_report_directory_error_is_nonzero(self):
        (self.root / "tests" / "test_pass.py").write_text("def test_pass(): pass\n", encoding="utf-8")
        destination = self.root / "not-a-directory"
        destination.write_text("file", encoding="utf-8")
        result = self.run_pytest("--no-testmon", "tests", report=destination)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("pytest report error", result.stdout)

    def test_unittest_failure_uses_native_pytest_reporting(self):
        (self.root / "tests" / "test_unittest.py").write_text(
            "import unittest\n"
            "class TestCase(unittest.TestCase):\n"
            "    def test_failure(self): self.fail('native-marker')\n",
            encoding="utf-8",
        )
        result = self.run_pytest("--no-testmon", "tests", report=self.root / "report-unittest")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("native-marker", result.stdout)
        report = ElementTree.parse(next((self.root / "report-unittest").glob("*/junit.xml"))).getroot()
        self.assertEqual(report.attrib["failures"], "1")

