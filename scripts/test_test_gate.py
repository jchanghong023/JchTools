"""test_gate 预算会计与真实模型授权边界回归单测.

覆盖两个已确认的预算缺陷；当前三级门共用非编译预算，真正编译独占区间扣除：

1. ``run_logged`` 的有限等待预算必须按绝对截止（started+timeout）计算，进程
   启动与 Job 绑定消耗的时间要从预算中扣除；
2. ``execute_gate`` 的非编译工作与最终汇总共用时钟；即使阶段报告成功，
   非编译墙钟越界也必须失败，不得报退出码 0。

确定性：用注入假时钟替代 ``time.monotonic``，不真实睡眠 61 秒；真实预算进程
无限等待直至被终止，并直接核对传入真实 wait 的剩余预算，避免宿主调度改变结论。
日志及真实清理夹具仅写 .tmp/test-gate、.tmp/parallel-review 与 .tmp/acp-review；CI 身份及默认等待使用模拟响应。

完整门授权边界仅调用真实计划器；合成 Cargo artifact 日志交由生产解析器读取，
fake GateRun 只记录排程，进程启动接缝全部禁止，不执行完整门、远程或模型调用。
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

from scripts import gate_runtime, local_gate_plan, make_tmp, test_gate, test_timing, xberg_test_engine
from scripts.test_gate import stage_remote_workflow


class GitSnapshotEncodingTests(unittest.TestCase):
    """覆盖 P-11/P-13：中文工作树快照不受宿主默认代码页影响。."""

    def test_chinese_git_status_uses_utf8_under_gbk_host(self) -> None:
        git = shutil.which("git")
        assert git is not None  # nosec B101: 测试依赖必须存在，缺失不能冒充通过。
        workspace = test_gate.ROOT / ".tmp" / "release-validation" / "encoding-regression"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            code = make_tmp.main()
            assert code == 0  # nosec B101: 必须真实创建隔离测试目录。
        # 覆盖 P-13：用户 Git 配置或 GIT_* 覆盖不能隐藏合成文件、重定向隔离仓库。
        git_environment = {key: value for key, value in os.environ.items() if not key.upper().startswith("GIT_")}
        git_environment.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull})
        with (
            tempfile.TemporaryDirectory(dir=workspace) as temporary,
            patch.dict(os.environ, git_environment, clear=True),
        ):
            root = Path(temporary)
            _ = subprocess.run([git, "init", "-q", str(root)], check=True, capture_output=True)
            _ = (root / "路径中文.txt").write_text("公开合成数据", encoding="utf-8")
            # 仅强制子进程默认解码接缝为 GBK；真实 git 仍输出实际 UTF-8 文件名。
            with patch.object(test_gate, "ROOT", root), patch("subprocess._text_encoding", return_value="gbk"):
                output = test_gate._git_output(  # noqa: SLF001  # pyright: ignore[reportPrivateUsage]  # 真实快照读取回归接缝。
                    git, ["-c", "core.quotePath=false", "status", "--porcelain"]
                )
            assert output == "?? 路径中文.txt"  # nosec B101: 真实文件名必须完整保留。


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


class AcceptanceCoverageGapTests(unittest.TestCase):
    """acceptance 覆盖缺口判定：仅剩环境受限条目（C08/C09）NOT RUN 时不得阻塞测试门。."""

    def test_only_environment_blocked_items_are_not_a_gap(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目：C08、C09；缺资产或 --only 子集不能冒充完整验收）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        assert test_gate.acceptance_coverage_gaps(text) == []  # nosec B101: 环境受限条目不构成缺口。

    def test_other_not_run_items_remain_gaps(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目：A25、C08；详见 markdown-acceptance.log）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        gaps = test_gate.acceptance_coverage_gaps(text)
        assert gaps  # nosec B101: A25 缺样本不属环境受限，不得放行。

    def test_not_run_without_item_ids_remains_gap(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目；详见 markdown-acceptance.log）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        gaps = test_gate.acceptance_coverage_gaps(text)
        assert gaps  # nosec B101: 提取不到条目号时 fail-closed，不得静默放行。

    def test_partial_environment_blocked_items_remain_gaps(self) -> None:
        text = "PARTIAL  markdown-acceptance（仍有 NOT RUN：C08、C09）\nPASS  snap-ocr-worker-root"
        gaps = test_gate.acceptance_coverage_gaps(text)
        assert gaps  # nosec B101: 环境例外仅放行 NOT RUN，PARTIAL 必须仍阻塞。


class RemoteWorkflowIdentityTests(unittest.TestCase):
    """新触发的流水线不能借用同提交历史运行的成功结果。."""

    def test_old_success_for_same_head_is_not_current_dispatch(self) -> None:
        head = "a" * 40
        historical = [
            {
                "databaseId": 123,
                "event": "workflow_dispatch",
                "headSha": head,
                "status": "completed",
                "conclusion": "success",
                "url": "https://github.com/example/repo/actions/runs/123",
            }
        ]
        response = subprocess.CompletedProcess(["gh"], 0, json.dumps(historical), "")
        with (
            patch.object(test_gate, "_git_output", side_effect=[head, "origin/main", "main"]),
            patch("scripts.test_gate.subprocess.run", return_value=response),
            patch("scripts.test_gate.time.sleep"),
            patch("scripts.test_gate.time.monotonic", side_effect=[0.0, 0.0, 2.0]),
        ):
            result = stage_remote_workflow("git", "gh", "check.yml", watch_seconds=1.0)
        assert result.status == test_gate.STATUS_UNVERIFIED  # nosec B101: 本次 run 尚未出现，旧 success 不能冒充。

    def test_new_dispatch_success_is_observed_when_wait_requested(self) -> None:
        head = "a" * 40
        new_run = [
            {
                "databaseId": 124,
                "event": "workflow_dispatch",
                "headSha": head,
                "status": "completed",
                "conclusion": "success",
                "url": "https://github.com/example/repo/actions/runs/124",
            }
        ]
        previous = subprocess.CompletedProcess(["gh"], 0, "[]", "")
        triggered = subprocess.CompletedProcess(["gh"], 0, "", "")
        current = subprocess.CompletedProcess(["gh"], 0, json.dumps(new_run), "")
        with (
            patch.object(test_gate, "_git_output", side_effect=[head, "origin/main", "main"]),
            patch("scripts.test_gate.subprocess.run", side_effect=[previous, triggered, current]),
            patch("scripts.test_gate.time.sleep"),
            patch("scripts.test_gate.time.monotonic", side_effect=[0.0, 0.0]),
        ):
            result = stage_remote_workflow("git", "gh", "check.yml", watch_seconds=1.0)
        assert result.status == test_gate.STATUS_OK  # nosec B101: 身份隔离不得阻止本次真实新 run 的最终成功。
        assert "actions/runs/124" in result.detail  # nosec B101: 最终证据绑定新 run。


class SlowtestDispatchBoundaryTests(unittest.TestCase):
    """默认 slowtest 触发后如实未验证，不自动等待远程结论。."""

    def test_default_slowtest_reports_new_run_without_waiting(self) -> None:
        head = "a" * 40
        new_run = [
            {
                "databaseId": 124,
                "event": "workflow_dispatch",
                "headSha": head,
                "status": "queued",
                "conclusion": "",
                "url": "https://github.com/example/repo/actions/runs/124",
            }
        ]
        ok = subprocess.CompletedProcess(["gh"], 0, "", "")
        previous = subprocess.CompletedProcess(["gh"], 0, "[]", "")
        current = subprocess.CompletedProcess(["gh"], 0, json.dumps(new_run), "")
        output = io.StringIO()
        with (
            patch.object(test_gate, "snapshot_lines", return_value=[]),
            patch("scripts.test_gate.shutil.which", return_value="tool"),
            patch.object(test_gate, "_git_output", side_effect=[head, "origin/main", "main"]),
            patch("scripts.test_gate.subprocess.run", side_effect=[previous, ok, current]),
            patch("scripts.test_gate.time.sleep", side_effect=AssertionError("默认触发不得轮询等待")),
            contextlib.redirect_stdout(output),
        ):
            result = stage_remote_workflow("tool", "tool", "check.yml", watch_seconds=None)
            code = test_gate.print_summary("legacy remote helper", [result], [])
        assert code != 0  # nosec B101: 新 run 未完成，整门不能宣称 PASS。
        assert "actions/runs/124" in output.getvalue()  # nosec B101: 交接必须提供本次触发的 run 链接。
        assert "UNVERIFIED" in output.getvalue()  # nosec B101: 触发不能冒充最终通过。


class TemporaryCleanupPathTests(unittest.TestCase):
    """临时目录删除契约由真实合成目录消费者验证。."""

    def test_cleanup_removes_real_owned_directory(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "parallel-review"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            code = make_tmp.main()
        assert code == 0  # nosec B101: 合成夹具根由项目临时工厂生成，CI 同样自包含。
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            directory = Path(temporary) / "owned"
            directory.mkdir()
            _ = (directory / "payload.txt").write_text("synthetic cleanup payload", encoding="utf-8")
            make_tmp.robust_rmtree(directory)
            assert not directory.exists()  # nosec B101: 真实 Windows 消费者删除必须完成，不仅检查字符串。


class _PlanningGateRun:
    """只接收真实计划器的阶段，不执行 argv，也不伪造模型响应。."""

    def __init__(self, log_dir: Path, logs: dict[str, Path]) -> None:
        self.log_dir: Path = log_dir
        self.logs: dict[str, Path] = logs
        self.stages: list[gate_runtime.Stage] = []

    def run(self, stages: list[gate_runtime.Stage]) -> list[gate_runtime.Result]:
        self.stages.extend(stages)
        return [
            gate_runtime.Result(
                stage.name,
                gate_runtime.STATUS_OK,
                "planning-only synthetic prerequisite; command not executed",
                log=self.logs.get(stage.name),
            )
            for stage in stages
        ]


class OpenCodeAuthorizationBoundaryTests(unittest.TestCase):
    """安装或配置 Agent 不是普通 fulltest / slowtest 的真实模型调用授权。."""

    def _assert_separate_model_authorization(self, installation: str) -> None:
        workspace = test_gate.ROOT / ".tmp" / "acp-review" / "gate-authorization"
        with (
            patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            code = make_tmp.main()
            assert code == 0  # nosec B101: 测试夹具目录必须创建成功。
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            log_dir = root / "logs"
            log_dir.mkdir()
            path_dir = root / "path"
            path_dir.mkdir()
            environment = {
                "PATH": str(path_dir),
                "PATHEXT": ".EXE",
                "APPDATA": str(root / "appdata"),
                "PROGRAMFILES": str(root / "program-files"),
                "PROGRAMFILES(X86)": str(root / "program-files-x86"),
            }
            # 授权边界用例需要完整有效的本地计划，不把缺少安装编译器当作测试前置。
            _ = (path_dir / "ISCC.exe").write_bytes(b"inert installer compiler; never execute")
            if installation != "absent":
                executable = {
                    "explicit": root / "explicit" / "opencode.exe",
                    "path": path_dir / "opencode.exe",
                    "npm": root / "appdata/npm/node_modules/@opencode/cli/bin/opencode.exe",
                }[installation]
                executable.parent.mkdir(parents=True, exist_ok=True)
                _ = executable.write_bytes(b"inert discovery fixture; never execute")
                executable.chmod(0o700)
                if installation == "explicit":
                    environment["JCHTOOLS_TEST_OPENCODE_EXE"] = str(executable)
            # 本地真实资产用例仍适用；这些占位文件仅供排程的 is_file 前置判定。
            for key, name in (
                ("JCHTOOLS_TEST_7ZIP", "7z.exe"),
                ("JCHTOOLS_TEST_BROKER_EXE", "JchTools.exe"),
            ):
                artifact = root / name
                _ = artifact.write_bytes(b"inert local artifact; never execute")
                environment[key] = str(artifact)
            environment.update(
                {
                    "JCHTOOLS_MEDIA_E2E_COMPONENT_DIR": str(root / "media-component"),
                    "JCHTOOLS_MEDIA_E2E_INPUT": str(root / "speech.wav"),
                    "JCHTOOLS_MEDIA_E2E_TRACKLESS_INPUT": str(root / "trackless.mp4"),
                    "JCHTOOLS_MEDIA_E2E_EXPECT_TEXT": "synthetic planning text",
                }
            )
            root_targets = (
                "acp_settings",
                "acp_discovery",
                "acp_discovery_env",
                "acp_callbacks",
                "acp_http_contract",
                "acp_http_disconnect",
                "acp_service_process",
                "acp_opencode",
                "archive",
                "xberg_assets",
                "markdown_media_e2e",
                "gui_flow",
            )
            build_targets = {
                "root-test-build": root_targets,
                "snap-ocr-core-test-build": ("snap_ocr_core",),
                "snap-ocr-worker-test-build": ("snap_ocr_worker",),
            }
            logs: dict[str, Path] = {}
            for build, targets in build_targets.items():
                log = log_dir / f"{build}.log"
                _ = log.write_text(
                    "\n".join(
                        json.dumps(
                            {
                                "reason": "compiler-artifact",
                                "profile": {"test": True},
                                "target": {"name": target, "kind": ["test"]},
                                "executable": str(root / "artifacts" / f"{target}.exe"),
                            }
                        )
                        for target in targets
                    ),
                    encoding="utf-8",
                )
                logs[build] = log
            snapshot_log = log_dir / "source-snapshot.log"
            _ = snapshot_log.write_text("synthetic source snapshot\n", encoding="utf-8")
            logs["source-snapshot"] = snapshot_log
            with (
                patch.dict(os.environ, environment, clear=True),
                patch("subprocess.run", side_effect=AssertionError("计划回归不得执行进程")),
                patch("subprocess.Popen", side_effect=AssertionError("计划回归不得启动进程")),
            ):
                for level in ("fulltest", "slowtest"):
                    with self.subTest(installation=installation, level=level):
                        run = _PlanningGateRun(log_dir, logs)
                        plan = local_gate_plan.gate_plan(level, root, log_dir, {})
                        results: list[gate_runtime.Result] = []
                        snapshot: list[str] = []
                        local_gate_plan.run_full_coverage(
                            cast("gate_runtime.GateRun", cast("object", run)), plan, results, snapshot, root
                        )
                        model_binary = str(root / "artifacts" / "acp_opencode.exe")
                        unauthorized = [
                            stage.name
                            for stage in run.stages
                            if stage.argv[0] == model_binary and "--ignored" in stage.argv
                        ]
                        assert unauthorized == [], "普通完整门不能自行取得真实模型调用授权"  # nosec B101: 授权边界回归。
                        model_results = [result for result in results if result.name == "real-opencode"]
                        assert len(model_results) == 1  # nosec B101: 真实模型验收必须独立报告。
                        assert (  # nosec B101: 不得把未授权模型调用冒报 PASS。
                            model_results[0].status == "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"
                        )

    def test_explicit_opencode_exe_does_not_authorize_model_calls(self) -> None:
        self._assert_separate_model_authorization("explicit")

    def test_path_native_opencode_exe_does_not_authorize_model_calls(self) -> None:
        self._assert_separate_model_authorization("path")

    def test_npm_native_opencode_exe_does_not_authorize_model_calls(self) -> None:
        self._assert_separate_model_authorization("npm")

    def test_no_opencode_installation_still_reports_separate_authorization(self) -> None:
        self._assert_separate_model_authorization("absent")


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


class GateFailFastTests(unittest.TestCase):
    """覆盖 P-13：阶段失败立即停止整个门，保留原始证据且不遗留并行进程。"""

    def setUp(self) -> None:
        self.workspace = test_gate.ROOT / ".tmp" / "test-gate" / "fail-fast-regression"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(self.workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=self.workspace)
        self.addCleanup(temporary.cleanup)
        self.workspace = Path(temporary.name)

    def test_failure_stops_pending_and_running_commands_at_every_level(self) -> None:
        for level in ("fastcheck", "fulltest", "slowtest"):
            with self.subTest(level=level), tempfile.TemporaryDirectory(dir=self.workspace) as temporary:
                root = Path(temporary)
                ready, release, later = (root / name for name in ("ready", "release", "later"))
                peer_code = (
                    "import pathlib,sys,time; "
                    "ready,release=map(pathlib.Path,sys.argv[1:]); ready.touch(); "
                    "\nwhile not release.exists(): time.sleep(0.01)"
                )
                failure_code = (
                    "import pathlib,sys,time; ready=pathlib.Path(sys.argv[1]); "
                    "\nwhile not ready.exists(): time.sleep(0.01)"
                    "\nprint('synthetic failure',flush=True); sys.exit(7)"
                )
                later_code = "import pathlib,sys; [pathlib.Path(p).touch() for p in sys.argv[1:]]"
                stages = [
                    gate_runtime.Stage("active-peer", [sys.executable, "-c", peer_code, str(ready), str(release)]),
                    gate_runtime.Stage(
                        "first-error", [sys.executable, "-c", failure_code, str(ready)],
                        exclusive_resource="failure-chain",
                    ),
                    gate_runtime.Stage(
                        "must-not-run", [sys.executable, "-c", later_code, str(later), str(release)],
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
                peer = next(proc for proc in processes if peer_code in proc.args)
                self.assertNotEqual(peer.returncode, 0, "并行命令未被终止，而是继续等到自然完成")
                self.assertIn("exit=7", output.getvalue())
                self.assertIn("synthetic failure", output.getvalue())

    def test_launch_error_skips_remaining_commands(self) -> None:
        later = self.workspace / "launch-error-later"
        stages = [
            gate_runtime.Stage("missing-executable", [str(self.workspace / "missing.exe")]),
            gate_runtime.Stage(
                "must-not-run", [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)]
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
        with (
            patch.object(test_gate, "static_plan", return_value=[]),
            patch.object(test_gate, "_gate_plan", return_value=plan),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            code = test_gate.execute_gate("fastcheck", 60.0)
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
        with (
            patch.object(test_gate, "static_plan", return_value=[]),
            patch.object(test_gate, "_gate_plan", return_value=plan),
            contextlib.redirect_stdout(output),
        ):
            code = test_gate.execute_gate("fastcheck", 60.0)
        self.assertEqual(code, 1)
        self.assertFalse(later.exists())
        self.assertIn("binding loop", output.getvalue())

    def test_artifact_error_skips_other_builds_and_packaging(self) -> None:
        later = self.workspace / "artifact-later"
        marker = [sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()", str(later)]
        plan = [
            gate_runtime.Stage("production-build", [sys.executable, "-c", "pass"]),
            gate_runtime.Stage("root-test-build", [sys.executable, "-c", "print('no test artifact')"]),
            gate_runtime.Stage("snap-ocr-core-test-build", marker),
            gate_runtime.Stage("package", marker),
        ]
        output = io.StringIO()
        with (
            patch.object(test_gate, "static_plan", return_value=[]),
            patch.object(test_gate, "_gate_plan", return_value=plan),
            contextlib.redirect_stdout(output),
        ):
            code = test_gate.execute_gate("fulltest", 60.0)
        self.assertEqual(code, 1)
        self.assertFalse(later.exists())
        self.assertIn("root-test-build-artifacts", output.getvalue())

    def test_static_checker_stops_at_first_failed_check(self) -> None:
        scripts = self.workspace / "scripts"
        scripts.mkdir()
        checker = scripts / "static_check.py"
        shutil.copyfile(test_gate.ROOT / "scripts/static_check.py", checker)
        result = subprocess.run([sys.executable, str(checker)], capture_output=True, text=True, check=False)
        self.assertNotEqual(result.returncode, 0)
        report = json.loads((self.workspace / ".tmp/static-check.json").read_text(encoding="utf-8"))
        self.assertEqual([row["name"] for row in report["checks"]], ["manifests"])
        self.assertIn("FAIL manifests", result.stdout)

    def test_nested_test_failure_does_not_wait_for_runner_or_budget(self) -> None:
        artifact = self.workspace / "artifact.json"
        artifact.write_text(json.dumps({
            "reason": "compiler-artifact",
            "profile": {"test": True},
            "target": {"name": "synthetic", "kind": ["test"]},
            "executable": sys.executable,
        }), encoding="utf-8")
        for level in ("fulltest", "slowtest"):
            with self.subTest(level=level):
                nested = local_gate_plan._test_binary_stages(
                    gate_runtime.Result("root-test-build", "PASS", "synthetic artifact", log=artifact)
                )[0]
                nested.argv = [
                    sys.executable, "-c",
                    "import pathlib,sys,threading; print('test first_failure ... FAILED',flush=True); "
                    "pathlib.Path(sys.argv[-1]).write_text('failed first_failure\\n'); threading.Event().wait()",
                ]
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    code = test_gate.execute_gate(level, 1.0, stages_override=[nested])
                self.assertEqual(code, 1)
                self.assertIn(f"{level} FAIL total=", output.getvalue())
                self.assertNotIn("TIMEOUT", output.getvalue())
                self.assertIn("test first_failure ... FAILED", output.getvalue())

    def test_expected_failure_text_does_not_stop_successful_runner(self) -> None:
        stage = gate_runtime.Stage(
            "expected-panic-fixture",
            [sys.executable, "-c",
             "import pathlib,sys; print('test expected_panic ... FAILED'); "
             "pathlib.Path(sys.argv[-1]).write_text('ok expected_panic\\n')"],
            watch_test_failures=True,
        )
        with contextlib.redirect_stdout(io.StringIO()):
            code = test_gate.execute_gate("fulltest", 60.0, stages_override=[stage])
        self.assertEqual(code, 0, "测试自行输出的失败文字被误当成 harness 失败")


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


class CargoArtifactEncodingTests(unittest.TestCase):
    """实际读取 PowerShell 5.1 UTF-8 BOM 日志，不能丢掉首个唯一 Cargo artifact。"""

    def test_single_artifact_with_or_without_bom_is_discovered(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "cargo-bom-regression"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            artifact = {
                "reason": "compiler-artifact",
                "profile": {"test": True},
                "target": {"name": "中文测试", "kind": ["test"]},
                "executable": str(root / "测试.exe"),
            }
            for encoding in ("utf-8-sig", "utf-8"):
                with self.subTest(encoding=encoding):
                    log = root / f"{encoding}.log"
                    log.write_text(json.dumps(artifact, ensure_ascii=False) + "\n", encoding=encoding)
                    if encoding == "utf-8-sig":
                        self.assertTrue(log.read_bytes().startswith(b"\xef\xbb\xbf"))
                    discovered = local_gate_plan._test_binary_stages(
                        gate_runtime.Result("root-test-build", "PASS", "合成 Cargo 日志", log=log)
                    )
                    self.assertEqual(len(discovered), 1, "唯一 artifact 因 UTF-8 BOM 被丢失")
                    self.assertEqual(discovered[0].name, "root-test-build-中文测试")


class GateCleanupCancellationTests(unittest.TestCase):
    """覆盖 AGENTS.md 3.4：清理阶段取消也必须输出统一计时证据。"""

    def test_cleanup_interrupt_preserves_failed_timing_summary(self) -> None:
        run = gate_runtime.GateRun(
            "fastcheck", 60.0, test_gate.ROOT, test_gate.LOG_DIR,
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


class TimingBudgetAndEncodingTests(unittest.TestCase):
    """覆盖 AGENTS.md 3.4：计时预算不扩张，中文用例名原样交给 --exact。"""

    def test_remaining_budget_bounds_single_test_timeout(self) -> None:
        suite = test_timing.Suite("synthetic", Path("never-run.exe"), seconds=6.0, test_names=["first", "second"])
        with (
            patch.object(test_timing, "_run_checked", side_effect=subprocess.TimeoutExpired("never-run.exe", 7.0)) as run,
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
            patch.object(xberg_test_engine, "release_from_metadata", return_value=xberg_test_engine.Release("v", "0" * 64, 1)),
            patch.object(xberg_test_engine, "ensure_test_engine_target", side_effect=OSError("重定向目录")),
            patch.object(xberg_test_engine, "current_engine_matches", return_value=True) as cached,
            patch.dict(os.environ, {"JCHTOOLS_TEST_XBERG_DIR": ""}),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(xberg_test_engine.main(), 1)
        cached.assert_not_called()


if __name__ == "__main__":
    _ = unittest.main()
