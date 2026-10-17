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
import mmap
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import cast
from unittest.mock import MagicMock, patch

from scripts import gate_runtime, local_gate_plan, make_tmp, test_gate, test_timing, xberg_test_engine
from scripts.quality_scope import PYTHON_TEST_FILES
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


class CompleteGatePythonCoverageTests(unittest.TestCase):
    """覆盖 P-12/P-13：完整门不能遗漏已有 Python 单测模块。"""

    def test_fetch_7zip_tests_are_scheduled_only_in_complete_gates(self) -> None:
        for level in ("fastcheck", "fulltest", "slowtest"):
            with self.subTest(level=level):
                plan = local_gate_plan.gate_plan(level, test_gate.ROOT, test_gate.LOG_DIR, {})
                stages = [stage for stage in plan if "scripts.test_fetch_7zip" in stage.argv]
                self.assertEqual(len(stages), 0 if level == "fastcheck" else 1)
                if stages:
                    self.assertEqual(stages[0].name, "fetch-7zip-unit")
                    self.assertEqual(
                        stages[0].argv,
                        [sys.executable, "-m", "scripts.unittest_report", "-f", "scripts.test_fetch_7zip"],
                    )

    def test_ocr_fixture_discovery_includes_connection_and_isolation_tests(self) -> None:
        for level in ("fastcheck", "fulltest", "slowtest"):
            with self.subTest(level=level):
                plan = local_gate_plan.gate_plan(level, test_gate.ROOT, test_gate.LOG_DIR, {})
                stages = [stage for stage in plan if "discover" in stage.argv and "tests/ocr_fixtures" in stage.argv]
                self.assertEqual(len(stages), 0 if level == "fastcheck" else 1)
                if stages:
                    pattern = stages[0].argv[stages[0].argv.index("-p") + 1]
                    for filename in ("test_worker_root_isolation.py", "test_gui_worker_connection.py"):
                        self.assertTrue(Path(filename).match(pattern), f"OCR 发现范围遗漏 {filename}")
                        self.assertIn(f"tests/ocr_fixtures/{filename}", PYTHON_TEST_FILES)


class OwnedCommandLaunchCancellationTests(unittest.TestCase):
    """覆盖 AGENTS.md 3.4：绑定 Job 后的启动取消不能遗留暂停进程或 Job。"""

    def test_resume_interrupt_reaps_process_and_closes_owned_job(self) -> None:
        process = MagicMock(spec=subprocess.Popen)
        process.pid = 987654321
        owner = MagicMock()
        with (
            patch.object(test_gate.subprocess, "Popen", return_value=process),
            patch.object(test_gate, "_CommandJob", return_value=owner),
            patch.object(test_gate, "_resume_owned_process", side_effect=KeyboardInterrupt),
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
                test_gate._CommandJob(process)
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
                        "first-error",
                        [sys.executable, "-c", failure_code, str(ready)],
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
                peer = next(proc for proc in processes if peer_code in proc.args)
                self.assertNotEqual(peer.returncode, 0, "并行命令未被终止，而是继续等到自然完成")
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
                        "import pathlib,sys,threading; pathlib.Path(sys.argv[1]).touch(); threading.Event().wait()"
                    )
                    runner_code = (
                        "import pathlib,sys,threading,time; ready=pathlib.Path(sys.argv[1]); "
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
                        gate_runtime.Stage("active-peer", [sys.executable, "-c", peer_code, str(ready)]),
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
                    peer = next(proc for proc in processes if peer_code in proc.args)
                    runner = next(proc for proc in processes if runner_code in proc.args)
                    for proc in (peer, runner):
                        self.assertIsNotNone(proc.poll(), "原生失败后仍遗留所属进程")
                        self.assertNotEqual(proc.returncode, 0, "保活进程应由门终止")
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
                    "marker.write_text(str(child.pid)); "
                    "test_gate._close_owned_command=lambda *a,**k: threading.Event().wait(); "
                    "raise SystemExit(test_gate._gate_worker())"
                )
                outer = (
                    "import sys,threading; from unittest.mock import patch; "
                    "from scripts import test_gate; "
                    f"command=[sys.executable,'-c',{worker!r}]; "
                    "\nwith patch.object(test_gate,'_gate_worker_command',return_value=command,create=True), "
                    "patch.object(test_gate,'_close_owned_command',"
                    "lambda *a,**k: threading.Event().wait()):"
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
                    "\nwith patch.object(test_gate,'_gate_worker_command',return_value=command):"
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


class GateProgressPublicationTests(unittest.TestCase):
    """覆盖 P-13：共享编译扣时必须完整发布，且只能与读取后的墙钟比较。."""

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

        def abort(_proc: subprocess.Popen[bytes]) -> None:
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
        terminate.assert_called_once_with(process)
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
                    struct.pack_into("<i", state, 8, test_gate._WORKER_OK)
                return value

            with (
                patch.object(test_gate, "_gate_state", return_value=contextlib.nullcontext(state)),
                patch.object(test_gate, "_start_owned_command", return_value=process),
                patch.object(test_gate, "_abort_gate_worker") as terminate,
                patch.object(test_gate, "_monotonic", preempted_clock),
                contextlib.redirect_stdout(output),
            ):
                code = test_gate._supervise_gate("fulltest", 5.0, 0.0)
        terminate.assert_called_once_with(process)
        self.assertEqual(code, 0, output.getvalue())
        self.assertNotIn("STOP", output.getvalue())

    def _supervise_invalid_or_expired_progress(
        self, values: list[float], clock: list[float]
    ) -> tuple[int, str, MagicMock]:
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
        terminate.assert_called_once_with(process)
        return code, output.getvalue(), sleep

    def test_invalid_progress_still_fails_closed_and_aborts_worker(self) -> None:
        for values in ([float("nan")], [float("inf")], [-1.0], [4.0], [1.0, 0.0]):
            with self.subTest(values=values):
                code, output, _ = self._supervise_invalid_or_expired_progress(values, [3.0])
                self.assertEqual(code, 1)
                self.assertIn("工作进程上报了无效的编译扣时", output)
                self.assertIn("fulltest FAIL total=", output)
                self.assertNotIn("TIMEOUT", output)

    def test_compile_exclusion_does_not_extend_non_compile_hard_deadline(self) -> None:
        code, output, sleep = self._supervise_invalid_or_expired_progress([2.0, 2.0], [6.5, 7.0])
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
                    self.assertEqual(discovered[0].argv[0], artifact["executable"])
                    self.assertTrue(discovered[0].watch_test_failures)

    def test_same_name_lib_bin_and_integration_artifacts_all_reach_reports(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "crate-artifacts"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            executables = {kind: str(root / f"{kind}.exe") for kind in ("lib", "bin", "test")}
            log = root / "cargo.json"
            log.write_text(
                "\n".join(
                    json.dumps(
                        {
                            "reason": "compiler-artifact",
                            "profile": {"test": True},
                            "target": {"name": "jchocr", "kind": [kind]},
                            "executable": executable,
                        }
                    )
                    for kind, executable in executables.items()
                ),
                encoding="utf-8",
            )
            stages = local_gate_plan._test_binary_stages(
                gate_runtime.Result("root-test-build", "PASS", "合成 Cargo 日志", log=log)
            )
            self.assertCountEqual([stage.argv[0] for stage in stages], executables.values())
            self.assertEqual(len({stage.name for stage in stages}), len(executables))
            # 真实调度与汇总消费者执行隔离 Python 子进程；不把合成日志冒称 Cargo 验证。
            for stage in stages:
                stage.argv = [sys.executable, "-c", "import sys; print(sys.argv[1])", stage.argv[0]]
            output = io.StringIO()
            with patch.object(test_gate, "LOG_DIR", root / "logs"), contextlib.redirect_stdout(output):
                code = test_gate.execute_gate("fastcheck", 60.0, stages_override=stages)
            self.assertEqual(code, 0)
            reports = output.getvalue().splitlines()
            report_positions = []
            logs = []
            for stage in stages:
                matching = [i for i, line in enumerate(reports) if line.startswith(f"PASS {stage.name}:")]
                self.assertEqual(len(matching), 1, "每个实际执行的 artifact 必须独立报告")
                report_positions.append(matching[0])
                # 门按级别创建日志子目录；消费者应读取报告公开的路径，不猜内部布局或文件名。
                self.assertLess(matching[0] + 1, len(reports))
                log_line = reports[matching[0] + 1]
                self.assertTrue(log_line.startswith("  log="), "每个 artifact 必须报告实际日志路径")
                artifact_log = Path(log_line.removeprefix("  log="))
                logs.append(artifact_log)
                self.assertEqual(
                    artifact_log.read_text(encoding="utf-8").splitlines(),
                    [stage.argv[-1]],
                    "每个 artifact 的独立日志必须包含自身子进程输出，不能混入其它 artifact",
                )
            self.assertEqual(len(set(report_positions)), len(executables))
            self.assertEqual(len(logs), len(executables))
            self.assertEqual(len(set(logs)), len(executables), "不同 artifact 不能共享日志路径")
            recorded = "\n".join(path.read_text(encoding="utf-8") for path in logs)
            for executable in executables.values():
                self.assertIn(executable, recorded)

    def test_artifact_desktop_exclusions_preserve_other_test_cases(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "artifact-desktop-boundary"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
            root = Path(temporary)
            names = ("gui_flow", "acp_service_process", "jchtools_gui", "jchtools_file_core")
            log = root / "cargo.json"
            log.write_text(
                "\n".join(
                    json.dumps(
                        {
                            "reason": "compiler-artifact",
                            "profile": {"test": True},
                            "target": {"name": name, "kind": ["test"] if index < 2 else ["lib"]},
                            "executable": str(root / f"{name}.exe"),
                        }
                    )
                    for index, name in enumerate(names)
                ),
                encoding="utf-8",
            )
            stages = local_gate_plan._test_binary_stages(
                gate_runtime.Result("root-test-build", "PASS", "合成 Cargo 日志", log=log)
            )
            self.assertCountEqual(
                [stage.argv[0] for stage in stages],
                [str(root / f"{name}.exe") for name in names[1:]],
            )
            for stage in stages:
                skipped = [
                    stage.argv[index + 1] for index, argument in enumerate(stage.argv[:-1]) if argument == "--skip"
                ]
                expected = (
                    [
                        "real_gui_save_close_kill_reopen_and_confirmed_service_exit",
                        "real_gui_pending_configuration_applies_only_after_inflight_completion",
                    ]
                    if stage.argv[0] == str(root / "acp_service_process.exe")
                    else []
                )
                self.assertEqual(skipped, expected)
                self.assertNotIn("--ignored", stage.argv)
                self.assertTrue(stage.watch_test_failures)


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


if __name__ == "__main__":
    _ = unittest.main()
