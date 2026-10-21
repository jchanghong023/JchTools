use super::boundaries::{excluded_destination, member_excluded, DestinationParents, GitBoundaries};
use super::names::DecodedFileName;
use super::observe::{observe_archive, stops_extraction};
use super::space::volume_scope_allowed;
use super::volumes::{numbered_entry, rar_part_stem, volume_set};
use anyhow::{bail, ensure, Context, Result};
use jchtools_file_core::{
    fsutil, rules,
    task::{Job, QUARANTINE_DIR_NAME},
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// X-04：目录落盘名规划（[`Staging`] 内容里的目录 → 实际落盘相对名的映射）。
/// 默认落盘名被普通文件、链接或 junction 占用时，为新目录选最小未占用序号
/// （`目录 (1)`、`目录 (2)`），该目录及全部后代成员整体映射到新目录；
/// 既有文件一律不动，与既有普通目录同名则合入。规划必须在成员合入前完成：
/// 文件成员的父链与空目录条目共用这一映射。
pub(super) fn plan_directory_renames(
    control: &jchtools_file_core::control::Control,
    stage_content: &Path,
    base: &Path,
    final_directories: &mut HashSet<String>,
) -> Result<HashMap<String, String>> {
    let mut dir_renames: HashMap<String, String> = HashMap::new();
    let mut reserved = HashSet::new();
    for entry in walkdir::WalkDir::new(stage_content)
        .follow_links(false)
        .min_depth(1)
    {
        control.checkpoint()?;
        let entry = entry?;
        if !entry.file_type().is_dir() {
            continue;
        }
        let rel = fsutil::relative_string(stage_content, entry.path())?;
        let (parent_rel, name) = match rel.rfind('/') {
            Some(index) => (&rel[..index], &rel[index + 1..]),
            None => ("", rel.as_str()),
        };
        // walkdir 保证父目录先于后代：父目录已改名时，后代在改后的父目录下规划。
        let parent_dest =
            mapped_entry_rel(&dir_renames, parent_rel).unwrap_or_else(|| parent_rel.to_string());
        let join = |name: &str| -> Result<PathBuf> {
            let rel = if parent_dest.is_empty() {
                name.to_string()
            } else {
                format!("{parent_dest}/{name}")
            };
            Ok(base.join(fsutil::safe_relative(&rel)?))
        };
        let destination = join(name)?;
        let occupancy = classify_occupancy(&destination)?;
        let protected_directory = matches!(occupancy, Occupancy::PlainDir)
            && (destination
                .file_name()
                .is_some_and(|name| name == QUARANTINE_DIR_NAME)
                || fsutil::is_git_root(&destination)?
                || fsutil::is_extraction_workspace(&destination)?);
        if !protected_directory && !matches!(occupancy, Occupancy::Blocked) {
            final_directories.insert(fsutil::fold_rel(&fsutil::path_string(&destination)?));
            // 目标空闲或普通目录可合入；Git、隔离容器与有所有权标记的工作区另选新目录名。
            continue;
        }
        let mut new_rel = None;
        for index in 1u64..=1_000_000 {
            let candidate = fsutil::suffixed_candidate(name, "", index);
            let candidate_rel = if parent_dest.is_empty() {
                candidate.clone()
            } else {
                format!("{parent_dest}/{candidate}")
            };
            // 正式位置尚未合入：同包原名与已规划的新名也占位，不能把不同目录混为一棵树。
            if !reserved.contains(&fsutil::fold_rel(&candidate_rel))
                && matches!(
                    classify_occupancy(&entry.path().with_file_name(&candidate))?,
                    Occupancy::Free
                )
                && matches!(classify_occupancy(&join(&candidate)?)?, Occupancy::Free)
            {
                new_rel = Some(candidate_rel);
                break;
            }
        }
        let Some(new_rel) = new_rel else {
            bail!(
                "无法为目录 {rel} 分配不冲突的落盘名（X-04：无法生成合法目标时该包不算完整成功）"
            );
        };
        reserved.insert(fsutil::fold_rel(&new_rel));
        final_directories.insert(fsutil::fold_rel(&fsutil::path_string(
            &base.join(fsutil::safe_relative(&new_rel)?),
        )?));
        dir_renames.insert(fsutil::fold_rel(&rel), new_rel);
    }
    Ok(dir_renames)
}

fn volume_family_key(name: &str) -> Option<String> {
    if let Some(key) = rules::archive_entry_key(name) {
        return Some(key);
    }
    let lower = name.to_ascii_lowercase();
    if let Some(stem) = rar_part_stem(&lower) {
        return rules::archive_entry_key(&format!("{stem}.part1.rar"));
    }
    if numbered_entry(&lower) {
        let (stem, _) = name.rsplit_once('.')?;
        return rules::archive_entry_key(&format!("{stem}.001"));
    }
    let tail = rules::old_style_tail(&lower)?;
    rules::archive_entry_key(&format!("{}.{}", tail.stem, tail.main_ext))
}

/// X-04/X-10：新解出的同族卷统一占位、统一改主体，不能与旧目录里的卷拼接。
pub(super) fn plan_volume_renames(
    job: &Job,
    content: &Path,
    base: &Path,
    directories: &HashMap<String, String>,
    decoded_names: &HashMap<String, DecodedFileName>,
    exclusions: &rules::Exclusions,
    git: &mut GitBoundaries,
) -> Result<HashMap<String, PathBuf>> {
    let mut families: BTreeMap<(PathBuf, String, usize), Vec<(String, PathBuf)>> = BTreeMap::new();
    let mut reserved = HashSet::new();
    for entry in walkdir::WalkDir::new(content)
        .follow_links(false)
        .min_depth(1)
    {
        job.context.control.checkpoint()?;
        let entry = entry?;
        let relative = fsutil::relative_string(content, entry.path())?;
        let decoded_name = decoded_names.get(&fsutil::fold_rel(&relative));
        let original = decoded_name.map_or(relative.as_str(), |name| name.original.as_str());
        let mapped = mapped_entry_rel(directories, original);
        let destination = base.join(fsutil::safe_relative(
            mapped.as_deref().unwrap_or(original),
        )?);
        reserved.insert(fsutil::fold_rel(&fsutil::path_string(&destination)?));
        if !entry.file_type().is_file() {
            continue;
        }
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .context("分卷成员名无效")?;
        let Some(key) = volume_family_key(name) else {
            continue;
        };
        let parent = destination.parent().context("分卷成员缺少父目录")?;
        let root_rel = fsutil::relative_string(&job.root, parent)?;
        if member_excluded(exclusions, &root_rel)
            || excluded_destination(&job.root, parent, &job.config, git)?
        {
            continue;
        }
        families
            .entry((
                parent.to_path_buf(),
                key,
                decoded_name.map_or(0, |name| name.occurrence),
            ))
            .or_default()
            .push((relative, destination));
    }
    let mut existing: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut renames = HashMap::new();
    for ((parent, key, _), members) in families {
        // 普通单文件包仍走 H-07；zip/rar 主包也必须避开既有老式尾卷族。
        if members.len() == 1 && key.starts_with("single:") {
            let name = members[0]
                .1
                .file_name()
                .and_then(|name| name.to_str())
                .context("归档目标名无效")?;
            let primary = name.rsplit_once('.').is_some_and(|(_, extension)| {
                extension.eq_ignore_ascii_case("zip") || extension.eq_ignore_ascii_case("rar")
            });
            if !primary && rules::old_style_tail(&name.to_ascii_lowercase()).is_none() {
                continue;
            }
        }
        if !existing.contains_key(&parent) {
            let mut keys = HashSet::new();
            match fs::read_dir(&parent) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry?;
                        if let Some(name) = entry.file_name().to_str() {
                            if let Some(key) = volume_family_key(name) {
                                keys.insert(key);
                            }
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            existing.insert(parent.clone(), keys);
        }
        if !existing
            .get(&parent)
            .is_some_and(|keys| keys.contains(&key))
        {
            // 同一归档中的重复卷族也不能拼接：先占住本组的原始主体。
            existing
                .get_mut(&parent)
                .context("缺少卷族占位")?
                .insert(key);
            continue;
        }
        let mut selected = None;
        let first_name = members[0]
            .1
            .file_name()
            .and_then(|name| name.to_str())
            .context("分卷目标名无效")?;
        let stem = fsutil::split_compound_name(first_name).0;
        let max_suffix_units = members.iter().try_fold(0usize, |max, (_, destination)| {
            let name = destination
                .file_name()
                .and_then(|name| name.to_str())
                .context("分卷目标名无效")?;
            Ok::<_, anyhow::Error>(
                max.max(fsutil::split_compound_name(name).1.encode_utf16().count()),
            )
        })?;
        for index in 1u64..=1_000_000 {
            let common_stem = family_candidate_stem(stem, max_suffix_units, index)?;
            let mut targets = Vec::with_capacity(members.len());
            let mut claimed = HashSet::new();
            for (_, destination) in &members {
                let name = destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("分卷目标名无效")?;
                let (_, suffix) = fsutil::split_compound_name(name);
                let candidate = format!("{common_stem}{suffix}");
                ensure!(candidate.ends_with(suffix), "分卷冲突目标无法保留完整后缀");
                fsutil::validate_component(&candidate)?;
                let target = parent.join(candidate);
                let target_key = fsutil::fold_rel(&fsutil::path_string(&target)?);
                if reserved.contains(&target_key)
                    || !claimed.insert(target_key)
                    || !matches!(classify_occupancy(&target)?, Occupancy::Free)
                {
                    targets.clear();
                    break;
                }
                targets.push(target);
            }
            if targets.len() == members.len() {
                selected = Some(targets);
                break;
            }
        }
        let targets = selected.context("无法为完整新分卷族分配统一合法主体")?;
        let target_family = targets
            .first()
            .and_then(|target| target.file_name())
            .and_then(|name| name.to_str())
            .and_then(volume_family_key)
            .context("分卷新主体无效")?;
        existing
            .get_mut(&parent)
            .context("缺少卷族占位")?
            .insert(target_family);
        for ((relative, _), target) in members.into_iter().zip(targets) {
            reserved.insert(fsutil::fold_rel(&fsutil::path_string(&target)?));
            renames.insert(relative, target);
        }
    }
    Ok(renames)
}

/// 目标位置的占用形态（X-04）：空闲、可合入的普通目录、或被文件/链接等占用。
pub(super) enum Occupancy {
    Free,
    /// 普通目录（非链接/junction）：同名新目录按 X-04 合入。
    PlainDir,
    /// 普通文件、悬空链接或重解析点：名字被占，新目录须另选未占用名。
    Blocked,
}
pub(super) fn classify_occupancy(path: &Path) -> Result<Occupancy> {
    match fs::symlink_metadata(path) {
        // symlink_metadata 不跟随链接：链接/junction 一律按占用处理（X-04：不得
        // 借合入写入受保护的重解析点，需要该位置时另选未占用名）。
        Ok(meta) if meta.is_dir() && !fsutil::is_link(&meta) => Ok(Occupancy::PlainDir),
        Ok(_) => Ok(Occupancy::Blocked),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Occupancy::Free),
        Err(error) => {
            Err(anyhow::Error::new(error).context(format!("无法检查 {}", path.display())))
        }
    }
}

/// 条目落盘路径的 X-04 目录映射：条目自身被改名（目录条目）或其最长祖先目录被
/// 改名（任意成员）时，返回映射后的相对路径；无需映射时返回 None。
pub(super) fn mapped_entry_rel(renames: &HashMap<String, String>, rel: &str) -> Option<String> {
    if let Some(mapped) = renames.get(&fsutil::fold_rel(rel)) {
        return Some(mapped.clone());
    }
    // 分隔符位置取自原字符串，不能用折叠键（非 BMP 编码长度可能不同）切片。
    for (index, _) in rel.rmatch_indices('/') {
        if let Some(mapped) = renames.get(&fsutil::fold_rel(&rel[..index])) {
            return Some(format!("{mapped}{}", &rel[index..]));
        }
    }
    None
}

/// 所有卷按最长原后缀留出相同预算，整个族只生成一次主体，绝不截断编号或后缀。
fn family_candidate_stem(stem: &str, max_suffix_units: usize, index: u64) -> Result<String> {
    let separator_units = index.ilog10() as usize + 4;
    let budget = 255usize
        .checked_sub(separator_units)
        .and_then(|budget| budget.checked_sub(max_suffix_units))
        .filter(|budget| *budget > 0)
        .context("无法为整族分卷主体保留合法字符")?;
    let mut end = 0;
    let mut units = 0;
    for (offset, ch) in stem.char_indices() {
        let width = ch.len_utf16();
        if units + width > budget {
            break;
        }
        end = offset + ch.len_utf8();
        units += width;
    }
    let prefix = stem[..end].trim_end_matches(|ch: char| ch == '.' || ch.is_whitespace());
    ensure!(!prefix.is_empty(), "无法为整族分卷主体保留合法字符");
    let mut candidate = String::with_capacity(prefix.len() + separator_units);
    candidate.push_str(prefix);
    std::fmt::Write::write_fmt(&mut candidate, format_args!(" ({index})"))?;
    Ok(candidate)
}

/// X-06/X-10：隔离整组一次规划目标名。先试原名；任一目标被占用时，选最小正整数
/// N 使整组以「主干 (N)原后缀」统一改名后全部未占用——后缀、编号及补零原样保留
/// （命名歧义组各卷后缀不同，也按各自原后缀保持，不改造成看似完整的卷集）。
/// 组内候选重名视同占用，整组换下一序号。返回 None 表示找不到整组可用的序号。
pub(super) fn quarantine_targets(
    dir: &Path,
    names: &[(String, String)],
) -> Result<Option<Vec<PathBuf>>> {
    if names.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let max_suffix_units = names
        .iter()
        .map(|(_, ext)| ext.encode_utf16().count())
        .max()
        .unwrap_or(0);
    let build = |index: Option<u64>| -> Result<Vec<PathBuf>> {
        let common_stem = index
            .map(|index| family_candidate_stem(&names[0].0, max_suffix_units, index))
            .transpose()?;
        let mut targets = Vec::with_capacity(names.len());
        let mut claimed = HashSet::new();
        for (stem, ext) in names {
            let name = match &common_stem {
                None => format!("{stem}{ext}"),
                Some(stem) => format!("{stem}{ext}"),
            };
            ensure!(name.ends_with(ext), "隔离目标无法完整保留分卷后缀：{ext}");
            fsutil::validate_component(&name)?;
            if !claimed.insert(name.clone()) {
                return Ok(Vec::new());
            }
            let target = dir.join(&name);
            if !matches!(classify_occupancy(&target)?, Occupancy::Free) {
                return Ok(Vec::new());
            }
            targets.push(target);
        }
        Ok(targets)
    };
    let original = build(None)?;
    if !original.is_empty() {
        return Ok(Some(original));
    }
    for index in 1u64..=1_000_000 {
        let candidates = build(Some(index))?;
        if !candidates.is_empty() {
            return Ok(Some(candidates));
        }
    }
    Ok(None)
}

/// 失败处置（X-06）：把未能完全解开的原包（连同兄弟卷）移入所选目录根下的
/// 「解压失败」子目录。移动用不覆盖改名；目标名按 X-10 对整组一次规划。
/// 失败原因是界面可查的日志字段。移动不走删除接口——隔离不是删除，原包保持可用
/// 等待人工处理。
pub(super) fn quarantine(job: &mut Job, archive_rel: &str, reason: &str) -> Result<()> {
    let control = job.context.control.clone();
    observe_archive("quarantine", &control, |boundary_stage| {
        *boundary_stage = "quarantine_move";
        job.context.control.checkpoint()?;
        let archive = fsutil::safe_join(&job.root, archive_rel)?;
        // 隔离使用 X-10 精确命名族的在场卷；失败包未必能由引擎确认卷数，
        // 因此不把成功删源所需的档案头佐证作为可逆隔离的前置条件。
        let sources = volume_set(&archive)?.paths;
        ensure!(
            volume_scope_allowed(job, archive_rel, &sources)?,
            "卷组含范围外源项，整组原位保留"
        );
        let mut moved = Vec::new();
        let result: Result<()> = (|| {
            let dir = job.root.join(QUARANTINE_DIR_NAME);
            if !dir.try_exists()? {
                fs::create_dir_all(&dir)?;
            }
            // S-04：容器位置是 junction/符号链接时不得穿透——否则失败包会被移出所选根、
            // 落到链接目标，且界面上声称的位置与实际不符。链接与普通文件占用都按
            // 「容器被占用」走隔离失败原地保留路径，但文案区分占用类型（X-06/U-10）。
            // H-06：容器自身是 Git 项目（.git 目录或文件）时整树保护优先——不得把失败包
            // 移入 Git 树，也不得在树内腾挪或覆盖；同样按「隔离失败」原地保留。
            let container_blocked = match fs::symlink_metadata(&dir) {
                Ok(meta) if fsutil::is_link(&meta) => {
                    Some("目标位置被链接占用，不穿透链接隔离（S-04）".to_string())
                }
                Ok(meta) if meta.is_dir() => match fsutil::is_git_root(&dir) {
                    Ok(true) => Some(
                        "目标目录是 Git 项目（含 .git），按 H-06 整树保护不把失败包移入其中"
                            .to_string(),
                    ),
                    Ok(false) => None,
                    Err(error) => {
                        anyhow::bail!("无法检查「{QUARANTINE_DIR_NAME}」的 Git 边界（{error:#}）")
                    }
                },
                _ => Some("目标位置被同名文件占用".to_string()),
            };
            if let Some(reason) = container_blocked {
                anyhow::bail!(
                    "无法建立「{QUARANTINE_DIR_NAME}」子目录：{}（{reason}）",
                    dir.display()
                );
            }
            // 只规划实际仍存在的源项（缺失卷不阻止整组隔离，与既有语义一致）。
            let mut present: Vec<PathBuf> = Vec::new();
            let mut names: Vec<(String, String)> = Vec::new();
            for source in &sources {
                if !source.try_exists()? {
                    continue;
                }
                let name = source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("无效压缩包名称")?
                    .to_string();
                let (stem, ext) = fsutil::split_compound_name(&name);
                names.push((stem.to_string(), ext.to_string()));
                present.push(source.clone());
            }
            let Some(targets) = quarantine_targets(&dir, &names)? else {
                bail!(
                    "无法为隔离卷集分配整组不冲突的目标名：{}",
                    archive.display()
                );
            };
            for (source, target) in present.into_iter().zip(targets) {
                // 移动仍逐卷进行：某卷移动失败时保留未移动源项并如实上抛（X-06），
                // 不宣称整包隔离成功。
                job.context.control.checkpoint()?;
                let source_rel = fsutil::relative_string(&job.root, &source)?;
                let size = fsutil::snapshot(&source).map_or(0, |s| s.size);
                fsutil::rename_noreplace(&source, &target)?;
                moved.push((source.clone(), target.clone()));
                let target_rel = fsutil::relative_string(&job.root, &target)?;
                job.summary.archives_quarantined += 1;
                // 移走后该路径的扫描记录不再代表磁盘现状，标为 inactive（任务库记录与磁盘保持一致）。
                job.db
                    .conn
                    .execute("UPDATE files SET active=0 WHERE rel=?1", [&source_rel])?;
                job.log(
                    "解压",
                    &source_rel,
                    &target_rel,
                    "移入解压失败",
                    reason,
                    size,
                )?;
            }
            Ok(())
        })();
        result.with_context(|| {
            let mut locations = Vec::with_capacity(sources.len());
            for source in &sources {
                if let Some((_, target)) = moved.iter().find(|(from, _)| from == source) {
                    locations.push(format!(
                        "已移动：{} → {}",
                        source.display(),
                        target.display()
                    ));
                } else {
                    locations.push(format!("未移动源项：{}", source.display()));
                }
            }
            format!("卷集隔离未完成；{}", locations.join("；"))
        })
    })
}

/// 单个条目合入结果：冲突（目标已存在）不淘汰、不询问，只给新成员改名。
pub(super) enum MergeOutcome {
    /// 目标位置空闲：成员按原相对路径落位。
    Placed(PathBuf),
    /// 目标已被既有文件/目录/链接占用：既有内容原样不动，新成员改用未占用名
    /// （X-04/H-07）。TOCTOU 竞争后改用唯一名的情况也走这一支：无论哪种冲突，
    /// 结果都是「新成员以另一个名字落盘」。
    Renamed(PathBuf),
}
/// 把暂存成员合入正式位置（X-08：整包解码校验完成后才合入）。
/// 既有目标一律不动：目标存在就改用唯一名（保留扩展名，含复合扩展名），
/// 内容相同也照常落盘；不比较内容、不淘汰、不询问、不删除。
pub(super) fn merge_extracted(
    root: &Path,
    source: &Path,
    target: &Path,
    parents: &mut DestinationParents,
) -> Result<MergeOutcome> {
    // 过滤获准后才创建父链，最终名仍按成员级冲突改名，绝不覆盖。
    parents.ensure(root, target.parent().context("目标缺少父目录")?)?;
    let reserved = parents
        .reserved_directories
        .contains(&fsutil::fold_rel(&fsutil::path_string(target)?));
    if !reserved && matches!(classify_occupancy(target)?, Occupancy::Free) {
        match fsutil::rename_noreplace(source, target) {
            Ok(()) => return Ok(MergeOutcome::Placed(target.to_path_buf())),
            Err(error) => {
                // TOCTOU：目标在 try_exists 与 rename 之间出现。与既有冲突同一处置：
                // 改用唯一名落盘，而不是整包失败（P-08 不要求防御外部改动，但这里
                // 本来就有不覆盖的安全落点）。
                if stops_extraction(&error) {
                    return Err(error);
                }
                let emergency = member_unique_target(root, target, &parents.reserved_directories)?;
                fsutil::rename_noreplace(source, &emergency)?;
                return Ok(MergeOutcome::Renamed(emergency));
            }
        }
    }
    // 目标已存在（普通文件、链接、坏链或目录）：绝不覆盖、不淘汰，直接改名落盘。
    let renamed = member_unique_target(root, target, &parents.reserved_directories)?;
    #[cfg(not(test))]
    let publish = fsutil::rename_noreplace(source, &renamed);
    // 测试只替换本次独占成员的发布 I/O 结果；解码、目标规划和错误上下文仍走真实路径。
    #[cfg(test)]
    let publish = super::tests::review_f11_publish_fault(source, &renamed)
        .map_or_else(|| fsutil::rename_noreplace(source, &renamed), Err);
    publish.with_context(|| {
        format!(
            "目标已存在且改名落盘失败：{} → {}",
            target.display(),
            renamed.display()
        )
    })?;
    Ok(MergeOutcome::Renamed(renamed))
}

/// 与 H-07 唯一名规则相同，同时避开本包尚未发布的目录。
pub(super) fn member_unique_target(
    root: &Path,
    target: &Path,
    reserved: &HashSet<String>,
) -> Result<PathBuf> {
    if reserved.is_empty() {
        return fsutil::unique_target(root, target);
    }
    let parent = target.parent().context("目标没有父目录")?;
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .context("无效文件名")?;
    let (stem, suffix) = fsutil::split_compound_name(name);
    let suffix_units = suffix.encode_utf16().count();
    for index in 1u64..=1_000_000 {
        ensure!(
            suffix_units + index.ilog10() as usize + 5 <= 255,
            "原扩展名过长，无法保留扩展名并追加冲突序号"
        );
        let candidate = parent.join(fsutil::suffixed_candidate(stem, suffix, index));
        fsutil::safe_relative(&fsutil::relative_string(root, &candidate)?)?;
        if !reserved.contains(&fsutil::fold_rel(&fsutil::path_string(&candidate)?))
            && matches!(classify_occupancy(&candidate)?, Occupancy::Free)
        {
            return Ok(candidate);
        }
    }
    bail!("无法为 {} 分配避开包内目录的不冲突名称", target.display())
}
