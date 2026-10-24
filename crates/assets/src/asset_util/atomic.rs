//! 原子落位：单文件与整目录的备份/恢复/替换。
//!
//! staged → destination 的两段 rename 语义由截图 OCR、转 Markdown 与推理
//! 组件安装链共用：覆盖前把旧目标改名备份，新资产就位后清理备份（清理失败
//! 按 B-1 不构成安装失败），关键 rename 失败则恢复旧目标，失败不动已验证
//! 资产。

use std::fs;
use std::path::Path;
use uuid::Uuid;

pub(crate) fn atomic_replace_file(staged: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| format!("无法确定安装文件目录：{}", destination.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("创建安装文件目录失败：{error}"))?;
    let backup = parent.join(format!(".old-file-{}", Uuid::new_v4().simple()));
    let had_existing = destination.exists();
    if had_existing {
        fs::rename(destination, &backup).map_err(|error| format!("暂存旧文件失败：{error}"))?;
    }
    if let Err(error) = fs::rename(staged, destination) {
        let install_error = format!("原子就位文件失败：{error}");
        if had_existing {
            if let Err(restore_error) = restore_backup(&backup, destination, "文件") {
                return Err(format!("{install_error}；{restore_error}"));
            }
        }
        return Err(install_error);
    }
    if had_existing {
        // B-1：新文件已就位，关键变更成功；旧备份清理失败不构成安装失败
        //（此前 Windows 上防护软件/索引器短暂持有句柄即可把成功误报为失败）。
        // 残留为同目录下的 .old-file-<uuid>（罕见），不影响新资产使用，可安全
        // 手动删除；本函数无进度通道，静默忽略。
        let _ = fs::remove_file(&backup);
    }
    Ok(())
}

pub fn atomic_replace_dir(staged: &Path, destination: &Path) -> Result<(), String> {
    let parent = destination
        .parent()
        .ok_or_else(|| format!("无法确定安装目录：{}", destination.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("创建安装目录失败：{error}"))?;
    let backup = parent.join(format!(".old-{}", Uuid::new_v4().simple()));
    let had_existing = destination.exists();
    if had_existing {
        fs::rename(destination, &backup).map_err(|error| format!("暂存旧资产失败：{error}"))?;
    }
    if let Err(error) = fs::rename(staged, destination) {
        let install_error = format!("原子就位资产失败：{error}");
        if had_existing {
            if let Err(restore_error) = restore_backup(&backup, destination, "资产") {
                return Err(format!("{install_error}；{restore_error}"));
            }
        }
        return Err(install_error);
    }
    if had_existing {
        // B-1：新目录已就位，关键变更成功；旧备份清理失败不构成安装失败。
        // 残留为同目录下的 .old-<uuid>（罕见，防护软件/索引器短暂持有句柄）。
        // 覆盖情况：xberg-inference/<tag> 目标的残留按设计交给
        // prune_old_inference_tags 收集（它枚举 xberg-inference 下所有非 tag
        // 目录删除；该安装链当前仅在测试中启用，生产初始化只校验共享目录，
        // XB-10）；其余目标（licenses 等）的残留不影响新资产使用，
        // 可手动删除。本函数无进度通道，静默忽略。
        let _ = fs::remove_dir_all(&backup);
    }
    Ok(())
}

pub(crate) fn restore_backup(backup: &Path, destination: &Path, kind: &str) -> Result<(), String> {
    fs::rename(backup, destination).map_err(|error| {
        format!(
            "恢复旧{kind}失败：{error}；旧{kind}仍保留在：{}",
            backup.display()
        )
    })
}
