use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

/// 只检查目录边界，不遍历 Git 工作树内部；`.git` 文件与目录均保护整树。
pub fn is_git_root(path: &Path) -> Result<bool> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        // 路径不存在或不是目录（如解压流程里按折叠键重建的成员路径）：
        // 与旧口径（path/.git 的 symlink_metadata NotFound → 无边界）一致，
        // 视为无 .git 边界而不是任务失败。
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("无法检查 Git 目录边界：{}", path.display()));
        }
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("无法检查 Git 目录边界：{}", path.display()))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// H-06：沿所选根的祖先到卷根检查直接目录项；任一祖先直接含 `.git` 说明所选根
/// 位于 Git 项目内部，两工具都必须拒绝整次处理（不拆散项目子树）。
pub fn root_inside_git_project(root: &Path) -> Result<()> {
    let mut current = root.parent();
    while let Some(dir) = current {
        if is_git_root(dir)? {
            bail!(
                "所选目录位于 Git 项目（{}）内部：为保护项目完整，本次处理不执行；请选择项目外的目录",
                crate::platform::display_path_text(&dir.to_string_lossy())
            );
        }
        current = dir.parent();
    }
    Ok(())
}
