#requires -Version 5.1
[CmdletBinding()]
param([switch]$SkipTests, [switch]$Offline)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($env:OS -ne 'Windows_NT') {throw 'Use Windows 11 / Windows build CI for the Windows release.'}
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Set-Location -LiteralPath $root
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {throw 'Developer prerequisite: Rust stable MSVC toolchain, Visual Studio C++ Build Tools, Windows SDK. End users do not need these.'}
$hostInfo = & rustc -vV | Out-String
if ($LASTEXITCODE -ne 0 -or $hostInfo -notmatch 'host: x86_64-pc-windows-msvc') {throw 'Select the x86_64-pc-windows-msvc Rust toolchain before packaging this x64 build.'}
function Invoke-Cargo([string[]]$Arguments) {
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {throw "cargo $($Arguments -join ' ') failed: $LASTEXITCODE"}
}
$archiveEngine = Join-Path $root 'resources\7zip\7z.exe'
$archiveDll = Join-Path $root 'resources\7zip\7z.dll'
if (-not $Offline) {& (Join-Path $PSScriptRoot 'fetch-7zip.ps1')}
if (-not (Test-Path -LiteralPath $archiveEngine) -or -not (Test-Path -LiteralPath $archiveDll) -or -not (Test-Path -LiteralPath 'resources\7zip\manifest.json')) {throw 'The verified bundled engine is missing (need 7z.exe, 7z.dll and manifest.json). Run fetch-7zip.ps1 on a connected build machine first.'}
$extra = @()
if ($Offline) {$extra += '--offline'}
if (-not (Test-Path -LiteralPath 'Cargo.lock')) {Invoke-Cargo (@('generate-lockfile') + $extra)}
Invoke-Cargo (@('check','--locked','--all-targets') + $extra)
if (-not $SkipTests) {
    Invoke-Cargo (@('test','--locked','--all-targets') + $extra)
    $previous = $env:JCHTOOLS_TEST_7ZIP
    try {
        $env:JCHTOOLS_TEST_7ZIP = $archiveEngine
        Invoke-Cargo (@('test','--locked','--test','archive') + $extra + @('--','--ignored','--test-threads=1'))
    } finally {$env:JCHTOOLS_TEST_7ZIP = $previous}
}
Invoke-Cargo (@('build','--locked','--release','--bins') + $extra)
# 尊重 CARGO_TARGET_DIR：未设置时回落到默认 target 目录。
if ($env:CARGO_TARGET_DIR) {
    $targetDir = $env:CARGO_TARGET_DIR
} else {
    $targetDir = Join-Path $root 'target'
}
$releaseDir = Join-Path $targetDir 'release'
# fail-closed：要求 build.rs 已把引擎真正编进 EXE。build.rs 部分失败只 warning，
# 这里读 OUT_DIR/engine_embed_status.txt，不是 "ok" 就中止，避免打包出未内嵌引擎的发布包。
$engineStatusFile = Get-ChildItem -LiteralPath $releaseDir -Recurse -Filter 'engine_embed_status.txt' -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($null -eq $engineStatusFile) {throw 'engine_embed_status.txt not found after build; the engine was not embedded into the EXE.'}
$engineStatus = (Get-Content -LiteralPath $engineStatusFile.FullName -Raw).Trim()
if ($engineStatus -ne 'ok') {throw "Engine embed status is '$engineStatus' (expected 'ok'); refuse to package a build without a fully embedded 7-Zip engine."}
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
# 内嵌前先校验引擎与清单一致：EXE 里编进去的就是这份经过校验的数据。
$engineManifest = Get-Content -LiteralPath 'resources\7zip\manifest.json' -Raw | ConvertFrom-Json
foreach ($file in $engineManifest.files) {
    $path = Join-Path 'resources\7zip' $file.name
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $file.sha256) {throw "Bundled engine hash mismatch: $($file.name)"}
}
$folder = Join-Path $root "dist\JchTools-Windows-x64-$stamp"
New-Item -ItemType Directory -Path $folder | Out-Null
Copy-Item -LiteralPath (Join-Path $releaseDir 'JchTools.exe') -Destination $folder
# 引擎已内嵌进 EXE；发布目录只保留许可证、NOTICE 与上游源码（LGPL 要求），
# 因此最终用户拿到的是单文件程序，缺引擎时运行期从 EXE 释放并校验 sha256。
$resources = Join-Path $folder 'resources'
New-Item -ItemType Directory -Path $resources | Out-Null
$engineDir = Join-Path $resources '7zip'
New-Item -ItemType Directory -Path $engineDir | Out-Null
foreach ($item in @('manifest.json','NOTICE.txt','licenses')) {
    $source = Join-Path 'resources\7zip' $item
    if (-not (Test-Path -LiteralPath $source)) {throw "Bundled engine license material is missing from resources\7zip: $item. Run fetch-7zip.ps1 on a connected build machine."}
    Copy-Item -LiteralPath $source -Destination $engineDir -Recurse
}
$sourceArchives = @(Get-ChildItem -LiteralPath 'resources\7zip' -File | Where-Object {$_.Name -like '7z*-src.tar.xz'})
if ($sourceArchives.Count -eq 0) {throw 'The upstream 7-Zip source archive is missing from resources\7zip: run fetch-7zip.ps1 on a connected build machine.'}
$sourceArchives | ForEach-Object {Copy-Item -LiteralPath $_.FullName -Destination $engineDir}
Copy-Item -LiteralPath 'README.md','LICENSE','THIRD_PARTY_NOTICES.md','Cargo.lock' -Destination $folder
Copy-Item -LiteralPath 'docs' -Destination $folder -Recurse
Copy-Item -LiteralPath 'scripts\launch-software.cmd' -Destination $folder
# Collect license texts from the exact resolved dependency graph, not a guessed static list.
$metadataRaw = & cargo metadata --locked --format-version 1 @extra
if ($LASTEXITCODE -ne 0) {throw 'Unable to enumerate dependency notices.'}
$metadata = ($metadataRaw | Out-String) | ConvertFrom-Json
$noticeRoot = Join-Path $folder 'third-party-rust'
New-Item -ItemType Directory -Path $noticeRoot | Out-Null
$licenseIndex = @()
foreach ($package in $metadata.packages) {
    if ($package.name -eq 'jchtools') {continue}
    $packageRoot = Split-Path -Parent $package.manifest_path
    $dest = Join-Path $noticeRoot "$($package.name)-$($package.version)"
    New-Item -ItemType Directory -Path $dest | Out-Null
    $licenseFiles = @(Get-ChildItem -LiteralPath $packageRoot -File | Where-Object {$_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)'})
    if ($package.license_file) {
        $explicit = Join-Path $packageRoot $package.license_file
        if (Test-Path -LiteralPath $explicit) {$licenseFiles += Get-Item -LiteralPath $explicit}
    }
    foreach ($file in ($licenseFiles | Sort-Object FullName -Unique)) {Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $dest $file.Name) -Force}
    $licenseIndex += @{name=$package.name;version=$package.version;license=$package.license;repository=$package.repository;license_files=$licenseFiles.Count}
}
$utf8 = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText((Join-Path $noticeRoot 'index.json'),($licenseIndex | ConvertTo-Json -Depth 5),$utf8)
# 发布目录不应出现引擎可执行文件：它们必须在 EXE 内部。
foreach ($name in @('7z.exe','7z.dll')) {
    if (Test-Path -LiteralPath (Join-Path $folder "resources\7zip\$name")) {throw "Engine executable leaked into the package: $name"}
}
if (-not (Test-Path -LiteralPath (Join-Path $folder 'resources\7zip\manifest.json'))) {throw 'Engine manifest is missing from the package.'}
# ===== 安装包（P-05/E-04）：Inno Setup 双形态交付的第二产物 =====
# ISCC 不可用时如实标注 NOT RUN 并继续产出便携 ZIP（CI 负责装 Inno Setup；本地缺件不阻断）。
# 安装包先于 ZIP 构建：BUILD-INFO.json 需要记录安装包阶段的真实结果，且必须在
# Compress-Archive 之前写入 $folder，否则不会进入便携 ZIP（此前时序相反导致两份
# 产物都不含构建记录，NOT RUN/NOT VERIFIED 标注只剩 CI 日志可见）。
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
if ($isccPath) {
    & $isccPath "/DSourceDir=$folder" "/DOutputDir=$(Join-Path $root 'dist')" "/DVersion=$version" (Join-Path $root 'installer\JchTools.iss')
    if ($LASTEXITCODE -ne 0) { throw "Inno Setup compiler failed: $LASTEXITCODE" }
    $setup = Join-Path $root 'dist\JchTools-Setup-x64.exe'
    if (-not (Test-Path -LiteralPath $setup)) { throw 'Installer was not produced at dist\JchTools-Setup-x64.exe' }
    $setupSummary = 'built dist\JchTools-Setup-x64.exe (per-user install, desktop shortcut, start menu, uninstall entry)'
    Write-Host "Created: $setup"
} else {
    Write-Warning "Inno Setup (ISCC.exe) not found: installer NOT RUN; portable ZIP is still produced."
}
$info = @{created=(Get-Date).ToUniversalTime().ToString('o');rustc=(& rustc --version | Out-String).Trim();tests= $(if($SkipTests){'NOT RUN'}else{'cargo tests and real-engine archive tests passed on this build machine'});installer=$setupSummary;windows_ui_manual='NOT VERIFIED BY THIS SCRIPT';multi_tb_benchmark='NOT VERIFIED BY THIS SCRIPT';source_validation='See git history and CI runs for validation evidence.'}
[IO.File]::WriteAllText((Join-Path $folder 'BUILD-INFO.json'),($info | ConvertTo-Json -Depth 5),$utf8)
$zip = "$folder.zip"
# Compress-Archive 逐条目写入 LastWriteTime；早于 1980-01-01（ZIP DOS 纪元）的时间无法
# 转换会直接失败。cargo registry 抽取的 crate 许可证常保留 tarball 的古董 mtime（实测
# 1970/1973 等），这里把暂存副本里过旧的时间规范化到当前时间；原始 registry 文件不受影响。
$zipEpoch = [datetime]::new(1980, 1, 1, 0, 0, 0, [System.DateTimeKind]::Utc)
foreach ($item in (Get-ChildItem -LiteralPath $folder -Recurse -Force)) {
    if ($item.LastWriteTimeUtc -lt $zipEpoch) { $item.LastWriteTime = Get-Date }
}
Compress-Archive -LiteralPath $folder -DestinationPath $zip -CompressionLevel Optimal
Write-Host "Created: $zip"
Write-Host 'End users extract this ZIP and run JchTools.exe; no separate 7-Zip installation.'
