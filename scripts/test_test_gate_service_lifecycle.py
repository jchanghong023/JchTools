"""test_test_gate 按主题拆分的用例模块：持久 Bazel 服务生命周期 / server PID 与不可变发布.

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


class PublishedTextConcurrencyTests(unittest.TestCase):
    """并发读者不得因句柄双重关闭误伤复用句柄值（错误 6）。"""

    def test_concurrent_published_reads_never_hit_invalid_handle(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            files = []
            for index in range(8):
                path = Path(temporary) / f"published-{index}.txt"
                _ = path.write_text("payload " * 512, encoding="utf-8")
                files.append(path)

            def hammer(worker: int) -> int:
                failures = 0
                for index in range(60):
                    try:
                        gate_runtime.read_published_text(files[(worker + index) % len(files)])
                    except OSError:
                        failures += 1
                return failures

            with ThreadPoolExecutor(max_workers=8) as pool:
                self.assertEqual(sum(pool.map(hammer, range(8))), 0)


class PersistentBazelLifecycleTests(unittest.TestCase):
    """P-12：失败、取消及不兼容租约只能结束本轮任务，不能终止持久服务。"""

    def test_release_publication_error_does_not_kill_service(self) -> None:
        lease = object.__new__(bazel_service.ServiceLease)
        lease.job, lease.process = 101, 102
        lease.lease = Path("synthetic.lease.json")
        lease.directory = Path("synthetic-service")
        lease.lock = MagicMock()
        with (
            patch.object(bazel_service, "_publish", side_effect=OSError("synthetic publication failure")),
            patch.object(bazel_service, "terminate_job") as terminate,
            patch.object(bazel_service, "terminate_requests", return_value=True, create=True),
            patch.object(bazel_service.win32api, "CloseHandle"),
        ):
            with self.assertRaises(OSError):
                lease.close()
        terminate.assert_not_called()
        self.assertIsNone(lease.job)
        self.assertIsNone(lease.process)

    def test_incompatible_live_service_is_rejected_without_termination(self) -> None:
        root = test_gate.ROOT
        identity = {
            "pid": 123,
            "job": "synthetic-service",
            "root": str(root),
            "version": "old",
            "created": "synthetic-created",
        }
        lease = object.__new__(bazel_service.ServiceLease)
        lease.job = lease.process = None
        lease.directory = root / ".tmp/synthetic-service"
        with (
            patch.object(bazel_service, "_alive", return_value=True),
            patch.object(bazel_service, "_process_times", return_value={"CreationTime": "synthetic-created"}),
            patch.object(bazel_service, "_open_job", return_value=101),
            patch.object(bazel_service, "_verify_job_identity"),
            patch.object(bazel_service, "_version", return_value="new"),
            patch.object(bazel_service, "_verify_native"),
            patch.object(bazel_service, "terminate_job") as terminate,
            patch.object(bazel_service.win32api, "CloseHandle"),
        ):
            with self.assertRaisesRegex(RuntimeError, "不兼容|不匹配|维护"):
                lease._reuse_owned_process(root, cast("bazel_service._Identity", identity), 102, maintenance=False)
        terminate.assert_not_called()
        self.assertIsNone(lease.job)

    def test_kill_tree_never_targets_persistent_service_job(self) -> None:
        process = MagicMock(spec=subprocess.Popen)
        process.pid = 123
        owner = MagicMock()
        owner.process, owner.job, owner.service_job = process, 101, 201
        owner.terminate.return_value = True
        with (
            patch.object(test_gate.sys, "platform", "win32"),
            patch.dict(test_gate._COMMAND_JOBS, {123: owner}, clear=True),
            patch.object(test_gate.win32job, "TerminateJobObject") as terminate,
        ):
            self.assertTrue(test_gate._kill_tree(process, timeout=0.1))
        terminate.assert_not_called()
        self.assertTrue(owner.terminate.called)
        self.assertFalse(any(call.kwargs.get("service") for call in owner.terminate.call_args_list))

    def test_request_cleanup_terminates_only_verified_nonpersistent_members(self) -> None:
        with (
            patch.object(bazel_service, "_process_id", return_value=100),
            patch.object(bazel_service, "_verify_native", return_value=(200, "server-created")),
            patch.object(bazel_service, "job_process_ids", side_effect=[(100, 200, 300), (100, 200)]),
            patch.object(bazel_service.win32api, "OpenProcess", return_value=333),
            patch.object(bazel_service, "_belongs", return_value=True),
            patch.object(bazel_service, "_alive", side_effect=[True, False]),
            patch.object(bazel_service.win32api, "TerminateProcess") as terminate,
            patch.object(bazel_service.win32api, "CloseHandle") as close,
            patch.object(bazel_service, "terminate_job") as terminate_job,
            patch.object(bazel_service.time, "sleep"),
        ):
            self.assertTrue(bazel_service.terminate_requests(Path("synthetic"), 101, 102, timeout=1.0))
        terminate.assert_called_once_with(333, 1)
        close.assert_called_once_with(333)
        terminate_job.assert_not_called()

    def test_concurrent_termination_access_denied_requires_signaled_process_handle(self) -> None:
        denied = bazel_service.pywintypes.error(
            bazel_service.winerror.ERROR_ACCESS_DENIED, "TerminateProcess", "concurrent termination"
        )
        for signaled in (True, False):
            with self.subTest(process_signaled=signaled):
                waits = [
                    bazel_service.win32event.WAIT_TIMEOUT,
                    bazel_service.win32event.WAIT_OBJECT_0 if signaled else bazel_service.win32event.WAIT_TIMEOUT,
                ]
                with (
                    patch.object(bazel_service, "_process_id", return_value=100),
                    patch.object(bazel_service, "_verify_native", return_value=(200, "server-created")),
                    patch.object(bazel_service, "job_process_ids", side_effect=[(100, 200, 300), (100, 200)]),
                    patch.object(bazel_service.win32api, "OpenProcess", return_value=333) as open_process,
                    patch.object(bazel_service, "_belongs", return_value=True),
                    patch.object(bazel_service.win32event, "WaitForSingleObject", side_effect=waits) as wait,
                    patch.object(bazel_service.win32api, "TerminateProcess", side_effect=denied) as terminate,
                    patch.object(bazel_service.win32api, "CloseHandle") as close,
                    patch.object(bazel_service, "terminate_job") as terminate_job,
                    patch.object(bazel_service.time, "sleep"),
                ):
                    self.assertEqual(
                        bazel_service.terminate_requests(Path("synthetic"), 101, 102, timeout=1.0),
                        signaled,
                        "Job 列表归零不能代替持有句柄的真实终态确认",
                    )
                open_process.assert_called_once_with(
                    bazel_service.win32con.SYNCHRONIZE
                    | bazel_service.win32con.PROCESS_QUERY_INFORMATION
                    | bazel_service.win32con.PROCESS_TERMINATE,
                    False,
                    300,
                )
                terminate.assert_called_once_with(333, 1)
                self.assertEqual([call.args for call in wait.call_args_list], [(333, 0), (333, 0)])
                close.assert_called_once_with(333)
                terminate_job.assert_not_called()

    def test_console_host_residue_is_reaped_but_unknown_members_stay_untouched(self) -> None:
        # 回归：客户端链派生的 conhost 会滞留在服务 Job 中；它既不是 bazel client 也
        # 不受保护，拒绝回收会让清理永远无法清空并卡死后续派发。
        handles = {200: 2200, 300: 2300}
        images = {2200: Path("C:/Windows/System32/conhost.exe"), 2300: Path("C:/tools/unknown.exe")}
        with (
            patch.object(bazel_service, "_process_id", return_value=100),
            patch.object(bazel_service, "_verify_native", return_value=None),
            patch.object(bazel_service, "job_process_ids", return_value=(100, 200, 300)),
            patch.object(
                bazel_service.win32api, "OpenProcess", side_effect=lambda _access, _inherit, pid: handles[pid]
            ),
            patch.object(bazel_service, "_belongs", return_value=True),
            patch.object(
                bazel_service.win32event, "WaitForSingleObject", return_value=bazel_service.win32event.WAIT_TIMEOUT
            ),
            patch.object(test_gate.process_runtime, "process_image", side_effect=lambda handle: images[handle]),
            patch.object(test_gate.process_runtime, "process_argv", return_value=("unknown",)),
            patch.object(bazel_service.win32api, "TerminateProcess") as terminate,
            patch.object(bazel_service.win32api, "CloseHandle") as close,
            patch.object(bazel_service, "terminate_job") as terminate_job,
        ):
            self.assertFalse(bazel_service.terminate_requests(Path("synthetic"), 101, 102, timeout=0.0))
        terminate.assert_called_once_with(2200, 1)
        self.assertEqual({call.args for call in close.call_args_list}, {(2200,), (2300,)})
        terminate_job.assert_not_called()

    def test_unknown_native_identity_fails_safe_without_killing_any_member(self) -> None:
        handles = {200: 2200, 300: 2300}
        images = {2200: Path("C:/tools/java.exe"), 2300: Path("C:/tools/unknown.exe")}
        with (
            patch.object(bazel_service, "_process_id", return_value=100),
            patch.object(bazel_service, "_verify_native", return_value=None),
            patch.object(bazel_service, "job_process_ids", return_value=(100, 200, 300)),
            patch.object(
                bazel_service.win32api, "OpenProcess", side_effect=lambda _access, _inherit, pid: handles[pid]
            ),
            patch.object(bazel_service, "_belongs", return_value=True),
            patch.object(
                bazel_service.win32event, "WaitForSingleObject", return_value=bazel_service.win32event.WAIT_TIMEOUT
            ),
            patch.object(test_gate.process_runtime, "process_image", side_effect=lambda handle: images[handle]),
            patch.object(test_gate.process_runtime, "process_argv", return_value=("unknown",)),
            patch.object(bazel_service.win32api, "TerminateProcess") as terminate,
            patch.object(bazel_service.win32api, "CloseHandle") as close,
            patch.object(bazel_service, "terminate_job") as terminate_job,
        ):
            self.assertFalse(bazel_service.terminate_requests(Path("synthetic"), 101, 102, timeout=0.0))
        terminate.assert_not_called()
        terminate_job.assert_not_called()
        self.assertEqual({call.args[0] for call in close.call_args_list}, set(handles.values()))

    def test_unpublished_server_cleanup_reaps_cmd_proxy_chain_but_protects_other_members(self) -> None:
        directory = (test_gate.ROOT / ".tmp" / "unpublished-client-fixture" / "service").resolve()
        output_base = directory.parent / "output/base"
        output_root = test_gate.ROOT / ".tmp/bazel-gate/output"
        # cmd 包装层已由持有其 Popen 的 broker 回收；Job 中仍可能保留下面两个原生后代。
        handles = {200: 2200, 300: 2300, 400: 2400, 500: 2500, 600: 2600}
        images = {
            2200: Path("C:/tools/java.exe"),
            2300: Path("C:/tools/bazelisk.exe"),
            2400: Path("C:/tools/bazel.exe"),
            2500: Path("C:/tools/unknown.exe"),
            2600: Path("C:/other/bazel.exe"),
        }
        arguments = {
            handle: (
                str(image),
                "--nobatch",
                f"--output_user_root={output_root}",
                f"--output_base={output_base if handle != 2600 else output_base.with_name('other-base')}",
                "build",
                "//:probe",
            )
            for handle, image in images.items()
        }
        for signaled in (True, False):
            with self.subTest(client_handles_signaled=signaled):
                terminated: set[int] = set()

                def terminate(handle: int, _code: int) -> None:
                    terminated.add(handle)

                def wait(handle: int, _timeout: int) -> int:
                    if handle in terminated and (signaled or handle == 2400):
                        return bazel_service.win32event.WAIT_OBJECT_0
                    return bazel_service.win32event.WAIT_TIMEOUT

                with (
                    patch.object(bazel_service, "_process_id", return_value=100),
                    patch.object(bazel_service, "_verify_native", side_effect=[None, (200, "server-created")]),
                    # 未知成员自行结束、server 发布 PID 后，只剩已证明的持久成员；
                    # 仍必须检查持有的两个 client 句柄，不能只相信这份 Job 快照。
                    patch.object(
                        bazel_service, "job_process_ids", side_effect=[(100, 200, 300, 400, 500, 600), (100, 200)]
                    ),
                    patch.object(
                        bazel_service.win32api, "OpenProcess", side_effect=lambda _access, _inherit, pid: handles[pid]
                    ),
                    patch.object(bazel_service, "_belongs", return_value=True),
                    patch.object(bazel_service.win32event, "WaitForSingleObject", side_effect=wait),
                    patch.object(test_gate.process_runtime, "process_image", side_effect=lambda handle: images[handle]),
                    patch.object(
                        test_gate.process_runtime, "process_argv", side_effect=lambda handle: arguments[handle]
                    ),
                    patch.object(bazel_service.win32api, "TerminateProcess", side_effect=terminate) as stop,
                    patch.object(bazel_service.win32api, "CloseHandle") as close,
                    patch.object(bazel_service, "terminate_job") as terminate_job,
                    patch.object(bazel_service.time, "sleep"),
                ):
                    self.assertEqual(
                        bazel_service.terminate_requests(directory, 101, 102, timeout=1.0),
                        signaled,
                        "包装层退出或 Job 列表清空均不能代替 client/proxy 句柄的终态",
                    )
                self.assertEqual(terminated, {2300, 2400}, "只允许终止本工作区 client/proxy，不得终止 Java 或未知成员")
                self.assertEqual({call.args for call in stop.call_args_list}, {(2300, 1), (2400, 1)})
                self.assertEqual({call.args[0] for call in close.call_args_list}, set(handles.values()))
                self.assertEqual(close.call_count, len(handles))
                terminate_job.assert_not_called()

    def test_cancellation_publication_error_still_cleans_only_round_tasks(self) -> None:
        owner = object.__new__(test_gate.CommandJob)
        owner.process = MagicMock(spec=subprocess.Popen)
        owner.process.args = ["synthetic.exe", "--lease-client", "synthetic-requests"]
        owner.job, owner.service_job, owner.service_process = 101, 201, 202
        with (
            patch.object(Path, "touch", side_effect=OSError("synthetic cancellation publication failure")),
            patch.object(test_gate.win32job, "TerminateJobObject") as terminate,
            patch.object(test_gate, "terminate_requests", return_value=True) as cleanup,
        ):
            with self.assertRaises(OSError):
                owner.terminate(0.0)
        terminate.assert_called_once_with(101, 1)
        cleanup.assert_called_once()
        self.assertEqual(cleanup.call_args.args[1:], (201, 202))

    def test_cleanup_failure_does_not_downgrade_hard_timeout(self) -> None:
        outcome = test_gate.GateSupervision(1.0, 0.0, status=test_gate.STATUS_TIMED_OUT, forced=True)
        service = MagicMock()
        service.close.side_effect = RuntimeError("synthetic cleanup deadline")
        with patch.object(test_gate, "_monotonic", return_value=1.0), contextlib.redirect_stdout(io.StringIO()):
            test_gate.close_supervised_service(service, outcome, success=False)
        self.assertEqual(outcome.status, test_gate.STATUS_TIMED_OUT)
        service.close.assert_called_once_with(timeout=0.0)

    def test_startup_cancellation_reaps_direct_native_client_before_server_pid_publication(self) -> None:
        prepare = MagicMock(spec=subprocess.Popen)
        prepare.poll.return_value, prepare.returncode = 0, 0
        prepare.pid = 301
        client = MagicMock(spec=subprocess.Popen)
        client.poll.return_value = None
        config = {
            "argv": ["synthetic-bazel.exe", "build", "//:probe"],
            "environment": {},
            "runtime": "synthetic/.tmp/bazel-gate/runtime.json",
        }
        with (
            patch.object(bazel_service, "read_published_text", side_effect=[json.dumps(config), "{}"]),
            patch.object(Path, "open", return_value=MagicMock()),
            patch.object(Path, "exists", return_value=False),
            patch.object(bazel_service.subprocess, "Popen", side_effect=[prepare, client]) as launch,
            patch.object(bazel_service, "_alive", side_effect=[True, True, False]),
            patch.object(bazel_service, "read_server_pid") as server_pid,
        ):
            self.assertEqual(bazel_service._dispatch_request(Path("synthetic/001.request.json"), 102), 1)
        self.assertEqual(launch.call_count, 2)
        self.assertEqual(launch.call_args_list[1].args, (config["argv"],))
        client.terminate.assert_called_once_with()
        client.wait.assert_called_once_with(timeout=1.0)
        server_pid.assert_not_called()

    def test_in_job_server_successor_is_accepted_across_warm_requests(self) -> None:
        # 回归：server 崩溃后由客户端在本 Job 内重启的接任者不得被当作身份篡改拒绝；
        # 归属验证已在 _verify_native 完成，未知/外部 server 仍会在那里失败。
        broker = bazel_service._Broker(Path("synthetic-service"), 101)
        broker.server = (111, "first-created")
        with (
            patch.object(bazel_service, "_verify_native", return_value=(222, "successor-created")),
        ):
            broker._verify_server(required=False)
        self.assertEqual(broker.server, (222, "successor-created"))

    def test_maintenance_stop_removes_stale_server_pid_record(self) -> None:
        # 回归：维护停止确认终止后残留的 server.pid.txt 会被 Windows PID 复用冒充
        # 存活服务，阻断下一轮客户端与租约校验；停止成功必须删除该记录。
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            pid_file = root / ".tmp/bazel-gate/output/base/server/server.pid.txt"
            pid_file.parent.mkdir(parents=True)
            _ = pid_file.write_text("74020", encoding="ascii")
            lease = MagicMock()
            lease.job = 101
            with (
                patch.object(bazel_service, "ServiceLease", return_value=lease),
                patch.object(bazel_service, "read_server_pid", return_value=74020),
                patch.object(bazel_service, "_open_process", return_value=777),
                patch.object(bazel_service, "_belongs", return_value=True),
                patch.object(bazel_service, "_stop_service_job") as stop_job,
                patch.object(
                    bazel_service.win32event,
                    "WaitForSingleObject",
                    return_value=bazel_service.win32event.WAIT_OBJECT_0,
                ),
                patch.object(bazel_service.win32api, "CloseHandle"),
            ):
                bazel_service.stop(root)
            stop_job.assert_called_once_with(101)
            lease.close.assert_called_once()
            self.assertFalse(pid_file.exists(), "维护停止后必须删除 server PID 记录")

    def test_broker_failure_and_exception_leave_persistent_job_alive(self) -> None:
        for error in (None, RuntimeError("synthetic dispatch failure")):
            with self.subTest(error=error):
                broker = MagicMock()
                broker.request.side_effect = [Path("001.request.json"), SystemExit(0)]
                broker.execute.return_value = 1
                broker.execute.side_effect = error
                broker.last = time.monotonic()
                with (
                    patch.object(bazel_service, "_open_job", return_value=201),
                    patch.object(bazel_service, "_Broker", return_value=broker),
                    patch.object(bazel_service, "terminate_job") as terminate,
                    patch.object(bazel_service.win32api, "CloseHandle"),
                    patch.object(bazel_service, "_publish"),
                    patch.object(bazel_service, "terminate_requests", return_value=True, create=True),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    with self.assertRaises(SystemExit):
                        bazel_service._broker(Path("synthetic-service"), "synthetic-job")
                terminate.assert_not_called()
                self.assertEqual(broker.request.call_count, 2)


class GateServerPidPublicationTests(unittest.TestCase):
    """真实 Windows 文件共享窗口不能绕过 server 的 Job 归属验证。."""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "server-pid-publication"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.enterContext(patch.object(test_gate, "ROOT", self.root))
        self.requests = self.root / "requests"
        self.requests.mkdir()
        self.pid_file = self.root / ".tmp/bazel-gate/output/base/server/server.pid.txt"
        self.pid_file.parent.mkdir(parents=True)
        start = cast("Callable[[list[str]], subprocess.Popen[bytes]]", vars(test_gate)["_start_owned_command"])
        self.process = start([sys.executable, "-c", "import time; time.sleep(60)"])
        close = cast("Callable[..., bool]", vars(test_gate)["_close_owned_command"])

        def cleanup_process() -> None:
            self.assertTrue(close(self.process, timeout=2.0) or self.process.poll() is not None)
            self.assertIsNotNone(self.process.poll())

        self.addCleanup(cleanup_process)
        self.process.args = [sys.executable, "--lease-client", str(self.requests)]
        self.observe = cast("Callable[[subprocess.Popen[bytes]], None]", vars(test_gate)["_observe_owned_command"])
        _ = (self.requests / "001.request.json").write_text("{}", encoding="utf-8")

    def test_shared_writer_and_atomic_replacement_are_readable(self) -> None:
        _ = self.pid_file.write_text(str(self.process.pid), encoding="ascii")
        handle = bazel_service.win32file.CreateFile(
            str(self.pid_file),
            bazel_service.win32con.GENERIC_WRITE,
            bazel_service.win32con.FILE_SHARE_READ
            | bazel_service.win32con.FILE_SHARE_WRITE
            | bazel_service.win32con.FILE_SHARE_DELETE,
            None,
            bazel_service.win32con.OPEN_EXISTING,
            bazel_service.win32con.FILE_ATTRIBUTE_NORMAL,
            None,
        )
        try:
            self.observe(self.process)
        finally:
            bazel_service.win32api.CloseHandle(handle)
        replacement = self.pid_file.with_suffix(".pending")
        _ = replacement.write_text(f"{self.process.pid}\n", encoding="ascii")
        _ = replacement.replace(self.pid_file)
        self.observe(self.process)
        self.assertEqual(bazel_service.read_server_pid(self.pid_file), self.process.pid)
        _ = (self.requests / "001.request.complete.json").write_text("0", encoding="ascii")
        self.observe(self.process)

    def test_exclusive_writer_defers_only_until_next_sample(self) -> None:
        handle = bazel_service.win32file.CreateFile(
            str(self.pid_file),
            bazel_service.win32con.GENERIC_WRITE,
            0,
            None,
            bazel_service.win32con.CREATE_ALWAYS,
            bazel_service.win32con.FILE_ATTRIBUTE_NORMAL,
            None,
        )
        try:
            self.observe(self.process)
        finally:
            bazel_service.win32api.CloseHandle(handle)
        _ = self.pid_file.write_text(str(self.process.pid), encoding="ascii")
        self.observe(self.process)
        _ = (self.requests / "001.request.complete.json").write_text("0", encoding="ascii")
        self.observe(self.process)

    def test_exclusive_writer_is_bounded_and_not_allowed_after_completion(self) -> None:
        handle = bazel_service.win32file.CreateFile(
            str(self.pid_file),
            bazel_service.win32con.GENERIC_WRITE,
            0,
            None,
            bazel_service.win32con.CREATE_ALWAYS,
            bazel_service.win32con.FILE_ATTRIBUTE_NORMAL,
            None,
        )
        try:
            with patch.object(test_gate.time, "monotonic", side_effect=[10.0, 12.0]):
                self.observe(self.process)
                with self.assertRaisesRegex(RuntimeError, "PID 发布超时"):
                    self.observe(self.process)
            _ = (self.requests / "001.request.complete.json").write_text("0", encoding="ascii")
            with self.assertRaisesRegex(RuntimeError, "保留所属进程身份失败"):
                self.observe(self.process)
        finally:
            bazel_service.win32api.CloseHandle(handle)

    def test_empty_publication_is_bounded_but_malformed_and_unknown_access_fail_immediately(self) -> None:
        _ = self.pid_file.write_bytes(b"")
        with patch.object(test_gate.time, "monotonic", side_effect=[10.0, 12.0]):
            self.observe(self.process)
            with self.assertRaisesRegex(RuntimeError, "PID 发布超时"):
                self.observe(self.process)
        _ = self.pid_file.write_bytes(b"\xff")
        with self.assertRaises(UnicodeDecodeError):
            self.observe(self.process)
        denied = bazel_service.pywintypes.error(
            bazel_service.winerror.ERROR_ACCESS_DENIED, "CreateFile", "controlled access denial"
        )
        with (
            patch.object(bazel_service.win32file, "CreateFile", side_effect=denied),
            self.assertRaisesRegex(RuntimeError, "保留所属进程身份失败"),
        ):
            self.observe(self.process)

    def test_foreign_live_pid_is_rejected_by_observer_and_server_preflight(self) -> None:
        _ = self.pid_file.write_text(str(os.getpid()), encoding="ascii")
        with self.assertRaisesRegex(RuntimeError, "不属于当前派发 Job"):
            self.observe(self.process)
        verify = cast("Callable[..., None]", vars(bazel_service)["_verify_native"])
        owner = cast("dict[int, object]", vars(test_gate)["_COMMAND_JOBS"])[self.process.pid]
        with self.assertRaisesRegex(RuntimeError, "不属于独占服务 Job"):
            verify(self.pid_file.parents[3] / "service", cast("int", getattr(owner, "job")), required=False)

    def test_supervisor_kills_owned_job_on_first_unproven_completion(self) -> None:
        _ = self.pid_file.write_text(str(os.getpid()), encoding="ascii")
        completion = self.requests / "001.request.complete.json"
        _ = completion.write_text("0", encoding="ascii")
        close = cast("Callable[..., bool]", vars(test_gate)["_close_owned_command"])
        kill = cast("Callable[..., bool]", vars(test_gate)["_kill_tree"])
        with contextlib.redirect_stdout(io.StringIO()):
            run = gate_runtime.GateRun(
                "fulltest",
                10.0,
                self.root,
                self.root / "logs",
                start_command=lambda _argv, **_kwargs: self.process,
                close_command=close,
                kill_tree=kill,
                compiler_ids=lambda _process: frozenset(),
                observe_tree=self.observe,
            )
            with self.assertRaises(gate_runtime.GateStoppedError):
                run.supervise_persistent(gate_runtime.Stage("pid-owner", ["owned-server"]), completion)
        self.assertIsNotNone(self.process.poll(), "监督首错必须立即杀死已拥有 Job")


class GateImmutablePublicationTests(unittest.TestCase):
    """已完整关闭并 replace 发布的通知不能被仍存活的共享删除句柄阻断。."""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "immutable-publication"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def _hold_shared_delete(self, path: Path) -> None:
        handle = bazel_service.win32file.CreateFile(
            str(path),
            bazel_service.win32con.GENERIC_WRITE | bazel_service.win32con.DELETE,
            bazel_service.win32con.FILE_SHARE_READ
            | bazel_service.win32con.FILE_SHARE_WRITE
            | bazel_service.win32con.FILE_SHARE_DELETE,
            None,
            bazel_service.win32con.OPEN_EXISTING,
            bazel_service.win32con.FILE_ATTRIBUTE_NORMAL,
            None,
        )
        self.addCleanup(lambda: bazel_service.win32api.CloseHandle(int(handle)))

    def _completion_code(self, completion: Path) -> int | None:
        read_code = cast(
            "Callable[[object, object, Path], int | None]",
            vars(gate_runtime.GateRun)["_persistent_completion_code"],
        )
        running = MagicMock()
        running.process.poll.return_value = None
        owner = MagicMock()
        code = read_code(owner, running, completion)
        owner._retain_process_identities.assert_called_once_with()
        return code

    def test_dispatch_reads_request_and_runtime_with_live_delete_handles(self) -> None:
        directory = self.root / "native" / "generated"
        directory.mkdir(parents=True)
        runtime = directory / "runtime.json"
        snapshot = json.dumps(
            {
                "consumer": {
                    "environment": {"CONSUMER_TOKEN": "published"},
                    "log": str(self.root / "consumer.log"),
                    "result_file": str(self.root / "consumer.results"),
                    "TEMP": None,
                    "TMP": None,
                }
            }
        ).encode("utf-8")
        _ = runtime.write_bytes(snapshot)
        request = self.root / "requests" / "001.request.json"
        request.parent.mkdir()
        pending = request.with_suffix(".pending")
        sidecar = self.root / bazel_label("consumer").split(":")[0].removeprefix("//") / "spec.runtime.json"
        sidecar.parent.mkdir(parents=True)
        artifact = self.root / "consumer.json"
        child = (
            "import json,pathlib,sys; "
            "live=json.loads(pathlib.Path(sys.argv[1]).read_text(encoding='utf-8')); "
            "pathlib.Path('consumer.json').write_text(json.dumps({"
            "'cwd':str(pathlib.Path.cwd()),'token':sys.argv[2],'runtime':live"
            "}),encoding='utf-8'); "
            "raise SystemExit(7)"
        )
        argv = [sys.executable, "-c", child, str(sidecar), "real-consumer"]
        failure = self.root / "first-failure.txt"
        environment = os.environ | {
            "PYTHONPATH": str(test_gate.ROOT) + os.pathsep + os.environ.get("PYTHONPATH", ""),
        }
        environment.pop("JCHTOOLS_GATE_BAZEL_SERVICE_LEASE", None)
        _ = (request.parent / "client.json").write_text(
            json.dumps({"JCHTOOLS_GATE_FAILURE_FILE": str(failure), "TEMP": str(self.root), "TMP": str(self.root)}),
            encoding="utf-8",
        )
        _ = pending.write_text(
            json.dumps({"argv": argv, "runtime": str(runtime), "environment": environment}), encoding="utf-8"
        )
        self.assertFalse(request.exists(), "pending 不应向消费者发布")
        _ = pending.replace(request)
        self._hold_shared_delete(request)
        self._hold_shared_delete(runtime)
        with self.assertRaises(PermissionError):
            _ = request.read_text(encoding="utf-8")
        dispatch = cast("Callable[[Path, int], int]", vars(bazel_service)["_dispatch_request"])
        owner = int(bazel_service.win32api.GetCurrentProcess())
        self.assertEqual(dispatch(request, owner), 7, "必须保留真实消费者非零退出码")
        self.assertEqual(
            json.loads(artifact.read_text(encoding="utf-8")),
            {
                "cwd": str(self.root.resolve()),
                "token": "real-consumer",
                "runtime": {
                    "environment": {"CONSUMER_TOKEN": "published"},
                    "log": str(self.root / "consumer.log"),
                    "result_file": str(self.root / "consumer.results"),
                    "TEMP": str(self.root),
                    "TMP": str(self.root),
                    "failure_file": str(failure),
                },
            },
        )
        self.assertEqual(gate_runtime.read_published_text(runtime).encode("utf-8"), snapshot)

    def test_real_adapter_startup_failure_notifies_without_mutating_runtime_snapshot(self) -> None:
        adapter = test_gate.ROOT / "scripts" / "bazel_action.py"
        for damaged in ("spec", "runtime"):
            with self.subTest(damaged=damaged):
                directory = self.root / damaged
                directory.mkdir()
                failure = directory / "first-failure.jsonl"
                unrelated_failure = directory / "other-gate-failure.jsonl"
                artifact = directory / "unexpected-child.txt"
                spec = directory / "spec.json"
                sidecar = spec.with_suffix(".runtime.json")
                runtime = directory / "runtime.json"
                snapshot = json.dumps(
                    {
                        "other-gate": {
                            "failure_file": str(unrelated_failure),
                        }
                    }
                ).encode("utf-8")
                _ = runtime.write_bytes(snapshot)
                _ = spec.write_text(
                    json.dumps(
                        {
                            "name": "consumer",
                            "kind": "check",
                            "root": str(directory),
                            "argv": [
                                sys.executable,
                                "-c",
                                "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('unexpected')",
                                str(artifact),
                            ],
                            "environment": dict(os.environ),
                            "watch_test_failures": False,
                        }
                    ),
                    encoding="utf-8",
                )
                _ = sidecar.write_text(
                    json.dumps(
                        {
                            "environment": {},
                            "log": str(directory / "consumer.log"),
                            "result_file": str(directory / "consumer.results"),
                            "TEMP": None,
                            "TMP": None,
                            "failure_file": str(failure),
                        }
                    ),
                    encoding="utf-8",
                )
                _ = (spec if damaged == "spec" else sidecar).write_bytes(b"{")
                sidecar_snapshot = sidecar.read_bytes()
                environment = dict(os.environ)
                environment["PYTHONIOENCODING"] = "utf-8"
                environment.pop("JCHTOOLS_GATE_FAILURE_FILE", None)
                if damaged == "runtime":
                    environment["JCHTOOLS_GATE_FAILURE_FILE"] = str(failure)
                self._hold_shared_delete(runtime)
                result = subprocess.run(
                    [sys.executable, str(adapter), str(spec)],
                    cwd=directory,
                    env=environment,
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    check=False,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("JSONDecodeError", result.stderr)
                self.assertFalse(artifact.exists(), "配置损坏不得启动真实动作")
                notifications = [json.loads(line) for line in failure.read_text(encoding="utf-8").splitlines()]
                self.assertEqual(len(notifications), 1, "启动首错必须通知本门且不能重复发布")
                self.assertIn("Bazel 动作准备失败：JSONDecodeError:", notifications[0]["detail"])
                self.assertFalse(unrelated_failure.exists(), "不能通知其它用例或门的失败路径")
                self.assertEqual(gate_runtime.read_published_text(runtime).encode("utf-8"), snapshot)
                self.assertEqual(sidecar.read_bytes(), sidecar_snapshot)

    def test_completion_reads_published_integer_with_live_delete_handle(self) -> None:
        completion = self.root / "001.request.complete.json"
        pending = completion.with_suffix(".pending")
        _ = pending.write_text("0", encoding="utf-8")
        _ = pending.replace(completion)
        self._hold_shared_delete(completion)
        with self.assertRaises(PermissionError):
            _ = completion.read_text(encoding="utf-8")
        self.assertEqual(self._completion_code(completion), 0)

    def test_completion_rejects_malformed_or_noninteger_publications(self) -> None:
        completion = self.root / "001.request.complete.json"
        for data in (b"", b"{", b"\xff", b"true", b"null", b'"0"', b"0.0"):
            with self.subTest(data=data):
                _ = completion.write_bytes(data)
                with self.assertRaises((ValueError, UnicodeError)):
                    _ = self._completion_code(completion)
        _ = completion.write_text("7", encoding="utf-8")
        self.assertEqual(self._completion_code(completion), 7, "非零退出码不能变成成功")

    def test_corrupt_request_or_runtime_never_launches_bazel(self) -> None:
        directory = self.root / "native" / "generated"
        directory.mkdir(parents=True)
        runtime = directory / "runtime.json"
        requests = self.root / "requests"
        requests.mkdir()
        sentinel = self.root / "unexpected-bazel.txt"
        environment = os.environ | {
            "PYTHONPATH": str(test_gate.ROOT) + os.pathsep + os.environ.get("PYTHONPATH", ""),
        }
        environment.pop("JCHTOOLS_GATE_BAZEL_SERVICE_LEASE", None)
        environment.pop("JCHTOOLS_GATE_FAILURE_FILE", None)
        _ = (requests / "client.json").write_text("{}", encoding="utf-8")
        prepare = cast("Callable[[str], int]", vars(bazel_gate)["_dispatch_main"])
        dispatch = cast("Callable[[Path, int], int]", vars(bazel_service)["_dispatch_request"])
        valid_request = json.dumps(
            {
                "argv": [
                    sys.executable,
                    "-c",
                    "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('unexpected')",
                    str(sentinel),
                ],
                "runtime": str(runtime),
                "environment": environment,
            }
        ).encode("utf-8")
        for number, (request_data, runtime_data) in enumerate(
            ((b"{", b"{}"), (valid_request, b"{"), (valid_request, b"\xff")), 1
        ):
            with self.subTest(request=request_data, runtime=runtime_data):
                request = requests / f"{number:03d}.request.json"
                _ = request.write_bytes(request_data)
                _ = runtime.write_bytes(runtime_data)
                with self.assertRaises((ValueError, UnicodeError)):
                    _ = prepare(str(request))
                if request_data == b"{":
                    with self.assertRaises(ValueError):
                        _ = dispatch(request, int(bazel_service.win32api.GetCurrentProcess()))
                else:
                    self.assertEqual(dispatch(request, int(bazel_service.win32api.GetCurrentProcess())), 1)
                    console = self.root / f"bazel-{number:03d}.console.log"
                    detail = console.read_text(encoding="utf-8", errors="replace")
                    self.assertIn("JSONDecodeError" if runtime_data == b"{" else "UnicodeDecodeError", detail)
                self.assertFalse(sentinel.exists(), "损坏请求或 runtime 不得执行真实消费者")

    def test_exclusive_sharing_and_unknown_acl_fail_without_retry(self) -> None:
        path = self.root / "001.request.complete.json"
        _ = path.write_text("0", encoding="utf-8")
        handle = bazel_service.win32file.CreateFile(
            str(path),
            bazel_service.win32con.GENERIC_WRITE,
            0,
            None,
            bazel_service.win32con.OPEN_EXISTING,
            bazel_service.win32con.FILE_ATTRIBUTE_NORMAL,
            None,
        )
        try:
            with self.assertRaises(bazel_service.pywintypes.error) as error:
                _ = self._completion_code(path)
            self.assertEqual(error.exception.winerror, bazel_service.winerror.ERROR_SHARING_VIOLATION)
        finally:
            bazel_service.win32api.CloseHandle(int(handle))
        denied = bazel_service.pywintypes.error(
            bazel_service.winerror.ERROR_ACCESS_DENIED, "CreateFile", "controlled ACL denial"
        )
        with patch.object(gate_runtime.win32file, "CreateFile", side_effect=denied) as opening:
            with self.assertRaises(bazel_service.pywintypes.error) as error:
                _ = self._completion_code(path)
        self.assertIs(error.exception, denied)
        opening.assert_called_once()
