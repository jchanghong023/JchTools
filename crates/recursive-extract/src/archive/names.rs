use anyhow::{ensure, Context, Result};
use jchtools_file_core::fsutil;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 条目清单结果：声明总大小、大小元数据是否完整、以及 H-06 的 Git 排除子树。
/// `git_subtrees` 存经 Windows 序数折叠的归档相对目录前缀：该目录「直接含有 .git」，其自身及
/// 全部后代（含 .git 的兄弟条目）在合入阶段整树跳过；空串表示归档根本身直接含
/// .git，整个暂存根都不解出。
pub(super) struct Listing {
    pub(super) total: u64,
    pub(super) sizes_complete: bool,
    pub(super) git_subtrees: HashSet<String>,
}

/// 只保存引擎暂存名与归档原名的差异，不把 `_N` 泄漏成正式落盘契约。
pub(super) struct DecodedFileName {
    pub(super) staged: String,
    pub(super) original: String,
    pub(super) occurrence: usize,
    pub(super) order: usize,
}

#[derive(Default)]
pub(super) struct DecoderNames {
    occupied: HashMap<String, usize>,
    pub(super) renamed: HashMap<String, DecodedFileName>,
    next_order: usize,
}

impl DecoderNames {
    pub(super) fn record(&mut self, raw: &str) -> Result<()> {
        // 7-Zip 对分段写入在成员名后附加位置；这些段共用输出，不自动改名。
        let (raw, split) = raw
            .rsplit_once(" <")
            .filter(|(_, position)| {
                position.strip_suffix('>').is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                })
            })
            .map_or((raw, false), |(name, _)| (name, true));
        let directory = raw.ends_with(['/', '\\']);
        let raw = raw.trim_end_matches(['/', '\\']);
        if raw == "." || raw.is_empty() {
            return Ok(());
        }
        let relative = fsutil::safe_relative(raw)?;
        let mut original = String::with_capacity(raw.len());
        for component in relative.components() {
            if !original.is_empty() {
                original.push('/');
            }
            original.push_str(component.as_os_str().to_str().context("解码成员名无效")?);
        }
        let key = fsutil::fold_rel(&original);
        for (index, _) in key.match_indices('/') {
            if !self.occupied.contains_key(&key[..index]) {
                self.occupied.insert(key[..index].to_string(), 0);
            }
        }
        if directory || split {
            self.occupied.entry(key).or_insert(usize::from(split));
            return Ok(());
        }
        let order = self.next_order;
        self.next_order += 1;
        let Some(count) = self.occupied.get_mut(&key) else {
            self.occupied.insert(key, 1);
            return Ok(());
        };
        let occurrence = *count;
        *count += 1;
        // 匹配随包引擎 FilePathAutoRename.cpp 的临时路径选择；正式目标仍走 H-07。
        let name_start = original.rfind('/').map_or(0, |index| index + 1);
        let dot = original.rfind('.').filter(|index| *index > name_start);
        let (stem, suffix) = dot.map_or((original.as_str(), ""), |index| original.split_at(index));
        let mut left = 1u32;
        let mut right = 1u32 << 30;
        while left != right {
            let mid = u32::midpoint(left, right);
            let candidate = format!("{stem}_{mid}{suffix}");
            if self.occupied.contains_key(&fsutil::fold_rel(&candidate)) {
                left = mid + 1;
            } else {
                right = mid;
            }
        }
        let staged = format!("{stem}_{right}{suffix}");
        let staged_key = fsutil::fold_rel(&staged);
        ensure!(
            !self.occupied.contains_key(&staged_key),
            "解码成员临时名已占用"
        );
        self.occupied.insert(staged_key.clone(), 0);
        self.renamed.insert(
            staged_key,
            DecodedFileName {
                staged,
                original,
                occurrence,
                order,
            },
        );
        Ok(())
    }
}

/// 原名成员先合入；引擎改名成员按实际解码顺序随后合入，不重复扫描暂存树。
pub(super) fn staged_files<'a>(
    content: &'a Path,
    names: &'a HashMap<String, DecodedFileName>,
    control: &'a jchtools_file_core::control::Control,
) -> impl Iterator<Item = Result<(walkdir::DirEntry, String)>> + 'a {
    let mut walk = Some(
        walkdir::WalkDir::new(content)
            .follow_links(false)
            .min_depth(1)
            .into_iter(),
    );
    let mut pending = Vec::new();
    let mut deferred: Option<std::vec::IntoIter<_>> = None;
    std::iter::from_fn(move || loop {
        if let Some(walk) = &mut walk {
            if let Some(entry) = walk.next() {
                if let Err(error) = control.checkpoint() {
                    return Some(Err(error));
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return Some(Err(error.into())),
                };
                if entry.file_type().is_dir() {
                    continue;
                }
                let relative = match fsutil::relative_string(content, entry.path()) {
                    Ok(relative) => relative,
                    Err(error) => return Some(Err(error)),
                };
                if !names.is_empty() {
                    if let Some(name) = names.get(&fsutil::fold_rel(&relative)) {
                        pending.push((name.order, entry, relative));
                        continue;
                    }
                }
                return Some(Ok((entry, relative)));
            }
        } else {
            let (_, entry, relative) = deferred.as_mut()?.next()?;
            return Some(control.checkpoint().map(|()| (entry, relative)));
        }
        walk = None;
        pending.sort_unstable_by_key(|(order, _, _)| *order);
        deferred = Some(std::mem::take(&mut pending).into_iter());
    })
}
