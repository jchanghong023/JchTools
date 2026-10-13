"""覆盖 E-03/E-05：打包读取本次 Cargo 产物及其内嵌状态，不猜测缓存时间。"""

from __future__ import annotations

import base64
import hashlib
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
if ($env:R24_CHECK_ENGINE -eq '1') {
    $engineArtifact = Get-CargoLibraryArtifact -Messages $records -LibraryName 'jchtools_file_core' -ManifestPath $env:R24_ENGINE_MANIFEST
    Assert-EmbeddedEngine -Messages $records -Artifact $engineArtifact
}
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
        $packages += [pscustomobject]@{id="vendor-$($entry.name)";name=$entry.name;version=$entry.version;manifest_path=(Join-Path (Join-Path $root $entry.directory) 'Cargo.toml');license_file=$entry.license_file;license=$entry.license;repository=$entry.repository}
    }
}
$packages += [pscustomobject]@{id='workspace-own';name='jchtools-assets';version='0.1.0';manifest_path=(Join-Path $root 'crates\assets\Cargo.toml')}
$metadata = [pscustomobject]@{packages=$packages;workspace_members=@('workspace-own')}
$licenseIndex = @()
$loops = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.ForEachStatementAst] -and $_.Variable.VariablePath.UserPath -ceq 'package'
})
if ($loops.Count -ne 1) {throw '无法唯一定位实际许可打包循环'}
. ([scriptblock]::Create($loops[0].Extent.Text))
"""

# 提取 worker 构建到主包构建调用之间的真实顶层语句，只替换外部 Cargo 边界。
# 清单回填、写盘、暂存校验及环境变量准备全部来自生产 AST，不能在驱动中重实现。
SNAP_MANIFEST_DRIVER = r"""
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$root = $env:R24_ROOT
$DestinationRoot = $env:R24_DESTINATION
$extra = @('--offline')
Set-Location -LiteralPath $root
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile($env:R24_SCRIPT, [ref]$tokens, [ref]$errors)
if ($errors.Count) {throw '部署脚本语法无效'}
foreach ($statement in $ast.EndBlock.Statements) {
    if ($statement -is [Management.Automation.Language.FunctionDefinitionAst]) {
        . ([scriptblock]::Create($statement.Extent.Text))
    }
}
$script:packageTotalWatch = [Diagnostics.Stopwatch]::StartNew()
Initialize-PackageTiming -RepositoryRoot $env:R24_REPOSITORY
[IO.File]::WriteAllText($env:R24_TIMING_POINTER, $script:packageTiming.path)
$statements = @($ast.EndBlock.Statements)
$starts = @()
$ends = @()
for ($index = 0; $index -lt $statements.Count; $index++) {
    $statement = $statements[$index]
    if ($statement -isnot [Management.Automation.Language.AssignmentStatementAst] -or
        $statement.Left -isnot [Management.Automation.Language.VariableExpressionAst]) {continue}
    switch -CaseSensitive ($statement.Left.VariablePath.UserPath) {
        'workerBuildMessages' {$starts += ($index - 1)}
        'mainArtifact' {$ends += $index}
    }
}
if ($starts.Count -ne 1 -or $ends.Count -ne 1 -or $starts[0] -ge $ends[0]) {
    throw '无法唯一定位实际 worker 清单回填和主包构建输入准备阶段'
}
$traps = @($ast.EndBlock.Traps)
if ($traps.Count -ne 1) {throw '必须保留实际打包异常边界'}
$stage = [scriptblock]::Create($traps[0].Extent.Text + "`r`n" + (($statements[$starts[0]..($ends[0] - 1)] |
    ForEach-Object {$_.Extent.Text}) -join "`r`n"))
$script:workerCalls = 0
$script:mainCalls = 0
$script:buildInput = $null
function Invoke-Cargo([string[]]$Arguments, [switch]$CaptureBuildMetadata) {
    if (-not $CaptureBuildMetadata -or $Arguments[0] -cne 'build') {
        throw '回归驱动禁止启动任何外部 Cargo 命令'
    }
    if ($Arguments -contains 'snap-ocr-worker' -and $Arguments -contains '--manifest-path') {
        $script:workerCalls++
        # 仅合成 Cargo 的产物记录；路径指向测试实际写盘的同一个 worker。
        return (Get-Content -LiteralPath $env:R24_RECORDS -Raw -Encoding UTF8 | ConvertFrom-Json)
    }
    if ($Arguments -contains '--bins' -and $Arguments -notcontains '--manifest-path') {
        $script:mainCalls++
        $packageArguments = @()
        for ($index = 0; $index -lt $Arguments.Count - 1; $index++) {
            if ($Arguments[$index] -ceq '-p') {$packageArguments += $Arguments[$index + 1]}
        }
        if (($packageArguments -join ',') -cne 'jchtools,jchocr') {throw '主构建必须实际选择 GUI 与独立 CLI package'}
        if ($env:R24_FAIL_MAIN -eq '1') {throw 'controlled main build failure'}
        # 直接在生产主构建调用边界观察输入，不伪造“转发成功”的清单结果。
        $path = $env:JCHTOOLS_SNAP_OCR_MANIFEST
        if (-not $path -or -not [IO.Path]::IsPathRooted($path)) {throw '主构建清单必须是实际绝对路径'}
        $script:buildInput = [pscustomobject]@{
            path = $path
            bytes_base64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($path))
            last_write_time_utc_ticks = (Get-Item -LiteralPath $path).LastWriteTimeUtc.Ticks.ToString()
        }
        return @()
    }
    throw '回归驱动遇到未经隔离的 Cargo 调用'
}
$env:JCHTOOLS_SNAP_OCR_MANIFEST = $env:R24_PREVIOUS_MANIFEST
try {. $stage}
finally {
    [IO.File]::WriteAllText($env:R24_RESTORED_ENV, [string]$env:JCHTOOLS_SNAP_OCR_MANIFEST)
}
Complete-PackageTiming
if ($script:workerCalls -ne 1 -or $script:mainCalls -ne 1 -or $null -eq $script:buildInput) {
    throw '必须执行一次真实 worker 阶段和一次真实主构建调用'
}
if ($env:JCHTOOLS_SNAP_OCR_MANIFEST -cne $env:R24_PREVIOUS_MANIFEST) {
    throw '主构建后未恢复调用者的清单环境变量'
}
[IO.File]::WriteAllText($env:R24_OBSERVATION, ($script:buildInput | ConvertTo-Json -Depth 5),
    (New-Object Text.UTF8Encoding($false)))
"""

TIMING_STORAGE_FAILURE_DRIVER = FUNCTION_LOADER + r"""
$script:packageTotalWatch = [Diagnostics.Stopwatch]::StartNew()
Initialize-PackageTiming -RepositoryRoot $env:R24_REPOSITORY
Start-PackageStep 'worker-build'
[IO.File]::WriteAllText($env:R24_TIMING_POINTER, $script:packageTiming.path)
# Read through a sharing-compatible handle while the actual append-only writer is live.
$reader = [IO.File]::Open($script:packageTiming.path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
$saved = New-Object IO.MemoryStream
try {
    $reader.CopyTo($saved)
    [IO.File]::WriteAllBytes($env:R24_SAVED_TIMING, $saved.ToArray())
} finally {$reader.Dispose(); $saved.Dispose()}
# Close the real storage handle to cause an actual append failure, not a mocked writer.
$script:packageTiming.writer.BaseStream.Dispose()
$traps = @($ast.EndBlock.Traps)
if ($traps.Count -ne 1) {throw '必须保留实际打包异常边界'}
. ([scriptblock]::Create($traps[0].Extent.Text + "`r`nthrow 'controlled original packaging failure'"))
"""

TIMING_LIVE_READER_DRIVER = FUNCTION_LOADER + r"""
$script:packageTotalWatch = [Diagnostics.Stopwatch]::StartNew()
Initialize-PackageTiming -RepositoryRoot $env:R24_REPOSITORY
[IO.File]::WriteAllText($env:R24_TIMING_POINTER, $script:packageTiming.path)
# Keep a real reader open across all subsequent writes; it does not share deletion.
$reader = [IO.File]::Open($script:packageTiming.path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
$saved = New-Object IO.MemoryStream
try {
    Start-PackageStep 'worker-build'
    Start-PackageStep 'manifest'
    Complete-PackageTiming
    $reader.CopyTo($saved)
    [IO.File]::WriteAllBytes($env:R24_SAVED_TIMING, $saved.ToArray())
} finally {$reader.Dispose(); $saved.Dispose()}
"""

def read_timing_snapshots(path: Path) -> list[dict]:
    """Consume complete UTF-8 JSONL snapshots; an interrupted final append is not evidence."""
    snapshots = [
        json.loads(line)
        for line in path.read_bytes().splitlines(keepends=True)
        if line.endswith(b"\n")
    ]
    if not snapshots:
        raise ValueError(f"No complete timing snapshot in {path}")
    return snapshots



class PackageWindowsArtifactTests(unittest.TestCase):
    """执行部署脚本的真实 artifact 选择和状态文件读取边界。"""

    @classmethod
    def setUpClass(cls) -> None:
        if os.name != "nt":
            raise RuntimeError("打包回归只在 Windows 上执行")
        cls.powershell = shutil.which("pwsh.exe") or shutil.which("powershell.exe")
        if cls.powershell is None:
            raise RuntimeError("缺少 PowerShell（pwsh.exe 或 powershell.exe）")
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
        self.engine_manifest = self.root / "crates" / "file-core" / "Cargo.toml"
        self.records = [
            {
                "reason": "compiler-artifact",
                "package_id": "path+file:///r24#jchtools@0.1.0",
                "manifest_path": str(self.manifest),
                "target": {"name": "JchTools", "kind": ["bin"]},
                "executable": str(self.executable),
            },
            {
                "reason": "compiler-artifact",
                "package_id": "path+file:///r24/crates/file-core#jchtools-file-core@0.1.0",
                "manifest_path": str(self.engine_manifest),
                "target": {"name": "jchtools_file_core", "kind": ["lib"]},
                "executable": None,
            },
            {
                "reason": "build-script-executed",
                "package_id": "path+file:///r24/crates/file-core#jchtools-file-core@0.1.0",
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
            R24_FUNCTIONS="Get-CargoArtifact,Get-CargoLibraryArtifact,Assert-EmbeddedEngine",
            R24_RECORDS=str(records_file),
            R24_BINARY="JchTools",
            R24_MANIFEST=str(self.manifest),
            R24_ENGINE_MANIFEST=str(self.engine_manifest),
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
        self.records[2]["package_id"] = "registry+https://example.invalid#foreign@1.0.0"
        self.assertNotEqual(self.run_selection().returncode, 0)

    def test_ambiguous_active_build_records_fail_closed(self) -> None:
        self.write_status("ok")
        self.records.append(dict(self.records[2]))
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
        self.assertFalse((destination / "jchtools-assets-0.1.0").exists(), "workspace 自有 crate 不属于第三方")

    def prepare_snap_manifest_fixture(self) -> Path:
        resources = self.root / "resources"
        resources.mkdir()
        shutil.copyfile(ROOT / "resources" / "snap-ocr-assets.json", resources / "snap-ocr-assets.json")
        worker = self.root / "worker 构建目录 with spaces" / "x86_64-pc-windows-msvc" / "release" / "snap-ocr-worker.exe"
        worker.parent.mkdir(parents=True)
        worker.write_bytes(b"MZ\x00synthetic worker executable fixture\x00version-one")
        return worker

    def run_snap_manifest_stage(self, worker: Path, destination: Path, *, fail_main: bool = False) -> dict:
        records_file = self.root / "worker-cargo-records.json"
        records_file.write_text(
            json.dumps([{
                "reason": "compiler-artifact",
                "package_id": "path+file:///r24#snap-ocr-worker@0.1.2",
                "manifest_path": str(self.root / "optional" / "snap-ocr-worker" / "Cargo.toml"),
                "target": {"name": "snap-ocr-worker", "kind": ["bin"]},
                "executable": str(worker),
            }]),
            encoding="utf-8",
        )
        driver = self.root / "snap-manifest-stage.ps1"
        driver.write_text(SNAP_MANIFEST_DRIVER, encoding="utf-8-sig")
        observation = self.root / "snap-manifest-observation.json"
        observation.unlink(missing_ok=True)
        source = self.root / "resources" / "snap-ocr-assets.json"
        source_bytes = source.read_bytes()
        environment = os.environ.copy()
        timing_pointer = self.root / "timing-pointer.txt"
        restored_env = self.root / "restored-env.txt"
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_ROOT=str(self.root),
            R24_DESTINATION=str(destination),
            R24_RECORDS=str(records_file),
            R24_OBSERVATION=str(observation),
            R24_PREVIOUS_MANIFEST=str(self.root / "caller-manifest-not-used.json"),
            R24_REPOSITORY=str(ROOT),
            R24_TIMING_POINTER=str(timing_pointer),
            R24_RESTORED_ENV=str(restored_env),
            R24_FAIL_MAIN="1" if fail_main else "0",
        )
        result = subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=self.root,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        timing_path = Path(timing_pointer.read_text(encoding="utf-8"))
        self.addCleanup(shutil.rmtree, timing_path.parent)
        timing_snapshots = read_timing_snapshots(timing_path)
        timing = timing_snapshots[-1]
        self.assertEqual(restored_env.read_text(encoding="utf-8"), environment["R24_PREVIOUS_MANIFEST"])
        if fail_main:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertIn("controlled main build failure", result.stderr)
            return timing
        self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        self.assertEqual(source.read_bytes(), source_bytes, "生产阶段不得修改 pending-build 源清单")
        snapshot = json.loads(observation.read_text(encoding="utf-8"))
        manifest_bytes = base64.b64decode(snapshot["bytes_base64"], validate=True)
        self.assertEqual(Path(snapshot["path"]).read_bytes(), manifest_bytes)
        stage = destination / "optional-components-v0.1.2"
        self.assertEqual((stage / "snap-ocr-assets.json").read_bytes(), manifest_bytes)
        self.assertEqual((stage / "snap-ocr-worker.exe").read_bytes(), worker.read_bytes())
        manifest = json.loads(manifest_bytes)
        source_manifest = json.loads(source_bytes)
        self.assertEqual(
            {key: value for key, value in manifest.items() if key != "workers"},
            {key: value for key, value in source_manifest.items() if key != "workers"},
        )
        self.assertEqual(len(manifest["workers"]), 1)
        built_worker = manifest["workers"][0]
        self.assertEqual(built_worker["status"], "ok")
        self.assertEqual(built_worker["size_bytes"], worker.stat().st_size)
        self.assertEqual(built_worker["sha256"], hashlib.sha256(worker.read_bytes()).hexdigest())
        self.assertEqual(
            {key: value for key, value in built_worker.items() if key not in {"status", "size_bytes", "sha256"}},
            {key: value for key, value in source_manifest["workers"][0].items()
             if key not in {"status", "size_bytes", "sha256"}},
        )
        snapshot["timing"] = timing
        snapshot["timing_snapshots"] = timing_snapshots
        return snapshot

    def test_snap_manifest_build_input_is_independent_of_destination_root(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        first = self.run_snap_manifest_stage(worker, self.root / "交付目录 A")
        second = self.run_snap_manifest_stage(worker, self.root / "交付目录 B")
        for field in ("path", "bytes_base64", "last_write_time_utc_ticks"):
            with self.subTest(build_input=field):
                self.assertEqual(first[field], second[field], f"仅变更 DestinationRoot 不得改变主构建输入 {field}")

    def test_snap_manifest_unchanged_worker_preserves_bytes_and_last_write_time(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        destination = self.root / "重复交付目录"
        first = self.run_snap_manifest_stage(worker, destination)
        manifest_path = Path(first["path"])
        # 使用确定的旧时间，避免依赖 sleep 或文件系统的写入时间分辨率。
        stamp = 946684800
        os.utime(manifest_path, (stamp, stamp))
        expected_ticks = str(621355968000000000 + manifest_path.stat().st_mtime_ns // 100)
        second = self.run_snap_manifest_stage(worker, destination)
        self.assertEqual(first["path"], second["path"])
        self.assertEqual(first["bytes_base64"], second["bytes_base64"])
        self.assertEqual(
            second["last_write_time_utc_ticks"], expected_ticks,
            "worker 与源清单均未变化时不得重写 Cargo rerun-if-changed 输入",
        )

    def test_snap_manifest_changed_worker_refreshes_sha256_and_size(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        destination = self.root / "worker 更新交付目录"
        first = self.run_snap_manifest_stage(worker, destination)
        original_size = worker.stat().st_size
        worker.write_bytes(b"MZ\x00real changed worker bytes\x00version-two-with-a-different-size")
        self.assertNotEqual(worker.stat().st_size, original_size)
        second = self.run_snap_manifest_stage(worker, destination)
        self.assertNotEqual(first["bytes_base64"], second["bytes_base64"], "真实 worker 变更不得静默复用旧清单")
        before = json.loads(base64.b64decode(first["bytes_base64"]))["workers"][0]
        after = json.loads(base64.b64decode(second["bytes_base64"]))["workers"][0]
        self.assertNotEqual(before["sha256"], after["sha256"])
        self.assertNotEqual(before["size_bytes"], after["size_bytes"])

    def test_snap_manifest_corrupt_content_addressed_input_is_repaired(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        first = self.run_snap_manifest_stage(worker, self.root / "清单篡改交付 A")
        Path(first["path"]).write_bytes(b"corrupt input")
        second = self.run_snap_manifest_stage(worker, self.root / "清单篡改交付 B")
        self.assertEqual(first["path"], second["path"])
        self.assertEqual(first["bytes_base64"], second["bytes_base64"])

    def test_snap_manifest_source_content_change_selects_new_build_input(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        first = self.run_snap_manifest_stage(worker, self.root / "源清单变更交付 A")
        source = self.root / "resources" / "snap-ocr-assets.json"
        manifest = json.loads(source.read_text(encoding="utf-8"))
        manifest["version"] = "regression-real-source-change"
        source.write_text(json.dumps(manifest, ensure_ascii=False), encoding="utf-8")
        second = self.run_snap_manifest_stage(worker, self.root / "源清单变更交付 B")
        self.assertNotEqual(first["path"], second["path"])
        self.assertNotEqual(first["bytes_base64"], second["bytes_base64"])
        self.assertEqual(json.loads(base64.b64decode(second["bytes_base64"]))["version"], manifest["version"])

    def test_main_build_failure_preserves_completed_times_and_unrun_steps(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        timing = self.run_snap_manifest_stage(worker, self.root / "失败交付目录", fail_main=True)
        steps = {entry["step"]: entry for entry in timing["steps"]}
        for name in ("worker-build", "manifest"):
            self.assertEqual(steps[name]["status"], "PASS")
            self.assertGreater(steps[name]["elapsed_seconds"], 0)
            self.assertIsNotNone(steps[name]["finished_utc"])
        self.assertEqual(steps["main-build"]["status"], "FAIL")
        self.assertIn("controlled main build failure", steps["main-build"]["detail"])
        for name in ("staging", "licenses", "package-validation", "installer", "zip"):
            self.assertEqual(steps[name]["status"], "NOT_RUN")
            self.assertEqual(steps[name]["elapsed_seconds"], 0)
            self.assertIsNone(steps[name]["started_utc"])
        self.assertEqual(timing["total"]["status"], "FAIL")
        self.assertGreaterEqual(timing["total"]["elapsed_seconds"], sum(entry["elapsed_seconds"] for entry in timing["steps"]))

    def test_successful_stage_records_transitions_without_claiming_delivery(self) -> None:
        worker = self.prepare_snap_manifest_fixture()
        snapshot = self.run_snap_manifest_stage(worker, self.root / "阶段计时交付目录")
        timing = snapshot["timing"]
        steps = {entry["step"]: entry for entry in timing["steps"]}
        for name in ("worker-build", "manifest", "main-build"):
            self.assertEqual(steps[name]["status"], "PASS")
            self.assertGreater(steps[name]["elapsed_seconds"], 0)
            self.assertIsNotNone(steps[name]["started_utc"])
            self.assertIsNotNone(steps[name]["finished_utc"])
        self.assertEqual(steps["installer"]["status"], "NOT_RUN")
        self.assertEqual(steps["zip"]["status"], "NOT_RUN")
        self.assertEqual(timing["total"]["status"], "PASS")
        history = snapshot["timing_snapshots"]
        self.assertGreater(len(history), 1)
        for name in ("worker-build", "manifest", "main-build"):
            statuses = {
                entry["status"]
                for record in history for entry in record["steps"]
                if entry["step"] == name
            }
            self.assertIn("RUNNING", statuses)
            self.assertIn("PASS", statuses)

    def test_live_reader_observes_appended_transitions_and_ignores_incomplete_tail(self) -> None:
        driver = self.root / "timing-live-reader.ps1"
        driver.write_text(TIMING_LIVE_READER_DRIVER, encoding="utf-8-sig")
        pointer = self.root / "timing-reader-pointer.txt"
        saved = self.root / "timing-reader-observed.jsonl"
        environment = os.environ.copy()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Initialize-PackageTiming,Save-PackageTiming,Stop-PackageStep,Start-PackageStep,Complete-PackageTiming",
            R24_REPOSITORY=str(ROOT),
            R24_TIMING_POINTER=str(pointer),
            R24_SAVED_TIMING=str(saved),
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
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        timing_path = Path(pointer.read_text(encoding="utf-8"))
        self.addCleanup(shutil.rmtree, timing_path.parent)
        self.assertEqual(saved.read_bytes(), timing_path.read_bytes())
        snapshots = read_timing_snapshots(saved)
        self.assertGreater(len(snapshots), 1)
        final = snapshots[-1]
        self.assertEqual(final["total"]["status"], "PASS")
        steps = {entry["step"]: entry for entry in final["steps"]}
        for name in ("setup", "worker-build", "manifest"):
            self.assertEqual(steps[name]["status"], "PASS")
            self.assertGreater(steps[name]["elapsed_seconds"], 0)
        self.assertEqual(steps["installer"]["status"], "NOT_RUN")
        with saved.open("ab") as stream:
            stream.write(b'{"total":')
        self.assertEqual(read_timing_snapshots(saved), snapshots)

    def test_timing_storage_failure_preserves_previous_record_and_original_error(self) -> None:
        driver = self.root / "timing-storage-failure.ps1"
        driver.write_text(TIMING_STORAGE_FAILURE_DRIVER, encoding="utf-8-sig")
        pointer = self.root / "timing-storage-pointer.txt"
        saved = self.root / "timing-storage-before.json"
        environment = os.environ.copy()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Initialize-PackageTiming,Save-PackageTiming,Stop-PackageStep,Start-PackageStep,Complete-PackageTiming",
            R24_REPOSITORY=str(ROOT),
            R24_TIMING_POINTER=str(pointer),
            R24_SAVED_TIMING=str(saved),
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
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("controlled original packaging failure", result.stderr)
        self.assertIn("Unable to finalize package timing", result.stdout + result.stderr)
        timing_path = Path(pointer.read_text(encoding="utf-8"))
        self.addCleanup(shutil.rmtree, timing_path.parent)
        self.assertEqual(timing_path.read_bytes(), saved.read_bytes())
        steps = {entry["step"]: entry for entry in read_timing_snapshots(saved)[-1]["steps"]}
        self.assertEqual(steps["setup"]["status"], "PASS")
        self.assertGreater(steps["setup"]["elapsed_seconds"], 0)
        self.assertEqual(steps["worker-build"]["status"], "RUNNING")
        self.assertEqual(steps["zip"]["status"], "NOT_RUN")

    def test_missing_engine_library_artifact_cannot_use_main_out_dir(self) -> None:
        self.write_status("ok")
        del self.records[1]
        self.records[1]["package_id"] = self.records[0]["package_id"]
        self.assertNotEqual(self.run_selection().returncode, 0)

    def test_ambiguous_engine_library_artifacts_fail_closed(self) -> None:
        self.write_status("ok")
        self.records.append(dict(self.records[1]))
        self.assertNotEqual(self.run_selection().returncode, 0)

    def test_resolved_patched_packages_use_local_path_sources(self) -> None:
        # 真实 Cargo resolver 查询：仅读取元数据，不编译、不执行任何 crate。
        sources = json.loads((ROOT / "vendor" / "SOURCES.json").read_text(encoding="utf-8"))
        patched = [
            entry for entry in sources["packages"]
            if entry["effective_source"]["type"] == "path"
        ]
        self.assertTrue(patched, "来源清单必须登记实际本地补丁，不能空跑")
        expected_identities = {(entry["name"], entry["version"]) for entry in patched}
        self.assertEqual(len(patched), len(expected_identities), "来源清单不能重复登记同一包")
        for entry in sources["packages"]:
            if entry["project_source_modifications"]:
                self.assertEqual(entry["effective_source"]["type"], "path", entry["name"])
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
            {
                "jchtools", "snap-ocr-core", "snap-ocr-worker", "jchocr",
                "jchtools-foundation", "jchtools-file-core", "jchtools-recursive-extract",
                "jchtools-directory-organizer", "jchtools-md-tools", "jchtools-git-tools",
                "jchtools-xberg", "jchtools-assets", "jchtools-markdown", "jchtools-acp-api",
                "jchtools-codeagent", "jchtools-ui", "jchtools-gui",
            },
        )
        workspace_members = set(metadata["workspace_members"])
        resolved_local_identities = {
            (package["name"], package["version"]) for package in packages.values()
            if package["source"] is None and package["id"] not in workspace_members
        }
        self.assertEqual(resolved_local_identities, expected_identities)
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        pending = list(metadata["workspace_members"])
        reachable = set()
        while pending:
            identity = pending.pop()
            if identity in reachable:
                continue
            reachable.add(identity)
            pending.extend(dependency["pkg"] for dependency in nodes[identity]["deps"])
        for entry in patched:
            name, version = entry["name"], entry["version"]
            matches = [
                package for package in packages.values()
                if package["name"] == name and package["version"] == version
            ]
            self.assertEqual(len(matches), 1, (name, version, matches))
            package = matches[0]
            self.assertIsNone(package["source"], (name, package["source"]))
            self.assertEqual(
                Path(package["manifest_path"]).resolve(),
                (ROOT / entry["effective_source"]["path"] / "Cargo.toml").resolve(),
            )
            self.assertIn(package["id"], reachable, name)


if __name__ == "__main__":
    unittest.main()
