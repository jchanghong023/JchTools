#requires -Version 5.1
<#
.SYNOPSIS
  JchTools 单命令验收入口：把 AGENTS.md 3.2 完成条件矩阵串成一个退出码。

.DESCRIPTION
  默认执行（无需参数）：static_check.py（含测试基线与界面规则检查）、cargo test、
  Snap OCR core/worker 的 all-targets 测试与 clippy、构建输出 binding loop 警告扫描。可选阶段：
    -WithEngine   追加真实引擎用例（JCHTOOLS_TEST_7ZIP 或 resources\7zip\7z.exe，
                  缺失时直接失败并提示先运行 fetch-7zip.ps1）
    -WithGuiSmoke 追加 OS 级 UIA 冒烟（默认 S1-S4/S15/S16/S18；资产就绪时自动扩展：
                  S6-S9 需含 xberg.exe 的有效目录，S5/S10-S14/S17 需 JCHTOOLS_TEST_XBERG_DIR
                  有效引擎（S5/S14 另需媒体样本），缺资产如实记 NOT RUN，不伪报已覆盖。
                  需要 -GuiData 指向 make_tmp.py testdata 生成的数据集与 cargo build 产物
                  （尊重 CARGO_TARGET_DIR，未设置时为 target\debug）；需要 pip install pywinauto）
    -WithPackage  追加发布打包自检（package-windows.ps1，需要引擎与 MSVC 工具链）
    -WithMarkdownAcceptance 追加转 Markdown 验收承接（scripts/markdown_acceptance.py，
                  F26 / ALL2MARKDOWN 附录 A；退出码 0=PASS、2=存在条目 NOT RUN（缺资产/
                  被测物，如实呈现）、其他=失败。-MarkdownArgs 透传驱动器参数）
  未执行的阶段在汇总里显式打印 NOT RUN；不得把 NOT RUN 报告成通过。
  所有日志写在 .tmp\acceptance\ 下，任何已执行阶段失败即以非零码终止。

.EXAMPLE
  powershell -NoProfile -File .\scripts\acceptance.ps1 -WithEngine
#>
[CmdletBinding()]
param(
    [switch]$WithEngine,
    [switch]$WithGuiSmoke,
    [switch]$WithPackage,
    [string]$GuiData,
    [switch]$WithMarkdownAcceptance,
    [string[]]$MarkdownArgs = @()
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') {throw 'Windows 专用验收脚本；本项目按合同 P-07 仅支持 Windows 平台。'}
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Set-Location -LiteralPath $root
$script:LogDir = Join-Path $root '.tmp\acceptance'
New-Item -ItemType Directory -Path $script:LogDir -Force | Out-Null
$script:Results = [System.Collections.Generic.List[string]]::new()
# python/cargo 输出为 UTF-8；不设置的话 PS 5.1 会按控制台代码页误解码成乱码。
try {[Console]::OutputEncoding = [Text.Encoding]::UTF8} catch {}

function Resolve-Python {
    foreach ($name in @('python','python3')) {
        $found = Get-Command $name -ErrorAction SilentlyContinue
        if ($found) {return $found.Source}
    }
    throw '未找到 python；static_check.py 需要 Python 3.11+（tomllib）。'
}
function Test-AbsoluteWindowsPath {
    param([string]$Path)
    return ($Path -match '^[A-Za-z]:[\\/]') -or $Path.StartsWith('\\',[StringComparison]::Ordinal)
}

function Resolve-PhysicalPath {
    param([string]$Path)
    if (-not ('JchAcceptancePath' -as [type])) {
        $typeDefinition = @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

public static class JchAcceptancePath
{
    [DllImport("kernel32.dll", EntryPoint = "CreateFileW", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFile(
        string fileName, uint desiredAccess, uint shareMode, IntPtr securityAttributes,
        uint creationDisposition, uint flags, IntPtr templateFile);

    [DllImport("kernel32.dll", EntryPoint = "GetFinalPathNameByHandleW", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern uint GetFinalPathNameByHandle(SafeFileHandle file, StringBuilder path, uint pathLength, uint flags);

    public static string TryResolveExistingPath(string path)
    {
        using (SafeFileHandle handle = CreateFile(path, 0, 7, IntPtr.Zero, 3, 0x02000000, IntPtr.Zero))
        {
            if (handle.IsInvalid)
            {
                int error = Marshal.GetLastWin32Error();
                if (error == 2 || error == 3) return null;
                throw new Win32Exception(error);
            }

            uint required = GetFinalPathNameByHandle(handle, null, 0, 0);
            if (required == 0) throw new Win32Exception(Marshal.GetLastWin32Error());
            StringBuilder result = new StringBuilder((int)required + 1);
            uint written = GetFinalPathNameByHandle(handle, result, (uint)result.Capacity, 0);
            if (written == 0 || written >= result.Capacity) throw new Win32Exception(Marshal.GetLastWin32Error());
            string finalPath = result.ToString();
            if (finalPath.StartsWith(@"\\?\UNC\", StringComparison.OrdinalIgnoreCase))
                return @"\\" + finalPath.Substring(8);
            if (finalPath.StartsWith(@"\\?\", StringComparison.OrdinalIgnoreCase))
                return finalPath.Substring(4);
            return finalPath;
        }
    }
}
'@
        Add-Type -TypeDefinition $typeDefinition | Out-Null
    }

    $fullPath = [IO.Path]::GetFullPath($Path)
    $rootPath = [IO.Path]::GetPathRoot($fullPath)
    $existingPath = $fullPath.TrimEnd([IO.Path]::DirectorySeparatorChar,[IO.Path]::AltDirectorySeparatorChar)
    if ($existingPath.Length -lt $rootPath.Length) {$existingPath = $rootPath}
    $remaining = [Collections.Generic.List[string]]::new()
    $resolvedPath = [JchAcceptancePath]::TryResolveExistingPath($existingPath)
    while ($null -eq $resolvedPath) {
        $parent = [IO.Directory]::GetParent($existingPath)
        $segment = [IO.Path]::GetFileName($existingPath)
        if (($null -eq $parent) -or (-not $segment)) {throw "无法解析隔离目录路径：$Path"}
        $remaining.Insert(0,$segment)
        $existingPath = $parent.FullName
        $resolvedPath = [JchAcceptancePath]::TryResolveExistingPath($existingPath)
    }
    foreach ($segment in $remaining) {$resolvedPath = Join-Path $resolvedPath $segment}
    return [IO.Path]::GetFullPath($resolvedPath)
}

function Assert-MarkdownIsolationRoot {
    param([string]$Name,[string]$Path,[string]$TempRoot,[string]$TempPrefix)
    if (-not (Test-AbsoluteWindowsPath $Path)) {
        throw "$Name 必须是仓库 .tmp 下的绝对隔离路径，拒绝使用：$Path"
    }
    $resolvedPath = Resolve-PhysicalPath $Path
    if (-not ($resolvedPath.Equals($TempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        $resolvedPath.StartsWith($TempPrefix, [StringComparison]::OrdinalIgnoreCase))) {
        throw "$Name 必须位于仓库 .tmp 下，拒绝创建或写入：$Path"
    }
    return $resolvedPath
}


function Invoke-Logged {
    # 以 PS 5.1 兼容的方式运行原生命令：临时放宽 EAP 避免 stderr（cargo 进度）被当成
    # 错误终止，凭退出码判定成败，完整输出落 .tmp\acceptance\<步骤>.log。
    param([string]$Name,[string]$File,[string[]]$Arguments = @(),[hashtable]$Environment)
    $log = Join-Path $script:LogDir (($Name -replace '[^\w.-]','_') + '.log')
    Write-Host "==> $Name"
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $saved = @{}
    # Python 3.13 默认输出 UTF-8；显式固定子进程编码，避免 Windows PowerShell
    # 按系统代码页解码中文日志后再以乱码写入验收记录。
    foreach ($key in @('PYTHONIOENCODING','PYTHONUTF8')) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key)
        $encoding = if ($key -eq 'PYTHONIOENCODING') {'utf-8'} else {'1'}
        Set-Item -Path ("Env:" + $key) -Value $encoding
    }
    if ($Environment) {foreach ($key in $Environment.Keys) {
        $saved[$key] = [Environment]::GetEnvironmentVariable($key)
        Set-Item -Path ("Env:" + $key) -Value $Environment[$key]
    }}
    try {$output = & $File @Arguments 2>&1}
    finally {
        foreach ($key in $saved.Keys) {
            if ($null -eq $saved[$key]) {Remove-Item ("Env:" + $key) -ErrorAction SilentlyContinue}
            else {Set-Item -Path ("Env:" + $key) -Value $saved[$key]}
        }
        $ErrorActionPreference = $previous
    }
    $code = $LASTEXITCODE
    $output | ForEach-Object {$_.ToString()} | Set-Content -LiteralPath $log -Encoding UTF8
    if ($code -ne 0) {
        $output | Select-Object -Last 12 | ForEach-Object {$_.ToString()} | Write-Host
        throw "$Name 失败（退出码 $code），完整日志：$log"
    }
    $script:Results.Add("PASS  $Name")
    return $log
}

$python = Resolve-Python
# UT/集成测试自己布置隔离环境。验收提供的真实资产和状态目录只用于
# 后面的桌面驱动，不能覆盖各测试的 tempfile 配置。
$testEnvironment = @{}
foreach ($key in @('JCHTOOLS_TEST_STATE_DIR','JCHTOOLS_TEST_ASSET_ROOT',
    'JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT','JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT',
    'JCHTOOLS_TEST_BROKER_EXE')) {
    $testEnvironment[$key] = $null
}

# 1) 静态检查：结构/配置/回调/SQL/测试基线/界面规则/产品名。
$null = Invoke-Logged -Name 'static-check' -File $python -Arguments @('scripts/static_check.py')
$null = Invoke-Logged -Name 'ocr-asset-manifest' -File $python `
    -Arguments @('tests/ocr_fixtures/check_asset_manifest.py')

# 2) 全量测试（默认特性，含 GUI 与属性测试）。
$testLog = Invoke-Logged -Name 'cargo-test' -File 'cargo' -Arguments @('test','--all-targets','--features','test-hooks') -Environment $testEnvironment

# 3) binding loop 警告扫描：AGENTS.md 第 4 节禁止布局绑定环。
#    注意增量构建可能不再重放旧警告；全量告警以干净构建（CI / package 步骤）为准。
$hits = @(Select-String -LiteralPath $testLog -Pattern 'binding loop' -SimpleMatch)
if ($hits.Count -gt 0) {
    $hits | ForEach-Object {"$($_.LineNumber): $($_.Line)"} | Write-Host
    throw "构建输出出现 binding loop 警告（AGENTS.md 第 4 节禁止）；详见 $testLog"
}
$script:Results.Add('PASS  binding-loop-scan')

# 截图 OCR 是独立 workspace 成员；根 cargo test/clippy 不覆盖它们。对当前 HEAD
# 的两个包同时运行测试和 lint，防止本地验收只依赖远程 CI 的旧结果。
foreach ($component in @('snap-ocr-core','snap-ocr-worker')) {
    $manifest = Join-Path $root "optional/$component/Cargo.toml"
    $null = Invoke-Logged -Name "$component-tests" -File 'cargo' `
        -Arguments @('test','--manifest-path',$manifest,'--all-targets','--features','test-hooks') -Environment $testEnvironment
    $null = Invoke-Logged -Name "$component-clippy" -File 'cargo' `
        -Arguments @('clippy','--manifest-path',$manifest,'--all-targets','--features','test-hooks','--','-D','warnings')
}
if ($env:JCHTOOLS_SNAP_OCR_ASSET_ROOT) {
    $ocrManifest = Get-Content -LiteralPath 'resources\snap-ocr-assets.json' -Raw -Encoding UTF8 | ConvertFrom-Json
    $workerVersion = Split-Path -Leaf (Split-Path -Parent $ocrManifest.workers[0].install_path)
    $null = Invoke-Logged -Name 'snap-ocr-worker-root' -File $python `
        -Arguments @('tests/ocr_fixtures/check_worker_root.py','--asset-root',$env:JCHTOOLS_SNAP_OCR_ASSET_ROOT,'--worker-version',$workerVersion)
} else {$script:Results.Add('NOT RUN  snap-ocr-worker-root（设置 JCHTOOLS_SNAP_OCR_ASSET_ROOT 为完整的已校验资产缓存）')}

# 4) 真实引擎用例（可选）。
if ($WithEngine) {
    $engine = $env:JCHTOOLS_TEST_7ZIP
    if (-not $engine) {
        $candidate = Join-Path $root 'resources\7zip\7z.exe'
        if (Test-Path -LiteralPath $candidate) {$engine = $candidate}
    }
    if (-not $engine) {throw '-WithEngine 需要真实引擎：设置 JCHTOOLS_TEST_7ZIP，或先运行 scripts/fetch-7zip.ps1'}
    $null = Invoke-Logged -Name 'engine-tests' -File 'cargo' `
        -Arguments @('test','--features','test-hooks','--test','archive','--','--ignored','--test-threads=1') `
        -Environment @{JCHTOOLS_TEST_7ZIP = $engine}
} else {$script:Results.Add('NOT RUN  engine-tests（加 -WithEngine）')}

# 5) OS 级 GUI 冒烟（可选）：默认 S1-S4/S15/S16/S18，资产就绪时自动扩展（S11-01）。
if ($WithGuiSmoke) {
    if (-not $GuiData) {throw '-WithGuiSmoke 需要 -GuiData 指向 make_tmp.py testdata 生成的数据集目录'}
    if (-not (Test-Path -LiteralPath $GuiData)) {throw "数据集目录不存在：$GuiData"}
    # 与 package-windows.ps1 同口径尊重 CARGO_TARGET_DIR：未设置时回落默认 target 目录。
    # 否则自定义 target 目录的机器上 cargo build 产物永远不在硬编码路径，冒烟误报失败。
    if ($env:CARGO_TARGET_DIR) {
        $targetDir = $env:CARGO_TARGET_DIR
    } else {
        $targetDir = Join-Path $root 'target'
    }
    $exe = Join-Path $targetDir 'debug\JchTools.exe'
    # 无条件重建：target 下残留旧 exe 时跳过构建会让冒烟作用于陈旧二进制，
    # 验证结论与当前源码脱节（AGENTS.md 3.2 对抗自证偏差）。
    # 保留普通生产构建维度；真实桌面测试启用显式隔离，不能启动用户常驻后台，
    # 否则后面的隔离 Markdown broker 会按 XB-14 拒绝第二个引擎。
    $null = Invoke-Logged -Name 'gui-production-build' -File 'cargo' -Arguments @('build')
    $null = Invoke-Logged -Name 'gui-build' -File 'cargo' -Arguments @('build','--features','test-hooks')
    $guiState = Join-Path $script:LogDir 'gui-state'
    $guiEnvironment = @{
        JCHTOOLS_TEST_STATE_DIR = $guiState
        JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT = Join-Path $script:LogDir 'gui-snap-assets'
        JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT = Join-Path $script:LogDir 'gui-markdown-assets'
    }
    # S11-01 阶段自动扩展：默认序列保持 S1-S4/S15/S16/S18（无资产环境行为不变）；
    # 本分支已具备 test-hooks EXE（上方 gui-build）与隔离状态目录/资产根，资产
    # 就绪时按前置逐组追加并向 gui_smoke 传 --stages；任一前置缺失只记 NOT RUN
    # 说明（不 fail），不得把缺资产伪报为已覆盖。
    $smokeStages = @('S1','S2','S3','S4','S15','S16','S18')
    # 前置（设置页组 S6-S9）：S6/S7 需要包含 xberg.exe 的有效目录（优先
    # JCHTOOLS_SMOKE_XBERG_DIR，回落 JCHTOOLS_TEST_XBERG_DIR）；S8/S9 本身无需
    # 资产，但随设置页组整组扩展，保证无资产环境的默认序列不变。
    $smokeXberg = $env:JCHTOOLS_SMOKE_XBERG_DIR
    if (-not $smokeXberg) {$smokeXberg = $env:JCHTOOLS_TEST_XBERG_DIR}
    if ($smokeXberg -and (Test-Path -LiteralPath (Join-Path $smokeXberg 'xberg.exe') -PathType Leaf)) {
        $smokeStages += @('S6','S7','S8','S9')
        $guiEnvironment['JCHTOOLS_SMOKE_XBERG_DIR'] = [IO.Path]::GetFullPath($smokeXberg)
    } else {
        $script:Results.Add('NOT RUN  gui-smoke S6-S9 设置页阶段（缺包含 xberg.exe 的有效目录：JCHTOOLS_SMOKE_XBERG_DIR / JCHTOOLS_TEST_XBERG_DIR；S8/S9 随设置页组整组扩展）')
    }
    # 前置（转换组 S5/S10-S14，S17 经下方先行调用覆盖）：需 JCHTOOLS_TEST_XBERG_DIR
    # 指向有效引擎；隔离状态先经 --seed-state 播种，再以 S17 先行真实初始化组件
    # （gui_smoke 的固定阶段顺序把 S17 排在 S5 之后，就绪必须在主序列之前建立，
    # 故拆成先行调用）；S5/S14 另需媒体样本（优先 JCHTOOLS_S5_MEDIA，回落公开
    # 合成夹具，与 markdown_acceptance 的缺省同源）。
    $convXberg = $env:JCHTOOLS_TEST_XBERG_DIR
    $mediaSample = $env:JCHTOOLS_S5_MEDIA
    if (-not $mediaSample) {
        $mediaCandidate = Join-Path $root 'tests\markdown_fixtures\video-to-notes-intro-zh.mp4'
        if (Test-Path -LiteralPath $mediaCandidate -PathType Leaf) {$mediaSample = $mediaCandidate}
    }
    if ($convXberg -and (Test-Path -LiteralPath (Join-Path $convXberg 'xberg.exe') -PathType Leaf)) {
        $convXberg = [IO.Path]::GetFullPath($convXberg)
        $null = Invoke-Logged -Name 'gui-smoke-seed-state' -File $python `
            -Arguments @('scripts/markdown_acceptance.py','--seed-state',$guiState,'--seed-xberg',$convXberg)
        $null = Invoke-Logged -Name 'gui-smoke-convert-init' -File $python `
            -Arguments @('scripts/gui_smoke.py','--exe',$exe,'--data',$GuiData,'--stages','S17') -Environment $guiEnvironment
        $smokeStages += @('S10','S11','S12','S13')
        if ($mediaSample) {
            $smokeStages += @('S5','S14')
            $guiEnvironment['JCHTOOLS_S5_MEDIA'] = [IO.Path]::GetFullPath($mediaSample)
        } else {
            $script:Results.Add('NOT RUN  gui-smoke S5/S14 运行态链路（缺媒体样本：JCHTOOLS_S5_MEDIA 或 tests/markdown_fixtures/video-to-notes-intro-zh.mp4）')
        }
    } else {
        $script:Results.Add('NOT RUN  gui-smoke S5/S10-S14/S17 转换阶段（缺有效 Xberg 引擎目录：JCHTOOLS_TEST_XBERG_DIR）')
        if (-not $mediaSample) {
            $script:Results.Add('NOT RUN  gui-smoke S5/S14 运行态链路（缺媒体样本：JCHTOOLS_S5_MEDIA 或 tests/markdown_fixtures/video-to-notes-intro-zh.mp4）')
        }
    }
    $null = Invoke-Logged -Name 'gui-smoke' -File $python `
        -Arguments @('scripts/gui_smoke.py','--exe',$exe,'--data',$GuiData,'--stages',($smokeStages -join ',')) -Environment $guiEnvironment
} else {$script:Results.Add('NOT RUN  gui-smoke（默认 S1-S4/S15/S16/S18，资产就绪自动扩展；加 -WithGuiSmoke -GuiData <目录>）')}

# 5.5) 旧 markdown-media-worker 已退役（XB-12：媒体转录迁移到 Xberg 推理组件）；
#      其验收阶段随之移除，截图 OCR 组件测试见下方 snap-ocr 阶段。

# 6) 发布打包自检（可选）。
if ($WithPackage) {
    $null = Invoke-Logged -Name 'package' -File 'powershell' `
        -Arguments @('-NoProfile','-File',(Join-Path $PSScriptRoot 'package-windows.ps1'))
} else {$script:Results.Add('NOT RUN  package（加 -WithPackage）')}

# 7) 转 Markdown 验收承接（可选；F26 / ALL2MARKDOWN 附录 A）。
if ($WithMarkdownAcceptance) {
    $mdAssetRoot = $env:JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT
    if (-not $mdAssetRoot) {$mdAssetRoot = Join-Path $script:LogDir 'markdown-assets'}
    $mdStateRoot = $env:JCHTOOLS_TEST_STATE_DIR
    if (-not $mdStateRoot) {$mdStateRoot = Join-Path $script:LogDir 'markdown-state'}

    # 在构建或创建隔离目录前校验绝对路径及实际 reparse 目标，避免 .tmp 外写入。
    $physicalRoot = Resolve-PhysicalPath $root
    $tmpRoot = [IO.Path]::GetFullPath((Join-Path $physicalRoot '.tmp'))
    $tmpPrefix = $tmpRoot.TrimEnd([IO.Path]::DirectorySeparatorChar,[IO.Path]::AltDirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    $mdAssetRoot = Assert-MarkdownIsolationRoot -Name 'JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT' `
        -Path $mdAssetRoot -TempRoot $tmpRoot -TempPrefix $tmpPrefix
    $mdStateRoot = Assert-MarkdownIsolationRoot -Name 'JCHTOOLS_TEST_STATE_DIR' `
        -Path $mdStateRoot -TempRoot $tmpRoot -TempPrefix $tmpPrefix

    # Markdown 驱动需要 test-hooks：它只在开发验收 EXE 中启用隔离资产根，
    # 发布构建与生产运行不启用该 feature。GUI 冒烟使用的生产维度构建已在上方
    # 完成；这里单独记录一次开发验收构建，避免测试环境变量被生产 EXE 忽略。
    $null = Invoke-Logged -Name 'markdown-gui-build' -File 'cargo' `
        -Arguments @('build','--features','test-hooks')
    # 转换验收只初始化文档组件；截图资产使用独立空目录，防止配置保存时
    # 唤起生产目录里的旧 worker 并占用用户会话唯一引擎。
    $mdSnapAssetRoot = Join-Path $script:LogDir 'markdown-snap-assets'
    $mdExe = $env:JCHTOOLS_TEST_GUI_EXE
    if (-not $mdExe) {
        $mdTargetDir = if ($env:CARGO_TARGET_DIR) {[IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)} else {Join-Path $root 'target'}
        $mdExe = Join-Path $mdTargetDir 'debug\JchTools.exe'
    }
    New-Item -ItemType Directory -Path $mdAssetRoot -Force | Out-Null
    New-Item -ItemType Directory -Path $mdStateRoot -Force | Out-Null
    $testXberg = $env:JCHTOOLS_TEST_XBERG_DIR
    if ($testXberg) {
        $testXberg = [IO.Path]::GetFullPath($testXberg)
        if (-not (Test-Path -LiteralPath (Join-Path $testXberg 'xberg.exe') -PathType Leaf)) {
            throw "JCHTOOLS_TEST_XBERG_DIR 缺少 xberg.exe：$testXberg"
        }
        # 驱动与 GUI 必须读取同一份隔离 SQLite；清除旧 downloaded 指针，避免
        # 测试引擎通过固定环境变量报 ready、GUI 却读取另一条路径。
        $null = Invoke-Logged -Name 'markdown-state-seed' -File $python `
            -Arguments @('scripts/markdown_acceptance.py','--seed-state',$mdStateRoot,'--seed-xberg',$testXberg)
    }
    $mdArgs = @('scripts/markdown_acceptance.py') + $MarkdownArgs
    Write-Host '==> markdown-acceptance'
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $savedMarkdownEnv = @{}
    foreach ($key in @('JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT','JCHTOOLS_TEST_ASSET_ROOT','JCHTOOLS_TEST_STATE_DIR','JCHTOOLS_TEST_GUI_EXE','JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT')) {
        $savedMarkdownEnv[$key] = [Environment]::GetEnvironmentVariable($key)
    }
    try {
        Set-Item -Path Env:JCHTOOLS_MARKDOWN_TEST_ASSET_ROOT -Value $mdAssetRoot
        Set-Item -Path Env:JCHTOOLS_TEST_ASSET_ROOT -Value $mdAssetRoot
        Set-Item -Path Env:JCHTOOLS_TEST_STATE_DIR -Value $mdStateRoot
        Set-Item -Path Env:JCHTOOLS_SNAP_OCR_TEST_ASSET_ROOT -Value $mdSnapAssetRoot
        Set-Item -Path Env:JCHTOOLS_TEST_GUI_EXE -Value $mdExe
        if ($testXberg) {
            $null = Invoke-Logged -Name 'markdown-gui-initialize' -File $python `
                -Arguments @('scripts/gui_smoke.py','--exe',$mdExe,'--data',$mdAssetRoot,'--stages','S17')
        }
        $mdOutput = & $python @mdArgs 2>&1
    } finally {
        foreach ($key in $savedMarkdownEnv.Keys) {
            if ($null -eq $savedMarkdownEnv[$key]) {Remove-Item ("Env:" + $key) -ErrorAction SilentlyContinue}
            else {Set-Item -Path ("Env:" + $key) -Value $savedMarkdownEnv[$key]}
        }
        $ErrorActionPreference = $previous
    }
    $mdCode = $LASTEXITCODE
    $mdLog = Join-Path $script:LogDir 'markdown-acceptance.log'
    $mdOutput | ForEach-Object {$_.ToString()} | Set-Content -LiteralPath $mdLog -Encoding UTF8
    if ($mdCode -eq 0) {
        # 退出码 0 也可能只是 --list / --seed-state 等非验收操作；必须看到真实条目结果行才可报通过。
        $itemResults = @($mdOutput | Where-Object {
            $_.ToString() -match '^\s*(?:PASS|FAIL|NOT RUN)\s+[A-Z]\d+\b'
        })
        if ($itemResults.Count -eq 0) {
            $script:Results.Add('NOT RUN  markdown-acceptance（未执行验收条目；详见 markdown-acceptance.log）')
        } else {
            # 驱动器可能只执行部分条目；保留逐项状态，不把跳过当成全覆盖通过。
            $notRun = @($itemResults | Where-Object { $_.ToString() -match '^\s*NOT RUN\s+[A-Z]\d+\b' })
            if ($notRun.Count -gt 0) {
                $script:Results.Add('PARTIAL  markdown-acceptance（已执行项通过，仍有 NOT RUN；详见 markdown-acceptance.log）')
            } else {
                $script:Results.Add('PASS  markdown-acceptance')
            }
        }
    } elseif ($mdCode -eq 2) {
        # 2 = 存在条目 NOT RUN（缺真实资产/被测物）：如实呈现，不当作通过，也不阻塞其余阶段。
        $script:Results.Add('NOT RUN  markdown-acceptance（存在未执行条目；缺资产或 --only 子集不能冒充完整验收；详见 markdown-acceptance.log）')
    } else {
        $mdOutput | Select-Object -Last 12 | ForEach-Object {$_.ToString()} | Write-Host
        throw "markdown-acceptance 失败（退出码 $mdCode），完整日志：$mdLog"
    }
} else {$script:Results.Add('NOT RUN  markdown-acceptance（加 -WithMarkdownAcceptance [-MarkdownArgs <透传参数>]）')}

Write-Host ''
Write-Host '==== 验收汇总 ===='
$script:Results | ForEach-Object {Write-Host $_}
