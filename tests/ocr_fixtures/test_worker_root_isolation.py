"""覆盖 O-29/XB-18：资产探测拒绝生产构建，不启动会访问用户配置的服务。"""

from __future__ import annotations

import contextlib
import json
import sqlite3
import subprocess
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import check_worker_root


class WorkerRootIsolationTests(unittest.TestCase):
    def test_settings_use_shared_schema_and_isolated_database(self) -> None:
        """覆盖 XB-18：共享 schema 迁入 foundation 后探测仍能写入隔离配置。"""
        state_root = check_worker_root.ROOT / ".tmp" / "synthetic-worker-state"
        engine_root = check_worker_root.ROOT / ".tmp" / "synthetic-engine"
        connection = sqlite3.connect(":memory:")
        try:
            with (
                patch.object(Path, "mkdir"),
                patch.object(check_worker_root.sqlite3, "connect", return_value=connection) as connect,
                patch.object(check_worker_root.contextlib, "closing", return_value=contextlib.nullcontext(connection)),
            ):
                check_worker_root.write_isolated_settings(state_root, engine_root)
            connect.assert_called_once_with(state_root / "config.sqlite3")
            self.assertEqual(
                connection.execute("SELECT value FROM app_settings WHERE key = 'xberg_directory'").fetchone(),
                (str(engine_root),),
            )
        finally:
            connection.close()

    def test_inherited_production_broker_cannot_escape_isolation(self) -> None:
        """覆盖 O-29/XB-18：继承的生产代理覆盖不能绕过 worker 能力守卫。"""
        sandbox = check_worker_root.ROOT / ".tmp" / "synthetic-worker-root"
        worker = sandbox / "assets" / "worker" / "v1" / "snap-ocr-worker.exe"
        production_broker = str(check_worker_root.ROOT / "production-JchTools.exe")
        process = Mock()
        process.poll.return_value = 0
        owner = Mock()
        capabilities = subprocess.CompletedProcess(
            [str(worker), "--capabilities"], 0,
            stdout=json.dumps({"test_hooks": True}).encode(), stderr=b"",
        )

        def spawn(*args: object, **kwargs: object) -> tuple[Mock, Mock]:
            # 捕获真正送入服务进程的环境：若沿用生产覆盖，此处即阻止逃逸。
            self.assertNotEqual(kwargs["env"]["JCHTOOLS_TEST_BROKER_EXE"], production_broker)
            self.assertEqual(kwargs["env"]["JCHTOOLS_TEST_BROKER_EXE"], str(worker))
            return process, owner

        with (
            patch.dict(check_worker_root.os.environ, {"JCHTOOLS_TEST_BROKER_EXE": production_broker}),
            patch.object(Path, "mkdir"),
            patch.object(Path, "is_file", return_value=True),
            patch.object(Path, "exists", return_value=False),
            patch.object(Path, "write_text"),
            patch.object(check_worker_root, "owned_test_directory", return_value=contextlib.nullcontext(str(sandbox))),
            patch.object(check_worker_root.shutil, "copytree"),
            patch.object(check_worker_root, "write_isolated_settings"),
            patch.object(check_worker_root.subprocess, "run", return_value=capabilities),
            patch.object(check_worker_root, "start_owned_process", side_effect=spawn),
            patch.object(check_worker_root, "pipe_name", return_value="isolated-pipe"),
            patch.object(check_worker_root, "wait_model", return_value=({"model": "ready", "ok": True}, "isolated-pipe")),
            patch.object(check_worker_root, "stop_broker"),
        ):
            check_worker_root.run_worker_root(
                Path("synthetic-assets"), "v1", Path("synthetic-engine"), without_font_cache=True,
            )
        owner.close.assert_called_once_with()

    def test_production_worker_is_rejected_before_service_spawn(self) -> None:
        sandbox = check_worker_root.ROOT / ".tmp" / "synthetic-worker-root"
        capabilities = subprocess.CompletedProcess(
            ["snap-ocr-worker.exe", "--capabilities"],
            0,
            stdout=json.dumps({"shared_xberg_protocol": 2, "background_service_protocol": 1}).encode(),
            stderr=b"",
        )
        with (
            patch.object(Path, "mkdir"),
            patch.object(Path, "is_file", return_value=True),
            patch.object(Path, "write_text"),
            patch.object(check_worker_root, "owned_test_directory", return_value=contextlib.nullcontext(str(sandbox))),
            patch.object(check_worker_root.shutil, "copytree"),
            patch.object(check_worker_root, "write_isolated_settings"),
            patch.object(check_worker_root.subprocess, "run", return_value=capabilities),
            patch.object(check_worker_root, "start_owned_process", side_effect=AssertionError("不能启动生产服务")) as spawn,
        ):
            with self.assertRaisesRegex(RuntimeError, "test-hooks"):
                check_worker_root.run_worker_root(Path("synthetic-assets"), "v1", Path("synthetic-engine"), without_font_cache=True)
            spawn.assert_not_called()


if __name__ == "__main__":
    unittest.main()
