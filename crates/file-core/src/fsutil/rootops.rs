use super::git::{is_git_root, root_inside_git_project};
use super::workspace::{is_extraction_workspace, is_extraction_workspace_container};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// S-05 受保护目录解析的纯函数核心（测试注入用，不触碰真实环境变量）：
/// 缺失或无法规范化都报错——保护信息不可用时必须拒绝开始，不得静默失去系统目录保护。
pub(super) fn resolve_protected_root(raw: Option<&std::ffi::OsStr>) -> Result<PathBuf> {
    let raw = raw.context(
        "无法确定 Windows 系统目录（SystemRoot 未设置）；为避免误处理系统文件，拒绝开始处理",
    )?;
    fs::canonicalize(raw).with_context(|| {
        format!(
            "无法访问 Windows 系统目录（{}）；为避免误处理系统文件，拒绝开始处理",
            Path::new(raw).display()
        )
    })
}
/// 读取并规范化操作系统报告的实际 Windows 安装目录，不信任可改写的环境变量。
#[cfg(windows)]
pub fn protected_root() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

    let query = |buffer: &mut [u16]| -> Result<usize> {
        let capacity = u32::try_from(buffer.len()).context("Windows 系统目录长度超出范围")?;
        // SAFETY: 缓冲区可写且容量与传入长度一致；系统调用不会保留指针。
        let length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), capacity) };
        if length == 0 {
            return Err(std::io::Error::last_os_error()).context("无法确定实际 Windows 系统目录");
        }
        usize::try_from(length).context("Windows 系统目录长度超出范围")
    };
    let mut buffer = [0u16; 260];
    let length = query(&mut buffer)?;
    let directory = if length < buffer.len() {
        std::ffi::OsString::from_wide(&buffer[..length])
    } else {
        let mut extended = vec![
            0u16;
            length
                .checked_add(1)
                .context("Windows 系统目录长度超出范围")?
        ];
        let length = query(&mut extended)?;
        anyhow::ensure!(length < extended.len(), "无法完整读取实际 Windows 系统目录");
        std::ffi::OsString::from_wide(&extended[..length])
    };
    resolve_protected_root(Some(&directory))
}
/// S-05 的参数化核心：规范化根，并给出「根包含受保护目录时需整树剪枝的子树」。
/// - 根位于受保护目录内（含等于）：拒绝整次任务；
/// - 根是受保护目录的严格上层：不拒绝，返回 `(根, Some(受保护目录))`，由扫描、
///   确认框清点与收尾清理按该子树统一剪枝并提示（S-05 第二句）；
/// - 其余：返回 `(根, None)`。`protected` 为 None 表示没有受保护目录信息（非 Windows）。
pub fn normalize_root_with(
    path: &Path,
    protected: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let root = fs::canonicalize(path).context("无法访问目标目录")?;
    if !root.is_dir() {
        bail!("请选择普通目录");
    }
    #[cfg(not(windows))]
    if root.parent().is_none() {
        // 非产品平台维持原有根目录边界；Windows 盘根按 S-05 排除系统子树。
        bail!("不允许直接整理根目录");
    }
    // S-05 只授权拒绝 Windows 安装目录及其后代；Program Files、用户目录、
    // 盘根不自动扩大进拒绝清单，仍受其余范围与权限规则约束。
    if let Some(protected) = protected {
        if root.starts_with(protected) {
            bail!("不允许整理 Windows 系统目录");
        }
    }
    // X-08：直接选择真实标记工作区或其后代，也不能绕过扫描范围排除。
    // 只在候选容器命名族内增加必要 Git 检查，避免在 Git 树内读取 OWNER 内容；
    // 普通根不增加重复 Git 扫描，预检仍按原有 H-06 闸门执行。
    for directory in root.ancestors() {
        if !directory
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(is_extraction_workspace_container)
        {
            continue;
        }
        root_inside_git_project(directory)?;
        anyhow::ensure!(
            !is_git_root(directory)?,
            "所选目录位于 Git 项目内；按 H-06 本次处理不执行"
        );
        anyhow::ensure!(
            !is_extraction_workspace(directory)?,
            "所选目录位于带所有权标记的解压工作区内；本次处理不执行，请先将待处理资料移出工作区"
        );
    }
    let protected_prune = protected
        .filter(|protected| protected.starts_with(&root))
        .map(Path::to_path_buf);
    Ok((root, protected_prune))
}
pub fn normalize_root(path: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let protected = protected_root()?;
        Ok(normalize_root_with(path, Some(&protected))?.0)
    }
    #[cfg(not(windows))]
    {
        normalize_root_with(path, None).map(|(root, _)| root)
    }
}
