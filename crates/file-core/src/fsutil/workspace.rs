use super::fold::is_link;
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::path::Path;

/// 工作区容器冲突只按现有不覆盖命名生成规范正整数后缀；不认可其他相似名称。
pub(super) fn is_extraction_workspace_container(name: &str) -> bool {
    if name.eq_ignore_ascii_case(".jchtools-work") {
        return true;
    }
    let Some((base, suffix)) = name.rsplit_once(" (") else {
        return false;
    };
    let Some(number) = suffix.strip_suffix(')') else {
        return false;
    };
    base.eq_ignore_ascii_case(".jchtools-work")
        && !number.is_empty()
        && !number.starts_with('0')
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// X-08：仅识别现有格式的真实解压工作区，不把同名用户目录当作工具所有物。
/// 识别不授予删除权限；清理仍须持有本次生成的所有权标记。
pub fn is_extraction_workspace(path: &Path) -> Result<bool> {
    use std::io::Read;

    if !path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(is_extraction_workspace_container)
    {
        return Ok(false);
    }
    let Some(owner) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    let mut canonical = [0u8; 36];
    if owner.len() != 36
        || !uuid::Uuid::parse_str(owner)
            .is_ok_and(|value| value.hyphenated().encode_lower(&mut canonical) == owner)
    {
        return Ok(false);
    }
    let directory = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("无法检查解压工作区目录"),
    };
    if is_link(&directory) || !directory.is_dir() {
        return Ok(false);
    }
    let marker = path.join("OWNER");
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("无法检查解压工作区所有权标记"),
    };
    anyhow::ensure!(
        !is_link(&metadata) && metadata.is_file(),
        "可疑解压工作区的所有权标记不是普通文件，拒绝进入"
    );
    if metadata.len() != 36 {
        return Ok(false);
    }
    let mut recorded_owner = String::with_capacity(36);
    File::open(&marker)
        .context("无法读取解压工作区所有权标记")?
        .take(37)
        .read_to_string(&mut recorded_owner)
        .context("无法读取解压工作区所有权标记")?;
    if recorded_owner != owner {
        return Ok(false);
    }
    let content = match fs::symlink_metadata(path.join("content")) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("无法检查解压工作区内容目录"),
    };
    anyhow::ensure!(
        !is_link(&content),
        "可疑解压工作区的内容目录为链接，拒绝进入"
    );
    Ok(content.is_dir())
}
