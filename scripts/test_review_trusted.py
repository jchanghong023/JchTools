"""R21 可信基回归：仅合成目录和真实解析行为，不启动桌面、引擎或 Cargo。"""

from __future__ import annotations

import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from scripts import make_tmp, requirement_coverage

ROOT = Path(__file__).resolve().parent.parent


class TrustedWorkspaceTests(unittest.TestCase):
    def setUp(self) -> None:
        workspace = ROOT / ".tmp" / "review-trusted"
        with patch.object(sys, "argv", ["make_tmp.py", "workspace", "--destination", str(workspace)]):
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(make_tmp.main(), 0)
        temporary = tempfile.TemporaryDirectory(dir=workspace)
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)


class DestinationBoundaryTests(TrustedWorkspaceTests):
    def assert_rejected(self, destination: Path) -> None:
        repository = self.directory / "fake-repository"
        (repository / "scripts").mkdir(parents=True, exist_ok=True)
        with (
            patch.object(make_tmp, "__file__", str(repository / "scripts" / "make_tmp.py")),
            patch.object(sys, "argv", ["make_tmp.py", "testdata", "--destination", str(destination), "--force"]),
            patch.object(make_tmp, "seven_zip", return_value=Path("not-executed.exe")) as engine,
            patch.object(make_tmp, "_clear_existing_target", side_effect=AssertionError("不应到达破坏性清空")),
        ):
            with self.assertRaisesRegex(SystemExit, r"\.tmp"):
                make_tmp.main()
            engine.assert_not_called()

    def test_external_destination_rejected_before_clear(self) -> None:
        external = self.directory / "synthetic-external"
        external.mkdir()
        sentinel = external / "sentinel.txt"
        sentinel.write_bytes(b"must survive")
        self.assert_rejected(external)
        self.assertEqual(sentinel.read_bytes(), b"must survive")
        self.assertEqual(sorted(path.name for path in external.iterdir()), ["sentinel.txt"])

    def test_junction_external_destination_rejected_before_clear(self) -> None:
        external = self.directory / "synthetic-external"
        external.mkdir()
        sentinel = external / "sentinel.txt"
        sentinel.write_bytes(b"must survive junction")
        temp = self.directory / "fake-repository" / ".tmp"
        temp.mkdir(parents=True)
        junction = temp / "linked-data"
        result = subprocess.run(
            ["cmd", "/d", "/c", "mklink", "/J", str(junction), str(external)],
            capture_output=True,
            check=False,
            timeout=20,
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.addCleanup(lambda: junction.rmdir() if junction.exists() else None)
        self.assert_rejected(junction)
        self.assertEqual(sentinel.read_bytes(), b"must survive junction")

    def test_valid_destination_reaches_existing_clear_boundary(self) -> None:
        repository = self.directory / "fake-repository"
        scripts = repository / "scripts"
        scripts.mkdir(parents=True)
        destination = repository / ".tmp" / "data"
        with (
            patch.object(make_tmp, "__file__", str(scripts / "make_tmp.py")),
            patch.object(sys, "argv", ["make_tmp.py", "testdata", "--destination", str(destination), "--force"]),
            patch.object(make_tmp, "seven_zip", return_value=Path("not-executed.exe")),
            patch.object(
                make_tmp, "_clear_existing_target", side_effect=AssertionError("合法目的地到达清空边界")
            ) as clear,
        ):
            with self.assertRaisesRegex(AssertionError, "合法目的地"):
                make_tmp.main()
            clear.assert_called_once_with(destination.resolve(), force=True)


class AttributeBaselineTests(TrustedWorkspaceTests):
    # static_check 自 10071d43 起按自身 ROOT 硬性要求登记清单全部在场；合成目录
    # 必须满足同一合同（含各 build producer），检查本身不为此放宽。
    _PRODUCTION_CRATES = (
        "foundation",
        "file-core",
        "recursive-extract",
        "directory-organizer",
        "md-tools",
        "git-tools",
        "xberg",
        "assets",
        "markdown",
        "acp-api",
        "codeagent",
        "jchocr",
        "ui",
        "gui",
    )
    _OPTIONAL_CRATES = ("snap-ocr-core", "snap-ocr-worker")
    _BUILD_PRODUCERS = (
        "",  # 根 build.rs
        "crates/file-core",
        "crates/assets",
        "crates/ui",
        "optional/snap-ocr-worker",
    )

    def _materialize_manifest_contract(self) -> None:
        (self.directory / "Cargo.toml").write_bytes(b"")
        (self.directory / "src").mkdir(exist_ok=True)
        for parent, names in (
            (self.directory / "crates", self._PRODUCTION_CRATES),
            (self.directory / "optional", self._OPTIONAL_CRATES),
        ):
            for crate in names:
                package = parent / crate
                (package / "src").mkdir(parents=True, exist_ok=True)
                (package / "Cargo.toml").write_bytes(b"")
                (package / "src" / "lib.rs").write_bytes(b"")
        for producer in self._BUILD_PRODUCERS:
            (self.directory / producer / "build.rs").write_bytes(b"")

    def collect(self, source: str) -> dict[str, object]:
        scripts = self.directory / "scripts"
        scripts.mkdir(exist_ok=True)
        shutil.copyfile(ROOT / "scripts" / "static_check.py", scripts / "static_check.py")
        tests = self.directory / "tests"
        tests.mkdir(exist_ok=True)
        (tests / "sample.rs").write_text(source, encoding="utf-8")
        self._materialize_manifest_contract()
        result = subprocess.run(
            [sys.executable, str(scripts / "static_check.py"), "--update-test-baseline"],
            capture_output=True,
            check=False,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        rows = json.loads((scripts / "test-baseline.json").read_text(encoding="utf-8"))["tests"]
        self.assertEqual(len(rows), 1)
        return rows[0]

    def test_ignore_across_comment_changes_baseline(self) -> None:
        baseline = self.collect("#[test]\nfn sample() { assert!(false); }\n")
        changed = self.collect("#[ignore]\n// 忽略原因\n#[test]\nfn sample() { assert!(false); }\n")
        self.assertFalse(baseline["ignored"])
        self.assertTrue(changed["ignored"])
        self.assertEqual(changed["body"], baseline["body"])

    def test_cfg_across_nested_comment_changes_baseline(self) -> None:
        baseline = self.collect("#[test]\nfn sample() { assert!(false); }\n")
        changed = self.collect("#[cfg(any())]\n/* 外层 /* 内层 */ 继续 */\n#[test]\nfn sample() { assert!(false); }\n")
        self.assertIsNone(baseline["cfg"])
        self.assertEqual(changed["cfg"], "any()")
        self.assertEqual(changed["body"], baseline["body"])

    def test_comment_alone_does_not_change_test_gates(self) -> None:
        changed = self.collect("// #[ignore] 只是注释\n#[test]\nfn sample() { assert!(false); }\n")
        self.assertFalse(changed["ignored"])
        self.assertIsNone(changed["cfg"])


class RequirementInventoryTests(TrustedWorkspaceTests):
    def test_ah_ca_authority_and_references_remain_unverified(self) -> None:
        requirements = self.directory / "docs" / "requirements"
        requirements.mkdir(parents=True)
        expected = {f"AH-{number:02}" for number in range(1, 17)} | {f"CA-{number:02}" for number in range(1, 8)}
        (requirements / "authority.md").write_text(
            "\n".join(f"- **{identifier}** 合成行为" for identifier in sorted(expected)), encoding="utf-8"
        )
        tests = self.directory / "tests"
        tests.mkdir()
        (tests / "sample.rs").write_text("// 覆盖 AH-01～AH-16 / CA-01～CA-07\n", encoding="utf-8")
        report = requirement_coverage.build_report(self.directory)
        self.assertEqual({item["id"] for item in report["items"]}, expected)
        self.assertEqual(report["summary"], {"total": 23, "without_reference": 0, "verified": 0, "unverified": 23})
        self.assertTrue(all(item["status"] == "UNVERIFIED" for item in report["items"]))


class DesktopAuthorizationTests(unittest.TestCase):
    def test_desktop_driver_entry_requires_explicit_authorization(self) -> None:
        # 只运行真实入口root赋值之前的if决策；不执行目录/工具发现、Cargo或桌面动作。
        harness = r"""
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$payload = [Console]::In.ReadToEnd() | ConvertFrom-Json
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($payload.path, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw '验收语法错误' }
$WithGuiSmoke=[bool]$payload.smoke
$WithMarkdownAcceptance=[bool]$payload.markdown
$WithDesktopTests=[bool]$payload.desktop
try {
    foreach ($statement in $ast.EndBlock.Statements) {
        if ($statement -is [Management.Automation.Language.AssignmentStatementAst] -and $statement.Left.Extent.Text -eq '$root') { break }
        if ($statement -is [Management.Automation.Language.IfStatementAst]) {
            . ([ScriptBlock]::Create($statement.Extent.Text))
        }
    }
    [Console]::WriteLine('ALLOWED')
} catch {
    [Console]::WriteLine('REFUSED:' + $_.Exception.Message)
}
"""
        powershell = Path(os.environ["SYSTEMROOT"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
        for smoke, markdown, desktop in (
            (True, False, False),
            (False, True, False),
            (True, True, False),
            (True, True, True),
            (False, False, False),
        ):
            with self.subTest(smoke=smoke, markdown=markdown, desktop=desktop):
                payload = {
                    "path": str(ROOT / "scripts" / "acceptance.ps1"),
                    "smoke": smoke,
                    "markdown": markdown,
                    "desktop": desktop,
                }
                result = subprocess.run(
                    [str(powershell), "-NoProfile", "-Command", harness],
                    input=json.dumps(payload),
                    text=True,
                    encoding="utf-8",
                    capture_output=True,
                    check=False,
                    timeout=30,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                if (smoke or markdown) and not desktop:
                    self.assertTrue(result.stdout.startswith("REFUSED:"), result.stdout)
                    self.assertIn("WithDesktopTests", result.stdout)
                else:
                    self.assertEqual(result.stdout.strip(), "ALLOWED")


if __name__ == "__main__":
    unittest.main()
