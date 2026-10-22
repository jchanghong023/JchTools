"""test_test_gate 按主题拆分的用例模块：Bazel 工具身份 / 原生超时 / 外部缓存与编译观测单元.

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


class GateToolContentIdentityTests(unittest.TestCase):
    def test_python_package_mutation_invalidates_identity_with_unchanged_metadata(self) -> None:
        """覆盖 P-12：安装包版本与 RECORD 不变时，实际工具源码变化仍须使缓存失效。"""
        workspace = test_gate.ROOT / ".tmp" / "tool-content-identity"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        sysroot = Path(
            subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, check=True).stdout.strip()
        )
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            metadata = root / "synthetic_tool-1.0.dist-info"
            metadata.mkdir()
            (metadata / "METADATA").write_text("Name: synthetic-tool\nVersion: 1.0\n", encoding="utf-8")
            record = metadata / "RECORD"
            record.write_text("tool.py,,\nsynthetic_tool-1.0.dist-info/METADATA,,\n", encoding="utf-8")
            source = root / "tool.py"
            source.write_text("def result(): return 'old'\n", encoding="utf-8")
            distribution = bazel_gate.importlib.metadata.PathDistribution(metadata)
            with patch.object(bazel_gate.importlib.metadata, "distributions", return_value=[distribution]):
                before = bazel_gate.tool_identity(sysroot)
                source.write_text("def result(): return 'new'\n", encoding="utf-8")
                after = bazel_gate.tool_identity(sysroot)
            self.assertNotEqual(before, after, "工具实际源码改变却复用了相同缓存输入身份")
            self.assertEqual(record.read_text(encoding="utf-8"), "tool.py,,\nsynthetic_tool-1.0.dist-info/METADATA,,\n")


class GateNativeTimeoutTests(unittest.TestCase):
    def test_native_bep_timeout_is_not_downgraded_to_generic_failure(self) -> None:
        """覆盖 P-12：Bazel 原生超时保持 TIMEOUT，不被首错终止后的事件改成 FAIL。"""
        workspace = test_gate.ROOT / ".tmp" / "native-timeout-result"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            log = root / "native.log"
            log.write_text("native timeout child started\n", encoding="utf-8")
            result = bazel_gate.native_result(
                gate_runtime.Stage("native-timeout", [sys.executable]),
                {"log": str(log)},
                {"result": {"status": "FAILED_TO_BUILD"}, "summary": {"overallStatus": "TIMEOUT"}},
                root / "native.bep.jsonl",
                timed_out=False,
            )
            self.assertEqual(result.status, "TIMEOUT")


class GateNativeExternalCacheTests(unittest.TestCase):
    """真实持久 action cache 不得跳过每轮外部状态检查。"""

    def test_external_command_executes_each_session_while_declared_check_is_cached(self) -> None:
        # Bazel 外部仓库含长路径夹具；缩短独占工作根，不改变生产缓存位置。
        workspace = test_gate.ROOT / ".tmp"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with contextlib.ExitStack() as cleanup:
            root = Path(tempfile.mkdtemp(prefix="n", dir=workspace))
            # 独占根保留供持久 server 后续复用；不得自动 stop 或删除活动运行状态。
            round_output: dict[int, tuple[str, str]] = {}

            def preserve_failure(
                _error_type: type[BaseException] | None, error: BaseException | None, _traceback: object
            ) -> None:
                if error is None:
                    return
                # 先沿用动作 sidecar 发布首错；外层无需等待诊断复制或 fixture 停机。
                notify = cast("Callable[[str | None, str], None]", vars(bazel_action)["_notify"])
                notify(
                    os.environ.get("JCHTOOLS_GATE_FAILURE_FILE"),
                    f"Native external-cache fixture failed: {type(error).__name__}: {str(error)[:1024]}; "
                    f"fixture={root.resolve()}; logs={state.resolve()}; "
                    f"round-1={state.resolve() / 'round-1'}; round-2={state.resolve() / 'round-2'}",
                )
                destination = (workspace / "gate-perf-validation" / f"external-cache-{root.name}").resolve()
                error.add_note(f"Native external-cache fixture evidence: {destination}")
                try:
                    with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(destination)]):
                        self.assertEqual(make_tmp.main(), 0)
                    for number, (stdout, stderr) in round_output.items():
                        _ = (destination / f"round-{number}.stdout.log").write_text(stdout, encoding="utf-8")
                        _ = (destination / f"round-{number}.stderr.log").write_text(stderr, encoding="utf-8")
                    if isinstance(error, subprocess.TimeoutExpired):
                        for name, stream in (("stdout", error.stdout), ("stderr", error.stderr)):
                            if stream is not None:
                                data = stream.encode("utf-8") if isinstance(stream, str) else stream
                                _ = (destination / f"timeout.{name}.log").write_bytes(data)
                    # 只枚举本用例的诊断目录；绝不遍历 output/install/external 或产物缓存。
                    candidates = [
                        path
                        for folder in (state / "round-1", state / "round-2")
                        for path in folder.rglob("*")
                        if path.suffix in {".log", ".jsonl", ".gz", ".compact"}
                        or path.name.endswith((".complete.json", ".phase.json"))
                    ]
                    candidates.extend((state / "bazel-gate/service").glob("*.log"))
                    candidates.extend(state.glob("result-*.json"))
                    candidates.extend(state / name for name in ("stable-count", "external-count"))
                    candidates.extend(
                        state / "bazel-gate/output/base" / name
                        for name in ("java.log", "server/jvm.out", "server/server.pid.txt")
                    )
                    remaining = 64 * 1024 * 1024
                    notes: list[str] = []
                    for source in candidates:
                        if source.is_symlink() or not source.is_file():
                            continue
                        relative = source.relative_to(state)
                        try:
                            size = source.stat().st_size
                            if size > min(16 * 1024 * 1024, remaining):
                                notes.append(f"Skipped oversized diagnostic ({size} bytes): {relative}")
                                continue
                            target = destination / relative
                            target.parent.mkdir(parents=True, exist_ok=True)
                            _ = shutil.copyfile(source, target)
                            remaining -= size
                        except OSError as copy_error:
                            notes.append(f"{relative}: {copy_error}")
                    _ = (destination / "capture.log").write_text("\n".join(notes), encoding="utf-8")
                except Exception as capture_error:
                    error.add_note(f"Evidence capture failed: {type(capture_error).__name__}: {capture_error}")

            def verify_fixture_tasks() -> None:
                if not (root / ".tmp/bazel-gate/service/identity.json").exists():
                    return
                job, broker = bazel_service.open_verified_job(root)
                try:
                    pid = bazel_service.read_server_pid(root / ".tmp/bazel-gate/output/base/server/server.pid.txt")
                    self.assertIsNotNone(pid, "真实夹具必须保留可复用的原生 server")
                    process_id = cast("Callable[[int], int]", vars(test_gate)["get_process_id"])
                    expected = {process_id(int(broker)), pid}
                    deadline = time.monotonic() + 2.0
                    while True:
                        members = set(bazel_service.job_process_ids(job))
                        if members == expected or time.monotonic() >= deadline:
                            break
                        time.sleep(0.01)
                    self.assertEqual(members, expected, "夹具结束只能保留持久 broker/server，不得遗留本轮任务")
                finally:
                    bazel_service.win32api.CloseHandle(broker)
                    bazel_service.win32api.CloseHandle(job)

            cleanup.push(preserve_failure)
            cleanup.callback(verify_fixture_tasks)
            state = root / ".tmp"
            state.mkdir()
            code = (
                "from pathlib import Path; import sys; "
                "p=Path(sys.argv[1]); "
                "p.write_text(p.read_text()+'run\\n' if p.exists() else 'run\\n'); "
                "print('checked')"
            )
            stages = [
                gate_runtime.Stage("static-cache-probe", [sys.executable, "-c", code, str(state / "stable-count")]),
                gate_runtime.Stage("xberg-latest", [sys.executable, "-c", code, str(state / "external-count")]),
            ]
            # 宿主引擎覆盖会按设计禁用所有结果复用；此回归只隔离该无关变量。
            environment = {
                key: value
                for key, value in os.environ.items()
                if not key.upper().startswith("JCHTOOLS_") or key.upper().startswith("JCHTOOLS_GATE_")
            }
            consumer = textwrap.dedent("""\
                import json, sys
                from pathlib import Path
                from scripts import gate_runtime, test_gate
                from scripts.bazel_gate import BazelSession
                from scripts.bazel_service import read_server_pid
                root, round_number = Path(sys.argv[1]), sys.argv[2]
                test_gate.ROOT, test_gate.LOG_DIR = root, root / ".tmp"
                stages = [gate_runtime.Stage(**value) for value in json.loads(sys.argv[3])]
                run = gate_runtime.GateRun(
                    "fulltest", 120.0, root, root / ".tmp" / ("round-" + round_number),
                    start_command=test_gate._start_owned_command,
                    close_command=test_gate._close_owned_command,
                    kill_tree=test_gate._kill_tree,
                    compiler_ids=test_gate._compiler_ids,
                    observe_tree=test_gate._observe_owned_command,
                )
                session, results = BazelSession(run), []
                try:
                    results = session.run(stages, parallel=False)
                    pid = read_server_pid(root / ".tmp/bazel-gate/output/base/server/server.pid.txt")
                    (root / ".tmp" / ("result-" + round_number + ".json")).write_text(
                        json.dumps({"pid": pid, "results": [
                            {"status": item.status, "detail": item.detail} for item in results
                        ]}), encoding="utf-8",
                    )
                finally:
                    session.close()
                raise SystemExit(run.finish(results))
                """)
            first_pid: int | None = None
            for round_number in (1, 2):
                done = subprocess.run(
                    [
                        sys.executable,
                        "-c",
                        consumer,
                        str(root),
                        str(round_number),
                        json.dumps([dataclasses.asdict(stage) for stage in stages]),
                    ],
                    cwd=test_gate.ROOT,
                    env=environment,
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    timeout=180.0,
                    check=False,
                )
                round_output[round_number] = (done.stdout, done.stderr)
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                evidence = cast(
                    "dict[str, object]", json.loads((state / f"result-{round_number}.json").read_text(encoding="utf-8"))
                )
                values = cast("list[dict[str, str]]", evidence["results"])
                self.assertTrue(all(item["status"] == "PASS" for item in values))
                self.assertIn("Bazel executed", values[1]["detail"])
                if round_number == 2:
                    self.assertIn("Bazel cached", values[0]["detail"])
                    self.assertEqual(evidence["pid"], first_pid, "独立 CLI 必须复用同一原生 server")
                first_pid = cast("int", evidence["pid"])
                self.assertEqual((state / "external-count").read_text().splitlines(), ["run"] * round_number)
                self.assertEqual((state / "stable-count").read_text().splitlines(), ["run"])


class GateNativeCompilerObservationTests(unittest.TestCase):
    """真实 dispatch / adapter / Job 链必须向消费者发布编译扣时。."""

    def test_real_adapter_excludes_rustc_but_not_following_python_work(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "compiler-observation"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            native = root / ".tmp" / "bazel-gate"
            native.mkdir(parents=True)
            adapter = native / "bazel_action.py"
            shutil.copyfile(test_gate.ROOT / "scripts" / "bazel_action.py", adapter)
            repository = test_gate.ROOT
            source = root / "observed.rs"
            source.write_text(
                "\n".join(
                    f"pub fn observed_{index}(x: u64) -> u64 {{ x.wrapping_add({index}) }}" for index in range(20000)
                ),
                encoding="utf-8",
            )
            rustc = subprocess.run(
                ["rustup", "which", "rustc"], capture_output=True, text=True, check=True
            ).stdout.strip()
            output = io.StringIO()
            with patch.object(test_gate, "ROOT", root), contextlib.redirect_stdout(output):
                run = gate_runtime.GateRun(
                    "fulltest",
                    60.0,
                    root,
                    root / "logs",
                    start_command=test_gate._start_owned_command,
                    close_command=test_gate._close_owned_command,
                    kill_tree=test_gate._kill_tree,
                    compiler_ids=test_gate._compiler_ids,
                    observe_tree=test_gate._observe_owned_command,
                )
                results = []
                excluded = 0.0
                for name, argv in (
                    (
                        "compiler",
                        [rustc, str(source), "--crate-type=lib", "--emit=metadata", "-o", str(root / "observed.rmeta")],
                    ),
                    ("noncompiler", [sys.executable, "-c", "import time; time.sleep(0.2); print('noncompile work')"]),
                    (
                        "overlap",
                        [
                            sys.executable,
                            "-c",
                            "import subprocess,sys,time; p=subprocess.Popen(sys.argv[1:]); time.sleep(0.2); sys.exit(p.wait())",
                            rustc,
                            str(source),
                            "--crate-type=lib",
                            "--emit=metadata",
                            "-o",
                            str(root / "overlap.rmeta"),
                        ],
                    ),
                ):
                    # 使用真实派发入口写逐动作 runtime；不注入 compiler_ids 或进程快照。
                    spec = root / bazel_label(name).split(":")[0].removeprefix("//") / "spec.json"
                    spec.parent.mkdir(parents=True, exist_ok=True)
                    spec.write_text(
                        json.dumps(
                            {
                                "name": name,
                                "kind": "compiler-observed",
                                "root": str(root),
                                "argv": argv,
                                "environment": dict(os.environ),
                                "watch_test_failures": False,
                            }
                        ),
                        encoding="utf-8",
                    )
                    runtime = native / "runtime.json"
                    runtime.write_text(
                        json.dumps(
                            {
                                name: {
                                    "environment": {},
                                    "log": str(root / (name + ".log")),
                                    "result_file": str(root / (name + ".results")),
                                    "TEMP": None,
                                    "TMP": None,
                                }
                            }
                        ),
                        encoding="utf-8",
                    )
                    config = native / "dispatch.json"
                    config.write_text(
                        json.dumps(
                            {
                                "argv": [sys.executable, str(adapter), str(spec)],
                                "runtime": str(runtime),
                            }
                        ),
                        encoding="utf-8",
                    )
                    prepare = cast("Callable[[str], int]", vars(bazel_gate)["_dispatch_main"])
                    self.assertEqual(prepare(str(config)), 0, "准备入口只发布本轮 runtime，不执行消费者")
                    results.extend(
                        run.supervise(
                            gate_runtime.Stage(
                                name,
                                [sys.executable, str(adapter), str(spec)],
                                kind="compiler-observed",
                                env={"PYTHONPATH": str(repository)},
                            )
                        )
                    )
                    if name == "compiler":
                        excluded = run.compile_excluded
                        self.assertGreater(excluded, 0.0, "真实 adapter 监督层遮蔽了 rustc 编译扣时")
                    else:
                        self.assertEqual(run.compile_excluded, excluded, "非编译工作或重叠工作仍被扣时")
                self.assertEqual(run.finish(results), 0)
            self.assertTrue((root / "observed.rmeta").is_file())
            self.assertEqual((root / "noncompiler.log").read_text().strip(), "noncompile work")
            self.assertIn("fulltest PASS total=", output.getvalue())
            self.assertIn(f"compile_excluded={excluded:.1f}s", output.getvalue())

    def _persistent_service_fixture(self, limit: float) -> tuple[Path, gate_runtime.GateRun, gate_runtime.Stage]:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "persistent-consumer"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        service = root / "service.py"
        _ = service.write_text(
            textwrap.dedent("""\
            import json
            import os
            from pathlib import Path
            import subprocess
            import sys
            import time
            import traceback

            root = Path(sys.argv[1])
            # An exclusive lifetime marker makes an accidental second service fail.
            with (root / "service-started").open("x", encoding="ascii") as marker:
                marker.write(str(os.getpid()))
            child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
            seen = set()
            values = []

            def publish(path, value):
                pending = path.with_suffix(path.suffix + ".pending")
                pending.write_text(json.dumps(value), encoding="utf-8")
                pending.replace(path)

            def read_request(path):
                # rename 发布后存在杀软/文件系统短暂不可读窗口（open 失败或暂空内容）；
                # 有界重试等待窗口过去，不吞掉持续性错误。
                for attempt in range(50):
                    try:
                        text = path.read_text(encoding="utf-8")
                        return json.loads(text)
                    except (OSError, ValueError):
                        if attempt == 49:
                            raise
                        time.sleep(0.02)

            try:
                while True:
                    for request in sorted(root.glob("*.request.json")):
                        if request.name in seen:
                            continue
                        command = read_request(request)
                        seen.add(request.name)
                        values.append(command["value"])
                        publish(request.with_suffix(".response.json"), {
                            "pid": os.getpid(), "child": child.pid, "values": values,
                        })
                        if command.get("wait"):
                            while True:
                                time.sleep(0.01)
                        publish(request.with_suffix(".complete.json"), command["exit"])
                    time.sleep(0.01)
            except BaseException:
                # 保留服务异常退出证据；夹具目录随后由 TemporaryDirectory 回收。
                (root / "service-crash.txt").write_text(traceback.format_exc(), encoding="utf-8")
                raise
            """),
            encoding="utf-8",
        )
        run = gate_runtime.GateRun(
            "fulltest",
            limit,
            root,
            root / "logs",
            start_command=test_gate._start_owned_command,
            close_command=test_gate._close_owned_command,
            kill_tree=test_gate._kill_tree,
            compiler_ids=test_gate._compiler_ids,
            observe_tree=test_gate._observe_owned_command,
        )
        self.addCleanup(run.finish, [])
        return root, run, gate_runtime.Stage("persistent-consumer", [sys.executable, str(service), str(root)])

    def _publish_persistent_request(self, root: Path, name: str, *, code: int = 0, wait: bool = False) -> Path:
        request = root / f"{name}.request.json"
        pending = request.with_suffix(".pending")
        _ = pending.write_text(json.dumps({"value": name, "exit": code, "wait": wait}), encoding="utf-8")
        _ = pending.replace(request)
        return request

    def _persistent_process_handles(self, response: Path) -> list[int]:
        identity = cast("dict[str, object]", json.loads(response.read_text(encoding="utf-8")))
        handles: list[int] = []
        for key in ("pid", "child"):
            pid = identity[key]
            self.assertIsInstance(pid, int)
            handle = test_gate.win32api.OpenProcess(test_gate.win32con.SYNCHRONIZE, False, cast("int", pid))
            self.addCleanup(test_gate.win32api.CloseHandle, handle)
            self.assertEqual(test_gate.win32event.WaitForSingleObject(handle, 0), test_gate.win32event.WAIT_TIMEOUT)
            handles.append(int(handle))
        return handles

    def test_real_persistent_requests_preserve_state_and_live_job_until_close(self) -> None:
        with contextlib.redirect_stdout(io.StringIO()):
            root, run, stage = self._persistent_service_fixture(30.0)
            first = self._publish_persistent_request(root, "first")
            first_results = run.supervise_persistent(stage, first.with_suffix(".complete.json"))
            self.assertEqual([result.status for result in first_results], ["PASS"])
            first_response = first.with_suffix(".response.json")
            handles = self._persistent_process_handles(first_response)
            first_state = cast("dict[str, object]", json.loads(first_response.read_text(encoding="utf-8")))
            self.assertEqual(first_state["values"], ["first"])
            first_bytes = {
                path: path.read_bytes()
                for path in (
                    first,
                    first_response,
                    first.with_suffix(".complete.json"),
                )
            }

            second = self._publish_persistent_request(root, "second")
            second_results = run.supervise_persistent(stage, second.with_suffix(".complete.json"))
            self.assertEqual([result.status for result in second_results], ["PASS"])
            second_state = cast(
                "dict[str, object]",
                json.loads(second.with_suffix(".response.json").read_text(encoding="utf-8")),
            )
            self.assertEqual(second_state["values"], ["first", "second"])
            self.assertEqual(second_state["pid"], first_state["pid"])
            self.assertEqual(second_state["child"], first_state["child"])
            for path, original in first_bytes.items():
                self.assertEqual(path.read_bytes(), original, "已发布的请求或完成快照被回写")
            for handle in handles:
                self.assertEqual(test_gate.win32event.WaitForSingleObject(handle, 0), test_gate.win32event.WAIT_TIMEOUT)
            run.close_persistent()
            for handle in handles:
                self.assertEqual(
                    test_gate.win32event.WaitForSingleObject(handle, 0),
                    test_gate.win32event.WAIT_OBJECT_0,
                    "会话关闭后仍遗留持久服务或所属后代",
                )
            self.assertEqual(run.finish([*first_results, *second_results]), 0)

    def test_real_persistent_failure_and_deadline_stop_service_and_descendant(self) -> None:
        for reason in ("failure", "timeout"):
            output = io.StringIO()
            with self.subTest(reason=reason), contextlib.redirect_stdout(output):
                root, run, stage = self._persistent_service_fixture(3.0 if reason == "timeout" else 30.0)
                warmup = self._publish_persistent_request(root, "warmup")
                self.assertEqual(
                    [result.status for result in run.supervise_persistent(stage, warmup.with_suffix(".complete.json"))],
                    ["PASS"],
                )
                handles = self._persistent_process_handles(warmup.with_suffix(".response.json"))
                request = self._publish_persistent_request(root, reason, code=7, wait=reason == "timeout")
                with self.assertRaises(gate_runtime.GateStoppedError) as stopped:
                    run.supervise_persistent(stage, request.with_suffix(".complete.json"))
                response_path = request.with_suffix(".response.json")
                if not response_path.exists():
                    crash = root / "service-crash.txt"
                    self.fail(
                        "持久服务未处理第二个请求即停止；"
                        f"目录={sorted(item.name for item in root.iterdir())}；"
                        f"服务异常={crash.read_text(encoding='utf-8') if crash.exists() else '无崩溃记录'}；"
                        f"内部停止输出={output.getvalue()}"
                    )
                response = cast(
                    "dict[str, object]",
                    json.loads(response_path.read_text(encoding="utf-8")),
                )
                self.assertEqual(response["values"], ["warmup", reason], "服务未实际接收第二个请求")
                for handle in handles:
                    self.assertEqual(
                        test_gate.win32event.WaitForSingleObject(handle, 0),
                        test_gate.win32event.WAIT_OBJECT_0,
                        "首错或预算耗尽后仍遗留持久服务或所属后代",
                    )
                self.assertEqual(run.finish(stopped.exception.results), 1)
                if reason == "failure":
                    self.assertEqual(request.with_suffix(".complete.json").read_text(encoding="utf-8"), "7")
                    self.assertIn("FAIL", [result.status for result in stopped.exception.results])
                    self.assertIn("exit=7", output.getvalue())
                    self.assertIn("fulltest FAIL total=", output.getvalue())
                else:
                    self.assertFalse(request.with_suffix(".complete.json").exists())
                    self.assertIn("TIMEOUT", [result.status for result in stopped.exception.results])
                    self.assertIn("fulltest TIMEOUT total=", output.getvalue())

    def test_doctest_native_cache_requires_exact_supported_command_and_environment(self) -> None:
        stage = gate_runtime.Stage(
            "root-doctests",
            ["cargo", "test", "--doc", "--features", "test-hooks", "--"],
            "compiler-observed",
        )
        with patch.dict(os.environ, {}, clear=True), patch.object(bazel_gate, "doc_cache_probe", return_value=True):
            self.assertTrue(bazel_gate.doctest(stage))
            self.assertTrue(bazel_gate._reuse(stage))
            with patch.dict(os.environ, {"RUSTDOC": "external-rustdoc.exe"}):
                self.assertFalse(bazel_gate._reuse(stage))
        self.assertFalse(bazel_gate.doctest(dataclasses.replace(stage, name="unknown-doctests")))
        self.assertFalse(bazel_gate.doctest(dataclasses.replace(stage, argv=[*stage.argv, "--ignored"])))

    def test_doctest_cache_allows_only_scheduling_and_progress_cargo_overrides(self) -> None:
        stage = gate_runtime.Stage(
            "root-doctests",
            ["cargo", "test", "--doc", "--features", "test-hooks", "--"],
            "compiler-observed",
        )
        allowed = {"CARGO_BUILD_JOBS": "28", "CARGO_TERM_PROGRESS_WHEN": "never"}
        with patch.dict(os.environ, {}, clear=True), patch.object(bazel_gate, "doc_cache_probe", return_value=True):
            with patch.dict(os.environ, allowed):
                self.assertTrue(bazel_gate._reuse(stage), "宿主调度/输出设置不引入外部文档依赖")
            self.assertTrue(bazel_gate._reuse(dataclasses.replace(stage, env=allowed)))
            for key in (
                "CARGO_TARGET_DIR",
                "CARGO_BUILD_TARGET",
                "CARGO_UNKNOWN_INPUT",
                "RUSTC_WRAPPER",
                "RUSTDOC",
                "RUSTFLAGS",
            ):
                with self.subTest(key=key):
                    with patch.dict(os.environ, {**allowed, key: "external"}):
                        self.assertFalse(bazel_gate._reuse(stage))
                    self.assertFalse(bazel_gate._reuse(dataclasses.replace(stage, env={**allowed, key: "external"})))

    def test_repository_engine_cache_requires_declared_in_repository_exe_and_dll(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "engine-cache-eligibility"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary) / "repository"
            engine = root / "resources/7zip/7z.exe"
            engine.parent.mkdir(parents=True)
            library = engine.with_name("7z.dll")
            # 只验证缓存元数据资格；这些合成字节绝不作为真实引擎执行。
            engine.write_bytes(b"metadata-only executable fixture")
            library.write_bytes(b"metadata-only library fixture")
            foreign = Path(temporary) / "foreign-7z.exe"
            foreign.write_bytes(b"foreign metadata fixture")
            stage = gate_runtime.Stage(
                "real-engine-tests-fixture",
                ["metadata-only-never-executed"],
                kind="check",
                env={"JCHTOOLS_TEST_7ZIP": str(engine)},
            )
            with (
                patch.object(bazel_gate, "__file__", str(root / "scripts/bazel_gate.py")),
                patch.dict(os.environ, {}, clear=True),
            ):
                for name in ("real-engine-tests-fixture", "real-engine-lib-tests-fixture"):
                    with self.subTest(name=name):
                        eligible = dataclasses.replace(stage, name=name)
                        self.assertTrue(bazel_gate._repository_engine_cacheable(eligible))
                        self.assertTrue(bazel_gate._reuse(eligible))
                for invalid in (
                    dataclasses.replace(stage, env={"JCHTOOLS_TEST_7ZIP": str(foreign)}),
                    dataclasses.replace(stage, env={"JCHTOOLS_TEST_7ZIP": str(engine), "UNKNOWN_INPUT": "undeclared"}),
                    dataclasses.replace(stage, env={}),
                    dataclasses.replace(stage, name="real-media-tests-fixture"),
                ):
                    with self.subTest(stage=invalid):
                        self.assertFalse(bazel_gate._repository_engine_cacheable(invalid))
                        self.assertFalse(bazel_gate._reuse(invalid))
                with patch.dict(os.environ, {"JCHTOOLS_UNDECLARED_INPUT": str(foreign)}):
                    self.assertFalse(bazel_gate._reuse(stage), "全局外部输入覆盖仍必须禁止缓存")
                library.unlink()
                self.assertFalse(bazel_gate._repository_engine_cacheable(stage))
                self.assertFalse(bazel_gate._reuse(stage), "仅有 EXE、缺少真实声明 DLL 不能获得资格")
                library.write_bytes(b"metadata-only library fixture")
                engine.unlink()
                self.assertFalse(bazel_gate._repository_engine_cacheable(stage))
                self.assertFalse(bazel_gate._reuse(stage))

    def test_new_rustdoc_examples_or_dynamic_documentation_disable_cache(self) -> None:
        for source, safe in (
            ("/// Plain API prose.\npub fn example() {}", True),
            ("/// ```rust\n/// assert!(true);\n/// ```", False),
            ('#[doc = include_str!("outside.md")]\npub struct Example;', False),
            ("/// fenced code block 开始：``` 或 ~~~。\nfn parse() {}", True),
            ('/// Heading.\n///\n///    panic!("not a doctest");', True),
            ('/// Heading.\n///\n///     panic!("real doctest");', False),
        ):
            with (
                self.subTest(source=source),
                patch.object(bazel_gate.os, "walk", return_value=[("synthetic", [], ["lib.rs"])]),
                patch.object(Path, "is_symlink", return_value=False),
                patch.object(Path, "read_text", return_value=source),
            ):
                self.assertEqual(bazel_gate.doc_cache_probe(Path("synthetic")), safe)

    def test_controller_identity_does_not_allow_generic_python_or_java(self) -> None:
        native = test_gate.ROOT / ".tmp" / "bazel-gate"
        python = Path(sys.executable)
        self.assertTrue(
            test_gate._gate_controller(
                python,
                (str(python), "-m", "scripts.bazel_gate", "--dispatch", str(native / "dispatch.json")),
            )
        )
        adapter = native / "bazel_action.py"
        spec = test_gate.ROOT / bazel_label("controller-fixture").split(":")[0].removeprefix("//") / "spec.json"
        self.assertTrue(test_gate._gate_controller(python, (str(python), str(adapter), str(spec))))
        for invalid in (
            native / "spec.json",
            spec.with_name("other.json"),
            spec.parent.with_name("not-a-hash") / "spec.json",
        ):
            with self.subTest(invalid=invalid):
                self.assertFalse(test_gate._gate_controller(python, (str(python), str(adapter), str(invalid))))
        self.assertFalse(test_gate._gate_controller(python, (str(python), "-c", "print('work')")))
        self.assertFalse(
            test_gate._gate_controller(
                python,
                (
                    str(python),
                    str(native / "unrelated.py"),
                    str(native / "spec.json"),
                ),
            )
        )
        java = Path("C:/tools/java.exe")
        self.assertTrue(
            test_gate._gate_controller(
                java,
                (
                    str(java),
                    "-jar",
                    str(native / "output" / "install" / "identity" / "A-server.jar"),
                    "--batch",
                    "--output_base=" + str(native / "output" / "base"),
                ),
            )
        )
        self.assertFalse(test_gate._gate_controller(java, (str(java), "-jar", "checker.jar")))
        self.assertFalse(
            test_gate._gate_controller(
                java,
                (
                    str(java),
                    "-jar",
                    str(native / "output" / "install" / "identity" / "A-server.jar"),
                    "--batch",
                    "--output_base=C:/unrelated",
                ),
            )
        )

    def test_shared_install_controllers_preserve_fixture_compiler_observation(self) -> None:
        source = Path(bazel_gate.__file__).resolve().parents[1]
        shared = source / ".tmp" / "bazel-gate" / "output"
        root = source / ".tmp" / "controller-identity-fixture"
        output_base = root / ".tmp" / "bazel-gate" / "output" / "base"
        foreign_base = root.with_name("other-controller-fixture") / ".tmp" / "bazel-gate" / "output" / "base"
        java = Path("C:/tools/java.exe")
        bazel = Path("C:/tools/bazel.exe")
        python = Path(sys.executable)
        jar = shared / "install" / "identity" / "A-server.jar"
        server = (str(java), "-jar", str(jar), "--batch", "--output_base=" + str(output_base))
        client = (
            str(bazel),
            "--nobatch",
            "--output_user_root=" + str(shared),
            "--output_base=" + str(output_base),
            "build",
        )
        cases = (
            ("shared-server-batch", java, server, True),
            ("shared-server-persistent", java, (*server[:3], "--max_idle_secs=60", server[-1]), True),
            ("shared-client", bazel, client, True),
            ("foreign-server-base", java, (*server[:-1], "--output_base=" + str(foreign_base)), False),
            ("nested-server-base", java, (*server[:-1], "--output_base=" + str(output_base / "other")), False),
            ("foreign-client-base", bazel, (*client[:3], "--output_base=" + str(foreign_base), "build"), False),
            ("missing-client-base", bazel, (*client[:3], "build"), False),
            (
                "foreign-client-root",
                bazel,
                (str(bazel), "--nobatch", "--output_user_root=" + str(root), client[3], "build"),
                False,
            ),
            ("foreign-server-jar", java, (*server[:2], str(root / "install" / "A-server.jar"), *server[3:]), False),
            ("missing-server-mode", java, (*server[:3], server[-1]), False),
            ("missing-client-mode", bazel, (client[0], *client[2:]), False),
            ("generic-java", java, (str(java), "-jar", "checker.jar"), False),
            ("generic-python", python, (str(python), "-c", "print('work')"), False),
        )
        process = MagicMock(spec=subprocess.Popen)
        process.pid = 12345
        owner = MagicMock()
        owner.process = process
        owner.process_handles.return_value = [101, 102]

        for name, executable, argv, allowed in cases:
            with self.subTest(name=name), patch.object(test_gate, "ROOT", root):
                self.assertEqual(test_gate._gate_controller(executable, argv), allowed)

                def image(handle, _flags, buffer, _size):
                    buffer.value = str(root / "rustc.exe" if handle == 101 else executable)
                    return True

                with (
                    patch.dict(test_gate._COMMAND_JOBS, {process.pid: owner}, clear=True),
                    patch.object(
                        test_gate.win32event, "WaitForSingleObject", return_value=test_gate.win32event.WAIT_TIMEOUT
                    ),
                    patch.object(test_gate, "query_process_image", side_effect=image),
                    patch.object(test_gate, "get_process_id", side_effect=lambda handle: handle),
                    patch.object(test_gate.process_runtime, "process_argv", return_value=argv),
                    patch.object(test_gate.win32api, "CloseHandle"),
                ):
                    self.assertEqual(test_gate._compiler_ids(process), frozenset({101}) if allowed else frozenset())

    def test_static_or_document_work_overlapping_rustc_remains_charged(self) -> None:
        for checker in ("clippy-driver.exe", "rustdoc.exe"):
            with self.subTest(checker=checker):
                process = MagicMock(spec=subprocess.Popen)
                process.pid = 12345
                owner = MagicMock()
                owner.process = process
                owner.process_handles.return_value = [101, 102]

                def image(handle, _flags, buffer, _size):
                    buffer.value = str(test_gate.ROOT / ("rustc.exe" if handle == 101 else checker))
                    return True

                with (
                    patch.dict(test_gate._COMMAND_JOBS, {process.pid: owner}, clear=True),
                    patch.object(
                        test_gate.win32event, "WaitForSingleObject", return_value=test_gate.win32event.WAIT_TIMEOUT
                    ),
                    patch.object(test_gate, "query_process_image", side_effect=image),
                    patch.object(test_gate, "get_process_id", side_effect=lambda handle: handle),
                    patch.object(test_gate.win32api, "CloseHandle"),
                ):
                    self.assertEqual(test_gate._compiler_ids(process), frozenset())
