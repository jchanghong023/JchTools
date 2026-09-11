#requires -Version 5.1
[CmdletBinding()]
param(
    [string]$Version = '26.03',
    [string]$Destination = (Join-Path $PSScriptRoot '..\resources\7zip')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
if ($env:OS -ne 'Windows_NT') { throw 'This packaging helper requires Windows.' }
if ($Version -notmatch '^\d{2}\.\d{2}$') { throw 'Invalid 7-Zip release version.' }
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$cache = Join-Path $repo ".cache\7zip-$Version"
New-Item -ItemType Directory -Force -Path $cache,$Destination | Out-Null
$Destination = [IO.Path]::GetFullPath($Destination)
$headers = @{ 'User-Agent'='JchTools-Builder'; 'Accept'='application/vnd.github+json' }
if ($env:GITHUB_TOKEN) { $headers.Authorization = "Bearer $env:GITHUB_TOKEN" }
$releaseUrl = "https://api.github.com/repos/ip7z/7zip/releases/tags/$Version"
Write-Host "Reading official 7-Zip release $Version..."
$release = Invoke-RestMethod -Uri $releaseUrl -Headers $headers
function Get-VerifiedAsset([string]$Name) {
    $asset = @($release.assets | Where-Object { $_.name -ceq $Name })
    if ($asset.Count -ne 1) { throw "Official release asset is missing: $Name. No mirror fallback." }
    $asset = $asset[0]
    if ($asset.browser_download_url -notlike 'https://github.com/ip7z/7zip/releases/download/*') { throw 'Unexpected download host.' }
    if (-not $asset.PSObject.Properties['digest']) {throw "Upstream digest is missing: $Name"}
    $digestMatch = [regex]::Match([string]$asset.digest, '^sha256:([0-9a-fA-F]{64})$')
    if (-not $digestMatch.Success) {
        throw "No upstream SHA-256 digest for $Name. Refusing an unverified download."
    }
    $expected = $digestMatch.Groups[1].Value.ToLowerInvariant()
    $path = Join-Path $cache $Name
    if (-not (Test-Path -LiteralPath $path) -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected) {
        Write-Host "Downloading $Name..."
        Invoke-WebRequest -Uri $asset.browser_download_url -UseBasicParsing -OutFile "$path.download"
        if ((Get-FileHash -LiteralPath "$path.download" -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected) { throw "SHA-256 mismatch: $Name" }
        Move-Item -LiteralPath "$path.download" -Destination $path -Force
    }
    [PSCustomObject]@{Path=$path; Name=$Name; Sha256=$expected; Url=$asset.browser_download_url}
}
$digits = $Version.Replace('.','')
$msi = Get-VerifiedAsset "7z$digits-x64.msi"
$source = Get-VerifiedAsset "7z$digits-src.tar.xz"
# Administrative image extraction: does not register/install 7-Zip on the end user's machine.
# This step runs only on the developer's build machine, never in JchTools at runtime.
$staging = Join-Path $cache ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $staging | Out-Null
$log = Join-Path $cache 'administrative-extraction.log'
try {
    Write-Host 'Extracting the full official 7z.exe + 7z.dll from its administrative MSI image...'
    $arguments = "/a `"$($msi.Path)`" /qn TARGETDIR=`"$staging`" /L*v `"$log`""
    $process = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" -ArgumentList $arguments -Wait -PassThru
    if ($process.ExitCode -notin @(0,3010)) { throw "MSI extraction failed ($($process.ExitCode)); see $log" }
    $manifestFiles = @()
    foreach ($name in @('7z.exe','7z.dll')) {
        $items = @(Get-ChildItem -LiteralPath $staging -Recurse -File | Where-Object { $_.Name -ieq $name })
        if ($items.Count -ne 1) { throw "Expected one $name in the full 7-Zip image." }
        Copy-Item -LiteralPath $items[0].FullName -Destination (Join-Path $Destination $name) -Force
        $manifestFiles += @{name=$name;sha256=(Get-FileHash -LiteralPath (Join-Path $Destination $name) -Algorithm SHA256).Hash.ToLowerInvariant()}
    }
    $engine = Join-Path $Destination '7z.exe'
    $info = & $engine i 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0 -or $info -notmatch 'Rar') { throw 'Bundled engine does not advertise RAR support.' }
    $licenseFiles = @(Get-ChildItem -LiteralPath $staging -Recurse -File | Where-Object { $_.Name -match '^(License|copying).*(\.txt)?$' })
    if ($licenseFiles.Count -eq 0) { throw 'Official 7-Zip license files were not found.' }
    $licenses = Join-Path $Destination 'licenses'
    New-Item -ItemType Directory -Force -Path $licenses | Out-Null
    foreach ($file in $licenseFiles) {Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $licenses $file.Name) -Force}
    Copy-Item -LiteralPath $source.Path -Destination (Join-Path $Destination $source.Name) -Force
    $manifest = @{version=$Version;upstream='https://www.7-zip.org/';files=$manifestFiles;installer=@{name=$msi.Name;sha256=$msi.Sha256;url=$msi.Url};source=@{name=$source.Name;sha256=$source.Sha256;url=$source.Url}}
    $utf8 = New-Object Text.UTF8Encoding($false)
    [IO.File]::WriteAllText((Join-Path $Destination 'manifest.json'),($manifest | ConvertTo-Json -Depth 6),$utf8)
    [IO.File]::WriteAllText((Join-Path $Destination 'NOTICE.txt'),"7-Zip $Version by Igor Pavlov.`r`nBundled unmodified full console executable and DLL.`r`nLicense: LGPL-2.1-or-later, BSD portions, and unRAR restriction; see licenses.`r`nCorresponding upstream source archive is included alongside this notice.`r`nSource: $($source.Url)`r`n",$utf8)
    Write-Host "Verified full 7-Zip engine is ready in $Destination"
} finally {
    # Delete only the fresh GUID administrative extraction directory created above.
    if (Test-Path -LiteralPath $staging) {Remove-Item -LiteralPath $staging -Recurse -Force}
}
