#requires -Version 5.1
[CmdletBinding()]
param([switch]$Offline)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$manifest = Get-Content -LiteralPath (Join-Path $root 'resources\git-bundle.json') -Raw | ConvertFrom-Json
$cache = Join-Path $root ('.tmp\git-' + $manifest.version)
& python (Join-Path $PSScriptRoot 'make_tmp.py') workspace --destination $cache
if ($LASTEXITCODE -ne 0) { throw 'Unable to create the isolated Git packaging cache.' }
$archive = Join-Path $cache 'PortableGit.7z.exe'
if (-not (Test-Path -LiteralPath $archive -PathType Leaf) -or
    (Get-Item -LiteralPath $archive).Length -ne $manifest.size_bytes -or
    (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant() -cne $manifest.sha256) {
    if ($Offline) { throw 'The verified Portable Git archive is absent from the build cache.' }
    if ($manifest.url -notlike 'https://github.com/git-for-windows/git/releases/download/*') { throw 'Unexpected Portable Git download source.' }
    Invoke-WebRequest -UseBasicParsing -Uri $manifest.url -OutFile ($archive + '.download')
    if ((Get-Item -LiteralPath ($archive + '.download')).Length -ne $manifest.size_bytes -or
        (Get-FileHash -LiteralPath ($archive + '.download') -Algorithm SHA256).Hash.ToLowerInvariant() -cne $manifest.sha256) { throw 'Portable Git archive differs from its official release digest.' }
    Move-Item -LiteralPath ($archive + '.download') -Destination $archive -Force
}
$destination = Join-Path $cache 'runtime'
$engine = Join-Path $root 'resources\7zip\7z.exe'
if (-not (Test-Path -LiteralPath $engine -PathType Leaf)) { throw 'The verified 7-Zip build engine is needed to unpack Portable Git.' }
& $engine x $archive ('-o' + $destination) -y | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Portable Git extraction failed.' }
# Complete the upstream portable setup inside this owned cache. Do not execute
# host-file/mount-table copying: those files belong to the build machine and must
# never be included in a release for another Windows user.
$resolvedCache = [IO.Path]::GetFullPath($cache).TrimEnd('\') + '\'
$resolvedRuntime = [IO.Path]::GetFullPath($destination)
if (-not $resolvedRuntime.StartsWith($resolvedCache, [StringComparison]::OrdinalIgnoreCase)) { throw 'Portable Git staging escaped its owned cache.' }
foreach ($scriptName in @('06-windows-files.post','03-mtab.post')) {
    $hostSetup = Join-Path $destination ('etc\post-install\' + $scriptName)
    if (Test-Path -LiteralPath $hostSetup) { Remove-Item -LiteralPath $hostSetup -Force }
}
$postInstall = Join-Path $destination 'post-install.bat'
$setupRunner = Join-Path $destination 'jchtools-post-install.bat'
Copy-Item -LiteralPath $postInstall -Destination $setupRunner -Force
$previousPath = $env:PATH
$previousGitExecPath = $env:GIT_EXEC_PATH
try {
    # The upstream shell script calls bare git; pin it to this staging tree,
    # otherwise a developer's installed Git could be selected and modified.
    $env:PATH = (Join-Path $destination 'cmd') + ';' + (Join-Path $destination 'ucrt64\bin') + ';' + (Join-Path $destination 'usr\bin') + ';' + $previousPath
    $env:GIT_EXEC_PATH = $null
    & $setupRunner
    $setupExit = $LASTEXITCODE
} finally {
    $env:PATH = $previousPath
    $env:GIT_EXEC_PATH = $previousGitExecPath
}
Remove-Item -LiteralPath $setupRunner -Force
if ($setupExit -ne 0 -or (Test-Path -LiteralPath $postInstall) -or
    -not (Test-Path -LiteralPath (Join-Path $destination 'ucrt64\libexec\git-core\dlls-copied.exe')) -or
    -not (Test-Path -LiteralPath (Join-Path $destination 'ucrt64\libexec\git-core\libcurl-4.dll'))) { throw 'Portable Git post-install did not complete.' }
foreach ($hostFile in @('hosts','protocols','services','networks','mtab')) {
    if (Test-Path -LiteralPath (Join-Path $destination ('etc\' + $hostFile))) { throw 'A build-machine file leaked into the Portable Git runtime.' }
}
$git = Join-Path $destination 'cmd\git.exe'
if (-not (Test-Path -LiteralPath $git -PathType Leaf) -or
    -not (Test-Path -LiteralPath (Join-Path $destination 'LICENSE.txt') -PathType Leaf)) { throw 'Portable Git is missing its executable or upstream license.' }
$version = & $git --version
if ($LASTEXITCODE -ne 0 -or $version -cne ('git version ' + $manifest.version)) { throw 'Portable Git version does not match the pinned release.' }
Copy-Item -LiteralPath (Join-Path $root 'resources\git-bundle.json') -Destination (Join-Path $destination 'JCHTOOLS-BUNDLE.json') -Force
Write-Output ('Verified Portable Git: ' + $destination)
