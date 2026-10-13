"""覆盖 E-03/E-05：打包读取本次 Cargo 产物及其内嵌状态，不猜测缓存时间。"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "package-windows.ps1"

# 只加载实际部署入口中的函数定义；不运行构建/下载/安装，不复制选择算法。
FUNCTION_LOADER = r"""
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($env:R24_SCRIPT, [ref]$tokens, [ref]$errors)
if ($errors.Count) {throw '部署脚本语法无效'}
foreach ($name in ($env:R24_FUNCTIONS -split ',')) {
    $definition = $ast.Find({param($node)
        $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -ceq $name
    }, $false)
    if ($null -eq $definition) {throw "实际部署入口缺少 $name"}
    . ([scriptblock]::Create($definition.Extent.Text))
}
"""

DRIVER = FUNCTION_LOADER + r"""
$records = @(Get-Content -LiteralPath $env:R24_RECORDS -Raw -Encoding UTF8 | ConvertFrom-Json)
$artifact = Get-CargoArtifact -Messages $records -BinaryName $env:R24_BINARY -ManifestPath $env:R24_MANIFEST
if ($env:R24_CHECK_ENGINE -eq '1') {Assert-EmbeddedEngine -Messages $records -Artifact $artifact}
[Console]::WriteLine($artifact.executable)
"""

LICENSE_DRIVER = FUNCTION_LOADER + r"""
$package = Get-Content -LiteralPath $env:R24_PACKAGE -Raw -Encoding UTF8 | ConvertFrom-Json
$sourcePackage = Get-Content -LiteralPath $env:R24_SOURCE_PACKAGE -Raw -Encoding UTF8 | ConvertFrom-Json
$count = Copy-PackageLicenseMaterials -Package $package -SourcePackage $sourcePackage -Destination $env:R24_DESTINATION
[Console]::WriteLine($count)
"""

NOTICE_LOOP_DRIVER = r"""
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$root = $env:R24_ROOT
$noticeRoot = $env:R24_DESTINATION
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($env:R24_SCRIPT, [ref]$tokens, [ref]$errors)
if ($errors.Count) {throw '部署脚本语法无效'}
foreach ($statement in $ast.EndBlock.Statements) {
    if ($statement -is [Management.Automation.Language.FunctionDefinitionAst]) {
        . ([scriptblock]::Create($statement.Extent.Text))
    }
}
$sourceIndex = @{}
$packages = @()
$sources = Get-Content -LiteralPath (Join-Path $root 'vendor\SOURCES.json') -Raw -Encoding UTF8 | ConvertFrom-Json
foreach ($entry in $sources.packages) {
    $sourceIndex["$($entry.name)@$($entry.version)"] = $entry
    if ($entry.name -in @('slint','cfg_aliases')) {
        $packages += [pscustomobject]@{name=$entry.name;version=$entry.version;manifest_path=(Join-Path (Join-Path $root $entry.directory) 'Cargo.toml');license_file=$entry.license_file;license=$entry.license;repository=$entry.repository}
    }
}
$metadata = [pscustomobject]@{packages=$packages}
$licenseIndex = @()
$loops = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.ForEachStatementAst] -and $_.Variable.VariablePath.UserPath -ceq 'package'
})
if ($loops.Count -ne 1) {throw '无法唯一定位实际许可打包循环'}
. ([scriptblock]::Create($loops[0].Extent.Text))
"""


class PackageWindowsArtifactTests(unittest.TestCase):
    """执行部署脚本的真实 artifact 选择和状态文件读取边界。"""

    @classmethod
    def setUpClass(cls) -> None:
        if os.name != "nt":
            raise RuntimeError("打包回归只在 Windows 上执行")
        cls.powershell = shutil.which("powershell.exe")
        if cls.powershell is None:
            raise RuntimeError("缺少 Windows PowerShell 5.1")
        cls.workspace = ROOT / ".tmp" / "r24-packaging-tests"
        created_workspace = not cls.workspace.exists()
        if created_workspace:
            cls.addClassCleanup(cls.workspace.rmdir)
        subprocess.run(
            ["python", str(ROOT / "scripts" / "make_tmp.py"), "workspace", "--destination", str(cls.workspace)],
            cwd=ROOT,
            check=True,
            capture_output=True,
            timeout=30,
        )

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(dir=self.workspace, prefix="case-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.manifest = self.root / "Cargo.toml"
        self.executable = self.root / "构建目录 with spaces" / "x86_64-pc-windows-msvc" / "release" / "JchTools.exe"
        self.out_dir = self.root / "构建目录 with spaces" / "x86_64-pc-windows-msvc" / "release" / "build" / "active" / "out"
        self.out_dir.mkdir(parents=True)
        self.records = [
            {
                "reason": "compiler-artifact",
                "package_id": "path+file:///r24#jchtools@0.1.0",
                "manifest_path": str(self.manifest),
                "target": {"name": "JchTools", "kind": ["bin"]},
                "executable": str(self.executable),
            },
            {
                "reason": "build-script-executed",
                "package_id": "path+file:///r24#jchtools@0.1.0",
                "out_dir": str(self.out_dir),
            },
        ]

    def run_selection(self, *, engine: bool = True) -> subprocess.CompletedProcess[str]:
        records_file = self.root / "cargo-records.json"
        records_file.write_text(json.dumps(self.records), encoding="utf-8")
        driver = self.root / "selection.ps1"
        driver.write_text(DRIVER, encoding="utf-8-sig")
        environment = os.environ.copy()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Get-CargoArtifact,Assert-EmbeddedEngine",
            R24_RECORDS=str(records_file),
            R24_BINARY="JchTools",
            R24_MANIFEST=str(self.manifest),
            R24_CHECK_ENGINE="1" if engine else "0",
        )
        return subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )

    def write_status(self, status: str, *, foreign: bool = False) -> None:
        directory = self.out_dir if not foreign else self.out_dir.parent.parent / "unrelated" / "out"
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / "engine_embed_status.txt"
        path.write_text(status + "\n", encoding="utf-8")
        # 故意让无关缓存更“新”，覆盖旧选择器的两个相反错误。
        stamp = 1790899200 if not foreign else 1790985600
        os.utime(path, (stamp, stamp))

    def test_newer_foreign_incomplete_does_not_reject_active_ok(self) -> None:
        self.write_status("ok")
        self.write_status("incomplete", foreign=True)
        result = self.run_selection()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.executable))

    def test_newer_foreign_ok_cannot_hide_active_incomplete(self) -> None:
        self.write_status("incomplete")
        self.write_status("ok", foreign=True)
        result = self.run_selection()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("incomplete", result.stderr)

    def test_missing_active_status_cannot_use_foreign_ok(self) -> None:
        self.write_status("ok", foreign=True)
        self.assertNotEqual(self.run_selection().returncode, 0)

    def test_missing_active_build_record_fails_closed(self) -> None:
        self.write_status("ok")
        self.records[1]["package_id"] = "registry+https://example.invalid#foreign@1.0.0"
        self.assertNotEqual(self.run_selection().returncode, 0)

    def test_ambiguous_active_build_records_fail_closed(self) -> None:
        self.write_status("ok")
        self.records.append(dict(self.records[1]))
        self.assertNotEqual(self.run_selection().returncode, 0)

    def test_configured_artifact_path_not_default_release_is_returned(self) -> None:
        result = self.run_selection(engine=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.executable))

    def test_same_binary_name_from_another_manifest_is_not_selected(self) -> None:
        foreign = dict(self.records[0])
        foreign["manifest_path"] = str(self.root / "dependency" / "Cargo.toml")
        foreign["executable"] = str(self.root / "wrong.exe")
        self.records.insert(0, foreign)
        result = self.run_selection(engine=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(self.executable))

    def test_ambiguous_binary_artifacts_fail_closed(self) -> None:
        self.records.append(dict(self.records[0]))
        self.assertNotEqual(self.run_selection(engine=False).returncode, 0)

    def run_license_copy(self, package_name: str, *, missing: bool = False) -> tuple[subprocess.CompletedProcess[str], dict, Path]:
        manifest = json.loads((ROOT / "vendor" / "SOURCES.json").read_text(encoding="utf-8"))
        entry = next(package for package in manifest["packages"] if package["name"] == package_name)
        package = {
            "manifest_path": str(ROOT / entry["directory"] / "Cargo.toml"),
            "license_file": entry["license_file"],
        }
        if missing:
            entry["license_and_notice_materials"] = ["LICENSES/nonexistent-license.txt"]
        package_file = self.root / "package.json"
        package_file.write_text(json.dumps(package), encoding="utf-8")
        source_file = self.root / "source.json"
        source_file.write_text(json.dumps(entry), encoding="utf-8")
        destination = self.root / "notices"
        driver = self.root / "licenses.ps1"
        driver.write_text(LICENSE_DRIVER, encoding="utf-8-sig")
        environment = os.environ.copy()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Copy-PackageLicenseMaterials",
            R24_PACKAGE=str(package_file),
            R24_SOURCE_PACKAGE=str(source_file),
            R24_DESTINATION=str(destination),
        )
        result = subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        return result, entry, destination

    def test_recorded_nested_slint_licenses_preserve_paths_and_bytes(self) -> None:
        result, entry, destination = self.run_license_copy("slint")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertGreater(len(entry["license_and_notice_materials"]), 0)
        for relative in entry["license_and_notice_materials"]:
            self.assertEqual(
                (destination / relative).read_bytes(),
                (ROOT / entry["directory"] / relative).read_bytes(),
            )

    def test_recorded_cfg_aliases_notice_is_shipped(self) -> None:
        result, entry, destination = self.run_license_copy("cfg_aliases")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (destination / "NOTICES.md").read_bytes(),
            (ROOT / entry["directory"] / "NOTICES.md").read_bytes(),
        )

    def test_missing_recorded_license_fails_closed(self) -> None:
        result, _, _ = self.run_license_copy("slint", missing=True)
        self.assertNotEqual(result.returncode, 0)

    def test_actual_notice_loop_preserves_all_recorded_materials(self) -> None:
        destination = self.root / "actual-notice-loop"
        destination.mkdir()
        driver = self.root / "actual-notice-loop.ps1"
        driver.write_text(NOTICE_LOOP_DRIVER, encoding="utf-8-sig")
        environment = os.environ.copy()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_ROOT=str(ROOT),
            R24_DESTINATION=str(destination),
        )
        result = subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        sources = json.loads((ROOT / "vendor" / "SOURCES.json").read_text(encoding="utf-8"))
        for entry in sources["packages"]:
            if entry["name"] not in {"slint", "cfg_aliases"}:
                continue
            for relative in entry["license_and_notice_materials"]:
                self.assertEqual(
                    (destination / f'{entry["name"]}-{entry["version"]}' / relative).read_bytes(),
                    (ROOT / entry["directory"] / relative).read_bytes(),
                )

    def test_resolved_patched_packages_use_local_path_sources(self) -> None:
        # 真实 Cargo resolver 查询：仅读取元数据，不编译、不执行任何 crate。
        patched = {
            "ureq": "2.12.1",
            "image-webp": "0.2.4",
            "fax": "0.2.7",
            "ident_case": "1.0.1",
            "strict-num": "0.1.1",
            "zerovec": "0.11.8",
            "ttf-parser": "0.25.1",
            "blocking": "1.7.0",
            "rspolib": "0.1.2",
            "pxfm": "0.1.30",
            "error-code": "3.4.0",
        }
        result = subprocess.run(
            ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = json.loads(result.stdout)
        packages = {package["id"]: package for package in metadata["packages"]}
        self.assertEqual(
            {packages[member]["name"] for member in metadata["workspace_members"]},
            {"jchtools", "snap-ocr-core", "snap-ocr-worker"},
        )
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        pending = list(metadata["workspace_members"])
        reachable = set()
        while pending:
            identity = pending.pop()
            if identity in reachable:
                continue
            reachable.add(identity)
            pending.extend(dependency["pkg"] for dependency in nodes[identity]["deps"])
        for name, version in patched.items():
            matches = [
                package for package in packages.values()
                if package["name"] == name and package["version"] == version
            ]
            self.assertEqual(len(matches), 1, (name, version, matches))
            package = matches[0]
            self.assertIsNone(package["source"], (name, package["source"]))
            self.assertEqual(
                Path(package["manifest_path"]).resolve(),
                (ROOT / "vendor" / f"{name}-{version}" / "Cargo.toml").resolve(),
            )
            self.assertIn(package["id"], reachable, name)


if __name__ == "__main__":
    unittest.main()
