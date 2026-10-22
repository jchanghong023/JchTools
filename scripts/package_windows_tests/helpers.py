"""打包验收测试共享驱动与常量（自 scripts/test_package_windows.py 原样搬移）."""

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
from scripts.test_gate import ps51_module_path
# 与原 scripts/test_package_windows.py 的 parent.parent 等价（本文件位于 scripts/package_windows_tests/）。
ROOT = Path(__file__).resolve().parents[2]


SCRIPT = ROOT / "scripts" / "package-windows.ps1"


# 只加载实际部署入口中的函数定义；不运行构建/下载/安装，不复制选择算法。
FUNCTION_LOADER = r"""
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.Encoding]::UTF8
. (Join-Path (Split-Path -Parent $env:R24_SCRIPT) 'windows-paths.ps1')
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
. (Join-Path (Split-Path -Parent $env:R24_SCRIPT) 'windows-paths.ps1')
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
    if ($Arguments[0] -ceq 'test' -and $Arguments -contains '--ignored' -and
        $Arguments -contains 'jchtools-recursive-extract' -and $Arguments -contains '--lib' -and
        $env:JCHTOOLS_TEST_7ZIP -ceq $archiveEngine) {
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
