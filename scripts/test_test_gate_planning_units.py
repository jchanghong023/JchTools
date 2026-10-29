"""门计划、授权边界及临时目录清理回归。"""

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
from unittest.mock import patch

from scripts import (
    gate_runtime,
    gate_summary,
    local_gate_plan,
    make_tmp,
    test_gate,
)


class _PlanningGateRun:
    """只接收真实计划器的阶段，不执行 argv，也不伪造模型响应。."""

    def __init__(self, log_dir: Path, logs: dict[str, Path]) -> None:
        self.log_dir: Path = log_dir
        self.logs: dict[str, Path] = logs
        self.stages: list[gate_runtime.Stage] = []
        self.batches: list[list[gate_runtime.Stage]] = []
        self.results: list[gate_runtime.Result] = []

    def run(self, stages: list[gate_runtime.Stage]) -> list[gate_runtime.Result]:
        self.batches.append(list(stages))
        self.stages.extend(stages)
        for stage in stages:
            if stage.name == "rust-coverage-status":
                self.log_dir.mkdir(parents=True, exist_ok=True)
                (self.log_dir / "rust-coverage-status.json").write_text(
                    json.dumps({"cache": {"status": "hit-exact"}}), encoding="utf-8"
                )
        results = [
            gate_runtime.Result(
                stage.name,
                "PASS",
                "planning-only synthetic prerequisite; command not executed",
                log=self.logs.get(stage.name),
            )
            for stage in stages
        ]
        self.results.extend(results)
        return results



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


class AcceptanceCoverageGapTests(unittest.TestCase):
    """acceptance 覆盖缺口判定：仅剩环境受限条目（C08/C09）NOT RUN 时不得阻塞测试门。."""

    def test_only_environment_blocked_items_are_not_a_gap(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目：C08、C09；缺资产或 --only 子集不能冒充完整验收）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        assert gate_summary.acceptance_coverage_gaps(text) == []  # nosec B101: 环境受限条目不构成缺口。

    def test_other_not_run_items_remain_gaps(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目：A25、C08；详见 markdown-acceptance.log）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        gaps = gate_summary.acceptance_coverage_gaps(text)
        assert gaps  # nosec B101: A25 缺样本不属环境受限，不得放行。

    def test_not_run_without_item_ids_remains_gap(self) -> None:
        summary = "NOT RUN  markdown-acceptance（存在未执行条目；详见 markdown-acceptance.log）"
        text = f"PASS  snap-ocr-worker-root\n{summary}"
        gaps = gate_summary.acceptance_coverage_gaps(text)
        assert gaps  # nosec B101: 提取不到条目号时 fail-closed，不得静默放行。

    def test_partial_environment_blocked_items_remain_gaps(self) -> None:
        text = "PARTIAL  markdown-acceptance（仍有 NOT RUN：C08、C09）\nPASS  snap-ocr-worker-root"
        gaps = gate_summary.acceptance_coverage_gaps(text)
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
        run = gate_runtime.GateRun("fastcheck", test_gate.ROOT, workspace / "native-cleanup-logs")
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
            run.results.append(gate_runtime.Result("native-fixture", "PASS", "真实特殊目录已写入并读回校验"))
            self.assertEqual(run.finish(), 0, "合法 verbatim 测试产物不能使完整门在收尾失败")
            self.assertFalse(native.exists(), "门报告通过后仍有自身测试目录残留")
        finally:
            if native.exists():
                shutil.rmtree(native)
            if run._temporary is not None:
                run._temporary.cleanup()


class OpenCodeAuthorizationBoundaryTests(unittest.TestCase):
    """覆盖 P-12/P-15：功能与打包门分层不授予真实模型调用权限。."""

    def test_worker_count_caps_available_cpus(self) -> None:
        for available, expected in ((1, 1), (8, 8), (15, 15), (64, 15), (None, 1), (0, 1)):
            with self.subTest(available=available), patch.object(os, "process_cpu_count", return_value=available):
                self.assertEqual(local_gate_plan.test_worker_count(), expected)

    def test_fastcheck_never_plans_nextest(self) -> None:
        plan = local_gate_plan.gate_plan("fastcheck", test_gate.ROOT, test_gate.ROOT / ".tmp", {})
        self.assertFalse(any(stage.argv[1:3] == ["nextest", "run"] for stage in plan))
        self.assertFalse(any(stage.argv[1:3] == ["-m", "pytest"] for stage in plan))

    def test_missing_real_engine_records_unverified(self) -> None:
        results: list[gate_runtime.Result] = []
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {"JCHTOOLS_TEST_7ZIP": ""}):
            with self.assertRaises(FileNotFoundError):
                local_gate_plan._real_coverage(results, Path(temporary))
        self.assertEqual([(result.name, result.status) for result in results], [("real-engine-tests", "UNVERIFIED")])

    def test_missing_media_prerequisite_records_unverified(self) -> None:
        results: list[gate_runtime.Result] = []
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run = _PlanningGateRun(root, {})
            with patch.dict(os.environ, {"JCHTOOLS_MEDIA_E2E_INPUT": "incomplete"}, clear=True):
                with self.assertRaises(RuntimeError):
                    local_gate_plan._real_media_stages(
                        cast("gate_runtime.GateRun", cast("object", run)), results, root, {}
                    )
        self.assertEqual([(result.name, result.status) for result in results], [("real-media-e2e", "UNVERIFIED")])

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
            logs: dict[str, Path] = {}
            snapshot_log = log_dir / "source-snapshot.log"
            _ = snapshot_log.write_text("synthetic source snapshot\n", encoding="utf-8")
            logs["source-snapshot"] = snapshot_log
            executed_by_level: dict[tuple[str, bool], list[str]] = {}
            with (
                patch.dict(os.environ, environment, clear=True),
                patch("subprocess.run", side_effect=AssertionError("计划回归不得执行进程")),
                patch("subprocess.Popen", side_effect=AssertionError("计划回归不得启动进程")),
                patch.object(test_gate, "ROOT", root),
            ):
                for level, full in ((level, full) for level in levels for full in (False, True)):
                    with self.subTest(installation=installation, level=level, full=full):
                        run = _PlanningGateRun(log_dir, logs)
                        plan = test_gate._gate_plan(level, log_dir, full=full)
                        results = run.results
                        snapshot: list[str] = []
                        test_gate._execute_plan(
                            cast("gate_runtime.GateRun", cast("object", run)), level, plan, snapshot,
                            full=full,
                        )
                        names = [stage.name for stage in run.stages]
                        executed_by_level[(level, full)] = names
                        self.assertEqual(len(names), len(set(names)), "完整功能阶段不得重复执行")
                        self.assertEqual(names.count("test-gate-pytest-unit"), 1)
                        pytest_gate_stage = next(stage for stage in run.stages if stage.name == "test-gate-pytest-unit")
                        self.assertEqual(pytest_gate_stage.argv.count("scripts/test_pytest_gate.py"), 1)
                        self.assertEqual(
                            sum(stage.argv.count("scripts/test_pytest_gate.py") for stage in run.stages), 1
                        )
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
                                self.assertEqual(names.count(name), 1)
                            package = next(stage for stage in run.stages if stage.name == "package")
                            self.assertIn(str(root / "scripts/package-windows.ps1"), package.argv)
                            release = next(stage for stage in run.stages if stage.name == "release-target-build")
                            self.assertEqual(release.argv[1:], ["build", "--all-targets", "--release"])
                        ordinary = [stage for stage in run.stages if stage.name in {
                            "root-nextest", "snap-ocr-core-nextest", "snap-ocr-worker-nextest"
                        }]
                        self.assertEqual(len(ordinary), 3)
                        for stage in ordinary:
                            self.assertEqual(stage.argv[1:3], ["nextest", "run"] if full else ["affected", "run"])
                            self.assertNotIn("--run-ignored", stage.argv)
                            self.assertNotIn("--ignore-default-filter", stage.argv)
                            if not full:
                                self.assertIn("--report-json", stage.argv)
                                self.assertIn("--", stage.argv)
                        real = [stage for stage in run.stages if stage.name.startswith(("real-engine-", "real-xberg-", "real-media-e2e-"))]
                        self.assertEqual(len(real), 5)
                        for stage in [*ordinary, *real]:
                            self.assertIn("--no-tests", stage.argv)
                            self.assertEqual(stage.argv[stage.argv.index("--no-tests") + 1], "fail")
                            self.assertIn(str(root / ".config/nextest.toml"), stage.argv)
                            self.assertEqual(
                                stage.argv[stage.argv.index("--test-threads") + 1],
                                str(local_gate_plan.test_worker_count()),
                            )
                            self.assertNotIn("--logfile", stage.argv)
                        for stage in real:
                            self.assertEqual(stage.argv[1:3], ["nextest", "run"])
                            self.assertEqual(stage.argv[stage.argv.index("--run-ignored") + 1], "only")
                            self.assertNotIn("acp_opencode", " ".join(stage.argv))
                        for stage in real[-2:]:
                            self.assertEqual(
                                (stage.env or {}).get("JCHTOOLS_TEST_BROKER_EXE"),
                                environment["JCHTOOLS_TEST_GUI_EXE"],
                            )
                            self.assertIn("test(=real_component_", " ".join(stage.argv))
                        python = [stage for stage in run.stages if stage.name.endswith("-unit")]
                        for stage in python:
                            self.assertEqual(stage.argv[1:5], ["-m", "pytest", "-p", "scripts.pytest_gate"])
                            self.assertIn("--jchtools-report-dir", stage.argv)
                            self.assertNotIn("--logfile", stage.argv)
                            self.assertIn("-x", stage.argv)
                            self.assertEqual(
                                stage.argv[stage.argv.index("-n") + 1],
                                str(local_gate_plan.test_worker_count()),
                            )
                            self.assertEqual(stage.argv[stage.argv.index("--dist") + 1], "loadscope")
                            self.assertEqual(stage.argv[stage.argv.index("--max-worker-restart") + 1], "0")
                            incremental = stage.name == "worker-root-isolation-unit" and not full
                            self.assertIn("--testmon" if incremental else "--no-testmon", stage.argv)
                            self.assertNotIn("--no-testmon" if incremental else "--testmon", stage.argv)
                            if incremental:
                                self.assertEqual(
                                    (stage.env or {}).get("TESTMON_DATAFILE"),
                                    str(root / ".tmp/testmon" / f"{stage.name}.testmondata"),
                                )
                                self.assertEqual(
                                    stage.argv[stage.argv.index("--testmon-env") + 1], f"{stage.name}-missing"
                                )
                            else:
                                self.assertNotIn("TESTMON_DATAFILE", stage.env or {})
                                self.assertNotIn("--testmon-env", stage.argv)
                        rust_batch = next(batch for batch in run.batches if any(s.name == "root-nextest" for s in batch))
                        self.assertEqual([s.name for s in rust_batch if s in ordinary + real],
                                         [s.name for s in ordinary + real])
                        self.assertTrue(any(stage.name.endswith("-doctests") for stage in rust_batch))
                        model_results = [result for result in results if result.name == "real-opencode"]
                        assert len(model_results) == 1  # nosec B101: 真实模型验收必须独立报告。
                        assert (  # nosec B101: 不得把未授权模型调用冒报 PASS。
                            model_results[0].status == "NOT_RUN_SEPARATE_USER_INSTRUCTION_REQUIRED"
                        )
            for full in (False, True):
                if ("fulltest", full) in executed_by_level and ("slowtest", full) in executed_by_level:
                    slow_functional = [
                        name for name in executed_by_level[("slowtest", full)]
                        if name not in {"release-target-build", "package", "pyquality-pip-audit",
                                        "package-windows-unit", "fetch-7zip-unit"}
                    ]
                    self.assertEqual(executed_by_level[("fulltest", full)], slow_functional)

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
