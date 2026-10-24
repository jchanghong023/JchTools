//! zip 归档安全解包。
//!
//! 下载归档的解包语义由截图 OCR 与转 Markdown 共用（T-06/O-09）：拒绝路径
//! 越界、Windows 替代数据流（冒号寻址）、符号链接成员与重复路径；同名成员
//! 按文件系统实际同名规则拒绝碰撞、保留已解出的首个成员；解包全程可取消。

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use zip::ZipArchive;

use super::ensure_not_cancelled;

pub fn extract_zip_safely(
    archive: &Path,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<(), String> {
    ensure_not_cancelled(cancel)?;
    let file = File::open(archive).map_err(|error| format!("打开压缩包失败：{error}"))?;
    let mut zip = ZipArchive::new(file).map_err(|error| format!("读取压缩包失败：{error}"))?;
    let mut seen = HashSet::new();
    for index in 0..zip.len() {
        ensure_not_cancelled(cancel)?;
        let mut entry = zip
            .by_index(index)
            .map_err(|error| format!("读取压缩包条目失败：{error}"))?;
        // zip 9 的成员名可能是非 UTF-8：name() 返回 Result，先取出再复用。
        let entry_name = entry
            .name()
            .map_err(|error| format!("读取压缩包条目名称失败：{error}"))?;
        // Windows 冒号可寻址替代数据流，enclosed_name 只保证路径不越界。
        if entry_name.contains(':') {
            return Err(format!("压缩包包含不安全路径：{entry_name}"));
        }
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| format!("压缩包包含不安全路径：{entry_name}"))?
            .clone();
        if !seen.insert(relative.clone()) {
            return Err(format!("压缩包包含重复路径：{}", relative.display()));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
        {
            return Err(format!("压缩包包含符号链接：{entry_name}"));
        }
        let target = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|error| format!("创建解包目录失败：{error}"))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("创建解包目录失败：{error}"))?;
        }
        // 使用文件系统的实际同名规则拒绝碰撞，保留已解出的首个成员。
        let mut output = File::options()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| format!("创建解包文件失败：{error}"))?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            ensure_not_cancelled(cancel)?;
            let read = entry
                .read(&mut buffer)
                .map_err(|error| format!("读取解包文件失败：{error}"))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("写入解包文件失败：{error}"))?;
        }
        output
            .sync_all()
            .map_err(|error| format!("同步解包文件失败：{error}"))?;
    }
    Ok(())
}
