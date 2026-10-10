"""test_gate 预算会计与真实模型授权边界回归单测.

覆盖两个已确认的预算缺陷；当前三级门共用非编译预算，真正编译独占区间扣除：

1. ``run_logged`` 的有限等待预算必须按绝对截止（started+timeout）计算，进程
   启动与 Job 绑定消耗的时间要从预算中扣除；
2. ``execute_gate`` 的非编译工作与最终汇总共用时钟；即使阶段报告成功，
   非编译墙钟越界也必须失败，不得报退出码 0。

确定性：用注入假时钟替代 ``time.monotonic``，不真实睡眠 61 秒；唯一真实进程
用 0.4s 睡眠命令，预算差额远大于进程启动抖动。日志及真实清理夹具仅写
.tmp/test-gate、.tmp/parallel-review 与 .tmp/acp-review；CI 身份及默认等待使用模拟响应。

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

from scripts import gate_runtime, local_gate_plan, make_tmp, test_gate
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
        with tempfile.TemporaryDirectory(dir=workspace) as temporary:
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
    """run_logged 必须把进程启动/Job 绑定耗时计入阶段预算（绝对截止）。."""

    def test_setup_time_is_charged_against_stage_timeout(self) -> None:
        # 假时钟：started=100.0；等待预算计算点已到 101.85（启动消耗 1.85s），
        # 清理读数点 101.95。阶段预算 2.0s → 修复后等待预算只剩 0.15s。
        clock = _FakeMonotonic([100.0, 101.85, 101.95])
        # 真实命令睡眠 0.4s（加解释器启动 ≈0.5s）：远大于修复后的 0.15s 等待预算
        # （必须 TIMEOUT），又远小于修复前固定的 1.5s 等待预算（修复前会 PASS）。
        argv = [sys.executable, "-c", "import time; time.sleep(0.4)"]
        with patch.object(test_gate, "_monotonic", clock):
            result = test_gate.run_logged("budget-setup-regression", argv, timeout=2.0)
        assert result.status == test_gate.STATUS_TIMED_OUT  # nosec B101: 回归断言，不用于产品输入校验。
        assert "总预算" in result.detail  # nosec B101: 回归断言，不用于产品输入校验。


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


if __name__ == "__main__":
    _ = unittest.main()
