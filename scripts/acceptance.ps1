#requires -Version 5.1
<#
.SYNOPSIS
  JchTools 单命令验收入口：把 AGENTS.md 3.2 完成条件矩阵串成一个退出码。

.DESCRIPTION
  默认执行（无需参数）：static_check.py（含测试基线与界面规则检查）、cargo test、
  Snap OCR core/worker 的 all-targets 测试与 clippy、构建输出 binding loop 警告扫描。可选阶段：
    -WithEngine   追加真实引擎用例（JCHTOOLS_TEST_7ZIP 或 resources\7zip\7z.exe，
                  缺失时直接失败并提示先运行 fetch-7zip.ps1）
    -WithGuiSmoke 追加 OS 级 UIA 冒烟 S1-S4（需要 -GuiData 指向 make_tmp.py testdata
                  生成的数据集与 cargo build 产物（尊重 CARGO_TARGET_DIR，未设置时为 target\debug）；需要 pip install pywinauto）
    -WithPackage  追加发布打包自检（package-windows.ps1，需要引擎与 MSVC 工具链）
    -WithMarkdownAcceptance 追加转 Markdown 验收承接（scripts/markdown_acceptance.py，
                  F26 / ALL2MARKDOWN 附录 A；退出码 0=PASS、2=全部条目 NOT RUN（缺资产/
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

function Invoke-Logged {
    # 以 PS 5.1 兼容的方式运行原生命令：临时放宽 EAP 避免 stderr（cargo 进度）被当成
    # 错误终止，凭退出码判定成败，完整输出落 .tmp\acceptance\<步骤>.log。
    param([string]$Name,[string]$File,[string[]]$Arguments = @(),[hashtable]$Environment)
    $log = Join-Path $script:LogDir (($Name -replace '[^\w.-]','_') + '.log')
    Write-Host "==> $Name"
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $saved = @{}
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

# 1) 静态检查：结构/配置/回调/SQL/测试基线/界面规则/产品名。
$null = Invoke-Logged -Name 'static-check' -File $python -Arguments @('scripts/static_check.py')

# 2) 全量测试（默认特性，含 GUI 与属性测试）。
$testLog = Invoke-Logged -Name 'cargo-test' -File 'cargo' -Arguments @('test','--all-targets')

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
        -Arguments @('test','--manifest-path',$manifest,'--all-targets')
    $null = Invoke-Logged -Name "$component-clippy" -File 'cargo' `
        -Arguments @('clippy','--manifest-path',$manifest,'--all-targets','--','-D','warnings')
}

# 4) 真实引擎用例（可选）。
if ($WithEngine) {
    $engine = $env:JCHTOOLS_TEST_7ZIP
    if (-not $engine) {
        $candidate = Join-Path $root 'resources\7zip\7z.exe'
        if (Test-Path -LiteralPath $candidate) {$engine = $candidate}
    }
    if (-not $engine) {throw '-WithEngine 需要真实引擎：设置 JCHTOOLS_TEST_7ZIP，或先运行 scripts/fetch-7zip.ps1'}
    $null = Invoke-Logged -Name 'engine-tests' -File 'cargo' `
        -Arguments @('test','--test','archive','--','--ignored','--test-threads=1') `
        -Environment @{JCHTOOLS_TEST_7ZIP = $engine}
} else {$script:Results.Add('NOT RUN  engine-tests（加 -WithEngine）')}

# 5) OS 级 GUI 冒烟 S1-S4（可选）。
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
    $null = Invoke-Logged -Name 'gui-build' -File 'cargo' -Arguments @('build')
    $null = Invoke-Logged -Name 'gui-smoke' -File $python -Arguments @('scripts/gui_smoke.py','--exe',$exe,'--data',$GuiData)
} else {$script:Results.Add('NOT RUN  gui-smoke S1-S4（加 -WithGuiSmoke -GuiData <目录>）')}

# 5.5) 可选媒体 worker 测试（workspace 成员、独立清单；F23——可选分发不等于可选验证）。
$null = Invoke-Logged -Name 'media-worker-tests' -File 'cargo' `
    -Arguments @('test','--manifest-path',(Join-Path $root 'optional/markdown-media-worker/Cargo.toml'),'--all-targets')

# 6) 发布打包自检（可选）。
if ($WithPackage) {
    $null = Invoke-Logged -Name 'package' -File 'powershell' `
        -Arguments @('-NoProfile','-File',(Join-Path $PSScriptRoot 'package-windows.ps1'))
} else {$script:Results.Add('NOT RUN  package（加 -WithPackage）')}

# 7) 转 Markdown 验收承接（可选；F26 / ALL2MARKDOWN 附录 A）。
if ($WithMarkdownAcceptance) {
    $mdArgs = @('scripts/markdown_acceptance.py') + $MarkdownArgs
    Write-Host '==> markdown-acceptance'
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {$mdOutput = & $python @mdArgs 2>&1} finally {$ErrorActionPreference = $previous}
    $mdCode = $LASTEXITCODE
    $mdLog = Join-Path $script:LogDir 'markdown-acceptance.log'
    $mdOutput | ForEach-Object {$_.ToString()} | Set-Content -LiteralPath $mdLog -Encoding UTF8
    if ($mdCode -eq 0) {
        $script:Results.Add('PASS  markdown-acceptance')
    } elseif ($mdCode -eq 2) {
        # 2 = 全部条目 NOT RUN（缺真实资产/被测物）：如实呈现，不当作通过，也不阻塞其余阶段。
        $script:Results.Add('NOT RUN  markdown-acceptance（全部条目缺资产/被测物；经 -MarkdownArgs 提供后重跑）')
    } else {
        $mdOutput | Select-Object -Last 12 | ForEach-Object {$_.ToString()} | Write-Host
        throw "markdown-acceptance 失败（退出码 $mdCode），完整日志：$mdLog"
    }
} else {$script:Results.Add('NOT RUN  markdown-acceptance（加 -WithMarkdownAcceptance [-MarkdownArgs <透传参数>]）')}

Write-Host ''
Write-Host '==== 验收汇总 ===='
$script:Results | ForEach-Object {Write-Host $_}
