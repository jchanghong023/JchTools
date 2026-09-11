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
if (-not $Offline) {& (Join-Path $PSScriptRoot 'fetch-7zip.ps1')}
if (-not (Test-Path -LiteralPath $archiveEngine) -or -not (Test-Path -LiteralPath 'resources\7zip\manifest.json')) {throw 'The verified bundled engine is missing. Run fetch-7zip.ps1 on a connected build machine first.'}
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
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$folder = Join-Path $root "dist\JchTools-Windows-x64-$stamp"
New-Item -ItemType Directory -Path $folder | Out-Null
Copy-Item -LiteralPath 'target\release\JchTools.exe','target\release\jchtools-cli.exe' -Destination $folder
$resources = Join-Path $folder 'resources'
New-Item -ItemType Directory -Path $resources | Out-Null
Copy-Item -LiteralPath 'resources\7zip' -Destination $resources -Recurse
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
# Check actual resources, independently of the runtime loader.
$manifest = Get-Content -LiteralPath (Join-Path $folder 'resources\7zip\manifest.json') -Raw | ConvertFrom-Json
foreach ($file in $manifest.files) {
    $path = Join-Path (Join-Path $folder 'resources\7zip') $file.name
    if ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $file.sha256) {throw "Packaging hash mismatch: $($file.name)"}
}
$zip = "$folder.zip"
Compress-Archive -LiteralPath $folder -DestinationPath $zip -CompressionLevel Optimal
Write-Host "Created: $zip"
Write-Host 'End users extract this ZIP and run JchTools.exe; no separate 7-Zip installation.'
