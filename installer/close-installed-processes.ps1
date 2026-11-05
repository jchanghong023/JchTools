#requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$InstallDirectory,
    [Parameter(Mandatory = $true)][string]$CliPath,
    [Parameter(Mandatory = $true)][string]$ControllerPath
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
Import-Module CimCmdlets -ErrorAction Stop
$installPrefix = [IO.Path]::GetFullPath($InstallDirectory).TrimEnd('\') + '\'
$installedCli = [IO.Path]::GetFullPath($CliPath)
if ($installPrefix -eq [IO.Path]::GetPathRoot($installPrefix)) {
    throw '安装目录不能是磁盘或共享根目录，已停止安装，未关闭任何进程。'
}

function Test-InstalledImage([string]$ImagePath) {
    if (-not $ImagePath) { return $false }
    $path = [IO.Path]::GetFullPath($ImagePath)
    return $path.StartsWith($installPrefix, [StringComparison]::OrdinalIgnoreCase) -or
        $path.Equals($installedCli, [StringComparison]::OrdinalIgnoreCase)
}

function Get-InstalledProcesses {
    Get-CimInstance Win32_Process -Property ProcessId,ExecutablePath,CommandLine |
        Where-Object { Test-InstalledImage $_.ExecutablePath }
}

function Get-ManagedService($ProcessInfo) {
    if ([IO.Path]::GetFileName($ProcessInfo.ExecutablePath) -ine 'JchTools.exe') { return '' }
    if (-not $ProcessInfo.CommandLine) { throw '无法读取旧主程序的角色，已停止安装，避免误终止维护服务。' }
    if ($ProcessInfo.CommandLine -match '--(codeagent|scheduled-tasks|system-pressure)-service(?:\s|"|$)') {
        return $Matches[1].ToLowerInvariant()
    }
    return ''
}

function Close-InstalledProcess($ProcessInfo) {
    $process = $null
    try {
        try { $process = [Diagnostics.Process]::GetProcessById([int]$ProcessInfo.ProcessId) }
        catch [ArgumentException] { return } # 枚举后已经自然退出。
        # 固定进程句柄后再次核对真实映像；不按名称、父 PID 或历史 PID 杀进程。
        $null = $process.Handle
        if ($process.HasExited) { return }
        if (-not (Test-InstalledImage $process.MainModule.FileName)) {
            throw '旧进程身份发生变化，已停止安装，未终止不属于安装目标的进程。'
        }
        Write-Output "正在结束本次安装所属旧进程 PID=$($process.Id)"
        $process.Kill()
        $process.WaitForExit()
    } finally {
        if ($null -ne $process) { $process.Dispose() }
    }
}

try {
    $services = @{}
    # 先结束 GUI 及推理/截图等客户进程，防止它们在维护期间再次启动后台。
    foreach ($processInfo in @(Get-InstalledProcesses)) {
        $service = Get-ManagedService $processInfo
        if ($service) { $services[$service] = $true }
        else { Close-InstalledProcess $processInfo }
    }
    foreach ($service in @('codeagent', 'scheduled-tasks', 'system-pressure')) {
        if (-not $services.ContainsKey($service)) { continue }
        # 只运行本次安装包的控制器，不执行可能不认识 --stop 的旧版镜像。
        # 仅对已确认属于本次安装目标的服务发停止请求，不干扰其他便携副本。
        $controller = Start-Process -FilePath $ControllerPath -ArgumentList @("--$service-service", '--stop') `
            -WorkingDirectory $InstallDirectory -WindowStyle Hidden -Wait -PassThru
        try {
            if ($controller.ExitCode -ne 0) { throw "后台 $service 未能安全退出，已停止安装。" }
        } finally { $controller.Dispose() }
    }
    # 收尾处理初次快照之后的残留客户进程；受保护服务必须已经自然退出。
    foreach ($processInfo in @(Get-InstalledProcesses)) {
        if (Get-ManagedService $processInfo) { throw '维护服务仍在运行，已停止安装，不强制覆盖旧文件。' }
        Close-InstalledProcess $processInfo
    }
    if (@(Get-InstalledProcesses).Count -ne 0) { throw '仍存在本次安装所属的旧进程，已停止安装。' }
    Write-Output '本次安装所属旧进程已全部退出，可以替换文件。'
    exit 0
} catch {
    Write-Error $_ -ErrorAction Continue
    exit 1
}
