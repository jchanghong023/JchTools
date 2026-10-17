function ConvertTo-WindowsFileSystemPath([string]$Path) {
    # 显式 extended-length 路径让 PowerShell 5.1 的创建、复制与校验不依赖
    # LongPathsEnabled；保留本地盘和 UNC 目标，不引入会绕过链接检查的别名。
    $fullPath = [IO.Path]::GetFullPath($Path).Replace('/','\')
    if ($fullPath.StartsWith('\\?\', [StringComparison]::Ordinal)) {return $fullPath}
    if ($fullPath.StartsWith('\\', [StringComparison]::Ordinal)) {return '\\?\UNC\' + $fullPath.Substring(2)}
    return '\\?\' + $fullPath
}
