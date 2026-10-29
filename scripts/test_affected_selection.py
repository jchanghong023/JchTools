"""独立的真实 cargo-affected 选测实验；不属于日常业务测试套件。

运行：pytest -q scripts/test_affected_selection.py。需要 git、Rust、cargo-nextest、
cargo-affected 和 llvm-tools；不下载依赖，任何工具失败都直接令测试失败。
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import tomllib
from pathlib import Path
from unittest.mock import patch

from scripts import gate_runtime, local_gate_plan, make_tmp


REPO = Path(__file__).resolve().parent.parent


def test_real_affected_selection() -> None:
    workspace = REPO / ".tmp" / "affected-selection"
    with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
        assert make_tmp.main() == 0

    with tempfile.TemporaryDirectory(prefix="experiment-", dir=workspace) as temporary:
        root = Path(temporary)
        (root / "src").mkdir()
        (root / "resources").mkdir()
        markers = root / "target" / "markers"
        markers.mkdir(parents=True)
        env = {key: value for key, value in os.environ.items() if not key.upper().startswith("GIT_")}
        env.update({
            "CARGO_TARGET_DIR": str(root / "target"),
            "CARGO_NET_OFFLINE": "true",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Selection test",
            "GIT_AUTHOR_EMAIL": "selection@example.invalid",
            "GIT_COMMITTER_NAME": "Selection test",
            "GIT_COMMITTER_EMAIL": "selection@example.invalid",
            "SELECTION_MARKERS": str(markers),
        })

        def run(*args: str) -> subprocess.CompletedProcess[str]:
            return subprocess.run(
                args, cwd=root, env=env, check=True, text=True,
                encoding="utf-8", errors="replace", capture_output=True,
            )

        # 直接复用生产 workspace 的规则，而不是在夹具中另写一份可能漂移的规则。
        production = tomllib.loads((REPO / "Cargo.toml").read_text(encoding="utf-8"))
        rules = production["workspace"]["metadata"]["affected"]["rule"]
        manifest = (
            '[package]\nname = "selection-fixture"\nversion = "0.1.0"\nedition = "2021"\n'
            '\n[features]\ntest-hooks = []\n\n[workspace]\n'
        )
        for rule in rules:
            manifest += (
                '\n[[workspace.metadata.affected.rule]]\n'
                f'globs = {json.dumps(rule["globs"])}\n'
                f'filterset = {json.dumps(rule["filterset"])}\n'
            )
        (root / "Cargo.toml").write_text(manifest, encoding="utf-8")
        (root / ".config").mkdir()
        (root / ".config" / "nextest.toml").write_text(
            '[profile.default]\nretries = 0\n', encoding="utf-8"
        )
        (root / ".gitignore").write_text("/target/\n", encoding="utf-8")
        (root / "resources" / "example.slint").write_text("original resource\n", encoding="utf-8")
        logic = root / "src" / "logic.rs"
        logic.write_text(
            "pub fn first() -> u32 {\n    7\n}\n\n"
            "pub fn second() -> u32 {\n    13\n}\n",
            encoding="utf-8",
        )
        (root / "src" / "lib.rs").write_text(
            'mod logic;\n'
            '#[cfg(test)]\nmod tests {\n'
            '    fn mark(name: &str) {\n'
            '        let dir = std::env::var_os("SELECTION_MARKERS").unwrap();\n'
            '        std::fs::write(std::path::Path::new(&dir).join(name), "ran").unwrap();\n'
            '    }\n'
            '    #[test]\n    fn first_test() {\n'
            '        assert!(super::logic::first() > 0);\n'
            '        let resource = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))'
            '.join("resources/example.slint");\n'
            '        assert!(!std::fs::read_to_string(resource).unwrap().is_empty());\n'
            '        mark("first");\n    }\n'
            '    #[test]\n    fn second_test() {\n'
            '        assert!(super::logic::second() > 0);\n'
            '        mark("second");\n    }\n}\n',
            encoding="utf-8",
        )
        run("git", "init", "-q")
        run("cargo", "generate-lockfile", "--offline")
        run("git", "add", ".")
        run("git", "commit", "-q", "-m", "fixture")

        def clear_markers() -> None:
            for marker in markers.iterdir():
                marker.unlink()

        def marked() -> set[str]:
            return {marker.name for marker in markers.iterdir()}

        def affected() -> dict:
            report = root / "target" / "selection.json"
            run(
                "cargo", "affected", "run", "--report-json", str(report), "--report-detail", "full",
                "--", "--workspace", "--all-targets", "--features", "test-hooks",
            )
            return json.loads(report.read_text(encoding="utf-8"))

        gate = gate_runtime.GateRun("fulltest", root, root / "target" / "gate-logs")
        with patch.dict(os.environ, env, clear=True):
            # A missing baseline on a dirty tree must run the native fallback,
            # not record coverage against the modified sources.
            logic.write_text(logic.read_text(encoding="utf-8").replace("    7\n", "    8\n"), encoding="utf-8")
            assert not local_gate_plan.prepare_rust_baseline(gate, root)
            status = json.loads((gate.log_dir / "rust-coverage-status.json").read_text(encoding="utf-8"))
            assert status["cache"]["status"] != "hit-exact"
            report = affected()
            assert marked() == {"first", "second"}
            run("git", "restore", "src/logic.rs")
            clear_markers()

            try:
                assert local_gate_plan.prepare_rust_baseline(gate, root)
            finally:
                for result in gate.results:
                    if result.status == "FAIL" and result.log is not None:
                        print(result.log.read_text(encoding="utf-8", errors="replace"))
            assert marked() == {"first", "second"}
            clear_markers()
            assert not local_gate_plan.prepare_rust_baseline(gate, root)
            status = json.loads((gate.log_dir / "rust-coverage-status.json").read_text(encoding="utf-8"))
            assert status["cache"]["status"] == "hit-exact"
            assert marked() == set()

        clear_markers()
        report = affected()
        assert report["cache"]["status"] == "hit-exact"
        assert report["selection"]["summary"]["selected"] == 0
        assert marked() == set()

        logic.write_text(logic.read_text(encoding="utf-8").replace("    7\n", "    8\n"), encoding="utf-8")
        report = affected()
        assert report["selection"]["summary"]["selected"] == 1
        assert marked() == {"first"}

        # 恢复 Rust 差异，证明生产规则而不是 LLVM 命中使资源改动选中测试。
        run("git", "restore", "src/logic.rs")
        clear_markers()
        (root / "resources" / "example.slint").write_text("changed resource\n", encoding="utf-8")
        report = affected()
        assert report["selection"]["summary"]["config"] == 2
        assert marked() == {"first", "second"}

        # A newly introduced test has no baseline coverage and must still run.
        run("git", "restore", "resources/example.slint")
        clear_markers()
        library = root / "src" / "lib.rs"
        library.write_text(
            library.read_text(encoding="utf-8").replace(
                '        mark("second");\n    }\n}\n',
                '        mark("second");\n    }\n'
                '    #[test]\n    fn new_test() { mark("new"); }\n}\n',
            ),
            encoding="utf-8",
        )
        report = affected()
        assert report["selection"]["summary"]["selected"] >= 1
        assert "new" in marked()
        run("git", "restore", "src/lib.rs")

        # 明确全量入口绕过 affected 的选择结果。
        run("git", "restore", "resources/example.slint")
        clear_markers()
        run("cargo", "nextest", "run")
        assert marked() == {"first", "second"}
