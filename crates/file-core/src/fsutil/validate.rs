use super::fold::is_link;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub fn validate_component(name: &str) -> Result<()> {
    // Windows 禁止尾随 ASCII 空格/点；合法 Unicode 空白由可选名称规范化规则处理。
    if name.is_empty() || name == "." || name == ".." || name.ends_with(['.', ' ']) {
        bail!("不安全或不兼容 Windows 的名称：{name:?}");
    }
    if name
        .chars()
        .any(|c| c <= '\u{1f}' || "<>:\"/\\|?*".contains(c))
    {
        bail!("文件名包含 Windows 不支持的字符：{name:?}");
    }
    // 官方保留名清单为 COM1-9 / LPT1-9（COM0/LPT0 并非保留名，可正常创建），
    // 这里按官方清单拒绝，不做额外扩大，避免误拒用户磁盘上真实存在的合法文件。
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CLOCK$"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().count() == 4
            && stem
                .chars()
                .last()
                .is_some_and(|c| "123456789¹²³".contains(c)))
    {
        bail!("Windows 保留文件名：{name}");
    }
    if name.encode_utf16().count() > 255 {
        bail!("单个文件名超过 255 个 UTF-16 单元");
    }
    Ok(())
}
pub fn safe_relative(raw: &str) -> Result<PathBuf> {
    if raw.starts_with('/') || raw.starts_with('\\') {
        bail!("拒绝压缩包绝对路径：{raw:?}");
    }
    let normalized = raw.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        validate_component(part)?;
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        bail!("空文件路径");
    }
    Ok(out)
}
pub fn path_string(path: &Path) -> Result<String> {
    Ok(path
        .to_str()
        .context("路径含不能无损表示为 UTF-8 的字符，已拒绝处理")?
        .to_string())
}
pub fn relative_string(root: &Path, path: &Path) -> Result<String> {
    Ok(path_string(path.strip_prefix(root).context("路径不在选定目录内")?)?.replace('\\', "/"))
}

pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let p = safe_relative(rel)?;
    let mut current = root.to_path_buf();
    for part in p.components() {
        if !matches!(part, Component::Normal(_)) {
            bail!("非法路径组件");
        }
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => bail!(
                "拒绝符号链接 / junction / reparse point：{}",
                current.display()
            ),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).with_context(|| format!("无法检查 {}", current.display())),
        }
    }
    Ok(current)
}
/// S-04：所选根本身或其任一上级组件是符号链接/junction/reparse point 时拒绝开始。
/// 检查必须作用于用户给出的原始路径（canonicalize 之前）——规范化会把链接解析成
/// 目标路径，reparse 身份随之丢失，链接边界就再也检不出来，两工具会沿 canonicalize
/// 结果处理链接目标树。逐组件（含根本身）用 symlink_metadata 判定并复用 [`is_link`]；
/// 不读取链接目标、不搬移、不删除链接本身。
pub fn ensure_plain_entry(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            // 盘符前缀、根分隔符与 `.`/`..` 组件不是可判定的目录项，跳过；
            // 路径最终是否存在、是否为目录交给 normalize_root 报告。
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => bail!(
                "所选路径或其上级包含符号链接/junction（{}）；已按 S-04 拒绝开始：不跟随链接、不搬移或删除链接本身，请直接选择实际目录",
                current.display()
            ),
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => {
                return Err(e).with_context(|| format!("无法检查路径边界：{}", current.display()));
            }
        }
    }
    Ok(())
}
