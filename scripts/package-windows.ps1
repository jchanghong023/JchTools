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
Copy-Item -LiteralPath (Join-Path $releaseDir 'JchTools.exe'),(Join-Path $releaseDir 'jchtools-cli.exe') -Destination $folder
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
$info = @{created=(Get-Date).ToUniversalTime().ToString('o');rustc=(& rustc --version | Out-String).Trim();tests= $(if($SkipTests){'NOT RUN'}else{'cargo tests and real-engine archive tests passed on this build machine'});windows_ui_manual='NOT VERIFIED BY THIS SCRIPT';multi_tb_benchmark='NOT VERIFIED BY THIS SCRIPT';source_validation='See docs/VALIDATION.md for the original source delivery environment.'}
[IO.File]::WriteAllText((Join-Path $folder 'BUILD-INFO.json'),($info | ConvertTo-Json -Depth 5),$utf8)
# 发布目录不应出现引擎可执行文件：它们必须在 EXE 内部。
foreach ($name in @('7z.exe','7z.dll')) {
    if (Test-Path -LiteralPath (Join-Path $folder "resources\7zip\$name")) {throw "Engine executable leaked into the package: $name"}
}
if (-not (Test-Path -LiteralPath (Join-Path $folder 'resources\7zip\manifest.json'))) {throw 'Engine manifest is missing from the package.'}
$zip = "$folder.zip"
Compress-Archive -LiteralPath $folder -DestinationPath $zip -CompressionLevel Optimal
Write-Host "Created: $zip"
Write-Host 'End users extract this ZIP and run JchTools.exe; no separate 7-Zip installation.'
