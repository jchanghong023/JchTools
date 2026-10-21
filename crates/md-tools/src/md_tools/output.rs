//! 输出临时文件、分片命名与目标占用检查（F05/M-07/M-11）。
use anyhow::{Context as _, Result};
use std::{
    fs::{File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

/// 在输出目录内创建本次任务独有的临时文件（F05/M-07/M-11：合并与拆分都先写
/// 临时文件再改名落盘，失败或取消时不留半成品输出；拆分的确认覆盖经 rename
/// 替换目录项，与已存在目标同 inode 的硬链接（如输入的另一名字）不会被截断，
/// M-08 原文件不修改由此保证）。名称带进程号、纳秒时戳与递增序号且不以 `.md`
/// 结尾，不会被下一次扫描当作输入；`create_new` 保证绝不覆盖任何已有文件。
pub(super) fn create_md_temp_output(dir: &Path) -> Result<(PathBuf, File)> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let mut attempt = 0u32;
    loop {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |delta| delta.as_nanos());
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(
            ".jchtools-md-out-{}-{nanos}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists && attempt < 8 => {
                attempt += 1;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("创建输出临时文件失败：{}", candidate.display()));
            }
        }
    }
}

/// 拆分输出文件名（M-11）：`document.md` → `document_001.md`、`document_002.md`…
/// 至少三位编号；分片总数超过 999 时统一扩大编号宽度（按总数确定），
/// 保证文件名排序与编号数值顺序一致、互不重名。
pub fn split_names(file_name: &str, count: usize) -> Vec<String> {
    let stem = file_name.rsplit_once('.').map_or(file_name, |(s, _)| s);
    let width = count.to_string().len().max(3);
    (1..=count)
        .map(|i| format!("{stem}_{i:0width$}.md"))
        .collect()
}

/// 输出路径已有目录项即视为冲突，包含目标不存在或无法解析的 reparse point。
/// 不跟随链接，保证合并、拆分与界面确认采用同一占用判定（M-07/M-11）。
pub fn output_entry_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// 目标目录中已存在的同名分片（M-11：写入前检测，不静默覆盖）。
pub fn conflicting_outputs(out_dir: &Path, names: &[String]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| out_dir.join(name))
        .filter(|path| output_entry_exists(path))
        .collect()
}
