"""覆盖 E-03/E-05：打包读取本次 Cargo 产物及其内嵌状态，不猜测缓存时间。"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid
from pathlib import Path
import zipfile

if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from scripts.test_gate import _ps51_module_path

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

DRIVER = (
    FUNCTION_LOADER
    + r"""
$records = @(Get-Content -LiteralPath $env:R24_RECORDS -Raw -Encoding UTF8 | ConvertFrom-Json)
$artifact = Get-CargoArtifact -Messages $records -BinaryName $env:R24_BINARY -ManifestPath $env:R24_MANIFEST
if ($env:R24_CHECK_ENGINE -eq '1') {
    $engineArtifact = Get-CargoLibraryArtifact -Messages $records -LibraryName 'jchtools_file_core' -ManifestPath $env:R24_ENGINE_MANIFEST
    Assert-EmbeddedEngine -Messages $records -Artifact $engineArtifact
}
[Console]::WriteLine($artifact.executable)
"""
)

LICENSE_DRIVER = (
    FUNCTION_LOADER
    + r"""
$package = Get-Content -LiteralPath $env:R24_PACKAGE -Raw -Encoding UTF8 | ConvertFrom-Json
$sourcePackage = Get-Content -LiteralPath $env:R24_SOURCE_PACKAGE -Raw -Encoding UTF8 | ConvertFrom-Json
$count = Copy-PackageLicenseMaterials -Package $package -SourcePackage $sourcePackage -Destination $env:R24_DESTINATION
[Console]::WriteLine($count)
"""
)

INSTALLER_COMPILER_DRIVER = (
    FUNCTION_LOADER
    + r"""
[Console]::WriteLine("实际安装包编译器：$env:R24_ISCC")
$banner = & $env:R24_ISCC '/?' | Out-String
[Console]::WriteLine($banner)
if ($banner -notmatch '\bInno Setup 6 Command-Line Compiler\b') {throw '深路径回归必须使用真实 Inno Setup 6；参见上述原生编译器路径与横幅'}
Invoke-InstallerCompiler -CompilerPath $env:R24_ISCC -SourceDirectory $env:R24_SOURCE_DIRECTORY -OutputDirectory $env:R24_DESTINATION -Version '0.0.0' -RepositoryRoot $env:R24_REPOSITORY -InstallerScript $env:R24_INSTALLER_SCRIPT
"""
)

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

TIMING_STORAGE_FAILURE_DRIVER = (
    FUNCTION_LOADER
    + r"""
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
)

TIMING_LIVE_READER_DRIVER = (
    FUNCTION_LOADER
    + r"""
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
)

# 执行实际默认 tests 分支，只替换 Cargo 和测试程序的进程边界。
# 分类器与桌面跳过名单必须来自现有 local_gate_plan，不在此复制选择实现。
PACKAGE_TEST_STAGE_DRIVER = (
    FUNCTION_LOADER
    + r"""
$root = $env:R25_REPOSITORY
Set-Location -LiteralPath $root
$SkipTests = $false
$extra = @('--offline')
$archiveEngine = 'synthetic engine; never executed'
$script:packageTiming = @{path=(Join-Path $env:R25_WORKSPACE 'timings.jsonl')}
$env:JCHTOOLS_TEST_7ZIP = 'caller-engine-value'
function Start-PackageStep([string]$Name) {}
function Invoke-Cargo([string[]]$Arguments, [switch]$CaptureBuildMetadata) {
    $global:LASTEXITCODE = 0
    if ($Arguments[0] -ceq 'check') {return}
    if ($Arguments[0] -ceq 'test' -and $Arguments -contains '--no-run' -and $CaptureBuildMetadata) {
        return (Get-Content -LiteralPath $env:R25_RECORDS -Raw -Encoding UTF8 | ConvertFrom-Json)
    }
    if ($Arguments[0] -ceq 'test' -and $Arguments -contains '--ignored' -and
        $Arguments -contains 'archive' -and $env:JCHTOOLS_TEST_7ZIP -ceq $archiveEngine) {
        [IO.File]::WriteAllText((Join-Path $env:R25_WORKSPACE 'real-engine-step.txt'), 'executed')
        return
    }
    throw '默认打包不得直接执行未经桌面分类的 Cargo 测试'
}
$blocks = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.IfStatementAst] -and
    $_.Clauses[0].Item1.Extent.Text -ceq '-not $SkipTests'
})
if ($blocks.Count -ne 1) {throw '无法唯一定位默认打包 tests 分支'}
. ([scriptblock]::Create($blocks[0].Extent.Text))
if ($env:JCHTOOLS_TEST_7ZIP -cne 'caller-engine-value') {throw '真实引擎测试后必须恢复调用者环境'}
"""
)

# 运行验收入口的真实默认/显式桌面决策以及 -WithPackage 分支；
# 只替换 Cargo 与打包的外部进程边界，Python 分类和已分类程序仍经真实 Invoke-Logged。
ACCEPTANCE_TEST_STAGE_DRIVER = (
    FUNCTION_LOADER
    + r"""
$root = $env:R25_REPOSITORY
Set-Location -LiteralPath $root
$python = $env:R25_PYTHON
$WithDesktopTests = $env:R25_DESKTOP -ceq '1'
$WithPackage = $true
$testEnvironment = @{}
$testLogs = @()
$script:LogDir = $env:R25_WORKSPACE
$script:Results = [Collections.Generic.List[string]]::new()
$script:calls = [Collections.Generic.List[object]]::new()
$script:realInvokeLogged = ${function:Invoke-Logged}
function Invoke-Logged {
    param([string]$Name,[string]$File,[string[]]$Arguments = @(),[hashtable]$Environment)
    if ($File -ceq 'cargo' -or $File -ceq 'powershell') {
        $script:calls.Add(@{name=$Name;file=$File;argv=$Arguments})
        $log = Join-Path $script:LogDir ($Name + '.log')
        if ($File -ceq 'cargo' -and $Arguments -contains '--no-run') {
            $records = Get-Content -LiteralPath $env:R25_RECORDS -Raw -Encoding UTF8 | ConvertFrom-Json
            $records | ForEach-Object {$_ | ConvertTo-Json -Depth 20 -Compress} |
                Set-Content -LiteralPath $log -Encoding UTF8
        } else {[IO.File]::WriteAllText($log, 'isolated external process')}
        return $log
    }
    & $script:realInvokeLogged @PSBoundParameters
}
$decisions = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.IfStatementAst] -and
    $_.Clauses[0].Item1.Extent.Text -ceq '$WithDesktopTests'
})
$packages = @($ast.EndBlock.Statements | Where-Object {
    $_ -is [Management.Automation.Language.IfStatementAst] -and
    $_.Clauses[0].Item1.Extent.Text -ceq '$WithPackage'
})
if ($decisions.Count -ne 1 -or $packages.Count -ne 1) {throw '无法唯一定位验收测试与打包入口'}
# 保留原始脚本文件名；无文件名的 ScriptBlock::Create 会把自动变量 PSScriptRoot 置空。
foreach ($branch in @($decisions[0], $packages[0])) {
    $branchAst = [Management.Automation.Language.Parser]::ParseInput(
        $branch.Extent.Text, $env:R24_SCRIPT, [ref]$tokens, [ref]$errors)
    if ($errors.Count) {throw '实际验收分支语法无效'}
    . ($branchAst.GetScriptBlock())
}
[IO.File]::WriteAllText((Join-Path $script:LogDir 'acceptance-observation.json'),
    (@{calls=@($script:calls.ToArray());results=@($script:Results.ToArray())} | ConvertTo-Json -Depth 10),
    [Text.UTF8Encoding]::new($false))
"""
)


def read_timing_snapshots(path: Path) -> list[dict]:
    """Consume complete UTF-8 JSONL snapshots; an interrupted final append is not evidence."""
    snapshots = [json.loads(line) for line in path.read_bytes().splitlines(keepends=True) if line.endswith(b"\n")]
    if not snapshots:
        raise ValueError(f"No complete timing snapshot in {path}")
    return snapshots


class PackageWindowsArtifactTests(unittest.TestCase):
    """执行部署脚本的真实 artifact 选择和状态文件读取边界。"""

    @classmethod
    def setUpClass(cls) -> None:
        if os.name != "nt":
            raise RuntimeError("打包回归只在 Windows 上执行")
        cls.powershell = shutil.which("powershell.exe") or shutil.which("pwsh.exe")
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
        self.out_dir = (
            self.root / "构建目录 with spaces" / "x86_64-pc-windows-msvc" / "release" / "build" / "active" / "out"
        )
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

    def powershell_environment(self) -> dict[str, str]:
        environment = os.environ.copy()
        if Path(self.powershell).name.lower() == "powershell.exe":
            # 复用原生 5.1 的模块路径策略，不继承宿主 PowerShell 7 的不兼容模块。
            module_path = _ps51_module_path()
            if module_path is not None:
                environment["PSModulePath"] = module_path
        return environment

    def run_selection(self, *, engine: bool = True) -> subprocess.CompletedProcess[str]:
        records_file = self.root / "cargo-records.json"
        records_file.write_text(json.dumps(self.records), encoding="utf-8")
        driver = self.root / "selection.ps1"
        driver.write_text(DRIVER, encoding="utf-8-sig")
        environment = self.powershell_environment()
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

    def installer_compiler(self) -> Path:
        candidates = [
            shutil.which("ISCC.exe"),
            str(Path(os.environ.get("LOCALAPPDATA", "")) / "Programs" / "Inno Setup 6" / "ISCC.exe"),
            str(Path(os.environ.get("ProgramFiles(x86)", "")) / "Inno Setup 6" / "ISCC.exe"),
            str(Path(os.environ.get("ProgramFiles", "")) / "Inno Setup 6" / "ISCC.exe"),
        ]
        for candidate in candidates:
            if candidate and Path(candidate).is_file():
                return Path(candidate)
        self.fail("深路径回归需要真实 Inno Setup 6；不得跳过或伪造编译器")

    @staticmethod
    def extended_path(path: Path) -> Path:
        """Create/read/delete the fixture without depending on Windows long-path policy."""
        absolute = str(path.absolute())
        if absolute.startswith("\\\\"):
            return Path("\\\\?\\UNC\\" + absolute[2:])
        return Path("\\\\?\\" + absolute)

    def installer_source_snapshot(self, source: Path) -> dict:
        extended = self.extended_path(source)
        snapshot = {}
        for directory, directories, files in os.walk(extended):
            for name in directories + files:
                member = Path(directory) / name
                metadata = member.stat()
                snapshot[str(member.relative_to(extended))] = (
                    member.is_dir(),
                    metadata.st_ino,
                    metadata.st_size,
                    metadata.st_mtime_ns,
                    None if member.is_dir() else hashlib.sha256(member.read_bytes()).hexdigest(),
                )
        return snapshot

    def run_installer_compiler(
        self, source: Path, destination: Path, installer_script: Path
    ) -> subprocess.CompletedProcess[str]:
        driver = self.root / "installer-compiler.ps1"
        driver.write_text(INSTALLER_COMPILER_DRIVER, encoding="utf-8-sig")
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Invoke-InstallerCompiler",
            R24_REPOSITORY=str(ROOT),
            R24_ISCC=str(self.installer_compiler()),
            R24_SOURCE_DIRECTORY=str(source),
            R24_DESTINATION=str(destination),
            R24_INSTALLER_SCRIPT=str(installer_script),
        )
        return subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=90,
            check=False,
        )

    def test_inno6_compiles_deep_hf_source_without_mutation_or_alias_leak(self) -> None:
        # Synthetic storage-only payload: neither executable nor model is run or claimed to support OCR.
        import winreg

        model_relative = Path(
            "xberg/models/models--xberg-io--paddleocr-onnx-models/snapshots/"
            "bc5ec866cf0e798e667808dfa51b0ba8ad0dafc8/v2/classifiers/PP-LCNet_x1_0_textline_ori.onnx"
        )
        # Same failure geometry as the real release: the file crosses MAX_PATH, its parent does not.
        source_parent = ROOT / ".tmp"
        source_name_length = 262 - len(str(source_parent)) - len(str(model_relative)) - 2
        unique_name = "p-" + uuid.uuid4().hex
        self.assertGreaterEqual(source_name_length, len(unique_name))
        source = source_parent / unique_name.ljust(source_name_length, "s")
        self.assertEqual(len(str(source / model_relative)), 262)
        self.assertLess(len(str((source / model_relative).parent)), 260)
        extended = self.extended_path(source)
        extended.mkdir(parents=True)
        self.addCleanup(shutil.rmtree, extended)
        members = {
            Path("JchTools.exe"): b"MZ synthetic non-executed GUI fixture",
            Path("jchocr.exe"): b"MZ synthetic non-executed CLI fixture",
            Path("xberg/xberg.exe"): b"MZ synthetic non-executed Xberg fixture",
            Path("xberg/jchtools-xberg-release.json"): b'{"fixture":"synthetic non-OCR storage-only payload"}',
            model_relative: b"synthetic model bytes; no inference claim",
        }
        for relative, payload in members.items():
            member = extended / relative
            member.parent.mkdir(parents=True, exist_ok=True)
            member.write_bytes(payload)
        before = self.installer_source_snapshot(source)
        aliases_before = set((ROOT / ".tmp").glob("i-*"))
        destination = self.root / "installer-output"
        destination.mkdir()
        # Include the real installer with its supported isolation defines. Silent installation
        # suppresses [Run]; no fake payload is executed, no uninstall entry or real shortcut is made.
        isolation = ROOT / ".tmp" / ("s-" + uuid.uuid4().hex[:12])
        isolation.mkdir()
        self.addCleanup(shutil.rmtree, self.extended_path(isolation))
        self.assertLess(len(str(isolation / "installed" / model_relative)), 260)
        app_id = uuid.uuid4().hex
        registry_key = rf"Software\JchTools-Review-{app_id}"
        with self.assertRaises(FileNotFoundError):
            winreg.OpenKey(winreg.HKEY_CURRENT_USER, registry_key)

        def remove_isolated_registry_key() -> None:
            try:
                winreg.DeleteKey(winreg.HKEY_CURRENT_USER, registry_key)
            except FileNotFoundError:
                pass

        self.addCleanup(remove_isolated_registry_key)
        installer_script = self.root / "isolated-installer.iss"
        installer_script.write_text(
            f'#define ReviewRemediationRoot "{isolation}"\n'
            f'#define ReviewRemediationAppId "{app_id}"\n'
            f'#include "{ROOT / "installer" / "JchTools.iss"}"\n'
            "[Setup]\n"
            f"SourceDir={ROOT / 'installer'}\n"
            "Uninstallable=no\n",
            encoding="utf-8-sig",
        )
        result = self.run_installer_compiler(source, destination, installer_script)
        diagnostics = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertEqual(self.installer_source_snapshot(source), before, diagnostics)
        self.assertEqual(set((ROOT / ".tmp").glob("i-*")), aliases_before, diagnostics)
        self.assertEqual(result.returncode, 0, diagnostics)
        setup = destination / "JchTools-Setup-x64.exe"
        self.assertTrue(setup.is_file(), diagnostics)
        self.assertGreater(setup.stat().st_size, 0, diagnostics)
        install_log = self.root / "isolated-install.log"
        installed = subprocess.run(
            [
                str(setup),
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/SP-",
                "/NOICONS",
                "/TASKS=!desktopicon,!quicklaunchicon",
                f"/LOG={install_log}",
            ],
            cwd=self.root,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=90,
            check=False,
        )
        install_diagnostics = (
            diagnostics + f"\ninstaller exit={installed.returncode}\nstdout:\n{installed.stdout}"
            f"\nstderr:\n{installed.stderr}\nlog:\n"
            + (install_log.read_text(encoding="utf-8-sig", errors="replace") if install_log.exists() else "(missing)")
        )
        self.assertEqual(installed.returncode, 0, install_diagnostics)
        for relative, payload in members.items():
            installed_path = (
                isolation / "WindowsApps" / relative.name
                if relative == Path("jchocr.exe")
                else isolation / "installed" / relative
            )
            extended_installed = self.extended_path(installed_path)
            self.assertTrue(extended_installed.is_file(), f"{relative}\n{install_diagnostics}")
            self.assertEqual(extended_installed.read_bytes(), payload, f"{relative}\n{install_diagnostics}")
        self.assertEqual(self.installer_source_snapshot(source), before, install_diagnostics)
        self.assertEqual(set((ROOT / ".tmp").glob("i-*")), aliases_before, install_diagnostics)

    def test_inno6_native_compile_error_removes_alias_without_mutating_source(self) -> None:
        source = self.root / "native-error-source"
        source.mkdir()
        for relative, payload in {
            "JchTools.exe": b"MZ synthetic non-executed GUI",
            "jchocr.exe": b"MZ synthetic non-executed CLI",
            "xberg/xberg.exe": b"MZ synthetic non-executed Xberg",
            "xberg/jchtools-xberg-release.json": b'{"fixture":"synthetic non-OCR"}',
            "xberg/models/retained.onnx": b"retained model bytes; no inference claim",
        }.items():
            member = source / relative
            member.parent.mkdir(parents=True, exist_ok=True)
            member.write_bytes(payload)
        installer_script = self.root / "native-error.iss"
        installer_script.write_text(
            f'#include "{ROOT / "installer" / "JchTools.iss"}"\n#error "JCHTOOLS_EXPECTED_NATIVE_COMPILER_ERROR"\n',
            encoding="utf-8-sig",
        )
        destination = self.root / "native-error-output"
        destination.mkdir()
        before = self.installer_source_snapshot(source)
        aliases_before = set((ROOT / ".tmp").glob("i-*"))
        result = self.run_installer_compiler(source, destination, installer_script)
        diagnostics = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        self.assertNotEqual(result.returncode, 0, diagnostics)
        self.assertIn("JCHTOOLS_EXPECTED_NATIVE_COMPILER_ERROR", result.stdout + result.stderr, diagnostics)
        self.assertIn("Inno Setup compiler failed: 2", result.stdout + result.stderr, diagnostics)
        self.assertEqual(self.installer_source_snapshot(source), before, diagnostics)
        self.assertEqual(set((ROOT / ".tmp").glob("i-*")), aliases_before, diagnostics)
        self.assertFalse((destination / "JchTools-Setup-x64.exe").exists(), diagnostics)

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

    def run_license_copy(
        self, package_name: str, *, missing: bool = False
    ) -> tuple[subprocess.CompletedProcess[str], dict, Path]:
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
        environment = self.powershell_environment()
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
        environment = self.powershell_environment()
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
                    (destination / f"{entry['name']}-{entry['version']}" / relative).read_bytes(),
                    (ROOT / entry["directory"] / relative).read_bytes(),
                )
        self.assertFalse((destination / "jchtools-assets-0.1.0").exists(), "workspace 自有 crate 不属于第三方")

    def prepare_snap_manifest_fixture(self) -> Path:
        resources = self.root / "resources"
        resources.mkdir()
        shutil.copyfile(ROOT / "resources" / "snap-ocr-assets.json", resources / "snap-ocr-assets.json")
        worker = (
            self.root / "worker 构建目录 with spaces" / "x86_64-pc-windows-msvc" / "release" / "snap-ocr-worker.exe"
        )
        worker.parent.mkdir(parents=True)
        worker.write_bytes(b"MZ\x00synthetic worker executable fixture\x00version-one")
        return worker

    def run_snap_manifest_stage(self, worker: Path, destination: Path, *, fail_main: bool = False) -> dict:
        records_file = self.root / "worker-cargo-records.json"
        records_file.write_text(
            json.dumps(
                [
                    {
                        "reason": "compiler-artifact",
                        "package_id": "path+file:///r24#snap-ocr-worker@0.1.2",
                        "manifest_path": str(self.root / "optional" / "snap-ocr-worker" / "Cargo.toml"),
                        "target": {"name": "snap-ocr-worker", "kind": ["bin"]},
                        "executable": str(worker),
                    }
                ]
            ),
            encoding="utf-8",
        )
        driver = self.root / "snap-manifest-stage.ps1"
        driver.write_text(SNAP_MANIFEST_DRIVER, encoding="utf-8-sig")
        observation = self.root / "snap-manifest-observation.json"
        observation.unlink(missing_ok=True)
        source = self.root / "resources" / "snap-ocr-assets.json"
        source_bytes = source.read_bytes()
        environment = self.powershell_environment()
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
            {
                key: value
                for key, value in source_manifest["workers"][0].items()
                if key not in {"status", "size_bytes", "sha256"}
            },
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
            second["last_write_time_utc_ticks"],
            expected_ticks,
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
        self.assertGreaterEqual(
            timing["total"]["elapsed_seconds"], sum(entry["elapsed_seconds"] for entry in timing["steps"])
        )

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
            statuses = {entry["status"] for record in history for entry in record["steps"] if entry["step"] == name}
            self.assertIn("RUNNING", statuses)
            self.assertIn("PASS", statuses)

    def test_live_reader_observes_appended_transitions_and_ignores_incomplete_tail(self) -> None:
        driver = self.root / "timing-live-reader.ps1"
        driver.write_text(TIMING_LIVE_READER_DRIVER, encoding="utf-8-sig")
        pointer = self.root / "timing-reader-pointer.txt"
        saved = self.root / "timing-reader-observed.jsonl"
        environment = self.powershell_environment()
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
        environment = self.powershell_environment()
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

    def test_default_package_tests_use_existing_non_desktop_classifier(self) -> None:
        # 覆盖 P-12 / AGENTS.md 3.4：-WithPackage 的默认调用也不能激活用户桌面。
        observations = self.root / "test-processes.jsonl"
        records = []
        for name in ("ordinary", "gui_flow", "acp_service_process"):
            executable = self.root / f"{name}.ps1"
            executable.write_text(
                "$ErrorActionPreference = 'Stop'\n"
                f"$record = @{{name='{name}';argv=@($args)}} | ConvertTo-Json -Compress\n"
                '[IO.File]::AppendAllText($env:R25_OBSERVATIONS, $record + "`n", [Text.UTF8Encoding]::new($false))\n'
                "exit 0\n",
                encoding="utf-8-sig",
            )
            records.append(
                {
                    "reason": "compiler-artifact",
                    "profile": {"test": True},
                    "target": {"name": name, "kind": ["test"]},
                    "executable": str(executable),
                }
            )
        records_path = self.root / "test-artifacts.json"
        records_path.write_text(json.dumps(records), encoding="utf-8")
        driver = self.root / "package-tests-stage.ps1"
        driver.write_text(PACKAGE_TEST_STAGE_DRIVER, encoding="utf-8-sig")
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(SCRIPT),
            R24_FUNCTIONS="Invoke-Cargo",
            R25_REPOSITORY=str(ROOT),
            R25_WORKSPACE=str(self.root),
            R25_RECORDS=str(records_path),
            R25_OBSERVATIONS=str(observations),
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
        processes = [json.loads(line) for line in observations.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([process["name"] for process in processes], ["ordinary", "acp_service_process"])
        self.assertEqual(processes[0]["argv"], ["--nocapture", "--test-threads=2"])
        self.assertEqual(
            processes[1]["argv"],
            [
                "--nocapture",
                "--test-threads=2",
                "--skip",
                "real_gui_save_close_kill_reopen_and_confirmed_service_exit",
                "--skip",
                "real_gui_pending_configuration_applies_only_after_inflight_completion",
            ],
        )
        self.assertEqual((self.root / "real-engine-step.txt").read_text(encoding="utf-8"), "executed")
        self.assertIn("NOT RUN", result.stdout)
        self.assertIn("desktop", result.stdout)

    def test_acceptance_package_keeps_desktop_authorization_explicit(self) -> None:
        # 覆盖 P-12/P-13：默认分类不漏普通测试，显式授权仍保留完整桌面入口。
        for desktop in (False, True):
            with self.subTest(desktop=desktop):
                workspace = self.root / ("desktop" if desktop else "default")
                workspace.mkdir()
                observations = workspace / "test-processes.jsonl"
                records = []
                for name in ("ordinary", "gui_flow", "acp_service_process"):
                    executable = workspace / f"{name}.ps1"
                    executable.write_text(
                        f"$record = @{{name='{name}';argv=@($args)}} | ConvertTo-Json -Compress\n"
                        '[IO.File]::AppendAllText($env:R25_OBSERVATIONS, $record + "`n", [Text.UTF8Encoding]::new($false))\n'
                        "exit 0\n",
                        encoding="utf-8-sig",
                    )
                    records.append(
                        {
                            "reason": "compiler-artifact",
                            "profile": {"test": True},
                            "target": {"name": name, "kind": ["test"]},
                            "executable": str(executable),
                        }
                    )
                records_path = workspace / "test-artifacts.json"
                records_path.write_text(json.dumps(records), encoding="utf-8")
                driver = workspace / "acceptance-tests-stage.ps1"
                driver.write_text(ACCEPTANCE_TEST_STAGE_DRIVER, encoding="utf-8-sig")
                environment = self.powershell_environment()
                environment.update(
                    R24_SCRIPT=str(ROOT / "scripts" / "acceptance.ps1"),
                    R24_FUNCTIONS="Invoke-Logged",
                    R25_REPOSITORY=str(ROOT),
                    R25_WORKSPACE=str(workspace),
                    R25_RECORDS=str(records_path),
                    R25_OBSERVATIONS=str(observations),
                    R25_PYTHON=sys.executable,
                    R25_DESKTOP="1" if desktop else "0",
                )
                result = subprocess.run(
                    [
                        self.powershell,
                        "-NoProfile",
                        "-NonInteractive",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                        str(driver),
                    ],
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
                observation = json.loads((workspace / "acceptance-observation.json").read_text(encoding="utf-8"))
                self.assertEqual([call["file"] for call in observation["calls"]], ["cargo", "powershell"])
                expected_cargo = ["test", "--all-targets", "--features", "test-hooks"]
                if not desktop:
                    expected_cargo += ["--no-run", "--message-format=json"]
                self.assertEqual(observation["calls"][0]["argv"], expected_cargo)
                self.assertEqual(
                    observation["calls"][1]["argv"],
                    ["-NoProfile", "-File", str(SCRIPT)],
                )
                if desktop:
                    self.assertFalse(observations.exists())
                    self.assertFalse(any("NOT RUN" in entry for entry in observation["results"]))
                else:
                    processes = [json.loads(line) for line in observations.read_text(encoding="utf-8").splitlines()]
                    self.assertEqual([process["name"] for process in processes], ["ordinary", "acp_service_process"])
                    self.assertEqual(processes[0]["argv"], ["--nocapture", "--test-threads=2"])
                    self.assertEqual(
                        processes[1]["argv"],
                        [
                            "--nocapture",
                            "--test-threads=2",
                            "--skip",
                            "real_gui_save_close_kill_reopen_and_confirmed_service_exit",
                            "--skip",
                            "real_gui_pending_configuration_applies_only_after_inflight_completion",
                        ],
                    )
                    self.assertTrue(any(entry.startswith("NOT RUN  desktop-tests") for entry in observation["results"]))

    def test_missing_engine_library_artifact_cannot_use_main_out_dir(self) -> None:
        self.write_status("ok")
        del self.records[1]
        self.records[1]["package_id"] = self.records[0]["package_id"]
        self.assertNotEqual(self.run_selection().returncode, 0)

    def test_ambiguous_engine_library_artifacts_fail_closed(self) -> None:
        self.write_status("ok")
        self.records.append(dict(self.records[1]))
        self.assertNotEqual(self.run_selection().returncode, 0)

    def xberg_fixture(self) -> dict:
        """Synthetic bytes exercise production archive/policy decisions, not OCR inference."""
        common = {
            name: name.encode()
            for name in (
                "xberg.exe",
                "LICENSE",
                "THIRD_PARTY_LICENSES.md",
                "onnxruntime.dll",
                "onnxruntime_providers_shared.dll",
                "MSVCP140.dll",
                "MSVCP140_1.dll",
                "VCRUNTIME140.dll",
                "VCRUNTIME140_1.dll",
                "models/paddleocr-onnx-models-LICENSE.txt",
                "sherpa-onnx/LICENSE",
                "sherpa-onnx/sherpa-onnx-c-api.dll",
                "sherpa-onnx/sherpa-onnx-cxx-api.dll",
            )
        }
        for name in ("onnxruntime.dll", "onnxruntime_providers_shared.dll"):
            common[f"sherpa-onnx/{name}"] = common[name]
        prefix = "models/models--xberg-io--paddleocr-onnx-models/snapshots/fixed/"
        document = {
            prefix + name: name.encode()
            for name in (
                "v6/det/tiny/model.onnx",
                "v6/rec/tiny/model.onnx",
                "v6/rec/tiny/dict.txt",
                "v2/classifiers/PP-LCNet_x1_0_textline_ori.onnx",
            )
        }
        snapshot = {
            f"models/snapshot-ocr/{name}": ("small-" + name).encode()
            for name in ("det.onnx", "rec.onnx", "dict/dict.txt")
        }
        identity_root = self.root / "identity"
        (identity_root / "resources").mkdir(parents=True)
        (identity_root / "resources" / "markdown-assets.json").write_text(
            json.dumps({"xberg": {"members": [self.xberg_member(p, b) for p, b in (document | snapshot).items()]}}),
            encoding="utf-8",
        )
        components = []
        for name, files in (("document", common | document), ("snapshot", common | snapshot)):
            archive = self.root / f"xberg-{name}-x86_64-pc-windows-msvc.zip"
            with zipfile.ZipFile(archive, "w") as output:
                for path, payload in files.items():
                    output.writestr(f"xberg-cli-x86_64-pc-windows-msvc/{path}", payload)
            components.append(
                {
                    "id": name,
                    "archive": archive.name,
                    "requires": [],
                    "size_bytes": archive.stat().st_size,
                    "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
                    "members": [self.xberg_member(p, b) for p, b in files.items()],
                }
            )
        release_url = "https://github.com/jchanghong023/xberg/releases/download/v-test/"
        audio = {
            "id": "audio-assets",
            "archive": "xberg-audio-assets-x86_64-pc-windows-msvc.zip",
            "requires": [],
            "size_bytes": 123,
            "sha256": "a" * 64,
            "members": [self.xberg_member("ffmpeg/LICENSE.txt", b"license")],
        }
        receipt = {
            "owner": "JchTools-bundled-xberg-v1",
            "tag": "v-test",
            "source": "https://api.github.com/repos/jchanghong023/xberg/releases/latest",
            "manifest": {
                "archive": "xberg-components-x86_64-pc-windows-msvc.json",
                "url": release_url + "xberg-components-x86_64-pc-windows-msvc.json",
                "size_bytes": 123,
                "sha256": "b" * 64,
            },
            "components": components,
            "audio_assets": audio | {"url": release_url + audio["archive"]},
        }
        return {"receipt": receipt, "root": identity_root}

    @staticmethod
    def xberg_member(path: str, payload: bytes) -> dict:
        return {"path": path, "size_bytes": len(payload), "sha256": hashlib.sha256(payload).hexdigest()}

    def run_xberg_policy(self, fixture: dict, action: str = "expand") -> subprocess.CompletedProcess:
        record = self.root / "xberg-receipt.json"
        record.write_text(json.dumps(fixture["receipt"]), encoding="utf-8")
        driver = self.root / "xberg-policy.ps1"
        driver.write_text(
            FUNCTION_LOADER
            + r"""
Add-Type -AssemblyName System.IO.Compression.FileSystem
$receipt = Get-Content -LiteralPath $env:R24_RECORDS -Raw -Encoding UTF8 | ConvertFrom-Json
$stage = Join-Path $env:R24_DESTINATION 'staged'
switch ($env:R24_ACTION) {
    'api' {
        $release = $receipt
        Get-XbergReleaseAsset $release 'xberg-document-x86_64-pc-windows-msvc.zip' | Out-Null
    }
    'cache' {Get-XbergCachedAsset $receipt $env:R24_DESTINATION -OfflineMode | Out-Null}
    'validate' {Assert-XbergPackage $stage $env:R24_ROOT}
    'install' {Install-XbergBaseFiles $stage (Join-Path $env:R24_DESTINATION 'installed') $env:R24_ROOT}
    default {
        $union = Assert-XbergBaseMembers @($receipt.components) $env:R24_ROOT
        New-Item -ItemType Directory -Path $stage -Force | Out-Null
        foreach ($component in $receipt.components) {
            Expand-XbergComponent (Join-Path $env:R24_DESTINATION $component.archive) $component $stage
        }
        Copy-Item -LiteralPath $env:R24_RECORDS -Destination (Join-Path $stage 'jchtools-xberg-release.json') -Force
        Assert-XbergPackage $stage $env:R24_ROOT
    }
}
""",
            encoding="utf-8-sig",
        )
        environment = self.powershell_environment()
        environment.update(
            R24_SCRIPT=str(ROOT / "scripts" / "prepare-xberg-runtime.ps1"),
            R24_FUNCTIONS="Assert-XbergRelativePath,Assert-XbergFile,Get-XbergReleaseAsset,Get-XbergCachedAsset,"
            "Get-XbergMemberMap,Test-XbergAudioPath,Assert-XbergBaseMembers,Expand-XbergComponent,"
            "Assert-XbergPackage,Install-XbergBaseFiles",
            R24_RECORDS=str(record),
            R24_DESTINATION=str(self.root),
            R24_ROOT=str(fixture["root"]),
            R24_ACTION=action,
        )
        return subprocess.run(
            [self.powershell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(driver)],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )

    def rewrite_xberg_zip(self, fixture: dict, members: list[tuple[str, bytes | zipfile.ZipInfo]]) -> None:
        component = fixture["receipt"]["components"][0]
        archive = self.root / component["archive"]
        with zipfile.ZipFile(archive, "w") as output:
            for path, payload in members:
                if isinstance(payload, zipfile.ZipInfo):
                    output.writestr(payload, b"link-target")
                else:
                    output.writestr(path, payload)
        component["size_bytes"] = archive.stat().st_size
        component["sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()

    def test_xberg_complete_base_receipt_and_same_root_refresh_preserve_audio_and_config(self) -> None:
        fixture = self.xberg_fixture()
        result = self.run_xberg_policy(fixture)
        self.assertEqual(result.returncode, 0, result.stderr)
        staged = self.root / "staged"
        self.assertEqual(json.loads((staged / "jchtools-xberg-release.json").read_text()), fixture["receipt"])
        self.assertFalse((staged / "ffmpeg").exists())
        installed = self.root / "installed"
        (installed / "ffmpeg").mkdir(parents=True)
        (installed / "ffmpeg" / "avcodec-63.dll").write_bytes(b"downloaded-audio")
        (installed / "config.json").write_bytes(b"user-config")
        (installed / "xberg.exe").write_bytes(b"old-engine")
        result = self.run_xberg_policy(fixture, "install")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((installed / "ffmpeg" / "avcodec-63.dll").read_bytes(), b"downloaded-audio")
        self.assertEqual((installed / "config.json").read_bytes(), b"user-config")
        self.assertEqual((installed / "xberg.exe").read_bytes(), (staged / "xberg.exe").read_bytes())

    def test_xberg_missing_common_model_identity_and_shared_conflict_fail_closed(self) -> None:
        original = self.xberg_fixture()
        for mutation in ("missing-common", "wrong-model", "shared-conflict", "audio-in-base", "audio-overwrites-base"):
            with self.subTest(mutation=mutation):
                fixture = {"receipt": json.loads(json.dumps(original["receipt"])), "root": original["root"]}
                if mutation == "missing-common":
                    fixture["receipt"]["components"][0]["members"] = [
                        m for m in fixture["receipt"]["components"][0]["members"] if m["path"] != "sherpa-onnx/LICENSE"
                    ]
                elif mutation == "wrong-model":
                    next(m for m in fixture["receipt"]["components"][0]["members"] if "/tiny/" in m["path"])[
                        "sha256"
                    ] = "f" * 64
                elif mutation == "shared-conflict":
                    next(m for m in fixture["receipt"]["components"][1]["members"] if m["path"] == "xberg.exe")[
                        "sha256"
                    ] = "f" * 64
                elif mutation == "audio-in-base":
                    fixture["receipt"]["components"][0]["members"].append(
                        self.xberg_member("ffmpeg/avcodec-63.dll", b"x")
                    )
                else:
                    fixture["receipt"]["audio_assets"]["members"].append(self.xberg_member("xberg.exe", b"x"))
                self.assertNotEqual(self.run_xberg_policy(fixture).returncode, 0)

    def test_xberg_archive_digest_mismatch_is_rejected(self) -> None:
        fixture = self.xberg_fixture()
        fixture["receipt"]["components"][0]["sha256"] = "f" * 64
        self.assertNotEqual(self.run_xberg_policy(fixture).returncode, 0)

    def test_xberg_zip_missing_unlisted_duplicate_unsafe_symlink_and_corrupt_members_rejected(self) -> None:
        fixture = self.xberg_fixture()
        archive = self.root / fixture["receipt"]["components"][0]["archive"]
        with zipfile.ZipFile(archive) as source:
            members = [(entry.filename, source.read(entry)) for entry in source.infolist()]
        for mutation in ("missing", "unlisted", "duplicate", "traversal", "ads", "symlink", "corrupt"):
            with self.subTest(mutation=mutation):
                changed = list(members)
                if mutation == "missing":
                    changed.pop()
                elif mutation == "unlisted":
                    changed.append(("xberg-cli-x86_64-pc-windows-msvc/extra.dll", b"x"))
                elif mutation == "duplicate":
                    prefix, path = changed[0][0].split("/", 1)
                    changed.append((prefix + "/" + path.upper(), changed[0][1]))
                elif mutation in ("traversal", "ads"):
                    path = "../outside" if mutation == "traversal" else "xberg.exe:stream"
                    changed.append(("xberg-cli-x86_64-pc-windows-msvc/" + path, b"x"))
                elif mutation == "symlink":
                    info = zipfile.ZipInfo(changed[0][0])
                    info.create_system = 3
                    info.external_attr = 0o120777 << 16
                    changed[0] = (info.filename, info)
                else:
                    changed[0] = (changed[0][0], b"z" * len(changed[0][1]))
                self.rewrite_xberg_zip(fixture, changed)
                self.assertNotEqual(self.run_xberg_policy(fixture).returncode, 0)

    def test_xberg_final_tree_detects_audio_leak_and_missing_base(self) -> None:
        fixture = self.xberg_fixture()
        result = self.run_xberg_policy(fixture)
        self.assertEqual(result.returncode, 0, result.stderr)
        (self.root / "staged" / "ffmpeg").mkdir()
        self.assertNotEqual(self.run_xberg_policy(fixture, "validate").returncode, 0)
        (self.root / "staged" / "ffmpeg").rmdir()
        (self.root / "staged" / "xberg.exe").unlink()
        self.assertNotEqual(self.run_xberg_policy(fixture, "validate").returncode, 0)

    def test_xberg_fixed_api_provenance_and_digest_are_required(self) -> None:
        name = "xberg-document-x86_64-pc-windows-msvc.zip"
        release = {
            "tag_name": "v-test",
            "draft": False,
            "prerelease": False,
            "html_url": "https://github.com/jchanghong023/xberg/releases/tag/v-test",
            "assets": [
                {
                    "name": name,
                    "state": "uploaded",
                    "size": 123,
                    "digest": "sha256:" + "a" * 64,
                    "browser_download_url": "https://github.com/jchanghong023/xberg/releases/download/v-test/" + name,
                }
            ],
        }
        fixture = {"receipt": release, "root": self.root}
        result = self.run_xberg_policy(fixture, "api")
        self.assertEqual(result.returncode, 0, result.stderr)
        for key, value in (("digest", None), ("browser_download_url", "https://example.invalid/base.zip"), ("size", 0)):
            changed = json.loads(json.dumps(release))
            changed["assets"][0][key] = value
            self.assertNotEqual(self.run_xberg_policy({"receipt": changed, "root": self.root}, "api").returncode, 0)

    def test_xberg_offline_cache_is_rehashed_not_trusted_by_filename(self) -> None:
        payload = b"cached-archive"
        record = self.xberg_member("unused", payload) | {
            "archive": "document.zip",
            "url": "https://example.invalid/never",
        }
        cache = self.root / f"{record['sha256']}-{record['archive']}"
        cache.write_bytes(payload)
        fixture = {"receipt": record, "root": self.root}
        result = self.run_xberg_policy(fixture, "cache")
        self.assertEqual(result.returncode, 0, result.stderr)
        cache.write_bytes(b"corrupt")
        self.assertNotEqual(self.run_xberg_policy(fixture, "cache").returncode, 0)
        cache.unlink()
        self.assertNotEqual(self.run_xberg_policy(fixture, "cache").returncode, 0)

    def test_resolved_patched_packages_use_local_path_sources(self) -> None:
        # 真实 Cargo resolver 查询：仅读取元数据，不编译、不执行任何 crate。
        sources = json.loads((ROOT / "vendor" / "SOURCES.json").read_text(encoding="utf-8"))
        patched = [entry for entry in sources["packages"] if entry["effective_source"]["type"] == "path"]
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
                "jchtools",
                "snap-ocr-core",
                "snap-ocr-worker",
                "jchocr",
                "jchtools-foundation",
                "jchtools-file-core",
                "jchtools-recursive-extract",
                "jchtools-directory-organizer",
                "jchtools-md-tools",
                "jchtools-git-tools",
                "jchtools-xberg",
                "jchtools-assets",
                "jchtools-markdown",
                "jchtools-acp-api",
                "jchtools-codeagent",
                "jchtools-ui",
                "jchtools-gui",
            },
        )
        workspace_members = set(metadata["workspace_members"])
        resolved_local_identities = {
            (package["name"], package["version"])
            for package in packages.values()
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
                package for package in packages.values() if package["name"] == name and package["version"] == version
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
