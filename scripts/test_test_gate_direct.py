"""顺序执行、独占日志、退出码及临时目录回归。"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts import gate_runtime, test_gate


class DirectGateTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="gate-direct-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def make_run(self) -> gate_runtime.GateRun:
        return gate_runtime.GateRun("fastcheck", self.root, self.root / "logs")

    def test_repeated_commands_preserve_distinct_logs(self) -> None:
        first = self.make_run()
        stage = gate_runtime.Stage("sample", [sys.executable, "-c", "print('first')"])
        first_result = first.run([stage])[0]
        second_result = first.run([gate_runtime.Stage("sample", [sys.executable, "-c", "print('second')"])])[0]
        other = self.make_run()
        third_result = other.run([stage])[0]
        self.assertEqual(len(first.results), 2)
        assert first_result.log is not None  # nosec B101: 测试结果必须保留日志。
        assert second_result.log is not None  # nosec B101: 测试结果必须保留日志。
        self.assertNotEqual(first_result.log, second_result.log)
        self.assertNotEqual(first_result.log, third_result.log)
        self.assertIn("first", first_result.log.read_text(encoding="utf-8"))
        self.assertIn("second", second_result.log.read_text(encoding="utf-8"))
        self.assertEqual(first.finish(), 0)
        self.assertEqual(other.finish(), 0)

    def test_nonzero_exit_preserves_log_and_skips_next_command(self) -> None:
        run = self.make_run()
        marker = self.root / "later"
        stages = [
            gate_runtime.Stage("failure", [sys.executable, "-c", "import sys; print('evidence'); sys.exit(7)"]),
            gate_runtime.Stage("later", [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(marker)]),
        ]
        with self.assertRaises(subprocess.CalledProcessError) as caught:
            run.run(stages)
        self.assertEqual(caught.exception.returncode, 7)
        assert run.results[0].log is not None  # nosec B101: 失败结果必须保留日志。
        self.assertEqual(run.results[0].returncode, 7)
        self.assertIn("evidence", run.results[0].log.read_text(encoding="utf-8"))
        self.assertFalse(marker.exists())
        self.assertEqual(run.finish(), 1)

    def test_start_failure_is_recorded_and_nonzero(self) -> None:
        run = self.make_run()
        with self.assertRaises(OSError):
            run.run([gate_runtime.Stage("missing", [str(self.root / "missing-program")])])
        self.assertEqual(run.results[0].status, gate_runtime.STATUS_FAILED)
        self.assertEqual(run.finish(), 1)
        with patch.object(test_gate, "ROOT", self.root), patch.object(test_gate, "LOG_DIR", self.root / "logs"):
            self.assertNotEqual(test_gate.execute_gate("fastcheck", stages_override=[gate_runtime.Stage("missing", [str(self.root / "missing-program")])]), 0)

    def test_binding_loop_blocks_next_command(self) -> None:
        run = self.make_run()
        marker = self.root / "later"
        with self.assertRaises(subprocess.CalledProcessError):
            run.run([
                gate_runtime.Stage("build", [sys.executable, "-c", "print('binding loop detected')"]),
                gate_runtime.Stage("later", [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(marker)]),
            ])
        self.assertFalse(marker.exists())
        self.assertEqual(run.results[0].status, gate_runtime.STATUS_FAILED)
        self.assertEqual(run.finish(), 1)

    def test_temporary_environment_is_isolated_and_cleaned(self) -> None:
        run = self.make_run()
        run.prepare_temporary()
        code = (
            "import os,pathlib; p=pathlib.Path(os.environ['TEMP']); "
            "print(p); assert os.environ['TMP']==str(p); "
            "(p/'ordinary').mkdir()"
        )
        result = run.run([gate_runtime.Stage("temp", [sys.executable, "-c", code])])[0]
        assert result.log is not None  # nosec B101: 测试结果必须保留日志。
        temporary = Path(result.log.read_text(encoding="utf-8").strip())
        self.assertFalse(temporary.is_relative_to(self.root))
        if os.name == "nt":
            special = "\\\\?\\" + str(temporary / "trailing.")
            os.mkdir(special)
        self.assertTrue(temporary.exists())
        self.assertEqual(run.finish(), 0)
        self.assertFalse(temporary.exists())

    def test_authorization_is_required(self) -> None:
        with patch.object(test_gate, "ROOT", self.root), patch.object(test_gate, "LOG_DIR", self.root / "logs"):
            self.assertNotEqual(test_gate.main(["fulltest"]), 0)
            self.assertNotEqual(test_gate.main(["slowtest"]), 0)
