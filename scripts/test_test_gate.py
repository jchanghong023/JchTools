"""test_gate 预算会计与真实模型授权边界回归单测.

覆盖两个已确认的预算缺陷；当前三级门共用非编译预算，真正编译独占区间扣除：

1. ``run_logged`` 的有限等待预算必须按绝对截止（started+timeout）计算，进程
   启动与 Job 绑定消耗的时间要从预算中扣除；
2. ``execute_gate`` 的非编译工作与最终汇总共用时钟；即使阶段报告成功，
   非编译墙钟越界也必须失败，不得报退出码 0。

确定性：用注入假时钟替代 ``time.monotonic``，不真实睡眠 61 秒；真实预算进程
无限等待直至被终止，并直接核对传入真实 wait 的剩余预算，避免宿主调度改变结论。
日志及真实清理夹具仅写 .tmp/test-gate、.tmp/parallel-review 与 .tmp/acp-review。

完整门授权边界仅调用真实计划器；合成 Cargo artifact 日志交由生产解析器读取，
计划替身只记录原语，预算与 Job 自检仅监督单条隔离命令，不执行完整门、远程或模型调用。
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


class GateToolContentIdentityTests(unittest.TestCase):
    def test_python_package_mutation_invalidates_identity_with_unchanged_metadata(self) -> None:
        """覆盖 P-12：安装包版本与 RECORD 不变时，实际工具源码变化仍须使缓存失效。"""
        workspace = test_gate.ROOT / ".tmp" / "tool-content-identity"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        sysroot = Path(subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, check=True).stdout.strip())
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
                before = bazel_gate._tool_identity(sysroot)
                source.write_text("def result(): return 'new'\n", encoding="utf-8")
                after = bazel_gate._tool_identity(sysroot)
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
            result = bazel_gate._native_result(
                gate_runtime.Stage("native-timeout", [sys.executable]),
                {"log": str(log)},
                {"result": {"status": "FAILED_TO_BUILD"}, "summary": {"overallStatus": "TIMEOUT"}},
                root / "native.bep.jsonl",
                timed_out=False,
            )
            self.assertEqual(result.status, "TIMEOUT")


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

    def test_gate_cleanup_removes_verbatim_dot_and_space_directories(self) -> None:
        # 覆盖 P-13 / AH-04：真实 ACP 用例可留下合法的尾点、尾空格目录，门必须完整回收。
        workspace = test_gate.ROOT / ".tmp" / "parallel-review"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        run = gate_runtime.GateRun(
            "fastcheck",
            60.0,
            test_gate.ROOT,
            workspace / "native-cleanup-logs",
            start_command=test_gate._start_owned_command,
            close_command=test_gate._close_owned_command,
            kill_tree=test_gate._kill_tree,
            compiler_ids=test_gate._compiler_ids,
        )
        run.prepare_temporary()
        self.assertIsNotNone(run._temporary)
        name = run._temporary.name
        if name.startswith("\\\\?\\"):
            native = Path(name)
        elif name.startswith("\\\\"):
            native = Path("\\\\?\\UNC\\" + name[2:])
        else:
            native = Path("\\\\?\\" + name)
        try:
            for name in ("caller-dot.", "caller-space ", "caller-dot", "caller-space"):
                directory = native / name
                directory.mkdir()
                payload = directory / "payload.txt"
                payload.write_bytes(name.encode("utf-8"))
                self.assertEqual(payload.read_bytes(), name.encode("utf-8"))
            produced = gate_runtime.Result("native-fixture", gate_runtime.STATUS_OK, "真实特殊目录已写入并读回校验")
            self.assertEqual(run.finish([produced]), 0, "合法 verbatim 测试产物不能使完整门在收尾失败")
            self.assertFalse(native.exists(), "门报告通过后仍有自身测试目录残留")
        finally:
            if native.exists():
                shutil.rmtree(native)
            if run._temporary is not None:
                run._temporary.cleanup()


class _PlanningGateRun:
    """只接收真实计划器的阶段，不执行 argv，也不伪造模型响应。."""

    def __init__(self, log_dir: Path, logs: dict[str, Path]) -> None:
        self.log_dir: Path = log_dir
        self.logs: dict[str, Path] = logs
        self.stages: list[gate_runtime.Stage] = []
        self.batches: list[list[gate_runtime.Stage]] = []

    def run(self, stages: list[gate_runtime.Stage], *, parallel: bool = True) -> list[gate_runtime.Result]:
        self.batches.append(list(stages))
        self.stages.extend(stages)
        for stage in stages:
            if stage.name.endswith("-list"):
                log = self.log_dir / f"{stage.name}.log"
                if stage.argv[1:3] == ["-m", "scripts.gate_unittest"]:
                    listing = 'JCHTOOLS_TEST_CASES=["planning.case"]\n'
                else:
                    listing = "ignored_case: test\n"
                    if not stage.name.endswith("-ignored-list"):
                        listing = "ordinary_case: test\n" + listing
                _ = log.write_text(listing, encoding="utf-8")
                self.logs[stage.name] = log
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
    """覆盖 P-12/P-15：功能与打包门分层不授予真实模型调用权限。."""

    def _assert_separate_model_authorization(
        self,
        installation: str,
        *,
        levels: tuple[str, ...] = ("fulltest", "slowtest"),
        installer_available: bool = True,
    ) -> None:
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
            # full 不需要安装编译器；slow 的有效完整计划仍要求保留该前置。
            if installer_available:
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
            # 走真实 _powershell_env 的 GUI 后备选择，不能从 package.env 偷取 broker。
            environment["JCHTOOLS_TEST_GUI_EXE"] = environment.pop("JCHTOOLS_TEST_BROKER_EXE")
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
                "root-test-build": tuple((target, "test") for target in root_targets)
                + (("jchtools_recursive_extract", "lib"),),
                "snap-ocr-core-test-build": (("snap_ocr_core", "test"),),
                "snap-ocr-worker-test-build": (("snap_ocr_worker", "test"),),
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
                                "target": {"name": target, "kind": [kind]},
                                "executable": str(root / "artifacts" / f"{target}.exe"),
                            }
                        )
                        for target, kind in targets
                    ),
                    encoding="utf-8",
                )
                logs[build] = log
            snapshot_log = log_dir / "source-snapshot.log"
            _ = snapshot_log.write_text("synthetic source snapshot\n", encoding="utf-8")
            logs["source-snapshot"] = snapshot_log
            executed_by_level: dict[str, list[str]] = {}
            with (
                patch.dict(os.environ, environment, clear=True),
                patch("subprocess.run", side_effect=AssertionError("计划回归不得执行进程")),
                patch("subprocess.Popen", side_effect=AssertionError("计划回归不得启动进程")),
                patch.object(test_gate, "ROOT", root),
            ):
                for level in levels:
                    with self.subTest(installation=installation, level=level):
                        run = _PlanningGateRun(log_dir, logs)
                        plan = test_gate._gate_plan(level, log_dir)
                        results: list[gate_runtime.Result] = []
                        snapshot: list[str] = []
                        test_gate._execute_bazel_plan(
                            cast("BazelSession", cast("object", run)), level, plan, results, snapshot
                        )
                        names = [stage.name for stage in run.stages]
                        executed_by_level[level] = names
                        self.assertEqual(len(names), len(set(names)), "完整功能阶段不得重复执行")
                        packaging = {
                            "release-target-build",
                            "package-windows-unit",
                            "fetch-7zip-unit",
                            "package",
                        }
                        self.assertEqual("pyquality-pip-audit" in names, level == "slowtest")
                        if level == "slowtest":
                            audit = next(stage for stage in run.stages if stage.name == "pyquality-pip-audit")
                            self.assertEqual(
                                audit.argv,
                                [sys.executable, "-m", "pip_audit", "-r", "scripts/requirements-dev.txt"],
                            )
                        if level == "fulltest":
                            self.assertTrue(packaging.isdisjoint(stage.name for stage in plan))
                            self.assertTrue(packaging.isdisjoint(names))
                            self.assertFalse(
                                any(
                                    "package-windows.ps1" in argument for stage in run.stages for argument in stage.argv
                                )
                            )
                        else:
                            for name in packaging:
                                self.assertEqual(
                                    names.count(f"{name}-planning.case" if name.endswith("-unit") else name), 1
                                )
                            package = next(stage for stage in run.stages if stage.name == "package")
                            self.assertIn(str(root / "scripts/package-windows.ps1"), package.argv)
                            release = next(stage for stage in run.stages if stage.name == "release-target-build")
                            self.assertEqual(release.argv[1:], ["build", "--all-targets", "--release"])
                        media = [
                            stage
                            for stage in run.stages
                            if stage.name.startswith("real-media-e2e-") and "--list" not in stage.argv
                        ]
                        self.assertTrue(media)
                        for stage in media:
                            self.assertEqual(
                                (stage.env or {}).get("JCHTOOLS_TEST_BROKER_EXE"),
                                environment["JCHTOOLS_TEST_GUI_EXE"],
                            )
                            self.assertEqual(stage.exclusive_resource, "shared-xberg-media")
                        model_binary = str(root / "artifacts" / "acp_opencode.exe")
                        unauthorized = [
                            stage.name
                            for stage in run.stages
                            if stage.argv[0] == model_binary
                            and "--ignored" in stage.argv
                            and "--list" not in stage.argv
                        ]
                        assert unauthorized == [], "普通完整门不能自行取得真实模型调用授权"  # nosec B101: 授权边界回归。
                        model_results = [result for result in results if result.name == "real-opencode"]
                        assert len(model_results) == 1  # nosec B101: 真实模型验收必须独立报告。
                        assert (  # nosec B101: 不得把未授权模型调用冒报 PASS。
                            model_results[0].status == "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"
                        )
            if "fulltest" in executed_by_level and "slowtest" in executed_by_level:
                slow_functional = [
                    name
                    for name in executed_by_level["slowtest"]
                    if name not in {"release-target-build", "package", "pyquality-pip-audit"}
                    and not name.startswith(("package-windows-unit-", "fetch-7zip-unit-"))
                ]
                self.assertEqual(executed_by_level["fulltest"], slow_functional)

    def test_full_without_iscc_completes_non_packaging_plan(self) -> None:
        """覆盖 P-12/P-15：fulltest 不要求 ISCC，且不触发发布构建或打包。."""

        def reject_installer_lookup(command: str) -> str:
            if command == "ISCC.exe":
                self.fail("fulltest 不得检查安装编译器")
            return command

        with patch.object(local_gate_plan.shutil, "which", side_effect=reject_installer_lookup):
            self._assert_separate_model_authorization("absent", levels=("fulltest",), installer_available=False)

    def test_explicit_opencode_exe_does_not_authorize_model_calls(self) -> None:
        self._assert_separate_model_authorization("explicit")

    def test_path_native_opencode_exe_does_not_authorize_model_calls(self) -> None:
        self._assert_separate_model_authorization("path")

    def test_npm_native_opencode_exe_does_not_authorize_model_calls(self) -> None:
        self._assert_separate_model_authorization("npm")

    def test_no_opencode_installation_still_reports_separate_authorization(self) -> None:
        self._assert_separate_model_authorization("absent")


class OwnedCommandLaunchCancellationTests(unittest.TestCase):
    """覆盖 AGENTS.md 3.4：绑定 Job 后的启动取消不能遗留暂停进程或 Job。"""

    def test_resume_interrupt_reaps_process_and_closes_owned_job(self) -> None:
        process = MagicMock(spec=subprocess.Popen)
        process.pid = 987654321
        owner = MagicMock()
        with (
            patch.object(test_gate.subprocess, "Popen", return_value=process),
            patch.object(test_gate, "_CommandJob", return_value=owner),
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
        request = self.root / "001.request.json"
        pending = request.with_suffix(".pending")
        sidecar = self.root / _label("consumer").split(":")[0].removeprefix("//") / "spec.runtime.json"
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
        _ = pending.write_text(json.dumps({"argv": argv, "runtime": str(runtime)}), encoding="utf-8")
        self.assertFalse(request.exists(), "pending 不应向消费者发布")
        _ = pending.replace(request)
        self._hold_shared_delete(request)
        self._hold_shared_delete(runtime)
        with self.assertRaises(PermissionError):
            _ = request.read_text(encoding="utf-8")
        dispatch = cast("Callable[[str], int]", vars(bazel_gate)["_dispatch_main"])
        failure = self.root / "first-failure.txt"
        with patch.dict(
            os.environ,
            {
                "JCHTOOLS_GATE_FAILURE_FILE": str(failure),
                "TEMP": str(self.root),
                "TMP": str(self.root),
            },
        ):
            self.assertEqual(dispatch(str(request)), 7, "必须保留真实消费者非零退出码")
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
        request = self.root / "001.request.json"
        dispatch = cast("Callable[[str], int]", vars(bazel_gate)["_dispatch_main"])
        valid_request = json.dumps({"argv": ["never-launched"], "runtime": str(runtime)}).encode("utf-8")
        for request_data, runtime_data in ((b"{", b"{}"), (valid_request, b"{"), (valid_request, b"\xff")):
            with self.subTest(request=request_data, runtime=runtime_data):
                _ = request.write_bytes(request_data)
                _ = runtime.write_bytes(runtime_data)
                with (
                    patch.object(bazel_gate.subprocess, "call") as command,
                    self.assertRaises((ValueError, UnicodeError)),
                ):
                    _ = dispatch(str(request))
                command.assert_not_called()

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


class GateNativeExternalCacheTests(unittest.TestCase):
    """真实持久 action cache 不得跳过每轮外部状态检查。"""

    def test_external_command_executes_each_session_while_declared_check_is_cached(self) -> None:
        # Bazel 外部仓库含长路径夹具；缩短独占工作根，不改变生产缓存位置。
        workspace = test_gate.ROOT / ".tmp"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        with contextlib.ExitStack() as cleanup:
            root = Path(tempfile.mkdtemp(prefix="n", dir=workspace))
            cleanup.callback(make_tmp.robust_rmtree, root)
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

            def stop_fixture() -> None:
                if (root / ".tmp/bazel-gate/service/identity.json").exists():
                    bazel_service.stop(root)

            cleanup.callback(stop_fixture)
            cleanup.push(preserve_failure)
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
                    spec = root / _label(name).split(":")[0].removeprefix("//") / "spec.json"
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
                    results.extend(
                        run.supervise(
                            gate_runtime.Stage(
                                name,
                                [sys.executable, "-m", "scripts.bazel_gate", "--dispatch", str(config)],
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

            while True:
                for request in sorted(root.glob("*.request.json")):
                    if request.name in seen:
                        continue
                    command = json.loads(request.read_text(encoding="utf-8"))
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
                response = cast(
                    "dict[str, object]",
                    json.loads(request.with_suffix(".response.json").read_text(encoding="utf-8")),
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
        with patch.dict(os.environ, {}, clear=True), patch.object(bazel_gate, "_doc_cache_safe", return_value=True):
            self.assertTrue(bazel_gate._doctest(stage))
            self.assertTrue(bazel_gate._reuse(stage))
            with patch.dict(os.environ, {"RUSTDOC": "external-rustdoc.exe"}):
                self.assertFalse(bazel_gate._reuse(stage))
        self.assertFalse(bazel_gate._doctest(dataclasses.replace(stage, name="unknown-doctests")))
        self.assertFalse(bazel_gate._doctest(dataclasses.replace(stage, argv=[*stage.argv, "--ignored"])))

    def test_doctest_cache_allows_only_scheduling_and_progress_cargo_overrides(self) -> None:
        stage = gate_runtime.Stage(
            "root-doctests",
            ["cargo", "test", "--doc", "--features", "test-hooks", "--"],
            "compiler-observed",
        )
        allowed = {"CARGO_BUILD_JOBS": "28", "CARGO_TERM_PROGRESS_WHEN": "never"}
        with patch.dict(os.environ, {}, clear=True), patch.object(bazel_gate, "_doc_cache_safe", return_value=True):
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
                self.assertEqual(bazel_gate._doc_cache_safe(Path("synthetic")), safe)

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
        spec = test_gate.ROOT / _label("controller-fixture").split(":")[0].removeprefix("//") / "spec.json"
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
                    patch.object(test_gate, "_query_process_image", side_effect=image),
                    patch.object(test_gate, "_get_process_id", side_effect=lambda handle: handle),
                    patch.object(test_gate, "_process_argv", return_value=argv),
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
                    patch.object(test_gate, "_query_process_image", side_effect=image),
                    patch.object(test_gate, "_get_process_id", side_effect=lambda handle: handle),
                    patch.object(test_gate.win32api, "CloseHandle"),
                ):
                    self.assertEqual(test_gate._compiler_ids(process), frozenset())


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


class GateCachedPayloadTests(unittest.TestCase):
    """缓存结果必须恢复原始数据；封套或损坏输出不能冒充通过。"""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "cached-payload"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def _frame(self, body: bytes) -> bytes:
        raw = self.root / "raw.log"
        raw.write_bytes(body)
        output = io.BytesIO()
        with patch.object(sys, "stdout", MagicMock(buffer=output)):
            self.assertEqual(bazel_action._publish_log(raw, None, 0), 0)
        return b"Executing tests from //fixture:target\r\n------------------\r\n" + output.getvalue()

    def _consume(self, frame: bytes) -> gate_runtime.Result:
        native = self.root / "test.log"
        native.write_bytes(frame)
        return bazel_gate._native_result(
            gate_runtime.Stage("cached-payload", [sys.executable]),
            {
                "log": str(self.root / "consumer.log"),
                "environment": {},
                "result_file": str(self.root / "result"),
                "TEMP": None,
                "TMP": None,
            },
            {
                "result": {
                    "status": "PASSED",
                    "cachedLocally": True,
                    "testActionOutput": [{"name": "test.log", "uri": native.as_uri()}],
                },
            },
            self.root / "evidence.jsonl",
            timed_out=False,
        )

    def test_cached_listings_restore_exact_bytes_including_empty_and_binary(self) -> None:
        for body in (b"", b"alpha: test\r\nbeta: benchmark\n", b"no-final-newline\x00\xff"):
            with self.subTest(body=body):
                (self.root / "consumer.log").write_bytes(b"stale host data")
                result = self._consume(self._frame(body))
                self.assertEqual(result.status, "PASS")
                self.assertEqual((self.root / "consumer.log").read_bytes(), body)

    def test_damaged_cached_payload_never_passes_even_with_existing_host_log(self) -> None:
        frame = self._frame(b"alpha: test\n")
        for broken in (
            b"wrapper only",
            frame[:-1],
            frame + b"junk",
            frame.replace(b"alpha", b"omega", 1),
            frame.replace(b"PAYLOAD_V1", b"PAYLOAD_V9", 1),
        ):
            with self.subTest(frame=broken):
                (self.root / "consumer.log").write_bytes(b"stale host data")
                result = self._consume(broken)
                self.assertNotEqual(result.status, "PASS")
                self.assertNotEqual(result.returncode, 0)


class GateUnittestCasesTests(unittest.TestCase):
    """逐 ID 协议只加载；精确执行保留原始实例、夹具和 subTest。"""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "test-gate" / "unittest-cases"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        scripts = self.root / "scripts"
        scripts.mkdir()
        _ = shutil.copyfile(test_gate.ROOT / "scripts" / "make_tmp.py", scripts / "make_tmp.py")

    def _invoke(self, body: str, *arguments: str) -> subprocess.CompletedProcess[str]:
        _ = (self.root / "case_fixture.py").write_text(
            "import unittest\nfrom pathlib import Path\nROOT = Path(__file__).resolve().parent\n"
            + textwrap.dedent(body),
            encoding="utf-8",
        )
        bootstrap = textwrap.dedent(
            """
            import sys
            from pathlib import Path
            from scripts import gate_unittest, test_reports
            root = Path(sys.argv[1])
            sys.path.insert(0, str(root))
            sys.dont_write_bytecode = True
            test_reports.ROOT = root
            test_reports._source_identity = lambda: {}
            raise SystemExit(gate_unittest.main(sys.argv[2:]))
            """
        )
        environment = dict(os.environ)
        environment["JCHTOOLS_GATE_FAILURE_FILE"] = str(self.root / "failure.jsonl")
        return subprocess.run(
            [sys.executable, "-c", bootstrap, str(self.root), *arguments],
            cwd=test_gate.ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=15,
            check=False,
        )

    def test_list_does_not_execute_fixtures_or_subtests_and_deduplicates_ids(self) -> None:
        result = self._invoke(
            """
            def setUpModule():
                raise AssertionError("must not run module setup")
            class Cases(unittest.TestCase):
                @classmethod
                def setUpClass(cls):
                    raise AssertionError("must not run class setup")
                def setUp(self):
                    raise AssertionError("must not run setup")
                def test_parent(self):
                    for item in range(2):
                        with self.subTest(item=item):
                            self.fail("must not run test")
                def tearDown(self):
                    raise AssertionError("must not run teardown")
            def load_tests(loader, tests, pattern):
                return unittest.TestSuite([tests, Cases("test_parent")])
            """,
            "--list",
            "--",
            "case_fixture",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout.splitlines()[-1].removeprefix("JCHTOOLS_TEST_CASES=")),
            ["case_fixture.Cases.test_parent"],
        )
        self.assertFalse((self.root / "failure.jsonl").exists())
        self.assertEqual(list((self.root / ".tmp").glob("test-reports/*/run.json")), [])

    def test_exact_case_retains_duplicates_subtests_and_real_fixtures(self) -> None:
        result = self._invoke(
            """
            def record(value):
                with (ROOT / "events").open("a", encoding="utf-8") as stream:
                    stream.write(value + "\\n")
            def setUpModule():
                record("module setup")
            def tearDownModule():
                record("module teardown")
            class Cases(unittest.TestCase):
                @classmethod
                def setUpClass(cls):
                    record("class setup")
                @classmethod
                def tearDownClass(cls):
                    record("class teardown")
                def setUp(self):
                    record("setup")
                def tearDown(self):
                    record("teardown")
                def test_parent(self):
                    for item in range(2):
                        with self.subTest(item=item):
                            record(str(item))
                            self.assertLess(item, 2)
                def test_parent_extra(self):
                    self.fail("prefix matching must not execute this")
            def load_tests(loader, tests, pattern):
                return unittest.TestSuite([tests, Cases("test_parent")])
            """,
            "--case",
            "case_fixture.Cases.test_parent",
            "--",
            "-f",
            "case_fixture",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.root / "events").read_text(encoding="utf-8").splitlines(),
            [
                "module setup",
                "class setup",
                "setup",
                "0",
                "1",
                "teardown",
                "setup",
                "0",
                "1",
                "teardown",
                "class teardown",
                "module teardown",
            ],
        )
        reports = list((self.root / ".tmp" / "test-reports").glob("*/run.json"))
        self.assertEqual(len(reports), 1)
        report = json.loads(reports[0].read_text(encoding="utf-8"))
        self.assertEqual(report["coverage"]["native_unittest"]["tests_run"], 2)
        self.assertEqual(report["coverage"]["completed_logical_cases"], 2)
        self.assertEqual(
            report["command"],
            [
                sys.executable,
                "-m",
                "scripts.gate_unittest",
                "--case",
                "case_fixture.Cases.test_parent",
                "--",
                "-f",
                "case_fixture",
            ],
        )

    def test_loading_errors_never_emit_valid_list_and_notify_failure(self) -> None:
        for body in (
            'raise ImportError("broken-import-marker")',
            "def load_tests(loader, tests, pattern):\n    raise RuntimeError('broken-load-marker')",
        ):
            with self.subTest(body=body):
                result = self._invoke(body, "--list", "--", "case_fixture")
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("JCHTOOLS_TEST_CASES=", result.stdout)
                signal = self.root / "failure.jsonl"
                self.assertTrue(signal.exists())
                self.assertTrue(json.loads(signal.read_text(encoding="utf-8").splitlines()[0])["detail"])
                signal.unlink()

    def test_missing_exact_case_fails_instead_of_zero_case_pass(self) -> None:
        result = self._invoke(
            """
            class Cases(unittest.TestCase):
                def test_present(self):
                    (ROOT / "unexpected-case").touch()
                    self.fail("must not execute unrelated case")
            """,
            "--case",
            "case_fixture.Cases.test_missing",
            "--",
            "case_fixture",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.root / "failure.jsonl").exists())
        self.assertFalse((self.root / "unexpected-case").exists())


class CargoArtifactSnapshotTests(unittest.TestCase):
    """使用真实磁盘文件验证产物恢复；真实 Cargo/Bazel 跨变体由隔离原生回归覆盖。"""

    def setUp(self) -> None:
        workspace = test_gate.ROOT / ".tmp" / "cargo-artifact-unit"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            self.assertEqual(make_tmp.main(), 0)
        self.temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        with patch.object(
            sys,
            "argv",
            ["make_tmp.py", "workspace", "--destination", str(self.root / ".tmp/bazel-gate")],
        ):
            self.assertEqual(make_tmp.main(), 0)
        self.target = self.root / "target/debug"
        self.target.mkdir(parents=True)
        self.program = self.target / "fixture.exe"
        self.depfile = self.target / "fixture.d"
        self.dll = self.target / "runtime.dll"
        self.pdb = self.target / "fixture.pdb"
        self.expected = {
            self.program: b"synthetic executable bytes; not a product program",
            self.depfile: b"fixture.exe: src/main.rs\n",
            self.dll: b"synthetic runtime library",
            self.pdb: b"synthetic symbol data",
        }
        for path, payload in self.expected.items():
            _ = path.write_bytes(payload)
        self.log = self.root / "cargo.log"
        _ = self.log.write_text(
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"kind": ["bin"], "name": "fixture"},
                    "profile": {"test": False},
                    "executable": str(self.program),
                    "filenames": [str(self.program), str(self.pdb)],
                }
            )
            + "\n",
            encoding="utf-8",
        )
        self.directory = self.root / "snapshot.cargo"
        self.manifest = self.root / "snapshot.cargo.json"
        bazel_action.capture_cargo_artifacts(self.root, self.log, self.directory, self.manifest)

    def _rewrite_snapshot(
        self,
        entries: list[tuple[str, bytes]],
        files: list[dict[str, object]],
        *,
        refresh_member_digests: bool = False,
    ) -> None:
        payload = self.directory / "payload.zip"
        with zipfile.ZipFile(payload, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=1) as archive:
            for name, data in entries:
                info = bazel_action._cargo_zip_info(name)
                with archive.open(info, "w", force_zip64=True) as writer:
                    _ = writer.write(data)
                if refresh_member_digests:
                    for record in files:
                        if record["path"] == name:
                            record["member_sha256"] = bazel_action._cargo_member_digest(archive, info)
            archive.writestr("metadata.json", json.dumps({"version": 3, "files": files}, sort_keys=True))
        self.manifest.write_text(
            json.dumps(
                {
                    "version": 3,
                    "files": files,
                    "payload_sha256": hashlib.sha256(payload.read_bytes()).hexdigest(),
                }
            ),
            encoding="utf-8",
        )

    def _snapshot_entries(self) -> list[tuple[str, bytes]]:
        with zipfile.ZipFile(self.directory / "payload.zip") as archive:
            return [
                (info.filename, archive.read(info)) for info in archive.infolist() if info.filename != "metadata.json"
            ]

    def test_member_digest_survives_writer_utf_flag_normalization_and_zip64_headers(self) -> None:
        for name in ("target/debug/ascii.dll", "target/debug/运行.dll"):
            with self.subTest(name=name):
                buffer = io.BytesIO()
                with zipfile.ZipFile(buffer, "w") as archive:
                    info = bazel_action._cargo_zip_info(name)
                    with archive.open(info, "w", force_zip64=True) as writer:
                        _ = writer.write(b"member with a finalized ZIP64 local header")
                    # force_zip64 的本地尺寸槽始终为占位值，真实尺寸在 extra field。
                    self.assertEqual(buffer.getvalue()[18:26], b"\xff" * 8)
                    flags = info.flag_bits
                    try:
                        # 在旧 Python 上也复现 3.13 的 ASCII 写侧内存 UTF 位差异。
                        if name.isascii():
                            info.flag_bits |= 0x800
                        expected = bazel_action._cargo_member_digest(archive, info)
                    finally:
                        info.flag_bits = flags
                _ = buffer.seek(0)
                with zipfile.ZipFile(buffer) as archive:
                    persisted = archive.getinfo(name)
                    self.assertEqual(bazel_action._cargo_member_digest(archive, persisted), expected)

    def test_payload_is_single_deterministic_compressed_file(self) -> None:
        second = self.root / "second.cargo"
        second_manifest = self.root / "second.cargo.json"
        bazel_action.capture_cargo_artifacts(self.root, self.log, second, second_manifest)
        self.assertEqual([path.name for path in self.directory.iterdir()], ["payload.zip"])
        self.assertEqual((self.directory / "payload.zip").read_bytes(), (second / "payload.zip").read_bytes())
        self.assertEqual(self.manifest.read_bytes(), second_manifest.read_bytes())
        with zipfile.ZipFile(self.directory / "payload.zip") as archive:
            self.assertTrue(all(info.compress_type == zipfile.ZIP_DEFLATED for info in archive.infolist()))

    def _bundled_engine(self) -> Path:
        engine = self.root / "resources/7zip/7z.exe"
        engine.parent.mkdir(parents=True)
        _ = engine.write_bytes(b"isolated engine placeholder; subprocess is patched")
        _ = engine.with_name("7z.dll").write_bytes(b"isolated library placeholder")
        return engine

    def test_native_engine_failure_cannot_publish_or_fall_back(self) -> None:
        _ = self._bundled_engine()
        directory, manifest = self.root / "failed.cargo", self.root / "failed.cargo.json"
        with (
            patch.object(bazel_action.subprocess, "run", return_value=subprocess.CompletedProcess([], 2, b"failed")),
            self.assertRaises(OSError),
        ):
            bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
        self.assertFalse(manifest.exists())
        self.assertEqual(list(directory.iterdir()), [])

    def test_incomplete_bundled_engine_is_not_an_absent_engine_fallback(self) -> None:
        self._bundled_engine().with_name("7z.dll").unlink()
        manifest = self.root / "missing-library.cargo.json"
        with self.assertRaises(OSError):
            bazel_action.capture_cargo_artifacts(self.root, self.log, self.root / "missing-library.cargo", manifest)
        self.assertFalse(manifest.exists())

    def test_native_members_restore_stored_and_deflated_bytes_and_mtime(self) -> None:
        _ = self._bundled_engine()
        for compression in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
            with self.subTest(compression=compression):
                directory = self.root / f"native-{compression}.cargo"
                manifest = directory.with_suffix(".cargo.json")

                def compress(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[bytes]:
                    with zipfile.ZipFile(directory / "payload.zip", "w", compression=compression) as archive:
                        for name in Path(argv[-1][1:]).read_text(encoding="utf-8").splitlines():
                            archive.write(self.root / name, name)
                    return subprocess.CompletedProcess(argv, 0, b"")

                expected_times = {path: path.stat().st_mtime_ns for path in self.expected}
                with patch.object(bazel_action.subprocess, "run", side_effect=compress):
                    bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
                self.assertEqual([path.name for path in directory.iterdir()], ["payload.zip"])
                for path in self.expected:
                    path.unlink()
                bazel_action.restore_cargo_artifacts(self.root, directory, manifest)
                for path, data in self.expected.items():
                    self.assertEqual(path.read_bytes(), data)
                    self.assertEqual(path.stat().st_mtime_ns, expected_times[path])

    def test_native_missing_extra_directory_and_corrupt_members_cannot_publish(self) -> None:
        _ = self._bundled_engine()
        for damage in ("missing", "extra", "directory", "bytes", "method"):
            with self.subTest(damage=damage):
                directory = self.root / f"native-{damage}.cargo"
                manifest = directory.with_suffix(".cargo.json")

                def compress(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[bytes]:
                    names = Path(argv[-1][1:]).read_text(encoding="utf-8").splitlines()
                    method = zipfile.ZIP_BZIP2 if damage == "method" else zipfile.ZIP_DEFLATED
                    with zipfile.ZipFile(directory / "payload.zip", "w", compression=method) as archive:
                        for name in names[1:] if damage == "missing" else names:
                            data = (self.root / name).read_bytes()
                            archive.writestr(name, b"corrupt" if damage == "bytes" else data)
                        if damage in {"extra", "directory"}:
                            archive.writestr("unexpected/" if damage == "directory" else "unexpected.dll", b"")
                    return subprocess.CompletedProcess(argv, 0, b"")

                with (
                    patch.object(bazel_action.subprocess, "run", side_effect=compress),
                    self.assertRaises(ValueError),
                ):
                    bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
                self.assertFalse(manifest.exists())
                self.assertFalse((directory / "cargo-files.txt").exists())
                for path, data in self.expected.items():
                    self.assertEqual(path.read_bytes(), data)

    def test_equal_consumers_do_not_inflate_runtime_members(self) -> None:
        original = zipfile.ZipExtFile.read

        def read_metadata(member: zipfile.ZipExtFile, *args: object, **kwargs: object) -> object:
            self.assertEqual(member.name, "metadata.json", "匹配消费者必须只解压小型内嵌文件表")
            return original(member, *args, **kwargs)

        with patch.object(zipfile.ZipExtFile, "read", autospec=True, side_effect=read_metadata):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_equal_consumers_still_require_full_payload_hash(self) -> None:
        payload = self.directory / "payload.zip"
        original = payload.read_bytes()
        payload.write_bytes(original[:-1] + bytes([original[-1] ^ 1]))
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_outer_file_table_cannot_claim_different_consumer_bytes(self) -> None:
        snapshot = json.loads(self.manifest.read_text(encoding="utf-8"))
        record = next(
            record for record in snapshot["files"] if record["path"] == self.program.relative_to(self.root).as_posix()
        )
        current = b"different consumer with altered outer metadata"
        self.program.write_bytes(current)
        record["sha256"] = hashlib.sha256(current).hexdigest()
        record["size"] = len(current)
        self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), current)

    def test_archive_member_missing_duplicate_and_size_mismatch_fail_closed(self) -> None:
        entries = self._snapshot_entries()
        files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        variants = (
            entries[:-1],
            [*entries, entries[0]],
            [(name, data + b"extra") if index == 0 else (name, data) for index, (name, data) in enumerate(entries)],
        )
        for index, changed in enumerate(variants):
            with self.subTest(variant=index):
                if index == 1:
                    with self.assertWarns(UserWarning):
                        self._rewrite_snapshot(changed, files)
                else:
                    self._rewrite_snapshot(changed, files)
                self.program.write_bytes(b"current consumer")
                self.depfile.write_bytes(b"current dep-info")
                with self.assertRaises(ValueError):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(self.program.read_bytes(), b"current consumer")
                self.assertEqual(self.depfile.read_bytes(), b"current dep-info")

    def test_bound_but_incorrect_file_digest_rejects_before_consumer_writes(self) -> None:
        entries = self._snapshot_entries()
        files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        # 重签整个 payload 不得掩盖匹配消费者对应的坏成员；内嵌成员绑定仍是原值。
        name, data = entries[-1]
        entries[-1] = (name, bytes(byte ^ 1 for byte in data))
        self._rewrite_snapshot(entries, files)
        self.program.write_bytes(b"current consumer")
        self.depfile.write_bytes(b"current dep-info")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")
        self.assertEqual(self.depfile.read_bytes(), b"current dep-info")

    def test_refreshed_member_binding_still_checks_restored_uncompressed_digest(self) -> None:
        entries = self._snapshot_entries()
        files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        name, data = entries[-1]
        entries[-1] = (name, bytes(byte ^ 1 for byte in data))
        self._rewrite_snapshot(entries, files, refresh_member_digests=True)
        # 已重新绑定的压缩流仍不能恢复与文件 SHA 不符的字节。
        for path in self.expected:
            path.write_bytes(b"current consumer")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path in self.expected:
            self.assertEqual(path.read_bytes(), b"current consumer")

    def test_equal_consumers_reject_rebound_bad_member_without_inflating(self) -> None:
        entries = self._snapshot_entries()
        files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        name, data = entries[-1]
        entries[-1] = (name, bytes(byte ^ 1 for byte in data))
        self._rewrite_snapshot(entries, files)
        original = zipfile.ZipExtFile.read

        def read_metadata(member: zipfile.ZipExtFile, *args: object, **kwargs: object) -> object:
            self.assertEqual(member.name, "metadata.json")
            return original(member, *args, **kwargs)

        with (
            patch.object(zipfile.ZipExtFile, "read", autospec=True, side_effect=read_metadata),
            self.assertRaises(ValueError),
        ):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_bound_metadata_rejects_invalid_schema_paths_digests_and_size_bounds(self) -> None:
        entries = self._snapshot_entries()
        original_files = json.loads(self.manifest.read_text(encoding="utf-8"))["files"]
        for key, invalid in (
            ("path", "../outside.exe"),
            ("path", "target/debug/../outside.exe"),
            ("path", "target\\debug\\outside.exe"),
            ("path", "src/main.rs"),
            ("sha256", "g" * 64),
            ("sha256", "0" * 63),
            ("member_sha256", "g" * 64),
            ("member_sha256", "0" * 63),
            ("size", -1),
            ("size", 1024**4 + 1),
            ("size", True),
            ("mtime_ns", -1),
        ):
            with self.subTest(key=key, invalid=invalid):
                files = [dict(record) for record in original_files]
                files[0][key] = invalid
                changed = [
                    (str(invalid), data) if key == "path" and index == 0 else (name, data)
                    for index, (name, data) in enumerate(entries)
                ]
                self._rewrite_snapshot(changed, files)
                self.program.write_bytes(b"current consumer")
                with self.assertRaises((TypeError, ValueError)):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(self.program.read_bytes(), b"current consumer")

    def test_original_v1_v2_and_noninteger_versions_are_rejected(self) -> None:
        original = self.manifest.read_text(encoding="utf-8")
        for version in (1, 2, True, 3.0):
            with self.subTest(version=version):
                snapshot = json.loads(original)
                snapshot["version"] = version
                self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
                self.program.write_bytes(b"current consumer")
                with self.assertRaises(ValueError):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(self.program.read_bytes(), b"current consumer")

    def test_duplicate_json_fields_are_rejected(self) -> None:
        original = self.manifest.read_text(encoding="utf-8")
        duplicate = '{"version": 3,' + original.lstrip()[1:]
        self.manifest.write_text(duplicate, encoding="utf-8")
        self.program.write_bytes(b"current consumer")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")

    def test_restoration_checks_archive_crc_before_consumer_writes(self) -> None:
        payload = self.directory / "payload.zip"
        data = bytearray(payload.read_bytes())
        with zipfile.ZipFile(payload) as archive:
            # 只破坏首个 central directory CRC，保留合法压缩流/文件 SHA/内嵌文件表。
            central = archive.start_dir
        data[central + 16] ^= 1
        payload.write_bytes(data)
        snapshot = json.loads(self.manifest.read_text(encoding="utf-8"))
        snapshot["payload_sha256"] = hashlib.sha256(data).hexdigest()
        self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
        self.program.write_bytes(b"current consumer")
        self.depfile.write_bytes(b"current dep-info")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")
        self.assertEqual(self.depfile.read_bytes(), b"current dep-info")
        self.assertEqual(list((self.root / ".tmp/bazel-gate").glob("cargo-restore-*")), [])

    def test_central_flags_remain_bound_after_writer_flag_normalization(self) -> None:
        payload = self.directory / "payload.zip"
        data = bytearray(payload.read_bytes())
        with zipfile.ZipFile(payload) as archive:
            central = archive.start_dir
        # ASCII 名称的 central UTF 位可以独立改变且仍能解析，不能忽略其篡改。
        data[central + 9] ^= 0x08
        payload.write_bytes(data)
        snapshot = json.loads(self.manifest.read_text(encoding="utf-8"))
        snapshot["payload_sha256"] = hashlib.sha256(data).hexdigest()
        self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            self.assertEqual(path.read_bytes(), expected)

    def test_existing_read_only_consumer_is_replaced_with_writable_bytes(self) -> None:
        self.program.write_bytes(b"readonly previous variant")
        self.program.chmod(self.program.stat().st_mode & ~(stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH))
        try:
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
            self.assertEqual(self.program.read_bytes(), self.expected[self.program])
            self.assertTrue(self.program.stat().st_mode & stat.S_IWUSR)
            self.program.write_bytes(b"next Cargo build")
        finally:
            self.program.chmod(self.program.stat().st_mode | stat.S_IWUSR)

    def test_missing_or_malformed_manifest_never_restores_consumers(self) -> None:
        original = self.manifest.read_bytes()
        self.manifest.unlink()
        _ = self.program.write_bytes(b"current consumer")
        with self.assertRaises(OSError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")
        _ = self.manifest.write_bytes(b'{"version": 3, "files":')
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer")
        _ = self.manifest.write_bytes(original)

    def test_changed_and_missing_consumers_restore_exact_snapshot_bytes(self) -> None:
        _ = self.program.write_bytes(b"overwritten by another build variant")
        self.depfile.unlink()
        _ = self.dll.write_bytes(b"changed runtime DLL")
        self.pdb.unlink()
        bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        for path, expected in self.expected.items():
            with self.subTest(path=path):
                self.assertEqual(path.read_bytes(), expected)

    def test_equal_bytes_preserve_existing_consumer_timestamp(self) -> None:
        os.utime(self.program, ns=(1_000_000_000, 1_000_000_000))
        before = self.program.stat().st_mtime_ns
        bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), self.expected[self.program])
        self.assertEqual(self.program.stat().st_mtime_ns, before)

    def test_read_only_snapshot_restores_writable_consumer_without_changing_source(self) -> None:
        cached_program = self.directory / "payload.zip"
        original_mode = cached_program.stat().st_mode
        cached_program.chmod(original_mode & ~(stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH))
        read_only_mode = cached_program.stat().st_mode
        source_mtime = self.program.stat().st_mtime_ns
        original_payload = cached_program.read_bytes()
        self.program.unlink()
        try:
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
            self.assertEqual(self.program.read_bytes(), self.expected[self.program])
            self.assertTrue(self.program.stat().st_mode & stat.S_IWUSR)
            self.assertEqual(self.program.stat().st_mtime_ns, source_mtime)
            _ = self.program.write_bytes(b"next Cargo build can overwrite restored output")
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
            self.assertEqual(self.program.read_bytes(), self.expected[self.program])
            _ = self.program.write_bytes(b"another build can still overwrite restored output")
            self.assertEqual(cached_program.stat().st_mode, read_only_mode)
            self.assertEqual(cached_program.read_bytes(), original_payload)
        finally:
            cached_program.chmod(original_mode)

    def test_missing_snapshot_file_rejects_without_partial_restoration(self) -> None:
        cached_program = self.directory / "payload.zip"
        cached_program.unlink()
        _ = self.program.write_bytes(b"current consumer remains untouched")
        _ = self.depfile.write_bytes(b"current dep-info remains untouched")
        with self.assertRaises((OSError, ValueError)):
            bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
        self.assertEqual(self.program.read_bytes(), b"current consumer remains untouched")
        self.assertEqual(self.depfile.read_bytes(), b"current dep-info remains untouched")

    def test_corrupted_snapshot_rejects_length_and_digest_mismatches(self) -> None:
        cached_program = self.directory / "payload.zip"
        expected = cached_program.read_bytes()
        for corrupted in (b"truncated", bytes(byte ^ 1 for byte in expected)):
            with self.subTest(length=len(corrupted)):
                _ = cached_program.write_bytes(corrupted)
                _ = self.program.write_bytes(b"current consumer")
                _ = self.depfile.write_bytes(b"current dep-info")
                with self.assertRaises(ValueError):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(self.program.read_bytes(), b"current consumer")
                self.assertEqual(self.depfile.read_bytes(), b"current dep-info")
        _ = cached_program.write_bytes(expected)

    def test_manifest_path_escape_cannot_overwrite_real_outside_file(self) -> None:
        outside = self.root.parent / (self.root.name + "-outside.exe")
        _ = outside.write_bytes(b"outside sentinel")
        self.addCleanup(outside.unlink, missing_ok=True)
        original = self.manifest.read_text(encoding="utf-8")
        for relative in ("../" + outside.name, str(outside), "target/debug/../../" + outside.name):
            with self.subTest(relative=relative):
                snapshot = json.loads(original)
                snapshot["files"][0]["path"] = relative
                _ = self.manifest.write_text(json.dumps(snapshot), encoding="utf-8")
                with self.assertRaises((OSError, ValueError)):
                    bazel_action.restore_cargo_artifacts(self.root, self.directory, self.manifest)
                self.assertEqual(outside.read_bytes(), b"outside sentinel")
        _ = self.manifest.write_text(original, encoding="utf-8")

    def test_missing_executable_dep_info_fails_capture(self) -> None:
        self.depfile.unlink()
        with self.assertRaises(OSError):
            bazel_action.capture_cargo_artifacts(
                self.root,
                self.log,
                self.root / "incomplete.cargo",
                self.root / "incomplete.cargo.json",
            )
        self.assertFalse((self.root / "incomplete.cargo.json").exists())

    def test_absent_or_malformed_cargo_artifact_evidence_cannot_publish_snapshot(self) -> None:
        for index, text in enumerate(
            (
                "",
                "Finished successfully, but no Cargo JSON evidence\n",
                '{"reason":"compiler-artifact"}\n',
                json.dumps(
                    {
                        "reason": "compiler-artifact",
                        "target": {"kind": ["lib"], "name": "fixture"},
                        "profile": {"test": False},
                        "filenames": "not a filename array",
                        "executable": None,
                    }
                )
                + "\n",
            )
        ):
            with self.subTest(index=index):
                _ = self.log.write_text(text, encoding="utf-8")
                directory = self.root / f"rejected-{index}.cargo"
                manifest = self.root / f"rejected-{index}.cargo.json"
                with self.assertRaises(ValueError):
                    bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
                self.assertFalse(manifest.exists())
                self.assertFalse(directory.exists())

    def test_check_without_runtime_products_materializes_empty_snapshot(self) -> None:
        metadata = self.target / "fixture.rmeta"
        _ = metadata.write_bytes(b"compiler-only metadata")
        _ = self.log.write_text(
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"kind": ["lib"], "name": "fixture"},
                    "profile": {"test": False},
                    "executable": None,
                    "filenames": [str(metadata)],
                }
            )
            + "\n",
            encoding="utf-8",
        )
        directory = self.root / "check.cargo"
        manifest = self.root / "check.cargo.json"
        bazel_action.capture_cargo_artifacts(self.root, self.log, directory, manifest)
        self.assertTrue(directory.is_dir())
        self.assertTrue(manifest.is_file())
        self.assertEqual([path.name for path in directory.iterdir()], ["payload.zip"])
        self.assertEqual(json.loads(manifest.read_text(encoding="utf-8"))["files"], [])
        before = self.program.read_bytes()
        bazel_action.restore_cargo_artifacts(self.root, directory, manifest)
        self.assertEqual(self.program.read_bytes(), before)


class CargoArtifactBatchRestoreTests(unittest.TestCase):
    """批量复用仍验证每个生产者，消费者只接收最后变体的完整字节。"""

    def setUp(self) -> None:
        self.snapshot = CargoArtifactSnapshotTests()
        self.addCleanup(self.snapshot.doCleanups)
        self.snapshot.setUp()
        fixture = self.snapshot
        self.final_bytes = b"final variant consumer"
        _ = fixture.program.write_bytes(self.final_bytes)
        self.final_directory = fixture.root / "final.cargo"
        self.final_manifest = fixture.root / "final.cargo.json"
        bazel_action.capture_cargo_artifacts(
            fixture.root,
            fixture.log,
            self.final_directory,
            self.final_manifest,
        )
        self.snapshots = [
            (fixture.directory, fixture.manifest),
            (self.final_directory, self.final_manifest),
        ]

    def test_final_variant_restores_missing_consumer_and_preserves_equal_timestamp(self) -> None:
        fixture = self.snapshot
        fixture.program.unlink()
        bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), self.final_bytes)
        os.utime(fixture.program, ns=(1_000_000_000, 1_000_000_000))
        before = fixture.program.stat().st_mtime_ns
        bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), self.final_bytes)
        self.assertEqual(fixture.program.stat().st_mtime_ns, before)

    def test_corrupted_superseded_payload_rejects_before_any_consumer_write(self) -> None:
        fixture = self.snapshot
        payload = fixture.directory / "payload.zip"
        original = payload.read_bytes()
        _ = payload.write_bytes(bytes(byte ^ 1 for byte in original))
        _ = fixture.program.write_bytes(b"current executable")
        _ = fixture.depfile.write_bytes(b"current dependency data")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), b"current executable")
        self.assertEqual(fixture.depfile.read_bytes(), b"current dependency data")

    def test_missing_later_payload_rejects_before_any_consumer_write(self) -> None:
        fixture = self.snapshot
        payload = self.final_directory / "payload.zip"
        payload.unlink()
        _ = fixture.program.write_bytes(b"current executable")
        _ = fixture.depfile.write_bytes(b"current dependency data")
        with self.assertRaises((OSError, ValueError)):
            bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), b"current executable")
        self.assertEqual(fixture.depfile.read_bytes(), b"current dependency data")

    def test_corrupted_later_payload_rejects_before_any_consumer_write(self) -> None:
        fixture = self.snapshot
        payload = self.final_directory / "payload.zip"
        original = payload.read_bytes()
        payload.write_bytes(original[:-1] + bytes([original[-1] ^ 1]))
        fixture.program.write_bytes(b"current executable")
        fixture.depfile.write_bytes(b"current dependency data")
        with self.assertRaises(ValueError):
            bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
        self.assertEqual(fixture.program.read_bytes(), b"current executable")
        self.assertEqual(fixture.depfile.read_bytes(), b"current dependency data")

    def test_rebound_bad_member_in_either_batch_rejects_before_any_consumer_write(self) -> None:
        fixture = self.snapshot
        for index, (directory, manifest) in enumerate(self.snapshots):
            with self.subTest(snapshot=index):
                payload = directory / "payload.zip"
                original_payload = payload.read_bytes()
                original_manifest = manifest.read_bytes()
                data = bytearray(original_payload)
                with zipfile.ZipFile(payload) as archive:
                    central = archive.start_dir
                # 整包 SHA 已更新，但成员结构与被绑定的原始表不一致。
                data[central + 16] ^= 1
                payload.write_bytes(data)
                snapshot = json.loads(original_manifest)
                snapshot["payload_sha256"] = hashlib.sha256(data).hexdigest()
                manifest.write_text(json.dumps(snapshot), encoding="utf-8")
                fixture.program.write_bytes(b"current executable")
                fixture.depfile.write_bytes(b"current dependency data")
                try:
                    with self.assertRaises(ValueError):
                        bazel_action.restore_cargo_artifact_snapshots(fixture.root, self.snapshots)
                    self.assertEqual(fixture.program.read_bytes(), b"current executable")
                    self.assertEqual(fixture.depfile.read_bytes(), b"current dependency data")
                finally:
                    payload.write_bytes(original_payload)
                    manifest.write_bytes(original_manifest)


if __name__ == "__main__":
    _ = unittest.main()
