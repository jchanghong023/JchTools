#requires -Version 5.1
[CmdletBinding()]
param(
    [switch]$SkipTests,
    [switch]$Offline,
    [string]$DestinationRoot,
    [string]$GateTimingFile
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. ([IO.Path]::Combine($PSScriptRoot, 'windows-paths.ps1'))
$script:packageTiming = $null
$script:packageTotalWatch = [Diagnostics.Stopwatch]::StartNew()
trap {
    $packageFailure = $_
    if ($script:packageTiming) {
        try {Complete-PackageTiming -Failure $packageFailure.Exception.Message}
        catch {Write-Warning "Unable to finalize package timing: $($_.Exception.Message)"}
    }
    throw $packageFailure
}
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Set-Location -LiteralPath $root
if ($DestinationRoot) {
    if (-not [IO.Path]::IsPathRooted($DestinationRoot)) {$DestinationRoot = Join-Path $root $DestinationRoot}
    $DestinationRoot = [IO.Path]::GetFullPath($DestinationRoot)
} else {
    $DestinationRoot = Join-Path $root 'dist'
}
function Initialize-PackageTiming([string]$RepositoryRoot) {
    $workspace = Join-Path $RepositoryRoot ('.tmp\package-windows-' + [guid]::NewGuid().ToString('N'))
    & python (Join-Path $RepositoryRoot 'scripts\make_tmp.py') workspace --destination $workspace
    if ($LASTEXITCODE -ne 0) {throw "Unable to create package timing workspace: $LASTEXITCODE"}
    $steps = @('setup','assets','font-and-engine-validation','lockfile','tests','worker-build','manifest','main-build','staging','xberg-base','licenses','package-validation','installer','zip')
    $script:packageTiming = @{
        path = (Join-Path $workspace 'timings.jsonl')
        writer = $null
        active = $null
        watch = $null
        records = @($steps | ForEach-Object {
            [pscustomobject]@{step=$_;status='NOT_RUN';elapsed_seconds=0.0;started_utc=$null;finished_utc=$null;detail=$null}
        })
        total = [pscustomobject]@{step='total';status='RUNNING';elapsed_seconds=0.0;detail=$null}
    }
    # Keep one append-only handle: Windows readers/scanners cannot race file replacement.
    $stream = [IO.File]::Open($script:packageTiming.path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try {$script:packageTiming.writer = New-Object IO.StreamWriter($stream, (New-Object Text.UTF8Encoding($false)))}
    catch {$stream.Dispose(); throw}
    $script:packageTiming.writer.AutoFlush = $true
    Write-Host "Package wall-clock timing record: $($script:packageTiming.path)"
    Start-PackageStep 'setup'
    # Setup includes timing workspace creation and all preceding script initialization.
    $script:packageTiming.watch = $script:packageTotalWatch
    $script:packageTiming.active.started_utc = [datetime]::UtcNow.AddSeconds(-$script:packageTotalWatch.Elapsed.TotalSeconds).ToString('o')
    Save-PackageTiming
}
function Save-PackageTiming {
    $script:packageTiming.total.elapsed_seconds = $script:packageTotalWatch.Elapsed.TotalSeconds
    $record = @{
        clock = 'wall-clock; includes Cargo command overhead, not compile-only budget accounting'
        steps = $script:packageTiming.records
        total = $script:packageTiming.total
    }
    # Each flushed line is a complete snapshot; earlier lines survive any later write failure.
    $script:packageTiming.writer.WriteLine(($record | ConvertTo-Json -Depth 6 -Compress))
}
function Stop-PackageStep([string]$Status = 'PASS', [string]$Detail) {
    if ($script:packageTiming.active) {
        $entry = $script:packageTiming.active
        $entry.elapsed_seconds = $script:packageTiming.watch.Elapsed.TotalSeconds
        $entry.finished_utc = [datetime]::UtcNow.ToString('o')
        $entry.status = $Status
        $entry.detail = $Detail
        Write-Host ("Package step {0}: {1} ({2:F3}s)" -f $entry.step, $Status, $entry.elapsed_seconds)
        $script:packageTiming.active = $null
        $script:packageTiming.watch = $null
    }
    Save-PackageTiming
}
function Start-PackageStep([string]$Name) {
    Stop-PackageStep
    $entries = @($script:packageTiming.records | Where-Object {$_.step -ceq $Name})
    if ($entries.Count -ne 1 -or $entries[0].status -cne 'NOT_RUN') {throw "Invalid package step transition: $Name"}
    $script:packageTiming.active = $entries[0]
    $script:packageTiming.active.status = 'RUNNING'
    $script:packageTiming.active.started_utc = [datetime]::UtcNow.ToString('o')
    $script:packageTiming.watch = [Diagnostics.Stopwatch]::StartNew()
    Save-PackageTiming
}
function Complete-PackageTiming([string]$Failure) {
    try {
        if ($Failure) {
            Stop-PackageStep -Status 'FAIL' -Detail $Failure
            $script:packageTiming.total.status = 'FAIL'
            $script:packageTiming.total.detail = $Failure
        } else {
            Stop-PackageStep
            $script:packageTiming.total.status = 'PASS'
        }
        Save-PackageTiming
        Write-Host ("Package total: {0} ({1:F3}s); record: {2}" -f $script:packageTiming.total.status, $script:packageTiming.total.elapsed_seconds, $script:packageTiming.path)
    } finally {
        if ($script:packageTiming.writer) {$script:packageTiming.writer.Dispose()}
    }
}
function Invoke-InstallerCompiler(
    [string]$CompilerPath,
    [string]$SourceDirectory,
    [string]$OutputDirectory,
    [string]$Version,
    [string]$RepositoryRoot,
    [string]$InstallerScript = (Join-Path $RepositoryRoot 'installer\JchTools.iss')
) {
    if (-not (Test-Path -LiteralPath $CompilerPath -PathType Leaf)) {throw "安装包编译器不存在：$CompilerPath"}
    if (-not (Test-Path -LiteralPath $InstallerScript -PathType Leaf)) {throw "安装脚本不存在：$InstallerScript"}
    if (-not (Test-Path -LiteralPath $SourceDirectory -PathType Container)) {throw "已校验的安装源目录不存在：$SourceDirectory"}
    if (-not (Test-Path -LiteralPath $OutputDirectory -PathType Container)) {throw "安装包输出目录不存在：$OutputDirectory"}
    if (-not (Test-Path -LiteralPath $RepositoryRoot -PathType Container)) {throw "仓库目录不存在：$RepositoryRoot"}
    $source = [IO.Path]::GetFullPath($SourceDirectory)
    $temporaryRoot = Join-Path ([IO.Path]::GetFullPath($RepositoryRoot)) '.tmp'
    New-Item -ItemType Directory -Path $temporaryRoot -Force | Out-Null
    # Inno 6 不接受 \\?\ 前缀；短 junction 只缩短编译器输入路径，不复制或改写已校验资产。
    $alias = Join-Path $temporaryRoot ('i-' + [guid]::NewGuid().ToString('N').Substring(0, 12))
    $junction = $null
    try {
        # 不用 -Force：绝不接管已有目录或其他调用创建的链接。
        $junction = New-Item -ItemType Junction -Path $alias -Value $source -ErrorAction Stop
        & $CompilerPath "/DSourceDir=$alias" "/DOutputDir=$OutputDirectory" "/DVersion=$Version" $InstallerScript
        if ($LASTEXITCODE -ne 0) { throw "Inno Setup compiler failed: $LASTEXITCODE" }
    } finally {
        # 仅删除本次拥有的目录链接；非递归 Delete 绝不进入源树。
        if ($null -ne $junction) {[IO.Directory]::Delete($alias, $false)}
    }
}
function Assert-PackageAudioExclusion([string]$Directory) {
    $nativeDirectory = ConvertTo-WindowsFileSystemPath $Directory
    # FullName 与枚举根必须使用同一种 native 表示，避免把 extended 前缀计入相对路径。
    $nativeRoot = (Get-Item -LiteralPath $nativeDirectory -Force -ErrorAction Stop).FullName.TrimEnd('\','/')
    foreach ($item in (Get-ChildItem -LiteralPath $nativeRoot -Recurse -Force)) {
        $relative = $item.FullName.Substring($nativeRoot.Length + 1).Replace('\','/')
        if ($relative -match '(?i)(^|/)(ffmpeg|sense_voice_[^/]+|vad)(/|$)' -or
            $item.Name -match '(?i)^(avcodec|avformat|avutil|swresample|swscale|avfilter|avdevice)(-[0-9]+)?\.dll$' -or
            $item.Name -match '(?i)^((silero[^/]*|sensevoice[^/]*|whisper[^/]*)\.(onnx|bin|gguf)|model\.int8\.onnx|tokens\.txt|silence-1s\.wav)$') {
            throw "默认安装包/ZIP 不得携带音频专属资产：$relative"
        }
    }
}
function New-PackageZip([string]$Directory, [string]$Destination) {
    $nativeDirectory = ConvertTo-WindowsFileSystemPath $Directory
    $nativeRoot = (Get-Item -LiteralPath $nativeDirectory -Force -ErrorAction Stop).FullName.TrimEnd('\','/')
    $nativeDestination = ConvertTo-WindowsFileSystemPath $Destination
    if (Test-Path -LiteralPath $nativeDestination) {throw "ZIP destination already exists: $Destination"}
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    # ZIP DOS 纪元从 1980-01-01 开始；仅规范化受控暂存树里的副本，
    # 不修改 cargo registry、下载缓存或原始 in-place 运行目录。
    $zipEpoch = [datetime]::new(1980, 1, 1, 0, 0, 0, [System.DateTimeKind]::Utc)
    foreach ($item in (Get-ChildItem -LiteralPath $nativeRoot -Recurse -Force)) {
        if ($item.LastWriteTimeUtc -lt $zipEpoch) {$item.LastWriteTime = Get-Date}
    }
    # .NET 直接读取 native 路径；includeBaseDirectory 保留便携包的源根目录，
    # ZIP 内部条目由相对路径生成，不会携带 extended-length 文件系统前缀。
    [IO.Compression.ZipFile]::CreateFromDirectory($nativeRoot, $nativeDestination, [IO.Compression.CompressionLevel]::Optimal, $true)
}
Initialize-PackageTiming -RepositoryRoot $root
function Set-GateCompiling([bool]$Compiling) {
    if (-not $GateTimingFile) {return}
    # A partial read is deliberately not evidence of a compile-only phase.
    $json = if ($Compiling) {'{"compiling":true}'} else {'{"compiling":false}'}
    [IO.File]::WriteAllText($GateTimingFile, $json, (New-Object Text.UTF8Encoding($false)))
}
if ($GateTimingFile) {
    if (-not [IO.Path]::IsPathRooted($GateTimingFile)) {$GateTimingFile = Join-Path $root $GateTimingFile}
    $GateTimingFile = [IO.Path]::GetFullPath($GateTimingFile)
    New-Item -ItemType Directory -Path (Split-Path -Parent $GateTimingFile) -Force | Out-Null
    Set-GateCompiling $false
}
if ($env:OS -ne 'Windows_NT') {throw 'Use Windows 11 / Windows build CI for the Windows release.'}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {throw 'Developer prerequisite: Rust stable MSVC toolchain, Visual Studio C++ Build Tools, Windows SDK. End users do not need these.'}
$hostInfo = & rustc -vV | Out-String
if ($LASTEXITCODE -ne 0 -or $hostInfo -notmatch 'host: x86_64-pc-windows-msvc') {throw 'Select the x86_64-pc-windows-msvc Rust toolchain before packaging this x64 build.'}
function Invoke-Cargo([string[]]$Arguments, [switch]$CaptureBuildMetadata) {
    $buildPhase = $Arguments.Count -gt 0 -and $Arguments[0] -eq 'build'
    $previousOutputEncoding = [Console]::OutputEncoding
    try {
        if ($buildPhase) {Set-GateCompiling $true}
        if ($CaptureBuildMetadata) {
            # 原生命令 stdout 按 UTF-8 解码，保留含中文的实际 Cargo 产物路径。
            [Console]::OutputEncoding = [Text.Encoding]::UTF8
            & cargo @Arguments '--message-format=json,json-render-diagnostics' | ForEach-Object {
                if ($_.TrimStart().StartsWith('{')) {
                    $message = $_ | ConvertFrom-Json
                    if ($message.reason -eq 'build-script-executed' -or
                        $message.reason -eq 'compiler-artifact') {
                        $message
                    }
                } else {
                    Write-Host $_
                }
            }
        } else {
            & cargo @Arguments
        }
        if ($LASTEXITCODE -ne 0) {throw "cargo $($Arguments -join ' ') failed: $LASTEXITCODE"}
    } finally {
        if ($buildPhase) {Set-GateCompiling $false}
        if ($CaptureBuildMetadata) {[Console]::OutputEncoding = $previousOutputEncoding}
    }
}
function Get-CargoArtifact([object[]]$Messages, [string]$BinaryName, [string]$ManifestPath) {
    $manifest = [IO.Path]::GetFullPath($ManifestPath)
    # PS 5.1 的 ConvertFrom-Json 可能把 JSON 数组作为一个管道对象返回；展开后逐条判定。
    $artifacts = @($Messages | ForEach-Object { $_ } | Where-Object {
        $reason = $_.PSObject.Properties['reason']
        if (-not $reason -or $reason.Value -ne 'compiler-artifact') {return $false}
        $target = $_.PSObject.Properties['target']
        $executable = $_.PSObject.Properties['executable']
        $packageManifest = $_.PSObject.Properties['manifest_path']
        if (-not $target -or -not $target.Value -or -not $executable -or
            -not $executable.Value -or -not $packageManifest -or -not $packageManifest.Value) {return $false}
        $name = $target.Value.PSObject.Properties['name']
        $kind = $target.Value.PSObject.Properties['kind']
        if (-not $name -or -not $kind) {return $false}
        $name.Value -ceq $BinaryName -and $kind.Value -contains 'bin' -and
        [IO.Path]::GetFullPath($packageManifest.Value).Equals($manifest, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($artifacts.Count -ne 1) {throw "无法唯一确定本次 Cargo 构建的 $BinaryName 可执行文件（找到 $($artifacts.Count) 项）。"}
    return $artifacts[0]
}
function Get-CargoLibraryArtifact([object[]]$Messages, [string]$LibraryName, [string]$ManifestPath) {
    $manifest = [IO.Path]::GetFullPath($ManifestPath)
    $artifacts = @($Messages | ForEach-Object { $_ } | Where-Object {
        $reason = $_.PSObject.Properties['reason']
        $target = $_.PSObject.Properties['target']
        $packageManifest = $_.PSObject.Properties['manifest_path']
        if (-not $reason -or $reason.Value -ne 'compiler-artifact' -or
            -not $target -or -not $target.Value -or -not $packageManifest -or -not $packageManifest.Value) {return $false}
        $name = $target.Value.PSObject.Properties['name']
        $kind = $target.Value.PSObject.Properties['kind']
        if (-not $name -or -not $kind) {return $false}
        $name.Value -ceq $LibraryName -and $kind.Value -contains 'lib' -and
        [IO.Path]::GetFullPath($packageManifest.Value).Equals($manifest, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($artifacts.Count -ne 1) {throw "无法唯一确定本次 Cargo 构建的 $LibraryName 库产物（找到 $($artifacts.Count) 项）。"}
    return $artifacts[0]
}
function Assert-EmbeddedEngine([object[]]$Messages, [object]$Artifact) {
    # 缓存命中的构建也由 Cargo 返回当前 OUT_DIR；不能按历史缓存时间猜测。
    $buildScripts = @($Messages | ForEach-Object { $_ } | Where-Object {
        $reason = $_.PSObject.Properties['reason']
        if (-not $reason -or $reason.Value -ne 'build-script-executed') {return $false}
        $packageId = $_.PSObject.Properties['package_id']
        $outDir = $_.PSObject.Properties['out_dir']
        $packageId -and $outDir -and $outDir.Value -and $packageId.Value -ceq $Artifact.package_id
    })
    if ($buildScripts.Count -ne 1) {throw '无法唯一确定本次 file-core 库构建的 OUT_DIR，拒绝打包。'}
    $statusPath = Join-Path $buildScripts[0].out_dir 'engine_embed_status.txt'
    if (-not (Test-Path -LiteralPath $statusPath -PathType Leaf)) {throw '本次 file-core 库构建缺少 engine_embed_status.txt，拒绝打包。'}
    $status = (Get-Content -LiteralPath $statusPath -Raw -Encoding UTF8).Trim()
    if ($status -cne 'ok') {throw "本次 file-core 库的引擎内嵌状态为 '$status'（应为 'ok'），拒绝打包未完整内嵌 7-Zip 的产物。"}
}
function Copy-PackageLicenseMaterials([object]$Package, [object]$SourcePackage, [string]$Destination) {
    $packageRoot = [IO.Path]::GetDirectoryName((ConvertTo-WindowsFileSystemPath $Package.manifest_path))
    $Destination = ConvertTo-WindowsFileSystemPath $Destination
    $materials = @(Get-ChildItem -LiteralPath $packageRoot -File |
        Where-Object {$_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)'} |
        ForEach-Object {$_.Name})
    if ($Package.license_file) {$materials += [string]$Package.license_file}
    if ($SourcePackage) {$materials += @($SourcePackage.license_and_notice_materials)}
    $materials = @($materials | Sort-Object -Unique)
    [IO.Directory]::CreateDirectory($Destination) | Out-Null
    foreach ($relative in $materials) {
        $source = [IO.Path]::Combine($packageRoot, $relative.Replace('/','\'))
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {throw "来源清单登记的许可证材料缺失：$source"}
        # 保留包内相对路径：不同子目录下的同名许可证不得覆盖或被漏掉。
        $destinationPath = [IO.Path]::Combine($Destination, $relative.Replace('/','\'))
        [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($destinationPath)) | Out-Null
        Copy-Item -LiteralPath $source -Destination $destinationPath -Force
    }
    return $materials.Count
}
Start-PackageStep 'assets'
$archiveEngine = Join-Path $root 'resources\7zip\7z.exe'
$archiveDll = Join-Path $root 'resources\7zip\7z.dll'
if (-not $Offline) {& (Join-Path $PSScriptRoot 'fetch-7zip.ps1')}
if (-not (Test-Path -LiteralPath $archiveEngine) -or -not (Test-Path -LiteralPath $archiveDll) -or -not (Test-Path -LiteralPath 'resources\7zip\manifest.json')) {throw 'The verified bundled engine is missing (need 7z.exe, 7z.dll and manifest.json). Run fetch-7zip.ps1 on a connected build machine first.'}
$extra = @()
if ($Offline) {$extra += '--offline'}
Start-PackageStep 'font-and-engine-validation'
# O-05/O-09: the result font is embedded in the shipped worker. Verify the exact
# source font and its license before building either delivery form.
$fontManifest = Get-Content -LiteralPath 'resources\snap-ocr-assets.json' -Raw -Encoding UTF8 | ConvertFrom-Json
$fontMember = @($fontManifest.assets | Where-Object {$_.id -ceq 'noto-sans-mono-cjk-sc'})[0].members[0]
$fontLicense = @($fontManifest.assets | Where-Object {$_.id -ceq 'noto-cjk-ofl-license'})[0]
foreach ($entry in @(
    @{name='NotoSansMonoCJKsc-Regular.otf';size=$fontMember.size_bytes;sha256=$fontMember.sha256},
    @{name='LICENSE-noto-ofl.txt';size=$fontLicense.size_bytes;sha256=$fontLicense.sha256}
)) {
    $fontPath = Join-Path 'resources\fonts' $entry.name
    if (-not (Test-Path -LiteralPath $fontPath -PathType Leaf) -or
        (Get-Item -LiteralPath $fontPath).Length -ne $entry.size -or
        (Get-FileHash -LiteralPath $fontPath -Algorithm SHA256).Hash.ToLowerInvariant() -cne $entry.sha256) {
        throw "Embedded screenshot font/license differs from the fixed manifest: $($entry.name)"
    }
}
$engineManifest = Get-Content -LiteralPath 'resources\7zip\manifest.json' -Raw | ConvertFrom-Json
foreach ($file in $engineManifest.files) {
    $path = Join-Path 'resources\7zip' $file.name
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $file.sha256) {throw "Bundled engine hash mismatch: $($file.name)"}
}
Start-PackageStep 'lockfile'
if (-not (Test-Path -LiteralPath 'Cargo.lock')) {Invoke-Cargo (@('generate-lockfile') + $extra)}
if (-not $SkipTests) {
    Start-PackageStep 'tests'
    Invoke-Cargo (@('check','--locked','--all-targets','--features','test-hooks') + $extra)
    # AGENTS.md 3.4：默认打包及 acceptance -WithPackage 都没有桌面授权。
    # 与 acceptance 复用同一 artifact 分类器，保留桌面用例的独立授权入口。
    $testBuildMessages = @(Invoke-Cargo -Arguments (@('test','--locked','--all-targets','--features','test-hooks','--no-run') + $extra) -CaptureBuildMetadata)
    $testArtifactLog = Join-Path (Split-Path -Parent $script:packageTiming.path) 'cargo-test-artifacts.jsonl'
    $testJsonLines = @($testBuildMessages | ForEach-Object {$_ | ConvertTo-Json -Depth 20 -Compress})
    [IO.File]::WriteAllLines($testArtifactLog, [string[]]$testJsonLines, (New-Object Text.UTF8Encoding($false)))
    $testPlanCode = @'
import json
import sys
from pathlib import Path
from scripts.gate_runtime import Result
from scripts.local_gate_plan import _test_binary_stages
stages = _test_binary_stages(Result('package-tests', 'OK', '', Path(sys.argv[1])))
print(json.dumps([{'name': stage.name, 'argv': stage.argv} for stage in stages]))
'@
    $testPlanJson = & python -c $testPlanCode $testArtifactLog
    if ($LASTEXITCODE -ne 0) {throw "Unable to classify package test artifacts: $LASTEXITCODE"}
    # Windows PowerShell 5.1 不枚举根 JSON 数组，显式展开后才逐个执行阶段。
    $testStages = @($testPlanJson | ConvertFrom-Json | ForEach-Object {$_})
    if ($testStages.Count -eq 0) {throw 'No non-desktop test artifacts were found; refusing to report zero tests as passed.'}
    foreach ($stage in $testStages) {
        $testExecutable = [string]$stage.argv[0]
        $testArguments = @($stage.argv | Select-Object -Skip 1)
        & $testExecutable @testArguments
        if ($LASTEXITCODE -ne 0) {throw "Package test $($stage.name) failed: $LASTEXITCODE"}
    }
    Write-Host 'NOT RUN desktop-tests (gui_flow and real_gui cases require separate explicit desktop acceptance authorization)'
    $previous = $env:JCHTOOLS_TEST_7ZIP
    try {
        $env:JCHTOOLS_TEST_7ZIP = $archiveEngine
        Invoke-Cargo (@('test','--locked','--test','archive','--features','test-hooks') + $extra + @('--','--ignored','--test-threads=1'))
        # 与本轮普通库测试保持同一 features/cache 维度，只执行显式真实引擎用例。
        Invoke-Cargo (@('test','--locked','-p','jchtools-recursive-extract','--lib','--features','test-hooks') + $extra + @('--','--ignored','--test-threads=1'))
    } finally {$env:JCHTOOLS_TEST_7ZIP = $previous}
}
# Build the screenshot OCR worker separately. It is shipped beside the main executable
# in both delivery forms (XB-25); the optional-components directory below is only a
# staging area for the build-time manifest backfill. Both Cargo invocations use the
# workspace lock.
Start-PackageStep 'worker-build'
$workerBuildMessages = @(Invoke-Cargo -Arguments (@('build','--locked','--release','--target','x86_64-pc-windows-msvc','--manifest-path','optional/snap-ocr-worker/Cargo.toml','--bin','snap-ocr-worker') + $extra) -CaptureBuildMetadata)
$workerArtifact = Get-CargoArtifact -Messages $workerBuildMessages -BinaryName 'snap-ocr-worker' -ManifestPath (Join-Path $root 'optional\snap-ocr-worker\Cargo.toml')
$workerExe = $workerArtifact.executable
if (-not (Test-Path -LiteralPath $workerExe -PathType Leaf)) {throw "Optional OCR worker build did not produce $workerExe"}
$builtWorkerBytes = (Get-Item -LiteralPath $workerExe).Length
if ($builtWorkerBytes -le 0) {throw 'Optional OCR worker executable is empty.'}
$builtWorkerSha = (Get-FileHash -LiteralPath $workerExe -Algorithm SHA256).Hash.ToLowerInvariant()
Start-PackageStep 'manifest'
$optionalStage = Join-Path $DestinationRoot 'optional-components-v0.1.2'
New-Item -ItemType Directory -Path $optionalStage -Force | Out-Null
$stagedWorker = Join-Path $optionalStage 'snap-ocr-worker.exe'
Copy-Item -LiteralPath $workerExe -Destination $stagedWorker -Force
# 构建期占位回填（设计见 crates/assets/src/snap_ocr_assets.rs 的 pending-build 状态）：仓库清单的
# worker 条目保持 pending-build 占位，打包阶段用本次构建的真实字节生成 staged 清单
# ——assets/build.rs 把它嵌入 JchTools.exe，发布目录的 resources/ 也用同一份。嵌入清单与
# 随包交付的 worker 因此逐字节一致（XB-25：worker 随主包交付，运行期安装按清单
# 校验大小与 SHA-256）；不再要求本地构建与某个已发布 release 字节相同（Rust/COFF
# 工具链版本不同会失配，旧口径在源码演进后离线打包必失败、在线打包则嵌入旧清单
# 交付新 worker，运行期校验拒绝服务）。
$manifest = Get-Content -LiteralPath 'resources\snap-ocr-assets.json' -Raw -Encoding UTF8 | ConvertFrom-Json
$workers = @($manifest.workers)
if ($workers.Count -ne 1 -or $workers[0].id -cne 'snap-ocr-worker' -or
    $workers[0].url -cne 'https://github.com/jchanghong023/JchTools/releases/download/optional-components-v0.1.2/snap-ocr-worker.exe' -or
    $workers[0].archive_type -cne 'file' -or
    $workers[0].install_path -cne 'worker/v0.1.2/snap-ocr-worker.exe' -or
    (@($workers[0].members)).Count -ne 0) {
    throw 'Optional OCR worker manifest does not contain the pinned release URL and destination.'
}
if ($workers[0].status -cne 'pending-build') {
    throw 'Optional OCR worker manifest must stay pending-build in the repository; packaging backfills the real size and SHA-256.'
}
$workers[0].status = 'ok'
$workers[0].size_bytes = [long]$builtWorkerBytes
$workers[0].sha256 = $builtWorkerSha
$utf8NoBom = New-Object Text.UTF8Encoding($false)
$manifestBytes = $utf8NoBom.GetBytes(($manifest | ConvertTo-Json -Depth 20))
$sha = [Security.Cryptography.SHA256]::Create()
try {$manifestSha = ([BitConverter]::ToString($sha.ComputeHash($manifestBytes))).Replace('-', '').ToLowerInvariant()}
finally {$sha.Dispose()}
# Content-addressed input lives beside the actual worker artifact, never under DestinationRoot.
$buildManifest = Join-Path (Split-Path -Parent $workerExe) "snap-ocr-assets-$manifestSha.json"
if (-not (Test-Path -LiteralPath $buildManifest -PathType Leaf) -or
    (Get-Item -LiteralPath $buildManifest).Length -ne $manifestBytes.Length -or
    (Get-FileHash -LiteralPath $buildManifest -Algorithm SHA256).Hash.ToLowerInvariant() -cne $manifestSha) {
    [IO.File]::WriteAllBytes($buildManifest, $manifestBytes)
}
if ((Get-Item -LiteralPath $buildManifest).Length -ne $manifestBytes.Length -or
    (Get-FileHash -LiteralPath $buildManifest -Algorithm SHA256).Hash.ToLowerInvariant() -cne $manifestSha) {
    throw 'Build-time screenshot manifest does not match its content-addressed input.'
}
$stagedManifest = Join-Path $optionalStage 'snap-ocr-assets.json'
Copy-Item -LiteralPath $buildManifest -Destination $stagedManifest -Force
if ((Get-Item -LiteralPath $stagedWorker).Length -ne $builtWorkerBytes -or
    (Get-FileHash -LiteralPath $stagedWorker -Algorithm SHA256).Hash.ToLowerInvariant() -cne $builtWorkerSha) {
    throw 'Optional OCR worker staging changed the built executable bytes.'
}
$utf8 = New-Object Text.UTF8Encoding($false)
# assets/build.rs embeds the exact stable manifest; restore the caller's environment
# even if compilation fails.
Start-PackageStep 'main-build'
$previousManifest = $env:JCHTOOLS_SNAP_OCR_MANIFEST
try {
    $env:JCHTOOLS_SNAP_OCR_MANIFEST = $buildManifest
    $mainBuildMessages = @(Invoke-Cargo -Arguments (@('build','--locked','--release','--target','x86_64-pc-windows-msvc','-p','jchtools','-p','jchocr','--bins') + $extra) -CaptureBuildMetadata)
} finally {
    $env:JCHTOOLS_SNAP_OCR_MANIFEST = $previousManifest
}
# fail-closed：本次可执行文件和内嵌状态都绑定 Cargo 的实际产物，不使用无关缓存。
$mainArtifact = Get-CargoArtifact -Messages $mainBuildMessages -BinaryName 'JchTools' -ManifestPath (Join-Path $root 'Cargo.toml')
$engineArtifact = Get-CargoLibraryArtifact -Messages $mainBuildMessages -LibraryName 'jchtools_file_core' -ManifestPath (Join-Path $root 'crates\file-core\Cargo.toml')
Assert-EmbeddedEngine -Messages $mainBuildMessages -Artifact $engineArtifact
$cliArtifact = Get-CargoArtifact -Messages $mainBuildMessages -BinaryName 'jchocr' -ManifestPath (Join-Path $root 'crates\jchocr\Cargo.toml')
foreach ($artifact in @($mainArtifact, $cliArtifact)) {
    if (-not (Test-Path -LiteralPath $artifact.executable -PathType Leaf)) {throw "本次构建未生成必需的可执行文件：$($artifact.executable)"}
    if ((Get-Item -LiteralPath $artifact.executable).Length -le 0) {throw "必需的可执行文件为空：$($artifact.executable)"}
}
Start-PackageStep 'staging'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$folder = Join-Path $DestinationRoot "JchTools-Windows-x64-$stamp"
New-Item -ItemType Directory -Path $folder | Out-Null
Copy-Item -LiteralPath $mainArtifact.executable -Destination $folder
Copy-Item -LiteralPath $cliArtifact.executable -Destination (Join-Path $folder 'jchocr.exe')
# 引擎已内嵌进 EXE；发布目录只保留许可证、NOTICE 与上游源码（LGPL 要求），
# 因此最终用户拿到的是单文件程序，缺引擎时运行期从 EXE 释放并校验 sha256。
$resources = Join-Path $folder 'resources'
New-Item -ItemType Directory -Path $resources | Out-Null
Copy-Item -LiteralPath $stagedManifest -Destination (Join-Path $resources 'snap-ocr-assets.json')
$fontNotices = Join-Path $resources 'fonts'
New-Item -ItemType Directory -Path $fontNotices | Out-Null
Copy-Item -LiteralPath 'resources\fonts\LICENSE-noto-ofl.txt' -Destination $fontNotices
$engineDir = Join-Path $resources '7zip'
New-Item -ItemType Directory -Path $engineDir | Out-Null
foreach ($item in @('manifest.json','NOTICE.txt','licenses')) {
    $source = Join-Path 'resources\7zip' $item
    if (-not (Test-Path -LiteralPath $source)) {throw "Bundled engine license material is missing from resources\7zip: $item. Run fetch-7zip.ps1 on a connected build machine."}
    Copy-Item -LiteralPath $source -Destination $engineDir -Recurse
}
$sourceEntry = $engineManifest.source
if ($null -eq $sourceEntry -or
    [string]$sourceEntry.name -notmatch '^7z\d{4}-src\.tar\.xz$' -or
    [string]$sourceEntry.sha256 -notmatch '^[0-9a-fA-F]{64}$') {
    throw 'The 7-Zip manifest is missing a valid source archive name or SHA-256.'
}
$sourceArchive = Join-Path 'resources\7zip' $sourceEntry.name
if (-not (Test-Path -LiteralPath $sourceArchive -PathType Leaf) -or
    (Get-FileHash -LiteralPath $sourceArchive -Algorithm SHA256).Hash.ToLowerInvariant() -cne ([string]$sourceEntry.sha256).ToLowerInvariant()) {
    throw 'The 7-Zip source archive does not match the SHA-256 recorded in the manifest.'
}
Copy-Item -LiteralPath $sourceArchive -Destination $engineDir
Copy-Item -LiteralPath 'README.md','LICENSE','THIRD_PARTY_NOTICES.md','Cargo.lock' -Destination $folder
Copy-Item -LiteralPath 'docs' -Destination $folder -Recurse
Copy-Item -LiteralPath 'scripts\launch-software.cmd' -Destination $folder
Start-PackageStep 'xberg-base'
# 同一已验证基础树用于 Inno Setup 与 ZIP；仅构建辅助脚本联网，build.rs 不获取资产。
& (Join-Path $PSScriptRoot 'prepare-xberg-runtime.ps1') -Destination (Join-Path $folder 'xberg') -Offline:$Offline
Start-PackageStep 'licenses'
# Collect license texts from the exact resolved dependency graph, not a guessed static list.
# cargo 的 stdout 是 UTF-8；中文 Windows 默认用 ANSI 代码页解码会破坏
# 非 ASCII 的 crate 描述，进而让 ConvertFrom-Json 在半截字符上失败。
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$metadataRaw = & cargo metadata --locked --format-version 1 @extra
if ($LASTEXITCODE -ne 0) {throw 'Unable to enumerate dependency notices.'}
$metadata = ($metadataRaw | Out-String) | ConvertFrom-Json
$noticeRoot = Join-Path $folder 'third-party-rust'
New-Item -ItemType Directory -Path $noticeRoot | Out-Null
$licenseIndex = @()
# AH-11：源码归属清单补齐发布 crate 归档中遗漏的上游许可证。
$sourceManifest = Get-Content -LiteralPath 'vendor\SOURCES.json' -Raw -Encoding UTF8 | ConvertFrom-Json
$sourceIndex = @{}
foreach ($sourcePackage in $sourceManifest.packages) {
    $sourceIndex["$($sourcePackage.name)@$($sourcePackage.version)"] = $sourcePackage
}
Copy-Item -LiteralPath 'vendor\SOURCES.json' -Destination (Join-Path $noticeRoot 'SOURCES.json')
foreach ($package in $metadata.packages) {
    if ($metadata.workspace_members -contains $package.id) {continue}
    $dest = Join-Path $noticeRoot "$($package.name)-$($package.version)"
    $sourcePackage = $sourceIndex["$($package.name)@$($package.version)"]
    $licenseFileCount = Copy-PackageLicenseMaterials -Package $package -SourcePackage $sourcePackage -Destination $dest
    $supplements = @()
    if ($sourcePackage) {
        $supplements = @($sourcePackage.supplemental_materials)
        $supplementNumber = 0
        foreach ($relative in $supplements) {
            $supplementNumber += 1
            $source = Join-Path $root $relative
            if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {throw "Missing recorded upstream license: $relative"}
            # 完整来源由 SOURCES.json 保留；随包短名按清单顺序编号，避免 MAX_PATH 与重名覆盖。
            $shortName = "{0:D3}-{1}" -f $supplementNumber, (Split-Path -Path $relative -Leaf)
            $destination = Join-Path (Join-Path $dest 'supplemental') $shortName
            New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
            Copy-Item -LiteralPath $source -Destination $destination
        }
    }
    $licenseIndex += @{name=$package.name;version=$package.version;license=$package.license;repository=$package.repository;license_files=$licenseFileCount;supplemental_files=$supplements}
}
$utf8 = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText((Join-Path $noticeRoot 'index.json'),($licenseIndex | ConvertTo-Json -Depth 5),$utf8)
Start-PackageStep 'package-validation'
# 发布目录不应出现引擎可执行文件：它们必须在 EXE 内部。
foreach ($name in @('7z.exe','7z.dll')) {
    if (Test-Path -LiteralPath (Join-Path $folder "resources\7zip\$name")) {throw "Engine executable leaked into the package: $name"}
}
if (-not (Test-Path -LiteralPath (Join-Path $folder 'resources\7zip\manifest.json'))) {throw 'Engine manifest is missing from the package.'}
# XB-27：后台服务/字体仍由本次构建提供，文档与截图推理资产统一随基础树交付。
Copy-Item -LiteralPath $workerExe -Destination (Join-Path $folder 'snap-ocr-worker.exe')
if ((Get-FileHash -LiteralPath (Join-Path $folder 'snap-ocr-worker.exe') -Algorithm SHA256).Hash -ne (Get-FileHash -LiteralPath $workerExe -Algorithm SHA256).Hash) {throw 'Bundled background service hash mismatch.'}
# 正向核对基础成员、模型身份、来源记录与实际字节；同时拒绝 FFmpeg/语音模型和未登记成员。
& (Join-Path $PSScriptRoot 'prepare-xberg-runtime.ps1') -Destination (Join-Path $folder 'xberg') -ValidateOnly
# 音频专属负载也不得散落到 Xberg 目录以外；7-Zip 内嵌与许可证校验保留。
Assert-PackageAudioExclusion -Directory $folder
# ===== 安装包（P-05/E-04）：Inno Setup 双形态交付的第二产物 =====
# ISCC 不可用时如实标注 NOT RUN 并继续产出便携 ZIP（CI 负责装 Inno Setup；本地缺件不阻断）。
# ISCC 编译前先写共同构建记录，使安装包也包含测试 NOT RUN 与未验证项。
# 安装包内的 installer 状态只能记录编译前事实，不能预报成功；
# 编译结束后更新便携目录中的真实结果，再生成 ZIP。
Start-PackageStep 'installer'
$version = (Select-String -LiteralPath 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$setupSummary = 'NOT RUN (ISCC not found on this machine; CI release job builds the installer)'
# Get-Command 找不到 ISCC 时返回 $null；Set-StrictMode Latest 下直接取 .Source 会抛
# PropertyNotFoundStrict 异常，导致「缺件不阻断」的降级路径永远走不到。
$isccCmd = Get-Command ISCC.exe -ErrorAction SilentlyContinue
$isccPath = if ($isccCmd) { $isccCmd.Source } else { $null }
if (-not $isccPath) {
    $isccPath = @(
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    ) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
}
$info = @{created=(Get-Date).ToUniversalTime().ToString('o');rustc=(& rustc --version | Out-String).Trim();tests= $(if($SkipTests){'NOT RUN'}else{'non-desktop cargo tests and real-engine archive tests passed on this build machine'});desktop_tests='NOT RUN (requires separate explicit desktop acceptance authorization)';installer=$(if($isccPath){'NOT VERIFIED (this record was captured before the installer compiler completed)'}else{$setupSummary});windows_ui_manual='NOT VERIFIED BY THIS SCRIPT';multi_tb_benchmark='NOT VERIFIED BY THIS SCRIPT';source_validation='Use local Windows test evidence for functional acceptance; CI only compiles and packages.'}
[IO.File]::WriteAllText((Join-Path $folder 'BUILD-INFO.json'),($info | ConvertTo-Json -Depth 5),$utf8)
if ($isccPath) {
    Invoke-InstallerCompiler -CompilerPath $isccPath -SourceDirectory $folder -OutputDirectory $DestinationRoot -Version $version -RepositoryRoot $root
    $setup = Join-Path $DestinationRoot 'JchTools-Setup-x64.exe'
    $setupLocation = if ($PSBoundParameters.ContainsKey('DestinationRoot')) {$setup} else {'dist\JchTools-Setup-x64.exe'}
    if (-not (Test-Path -LiteralPath $setup)) { throw "Installer was not produced at $setupLocation" }
    $setupSummary = "built $setupLocation (per-user install and CLI PATH, desktop shortcut, start menu, uninstall entry)"
    Write-Host "Created: $setup"
} else {
    Write-Warning "Inno Setup (ISCC.exe) not found: installer NOT RUN; portable ZIP is still produced."
}
$info.installer = $setupSummary
[IO.File]::WriteAllText((Join-Path $folder 'BUILD-INFO.json'),($info | ConvertTo-Json -Depth 5),$utf8)
if (-not $isccPath) {Stop-PackageStep -Status 'NOT_RUN' -Detail 'ISCC.exe not found; portable ZIP still produced'}
Start-PackageStep 'zip'
$zip = "$folder.zip"
New-PackageZip -Directory $folder -Destination $zip
Write-Host "Created: $zip"
Write-Host "Staged optional OCR worker and pinned manifest locally: $optionalStage (NOT UPLOADED; asset release requires separate authorization)"
Write-Host '解压此 ZIP 后可运行 JchTools.exe 或 jchocr.exe；安装包将 jchocr.exe 复制到用户已有 PATH 的 WindowsApps 目录，不修改 PATH，无需重启终端。'
Complete-PackageTiming
