use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use jchtools_file_core::fsutil;

use super::formats::MEDIA;
use super::media::media_dir_name_reserved;
use super::naming::{
    compare_paths, markdown_name, occupancy_key, path_begins_with, path_equal, OccupiedIndex,
};
use super::run::{Options, Summary};

#[derive(Clone, Debug)]
pub(super) struct Item {
    pub(super) source: PathBuf,
    pub(super) relative: PathBuf,
    pub(super) target: PathBuf,
    pub(super) is_media: bool,
    pub(super) media_dir: String,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Plan {
    pub(super) output_root: PathBuf,
    pub(super) items: Vec<Item>,
    pub(super) summary: Summary,
    /// S4-03/T-24：扫描期的单文件失败（相对路径 + 带阶段的原因），由 `run`
    /// 在批次开始后逐条呈现并计入统计；目录级全局错误（如 `read_dir` 失败）
    /// 仍直接中止整次扫描，不进入此列表。
    pub(super) scan_failures: Vec<(PathBuf, String)>,
}

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
pub(super) fn scan(options: &Options, supported: &BTreeSet<String>) -> Result<Plan, String> {
    let cancel = AtomicBool::new(false);
    scan_cancelable(options, supported, &cancel)
}

pub(super) fn scan_cancelable(
    options: &Options,
    supported: &BTreeSet<String>,
    cancel: &AtomicBool,
) -> Result<Plan, String> {
    if cancel.load(AtomicOrdering::Acquire) {
        return Err("扫描已取消".to_string());
    }
    let input = checked_directory(&options.input_dir)?;
    let output = checked_directory(&options.output_dir)?;
    if path_equal(&input, &output) {
        return Err("输入与输出目录不能相同".to_string());
    }
    let mut paths = Vec::new();
    // T-09 只授权「输出位于输入目录子树内」时排除整棵输出子树；输出是输入的
    // 祖先或两者在不同分支时不排除任何目录——否则输入子树会因前缀判定被整体
    // 静默跳过，违反 T-07 的递归处理要求。
    let output_in_input = path_begins_with(&output, &input);
    let mut pending = vec![input.clone()];
    while let Some(dir) = pending.pop() {
        if cancel.load(AtomicOrdering::Acquire) {
            return Err("扫描已取消".to_string());
        }
        for entry in fs::read_dir(&dir).map_err(|e| format!("无法扫描 {}：{e}", dir.display()))?
        {
            if cancel.load(AtomicOrdering::Acquire) {
                return Err("扫描已取消".to_string());
            }
            let entry = entry.map_err(|e| format!("无法读取 {} 的目录项：{e}", dir.display()))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("无法检查 {}：{e}", path.display()))?;
            if fsutil::is_link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                if !(output_in_input && path_begins_with(&path, &output)) {
                    pending.push(path);
                }
            } else if metadata.is_file()
                && path
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|ext| supported.contains(&ext.to_ascii_lowercase()))
            {
                paths.push(path);
            }
        }
    }
    paths.sort_by(|a, b| {
        compare_paths(
            a.strip_prefix(&input).unwrap_or(a),
            b.strip_prefix(&input).unwrap_or(b),
        )
    });
    let mut occupied = OccupiedIndex::default();
    let mut media_occupied: BTreeMap<PathBuf, OccupiedIndex> = BTreeMap::new();
    let mut checked_output_parents = BTreeSet::from([output.clone()]);
    let mut plan = Plan {
        output_root: output.clone(),
        ..Plan::default()
    };
    for source in paths {
        let relative = source
            .strip_prefix(&input)
            .map_err(|e| format!("输入路径超出根目录：{e}"))?
            .to_path_buf();
        let file_name = markdown_name(&source)?;
        // 冲突决胜与平铺一致：排序靠前者保留，后续计 skipped_duplicate（T-24：
        // 与转换失败可区分）。
        let occupancy = occupancy_key(
            options.flat,
            relative.parent().unwrap_or_else(|| Path::new("")),
            &file_name,
        );
        if occupied.contains(occupancy.as_os_str()) {
            plan.summary.skipped_duplicate += 1;
            continue;
        }
        occupied.insert(occupancy);
        let target = if options.flat {
            output.join(file_name)
        } else {
            output
                .join(relative.parent().unwrap_or_else(|| Path::new("")))
                .join(file_name)
        };
        let parent = target.parent().unwrap_or(&output);
        // T-09/S-04：先逐段检查已有输出父目录，之后才可查询结果或媒体占用。
        // 缓存本批已检查的父目录；P-08 不要求应对处理期间的外部替换。
        if !checked_output_parents.contains(parent) {
            if let Err(message) = check_output_parent(&output, parent) {
                plan.summary.failed += 1;
                plan.scan_failures
                    .push((relative.clone(), format!("检查输出目录失败：{message}")));
                continue;
            }
            checked_output_parents.insert(parent.to_path_buf());
        }
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                plan.summary.skipped_existing += 1;
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                plan.summary.failed += 1;
                plan.scan_failures.push((
                    relative.clone(),
                    format!("检查已有结果失败 {}：{error}", target.display()),
                ));
                continue;
            }
        }
        let is_media = source
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| MEDIA.contains(&ext.to_ascii_lowercase().as_str()));
        let media_dir = if is_media {
            String::new()
        } else {
            let occupied_media = media_occupied.entry(parent.to_path_buf()).or_default();
            // S4-03/T-24：媒体目录分配失败是单文件问题（该文件的输出位置被
            // 链接占用或无法检查）——计入失败统计并继续其余文件；目录级全局
            // 错误（遍历期的 read_dir 等）不经过这里，仍保持整批中止。
            match media_dir_name_reserved(parent, &target, occupied_media) {
                Ok(name) => name,
                Err(message) => {
                    plan.summary.failed += 1;
                    plan.scan_failures
                        .push((relative.clone(), format!("分配媒体目录失败：{message}")));
                    continue;
                }
            }
        };
        plan.items.push(Item {
            source,
            relative,
            target,
            is_media,
            media_dir,
        });
    }
    Ok(plan)
}

fn checked_directory(path: &Path) -> Result<PathBuf, String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => current.push(component.as_os_str()),
            Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|e| format!("无法检查目录 {}：{e}", current.display()))?;
                if fsutil::is_link(&metadata) {
                    return Err(format!(
                        "目录路径含符号链接或 junction：{}",
                        current.display()
                    ));
                }
            }
            Component::CurDir => (),
            Component::ParentDir => return Err("请选择不含上级跳转的目录路径".to_string()),
        }
    }
    let canonical =
        fs::canonicalize(path).map_err(|e| format!("无法访问目录 {}：{e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("不是目录：{}", path.display()));
    }
    Ok(canonical)
}

/// 检查输出根以内已有的父目录，不穿过链接查询其后代，也不为扫描创建目录。
fn check_output_parent(output_root: &Path, parent: &Path) -> Result<(), String> {
    let relative = parent
        .strip_prefix(output_root)
        .map_err(|error| format!("结果不在输出目录内：{error}"))?;
    let mut current = output_root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err("输出路径含非法目录段".to_string());
        }
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if fsutil::is_link(&metadata) => {
                return Err(format!(
                    "输出目录路径含符号链接或 junction：{}",
                    current.display()
                ));
            }
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(format!("输出路径不是普通目录：{}", current.display())),
            // 后续目录尚不存在；本批只会自行创建普通目录（P-08）。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!("无法检查输出目录 {}：{error}", current.display()));
            }
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn test_scan_media_dirs(options: &Options) -> Result<Vec<String>, String> {
    let supported: BTreeSet<String> = [
        "docx", "docm", "dotx", "dotm", "pptx", "pptm", "ppsx", "potx", "potm", "xlsx", "xlsm",
        "xlsb", "xltx", "xltm", "xlam", "odt", "ods", "odp",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let cancel = AtomicBool::new(false);
    Ok(scan_cancelable(options, &supported, &cancel)?
        .items
        .into_iter()
        .map(|item| item.media_dir)
        .collect())
}

#[doc(hidden)]
pub fn test_scan_count_with_cancel(
    options: &Options,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    let supported: BTreeSet<String> = ["pdf", "docx"].into_iter().map(str::to_string).collect();
    Ok(scan_cancelable(options, &supported, cancel)?.items.len())
}
