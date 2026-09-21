use crate::{config::DeleteMode, control::Control, fsutil};
use anyhow::{bail, Context, Result};
use std::{fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteResult {
    Kept,
    /// 已从磁盘永久删除。S-02 之后不再有回收站路径，删除不可由本软件恢复。
    Permanent,
}
/// 界面/日志展示用：去掉 Windows 扩展路径前缀，避免用户看到 `\\?\D:\...`。
pub fn display_path_text(path: &str) -> String {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = path.strip_prefix(r"\\?\") {
        local.to_string()
    } else {
        path.to_string()
    }
}
/// 界面展示用：把纳秒时间戳（可能为负）转成本地可读时间；不可表示时回落到原始数字。
pub fn display_time_text(ns: i64) -> String {
    let seconds = ns.div_euclid(1_000_000_000);
    chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
        || ns.to_string(),
        |utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        },
    )
}
/// 安全删除一个已规划的路径：拒绝链接/非空目录，删除前复查类型与空目录；
/// 用户取消后不执行任何删除（S-02：删除一律为永久删除，不经回收站，
/// `MUST NOT` 提供回收站选项或降级开关）。
/// P-08：假定处理期间文件不被其他程序改动，因此不再做删除前的快照比对。
pub fn remove(path: &Path, mode: DeleteMode, control: &Control) -> Result<DeleteResult> {
    control.checkpoint()?;
    if mode == DeleteMode::Keep {
        return Ok(DeleteResult::Kept);
    }
    let meta = fs::symlink_metadata(path)?;
    if fsutil::is_link(&meta) {
        bail!("拒绝删除链接 / reparse point");
    }
    if meta.is_dir() && fs::read_dir(path)?.next().is_some() {
        bail!("目录不是空目录，不会递归删除用户目录");
    }
    control.check_cancelled()?;
    // 永久删除前再确认一次类型与空目录（防检查后类型被替换）。
    let meta = fs::symlink_metadata(path)?;
    if fsutil::is_link(&meta) {
        bail!("拒绝删除链接 / reparse point");
    }
    if meta.is_dir() && fs::read_dir(path)?.next().is_some() {
        bail!("目录不是空目录，不会递归删除用户目录");
    }
    if meta.is_dir() {
        fs::remove_dir(path).context("删除空目录失败")?;
    } else {
        fs::remove_file(path).context("永久删除失败（未自动提升权限或修改只读属性）")?;
    }
    Ok(DeleteResult::Permanent)
}

#[cfg(test)]
mod tests {
    use super::{display_path_text, display_time_text};

    #[test]
    fn display_path_text_strips_extended_prefix() {
        assert_eq!(display_path_text(r"\\?\D:\testzip"), r"D:\testzip");
        assert_eq!(
            display_path_text(r"\\?\UNC\server\share"),
            r"\\server\share"
        );
        assert_eq!(display_path_text(r"D:\plain"), r"D:\plain");
    }

    #[test]
    fn display_time_text_formats_epoch_and_falls_back_on_overflow() {
        // 任意本地时区下都应格式化为日期时间（含 - 与 :），而不是回落数字。
        let formatted = display_time_text(0);
        assert!(
            formatted.contains('-') && formatted.contains(':'),
            "0ns 应格式化为日期时间：{formatted}"
        );
        // i64::MAX 纳秒约 2262 年，仍在 chrono 范围内：格式化成功即可。
        assert!(display_time_text(i64::MAX).contains('-'));
        // 负时间戳（如 1601 Windows FILETIME 原点之前）不得 panic。
        let _ = display_time_text(i64::MIN);
    }
}
