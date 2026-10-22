"""test_test_gate 按主题拆分的用例模块：命令树启动取消 / 日志独占 / 进度发布与预算守卫.

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
from scripts.bazel_gate import BazelSession, bazel_label
class _FakeMonotonic:
    """假墙钟：按预约值序列读数，之后恒定；测试内可再手动推进。."""

    def __init__(self, values: list[float]) -> None:
        self._scheduled: list[float] = list(values)
        self._current: float = values[-1] if values else 0.0

    def __call__(self) -> float:
        if self._scheduled:
            self._current = self._scheduled.pop(0)
        return self._current

    def advance(self, seconds: float) -> None:
        self._current += seconds



class OwnedCommandLaunchCancellationTests(unittest.TestCase):
    """覆盖 AGENTS.md 3.4：绑定 Job 后的启动取消不能遗留暂停进程或 Job。"""

    def test_resume_interrupt_reaps_process_and_closes_owned_job(self) -> None:
        process = MagicMock(spec=subprocess.Popen)
        process.pid = 987654321
        owner = MagicMock()
        with (
            patch.object(test_gate.subprocess, "Popen", return_value=process),
            patch.object(test_gate, "CommandJob", return_value=owner),
            patch.object(test_gate.process_runtime, "resume_owned_process", side_effect=KeyboardInterrupt),
            patch.dict(test_gate._COMMAND_JOBS, {}, clear=True),
        ):
            with self.assertRaises(KeyboardInterrupt):
                test_gate._start_owned_command(["synthetic.exe"])
            self.assertNotIn(process.pid, test_gate._COMMAND_JOBS)
        owner.close.assert_called_once_with()
        process.kill.assert_called_once_with()
        process.wait.assert_called_once_with()

    def test_job_configuration_interrupt_closes_new_handle(self) -> None:
        process = MagicMock(spec=subprocess.Popen)
        with (
            patch.object(test_gate.win32job, "CreateJobObject", return_value=123),
            patch.object(test_gate.win32job, "QueryInformationJobObject", side_effect=KeyboardInterrupt),
            patch.object(test_gate.win32api, "CloseHandle") as close,
        ):
            with self.assertRaises(KeyboardInterrupt):
                test_gate.CommandJob(process)
        close.assert_called_once_with(123)


class RunLoggedSetupBudgetTests(unittest.TestCase):
    """覆盖 P-13 / AGENTS.md 3.4：启动与 Job 绑定耗时计入绝对截止预算。."""

    def test_setup_time_is_charged_against_stage_timeout(self) -> None:
        # 假时钟：started=100.0；启动消耗 1.35s，清理读数点 101.95。
        # 阶段预算 2.0s 中保留 0.5s 给进程树清理，实际等待只剩 0.15s。
        clock = _FakeMonotonic([100.0, 101.35, 101.95])
        # 子进程无限等待，必须由真实超时清理链路终止；不能依赖 0.4s 睡眠
        # 与父进程调度的先后关系。直接核对真实 wait 的预算，使旧固定 1.5s 仍失败。
        argv = [sys.executable, "-c", "import threading; threading.Event().wait()"]
        wait_timeouts: list[float | None] = []
        waited_processes: list[subprocess.Popen[bytes]] = []
        real_wait = subprocess.Popen.wait

        def recording_wait(proc: subprocess.Popen[bytes], timeout: float | None = None) -> int:
            if not waited_processes:
                waited_processes.append(proc)
            wait_timeouts.append(timeout)
            return real_wait(proc, timeout=timeout)

        try:
            with (
                patch.object(test_gate, "_monotonic", clock),
                patch.object(subprocess.Popen, "wait", recording_wait),
            ):
                result = test_gate.run_logged("budget-setup-regression", argv, timeout=2.0)
            assert wait_timeouts  # nosec B101: 必须经过真实进程等待，不能仅伪造超时结果。
            first_wait = wait_timeouts[0]
            assert first_wait is not None and 0.0 < first_wait <= 0.15 + 1e-9  # nosec B101: 仅容许浮点舍入误差。
            assert result.status == test_gate.STATUS_TIMED_OUT  # nosec B101: 回归断言，不用于产品输入校验。
            assert "总预算" in result.detail  # nosec B101: 回归断言，不用于产品输入校验。
            assert waited_processes[0].poll() is not None  # nosec B101: 被测超时清理必须真实终止子进程。
        finally:
            # 即使回归断言或被测清理失败，也不遗留无限等待的合成进程。
            if waited_processes and waited_processes[0].poll() is None:
                waited_processes[0].kill()
                _ = real_wait(waited_processes[0], timeout=5.0)


class GateLogIsolationTests(unittest.TestCase):
    """覆盖 AGENTS.md 3.4：重跑及不同进程不能覆盖已有阶段日志或失败证据。"""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "log-isolation-regression"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    @staticmethod
    def _reported_log(output: str) -> Path:
        return Path(next(line.removeprefix("  log=") for line in output.splitlines() if line.startswith("  log=")))

    def test_repeated_processes_preserve_stdout_and_native_results(self) -> None:
        stage_code = (
            "import pathlib,sys; token=sys.argv[1]; print(token,flush=True); "
            "pathlib.Path(sys.argv[-1]).write_text('ok '+token+'\\n',encoding='utf-8')"
        )
        bootstrap = (
            "import os,pathlib,sys; from scripts import test_gate,gate_runtime; "
            "test_gate.LOG_DIR=pathlib.Path(sys.argv[1]); token='owner-'+str(os.getpid()); "
            f"stage=gate_runtime.Stage('same-stage',[sys.executable,'-c',{stage_code!r},token],"
            "watch_test_failures=True); print('TOKEN='+token,flush=True); "
            "raise SystemExit(test_gate.execute_gate(sys.argv[2],60.0,stages_override=[stage]))"
        )
        for level in ("fulltest", "slowtest"):
            with self.subTest(level=level):
                previous = None
                for index in range(2):
                    output_path = self.root / f"{level}-owner-{index}.log"
                    with output_path.open("xb") as sink:
                        proc = test_gate._start_owned_command(
                            [sys.executable, "-c", bootstrap, str(self.root / "logs"), level], stdout=sink
                        )
                        try:
                            code = proc.wait(timeout=10.0)
                        finally:
                            test_gate._close_owned_command(proc, timeout=2.0)
                            proc.wait(timeout=2.0)
                    self.assertEqual(code, 0, output_path.read_text(encoding="utf-8", errors="replace"))
                    output = output_path.read_text(encoding="utf-8")
                    token = next(
                        line.removeprefix("TOKEN=") for line in output.splitlines() if line.startswith("TOKEN=")
                    )
                    log = self._reported_log(output)
                    native = log.with_suffix(".test-results")
                    self.assertEqual(log.read_text(encoding="utf-8").strip(), token)
                    self.assertEqual(native.read_text(encoding="utf-8"), f"ok {token}\n")
                    if previous is not None:
                        old_log, old_stdout, old_native = previous
                        self.assertEqual(old_log.read_bytes(), old_stdout, "重跑覆盖了上一进程的 stdout 日志")
                        self.assertEqual(
                            old_log.with_suffix(".test-results").read_bytes(),
                            old_native,
                            "重跑覆盖了上一进程的原生结果",
                        )
                        self.assertNotEqual(old_log, log, "不同门进程复用了同一日志文件")
                    previous = (log, log.read_bytes(), native.read_bytes())

    def test_same_process_retry_retains_first_failure_evidence(self) -> None:
        first_output = io.StringIO()
        second_output = io.StringIO()
        failure_code = (
            "import json,os,pathlib,sys; print('first-failure',flush=True); "
            "pathlib.Path(sys.argv[-1]).write_text('failed first-failure\\n',encoding='utf-8'); "
            "pathlib.Path(os.environ['JCHTOOLS_GATE_FAILURE_FILE']).write_text("
            "json.dumps({'detail':'first-failure'})+'\\n',encoding='utf-8')"
        )
        success_code = (
            "import pathlib,sys; print('second-success',flush=True); "
            "pathlib.Path(sys.argv[-1]).write_text('ok second-success\\n',encoding='utf-8')"
        )
        with patch.object(test_gate, "LOG_DIR", self.root / "logs"):
            with contextlib.redirect_stdout(first_output):
                first_code = test_gate.execute_gate(
                    "fulltest",
                    60.0,
                    stages_override=[
                        gate_runtime.Stage("same-stage", [sys.executable, "-c", failure_code], watch_test_failures=True)
                    ],
                )
            self.assertEqual(first_code, 1)
            first_log = self._reported_log(first_output.getvalue())
            evidence = {
                path: path.read_bytes()
                for path in (first_log, first_log.with_suffix(".test-results"), first_log.with_suffix(".failure.jsonl"))
            }
            with contextlib.redirect_stdout(second_output):
                second_code = test_gate.execute_gate(
                    "fulltest",
                    60.0,
                    stages_override=[
                        gate_runtime.Stage("same-stage", [sys.executable, "-c", success_code], watch_test_failures=True)
                    ],
                )
        self.assertEqual(second_code, 0, "重跑读到了前一次门的失败通知")
        second_log = self._reported_log(second_output.getvalue())
        for path, original in evidence.items():
            self.assertTrue(path.is_file(), f"重跑删除了首错证据：{path}")
            self.assertEqual(path.read_bytes(), original, f"重跑改写了首错证据：{path}")
        self.assertNotEqual(first_log, second_log)

    def test_standalone_command_retry_preserves_previous_log(self) -> None:
        with patch.object(test_gate, "LOG_DIR", self.root / "logs"):
            first = test_gate.run_logged("same-command", [sys.executable, "-c", "print('first-command')"], timeout=10.0)
            self.assertEqual(first.status, test_gate.STATUS_OK)
            first_log = first.log
            self.assertIsNotNone(first_log)
            original = first_log.read_bytes()
            second = test_gate.run_logged(
                "same-command", [sys.executable, "-c", "print('second-command')"], timeout=10.0
            )
        self.assertEqual(second.status, test_gate.STATUS_OK)
        self.assertEqual(first_log.read_bytes(), original, "独立命令重跑覆盖了上一命令日志")
        self.assertNotEqual(first.log, second.log)


class GateProgressPublicationTests(unittest.TestCase):
    """覆盖 P-13：共享编译扣时必须完整发布，且只能与读取后的墙钟比较。."""

    def setUp(self) -> None:
        # 仅验证共享进度与监督时钟，替身 worker 不应争用外层门的 Bazel 租约。
        self.enterContext(patch.object(test_gate, "ServiceLease"))

    def test_worker_publication_never_exposes_intermediate_zero_to_supervisor(self) -> None:
        first_published = threading.Event()
        publish_next = threading.Event()
        publication_boundary = threading.Event()
        finish_publication = threading.Event()
        worker_codes: list[int] = []
        worker_errors: list[Exception] = []
        worker: threading.Thread | None = None
        process = MagicMock(spec=subprocess.Popen)
        pack_into = struct.pack_into

        def interrupted_pack(fmt: str, state: mmap.mmap, offset: int, value: float | int) -> None:
            if fmt == "<d" and offset == 0 and value == 2.0:
                # 确定性重现 CPython pack_into 先清零、再写值时被读方抢占。
                # 原子发布不能经过这个可见中间态；不靠重复竞争碰运气。
                pack_into(fmt, state, offset, 0.0)
                publication_boundary.set()
                if not finish_publication.wait(5.0):
                    raise TimeoutError("监督器未释放编译扣时发布夹具")
            pack_into(fmt, state, offset, value)

        def execute(_level: str, _limit: float, **kwargs: object) -> int:
            progress = cast("Callable[[float, str], None]", kwargs["progress"])
            progress(1.0, "")
            first_published.set()
            if not publish_next.wait(5.0):
                raise TimeoutError("监督器未读取首个编译扣时")
            progress(2.0, "")
            return 0

        def start(_argv: list[str], *, stdout: object, env: dict[str, str]) -> subprocess.Popen[bytes]:
            nonlocal worker
            _ = stdout

            def run_worker() -> None:
                try:
                    with patch.dict(os.environ, env):
                        worker_codes.append(test_gate._gate_worker())
                except Exception as error:  # noqa: BLE001 - 在线程所属测试中保留原始错误。
                    worker_errors.append(error)
                finally:
                    first_published.set()
                    publication_boundary.set()

            worker = threading.Thread(target=run_worker, daemon=True)
            worker.start()
            self.assertTrue(first_published.wait(5.0), "工作线程未发布初始扣时")
            return cast("subprocess.Popen[bytes]", process)

        def pause(_seconds: float) -> None:
            publish_next.set()
            self.assertTrue(publication_boundary.wait(5.0), "工作线程未到达第二次发布边界")

        def abort(_proc: subprocess.Popen[bytes], *, timeout: float) -> None:
            finish_publication.set()
            publish_next.set()
            if worker is not None:
                worker.join(5.0)

        def poll() -> int | None:
            return 0 if worker is not None and not worker.is_alive() else None

        process.poll.side_effect = poll
        output = io.StringIO()
        with (
            patch.object(test_gate, "execute_gate", execute),
            patch.object(test_gate, "_start_owned_command", start),
            patch.object(test_gate, "_abort_gate_worker", side_effect=abort) as terminate,
            patch.object(test_gate, "_monotonic", return_value=3.0),
            patch.object(test_gate.time, "sleep", pause),
            patch.object(test_gate.struct, "pack_into", interrupted_pack),
            contextlib.redirect_stdout(output),
        ):
            code = test_gate._supervise_gate("fulltest", 5.0, 0.0)
        self.assertEqual(worker_errors, [])
        self.assertEqual(worker_codes, [0])
        self.assertIsNotNone(worker)
        self.assertFalse(cast("threading.Thread", worker).is_alive())
        terminate.assert_called_once_with(process, timeout=4.0)
        self.assertEqual(code, 0, output.getvalue())
        self.assertNotIn("STOP", output.getvalue())

    def test_progress_published_after_clock_sample_is_not_compared_to_stale_total(self) -> None:
        name = f"Local\\JchToolsGateClockRegression-{os.getpid()}-{time.monotonic_ns()}"
        process = MagicMock(spec=subprocess.Popen)
        process.poll.return_value = 0
        clock = _FakeMonotonic([1.0, 3.0])
        sampled = False
        output = io.StringIO()
        with test_gate._gate_state(name) as state:
            struct.pack_into("<d", state, 0, 1.0)

            def preempted_clock() -> float:
                nonlocal sampled
                value = clock()
                if not sampled:
                    sampled = True
                    # 时钟已采到 1s 后抢占至 3s，写方合法发布 2s 编译扣时。
                    struct.pack_into("<d", state, 0, 2.0)
                    struct.pack_into("<i", state, 8, test_gate.WORKER_OK)
                return value

            with (
                patch.object(test_gate, "_gate_state", return_value=contextlib.nullcontext(state)),
                patch.object(test_gate, "_start_owned_command", return_value=process),
                patch.object(test_gate, "_abort_gate_worker") as terminate,
                patch.object(test_gate, "_monotonic", preempted_clock),
                contextlib.redirect_stdout(output),
            ):
                code = test_gate._supervise_gate("fulltest", 5.0, 0.0)
        terminate.assert_called_once_with(process, timeout=3.0)
        self.assertEqual(code, 0, output.getvalue())
        self.assertNotIn("STOP", output.getvalue())

    def _supervise_invalid_or_expired_progress(
        self, values: list[float], clock: list[float]
    ) -> tuple[int, str, MagicMock, MagicMock]:

        name = f"Local\\JchToolsGateGuardRegression-{os.getpid()}-{time.monotonic_ns()}"
        process = MagicMock(spec=subprocess.Popen)
        process.poll.return_value = None
        remaining = iter(values[1:])
        output = io.StringIO()
        with test_gate._gate_state(name) as state:
            struct.pack_into("<d", state, 0, values[0])

            def publish_next(_seconds: float) -> None:
                struct.pack_into("<d", state, 0, next(remaining))

            with (
                patch.object(test_gate, "_gate_state", return_value=contextlib.nullcontext(state)),
                patch.object(test_gate, "_start_owned_command", return_value=process),
                patch.object(test_gate, "_abort_gate_worker") as terminate,
                patch.object(test_gate, "_monotonic", _FakeMonotonic(clock)),
                patch.object(test_gate.time, "sleep", side_effect=publish_next) as sleep,
                contextlib.redirect_stdout(output),
            ):
                code = test_gate._supervise_gate("fulltest", 5.0, 0.0)
        terminate.assert_called_once()
        self.assertEqual(terminate.call_args.args, (process,))
        return code, output.getvalue(), sleep, terminate

    def test_invalid_progress_still_fails_closed_and_aborts_worker(self) -> None:
        for values in ([float("nan")], [float("inf")], [-1.0], [4.0], [1.0, 0.0]):
            with self.subTest(values=values):
                code, output, _, terminate = self._supervise_invalid_or_expired_progress(values, [3.0])
                self.assertGreaterEqual(terminate.call_args.kwargs["timeout"], 0.0)
                self.assertEqual(code, 1)
                self.assertIn("工作进程上报了无效的编译扣时", output)
                self.assertIn("fulltest FAIL total=", output)
                self.assertNotIn("TIMEOUT", output)

    def test_compile_exclusion_does_not_extend_non_compile_hard_deadline(self) -> None:
        code, output, sleep, terminate = self._supervise_invalid_or_expired_progress([2.0, 2.0], [6.5, 7.0])
        self.assertEqual(terminate.call_args.kwargs["timeout"], 0.0)
        self.assertEqual(code, 1)
        sleep.assert_called_once()
        self.assertIn("fulltest TIMEOUT total=7.0s", output)
        self.assertIn("compile_excluded=2.0s budgeted=5.0s limit=5.0s", output)
        self.assertNotIn("STOP", output)


class FastcheckWallClockGuardTests(unittest.TestCase):
    """非编译工作的终局守卫；最终汇总同样计入共享预算。."""

    def _run_fastcheck(self, clock: _FakeMonotonic, last_stage_end: float) -> int:
        def fake_start(
            argv: list[str],
            *,
            stdout: object = subprocess.DEVNULL,
            env: dict[str, str] | None = None,
        ) -> subprocess.Popen[bytes]:
            _ = argv, stdout, env
            clock.advance(last_stage_end)
            proc = MagicMock(spec=subprocess.Popen)
            proc.pid = 1
            proc.returncode = 0
            proc.args = argv
            cast("MagicMock", proc.poll).return_value = 0
            cast("MagicMock", proc.wait).return_value = 0
            return cast("subprocess.Popen[bytes]", proc)

        with (
            patch.object(test_gate, "_start_owned_command", fake_start),
            patch.object(test_gate, "_close_owned_command", return_value=True),
            patch.object(test_gate, "_kill_tree", return_value=True),
        ):
            return test_gate.execute_gate(
                "fastcheck",
                60.0,
                stages_override=[gate_runtime.Stage("cached-build", ["isolated-clock-stub"])],
                compiler_ids_override=lambda _proc: frozenset(),
                clock_override=clock,
            )

    def test_total_wall_clock_over_budget_fails_even_when_all_stages_ok(self) -> None:
        clock = _FakeMonotonic([0.0])
        exit_code = self._run_fastcheck(clock, last_stage_end=61.0)
        assert exit_code != 0  # nosec B101: 总墙钟 61s > 60s 硬上限时不得报成功。

    def test_fractional_overrun_cannot_use_accounting_tolerance(self) -> None:
        clock = _FakeMonotonic([0.0])
        exit_code = self._run_fastcheck(clock, last_stage_end=60.25)
        assert exit_code != 0  # nosec B101: 60 秒是硬上限，不得给亚秒超限成功豁免。

    def test_total_wall_clock_within_budget_still_passes(self) -> None:
        # 正向对照：启动与最终汇总共享同一时钟，预算内的阶段仍通过。
        clock = _FakeMonotonic([0.0])
        exit_code = self._run_fastcheck(clock, last_stage_end=59.5)
        assert exit_code == 0  # nosec B101: 各阶段与总墙钟都在预算内时必须仍是 0。


class GateCleanupCancellationTests(unittest.TestCase):
    """覆盖 AGENTS.md 3.4：清理阶段取消也必须输出统一计时证据。"""

    def test_cleanup_interrupt_preserves_failed_timing_summary(self) -> None:
        run = gate_runtime.GateRun(
            "fastcheck",
            60.0,
            test_gate.ROOT,
            test_gate.LOG_DIR,
            start_command=MagicMock(side_effect=AssertionError("不应启动阶段")),
            close_command=MagicMock(return_value=True),
            kill_tree=MagicMock(return_value=True),
            compiler_ids=MagicMock(return_value=frozenset()),
        )
        temporary = MagicMock()
        temporary.name = "synthetic cleanup directory; never created"
        temporary.cleanup.side_effect = KeyboardInterrupt
        run._temporary = temporary
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = run.finish([])
        self.assertEqual(code, 1)
        self.assertIn("temporary-cleanup", output.getvalue())
        self.assertIn("fastcheck FAIL total=", output.getvalue())
        for field in ("compile_excluded=", "budgeted=", "limit="):
            self.assertIn(field, output.getvalue())
