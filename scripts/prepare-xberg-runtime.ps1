#requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Destination,
    [switch]$Offline,
    [switch]$ValidateOnly,
    [string]$CacheDirectory
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Assert-XbergRelativePath([string]$Path) {
    if (-not $Path -or $Path.Contains('\') -or $Path.StartsWith('/') -or
        $Path -match '(^|/)(\.|\.\.)(/|$)|//|[\x00-\x1f:<>"|?*]|[. ](/|$)' -or
        $Path -match '(^|/)(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.[^/]*)?(/|$)' -or
        $Path.EndsWith('/')) {throw "Xberg 成员路径不安全：$Path"}
}
function Assert-XbergFile([string]$Path, [object]$Record) {
    if ([string]$Record.sha256 -cnotmatch '^[0-9a-f]{64}$' -or [long]$Record.size_bytes -le 0) {
        throw "Xberg 大小或 SHA-256 无效：$Path"
    }
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $item.Length -ne [long]$Record.size_bytes -or
        (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() -cne [string]$Record.sha256) {
        throw "Xberg 文件完整性校验失败：$Path"
    }
}
function Get-XbergReleaseAsset([object]$Release, [string]$Name) {
    if ([string]$Release.tag_name -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$' -or
        $Release.draft -ne $false -or $Release.prerelease -ne $false -or
        [string]$Release.html_url -cne "https://github.com/jchanghong023/xberg/releases/tag/$($Release.tag_name)") {
        throw 'Xberg 发布来源、正式发布状态或标签无效。'
    }
    $matches = @($Release.assets | Where-Object {$_.name -ceq $Name})
    if ($matches.Count -ne 1) {throw "Xberg 发布资产缺失或重复：$Name"}
    $asset = $matches[0]
    $url = "https://github.com/jchanghong023/xberg/releases/download/$($Release.tag_name)/$Name"
    if ($asset.state -cne 'uploaded' -or $asset.browser_download_url -cne $url -or
        [long]$asset.size -le 0 -or [string]$asset.digest -cnotmatch '^sha256:[0-9a-f]{64}$') {
        throw "Xberg 发布资产的固定 URL、大小或 API 摘要无效：$Name"
    }
    return [pscustomobject]@{archive=$Name;url=$url;size_bytes=[long]$asset.size;sha256=$asset.digest.Substring(7)}
}
function Save-XbergDownload([string]$Url, [string]$Path, [hashtable]$Headers) {
    $part = "$Path.$([guid]::NewGuid().ToString('N')).part"
    try {
        Invoke-WebRequest -Uri $Url -Headers $Headers -UseBasicParsing -OutFile $part | Out-Null
        Move-Item -LiteralPath $part -Destination $Path -Force
    } finally {
        if (Test-Path -LiteralPath $part) {Remove-Item -LiteralPath $part -Force}
    }
}
function Get-XbergCachedAsset([object]$Record, [string]$Cache, [switch]$OfflineMode) {
    # Content addressing prevents a later release from silently reusing a same-name ZIP.
    $path = Join-Path $Cache "$($Record.sha256)-$($Record.archive)"
    if (Test-Path -LiteralPath $path) {
        Assert-XbergFile $path $Record
        Write-Host "复用已重新校验的 Xberg 构建缓存：$($Record.archive)"
    } else {
        if ($OfflineMode) {throw "离线构建缺少已锁定的 Xberg 资产缓存：$($Record.archive)"}
        Save-XbergDownload $Record.url $path @{'User-Agent'='JchTools-release-builder'}
        Assert-XbergFile $path $Record
    }
    return $path
}
function Get-XbergMemberMap([object]$Component) {
    $map = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
    if (@($Component.requires).Count -ne 0 -or @($Component.members).Count -eq 0 -or
        [long]$Component.size_bytes -le 0 -or [string]$Component.sha256 -cnotmatch '^[0-9a-f]{64}$') {
        throw "Xberg 组件清单无效：$($Component.id)"
    }
    foreach ($member in $Component.members) {
        $path = [string]$member.path
        Assert-XbergRelativePath $path
        if ([long]$member.size_bytes -le 0 -or [string]$member.sha256 -cnotmatch '^[0-9a-f]{64}$' -or $map.ContainsKey($path)) {
            throw "Xberg 成员重复或完整性记录无效：$path"
        }
        $map.Add($path, $member)
    }
    # Files must not also be parents of another file (including Windows case aliases).
    foreach ($path in $map.Keys) {
        $parent = $path
        while ($parent.Contains('/')) {
            $parent = $parent.Substring(0, $parent.LastIndexOf('/'))
            if ($map.ContainsKey($parent)) {throw "Xberg 文件与目录冲突：$parent"}
        }
    }
    return ,$map
}
function Test-XbergAudioPath([string]$Path) {
    return $Path -match '^(?i:ffmpeg/|models/(sense_voice_[^/]+|vad)/|samples/)' -or
        $Path -match '(?i)(^|/)(avcodec|avformat|avutil|swresample|swscale|avfilter|avdevice)(-[0-9]+)?\.dll$' -or
        $Path -match '(?i)(^|/)((silero[^/]*|sensevoice[^/]*|whisper[^/]*)\.(onnx|bin|gguf)|model\.int8\.onnx|tokens\.txt|silence-1s\.wav)$'
}
function Assert-XbergBaseMembers([object[]]$Components, [string]$RepositoryRoot) {
    if ($Components.Count -ne 2 -or $Components[0].id -cne 'document' -or $Components[1].id -cne 'snapshot') {
        throw '随包 Xberg 基础组件必须恰好为 document、snapshot。'
    }
    $union = [Collections.Generic.Dictionary[string,object]]::new([StringComparer]::OrdinalIgnoreCase)
    $maps = @()
    foreach ($component in $Components) {
        $map = Get-XbergMemberMap $component
        $maps += ,$map
        foreach ($path in $map.Keys) {
            if (Test-XbergAudioPath $path) {throw "Xberg 基础包不得含音频专属资产：$path"}
            if ($path -ceq 'jchtools-xberg-release.json') {throw '上游组件不得覆盖 JchTools 来源记录。'}
            if ($union.ContainsKey($path)) {
                $other = $union[$path]
                if ($other.path -cne $path -or $other.size_bytes -ne $map[$path].size_bytes -or
                    $other.sha256 -cne $map[$path].sha256) {throw "Xberg 共用成员字节不一致：$path"}
            } else {$union.Add($path, $map[$path])}
        }
    }
    foreach ($path in $union.Keys) {
        $parent = $path
        while ($parent.Contains('/')) {
            $parent = $parent.Substring(0, $parent.LastIndexOf('/'))
            if ($union.ContainsKey($parent)) {throw "Xberg 跨组件文件与目录冲突：$parent"}
        }
    }
    $required = @('xberg.exe','LICENSE','THIRD_PARTY_LICENSES.md','onnxruntime.dll',
        'onnxruntime_providers_shared.dll','MSVCP140.dll','MSVCP140_1.dll','VCRUNTIME140.dll','VCRUNTIME140_1.dll',
        'models/paddleocr-onnx-models-LICENSE.txt','sherpa-onnx/LICENSE',
        'sherpa-onnx/onnxruntime.dll','sherpa-onnx/onnxruntime_providers_shared.dll',
        'sherpa-onnx/sherpa-onnx-c-api.dll','sherpa-onnx/sherpa-onnx-cxx-api.dll')
    foreach ($path in $required) {
        foreach ($map in $maps) {
            if (-not $map.ContainsKey($path) -or $map[$path].path -cne $path) {throw "Xberg 独立基础包缺少共用运行库或许可：$path"}
        }
    }
    foreach ($name in @('onnxruntime.dll','onnxruntime_providers_shared.dll')) {
        if ($union[$name].size_bytes -ne $union["sherpa-onnx/$name"].size_bytes -or
            $union[$name].sha256 -cne $union["sherpa-onnx/$name"].sha256) {throw "Xberg 根目录与 sherpa 运行库身份不一致：$name"}
    }
    # 固定模型清单是唯一身份来源；发布版本更新不允许静默更换文档 tiny、
    # 截图 medium 检测、TextSnap small 识别及原字典。
    $fixed = Get-Content -LiteralPath (Join-Path $RepositoryRoot 'resources/markdown-assets.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $identities = @($fixed.xberg.members | Where-Object {
        $_.path -cmatch '^models/(models--xberg-io--paddleocr-onnx-models/|snapshot-ocr/)'
    })
    if ($identities.Count -ne 7) {throw '固定文档 tiny / 截图 medium 检测与 small 识别模型身份清单不完整。'}
    foreach ($identity in $identities) {
        $path = [string]$identity.path
        if (-not $union.ContainsKey($path) -or $union[$path].path -cne $path -or
            $union[$path].size_bytes -ne $identity.size_bytes -or $union[$path].sha256 -cne $identity.sha256) {
            throw "Xberg 文档 tiny / 截图 medium 检测与 small 识别模型或字典身份不匹配：$path"
        }
        $owner = if ($path.StartsWith('models/snapshot-ocr/')) {1} else {0}
        if (-not $maps[$owner].ContainsKey($path) -or $maps[1 - $owner].ContainsKey($path)) {
            throw "Xberg 两套 OCR 模型归属混用：$path"
        }
    }
    foreach ($path in $union.Keys) {
        if ($path.StartsWith('models/', [StringComparison]::Ordinal) -and
            $path -cne 'models/paddleocr-onnx-models-LICENSE.txt' -and $identities.path -cnotcontains $path) {
            throw "Xberg 基础包含未批准的模型或模型数据：$path"
        }
    }
    return ,$union
}
function Expand-XbergComponent([string]$Archive, [object]$Component, [string]$Stage) {
    Assert-XbergFile $Archive $Component
    $members = Get-XbergMemberMap $Component
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        foreach ($entry in $zip.Entries) {
            $prefix = 'xberg-cli-x86_64-pc-windows-msvc/'
            if (-not $entry.FullName.StartsWith($prefix, [StringComparison]::Ordinal)) {throw "Xberg ZIP 根目录不正确：$($entry.FullName)"}
            $path = $entry.FullName.Substring($prefix.Length)
            Assert-XbergRelativePath $path
            if (-not $seen.Add($path) -or -not $members.ContainsKey($path) -or
                $members[$path].path -cne $path -or $entry.Length -ne $members[$path].size_bytes -or
                (($entry.ExternalAttributes -shr 16) -band 0xF000) -eq 0xA000 -or
                ($entry.ExternalAttributes -band 0x400) -ne 0) {throw "Xberg ZIP 成员重复、不安全或与清单不同：$path"}
            $source = $entry.Open()
            $hash = [Security.Cryptography.SHA256]::Create()
            try {$digest = ([BitConverter]::ToString($hash.ComputeHash($source))).Replace('-','').ToLowerInvariant()}
            finally {$source.Dispose(); $hash.Dispose()}
            if ($digest -cne $members[$path].sha256) {throw "Xberg ZIP 成员 SHA-256 不匹配：$path"}
            $target = Join-Path $Stage $path
            if (Test-Path -LiteralPath $target) {Assert-XbergFile $target $members[$path]; continue}
            New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
            $source = $entry.Open()
            $output = [IO.File]::Open($target, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
            try {$source.CopyTo($output)} finally {$source.Dispose(); $output.Dispose()}
            Assert-XbergFile $target $members[$path]
        }
        if ($seen.Count -ne $members.Count) {throw "Xberg ZIP 缺少清单成员：$($Component.id)"}
    } finally {$zip.Dispose()}
}
function Assert-XbergPackage([string]$Runtime, [string]$RepositoryRoot) {
    $receipt = Get-Content -LiteralPath (Join-Path $Runtime 'jchtools-xberg-release.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($receipt.owner -cne 'JchTools-bundled-xberg-v1' -or
        $receipt.source -cne 'https://api.github.com/repos/jchanghong023/xberg/releases/latest' -or
        [string]$receipt.tag -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') {throw '随包 Xberg 来源记录无效。'}
    $releasePrefix = "https://github.com/jchanghong023/xberg/releases/download/$($receipt.tag)/"
    if ($receipt.manifest.archive -cne 'xberg-components-x86_64-pc-windows-msvc.json' -or
        $receipt.manifest.url -cne ($releasePrefix + $receipt.manifest.archive) -or
        [long]$receipt.manifest.size_bytes -le 0 -or [string]$receipt.manifest.sha256 -cnotmatch '^[0-9a-f]{64}$') {
        throw '随包 Xberg manifest 来源记录无效。'
    }
    foreach ($component in $receipt.components) {
        if ($component.archive -cne "xberg-$($component.id)-x86_64-pc-windows-msvc.zip") {throw '随包 Xberg 组件归档名无效。'}
    }
    $union = Assert-XbergBaseMembers @($receipt.components) $RepositoryRoot
    $audio = $receipt.audio_assets
    if ($audio.id -cne 'audio-assets' -or $audio.archive -cne 'xberg-audio-assets-x86_64-pc-windows-msvc.zip' -or
        $audio.url -cne ($releasePrefix + $audio.archive)) {throw '随包 Xberg 音频来源记录无效。'}
    $audioMap = Get-XbergMemberMap $audio
    foreach ($path in $audioMap.Keys) {
        if ($path -cnotmatch '^(ffmpeg/|models/(sense_voice_[^/]+|vad)/|samples/)' -or $union.ContainsKey($path)) {throw "音频附加资产不得覆盖基础成员或带入其他路径：$path"}
    }
    foreach ($path in $union.Keys) {Assert-XbergFile (Join-Path $Runtime $path) $union[$path]}
    foreach ($item in (@(Get-Item -LiteralPath $Runtime -Force) + @(Get-ChildItem -LiteralPath $Runtime -Recurse -Force))) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {throw "随包 Xberg 不允许链接：$($item.FullName)"}
        if ($item.FullName -ceq $Runtime) {continue}
        $path = $item.FullName.Substring($Runtime.TrimEnd('\','/').Length + 1).Replace('\','/')
        if ($item.PSIsContainer) {
            if (Test-XbergAudioPath ($path + '/')) {throw "随包 Xberg 不得含音频资产目录：$path"}
            continue
        }
        if ($path -cne 'jchtools-xberg-release.json' -and (-not $union.ContainsKey($path) -or (Test-XbergAudioPath $path))) {
            throw "随包 Xberg 含未登记或音频专属文件：$path"
        }
    }
}

function Install-XbergBaseFiles([string]$Stage, [string]$Destination, [string]$RepositoryRoot) {
    Assert-XbergPackage $Stage $RepositoryRoot
    $receipt = Get-Content -LiteralPath (Join-Path $Stage 'jchtools-xberg-release.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $union = Assert-XbergBaseMembers @($receipt.components) $RepositoryRoot
    # Only supplied members are refreshed; preserve downloaded audio and all unrelated files.
    $ancestor = $Destination
    while ($ancestor) {
        if (Test-Path -LiteralPath $ancestor) {
            if (((Get-Item -LiteralPath $ancestor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {throw "Xberg 目标祖先路径不能是链接：$ancestor"}
        }
        $ancestor = Split-Path -Parent $ancestor
    }
    New-Item -ItemType Directory -Path $Destination -Force | Out-Null
    foreach ($item in (Get-ChildItem -LiteralPath $Destination -Recurse -Force)) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {throw "Xberg 目标目录不能含链接：$($item.FullName)"}
    }
    $paths = @($union.Keys) + @('jchtools-xberg-release.json')
    foreach ($relative in $paths) {
        $target = Join-Path $Destination $relative
        if (Test-Path -LiteralPath $target -PathType Container) {throw "Xberg 基础文件目标已被目录占用：$relative"}
        $parent = Split-Path -Parent $target
        while ($parent -and $parent -cne $Destination) {
            if (Test-Path -LiteralPath $parent -PathType Leaf) {throw "Xberg 基础目录目标已被文件占用：$parent"}
            $parent = Split-Path -Parent $parent
        }
    }
    foreach ($relative in $paths) {
        $target = Join-Path $Destination $relative
        New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
        Copy-Item -LiteralPath (Join-Path $Stage $relative) -Destination $target -Force
    }
    foreach ($path in $union.Keys) {Assert-XbergFile (Join-Path $Destination $path) $union[$path]}
}

$repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if ($ValidateOnly) {Assert-XbergPackage ([IO.Path]::GetFullPath($Destination)) $repository; return}
if (-not $CacheDirectory) {$CacheDirectory = Join-Path $repository '.tmp/xberg-release-cache'}
$CacheDirectory = [IO.Path]::GetFullPath($CacheDirectory)
$Destination = [IO.Path]::GetFullPath($Destination)
& python (Join-Path $repository 'scripts/make_tmp.py') workspace --destination $CacheDirectory
if ($LASTEXITCODE -ne 0) {throw '无法创建 Xberg 构建缓存。'}
Add-Type -AssemblyName System.IO.Compression.FileSystem
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$source = 'https://api.github.com/repos/jchanghong023/xberg/releases/latest'
$releaseFile = Join-Path $CacheDirectory 'locked-release.json'
if ($Offline) {
    if (-not (Test-Path -LiteralPath $releaseFile -PathType Leaf)) {throw '离线构建需要先联网获取正式发布元数据与已校验基础资产；不能离线解析 latest。'}
    Write-Host '离线构建复用缓存的固定发布来源记录；不声称它仍是最新版本。'
} else {
    $headers = @{'User-Agent'='JchTools-release-builder';'Accept'='application/vnd.github+json';'X-GitHub-Api-Version'='2022-11-28'}
    if ($env:GITHUB_TOKEN) {$headers['Authorization'] = "Bearer $env:GITHUB_TOKEN"}
    Save-XbergDownload $source $releaseFile $headers
}
$release = Get-Content -LiteralPath $releaseFile -Raw -Encoding UTF8 | ConvertFrom-Json
$manifestRecord = Get-XbergReleaseAsset $release 'xberg-components-x86_64-pc-windows-msvc.json'
$manifestPath = Get-XbergCachedAsset $manifestRecord $CacheDirectory -OfflineMode:$Offline
$manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ($manifest.schema_version -ne 1 -or $manifest.target -cne 'x86_64-pc-windows-msvc' -or @($manifest.components).Count -ne 3) {
    throw 'Xberg 功能组件 manifest schema、target 或组件数量无效。'
}
$components = @()
foreach ($id in @('document','snapshot','media')) {
    $matches = @($manifest.components | Where-Object {$_.id -ceq $id})
    if ($matches.Count -ne 1 -or $matches[0].archive -cne "xberg-$id-x86_64-pc-windows-msvc.zip") {throw "Xberg 功能组件标识或归档名无效：$id"}
    if ($id -cne 'media') {
        $record = Get-XbergReleaseAsset $release $matches[0].archive
        if ($record.size_bytes -ne $matches[0].size_bytes -or $record.sha256 -cne $matches[0].sha256) {throw "Xberg manifest 与发布 API 摘要不符：$id"}
        $components += $matches[0]
    }
}
$union = Assert-XbergBaseMembers $components $repository
$audio = $manifest.audio_assets
if ($audio.id -cne 'audio-assets' -or $audio.archive -cne 'xberg-audio-assets-x86_64-pc-windows-msvc.zip') {throw 'Xberg 发布缺少专用音频附加资产记录。'}
$audioRecord = Get-XbergReleaseAsset $release $audio.archive
if ($audioRecord.size_bytes -ne $audio.size_bytes -or $audioRecord.sha256 -cne $audio.sha256) {throw 'Xberg 音频清单与发布 API 摘要不一致。'}
$audioMap = Get-XbergMemberMap $audio
foreach ($path in $audioMap.Keys) {
    if ($path -cnotmatch '^(ffmpeg/|models/(sense_voice_[^/]+|vad)/|samples/)' -or $union.ContainsKey($path)) {throw "Xberg 音频附加包含非音频或共用成员：$path"}
}
$workspace = Join-Path $repository ('.tmp/xberg-prepare-' + [guid]::NewGuid().ToString('N'))
& python (Join-Path $repository 'scripts/make_tmp.py') workspace --destination $workspace
if ($LASTEXITCODE -ne 0) {throw '无法创建 Xberg 基础资产暂存目录。'}
try {
    foreach ($component in $components) {
        $record = Get-XbergReleaseAsset $release $component.archive
        $archive = Get-XbergCachedAsset $record $CacheDirectory -OfflineMode:$Offline
        Expand-XbergComponent $archive $component $workspace
    }
    $audioReceipt = [ordered]@{}
    foreach ($property in $audio.PSObject.Properties) {$audioReceipt[$property.Name] = $property.Value}
    $audioReceipt['url'] = $audioRecord.url
    $receipt = [ordered]@{owner='JchTools-bundled-xberg-v1';tag=$release.tag_name;source=$source;manifest=$manifestRecord;components=$components;audio_assets=$audioReceipt}
    [IO.File]::WriteAllText((Join-Path $workspace 'jchtools-xberg-release.json'), ($receipt | ConvertTo-Json -Depth 12) + "`n", (New-Object Text.UTF8Encoding($false)))
    Install-XbergBaseFiles $workspace $Destination $repository
    Write-Host "已准备 Xberg 基础运行资产：$($release.tag_name)（document + snapshot，不下载 media/audio ZIP）"
} finally {
    Remove-Item -LiteralPath $workspace -Recurse -Force
}
