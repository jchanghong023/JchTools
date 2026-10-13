"""覆盖 O-29/XB-18：资产探测拒绝生产构建，不启动会访问用户配置的服务。"""

from __future__ import annotations

import contextlib
import json
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

import check_worker_root


class WorkerRootIsolationTests(unittest.TestCase):
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
            patch.object(check_worker_root.subprocess, "Popen", side_effect=AssertionError("不能启动生产服务")) as spawn,
        ):
            with self.assertRaisesRegex(RuntimeError, "test-hooks"):
                check_worker_root.run_worker_root(Path("synthetic-assets"), "v1", Path("synthetic-engine"), without_font_cache=True)
            spawn.assert_not_called()


if __name__ == "__main__":
    unittest.main()
