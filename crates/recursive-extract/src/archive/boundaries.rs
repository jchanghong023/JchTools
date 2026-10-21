use anyhow::{ensure, Context, Result};
use jchtools_file_core::{fsutil, rules};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// 成员路径里出现 `.git` 组件时，返回「直接含该 .git 的那个目录」的序数折叠相对路径
/// （空串 = 归档根）。H-06 的边界按目录项识别，不深入 Git 树内部。
pub(super) fn git_boundary_prefix(rel: &str) -> Option<String> {
    let parts: Vec<&str> = rel.split('/').collect();
    let index = parts
        .iter()
        .position(|part| part.eq_ignore_ascii_case(".git"))?;
    Some(fsutil::fold_rel(&parts[..index].join("/")))
}

/// 成员（或空目录）是否落在某个「直接含 .git 的目录」子树内：H-06 要求该目录及
/// 全部后代整树排除，含 .git 的兄弟条目——按路径组件比较，`projectx` 不会命中
/// `project` 的边界。
pub(super) fn inside_git_subtree(subtrees: &HashSet<String>, rel: &str) -> bool {
    if subtrees.is_empty() {
        return false;
    }
    let folded = fsutil::fold_rel(rel);
    folded
        .match_indices('/')
        .map(|(end, _)| end)
        .chain(std::iter::once(folded.len()))
        .any(|end| subtrees.contains(&folded[..end]))
}

/// Git 边界判定缓存（H-06）：同一目录在一次解压里被反复询问（同一包的兄弟成员、
/// 嵌套归档的父链），缓存把目录项检查从「成员数 × 深度」收敛到「不同目录数」。
/// 本工具从不创建 `.git` 条目，一次解压期间目录的 Git 状态不变，缓存安全。
#[derive(Default)]
pub(super) struct GitBoundaries {
    known: HashMap<PathBuf, bool>,
    #[cfg(windows)]
    system_checked: bool,
    #[cfg(windows)]
    system_prefix: Option<String>,
}

impl GitBoundaries {
    /// 目录是否位于 Git 树内：自该目录起、直到（不含）用户选定的根目录，任一级
    /// 直接含 `.git` 即为真。用户选定的根本身不参与判定——根即 Git 根属整次任务的
    /// 前置拒绝，不在解压层处理。
    pub(super) fn blocked(&mut self, root: &Path, directory: &Path) -> Result<bool> {
        // S-05：只解析一次实际系统目录；仅所选根包含它时逐目标检查序数路径前缀。
        #[cfg(windows)]
        {
            if !self.system_checked {
                let protected = fsutil::protected_root()?;
                let protected_key =
                    fsutil::fold_rel(&fsutil::path_string(&protected)?.replace('\\', "/"));
                let root_key = fsutil::fold_rel(&fsutil::path_string(root)?.replace('\\', "/"));
                let root_key = root_key.trim_end_matches('/');
                if protected_key == root_key || protected_key.starts_with(&format!("{root_key}/")) {
                    self.system_prefix = Some(format!("{protected_key}/"));
                }
                self.system_checked = true;
            }
            if let Some(prefix) = &self.system_prefix {
                let key = fsutil::fold_rel(&fsutil::path_string(directory)?.replace('\\', "/"));
                if key == prefix.trim_end_matches('/') || key.starts_with(prefix) {
                    return Ok(true);
                }
            }
        }
        if let Some(&cached) = self.known.get(directory) {
            return Ok(cached);
        }
        let mut chain: Vec<PathBuf> = Vec::new();
        let mut verdict: Option<bool> = None;
        let mut current = Some(directory.to_path_buf());
        while let Some(candidate) = current {
            if candidate.as_path() == root {
                break;
            }
            if let Some(&cached) = self.known.get(&candidate) {
                verdict = Some(cached);
                break;
            }
            current = candidate.parent().map(Path::to_path_buf);
            chain.push(candidate);
        }
        // 祖先缓存只证明祖先自身；仍须检查后代是否开启了新的 Git 边界。
        let mut blocked = verdict.unwrap_or(false);
        for candidate in chain.into_iter().rev() {
            if !blocked {
                blocked = fsutil::is_git_root(&candidate)?
                    || fsutil::is_extraction_workspace(&candidate)?;
            }
            self.known.insert(candidate, blocked);
        }
        Ok(blocked)
    }
}

/// 单包共享的目标父链：先只校验，成员获准后才创建。
/// false 表示已校验但尚不存在，true 表示已确认的普通目录（含本包新建目录）。
/// P-08 不复核外部变化；首次接触及创建后的链接/junction 检查仍保留。
#[derive(Default)]
pub(super) struct DestinationParents {
    known: HashMap<PathBuf, bool>,
    /// 包内全部目录的最终落点先占位，文件冲突分配不得抢占尚未创建的空目录。
    pub(super) reserved_directories: HashSet<String>,
}

impl DestinationParents {
    pub(super) fn validate(&mut self, root: &Path, directory: &Path) -> Result<()> {
        if directory == root || self.known.contains_key(directory) {
            return Ok(());
        }
        let relative = directory.strip_prefix(root).context("目标目录超出所选根")?;
        ensure!(
            relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
            "非法路径组件"
        );
        let mut chain = Vec::new();
        for current in directory.ancestors() {
            if current == root || self.known.contains_key(current) {
                break;
            }
            chain.push(current);
        }
        for current in chain.into_iter().rev() {
            let exists = match fs::symlink_metadata(current) {
                Ok(meta) => {
                    ensure!(
                        !fsutil::is_link(&meta),
                        "拒绝符号链接 / junction / reparse point：{}",
                        current.display()
                    );
                    ensure!(meta.is_dir(), "目标父链不是普通目录：{}", current.display());
                    true
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => {
                    return Err(error).with_context(|| format!("无法检查 {}", current.display()))
                }
            };
            self.known.insert(current.to_path_buf(), exists);
        }
        Ok(())
    }

    pub(super) fn ensure(&mut self, root: &Path, directory: &Path) -> Result<()> {
        self.validate(root, directory)?;
        let mut chain = Vec::new();
        for current in directory.ancestors() {
            if current == root || self.known.get(current) == Some(&true) {
                break;
            }
            chain.push(current);
        }
        for current in chain.into_iter().rev() {
            match fs::create_dir(current) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("无法创建目录 {}", current.display()))
                }
            }
            let meta = fs::symlink_metadata(current)?;
            ensure!(
                !fsutil::is_link(&meta),
                "拒绝符号链接 / junction / reparse point：{}",
                current.display()
            );
            ensure!(meta.is_dir(), "目标父链不是普通目录：{}", current.display());
            *self.known.get_mut(current).context("目标目录尚未校验")? = true;
        }
        Ok(())
    }
}

/// 成员排除判定与扫描剪枝口径对齐：扫描对目录 X 整树剪枝，因此裸目录名 `X`
/// 也必须排除其下成员 `X/y.txt`，否则成员落盘成为扫描不可见的影子，且原包每轮
/// 因目录 X 命中排除而强制保留、重复解压永不收敛。
pub(super) fn member_excluded(exclusions: &rules::Exclusions, rel: &str) -> bool {
    let bytes = rel.as_bytes();
    for i in 0..=bytes.len() {
        if i == bytes.len() || bytes[i] == b'/' {
            let prefix = &rel[..i];
            if exclusions.is_match(prefix) {
                return true;
            }
        }
    }
    false
}

/// 非 Windows 上点开头路径组件等价隐藏目录/文件（engine 扫描按组件剪枝）：
/// 未开启 include_hidden 时这样的解压目标不得落盘，否则会成为扫描不可见的影子内容。
#[cfg(not(windows))]
pub(super) fn has_hidden_component(rel: &str) -> bool {
    rel.split('/').any(|part| part.starts_with('.'))
}

/// 只判断根目录以下的层级。用户选定的根目录本身（例如位于隐藏的 AppData 之下）不参与隐藏/系统判定，
/// 否则整棵树都会被上级目录的属性判定为隐藏，所有解压结果都会被跳过；
/// 根即 Git 根属整次任务的前置拒绝，同理不在此判定。
/// H-06：目标（或其任一级祖先目录）直接含 `.git` 时整树排除——不解压、不写入。
pub(super) fn excluded_destination(
    root: &Path,
    path: &Path,
    config: &jchtools_file_core::config::Config,
    git: &mut GitBoundaries,
) -> Result<bool> {
    if git.blocked(root, path)? {
        return Ok(true);
    }
    // 隐藏/系统开关都开启时无需逐级判定（默认即如此，H-06 之外不再有排除）。
    if config.include_hidden && config.include_system {
        return Ok(false);
    }
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate == root {
            break;
        }
        if !config.include_hidden && is_hidden(candidate) {
            return Ok(true);
        }
        if !config.include_system && is_system(candidate) {
            return Ok(true);
        }
        current = candidate.parent();
    }
    Ok(false)
}

#[cfg(windows)]
fn is_hidden(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_attributes() & 2 != 0)
}
#[cfg(not(windows))]
fn is_hidden(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}
#[cfg(windows)]
fn is_system(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_attributes() & 4 != 0)
}
#[cfg(not(windows))]
fn is_system(_path: &Path) -> bool {
    false
}
