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
Invoke-Cargo (@('check','--locked','--all-targets','--features','test-hooks') + $extra)
if (-not $SkipTests) {
    Invoke-Cargo (@('test','--locked','--all-targets','--features','test-hooks') + $extra)
    $previous = $env:JCHTOOLS_TEST_7ZIP
    try {
        $env:JCHTOOLS_TEST_7ZIP = $archiveEngine
        Invoke-Cargo (@('test','--locked','--test','archive','--features','test-hooks') + $extra + @('--','--ignored','--test-threads=1'))
    } finally {$env:JCHTOOLS_TEST_7ZIP = $previous}
}
# Build the screenshot OCR worker separately. It is shipped beside the main executable
# in both delivery forms (XB-25); the optional-components directory below is only a
# staging area for the build-time manifest backfill. Both Cargo invocations use the
# workspace lock.
if ($env:CARGO_TARGET_DIR) {
    $targetDir = [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR)
} else {
    $targetDir = Join-Path $root 'target'
}
$releaseDir = Join-Path $targetDir 'release'
Invoke-Cargo (@('build','--locked','--release','--manifest-path','optional/snap-ocr-worker/Cargo.toml','--bin','snap-ocr-worker') + $extra)
$workerExe = Join-Path $releaseDir 'snap-ocr-worker.exe'
if (-not (Test-Path -LiteralPath $workerExe -PathType Leaf)) {throw "Optional OCR worker build did not produce $workerExe"}
$builtWorkerBytes = (Get-Item -LiteralPath $workerExe).Length
if ($builtWorkerBytes -le 0) {throw 'Optional OCR worker executable is empty.'}
$builtWorkerSha = (Get-FileHash -LiteralPath $workerExe -Algorithm SHA256).Hash.ToLowerInvariant()
$optionalStage = Join-Path $root 'dist\optional-components-v0.1.2'
New-Item -ItemType Directory -Path $optionalStage -Force | Out-Null
$stagedWorker = Join-Path $optionalStage 'snap-ocr-worker.exe'
Copy-Item -LiteralPath $workerExe -Destination $stagedWorker -Force
# 构建期占位回填（设计见 src/snap_ocr_assets.rs 的 pending-build 状态）：仓库清单的
# worker 条目保持 pending-build 占位，打包阶段用本次构建的真实字节生成 staged 清单
# ——build.rs 把它嵌入 JchTools.exe，发布目录的 resources/ 也用同一份。嵌入清单与
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
$stagedManifest = Join-Path $optionalStage 'snap-ocr-assets.json'
$utf8NoBom = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText($stagedManifest, ($manifest | ConvertTo-Json -Depth 20), $utf8NoBom)
if ((Get-Item -LiteralPath $stagedWorker).Length -ne $builtWorkerBytes -or
    (Get-FileHash -LiteralPath $stagedWorker -Algorithm SHA256).Hash.ToLowerInvariant() -cne $builtWorkerSha) {
    throw 'Optional OCR worker staging changed the built executable bytes.'
}
$utf8 = New-Object Text.UTF8Encoding($false)
# build.rs embeds the exact staged manifest into JchTools.exe; restore the caller's
# environment even if compilation fails.
$previousManifest = $env:JCHTOOLS_SNAP_OCR_MANIFEST
try {
    $env:JCHTOOLS_SNAP_OCR_MANIFEST = $stagedManifest
    Invoke-Cargo (@('build','--locked','--release','--bins') + $extra)
} finally {
    $env:JCHTOOLS_SNAP_OCR_MANIFEST = $previousManifest
}
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
Copy-Item -LiteralPath $stagedManifest -Destination (Join-Path $resources 'snap-ocr-assets.json')
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
# cargo 的 stdout 是 UTF-8；中文 Windows 默认用 ANSI 代码页解码会破坏
# 非 ASCII 的 crate 描述，进而让 ConvertFrom-Json 在半截字符上失败。
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
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
# XB-25: ship the background service; inference assets remain optional.
Copy-Item -LiteralPath $workerExe -Destination (Join-Path $folder 'snap-ocr-worker.exe')
if ((Get-FileHash -LiteralPath (Join-Path $folder 'snap-ocr-worker.exe') -Algorithm SHA256).Hash -ne (Get-FileHash -LiteralPath $workerExe -Algorithm SHA256).Hash) {throw 'Bundled background service hash mismatch.'}
foreach ($name in @('onnxruntime.dll','inference.onnx','NotoSansMonoCJKsc-Regular.otf','xberg.exe','det.onnx','rec.onnx','model.int8.onnx','silero_vad.onnx','tokens.txt','sherpa-onnx-c-api.dll','sherpa-onnx-cxx-api.dll','onnxruntime_providers_shared.dll','avcodec-63.dll','avformat-63.dll','avutil-61.dll','swresample-7.dll')) {
    if (@(Get-ChildItem -LiteralPath $folder -Recurse -File -Filter $name).Count -ne 0) {
        throw "Optional OCR payload leaked into the main installer/ZIP staging directory: $name"
    }
}
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
Write-Host "Staged optional OCR worker and pinned manifest locally: $optionalStage (NOT UPLOADED; asset release requires separate authorization)"
Write-Host 'End users extract this ZIP and run JchTools.exe; no separate 7-Zip installation.'
