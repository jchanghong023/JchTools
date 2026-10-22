"""test_test_gate 按主题拆分的用例模块：快照编码 / 覆盖缺口 / 临时回收与真实计划器授权边界.

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
