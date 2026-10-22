"""test_test_gate 按主题拆分的用例模块：首错即停与阻塞输出下的监督通知回归.

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


class GateFailFastTests(unittest.TestCase):
    """覆盖 P-13：阶段失败立即停止整个门，保留原始证据且不遗留并行进程。"""

    def setUp(self) -> None:
        self.workspace = test_gate.ROOT / ".tmp" / "test-gate" / "fail-fast-regression"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(self.workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=self.workspace)
        self.addCleanup(temporary.cleanup)
        self.workspace = Path(temporary.name)

    def _assert_owned_child_exited(self, pid: int) -> None:
        try:
            handle = test_gate.win32api.OpenProcess(test_gate.win32con.SYNCHRONIZE, False, pid)
        except test_gate.win32api.error as error:
            self.assertEqual(error.winerror, 87, "进程查询失败不能冒充已退出")
            return
        try:
            self.assertEqual(
                test_gate.win32event.WaitForSingleObject(handle, 0),
                test_gate.win32event.WAIT_OBJECT_0,
                "门返回后仍遗留所属子进程",
            )
        finally:
            test_gate.win32api.CloseHandle(handle)

    def test_failure_stops_pending_and_running_commands_at_every_level(self) -> None:
        for level in ("fastcheck", "fulltest", "slowtest"):
            with self.subTest(level=level), tempfile.TemporaryDirectory(dir=self.workspace) as temporary:
                root = Path(temporary)
                ready, release, later = (root / name for name in ("ready", "release", "later"))
                peer_code = (
                    "import os,pathlib,sys,time; "
                    "ready,release=map(pathlib.Path,sys.argv[1:]); ready.write_text(str(os.getpid())); "
                    "\nwhile not release.exists(): time.sleep(0.01)"
                )
                failure_code = (
                    "import pathlib,subprocess,sys,time; ready=pathlib.Path(sys.argv[1]); "
                    f"subprocess.Popen([sys.executable,'-c',{peer_code!r},*sys.argv[1:]]); "
                    "\nwhile not ready.exists(): time.sleep(0.01)"
                    "\nprint('synthetic failure',flush=True); sys.exit(7)"
                )
                later_code = "import pathlib,sys; [pathlib.Path(p).touch() for p in sys.argv[1:]]"
                stages = [
                    gate_runtime.Stage(
                        "first-error",
                        [sys.executable, "-c", failure_code, str(ready), str(release)],
                        exclusive_resource="failure-chain",
                    ),
                    gate_runtime.Stage(
                        "must-not-run",
                        [sys.executable, "-c", later_code, str(later), str(release)],
                        exclusive_resource="failure-chain",
                    ),
                ]
                processes: list[subprocess.Popen[bytes]] = []
                start = test_gate._start_owned_command

                def record_start(argv, **kwargs):
                    proc = start(argv, **kwargs)
                    processes.append(proc)
                    return proc

                output = io.StringIO()
                with (
                    patch.object(test_gate, "LOG_DIR", root / "logs"),
                    patch.object(test_gate, "_start_owned_command", record_start),
                    contextlib.redirect_stdout(output),
                ):
                    code = test_gate.execute_gate(level, 60.0, stages_override=stages)
                self.assertEqual(code, 1)
                self.assertFalse(later.exists(), "阶段失败后仍启动了后续命令")
                self.assertTrue(all(proc.poll() is not None for proc in processes))
                self._assert_owned_child_exited(int(ready.read_text()))
                self.assertIn("exit=7", output.getvalue())
                self.assertIn("synthetic failure", output.getvalue())

    def test_launch_error_skips_remaining_commands(self) -> None:
        later = self.workspace / "launch-error-later"
        stages = [
            gate_runtime.Stage("missing-executable", [str(self.workspace / "missing.exe")]),
            gate_runtime.Stage(
                "must-not-run",
                [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)],
            ),
        ]
        with contextlib.redirect_stdout(io.StringIO()):
            code = test_gate.execute_gate("fastcheck", 60.0, stages_override=stages)
        self.assertNotEqual(code, 0)
        self.assertFalse(later.exists(), "命令启动报错后仍执行了后续项")

    def test_precheck_failure_skips_later_compile_phases(self) -> None:
        later = self.workspace / "compile-later"
        marker = [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)]
        plan = [
            gate_runtime.Stage("production-build", [sys.executable, "-c", "import sys; sys.exit(9)"]),
            gate_runtime.Stage("test-target-check", marker),
            gate_runtime.Stage("clippy", marker),
        ]
        with contextlib.redirect_stdout(io.StringIO()):
            code = test_gate.execute_gate("fastcheck", 60.0, stages_override=plan)
        self.assertEqual(code, 1)
        self.assertFalse(later.exists(), "前置报错后仍执行了测试编译或 Clippy")

    def test_binding_loop_error_stops_before_next_phase(self) -> None:
        later = self.workspace / "warning-later"
        marker = [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)]
        plan = [
            gate_runtime.Stage("production-build", [sys.executable, "-c", "print('warning: binding loop')"]),
            gate_runtime.Stage("test-target-check", marker),
        ]
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = test_gate.execute_gate("fastcheck", 60.0, stages_override=plan)
        self.assertEqual(code, 1)
        self.assertFalse(later.exists())
        self.assertIn("binding loop", output.getvalue())

    def test_artifact_error_skips_other_builds_and_packaging(self) -> None:
        later = self.workspace / "artifact-later"
        marker = [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)]
        native = self.workspace / ".tmp" / "bazel-gate"
        native.mkdir(parents=True)
        spec = self.workspace / _label("root-test-build").split(":")[0].removeprefix("//") / "spec.json"
        spec.parent.mkdir(parents=True, exist_ok=True)
        spec.write_text(
            json.dumps(
                {
                    "name": "root-test-build",
                    "kind": "compile",
                    "root": str(self.workspace),
                    "argv": [sys.executable, "-c", "print('no test artifact')"],
                    "environment": dict(os.environ),
                    "watch_test_failures": False,
                }
            ),
            encoding="utf-8",
        )
        spec.with_suffix(".runtime.json").write_text(
            json.dumps(
                {
                    "environment": {},
                    "log": str(native / "artifact.log"),
                    "result_file": str(native / "artifact.test-results"),
                    "TEMP": os.environ.get("TEMP"),
                    "TMP": os.environ.get("TMP"),
                }
            ),
            encoding="utf-8",
        )
        plan = [
            gate_runtime.Stage(
                "root-test-build",
                [sys.executable, str(test_gate.ROOT / "scripts/bazel_action.py"), str(spec)],
            ),
            gate_runtime.Stage("snap-ocr-core-test-build", marker),
            gate_runtime.Stage("package", marker),
        ]
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = test_gate.execute_gate("fulltest", 60.0, stages_override=plan)
        self.assertEqual(code, 1)
        self.assertFalse(later.exists())
        self.assertIn("root-test-build-artifacts", output.getvalue())

    def test_static_checker_stops_at_first_failed_check(self) -> None:
        scripts = self.workspace / "scripts"
        scripts.mkdir()
        checker = scripts / "static_check.py"
        shutil.copyfile(test_gate.ROOT / "scripts/static_check.py", checker)
        result = subprocess.run(
            [sys.executable, str(checker)], capture_output=True, text=True, encoding="utf-8", check=False
        )
        self.assertNotEqual(result.returncode, 0)
        report = json.loads((self.workspace / ".tmp/static-check.json").read_text(encoding="utf-8"))
        self.assertEqual([row["name"] for row in report["checks"]], ["manifests"])
        self.assertIn("FAIL manifests", result.stdout)

    def test_review_remediation_native_failure_records_stop_runner_and_pending_stages(self) -> None:
        # P3-41：Rust 1.98.1 原生结果文件含普通失败、带消息失败及跨行消息。
        artifact = self.workspace / "artifact.json"
        artifact.write_text(
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "profile": {"test": True},
                    "target": {"name": "synthetic", "kind": ["test"]},
                    "executable": sys.executable,
                }
            ),
            encoding="utf-8",
        )
        variants = (
            ("ordinary", "failed first_failure\n", "test first_failure ... FAILED"),
            (
                "message",
                "failed: test did not panic as expected first_failure\n",
                "test did not panic as expected",
            ),
            (
                "multiline",
                "failed: panic did not contain expected string\n"
                f'      panic message: "{"x" * 16_384}"\n'
                ' expected substring: "expected" first_failure\n',
                "panic did not contain expected string",
            ),
            (
                "long-message",
                f"failed: native failure {'x' * 16_384}\n",
                "native failure",
            ),
        )
        for level in ("fulltest", "slowtest"):
            for variant, record, summary in variants:
                with (
                    self.subTest(level=level, variant=variant),
                    tempfile.TemporaryDirectory(dir=self.workspace) as temporary,
                ):
                    root = Path(temporary)
                    ready, later = root / "peer-ready", root / "pending-ran"
                    peer_code = (
                        "import os,pathlib,sys,threading; "
                        "pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); threading.Event().wait()"
                    )
                    runner_code = (
                        "import pathlib,subprocess,sys,threading,time; ready=pathlib.Path(sys.argv[1]); "
                        f"subprocess.Popen([sys.executable,'-c',{peer_code!r},str(ready)]); "
                        "\nwhile not ready.exists(): time.sleep(0.01)"
                        "\npathlib.Path(sys.argv[-1]).write_text(sys.argv[2],encoding='utf-8'); "
                        "print('runner stays alive after native result',flush=True); threading.Event().wait()"
                    )
                    nested = local_gate_plan._test_binary_stages(
                        gate_runtime.Result("root-test-build", "PASS", "synthetic artifact", log=artifact)
                    )[0]
                    nested.argv = [sys.executable, "-c", runner_code, str(ready), record]
                    nested.exclusive_resource = "native-failure-chain"
                    stages = [
                        nested,
                        gate_runtime.Stage(
                            "must-not-run",
                            [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)],
                            exclusive_resource="native-failure-chain",
                        ),
                    ]
                    processes: list[subprocess.Popen[bytes]] = []
                    start = test_gate._start_owned_command

                    def record_start(argv, **kwargs):
                        proc = start(argv, **kwargs)
                        processes.append(proc)
                        return proc

                    output = io.StringIO()
                    with (
                        patch.object(test_gate, "LOG_DIR", root / "logs"),
                        patch.object(test_gate, "_start_owned_command", record_start),
                        contextlib.redirect_stdout(output),
                    ):
                        code = test_gate.execute_gate(level, 3.0, stages_override=stages)
                    self.assertEqual(code, 1)
                    self.assertFalse(later.exists(), "原生失败后仍启动了待执行阶段")
                    self.assertTrue(ready.exists(), "并行所属进程未进入运行状态")
                    self._assert_owned_child_exited(int(ready.read_text()))
                    runner = next(proc for proc in processes if runner_code in proc.args)
                    self.assertIsNotNone(runner.poll(), "原生失败后仍遗留所属进程")
                    self.assertNotEqual(runner.returncode, 0, "保活进程应由门终止")
                    text = output.getvalue()
                    self.assertIn(f"{level} FAIL total=", text)
                    self.assertNotIn("TIMEOUT", text)
                    self.assertIn(summary, text, "门报告必须保留真实原生失败摘要")
                    self.assertLess(len(text), 8_192, "跨行消息不得无限膨胀门摘要")
                    if variant != "ordinary":
                        self.assertNotIn(
                            f"test {record.removeprefix('failed: ').strip()} ... FAILED",
                            text,
                            "冒号后的失败消息不是测试名",
                        )
                        self.assertNotIn(f"test {summary} ... FAILED", text)

    def test_review_remediation_stdout_failure_text_does_not_stop_successful_runner(self) -> None:
        # P3-41：stdout 的普通/冒号/跨行假失败不属于原生结果协议。
        later = self.workspace / "successful-runner-later"
        stage = gate_runtime.Stage(
            "expected-panic-fixture",
            [
                sys.executable,
                "-c",
                "import pathlib,sys; print('test expected_panic ... FAILED'); "
                "print('failed first_failure'); print('failed: test did not panic as expected first_failure'); "
                "print('failed: panic did not contain expected string\\n"
                '      panic message: "actual"\\n expected substring: "expected" first_failure\'); '
                "pathlib.Path(sys.argv[-1]).write_text('ok expected_panic\\n')",
            ],
            exclusive_resource="native-success-chain",
            watch_test_failures=True,
        )
        pending = gate_runtime.Stage(
            "successful-runner-next",
            [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)],
            exclusive_resource="native-success-chain",
        )
        with contextlib.redirect_stdout(io.StringIO()):
            code = test_gate.execute_gate("fulltest", 60.0, stages_override=[stage, pending])
        self.assertEqual(code, 0, "测试自行输出的失败文字被误当成 harness 失败")
        self.assertTrue(later.exists(), "成功的原生结果不应阻止后续阶段")

    def test_native_failure_partial_record_waits_for_newline_or_runner_exit(self) -> None:
        for prefix in ("failed ", "failed: "):
            for exited in (False, True):
                with self.subTest(prefix=prefix, exited=exited):
                    log = self.workspace / "partial-native.log"
                    native = log.with_suffix(".test-results")
                    native.write_bytes(f"{prefix}first_failure".encode())
                    proc = MagicMock(spec=subprocess.Popen)
                    proc.poll.return_value = None
                    run = gate_runtime.GateRun(
                        "fulltest",
                        60.0,
                        self.workspace,
                        self.workspace,
                        start_command=MagicMock(side_effect=AssertionError("不应启动阶段")),
                        close_command=MagicMock(return_value=True),
                        kill_tree=MagicMock(return_value=True),
                        compiler_ids=MagicMock(return_value=frozenset()),
                    )
                    with io.BytesIO() as sink, contextlib.redirect_stdout(io.StringIO()):
                        running = gate_runtime._Running(
                            gate_runtime.Stage("partial-native", [], watch_test_failures=True),
                            proc,
                            sink,
                            log,
                            0.0,
                        )
                        run._observe_test_failure(running)
                        self.assertIsNone(running.test_failure, "活跃 runner 的半行不得提前判为失败")
                        self.assertEqual(running.output_offset, 0, "半行必须保留到下次完整读取")
                        self.assertFalse(run._interrupted)
                        if exited:
                            proc.poll.return_value = 1
                        else:
                            with native.open("ab") as output:
                                _ = output.write(b"\n")
                        run._observe_test_failure(running)
                    self.assertEqual(
                        running.test_failure,
                        "test first_failure ... FAILED" if prefix == "failed " else "failed: first_failure",
                    )
                    self.assertTrue(run._interrupted)
                    self.assertEqual(running.output_offset, native.stat().st_size)

    def test_python_failure_signal_stops_runner_before_teardown_finishes(self) -> None:
        for level in ("fulltest", "slowtest"):
            with self.subTest(level=level), tempfile.TemporaryDirectory(dir=self.workspace) as temporary:
                root = Path(temporary)
                later = root / "must-not-run"
                command = (
                    "import json,os,pathlib,threading; "
                    "signal=os.environ.get('JCHTOOLS_GATE_FAILURE_FILE'); "
                    "\nif signal: pathlib.Path(signal).write_text("
                    "json.dumps({'detail':'fixture failed before blocked teardown'})+'\\n',encoding='utf-8')"
                    "\nthreading.Event().wait()"
                )
                stages = [
                    gate_runtime.Stage(
                        "python-failure", [sys.executable, "-c", command], exclusive_resource="python-chain"
                    ),
                    gate_runtime.Stage(
                        "must-not-run",
                        [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)],
                        exclusive_resource="python-chain",
                    ),
                ]
                output = io.StringIO()
                with patch.object(test_gate, "LOG_DIR", root / "logs"), contextlib.redirect_stdout(output):
                    code = test_gate.execute_gate(level, 3.0, stages_override=stages)
                self.assertEqual(code, 1)
                self.assertFalse(later.exists())
                self.assertIn("fixture failed before blocked teardown", output.getvalue())
                self.assertNotIn("TIMEOUT", output.getvalue())

    def test_cli_deadline_kills_blocked_cleanup_and_its_descendants(self) -> None:
        # 覆盖 AGENTS.md 3.4：清理卡死也必须由独立监督进程在共享上限终止。
        for level in ("fulltest", "slowtest"):
            with self.subTest(level=level):
                marker = self.workspace / f"{level}-descendant"
                worker = (
                    "import pathlib,subprocess,sys,threading; "
                    "from scripts import test_gate; "
                    f"marker=pathlib.Path({str(marker)!r}); "
                    "child=subprocess.Popen([sys.executable,'-c',"
                    "'import threading; threading.Event().wait()']); "
                    # PID 仅在完整关闭写句柄后发布，exists 不能代表 write_text 已写完。
                    "pending=marker.with_suffix('.pending'); "
                    "pending.write_text(str(child.pid)); pending.replace(marker); "
                    "test_gate._close_owned_command=lambda *a,**k: threading.Event().wait(); "
                    "raise SystemExit(test_gate._gate_worker())"
                )
                outer = (
                    "import sys,threading; from unittest.mock import patch; "
                    "from scripts import test_gate; "
                    f"command=[sys.executable,'-c',{worker!r}]; "
                    "\nwith patch.object(test_gate,'_gate_worker_command',return_value=command,create=True), "
                    "patch.object(test_gate,'_close_owned_command',"
                    "lambda *a,**k: threading.Event().wait()), "
                    "patch.object(test_gate,'ServiceLease'):"
                    f"\n raise SystemExit(test_gate.main([{level!r},'--authorized','--deadline-seconds','1']))"
                )
                log = self.workspace / f"{level}-deadline.log"
                with log.open("wb") as sink:
                    started = test_gate.time.monotonic()
                    proc = test_gate._start_owned_command([sys.executable, "-c", outer], stdout=sink)
                    child = None
                    try:
                        while (
                            not marker.exists() and proc.poll() is None and (test_gate.time.monotonic() - started < 2.0)
                        ):
                            test_gate.time.sleep(0.01)
                        if marker.exists():
                            child = test_gate.win32api.OpenProcess(
                                test_gate.win32con.SYNCHRONIZE, False, int(marker.read_text())
                            )
                        try:
                            proc.wait(timeout=4.0)
                        except subprocess.TimeoutExpired:
                            self.fail("测试门清理阻塞后越过共享总预算仍未退出")
                        elapsed = test_gate.time.monotonic() - started
                        self.assertNotEqual(proc.returncode, 0)
                        self.assertLess(elapsed, 2.0, "独立监督器未在总预算到期终止")
                        self.assertIsNotNone(child, "实际阻塞进程未运行")
                        self.assertEqual(
                            test_gate.win32event.WaitForSingleObject(child, 500),
                            test_gate.win32event.WAIT_OBJECT_0,
                            "总预算到期后遗留了所属后代",
                        )
                    finally:
                        test_gate._close_owned_command(proc, timeout=2.0)
                        proc.wait(timeout=2.0)
                        if child is not None:
                            test_gate.win32api.CloseHandle(child)
                self.assertIn(f"{level} TIMEOUT total=", log.read_text(encoding="utf-8", errors="replace"))

    def test_cli_nonzero_exit_stops_before_blocked_stage_cleanup(self) -> None:
        # 覆盖 P-13：非零退出的发现不能排在可能阻塞的进程树清理之后。
        for level in ("fulltest", "slowtest"):
            with self.subTest(level=level):
                worker = (
                    "import sys,threading; from scripts import test_gate,gate_runtime; "
                    "real=test_gate.execute_gate; "
                    "stages=[gate_runtime.Stage('exit-seven',[sys.executable,'-c','import sys; sys.exit(7)'])]; "
                    "test_gate.execute_gate=lambda level,limit,**kw: real(level,limit,stages_override=stages,**kw); "
                    "test_gate._close_owned_command=lambda *a,**k: threading.Event().wait(); "
                    "raise SystemExit(test_gate._gate_worker())"
                )
                outer = (
                    "import sys; from unittest.mock import patch; from scripts import test_gate; "
                    f"command=[sys.executable,'-c',{worker!r}]; "
                    "\nwith patch.object(test_gate,'_gate_worker_command',return_value=command), "
                    "patch.object(test_gate,'ServiceLease'):"
                    f"\n raise SystemExit(test_gate.main([{level!r},'--authorized','--deadline-seconds','20']))"
                )
                log = self.workspace / f"{level}-exit-before-cleanup.log"
                with log.open("wb") as sink:
                    proc = test_gate._start_owned_command([sys.executable, "-c", outer], stdout=sink)
                    try:
                        try:
                            proc.wait(timeout=3.0)
                        except subprocess.TimeoutExpired:
                            self.fail("命令已非零退出，但门仍卡在失败阶段的清理中")
                        self.assertNotEqual(proc.returncode, 0)
                    finally:
                        test_gate._close_owned_command(proc, timeout=2.0)
                        proc.wait(timeout=2.0)
                output = log.read_text(encoding="utf-8", errors="replace")
                self.assertIn("exit=7", output)
                self.assertIn(f"{level} FAIL total=", output)
                self.assertNotIn("TIMEOUT", output)


class GateBlockedFailureOutputTests(unittest.TestCase):
    """覆盖 P-13 / AGENTS.md 3.4：首错通知与所属树终止不能等待诊断输出。"""

    def setUp(self) -> None:
        # 合成阶段不调用 Bazel；只隔离服务租约，真实 worker / Job / 共享映射不变。
        self.enterContext(patch.object(test_gate, "ServiceLease"))
        workspace = test_gate.ROOT / ".tmp" / "review-repairs" / "GateRegress"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.workspace = Path(temporary.name)

    def _assert_failure_survives_blocked_output(self, mode: str, level: str) -> None:
        root = self.workspace / f"{mode}-{level}"
        root.mkdir()
        blocked, peer_pid, pending = (root / name for name in ("output-blocked", "peer-pid", "pending-ran"))
        peer_code = (
            "import os,pathlib,sys,threading; "
            "ready=pathlib.Path(sys.argv[1]); staging=ready.with_suffix('.tmp'); "
            "staging.write_text(str(os.getpid()),encoding='ascii'); staging.replace(ready); "
            "threading.Event().wait()"
        )
        failure_code = (
            "import pathlib,sys,time; ready=pathlib.Path(sys.argv[1]); "
            "\nwhile not ready.exists(): time.sleep(0.01)"
            "\nsys.exit(7)"
        )
        # 真实 worker / 命名映射 / 监督器 / Windows Job 均保留；仅替换阶段、
        # 输出端、会计时钟和不用的 Bazel 服务租约。STOP 首次 write 由所属 Job 终止。
        worker = textwrap.dedent(
            f"""\
            import io, pathlib, sys, threading, time
            from scripts import gate_runtime, test_gate

            root = pathlib.Path({str(root)!r})
            blocked = pathlib.Path({str(blocked)!r})
            peer_pid = pathlib.Path({str(peer_pid)!r})
            pending = pathlib.Path({str(pending)!r})
            peer_argv = [sys.executable, "-c", {peer_code!r}, str(peer_pid)]
            real_execute = test_gate.execute_gate

            class BlockingOutput(io.StringIO):
                def write(self, text):
                    if text.startswith("STOP "):
                        blocked.touch()
                        threading.Event().wait()
                    return super().write(text)

            def execute(level, limit, **kwargs):
                # 不把夹具启动时间与监督器的确定性逻辑时钟混用。
                kwargs["clock_override"] = lambda: 0.0
                test_gate.LOG_DIR = root / "logs"
                if {mode!r} == "halt":
                    test_gate._start_owned_command(peer_argv)
                    stages = [
                        gate_runtime.Stage(
                            "first-error",
                            [sys.executable, "-c", {failure_code!r}, str(peer_pid)],
                            exclusive_resource="failure-chain",
                        ),
                        gate_runtime.Stage(
                            "must-not-run",
                            [sys.executable, "-c",
                             "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(pending)],
                            exclusive_resource="failure-chain",
                        ),
                    ]
                    return real_execute(level, limit, stages_override=stages, **kwargs)
                test_gate._start_owned_command(peer_argv)
                deadline = time.monotonic() + 10.0
                while not peer_pid.exists():
                    if time.monotonic() >= deadline:
                        raise TimeoutError("所属并行进程未启动")
                    time.sleep(0.01)
                run = gate_runtime.GateRun(
                    level, limit, root, root / "logs",
                    start_command=test_gate._start_owned_command,
                    close_command=test_gate._close_owned_command,
                    kill_tree=test_gate._kill_tree,
                    compiler_ids=lambda proc: frozenset(),
                    clock=kwargs["clock_override"],
                    progress=kwargs["progress"],
                )
                return run.finish([gate_runtime.Result("external-error", "FAIL", "合成外部阶段失败")])

            test_gate.execute_gate = execute
            sys.stdout = BlockingOutput()
            raise SystemExit(test_gate._gate_worker())
            """
        )
        processes: list[subprocess.Popen[bytes]] = []
        state_names: list[str] = []
        markers: list[int] = []
        peer_handles: list[int] = []
        clock = [1.0]
        real_start = test_gate._start_owned_command
        real_sleep = time.sleep

        def start(argv, **kwargs):
            proc = real_start(argv, **kwargs)
            processes.append(proc)
            state_names.append(kwargs["env"]["JCHTOOLS_GATE_PROGRESS"])
            return proc

        def sample_clock() -> float:
            if not markers:
                deadline = time.monotonic() + 10.0
                while not blocked.exists():
                    if processes[0].poll() is not None:
                        raise RuntimeError("工作进程未进入阻塞输出端便退出")
                    if time.monotonic() >= deadline:
                        raise TimeoutError("工作进程未进入阻塞输出端")
                    real_sleep(0.01)
                # write 已卡住；此处读取真正跨进程发布的状态，而非记录调用顺序。
                with test_gate._gate_state(state_names[0]) as state:
                    markers.append(struct.unpack_from("<i", state, 8)[0])
                peer_handles.append(
                    test_gate.win32api.OpenProcess(
                        test_gate.win32con.SYNCHRONIZE, False, int(peer_pid.read_text(encoding="ascii"))
                    )
                )
            return clock[0]

        def advance_to_deadline(_seconds: float) -> None:
            # 错误实现看不到首错时立即推进到硬截止，避免测试本身等待整级预算。
            # 正确实现首轮便收到 _WORKER_FAILED，不会走这个分支。
            clock[0] = 61.0

        output = io.StringIO()
        try:
            with (
                patch.object(test_gate, "_gate_worker_command", return_value=[sys.executable, "-c", worker]),
                patch.object(test_gate, "_start_owned_command", start),
                patch.object(test_gate, "_monotonic", side_effect=sample_clock),
                patch.object(test_gate.time, "sleep", side_effect=advance_to_deadline),
                contextlib.redirect_stdout(output),
            ):
                code = test_gate._supervise_gate(level, 60.0, 0.0)
            self.assertEqual(len(processes), 1, output.getvalue())
            self.assertNotEqual(processes[0].wait(timeout=5.0), 0)
            self.assertEqual(len(peer_handles), 1, "未观察到实际运行的所属并行进程")
            self.assertEqual(
                test_gate.win32event.WaitForSingleObject(peer_handles[0], 5000),
                test_gate.win32event.WAIT_OBJECT_0,
                "监督器返回后仍遗留所属并行进程",
            )
            self.assertFalse(pending.exists(), "首错后仍调度了后续阶段")
            self.assertEqual(code, 1, output.getvalue())
            self.assertEqual(markers, [test_gate._WORKER_FAILED], "首错输出阻塞时监督器仍看不到失败通知")
            self.assertIn(f"{level} FAIL total=", output.getvalue())
            self.assertNotIn("TIMEOUT", output.getvalue(), "首错被错误拖延到总预算耗尽")
        finally:
            # 即使夹具、断言或监督器异常，也通过启动时绑定的真实 Job 回收整树。
            for proc in processes:
                try:
                    test_gate._close_owned_command(proc, timeout=5.0)
                finally:
                    proc.wait(timeout=5.0)
            for handle in peer_handles:
                test_gate.win32api.CloseHandle(handle)

    def test_review_f55_halt_notifies_supervisor_before_blocked_output(self) -> None:
        for level in ("fulltest", "slowtest"):
            with self.subTest(level=level):
                self._assert_failure_survives_blocked_output("halt", level)

    def test_review_f55_external_failure_notifies_supervisor_before_blocked_output(self) -> None:
        for level in ("fulltest", "slowtest"):
            with self.subTest(level=level):
                self._assert_failure_survives_blocked_output("external", level)

    def _assert_exception_published_before_block(self, boundary: str) -> None:
        class BlockingBoundaryReached(BaseException):
            """代替不可返回的 I/O；不被门的 Exception 清理分支吞掉。"""

        name = f"Local\\JchToolsGateExceptionRegression-{os.getpid()}-{time.monotonic_ns()}"
        observed: list[tuple[str, int]] = []
        session = MagicMock()
        session.diagnostics.finish.side_effect = RuntimeError("synthetic diagnostics failure")

        with test_gate._gate_state(name) as state:
            def block_at(location: str) -> None:
                # 读取真实 worker 回调写入的映射，不能用 mock 调用顺序代替失败通知。
                observed.append((location, struct.unpack_from("<i", state, 8)[0]))
                raise BlockingBoundaryReached

            class BlockingStderr(io.StringIO):
                def write(self, text: str) -> int:
                    block_at("stderr")
                    return len(text)

            session.close.side_effect = lambda **_kwargs: block_at("cleanup")
            environment = {
                "JCHTOOLS_GATE_LEVEL": "fulltest",
                "JCHTOOLS_GATE_LIMIT": "60",
                "JCHTOOLS_GATE_STARTED": "0",
                "JCHTOOLS_GATE_PROGRESS": name,
            }
            preparation_error = RuntimeError("synthetic preparation failure") if boundary == "stderr" else None
            with (
                patch.dict(os.environ, environment),
                patch.object(test_gate, "_monotonic", return_value=0.0),
                patch.object(test_gate, "LOG_DIR", self.workspace / boundary),
                patch.object(test_gate, "_prepare_process_workspace", side_effect=preparation_error),
                patch.object(test_gate, "_gate_plan", return_value=[]),
                patch.object(test_gate, "BazelSession", return_value=session),
                patch.object(test_gate, "_execute_bazel_plan"),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(BlockingStderr()),
                self.assertRaises(BlockingBoundaryReached),
            ):
                # 保留 execute_gate、GateRun.record_results 与 worker 的真实发布链；
                # 只替换准备/诊断边界，不启动业务、Bazel 或进程。
                test_gate._gate_worker()

        self.assertEqual(
            observed,
            [(boundary, test_gate._WORKER_FAILED)],
            "进入可能永久阻塞的输出/清理前，监督器必须已经能够读取失败标记",
        )

    def test_preparation_exception_publishes_failure_before_stderr_blocks(self) -> None:
        self._assert_exception_published_before_block("stderr")

    def test_diagnostics_exception_publishes_failure_before_cleanup_blocks(self) -> None:
        self._assert_exception_published_before_block("cleanup")
