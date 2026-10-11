use crate::{config::DeleteMode, engine::Job, fsutil, model::bytes, process, rules};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::Ordering,
};

/// 「解压失败」子目录名（X-06）：建在所选目录根下，收纳未能完全解开的原包。
/// 扫描（两工具）与重跑计数默认排除它（X-07 / C-09）。
pub const QUARANTINE_DIR_NAME: &str = "解压失败";
fn observe_archive<T>(
    operation: &'static str,
    control: &crate::control::Control,
    run: impl FnOnce(&mut &'static str) -> Result<T>,
) -> Result<T> {
    let span = crate::logging::operation_span("archive", operation);
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let mut stage = "archive_boundary";
    tracing::info!(
        event = "archive_operation_started",
        operation,
        "归档操作开始"
    );
    let result = run(&mut stage);
    let elapsed_ms = crate::logging::elapsed_ms(started);
    match &result {
        Ok(_) => tracing::info!(
            event = "archive_operation_completed",
            operation,
            stage,
            elapsed_ms,
            "归档操作完成"
        ),
        Err(_) if control.is_cancelled() => tracing::info!(
            event = "archive_operation_cancelled",
            operation,
            stage,
            elapsed_ms,
            "归档操作已取消"
        ),
        Err(error) => {
            let io = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>());
            tracing::error!(
                event = "archive_operation_failed",
                operation,
                stage,
                elapsed_ms,
                error_type = if io.is_some() {
                    "io"
                } else if stops_extraction(error) {
                    "extraction_stop"
                } else {
                    "archive_boundary"
                },
                error_code = io.and_then(std::io::Error::raw_os_error),
                "归档操作失败"
            );
        }
    }
    result
}
fn member_rejected(code: &'static str, member: &str) {
    tracing::error!(event = "archive_member_rejected", stage = "member_validation",
        error_type = "archive_validation", error_code = code,
        member = %crate::logging::safe_error(member), "归档成员安全校验拒绝");
}

/// X-04：目录落盘名规划（[`Staging`] 内容里的目录 → 实际落盘相对名的映射）。
/// 默认落盘名被普通文件、链接或 junction 占用时，为新目录选最小未占用序号
/// （`目录 (1)`、`目录 (2)`），该目录及全部后代成员整体映射到新目录；
/// 既有文件一律不动，与既有普通目录同名则合入。规划必须在成员合入前完成：
/// 文件成员的父链与空目录条目共用这一映射。
fn plan_directory_renames(
    control: &crate::control::Control,
    stage_content: &Path,
    base: &Path,
) -> Result<HashMap<String, String>> {
    let mut dir_renames: HashMap<String, String> = HashMap::new();
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
                || fsutil::is_git_root(&destination)?);
        if !protected_directory && !matches!(occupancy, Occupancy::Blocked) {
            // 目标空闲或普通目录可合入；Git 项目和既有隔离容器另选新目录名。
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
            if matches!(classify_occupancy(&join(&candidate)?)?, Occupancy::Free) {
                new_rel = Some(candidate_rel);
                break;
            }
        }
        let Some(new_rel) = new_rel else {
            bail!(
                "无法为目录 {rel} 分配不冲突的落盘名（X-04：无法生成合法目标时该包不算完整成功）"
            );
        };
        dir_renames.insert(rel, new_rel);
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
        return rules::archive_entry_key(&format!("{}.001", &name[..name.len() - 4]));
    }
    let tail = rules::old_style_tail(&lower)?;
    rules::archive_entry_key(&format!("{}.{}", tail.stem, tail.main_ext))
}

/// X-04/X-10：新解出的同族卷统一占位、统一改主体，不能与旧目录里的卷拼接。
fn plan_volume_renames(
    job: &Job,
    content: &Path,
    base: &Path,
    directories: &HashMap<String, String>,
    exclusions: &rules::Exclusions,
    git: &mut GitBoundaries,
) -> Result<HashMap<String, PathBuf>> {
    let mut families: BTreeMap<(PathBuf, String), Vec<(String, PathBuf)>> = BTreeMap::new();
    let mut reserved = HashSet::new();
    for entry in walkdir::WalkDir::new(content)
        .follow_links(false)
        .min_depth(1)
    {
        job.context.control.checkpoint()?;
        let entry = entry?;
        let relative = fsutil::relative_string(content, entry.path())?;
        let mapped = mapped_entry_rel(directories, &relative);
        let destination = base.join(fsutil::safe_relative(
            mapped.as_deref().unwrap_or(&relative),
        )?);
        reserved.insert(fsutil::fold_rel(&fsutil::path_string(&destination)?));
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_str().context("分卷成员名无效")?;
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
            .entry((parent.to_path_buf(), key))
            .or_default()
            .push((relative, destination));
    }
    let mut existing: HashMap<PathBuf, HashSet<String>> = HashMap::new();
    let mut renames = HashMap::new();
    for ((parent, key), members) in families {
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
        for ((relative, _), target) in members.into_iter().zip(targets) {
            reserved.insert(fsutil::fold_rel(&fsutil::path_string(&target)?));
            renames.insert(relative, target);
        }
    }
    Ok(renames)
}

/// 条目清单结果：声明总大小、大小元数据是否完整、以及 H-06 的 Git 排除子树。
/// `git_subtrees` 存经 Windows 序数折叠的归档相对目录前缀：该目录「直接含有 .git」，其自身及
/// 全部后代（含 .git 的兄弟条目）在合入阶段整树跳过；空串表示归档根本身直接含
/// .git，整个暂存根都不解出。
struct Listing {
    total: u64,
    sizes_complete: bool,
    git_subtrees: HashSet<String>,
}

/// 成员路径里出现 `.git` 组件时，返回「直接含该 .git 的那个目录」的序数折叠相对路径
/// （空串 = 归档根）。H-06 的边界按目录项识别，不深入 Git 树内部。
fn git_boundary_prefix(rel: &str) -> Option<String> {
    let parts: Vec<&str> = rel.split('/').collect();
    let index = parts
        .iter()
        .position(|part| part.eq_ignore_ascii_case(".git"))?;
    Some(fsutil::fold_rel(&parts[..index].join("/")))
}

/// 成员（或空目录）是否落在某个「直接含 .git 的目录」子树内：H-06 要求该目录及
/// 全部后代整树排除，含 .git 的兄弟条目——按路径组件比较，`projectx` 不会命中
/// `project` 的边界。
fn inside_git_subtree(subtrees: &HashSet<String>, rel: &str) -> bool {
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
struct GitBoundaries {
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
    fn blocked(&mut self, root: &Path, directory: &Path) -> Result<bool> {
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
                blocked = fsutil::is_git_root(&candidate)?;
            }
            self.known.insert(candidate, blocked);
        }
        Ok(blocked)
    }
}

/// 分卷组实际体积合计（X-08 的展开比例分母）：主体与每个随组分卷各按实际文件大小
/// 相加。单卷大小会随分卷数缩小分母，把健康的分卷组误判为「展开比例超限」。
/// 某个卷读取失败即上抛：分母不能凭空变小。
fn volume_bytes(volumes: &[PathBuf]) -> Result<u64> {
    let mut total = 0u64;
    for path in volumes {
        total = total.saturating_add(fs::metadata(path)?.len());
    }
    Ok(total)
}

/// X-08：每步解码前按已知新增逻辑大小检查比例与目标卷预留。
fn check_expansion_space(
    job: &Job,
    declared: Option<u64>,
    packed: u64,
    prior_decoded: u64,
) -> Result<()> {
    if let Some(total) = declared {
        let decoded = prior_decoded
            .checked_add(total)
            .context("累计解压字节计数溢出")?;
        if job.config.max_ratio > 0 {
            ensure!(
                decoded <= packed.saturating_mul(job.config.max_ratio),
                "压缩包展开比例超过用户设置的上限"
            );
        }
    }
    let reserve = job.config.reserve_bytes;
    let free = fs2::available_space(&job.root)
        .map_err(|error| StopExtraction(format!("无法查询磁盘可用空间：{error}")))?;
    if let Some(total) = declared {
        if total.checked_add(reserve).context("容量计算溢出")? > free {
            return Err(StopExtraction(format!(
                "可用空间不足：本包需 {}，预留 {}，当前 {}；已保留原包并停止本次解压，未合入任何解压文件",
                bytes(total), bytes(reserve), bytes(free)
            )).into());
        }
    }
    Ok(())
}

fn skip_git_root(job: &mut Job, archive_rel: &str, subtrees: &HashSet<String>) -> Result<bool> {
    if !subtrees.contains("") {
        return Ok(false);
    }
    job.summary.skipped += 1;
    job.log(
        "解压",
        archive_rel,
        "",
        "跳过",
        "压缩包根目录含 .git：按 H-06 整树排除，未解出任何成员；原包保留",
        0,
    )?;
    Ok(true)
}

/// X-08：完整暂存结果先检查，再允许任何成员合入。未知声明大小同样检查实际比例。
fn validate_staging(
    job: &Job,
    content: &Path,
    declared: Option<u64>,
    packed: u64,
    prior_decoded: u64,
) -> Result<u64> {
    let mut expanded = 0u64;
    for entry in walkdir::WalkDir::new(content)
        .follow_links(false)
        .min_depth(1)
    {
        job.context.control.checkpoint()?;
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !fsutil::is_link(&metadata),
            "解压结果出现链接，拒绝整包合入"
        );
        let relative = fsutil::relative_string(content, entry.path())?;
        fsutil::safe_relative(&relative)?;
        if metadata.is_file() {
            expanded = expanded
                .checked_add(metadata.len())
                .context("解压字节计数溢出")?;
        } else {
            ensure!(metadata.is_dir(), "解压结果含非普通文件");
        }
    }
    if let Some(total) = declared {
        ensure!(expanded == total, "解压总量与条目清单不一致，原包保留");
    }
    let decoded = prior_decoded
        .checked_add(expanded)
        .context("累计解压字节计数溢出")?;
    if job.config.max_ratio > 0 {
        ensure!(
            decoded <= packed.saturating_mul(job.config.max_ratio),
            "压缩包实际展开比例超过用户设置的上限"
        );
    }
    Ok(decoded)
}

/// 任务级中止信号（X-05/X-06/X-08）：空间不足、以及成功原包/分卷删除失败都不是包
/// 损坏——保留当前源包（含未删除的分卷）、报错并停止整个解压任务，不隔离本包，也不
/// 继续处理后续包。错误在解压回调里会被 `with_context` 包装，判定走错误链而不是顶层
/// downcast（见 [`stops_extraction`]）。
#[derive(Debug)]
struct StopExtraction(String);

impl std::fmt::Display for StopExtraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StopExtraction {}

/// 错误链里是否带「停止任务」信号（空间不足无法继续）：命中即整次解压中止，
/// 不做隔离处置。
fn stops_extraction(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<StopExtraction>().is_some())
}

pub struct SevenZip {
    executable: PathBuf,
}
impl SevenZip {
    pub fn from_bundle() -> Result<Self> {
        // 随包目录与内嵌引擎统一走 resolve_executable：随包路径与 AGENTS §3.1 一致
        // 仅警告放行（LGPL 允许替换外部 7-Zip 引擎），内嵌释放路径保留硬校验。
        // 不再对随包路径做硬哈希 bail——那会拒绝用户自备/替换的引擎，与 LGPL 冲突。
        Ok(Self {
            executable: crate::engine_bundle::resolve_executable()?,
        })
    }
    /// Explicit test/development injection, never inferred from the system PATH.
    pub fn with_executable(executable: &Path) -> Result<Self> {
        Ok(Self {
            executable: fs::canonicalize(executable).context("指定的测试解压引擎不存在")?,
        })
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        if let Some(directory) = self.executable.parent() {
            command.current_dir(directory);
        }
        command
    }
    /// 列出压缩包条目：声明总大小、大小元数据是否完整、以及 H-06 的 Git 边界子树。
    /// 7-Zip 对 bzip2/xz 等流式格式可能不输出成员 Path，甚至不输出 Size；此时不能把条目静默丢掉，
    /// 否则 total=0 会在合入阶段误报「实际解压量超过压缩包声明」。
    /// 展开比例与空间预检由调用方执行（分母是实际卷集合合计，见 extract_one）。
    fn list(&self, archive: &Path, job: &mut Job) -> Result<Listing> {
        let control = job.context.control.clone();
        observe_archive("list_members", &control, |boundary_stage| {
            *boundary_stage = "member_listing_protocol";
            let result: Result<Listing> = (|| {
                let mut command = self.command();
                command
                    .args(["l", "-slt", "-ba", "-sccUTF-8", "-p-", "--"])
                    .arg(archive);
                let mut fields = BTreeMap::<String, String>::new();
                let mut total = 0u64;
                let mut count = 0u64;
                let mut sizes_complete = true;
                let mut git_subtrees = HashSet::new();
                let cfg = job.config.clone();
                let mut flush = |fields: &mut BTreeMap<String, String>| -> Result<()> {
                    let raw = if let Some(raw) = fields.remove("Path") {
                        raw
                    } else {
                        // 流式格式（bzip2/xz）可能没有 Path；若块内仍有成员元数据则用包名合成。
                        if fields.is_empty() {
                            return Ok(());
                        }
                        let has_meta = fields.contains_key("Size")
                            || fields.contains_key("Packed Size")
                            || fields.contains_key("Folder")
                            || fields.contains_key("Encrypted")
                            || fields.contains_key("Attributes");
                        if !has_meta {
                            fields.clear();
                            return Ok(());
                        }
                        stream_member_name(archive)
                    };
                    if raw == "." || raw == "./" {
                        fields.clear();
                        return Ok(());
                    }
                    let relative = fsutil::safe_relative(&raw).map_err(|error| {
                        member_rejected("invalid_member_path", &raw);
                        error
                    })?;
                    let raw = fsutil::path_string(&relative)?.replace('\\', "/");
                    if raw.split('/').any(|s| {
                        s.eq_ignore_ascii_case(".jchtools-work")
                            || s.eq_ignore_ascii_case("$RECYCLE.BIN")
                            || s.eq_ignore_ascii_case("System Volume Information")
                    }) {
                        member_rejected("protected_member_path", &raw);
                        bail!("拒绝压缩包中的程序工作区/系统目录条目：{raw}");
                    }
                    // X-07：「解压失败」同名普通成员正常落盘，但不继续递归处理其子树；
                    // 内部链接标记不是用户命名空间，仍在完整预检阶段拒绝。
                    if raw.split('/').any(|s| s.starts_with(".jchtools-link-")) {
                        member_rejected("internal_link_marker", &raw);
                        bail!("拒绝压缩包中的内部链接标记条目：{raw}");
                    }
                    // H-06：成员路径里出现 .git 组件时，登记「直接含该 .git 的目录」为整树
                    // 排除边界。边界必须在合入前从完整条目清单确定：该目录及全部后代整树
                    // 排除，包括 .git 的兄弟条目——只看落盘结果会把兄弟条目先写进用户目录，
                    // 而一旦 .git 落盘，该目录在后续扫描里就是永久排除区。
                    if let Some(prefix) = git_boundary_prefix(&raw) {
                        git_subtrees.insert(prefix);
                    }
                    if fields.get("Encrypted").is_some_and(|s| s == "+") {
                        member_rejected("encrypted_archive", &raw);
                        bail!("加密压缩包需要人工处理；没有把密码写入进程命令行");
                    }
                    for field in ["Symbolic Link", "Hard Link", "Reparse", "Alternate Stream"] {
                        if fields.get(field).is_some_and(|s| !s.is_empty() && s != "-") {
                            member_rejected("link_or_alternate_stream", &raw);
                            bail!("拒绝带链接、reparse 或备用数据流的压缩包：{raw}");
                        }
                    }
                    let attr = fields.get("Attributes").cloned().unwrap_or_default();
                    if attr.split_whitespace().any(|s| s.starts_with('l')) {
                        member_rejected("unix_symlink", &raw);
                        bail!("拒绝 Unix 符号链接条目：{raw}");
                    }
                    let directory = fields.get("Folder").is_some_and(|s| s == "+")
                        || attr.starts_with('D')
                        || attr.starts_with('d');
                    let size = match fields.get("Size") {
                        // 目录条目一律按 0 计入总量：部分引擎会给目录填非 0 Size，导致合入后 expanded!=total 必败。
                        _ if directory => 0,
                        Some(s) if !s.is_empty() => {
                            s.parse::<u64>().context("压缩包条目大小无效")?
                        }
                        _ => {
                            // 流式单文件包可能不声明展开大小：记为不完整，合入阶段跳过精确大小校验。
                            sizes_complete = false;
                            0
                        }
                    };
                    count = count.checked_add(1).context("条目计数溢出")?;
                    total = total.checked_add(size).context("解压总大小溢出")?;
                    if count > cfg.max_entries {
                        member_rejected("entry_limit", &raw);
                        bail!("压缩包条目数量超过用户设置的上限");
                    }
                    // X-08：单包与单文件展开体积不另设固定上限（允许 TB 级资料），
                    // 仍受条目数、展开比例、空间和文件系统能力约束。
                    // 条目信息只在此处做限额/危险校验，不再整包入库：archive_members 表此前
                    // 只写不读（上限百万条目的纯写放大），已随表一起删除。
                    fields.clear();
                    Ok(())
                };
                let ctl = job.context.control.clone();
                // 条目校验只读不落库（archive_members 已删除），无需事务壳。
                let listed = process::run(
                    &mut command,
                    &ctl,
                    |err, line| {
                        if err {
                            return Ok(());
                        }
                        if line.trim().is_empty() {
                            return flush(&mut fields);
                        }
                        if let Some((key, value)) = line.split_once(" = ") {
                            if fields.len() >= 100 {
                                tracing::error!(
                                    event = "archive_protocol_failed",
                                    stage = "member_metadata",
                                    error_type = "protocol",
                                    error_code = "metadata_field_limit",
                                    "归档成员元数据字段过多"
                                );
                                bail!("压缩包元数据异常");
                            }
                            if fields.insert(key.to_string(), value.to_string()).is_some() {
                                tracing::error!(
                                    event = "archive_protocol_failed",
                                    stage = "member_metadata",
                                    error_type = "protocol",
                                    error_code = "duplicate_metadata_field",
                                    "归档成员元数据存在重复字段"
                                );
                                bail!("压缩包元数据有重复字段，无法安全解析");
                            }
                        }
                        Ok(())
                    },
                    || Ok(()),
                )
                .and_then(|()| flush(&mut fields));
                listed?;
                Ok(Listing {
                    total,
                    sizes_complete,
                    git_subtrees,
                })
            })();
            if let Ok(listing) = &result {
                tracing::info!(
                    event = "archive_listing_result",
                    declared_bytes = listing.total,
                    sizes_complete = listing.sizes_complete,
                    git_subtrees = listing.git_subtrees.len(),
                    "归档成员清单结果"
                );
            }
            result
        })
    }
    /// 只读取最外层档案头中的格式与实际卷数。注释和后续内层档案头不具有删除授权。
    fn archive_volume_count(&self, archive: &Path, job: &mut Job) -> Result<ArchiveVolumes> {
        let control = job.context.control.clone();
        observe_archive("volume_count", &control, |boundary_stage| {
            *boundary_stage = "volume_header_protocol";
            let result: Result<ArchiveVolumes> = (|| {
                let mut command = self.command();
                command
                    .args(["l", "-slt", "-sccUTF-8", "-p-", "--"])
                    .arg(archive);
                let mut in_header = false;
                let mut header_seen = false;
                let mut kind = String::new();
                let mut count = None;
                let mut multipart = false;
                let mut new_rar_names = false;
                let ctl = job.context.control.clone();
                process::run(
                    &mut command,
                    &ctl,
                    |err, line| {
                        if err {
                            return Ok(());
                        }
                        let line = line.trim_end();
                        if line == "--" && !header_seen {
                            header_seen = true;
                            in_header = true;
                            return Ok(());
                        }
                        // Comment 是不可信的原样文本；此后绝不重新进入头块，哪怕注释伪造 } / --。
                        if line.starts_with("----") || line.starts_with("Comment =") || line == "{"
                        {
                            in_header = false;
                        }
                        if !in_header {
                            return Ok(());
                        }
                        if let Some((key, value)) = line.split_once(" = ") {
                            match key {
                                "Type" => value.clone_into(&mut kind),
                                "Volumes" => {
                                    let value =
                                        value.parse::<usize>().context("引擎分卷数量无效")?;
                                    ensure!(value > 0, "引擎分卷数量不能为零");
                                    count = Some(value);
                                }
                                "Characteristics" => {
                                    new_rar_names =
                                        value.split_whitespace().any(|flag| flag == "NewVolName");
                                }
                                "Volume Index" => multipart = true,
                                "Multivolume" => multipart |= value == "+",
                                _ => {}
                            }
                        }
                        Ok(())
                    },
                    || Ok(()),
                )
                .with_context(|| "无法确认压缩包实际分卷")?;
                ensure!(
                    !multipart || count.is_some(),
                    "引擎未提供可信的实际分卷数量，保留源包"
                );
                Ok(ArchiveVolumes {
                    kind,
                    count: count.unwrap_or(1),
                    new_rar_names,
                })
            })();
            if let Ok(info) = &result {
                tracing::info!(
                    event = "archive_volume_result",
                    count = info.count,
                    "归档实际分卷数量"
                );
            }
            result
        })
    }
    fn decode_into(&self, job: &Job, archive: &Path, output: &Path, label: &str) -> Result<()> {
        let control = job.context.control.clone();
        observe_archive("decode", &control, |boundary_stage| {
            *boundary_stage = "decoder_process_and_space";
            let mut command = self.command();
            command
                .args([
                    "x",
                    "-aou",
                    "-y",
                    "-bb0",
                    "-bsp1",
                    "-bso1",
                    "-bse2",
                    "-sccUTF-8",
                    "-p-",
                    "-mmt=2",
                ])
                .arg(format!("-o{}", fsutil::path_string(output)?))
                .arg("--")
                .arg(archive);
            process::run(
                &mut command,
                &job.context.control,
                |err, line| {
                    if !err && line.contains('%') {
                        job.context
                            .status(format!("正在解压 {label} · {}", line.trim()));
                    }
                    Ok(())
                },
                || {
                    if fs2::available_space(&job.root)
                        .map_err(|error| StopExtraction(format!("无法查询磁盘可用空间：{error}")))?
                        < job.config.reserve_bytes
                    {
                        return Err(StopExtraction(format!(
                            "磁盘剩余空间低于预留阈值 {}，已停止解压并保留原包",
                            bytes(job.config.reserve_bytes)
                        ))
                        .into());
                    }
                    Ok(())
                },
            )
            .with_context(|| "解压失败（可能已损坏、加密或格式不受支持）")
        })
    }

    fn extract_one(&self, job: &mut Job, archive_rel: &str, depth: u32) -> Result<bool> {
        let control = job.context.control.clone();
        observe_archive("extract_one", &control, |boundary_stage| {
            *boundary_stage = "input_volumes";
            let archive = fsutil::safe_join(&job.root, archive_rel)?;
            job.context.status(format!("检查压缩包：{archive_rel}"));
            // 文件名仅用于发现候选；删除与展开比例只能使用引擎实际打开的连续分卷。
            // 隔离仍使用独立的可逆宽匹配，不能把该集合复用为永久删除授权。
            let named = volume_set(&archive)?;
            x10_volume_precheck(archive_rel, &archive, &named)?;
            let volumes = if named.paths.len() > 1
                || matches!(named.scheme, VolumeScheme::RarParts | VolumeScheme::OldRar)
            {
                let info = self.archive_volume_count(&archive, job)?;
                named.scheme.actual_paths(&archive, &info)?
            } else {
                named.paths
            };
            // X-08：展开比例的分母是实际卷集合合计，不是主体单卷大小。
            let packed = volume_bytes(&volumes)?;
            *boundary_stage = "member_listing";
            let Listing {
                total,
                sizes_complete,
                mut git_subtrees,
            } = self.list(&archive, job)?;
            let composite = composite_stream(&archive);
            if !composite && skip_git_root(job, archive_rel, &git_subtrees)? {
                return Ok(false);
            }
            *boundary_stage = "expansion_space";
            check_expansion_space(job, sizes_complete.then_some(total), packed, 0)?;
            *boundary_stage = "staging_create";
            let mut stage = Staging::new(&job.root)?;
            *boundary_stage = "decode";
            self.decode_into(job, &archive, &stage.content, archive_rel)?;
            // -ba 列项可能透过复合流直接列出 tar 成员，首步解码却只生成中间 tar。
            // 中间物只在本次所有权暂存树内存在，不按最终清单总量校验、不正式落盘入队。
            *boundary_stage = "staging_validation";
            let mut decoded = validate_staging(
                job,
                &stage.content,
                (!composite && sizes_complete).then_some(total),
                packed,
                0,
            )?;
            if composite {
                *boundary_stage = "composite_container";
                let numbered = matches!(named.scheme, VolumeScheme::Numbered);
                let mut opened_tar = false;
                // 数字卷可能先重组压缩流；允许这一额外内部步骤，复合包仍只计一层。
                for layer in 0..=u8::from(numbered) {
                    let mut member = None;
                    for entry in walkdir::WalkDir::new(&stage.content)
                        .follow_links(false)
                        .min_depth(1)
                    {
                        job.context.control.checkpoint()?;
                        let entry = entry?;
                        if entry.file_type().is_file() {
                            ensure!(member.is_none(), "复合压缩流解码结果不是单一内部容器");
                            member = Some(entry.into_path());
                        }
                    }
                    let member = member.context("复合压缩流缺少内部 tar 容器")?;
                    let format = self.archive_volume_count(&member, job)?.kind;
                    let tar = format.eq_ignore_ascii_case("tar");
                    ensure!(
                        tar || (numbered && layer == 0 && compressed_stream_kind(&format)),
                        "复合压缩流未生成合法 tar 容器"
                    );
                    let listing = self.list(&member, job)?;
                    if tar && skip_git_root(job, archive_rel, &listing.git_subtrees)? {
                        return Ok(false);
                    }
                    check_expansion_space(
                        job,
                        listing.sizes_complete.then_some(listing.total),
                        packed,
                        decoded,
                    )?;
                    let next = Staging::new(&job.root)?;
                    self.decode_into(job, &member, &next.content, archive_rel)?;
                    decoded = validate_staging(
                        job,
                        &next.content,
                        (tar && listing.sizes_complete).then_some(listing.total),
                        packed,
                        decoded,
                    )?;
                    stage = next;
                    if tar {
                        git_subtrees = listing.git_subtrees;
                        opened_tar = true;
                        break;
                    }
                }
                ensure!(opened_tar, "复合压缩流缺少完整 tar 解码结果");
            }
            *boundary_stage = "destination_plan";
            let mut complete = true;
            let exclusions = rules::build_exclusions(&job.config.exclusions)?;
            let mut git = GitBoundaries::default();
            let base = archive.parent().context("压缩包缺少父目录")?;
            // X-04：目录落盘名规划。压缩包目录的默认落盘名被普通文件、链接或 junction
            // 占用时，为新目录选最小未占用序号（`目录 (1)`、`目录 (2)`），该目录及全部
            // 后代成员整体映射到新目录；既有文件一律不动，与既有普通目录同名则合入。
            // 规划必须在成员合入前完成：文件成员的父链与空目录条目共用这一映射。
            let dir_renames = plan_directory_renames(&job.context.control, &stage.content, base)?;
            let volume_renames = plan_volume_renames(
                job,
                &stage.content,
                base,
                &dir_renames,
                &exclusions,
                &mut git,
            )?;
            // One archive is decoded once, including solid archives. Final placement is rename, never copy.
            *boundary_stage = "member_output";
            let mut nested_members: BTreeMap<PathBuf, BTreeMap<String, PathBuf>> = BTreeMap::new();
            for entry in walkdir::WalkDir::new(&stage.content)
                .follow_links(false)
                .min_depth(1)
            {
                job.context.control.checkpoint()?;
                let entry = entry?;
                let meta = fs::symlink_metadata(entry.path())?;
                if fsutil::is_link(&meta) {
                    bail!("解压结果出现链接，已停止合入");
                }
                if meta.is_dir() {
                    continue;
                }
                if !meta.is_file() {
                    bail!("解压结果含非普通文件");
                }
                let relative = fsutil::relative_string(&stage.content, entry.path())?;
                let mut destination = base.join(fsutil::safe_relative(&relative)?);
                // X-04：祖先目录因被既有文件占用而整体改名时，成员落盘路径跟随映射。
                if let Some(mapped) = mapped_entry_rel(&dir_renames, &relative) {
                    destination = base.join(fsutil::safe_relative(&mapped)?);
                }
                if let Some(target) = volume_renames.get(&relative) {
                    destination.clone_from(target);
                }
                // 压缩包里含有与压缩包同名的成员（gzip 头会记录原始文件名，base.tgz 里就可能是 base.tgz）：
                // 绝不能覆盖仍在使用的源包。流式包的解压结果其实就是去掉一层压缩后的内容，
                // 用真实名字（base.tar）落盘并按正常冲突策略处理；其他格式改名放置。
                // 路径相等判断仅在 Windows 上忽略大小写（NTFS 不区分）；其他平台区分大小写。
                // 与 Windows 文件系统相同的序数键比较，Unicode lowercase 会错误展开主体。
                let collides_with_source = if cfg!(windows) {
                    fsutil::fold_rel(&fsutil::path_string(&destination)?)
                        == fsutil::fold_rel(&fsutil::path_string(&archive)?)
                } else {
                    destination == archive
                };
                if collides_with_source {
                    match stream_stem(&archive).map(|stem| base.join(stem)) {
                        Some(candidate) => destination = candidate,
                        None => destination = fsutil::unique_target(&job.root, &destination)?,
                    }
                }
                let destination_rel = fsutil::relative_string(&job.root, &destination)?;
                // 父链仍做整链校验；最终名可能是既有链接 / junction / OneDrive 在线占位，
                // 那是成员级场景（merge_extracted 改用唯一名落盘），整链校验会让含这类
                // 成员的整包解压失败。
                if let Some(split) = destination_rel.rfind('/') {
                    fsutil::safe_join(&job.root, &destination_rel[..split])?;
                }
                if inside_git_subtree(&git_subtrees, &relative) {
                    // H-06：该成员所属目录直接含 .git——整树排除（含 .git 的兄弟条目与
                    // 全部后代），不解出；原包按未完全解开处理（X-06），内容仍在原包里。
                    complete = false;
                    job.summary.skipped += 1;
                    job.log(
                        "解压",
                        archive_rel,
                        &destination_rel,
                        "跳过",
                        "成员位于含 .git 的目录树内：H-06 整树排除，不解压；原包保留",
                        meta.len(),
                    )?;
                    continue;
                }
                if member_excluded(&exclusions, &destination_rel)
                    || excluded_destination(&job.root, &destination, &job.config, &mut git)?
                {
                    complete = false;
                    job.summary.skipped += 1;
                    job.log(
                        "解压",
                        archive_rel,
                        &destination_rel,
                        "跳过",
                        "目标命中排除/隐藏/系统文件或 Git 目录树设置；原包保留",
                        meta.len(),
                    )?;
                    continue;
                }
                // 非 Windows 上点开头路径组件等价隐藏目录/文件：未开启 include_hidden 时不落盘，
                // 否则会成为扫描不可见的影子内容（engine 扫描按组件剪枝，去重/归类/清理都看不到）。
                #[cfg(not(windows))]
                if !job.config.include_hidden && has_hidden_component(&destination_rel) {
                    complete = false;
                    job.summary.skipped += 1;
                    job.log(
                        "解压",
                        archive_rel,
                        &destination_rel,
                        "跳过",
                        "路径含点开头（隐藏）组件，未开启包含隐藏文件；原包保留",
                        meta.len(),
                    )?;
                    continue;
                }
                let (final_path, renamed) =
                    match merge_extracted(&job.root, entry.path(), &destination)? {
                        MergeOutcome::Placed(path) => (path, false),
                        MergeOutcome::Renamed(path) => (path, true),
                    };
                job.summary.extracted += 1;
                let final_rel = fsutil::relative_string(&job.root, &final_path)?;
                if !normalize_new_member_attributes(&final_path, &job.config) {
                    // 剥离失败 = 成员带隐藏/系统属性落盘 = 扫描不可见的影子内容。
                    // 与其它影子内容同口径：原包强制保留并留日志（不把用户内容留成盲区）。
                    complete = false;
                    job.log(
                "解压",
                archive_rel,
                &final_rel,
                "保留",
                "成员已解压，但隐藏/系统属性剥离失败（将成扫描不可见的影子文件）；原包强制保留",
                meta.len(),
            )?;
                } else if renamed {
                    job.log(
                        "解压",
                        archive_rel,
                        &final_rel,
                        "成功",
                        "目标已存在：既有文件保持不动，新成员按 H-07 改名落盘（保留扩展名）",
                        meta.len(),
                    )?;
                } else {
                    job.log(
                        "解压",
                        archive_rel,
                        &final_rel,
                        "成功",
                        "解压并同卷移动",
                        meta.len(),
                    )?;
                }
                // X-08：继续处理本次解出的嵌套压缩包（层数上限沿用 max_depth）；
                // H-06：落点若位于 Git 目录树内则不处理该包（该树整树排除，不解压）。
                if potential_archive_member(
                    final_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("成员文件名无效")?,
                ) && !git.blocked(&job.root, final_path.parent().context("成员缺少父目录")?)?
                {
                    let excluded_tree = relative
                        .split('/')
                        .chain(final_rel.split('/'))
                        .any(|component| component.eq_ignore_ascii_case(QUARANTINE_DIR_NAME));
                    let below_root = final_path.parent().is_some_and(|parent| parent != job.root);
                    if excluded_tree || (!job.config.recursive && below_root) {
                        job.summary.skipped += 1;
                        job.log(
                            "解压",
                            archive_rel,
                            &final_rel,
                            "跳过",
                            if excluded_tree {
                                "新成员已落盘；隔离容器同名目录不继续递归解压"
                            } else {
                                "新成员已落盘；未开启递归，不继续处理子目录嵌套包"
                            },
                            meta.len(),
                        )?;
                    } else {
                        let parent = final_path.parent().context("成员缺少父目录")?.to_path_buf();
                        let name = final_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .context("成员文件名无效")?
                            .to_ascii_lowercase();
                        nested_members
                            .entry(parent)
                            .or_default()
                            .insert(name, final_path);
                    }
                }
            }
            // Preserve empty archive directories too. Do not merge them before checking for file/dir collisions.
            for entry in walkdir::WalkDir::new(&stage.content)
                .follow_links(false)
                .min_depth(1)
            {
                // 取消在目录操作的安全边界生效：已落盘的成员保留，后续空目录不再创建。
                job.context.control.checkpoint()?;
                let entry = entry?;
                if entry.file_type().is_dir() {
                    let rel = fsutil::relative_string(&stage.content, entry.path())?;
                    if inside_git_subtree(&git_subtrees, &rel) {
                        // H-06：该目录所属子树直接含 .git——整树排除，空目录也不创建。
                        complete = false;
                        job.summary.skipped += 1;
                        job.log(
                    "解压",
                    archive_rel,
                    "",
                    "跳过",
                    &format!("空目录位于含 .git 的目录树内（{rel}）：H-06 整树排除，不创建；原包保留"),
                    0,
                )?;
                        continue;
                    }
                    // X-04：目录自身或祖先被改名时，空目录条目落在新名字下（规划阶段已选好）。
                    let dest_rel =
                        mapped_entry_rel(&dir_renames, &rel).unwrap_or_else(|| rel.clone());
                    let dest = base.join(fsutil::safe_relative(&dest_rel)?);
                    let root_rel = fsutil::relative_string(&job.root, &dest)?;
                    // 父链仍整链校验；最终名可能是链接/junction（如 OneDrive 占位目录）：
                    // 已存在的目录直接合入；链接到目录不会创建也不会被写入（空目录无成员；
                    // 含成员时成员路径的父链校验仍会拒绝），不再让整包解压失败。
                    let dir_parent_rel = match root_rel.rfind('/') {
                        Some(i) => &root_rel[..i],
                        None => "",
                    };
                    if !dir_parent_rel.is_empty() {
                        fsutil::safe_join(&job.root, dir_parent_rel)?;
                    }
                    // 与文件合入/扫描同一过滤口径：X/** 不匹配 bare X，需补 X/ 变体；
                    // 祖先目录命中排除同样跳过（扫描对 X 整树剪枝，子目录不得落盘）。
                    if member_excluded(&exclusions, &root_rel)
                        || excluded_destination(&job.root, &dest, &job.config, &mut git)?
                    {
                        complete = false;
                        job.summary.skipped += 1;
                        job.log(
                            "解压",
                            archive_rel,
                            &root_rel,
                            "跳过",
                            "目标命中排除/隐藏/系统文件或 Git 目录树设置；原包保留",
                            0,
                        )?;
                        continue;
                    }
                    // 非 Windows 上点开头目录组件等价隐藏目录：不创建，否则目录连同其中的
                    // 成员都会成为扫描不可见的影子内容（excluded_destination 对尚不存在的
                    // 目标判不出"隐藏"，必须按名称判定）。
                    #[cfg(not(windows))]
                    if !job.config.include_hidden && has_hidden_component(&root_rel) {
                        complete = false;
                        job.summary.skipped += 1;
                        job.log(
                            "解压",
                            archive_rel,
                            &root_rel,
                            "跳过",
                            "路径含点开头（隐藏）组件，未开启包含隐藏文件；原包保留",
                            0,
                        )?;
                        continue;
                    }
                    if !dest.try_exists()? {
                        if let Err(error) = fs::create_dir_all(&dest) {
                            // 最终名被悬空链接等占用（try_exists 跟随链接判为不存在，但名字已被占）：
                            // 与文件成员的应急改名同口径降级为跳过，不让整包解压失败。
                            if fs::symlink_metadata(&dest).is_ok() {
                                complete = false;
                                job.summary.skipped += 1;
                                job.log(
                                    "解压",
                                    archive_rel,
                                    &root_rel,
                                    "跳过",
                                    &format!(
                                        "空目录名被既有文件/链接占用且无法创建：{error}；原包保留"
                                    ),
                                    0,
                                )?;
                                continue;
                            }
                            return Err(error)
                                .with_context(|| format!("无法创建目录 {}", dest.display()));
                        }
                    } else if !dest.is_dir() {
                        complete = false;
                        job.summary.skipped += 1;
                        job.log(
                            "解压",
                            archive_rel,
                            &root_rel,
                            "跳过",
                            "空目录名与目标处已有文件冲突；原包保留",
                            0,
                        )?;
                    }
                }
            }
            *boundary_stage = "nested_queue";
            enqueue_nested_groups(job, nested_members, depth + 1)?;
            // X-05：只有整包解码与校验成功、全部成员（含空目录）完整落盘后，才永久删除
            // 原包及其实际分卷；失败、部分解开或取消都不启动删除，也不删除仅同主干的文件。
            if complete {
                *boundary_stage = "source_delete";
                delete_successful_source(job, archive_rel, &volumes)?;
            }
            // 「解压成功」包数由调用方按 complete 口径累加：未完全解开的包要计入失败
            // 并移入「解压失败」（X-06），不得在解压层无条件先记一次成功。
            tracing::info!(
                event = "archive_extract_result",
                complete,
                volume_count = volumes.len(),
                decoded_bytes = decoded,
                "归档解压结果"
            );
            Ok(complete)
        })
    }
}
/// X-05：完整解开的原包与其实际分卷按永久删除处置（`volumes` 已在解压前按命名口径
/// 与档案级佐证门校验：主体 + 精确命名兄弟卷 + 已证实的宽命名兄弟卷）。
/// 复用 [`Job::delete_path`] 的既有语义（审计日志、删除计数、files 行失活），删除方式
/// 固定为永久删除，不受目录整理的全局删除方式影响。
/// 每个卷删除前检查取消：取消后不再启动后续删除（已落盘结果与未删除的卷保留）。
/// 删除失败即如实上抛为任务级中止（不是包损坏）：解压结果已落盘，未删除的卷保留在
/// 原位置，不做隔离、不虚计成功、也不回滚已删除的卷（X-05 不承诺多卷删除事务性）。
fn delete_successful_source(job: &mut Job, archive_rel: &str, volumes: &[PathBuf]) -> Result<()> {
    for (index, volume) in volumes.iter().enumerate() {
        // 取消在文件操作的安全边界生效：取消后不再启动任何删除。
        job.context.control.checkpoint()?;
        let expected = fsutil::snapshot(volume).ok();
        let reason = if index == 0 {
            "已完全解压落盘：按 X-05 永久删除原包"
        } else {
            "已完全解压落盘：按 X-05 随主体一并永久删除分卷"
        };
        if let Err(error) = job.delete_path(
            volume,
            expected.as_ref(),
            DeleteMode::Permanent,
            reason,
            true,
        ) {
            let unattempted = volumes.len() - index - 1;
            let volume_rel = fsutil::relative_string(&job.root, volume)
                .unwrap_or_else(|_| volume.display().to_string());
            let stop = StopExtraction(format!(
                "解压已完全落盘，但源包清理失败：永久删除原包/分卷时出错（{archive_rel} → {volume_rel}）；\
                 当前卷可能已删除，后续 {unattempted} 个卷尚未尝试删除；已删除的卷不回滚；不隔离本包，也不继续处理后续包：{error:#}"
            ));
            // 文件删除后的结果日志或数据库更新同样可能失败；不能据 Err 声称当前卷仍在。
            // 尽力补写日志，但始终保留清理失败的任务级中止，不再隔离已部分删除的包组。
            let _ = job.log(
                "删除",
                &volume_rel,
                "",
                "失败",
                &format!(
                    "源包清理未完成：解压结果已完全落盘，当前卷可能已删除；后续 {unattempted} 个卷未尝试删除（不隔离、不回滚）"
                ),
                expected.as_ref().map_or(0, |snapshot| snapshot.size),
            );
            return Err(stop.into());
        }
    }
    Ok(())
}
fn composite_stream(archive: &Path) -> bool {
    let Some(name) = archive.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = match name.rsplit_once('.') {
        Some((rest, digits))
            if digits.len() == 3 && digits.bytes().all(|digit| digit.is_ascii_digit()) =>
        {
            rest
        }
        _ => name,
    };
    [
        ".tgz",
        ".tbz2",
        ".txz",
        ".tzst",
        ".tar.gz",
        ".tar.bz2",
        ".tar.xz",
        ".tar.zst",
        ".tar.lzma",
        ".tar.z",
    ]
    .iter()
    .any(|suffix| {
        name.get(name.len().saturating_sub(suffix.len())..)
            .is_some_and(|ending| ending.eq_ignore_ascii_case(suffix))
    })
}

fn compressed_stream_kind(kind: &str) -> bool {
    ["gzip", "bzip2", "xz", "zstd", "lzma", "lzma86", "z"]
        .iter()
        .any(|supported| kind.eq_ignore_ascii_case(supported))
}

fn potential_archive_member(name: &str) -> bool {
    if rules::archive_name(name) {
        return true;
    }
    let Some((base, suffix)) = name.rsplit_once('.') else {
        return false;
    };
    if suffix.eq_ignore_ascii_case("rar") {
        return rar_part_stem(&name.to_ascii_lowercase()).is_some();
    }
    if suffix.len() == 3 && suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return rules::archive_name(base);
    }
    if suffix.len() == 3
        && matches!(suffix.as_bytes()[0], b'r' | b'R' | b'z' | b'Z')
        && suffix.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return rules::old_style_tail(&name.to_ascii_lowercase()).is_some();
    }
    false
}

fn enqueue_nested_groups(
    job: &Job,
    directories: BTreeMap<PathBuf, BTreeMap<String, PathBuf>>,
    depth: u32,
) -> Result<()> {
    for members in directories.into_values() {
        let mut seen = HashSet::new();
        for (name, path) in &members {
            if let Some(key) = rules::archive_entry_key(name) {
                if seen.insert(key) {
                    enqueue(job, path, depth)?;
                }
            }
        }
        let names: Vec<String> = members.keys().cloned().collect();
        for name in rules::part_rar_missing_first_groups(&names) {
            let path = members.get(&name).context("缺首卷候选不存在")?;
            if missing_part_rar_first(path).is_none() {
                continue;
            }
            let stem = rar_part_stem(&name).context("缺首卷候选形态无效")?;
            let key = rules::archive_entry_key(&format!("{stem}.part1.rar"))
                .context("缺首卷族无法归组")?;
            if seen.insert(key) {
                enqueue(job, path, depth)?;
            }
        }
        for name in rules::numbered_volume_missing_entry_groups(&names) {
            let path = members.get(&name).context("缺入口候选不存在")?;
            if missing_numbered_entry(path).is_none() {
                continue;
            }
            let key = rules::archive_entry_key(&format!("{}.001", &name[..name.len() - 4]))
                .context("缺入口族无法归组")?;
            if seen.insert(key) {
                enqueue(job, path, depth)?;
            }
        }
        for (name, path) in &members {
            let Some(tail) = rules::old_style_tail(name) else {
                continue;
            };
            if !missing_old_style_main(path) {
                continue;
            }
            let key = rules::archive_entry_key(&format!("{}.{}", tail.stem, tail.main_ext))
                .context("缺主包族无法归组")?;
            if seen.insert(key) {
                enqueue(job, path, depth)?;
            }
        }
    }
    Ok(())
}

/// 流式压缩包去掉**一层**压缩后缀后的名字；不是流式格式时返回 None。
/// 后缀集合与 X-01 白名单里的压缩流一致（`.tar.<流后缀>` 只去一层，剩下的 `.tar`
/// 由 tar 处理逻辑继续展开）。
fn stream_stem(archive: &Path) -> Option<String> {
    let name = archive.file_name()?.to_str()?.to_string();
    let lower = name.to_ascii_lowercase();
    for suffix in [
        ".tgz", ".tbz2", ".tbz", ".txz", ".tzst", ".bz2", ".gz", ".xz", ".lzma", ".zst", ".z",
    ] {
        if lower.ends_with(suffix) {
            let stem = &name[..name.len() - suffix.len()];
            if stem.is_empty() {
                return None;
            }
            // tgz/tbz/tbz2/txz/tzst 本质是 tar 容器，名字里补回 .tar
            if matches!(suffix, ".tgz" | ".tbz" | ".tbz2" | ".txz" | ".tzst")
                && !stem.to_ascii_lowercase().ends_with(".tar")
            {
                return Some(format!("{stem}.tar"));
            }
            return Some(stem.to_string());
        }
    }
    None
}
/// 流式压缩包（bzip2/xz/gzip）成员名：7-Zip 有时不输出 Path，用包名去掉**一层**压缩后缀合成。
fn stream_member_name(archive: &Path) -> String {
    if let Some(stem) = stream_stem(archive) {
        return stem;
    }
    let name = archive
        .file_name()
        .map_or_else(|| "content".into(), |s| s.to_string_lossy().into_owned());
    match archive.file_stem() {
        Some(stem) => stem.to_string_lossy().into_owned(),
        None => name,
    }
}

/// 成员排除判定与扫描剪枝口径对齐：扫描对目录 X 整树剪枝，因此裸目录名 `X`
/// 也必须排除其下成员 `X/y.txt`，否则成员落盘成为扫描不可见的影子，且原包每轮
/// 因目录 X 命中排除而强制保留、重复解压永不收敛。
fn member_excluded(exclusions: &rules::Exclusions, rel: &str) -> bool {
    let bytes = rel.as_bytes();
    for i in 0..=bytes.len() {
        if i == bytes.len() || bytes[i] == b'/' {
            let prefix = &rel[..i];
            if exclusions.is_match(prefix) || exclusions.is_match(format!("{prefix}/")) {
                return true;
            }
        }
    }
    false
}

/// 非 Windows 上点开头路径组件等价隐藏目录/文件（engine 扫描按组件剪枝）：
/// 未开启 include_hidden 时这样的解压目标不得落盘，否则会成为扫描不可见的影子内容。
#[cfg(not(windows))]
fn has_hidden_component(rel: &str) -> bool {
    rel.split('/').any(|part| part.starts_with('.'))
}

/// 新解压成员归本工具管理：7-Zip 会还原压缩包内的隐藏/系统属性，若不剥离，
/// 成员会变成扫描不可见的"影子文件"（去重/归类/清理都看不到它）。按用户当前的
/// 隐藏/系统过滤设置剥离相应属性，内容与名称不变。
/// 返回 false 表示剥离未生效，调用方必须按影子内容口径保留原包——剥离失败并不
/// 「不影响解压结果」：complete 若仍为真，原包会被删除，用户视角即内容丢失。
/// 路径必须显式补 \\?\ verbatim 前缀：裸 Win32 调用不走 std 的自动转换，
/// Windows 10 / 旧版 Windows 11（LongPathsEnabled 默认 0，清单需注册表配合）上
/// >260 字符路径会直接失败；UNC 目标须用 \\?\UNC\ 形式。
#[cfg(windows)]
fn normalize_new_member_attributes(path: &Path, config: &crate::config::Config) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    let original = meta.file_attributes();
    let mut attrs = original;
    if !config.include_hidden {
        attrs &= !2;
    }
    if !config.include_system {
        attrs &= !4;
    }
    if attrs == original {
        return true;
    }
    let text: Vec<u16> = path.as_os_str().encode_wide().collect();
    // \\?\ 与 \\?\UNC\ 前缀内的路径不做规范化，按原始组件逐字拼接。
    let (prefix, skip): (&[u16], usize) =
        if text.len() >= 4 && text[..4] == [0x5C, 0x5C, 0x3F, 0x5C] {
            (&[], 0) // 已是 verbatim：std 内部构造的 PathBuf 可能带 \\?\ 前缀，按原样使用
        } else if text.len() >= 2 && text[..2] == [0x5C, 0x5C] {
            (&[0x5C, 0x5C, 0x3F, 0x5C, 0x55, 0x4E, 0x43, 0x5C], 2) // \\server\... → \\?\UNC\server\...
        } else {
            (&[0x5C, 0x5C, 0x3F, 0x5C], 0)
        };
    let mut wide: Vec<u16> = Vec::with_capacity(prefix.len() + text.len() + 1);
    wide.extend_from_slice(prefix);
    wide.extend_from_slice(&text[skip..]);
    wide.push(0);
    // SAFETY: wide 是以 NUL 结尾的 UTF-16 路径（含 verbatim 前缀）；调用只读取该缓冲区。
    let result = unsafe { SetFileAttributesW(wide.as_ptr(), attrs) };
    result != 0
}
#[cfg(not(windows))]
fn normalize_new_member_attributes(_path: &Path, _config: &crate::config::Config) -> bool {
    true
}

/// 只判断根目录以下的层级。用户选定的根目录本身（例如位于隐藏的 AppData 之下）不参与隐藏/系统判定，
/// 否则整棵树都会被上级目录的属性判定为隐藏，所有解压结果都会被跳过；
/// 根即 Git 根属整次任务的前置拒绝，同理不在此判定。
/// H-06：目标（或其任一级祖先目录）直接含 `.git` 时整树排除——不解压、不写入。
fn excluded_destination(
    root: &Path,
    path: &Path,
    config: &crate::config::Config,
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
/// 解析 RAR 新式分卷名：把小写文件名拆成主干与 `.partN` 的数字串。
/// 与 rules 的分卷规则对齐：ASCII 正整数，允许前导零且不受整数机器宽度限制；
/// report.partial.rar、report.part0.rar 不得当作分卷。
fn split_rar_part(name: &str) -> Option<(&str, &str)> {
    let base = name.strip_suffix(".rar")?;
    let (stem, part) = base.rsplit_once(".part")?;
    (!part.is_empty()
        && part.bytes().all(|b| b.is_ascii_digit())
        && part.bytes().any(|b| b != b'0'))
    .then_some((stem, part))
}
/// 将小写文件名解析为 RAR 新式分卷主干（去掉末尾 `.partN.rar` 后的部分）。
fn rar_part_stem(name: &str) -> Option<&str> {
    split_rar_part(name).map(|(stem, _)| stem)
}
/// part rar 族的卷号数字串（`a.part01.rar` → `01`），判定与 [`rar_part_stem`] 同口径。
fn part_digits(name: &str) -> Option<&str> {
    split_rar_part(name).map(|(_, part)| part)
}
struct ArchiveVolumes {
    kind: String,
    count: usize,
    new_rar_names: bool,
}

/// 分卷命名仅描述引擎逐卷打开时使用的路径；是否多卷以及实际数量必须由引擎确认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeScheme {
    Single,
    RarParts,
    Numbered,
    OldRar,
    SplitZip,
}
impl VolumeScheme {
    fn actual_paths(self, archive: &Path, info: &ArchiveVolumes) -> Result<Vec<PathBuf>> {
        let kind = &info.kind;
        let count = info.count;
        if count == 1 {
            return Ok(vec![archive.to_path_buf()]);
        }
        let compatible = match self {
            Self::RarParts | Self::OldRar => {
                kind.eq_ignore_ascii_case("rar") || kind.eq_ignore_ascii_case("rar5")
            }
            Self::Numbered => kind.eq_ignore_ascii_case("split"),
            Self::SplitZip => kind.eq_ignore_ascii_case("zip"),
            Self::Single => false,
        };
        ensure!(compatible, "引擎格式与分卷命名不一致，保留源包");
        let name = archive
            .file_name()
            .and_then(|name| name.to_str())
            .context("无效分卷名称")?;
        // X-10：档案头证明卷数，但不能扩大精确命名族的删除授权。
        ensure!(
            self != Self::OldRar || (!info.new_rar_names && !kind.eq_ignore_ascii_case("rar5")),
            "RAR 新式卷名不属于老式 .rNN 命名族，保留源包"
        );
        ensure!(
            match self {
                Self::Numbered => count <= 999,
                Self::OldRar => count <= 101,
                Self::SplitZip => count <= 100,
                _ => true,
            },
            "实际卷数超出 X-10 精确命名族，保留源包"
        );
        let base = &name[..name.len() - 4];
        let mut rar_number = if self == Self::RarParts {
            base.as_bytes()
                .iter()
                .rposition(u8::is_ascii_digit)
                .map(|last| {
                    let end = last + 1;
                    let start = base.as_bytes()[..end]
                        .iter()
                        .rposition(|byte| !byte.is_ascii_digit())
                        .map_or(0, |position| position + 1);
                    (start, end, name.as_bytes()[start..end].to_vec())
                })
        } else {
            None
        };
        let mut paths = vec![archive.to_path_buf()];
        for index in 1..count {
            let next = match self {
                Self::RarParts | Self::OldRar => {
                    if let Some((start, end, digits)) = &mut rar_number {
                        let mut carry = true;
                        for digit in digits.iter_mut().rev() {
                            if *digit == b'9' {
                                *digit = b'0';
                            } else {
                                *digit += 1;
                                carry = false;
                                break;
                            }
                        }
                        if carry {
                            digits.insert(0, b'1');
                        }
                        format!(
                            "{}{}{}",
                            &name[..*start],
                            std::str::from_utf8(digits)?,
                            &name[*end..]
                        )
                    } else {
                        format!("{base}.r{:02}", index - 1)
                    }
                }
                Self::Numbered => format!("{base}.{:03}", index + 1),
                Self::SplitZip => format!("{}.z{index:02}", &name[..name.len() - 4]),
                Self::Single => unreachable!(),
            };
            let path = archive.with_file_name(next);
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("实际分卷不可访问：{}", path.display()))?;
            ensure!(
                metadata.file_type().is_file(),
                "实际分卷不是普通文件，保留源包：{}",
                path.display()
            );
            paths.push(path);
        }
        Ok(paths)
    }
}
/// 名称是否以恰好三位 ASCII 数字 `.NNN` 结尾（X-10 数字尾卷族的入口形态）。
/// 主干 = 去掉 `.NNN`；匹配不区分大小写由调用方的小写化保证，`.NNN` 本身是数字。
fn numbered_entry(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 4
        && bytes[bytes.len() - 4] == b'.'
        && bytes[bytes.len() - 3..].iter().all(u8::is_ascii_digit)
}
/// 候选组供可逆隔离使用；成功删除前必须转换成经引擎确认的实际卷集合。
#[derive(Debug)]
struct VolumeSet {
    paths: Vec<PathBuf>,
    scheme: VolumeScheme,
    /// X-10 part rar 命名歧义组（同一卷号数值存在多种补零写法，如 part1 与
    /// part01 并存）的描述；Some 时调用方必须整组按失败包处置——不猜测归属、
    /// 不解码删源。宽匹配的 paths 仍是整组卷，隔离可复用。
    part_ambiguity: Option<String>,
}
/// X-10：该文件是老式族尾卷、且同目录没有对应主包（`主干.zip`/`主干.rar`）。
/// 只发现这些尾卷时整组按失败包处置并报告缺主包（X-10：缺入口仍是一组失败包）。
/// X-10 卷集形态预检（纯文件系统判定，不依赖 7-Zip 引擎）：命名歧义组、老式族
/// 缺主包、part rar 缺首卷、数字尾卷缺入口，任一命中即整组按失败包隔离。
/// E-05 边界：这类隔离不是解压，不得因宿主无引擎而被整体报错吞掉——预检必须
/// 先于引擎解析执行（回归：CI run 37131023474 上缺首卷组因引擎解析前置报错，
/// 未能按 X-06 隔离）。
fn x10_volume_precheck(archive_rel: &str, archive: &Path, named: &VolumeSet) -> Result<()> {
    // X-10：同主干混用补零模式（如 part1 与 part01 并存）是命名歧义组：
    // 列出全部歧义卷，不猜测归属、不解码删源——整组按失败包走 X-06 隔离。
    if let Some(ambiguity) = &named.part_ambiguity {
        anyhow::bail!("X-10 命名歧义分卷组：{archive_rel}（{ambiguity}）");
    }
    // X-10：老式 zip/rar 族只发现尾卷、没有主包时整组按失败包处置并报告缺主包，
    // 不把尾卷交给引擎猜格式（引擎对孤立 .zNN/.rNN 的报错只会说「无法打开」，
    // 指向不了真正原因）。
    if missing_old_style_main(archive) {
        anyhow::bail!(
            "老式分卷族缺主包：{archive_rel}（只发现 .zNN/.rNN 尾卷，未找到 主干.zip/主干.rar）"
        );
    }
    // X-10：part rar 族缺首卷——当前卷是 partN（N≥2）且同目录没有对应宽度的
    // part1 入口（缺首卷组由扫描按代表入队）。整组按失败包处置并报告缺首卷，
    // 不把余卷当独立包或交给引擎猜。附录 E：a.part02+a.part03 缺 01 → 一组
    // 缺首卷失败，不当两个完整 rar 解压。
    if let Some(reason) = missing_part_rar_first(archive) {
        anyhow::bail!("part rar 分卷族缺首卷：{archive_rel}（{reason}）");
    }
    // X-10：.000 不是入口；它即使与 .001 或后续卷共存也使整组非法。
    // 入口扫描会将 .000 入队，后续卷检查也必须能从卷集合中发现它。
    if named.scheme == VolumeScheme::Numbered {
        if let Some(zero) = named.paths.iter().find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".000"))
        }) {
            anyhow::bail!(
                "数字尾卷族包含非法起始卷：{archive_rel}（{}）",
                zero.display()
            );
        }
    }
    // X-10：数字尾卷族缺入口——.NNN（N≥2）且同目录没有 .001 入口。
    // .000 已在上面作为非法起始卷单独拒绝，不得用它替代 .001。
    if let Some(stem) = missing_numbered_entry(archive) {
        anyhow::bail!("数字尾卷族缺起始卷：{archive_rel}（未找到 {stem}.001 入口卷）");
    }
    if named.scheme != VolumeScheme::Single {
        let mut numbers = Vec::with_capacity(named.paths.len());
        for path in &named.paths {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .context("分卷名称无法无损表示")?
                .to_ascii_lowercase();
            let number = match named.scheme {
                VolumeScheme::Numbered => name[name.len() - 3..].parse::<u64>()?,
                VolumeScheme::RarParts => part_digits(&name)
                    .context("无效 part 分卷名")?
                    .parse::<u64>()
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "X-10 分卷不全：{archive_rel}（编号超出本组可能的连续范围）"
                        )
                    })?,
                VolumeScheme::OldRar | VolumeScheme::SplitZip => {
                    if rules::old_style_tail(&name).is_some() {
                        let number = name[name.len() - 2..].parse::<u64>()?;
                        if named.scheme == VolumeScheme::OldRar {
                            number + 1
                        } else {
                            number
                        }
                    } else {
                        0
                    }
                }
                VolumeScheme::Single => unreachable!(),
            };
            numbers.push(number);
        }
        numbers.sort_unstable();
        let start = u64::from(matches!(
            named.scheme,
            VolumeScheme::Numbered | VolumeScheme::RarParts
        ));
        for (offset, number) in numbers.into_iter().enumerate() {
            ensure!(
                number == start + u64::try_from(offset)?,
                "X-10 分卷不全：{archive_rel}（缺起始编号或中间断号）"
            );
        }
    }
    Ok(())
}

fn missing_old_style_main(archive: &Path) -> bool {
    let Some(name) = archive.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    let Some(tail) = rules::old_style_tail(&lower) else {
        return false;
    };
    !fs::symlink_metadata(archive.with_file_name(format!("{}.{}", tail.stem, tail.main_ext)))
        .is_ok_and(|metadata| metadata.file_type().is_file())
}
/// X-10：该文件是 partN（N≥2）的分卷、且同目录没有同主干同宽度的 part1 入口。
/// 返回缺首卷的说明；不是该形态或入口在场时返回 None。任意补零写法均以卷号 1 为入口，
/// 同主干宽度冲突由 volume_set 另行判为命名歧义。
fn missing_part_rar_first(archive: &Path) -> Option<String> {
    let name = archive.file_name().and_then(|s| s.to_str())?;
    let lower = name.to_ascii_lowercase();
    let (stem, digits) = split_rar_part(&lower)?;
    let number = digits.trim_start_matches('0');
    if number == "1" {
        return None;
    }
    // 入口按卷号数值判定（X-10「位数超过最小宽度时自然增长」）：`part1` 在场
    // 即有入口，任何写法（part1/part01/part001）都算——按当前卷的补零宽度拼
    // `part01` 会把完整无补零 10+ 卷集（…part9 + part10）误判成缺首卷。
    if sibling_part_number_one(archive, stem) {
        return None;
    }
    Some(format!(
        "未找到 {stem} 命名族的入口卷（part1，任意补零写法）"
    ))
}
/// 同目录下同主干的 part rar 卷里是否存在卷号 1（任意补零写法）。
fn sibling_part_number_one(archive: &Path, stem: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(archive.parent().unwrap_or_else(|| Path::new("."))) else {
        // 无法枚举兄弟卷时保守视为入口在场，交由引擎整包校验兜底。
        return true;
    };
    let stem_key = fsutil::fold_rel(stem);
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let Some((sibling_stem, sibling_digits)) = split_rar_part(&lower) else {
            continue;
        };
        if fsutil::fold_rel(sibling_stem) == stem_key
            && sibling_digits.trim_start_matches('0') == "1"
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            // 卷号 1 的任意补零写法（part1/part01/part001）都是该族入口。
            return true;
        }
    }
    false
}
/// X-10：该文件是数字尾卷族的 `.NNN`（编号 ≥ 2）、且同目录没有 `.001` 入口
///（也没有作为可识别非法起始入队的 `.000`）。返回主干名供错误指认。
fn missing_numbered_entry(archive: &Path) -> Option<String> {
    let name = archive.file_name().and_then(|s| s.to_str())?;
    let lower = name.to_ascii_lowercase();
    if !numbered_entry(&lower) {
        return None;
    }
    let stem = &lower[..lower.len() - 4];
    let digits = &lower[lower.len() - 3..];
    let Ok(number) = digits.parse::<u64>() else {
        return None;
    };
    if number < 2 {
        return None;
    }
    if archive.with_file_name(format!("{stem}.001")).exists()
        || archive.with_file_name(format!("{stem}.000")).exists()
    {
        return None;
    }
    Some(stem.to_string())
}
/// RAR part 编号中显式前导零代表最小补零宽度；无前导零时返回 None。
fn part_padding_width(digits: &str) -> Option<usize> {
    let natural_width = digits.trim_start_matches('0').len().max(1);
    (digits.len() > natural_width).then_some(digits.len())
}
/// 分卷组解析：返回主体自身 + 同目录下的兄弟卷（X-05 删除与 X-06 隔离的处置单位）。
/// 非分卷包（命名不能匹配任何分卷方案）返回只含主体自身的单项集合，且**不枚举目录**：
/// 单卷 7z/tar/gz 等格式没有可匹配的兄弟卷命名，逐包扫描目录是纯粹的重复工作。
/// 识别口径与旧 protect_volumes 一致：分卷识别只走 rar_part_stem，不用 starts_with
/// 宽匹配（report.partial.rar 不是分卷）。
fn volume_set(archive: &Path) -> Result<VolumeSet> {
    let name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .context("无效压缩包名称")?
        .to_ascii_lowercase();
    let (stem, scheme) = if let Some(stem) = rar_part_stem(&name) {
        (stem.to_string(), VolumeScheme::RarParts)
    } else if numbered_entry(&name) {
        // X-10：主包完整名带白名单扩展名（含复合扩展名）再带恰好三位 .NNN。
        // 入口侧（rules）只放行 `<白名单后缀>.NNN`；这里按「.NNN 结尾」认族，
        // 主干即去掉 `.NNN`（如 `x.tar.gz.001` 的主干是 `x.tar.gz`）。
        (name[..name.len() - 4].to_string(), VolumeScheme::Numbered)
    } else if let Some(tail) = rules::old_style_tail(&name) {
        // X-10：老式族尾卷按其族方案成组——主包缺位时（尾卷组已按缺主包入队），
        // 失败处置与隔离仍能按整组移动；主包在场时尾卷不会单独走到这里（不单独入队）。
        let scheme = if tail.main_ext == "zip" {
            VolumeScheme::SplitZip
        } else {
            VolumeScheme::OldRar
        };
        (tail.stem.to_string(), scheme)
    } else if let Some(stem) = name.strip_suffix(".rar") {
        (stem.to_string(), VolumeScheme::OldRar)
    } else if let Some(stem) = name.strip_suffix(".zip") {
        (stem.to_string(), VolumeScheme::SplitZip)
    } else {
        return Ok(VolumeSet {
            paths: vec![archive.to_path_buf()],
            scheme: VolumeScheme::Single,
            part_ambiguity: None,
        });
    };
    // 语法只折叠 ASCII；主干用文件系统序数键，不能先 Unicode 展开再猜测卷归属。
    let stem_key = fsutil::fold_rel(&stem);
    let matches_candidate = |candidate: &str| -> bool {
        match scheme {
            VolumeScheme::RarParts => {
                rar_part_stem(candidate).is_some_and(|stem| fsutil::fold_rel(stem) == stem_key)
            }
            VolumeScheme::Numbered => candidate.rsplit_once('.').is_some_and(|(stem, digits)| {
                digits.len() == 3
                    && digits.bytes().all(|digit| digit.is_ascii_digit())
                    && fsutil::fold_rel(stem) == stem_key
            }),
            VolumeScheme::OldRar | VolumeScheme::SplitZip => rules::old_style_tail(candidate)
                .is_some_and(|tail| {
                    (tail.main_ext == "rar") == (scheme == VolumeScheme::OldRar)
                        && fsutil::fold_rel(tail.stem) == stem_key
                }),
            VolumeScheme::Single => false,
        }
    };
    let mut paths = vec![archive.to_path_buf()];
    // X-10 part rar 命名歧义检测：混用补零宽度或同一卷号有不同写法都不猜测归属。
    // 键为去前导零的十进制串，正整数卷号不受机器整数宽度限制。
    let mut part_ambiguity: Option<String> = None;
    let mut padded_width: Option<usize> = None;
    let mut part_seen: HashMap<String, String> = HashMap::new();
    if matches!(scheme, VolumeScheme::RarParts) {
        if let Some(digits) = part_digits(&name) {
            part_seen.insert(
                digits.trim_start_matches('0').to_string(),
                digits.to_string(),
            );
            padded_width = part_padding_width(digits);
        }
    }
    for entry in fs::read_dir(archive.parent().context("压缩包缺少目录")?)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let candidate = name.to_ascii_lowercase();
        // 主体自身已在集合里（如 part1.rar 对主干同判）；大小写不敏感路径上可能重复命名，去重交给文件系统唯一性。
        if matches_candidate(&candidate) && entry.path() != archive && entry.file_type()?.is_file()
        {
            if matches!(scheme, VolumeScheme::RarParts) {
                if let Some(digits) = part_digits(&candidate) {
                    if let Some(width) = part_padding_width(digits) {
                        match padded_width {
                            Some(existing) if existing != width => {
                                part_ambiguity
                                    .get_or_insert_with(|| "同一主干混用不同补零位数".to_string());
                            }
                            None => padded_width = Some(width),
                            _ => {}
                        }
                    }
                    let key = digits.trim_start_matches('0');
                    match part_seen.get(key) {
                        Some(existing) if existing != digits && part_ambiguity.is_none() => {
                            part_ambiguity = Some(format!(
                                "同一卷号存在多种补零写法（{existing} 与 {digits} 并存）"
                            ));
                        }
                        None => {
                            part_seen.insert(key.to_string(), digits.to_string());
                        }
                        _ => {}
                    }
                }
            }
            paths.push(entry.path());
        }
    }
    if part_ambiguity.is_none()
        && padded_width.is_some_and(|width| {
            part_seen.values().any(|digits| {
                let natural_width = digits.trim_start_matches('0').len().max(1);
                digits.len() != width.max(natural_width)
            })
        })
    {
        part_ambiguity = Some("同一主干混用补零模式".to_string());
    }
    // 列出全部歧义卷：整组卷清单随描述一并返回（X-10「列出全部歧义卷」）。
    let part_ambiguity = part_ambiguity.map(|reason| {
        let names = paths
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>()
            .join("、");
        format!("{reason}；整组卷：{names}")
    });
    Ok(VolumeSet {
        paths,
        scheme,
        part_ambiguity,
    })
}
/// 目标位置的占用形态（X-04）：空闲、可合入的普通目录、或被文件/链接等占用。
enum Occupancy {
    Free,
    /// 普通目录（非链接/junction）：同名新目录按 X-04 合入。
    PlainDir,
    /// 普通文件、悬空链接或重解析点：名字被占，新目录须另选未占用名。
    Blocked,
}
fn classify_occupancy(path: &Path) -> Result<Occupancy> {
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
fn mapped_entry_rel(renames: &HashMap<String, String>, rel: &str) -> Option<String> {
    if let Some(mapped) = renames.get(rel) {
        return Some(mapped.clone());
    }
    let components: Vec<&str> = rel.split('/').collect();
    let mut prefix = String::new();
    let mut matched: Option<(&String, usize)> = None;
    for component in components.iter().take(components.len().saturating_sub(1)) {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        if let Some(mapped) = renames.get(&prefix) {
            matched = Some((mapped, prefix.len()));
        }
    }
    let (mapped, matched_len) = matched?;
    Some(format!("{mapped}{}", &rel[matched_len..]))
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
fn quarantine_targets(dir: &Path, names: &[(String, String)]) -> Result<Option<Vec<PathBuf>>> {
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
fn quarantine(job: &mut Job, archive_rel: &str, reason: &str) -> Result<()> {
    let control = job.context.control.clone();
    observe_archive("quarantine", &control, |boundary_stage| {
        *boundary_stage = "quarantine_move";
        let archive = fsutil::safe_join(&job.root, archive_rel)?;
        // 隔离使用 X-10 精确命名族的在场卷；失败包未必能由引擎确认卷数，
        // 因此不把成功删源所需的档案头佐证作为可逆隔离的前置条件。
        let sources = volume_set(&archive)?.paths;
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
            let source_rel = fsutil::relative_string(&job.root, &source)?;
            let size = fsutil::snapshot(&source).map_or(0, |s| s.size);
            fsutil::rename_noreplace(&source, &target)?;
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
    })
}
/// 单个条目合入结果：冲突（目标已存在）不淘汰、不询问，只给新成员改名。
enum MergeOutcome {
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
fn merge_extracted(root: &Path, source: &Path, target: &Path) -> Result<MergeOutcome> {
    // 只校验/创建父目录链（ensure_dir 创建目录链本身，ensure_parent 只会创建到祖父目录，
    // 带子目录的成员会因此以「系统找不到指定的路径」整包失败）；
    // 最终名被符号链接 / junction / OneDrive 在线占位占用是成员级场景
    //（下方改用唯一名落盘），对最终名也做整链校验会让含这类成员的整包失败。
    let parent = target.parent().context("目标缺少父目录")?;
    if parent != root {
        fsutil::ensure_dir(root, parent)?;
    }
    if !target.try_exists()? {
        match fsutil::rename_noreplace(source, target) {
            Ok(()) => return Ok(MergeOutcome::Placed(target.to_path_buf())),
            Err(error) => {
                // TOCTOU：目标在 try_exists 与 rename 之间出现。与既有冲突同一处置：
                // 改用唯一名落盘，而不是整包失败（P-08 不要求防御外部改动，但这里
                // 本来就有不覆盖的安全落点）。
                let emergency = fsutil::unique_target(root, target)?;
                fsutil::rename_noreplace(source, &emergency).map_err(|_| error)?;
                return Ok(MergeOutcome::Renamed(emergency));
            }
        }
    }
    // 目标已存在（普通文件、链接、坏链或目录）：绝不覆盖、不淘汰，直接改名落盘。
    let renamed = fsutil::unique_target(root, target)?;
    fsutil::rename_noreplace(source, &renamed).with_context(|| {
        format!(
            "目标已存在且改名落盘失败：{} → {}",
            target.display(),
            renamed.display()
        )
    })?;
    Ok(MergeOutcome::Renamed(renamed))
}
pub fn enqueue(job: &Job, archive: &Path, depth: u32) -> Result<()> {
    let snapshot = fsutil::snapshot(archive)?;
    let relative = fsutil::relative_string(&job.root, archive)?;
    let fingerprint = format!(
        "{}:{}:{}:{}",
        relative, snapshot.size, snapshot.modified_ns, snapshot.identity
    );
    // 同一路径的包可能已被扫描按旧内容入队、随后被其他包的成员覆盖：清掉未处理的旧行再入队。
    // 唯一键在 fingerprint 上，INSERT OR IGNORE 挡不住同 rel 的新内容；旧行残留会在其
    // 删除该包后让新行 snapshot 失败，把「解压失败 N 包」与错误计数虚高。
    job.db.conn.execute(
        "DELETE FROM archives WHERE rel=?1 AND state='pending'",
        params![relative],
    )?;
    job.db.conn.execute(
        "INSERT OR IGNORE INTO archives(rel,fingerprint,depth) VALUES(?1,?2,?3)",
        params![relative, fingerprint, depth],
    )?;
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "extract_batch", skip_all)
)]
pub fn extract_queued(job: &mut Job, resolve_engine: impl Fn() -> Result<SevenZip>) -> Result<()> {
    let control = job.context.control.clone();
    observe_archive("extract_queue", &control, |boundary_stage| {
        *boundary_stage = "queue_database"; // 引擎懒解析（E-05/X-06 边界）：缺首卷/缺入口/歧义组/缺主包的整组隔离是
                                            // 纯文件系统判定，不需要 7-Zip 引擎，先于引擎解析执行——无引擎宿主上这些
                                            // 组仍按 X-06 隔离（回归 CI run 37131023474：引擎解析前置曾把缺首卷组整体
                                            // 报错）。首个通过预检的真实解压包出现时才解析引擎；解析失败按 E-05 明确
                                            // 报错并终止任务，不得把这些包静默跳过或全部隔离。
        let mut engine: Option<SevenZip> = None;
        loop {
            *boundary_stage = "queue_database";
            job.context.control.checkpoint()?;
            let next: Option<(i64, String, u32, String)> = job
        .db
        .conn
        .query_row(
            "SELECT id,rel,depth,fingerprint FROM archives WHERE state='pending' ORDER BY depth,id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
            let Some((id, relative, depth, stored_fingerprint)) = next else {
                break;
            };
            let item_span = tracing::info_span!("archive_item", archive_id = id, depth,
        source = %crate::logging::safe_error(&relative));
            let _item_entered = item_span.enter();
            let item_started = std::time::Instant::now();
            tracing::info!(event = "archive_item_started", "压缩包处理开始");
            // 指纹复核：排队期间该路径可能已被删除或替换（X-04 之后成员一律改名落盘，
            // 不会再顶替既有路径，所以这里只剩外部改动这一种可能）。与 enqueue 同公式
            // 重算指纹，不一致即「原包已不在原位」，清掉残留 pending 行并如实记一条跳过：
            // 不得把已改动的路径当原包解压，也不得虚报失败计数或隔离它。
            let Ok(path) = fsutil::safe_join(&job.root, &relative) else {
                tracing::warn!(
                    event = "archive_item_skipped",
                    stage = "input_path",
                    elapsed_ms = crate::logging::elapsed_ms(item_started),
                    "原包路径不可安全访问，清除队列行"
                );
                job.db
                    .conn
                    .execute("DELETE FROM archives WHERE id=?1", [id])?;
                continue;
            };
            // 路径已不存在或此刻无法读取：残留行一并清除。
            let superseded = if let Ok(snapshot) = fsutil::snapshot(&path) {
                let fingerprint = format!(
                    "{}:{}:{}:{}",
                    relative, snapshot.size, snapshot.modified_ns, snapshot.identity
                );
                fingerprint != stored_fingerprint
            } else {
                true
            };
            if superseded {
                job.db
                    .conn
                    .execute("DELETE FROM archives WHERE id=?1", [id])?;
                job.summary.skipped += 1;
                job.log(
                    "解压",
                    &relative,
                    "",
                    "跳过",
                    "扫描入队后无法确认原包仍在原位（已被删除、改写或此刻不可读），本次不再处理",
                    0,
                )?;
                tracing::warn!(
                    event = "archive_item_skipped",
                    stage = "input_fingerprint",
                    elapsed_ms = crate::logging::elapsed_ms(item_started),
                    "无法确认原包仍在原位，跳过处理"
                );
                continue;
            }
            job.db
                .conn
                .execute("UPDATE archives SET state='running' WHERE id=?1", [id])?;
            *boundary_stage = "volume_precheck";
            let result = if depth >= job.config.max_depth {
                Err(anyhow::anyhow!("达到最大嵌套层数（X-08 防护上限）"))
            } else if let Err(reason) =
                volume_set(&path).and_then(|named| x10_volume_precheck(&relative, &path, &named))
            {
                Err(reason)
            } else {
                if engine.is_none() {
                    *boundary_stage = "engine_resolution";
                    engine = Some(observe_archive("engine_resolution", &control, |stage| {
                        *stage = "engine_assets";
                        resolve_engine()
                    })?);
                }
                // 上一分支保证引擎已解析；此处拿不到引用只能说明内部状态被破坏，
                // 按 E-05 口径报错而不是 panic。
                let Some(active) = engine.as_ref() else {
                    anyhow::bail!("共享 7-Zip 引擎未就绪（E-05：无引擎时解压必须明确报错并停止）");
                };
                *boundary_stage = "extract_one";
                active.extract_one(job, &relative, depth)
            };
            match &result {
                Ok(complete) => tracing::info!(
                    event = "archive_item_decoded",
                    complete,
                    elapsed_ms = crate::logging::elapsed_ms(item_started),
                    "压缩包解压阶段结束"
                ),
                Err(_) if job.context.control.is_cancelled() => tracing::info!(
                    event = "archive_item_cancelled",
                    elapsed_ms = crate::logging::elapsed_ms(item_started),
                    "压缩包处理已取消"
                ),
                Err(_) => tracing::error!(
                    event = "archive_item_failed",
                    stage = *boundary_stage,
                    error_type = "archive_boundary",
                    elapsed_ms = crate::logging::elapsed_ms(item_started),
                    "压缩包处理失败"
                ),
            }
            *boundary_stage = "settle_or_quarantine";
            match result {
                Ok(true) => {
                    job.db
                        .conn
                        .execute("UPDATE archives SET state='done' WHERE id=?1", [id])?;
                    // X-05/X-07：完全解开的包（含全部分卷）已在 extract_one 尾部永久删除，
                    // 删除失败会以任务级中止上抛、走不到这里；同次任务里该包已标 done，
                    // 不会再入队（X-07/H-04）。
                    job.summary.archives_ok += 1;
                    job.log(
                        "解压",
                        &relative,
                        "",
                        "成功",
                        "已完全解开；原包与分卷按 X-05 永久删除",
                        0,
                    )?;
                }
                Ok(false) => {
                    job.db
                        .conn
                        .execute("UPDATE archives SET state='done' WHERE id=?1", [id])?;
                    job.summary.archives_failed += 1;
                    job.log(
                        "解压",
                        &relative,
                        "",
                        "未完全解开",
                        "有成员被跳过或未落盘（排除规则/Git 目录树/目标冲突无法落位）；原包保留",
                        0,
                    )?;
                    // X-06：隔离自身失败（容器被占用、无法分配目标名或某卷移动失败）时，
                    // 该失败包原地保留并记录隔离失败原因与未移动源项位置，其余包可继续。
                    if let Err(error) = quarantine(
                        job,
                        &relative,
                        "未能完全解开：有成员被跳过、被排除规则命中或位于 Git 目录树内",
                    ) {
                        job.summary.errors += 1;
                        job.log(
                            "解压",
                            &relative,
                            "",
                            "隔离失败",
                            &format!("原包原地保留，未移动的源项仍在原位置：{error:#}"),
                            0,
                        )?;
                        job.context.control.check_cancelled()?;
                    }
                }
                Err(error) => {
                    // 归档行先标 failed，避免永久停在 running。
                    job.db
                        .conn
                        .execute("UPDATE archives SET state='failed' WHERE id=?1", [id])?;
                    // 用户主动取消：上抛取消错误，不隔离、不累加失败计数、不写失败日志
                    // （与 apply_with 的取消口径一致）。单出口，避免双写 failed/误计失败。
                    if job.context.control.is_cancelled() {
                        job.context.control.check_cancelled()?;
                    }
                    // X-06/X-08：空间不足不是包损坏——保留源包并停止整个解压任务，
                    // 不隔离本包，也不继续批量隔离后续正常包。
                    if stops_extraction(&error) {
                        job.summary.errors += 1;
                        return Err(error);
                    }
                    job.summary.archives_failed += 1;
                    job.summary.errors += 1;
                    job.log("解压", &relative, "", "失败", &format!("{error:#}"), 0)?;
                    // X-06：解压出错（损坏/加密/不支持/触上限）的原包移入「解压失败」。
                    // 隔离自身失败时记录隔离失败原因与原包位置，不中止其余包的处理。
                    if let Err(quarantine_error) =
                        quarantine(job, &relative, &format!("解压失败：{error:#}"))
                    {
                        job.log(
                            "解压",
                            &relative,
                            "",
                            "隔离失败",
                            &format!("原包原地保留，未移动的源项仍在原位置：{quarantine_error:#}"),
                            0,
                        )?;
                    }
                    job.context.control.check_cancelled()?;
                }
            }
            tracing::info!(
                event = "archive_item_completed",
                elapsed_ms = crate::logging::elapsed_ms(item_started),
                total_ok = job.summary.archives_ok,
                total_failed = job.summary.archives_failed,
                errors = job.summary.errors,
                "压缩包处理及处置结束"
            );
            // U-03：解压页的实时计数按「已处理的包」统计（成败都算处理过）；与目录整理的
            // 计划项计数互不影响（两工具各自持有独立 Control）。
            job.context
                .control
                .completed
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    })
}
struct Staging {
    directory: PathBuf,
    content: PathBuf,
    owner: String,
}
impl Staging {
    fn new(root: &Path) -> Result<Self> {
        let owner = uuid::Uuid::new_v4().to_string();
        let relative = format!(".jchtools-work/{owner}");
        let directory = fsutil::safe_join(root, &relative)?;
        fs::create_dir_all(&directory)?;
        let marker = directory.join("OWNER");
        fs::write(&marker, &owner)?;
        let content = directory.join("content");
        fs::create_dir(&content)?;
        Ok(Self {
            directory,
            content,
            owner,
        })
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        if fs::read_to_string(self.directory.join("OWNER"))
            .ok()
            .as_deref()
            == Some(self.owner.as_str())
        {
            // Only this freshly generated staging tree, never a user's original directory.
            if let Err(error) = fs::remove_dir_all(&self.directory) {
                tracing::warn!(
                    event = "archive_staging_cleanup_failed",
                    stage = "owned_directory",
                    error_type = "io",
                    error_code = error.raw_os_error(),
                    "本次归档暂存目录清理失败"
                );
            }
            if let Some(parent) = self.directory.parent() {
                if let Err(error) = fs::remove_dir(parent) {
                    if error.kind() != std::io::ErrorKind::DirectoryNotEmpty
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(
                            event = "archive_staging_cleanup_failed",
                            stage = "work_directory",
                            error_type = "io",
                            error_code = error.raw_os_error(),
                            "归档工作空目录清理失败"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 覆盖 H-06：已知祖先不在 Git 树内，不代表后代也没有独立 Git 边界。
    #[test]
    fn git_cache_checks_descendants_of_an_allowed_directory() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let repo = parent.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        let mut boundaries = GitBoundaries::default();
        assert!(!boundaries.blocked(root.path(), &parent).unwrap());
        assert!(boundaries.blocked(root.path(), &repo).unwrap());
    }

    // 覆盖 H-06：Git 保护只向后代传播，不能错误排除 Git 树外的兄弟目录。
    #[test]
    fn git_cache_does_not_protect_siblings_of_a_repository() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("parent/repo");
        let sibling = root.path().join("parent/ordinary");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let mut boundaries = GitBoundaries::default();
        assert!(boundaries.blocked(root.path(), &repo).unwrap());
        assert!(!boundaries.blocked(root.path(), &sibling).unwrap());
    }
    use crate::config::Config;
    use crate::control::{Context as TaskContext, Control};
    use crate::db::Database;
    use crate::engine::Job;
    use anyhow::Context;

    // 覆盖 X-03（成员就地解到包所在位置，含缺失父目录）
    #[test]
    fn merge_creates_missing_parent_directories() {
        // 回归：合入成员时把「父目录」传给只创建父级的 ensure_parent，实际只创建到祖父目录，
        // 带子目录的成员改名必然报「系统找不到指定的路径」，于是含子目录的整包（RAR 的子目录成员、
        // 分卷包里的 vols/ 目录）解压失败并留下半截空目录。合入必须创建到成员的父目录。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("file1.txt");
        fs::write(&source, b"member payload").unwrap();
        let target = root.join("archives/sub/dir1/file1.txt");

        let outcome = merge_extracted(&root, &source, &target).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Placed(_)),
            "目标空闲时成员应落到目标路径"
        );
        assert_eq!(fs::read(&target).unwrap(), b"member payload");
        assert!(!source.exists(), "合入后暂存文件应已改名离开");
    }

    // 覆盖 X-04, H-07（回归：目标已有同内容文件时仍必须落盘改名副本，不得以内容相同跳过）
    #[test]
    fn merge_conflict_with_identical_bytes_lands_a_renamed_copy() {
        // 合同 X-04/H-07：冲突即使内容完全相同，也不得跳过新文件落盘、不按内容淘汰；
        // 既有文件不动，新文件改用未占用的名字。整改前这里走「等字节即视为已落位」的
        // 快捷路径：成员不落盘、extracted 虚计一次，用户拿不到本次解出的那份副本。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let target = root.join("a.txt");
        fs::write(&target, b"same bytes").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("a.txt");
        fs::write(&source, b"same bytes").unwrap();
        let outcome = merge_extracted(&root, &source, &target).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Renamed(_)),
            "目标已存在时必须改名落盘而不是按内容跳过"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"same bytes",
            "既有文件必须原样保留（H-07）"
        );
        assert_eq!(
            fs::read(root.join("a (1).txt")).unwrap(),
            b"same bytes",
            "等内容的成员仍必须落盘为改名副本（X-04/H-07）"
        );
        assert!(!source.exists(), "成员已改名离开暂存区");
    }

    // 覆盖 H-07, X-04（回归：冲突改名只改文件名主体，复合扩展名整体保留）
    #[test]
    fn merge_conflict_preserves_compound_extension() {
        // 合同 H-07 的例子：资料.tar.gz → 资料 (1).tar.gz；不得变成 资料.tar (1).gz，
        // 也不得通过更换/追加扩展名规避冲突。整改前该路径会按冲突策略删除既有文件
        // （或按策略丢弃新成员），两个断言都不成立。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let target = root.join("资料.tar.gz");
        fs::write(&target, b"old package").unwrap();
        let stage = tempfile::tempdir().unwrap();
        let source = stage.path().join("资料.tar.gz");
        fs::write(&source, b"brand new package content").unwrap();
        let outcome = merge_extracted(&root, &source, &target).unwrap();
        assert!(
            matches!(outcome, MergeOutcome::Renamed(_)),
            "目标已存在时必须改名落盘（H-07）"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"old package",
            "既有文件不得被覆盖或删除（H-07/S-01）"
        );
        assert_eq!(
            fs::read(root.join("资料 (1).tar.gz")).unwrap(),
            b"brand new package content",
            "冲突改名必须保留复合扩展名（H-07）"
        );
        assert!(
            !root.join("资料.tar (1).gz").exists(),
            "不得把复合扩展名拆开改名（H-07）"
        );
    }

    #[test]
    fn enqueue_replaces_pending_row_for_same_rel() {
        // 回归：archives 表唯一键在 fingerprint 上；同一路径的压缩包被另一个包的成员
        // 处理并按规则删除该包后，新行 snapshot 必然失败，把「解压失败 N 包」与错误
        // 计数虚高。入队时应先清同 rel 的未处理旧行。
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let db_dir = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let pack = root.join("pack.zip");
        fs::write(&pack, b"first version bytes").unwrap();

        let job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&db_dir).unwrap(),
            summary: crate::model::Summary::default(),
        };
        enqueue(&job, &pack, 0).unwrap();
        // 同路径包内容被覆盖（大小与内容都变了）后再次入队。
        fs::write(&pack, b"second version with different length").unwrap();
        enqueue(&job, &pack, 0).unwrap();
        let rows: i64 = job
            .db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM archives WHERE rel='pack.zip'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "同一路径只应保留最新 fingerprint 的待处理行");
    }

    // 平台门禁原因：验证对象是 Windows 路径长度语义（LongPathsEnabled=0 时 >260 普通路径的
    // 裸 Win32 调用直接失败）与 \\?\ verbatim 前缀拼接，只能在 Windows 上构造。
    // 覆盖 X-08, S-04（隐藏属性剥离，避免成员成为扫描不可见的影子文件）
    #[cfg(windows)]
    #[test]
    fn normalize_strips_hidden_on_plain_long_path() {
        // 直测锚点：集成测试经 prepare_at 的 root 已 canonicalize（verbatim），裸调用恰好总是
        // 成功，锁不住「普通路径 + 超长」这一原始缺陷形态；这里显式传非 verbatim 的 >260 路径，
        // LongPathsEnabled=0 的机器上修复前（裸调用 + 吞错记成功）必失败。
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};
        let temp = tempfile::tempdir().unwrap();
        let mut dir = temp.path().to_path_buf();
        let seg = "n".repeat(40);
        for i in 0..7 {
            dir.push(format!("{seg}{i}"));
        } // 相对段 ≈300 字符，基路径短但总长 >260
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("deep-hidden.txt");
        fs::write(&file, b"payload").unwrap();
        // 植入隐藏属性：这条路径本身 >260，植入同样要走 \\?\（裸调用会以同样方式失败）。
        let mut wide: Vec<u16> = r"\\".encode_utf16().chain(r"?\".encode_utf16()).collect();
        wide.extend(file.as_os_str().encode_wide());
        wide.push(0);
        assert_ne!(
            // SAFETY: wide 是以 NUL 结尾的 UTF-16 verbatim 路径；调用只读取该缓冲区。
            unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) },
            0,
            "测试前置：植入隐藏属性失败"
        );
        let config = Config {
            include_hidden: false,
            ..Config::default()
        };
        assert!(
            normalize_new_member_attributes(&file, &config),
            "普通超长路径的属性剥离必须成功"
        );
        let attrs = fs::symlink_metadata(&file).unwrap().file_attributes();
        assert_eq!(attrs & FILE_ATTRIBUTE_HIDDEN, 0, "隐藏属性必须被剥离");
    }

    // 覆盖 X-06, X-08（回归：空间不足是任务级中止，不得再走隔离；取消与包级失败口径不变）
    #[test]
    fn failure_classification_separates_stop_from_quarantine() {
        // 空间不足必须被识别为「停止整个解压任务」：即使被 with_context 包装（解压
        // 回调的实际形态）也要命中——判定走错误链，而不是顶层 downcast；否则空间不足
        // 又会退回「逐包隔离并继续处理后续包」。
        let space: anyhow::Error = StopExtraction("可用空间不足：本包需 1 GiB".into()).into();
        assert!(stops_extraction(&space), "空间不足必须停止整个任务（X-08）");
        let wrapped = Err::<(), _>(StopExtraction("磁盘剩余空间低于预留阈值".into()))
            .context("解压失败（可能已损坏、加密或格式不受支持）")
            .unwrap_err();
        assert!(
            stops_extraction(&wrapped),
            "被 with_context 包装后仍必须识别（解压回调的实际形态）"
        );
        // 防护上限属于包级失败：按 X-06 隔离，不得停止整个任务。
        let capped = anyhow::anyhow!("压缩包条目数量超过用户设置的上限");
        assert!(!stops_extraction(&capped), "防护上限走包级隔离口径（X-06）");
        // 用户取消：既不是空间不足，也不隔离——沿用 control 的取消口径。
        let control = Control::default();
        control.cancel();
        let cancelled = control.check_cancelled().unwrap_err();
        assert!(!stops_extraction(&cancelled), "取消不得被当成空间不足");
    }

    // 覆盖 X-05, X-08（取消在删除的安全边界生效）：已取消时不启动任何删除，
    // 错误沿用取消口径（不是「停止任务」信号，不隔离、不虚计成功）。
    #[test]
    fn delete_successful_source_stops_on_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let volume = root.join("pack.zip");
        fs::write(&volume, b"archive bytes").unwrap();
        let mut job = Job {
            root,
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&temp.path().join("state")).unwrap(),
            summary: crate::model::Summary::default(),
        };
        job.context.control.cancel();

        let error = delete_successful_source(&mut job, "pack.zip", std::slice::from_ref(&volume))
            .unwrap_err();
        assert!(
            !stops_extraction(&error),
            "取消沿用取消口径，不得被当成任务级中止信号处理：{error:#}"
        );
        assert!(volume.is_file(), "取消后不得启动任何删除");
        assert_eq!(job.summary.deleted, 0, "取消后不得虚计删除");
    }

    // 覆盖 X-10（回归：数字尾卷族按「名称以恰好三位 .NNN 结尾」识别入口，
    // 复合扩展名（如 .tar.gz.001）的卷集同样成组，不再只认 .7z.001/.zip.001）
    #[test]
    fn volume_set_groups_compound_numbered_volumes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("资料.tar.gz.001"), b"vol1").unwrap();
        fs::write(root.join("资料.tar.gz.002"), b"vol2").unwrap();
        // 完整主包名自身不加入数字尾卷族（X-10），也不得被吸入卷集。
        fs::write(root.join("资料.tar.gz"), b"standalone").unwrap();
        let set = volume_set(&root.join("资料.tar.gz.001")).unwrap();
        assert_eq!(set.scheme, VolumeScheme::Numbered);
        assert_eq!(set.paths.len(), 2, "同主干 .001/.002 应识别为同一卷集");
    }

    // 覆盖 X-10（回归：普通扩展名 + .NNN 同样按数字尾卷族识别，主干为去掉 .NNN）
    #[test]
    fn volume_set_recognizes_any_three_digit_numbered_entry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("x.rar.001"), b"vol1").unwrap();
        fs::write(root.join("x.rar.002"), b"vol2").unwrap();
        let set = volume_set(&root.join("x.rar.001")).unwrap();
        assert_eq!(set.scheme, VolumeScheme::Numbered);
        assert_eq!(set.paths.len(), 2);
    }
    // 覆盖 X-10（回归：.000 即使与其他分卷同组也必须在引擎解析前按非法起始卷拒绝）。
    #[test]
    fn numbered_volume_zero_is_rejected_by_precheck() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let zero = root.join("x.7z.000");
        fs::write(&zero, b"invalid start").unwrap();
        fs::write(root.join("x.7z.002"), b"later volume").unwrap();

        let named = volume_set(&zero).unwrap();
        assert!(
            x10_volume_precheck("x.7z.000", &zero, &named).is_err(),
            ".000 是可识别的非法起始卷，必须在引擎解析前走整组失败隔离（X-10）"
        );

        let second = root.join("x.7z.002");
        let named = volume_set(&second).unwrap();
        assert!(
            x10_volume_precheck("x.7z.002", &second, &named).is_err(),
            "从后续卷检查时也必须识别同组的非法 .000 起始卷（X-10）"
        );
    }

    // 覆盖 X-10/E-05（同名目录不是老式分卷族的主包文件，缺引擎时也须在预检拒绝）。
    #[test]
    fn old_style_main_directory_does_not_satisfy_precheck() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let main = root.join("x.rar");
        fs::create_dir(&main).unwrap();
        let tail = root.join("x.r00");

        assert!(
            missing_old_style_main(&tail),
            "同名目录不能代替老式分卷族的主包文件（X-10/E-05）"
        );

        fs::remove_dir(&main).unwrap();
        fs::write(&main, b"main archive").unwrap();
        assert!(
            !missing_old_style_main(&tail),
            "实际普通文件主包应满足老式分卷族入口检查"
        );
    }

    // 覆盖 X-10（回归：混用最小补零宽度即使卷号不同也属于命名歧义）。
    #[test]
    fn part_rar_mixed_padding_widths_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("x.part01.rar"), b"padded first volume").unwrap();
        let second = root.join("x.part2.rar");
        fs::write(&second, b"unpadded second volume").unwrap();

        let named = volume_set(&second).unwrap();
        assert!(
            named.part_ambiguity.is_some(),
            "part01 与 part2 混用补零模式，即使卷号不同也必须识别为歧义（X-10）"
        );
        assert!(
            x10_volume_precheck("x.part2.rar", &second, &named).is_err(),
            "命名歧义组必须在引擎解析前按整组失败处置（X-10）"
        );
    }

    /// F03 夹具：所选根下已有「解压失败」容器且容器是 Git 项目（.git 由 kind 决定
    /// 目录或文件形态），再放一个坏包，调用隔离并断言按「隔离失败」原地保留。
    fn quarantine_git_container_case(kind: &str) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let container = root.join(QUARANTINE_DIR_NAME);
        if kind == "dir" {
            fs::create_dir_all(container.join(".git")).unwrap();
        } else {
            fs::create_dir_all(&container).unwrap();
            fs::write(container.join(".git"), b"gitdir: elsewhere\n").unwrap();
        }
        let pack = root.join("pack.zip");
        fs::write(&pack, b"broken archive bytes").unwrap();
        let mut job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&temp.path().join("state")).unwrap(),
            summary: crate::model::Summary::default(),
        };
        let error = quarantine(&mut job, "pack.zip", "测试：无法解开的包").unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("Git"),
            "隔离失败原因必须说明 Git 整树保护（H-06）：{text}"
        );
        assert!(pack.is_file(), "坏包必须原地保留（隔离失败，不腾挪不覆盖）");
        assert!(
            container.join(".git").exists(),
            "Git 项目树不得被触碰（H-06）"
        );
        // 原包没有移入容器：容器内除 .git 外不得出现任何新条目。
        let extra: Vec<_> = fs::read_dir(&container)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != ".git")
            .collect();
        assert!(
            extra.is_empty(),
            "隔离失败时不得向 Git 容器写入任何条目：{extra:?}"
        );
    }

    // 覆盖 H-06（回归：「解压失败」容器自身是 Git 项目（.git 为目录）时，
    // 坏包不得移入其中；修复前容器规划只拒链接/同名文件，失败包被腾入 Git 树）
    #[test]
    fn quarantine_refuses_git_project_container_directory() {
        quarantine_git_container_case("dir");
    }

    // 覆盖 H-06（回归：同上，.git 为文件形态——is_git_root 两种形态都保护整树）
    #[test]
    fn quarantine_refuses_git_project_container_file() {
        quarantine_git_container_case("file");
    }
    // 覆盖 X-09/X-10：相似但不属于精确卷族的文件不得加入隔离集合。
    #[test]
    fn review_exact_volume_families_exclude_wide_numeric_suffixes() {
        let temp = tempfile::tempdir().unwrap();
        for (entry, included, excluded) in [
            ("a.zip", "a.z01", "a.z001"),
            ("b.rar", "b.r00", "b.r000"),
            ("c.7z.001", "c.7z.002", "c.7z.1000"),
        ] {
            for name in [entry, included, excluded] {
                fs::write(temp.path().join(name), b"volume").unwrap();
            }
            let set = volume_set(&temp.path().join(entry)).unwrap();
            assert!(set.paths.contains(&temp.path().join(included)));
            assert!(
                !set.paths.contains(&temp.path().join(excluded)),
                "非精确卷族文件不得隔离：{excluded}"
            );
        }
    }

    // 覆盖 X-04/H-06：普通目录若是 Git 项目，应为新目录另选名字。
    #[test]
    fn review_directory_collision_with_git_tree_uses_new_name() {
        let root = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("repo/.git")).unwrap();
        fs::write(root.path().join("repo/keep.txt"), b"original").unwrap();
        fs::create_dir_all(stage.path().join("repo/sub")).unwrap();
        let renames =
            plan_directory_renames(&Control::default(), stage.path(), root.path()).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, "repo/sub/member.txt"),
            Some("repo (1)/sub/member.txt".to_string())
        );
        assert_eq!(
            fs::read(root.path().join("repo/keep.txt")).unwrap(),
            b"original"
        );
        assert!(!root.path().join("repo/sub").exists());
        fs::create_dir(root.path().join(QUARANTINE_DIR_NAME)).unwrap();
        fs::write(root.path().join("解压失败/old.zip"), b"old user archive").unwrap();
        fs::create_dir(stage.path().join(QUARANTINE_DIR_NAME)).unwrap();
        let renames =
            plan_directory_renames(&Control::default(), stage.path(), root.path()).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, "解压失败/new.zip"),
            Some("解压失败 (1)/new.zip".to_string()),
            "新目录不得向已有隔离容器合入"
        );
        assert_eq!(
            fs::read(root.path().join("解压失败/old.zip")).unwrap(),
            b"old user archive"
        );
    }

    // 覆盖 X-04：祖先改名必须传递至任意深度的占用规划。
    #[test]
    fn review_directory_mapping_propagates_through_unrenamed_parents() {
        let root = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        fs::write(root.path().join("a"), b"occupier").unwrap();
        fs::create_dir_all(stage.path().join("a/b/c")).unwrap();
        let renames =
            plan_directory_renames(&Control::default(), stage.path(), root.path()).unwrap();
        assert_eq!(
            mapped_entry_rel(&renames, "a/b/c/member.txt"),
            Some("a (1)/b/c/member.txt".to_string())
        );
    }

    // 覆盖 X-10：非 part 名字中的数字不能授权吸入另一个独立 rar。
    #[test]
    fn review_rar_numeric_stem_does_not_authorize_other_archives() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("report1.rar");
        fs::write(&archive, b"first").unwrap();
        fs::write(root.path().join("report2.rar"), b"independent").unwrap();
        let info = ArchiveVolumes {
            kind: "rar5".to_string(),
            count: 2,
            new_rar_names: true,
        };
        assert!(
            VolumeScheme::OldRar.actual_paths(&archive, &info).is_err(),
            "非精确族的第二个独立包不得成为删源集合"
        );
        assert_eq!(
            fs::read(root.path().join("report2.rar")).unwrap(),
            b"independent"
        );
    }

    // 覆盖 X-10：冲突序号不能以截断分卷后缀来腾位。
    #[test]
    fn review_quarantine_preserves_long_volume_suffix_or_fails() {
        let root = tempfile::tempdir().unwrap();
        let ext = format!(".part{}.rar", "1".repeat(245));
        fs::write(root.path().join(format!("a{ext}")), b"occupier").unwrap();
        assert!(
            quarantine_targets(root.path(), &[("a".to_string(), ext)]).is_err(),
            "不能完整保留后缀时必须安全失败"
        );
    }

    // Windows 项目既有脚本引擎夹具惯例：只模拟输出，检查真实落盘/删源接口。
    fn review_script_engine(root: &Path, listing: &str, contents: &str) -> SevenZip {
        let path = root.join("review-engine.cmd");
        let body = format!(
            "@echo off\r\nif \"%~1\"==\"l\" (\r\n{listing}\r\nexit /b 0\r\n)\r\n\
             :args\r\nif \"%~1\"==\"\" exit /b 1\r\nset \"arg=%~1\"\r\n\
             if \"%arg:~0,2%\"==\"-o\" goto extract\r\nshift\r\ngoto args\r\n\
             :extract\r\nset \"output=%arg:~2%\"\r\n{contents}\r\nexit /b 0\r\n"
        );
        fs::write(&path, body).unwrap();
        SevenZip::with_executable(&path).unwrap()
    }

    fn review_job(root: &Path, state: &Path, max_ratio: u64) -> Job {
        Job {
            root: root.to_path_buf(),
            config: Config {
                reserve_bytes: 0,
                max_ratio,
                ..Config::default()
            },
            context: TaskContext::default(),
            db: Database::create(state).unwrap(),
            summary: crate::model::Summary::default(),
        }
    }

    // 覆盖 X-08：未知Size仍按实际解码逻辑量限制比例，不得绕过防护并删源。
    #[test]
    fn review_unknown_size_stream_obeys_expansion_ratio() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("pack.gz"), b"x").unwrap();
        let engine = review_script_engine(
            temp.path(),
            "echo Path = payload.txt\r\necho Packed Size = 1\r\necho.",
            "echo 0123456789> \"%output%\\payload.txt\"",
        );
        let mut job = review_job(&root, &temp.path().join("state"), 1);
        assert!(engine.extract_one(&mut job, "pack.gz", 0).is_err());
        assert!(root.join("pack.gz").is_file(), "超比例不删源");
        assert!(!root.join("payload.txt").exists(), "检查完成前不得合入成员");
    }

    // 覆盖 X-08：整包实际逻辑量不符必须先拒绝，不能合入安全子集。
    #[test]
    fn review_staging_size_validation_precedes_every_merge() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("pack.7z"), b"x").unwrap();
        let engine = review_script_engine(
            temp.path(),
            "echo Path = payload.txt\r\necho Size = 99\r\necho.",
            "echo short> \"%output%\\payload.txt\"",
        );
        let mut job = review_job(&root, &temp.path().join("state"), 0);
        assert!(engine.extract_one(&mut job, "pack.7z", 0).is_err());
        assert!(root.join("pack.7z").is_file());
        assert!(
            !root.join("payload.txt").exists(),
            "总量检查失败的包不得合入任何成员"
        );
    }

    // 覆盖 X-10：精确命名族缺中间卷时，解码前直接拒绝整组。
    #[test]
    fn review_exact_volume_gaps_are_rejected_before_engine() {
        let root = tempfile::tempdir().unwrap();
        for (first, later) in [
            ("numbered.7z.001", "numbered.7z.003"),
            ("part.part1.rar", "part.part3.rar"),
            ("old.rar", "old.r01"),
            ("zip.zip", "zip.z02"),
        ] {
            let archive = root.path().join(first);
            fs::write(&archive, b"first").unwrap();
            fs::write(root.path().join(later), b"later").unwrap();
            let named = volume_set(&archive).unwrap();
            assert!(
                x10_volume_precheck(first, &archive, &named).is_err(),
                "中间断号必须拒绝：{first} / {later}"
            );
        }
    }

    // 覆盖 S-05：只读目标判定不得把系统属性“包含”解释成允许写入系统目录。
    #[test]
    fn review_system_directory_destination_is_excluded() {
        let protected = fsutil::protected_root().unwrap();
        let root = protected.parent().unwrap();
        let destination = protected.join("jchtools-review-never-created.txt");
        assert!(
            excluded_destination(
                root,
                &destination,
                &Config::default(),
                &mut GitBoundaries::default(),
            )
            .unwrap(),
            "实际系统目录及其后代必须排除；本测试只读路径，不创建任何系统文件"
        );
    }

    // 覆盖 H-06：归档内同目录的大小写变体仍属于同一 Git 排除子树。
    #[test]
    fn review_git_subtree_matches_windows_case_variants() {
        let subtrees = HashSet::from([git_boundary_prefix("project/.GIT/config").unwrap()]);
        assert!(inside_git_subtree(&subtrees, "PROJECT/README.md"));
        assert!(inside_git_subtree(&subtrees, "Project/src/member.txt"));
        assert!(!inside_git_subtree(&subtrees, "project-other/README.md"));
    }

    // X-10：Unicode 小写扩展不是 Windows 序数等价，独立族不得参与彼此删源。
    #[test]
    fn review_ordinal_volume_families() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("i\u{0307}.zip.001");
        let unrelated = root.path().join("\u{0130}.zip.002");
        fs::write(&first, b"selected volume").unwrap();
        fs::write(&unrelated, b"independent user volume").unwrap();
        assert_ne!(
            fsutil::fold_rel("i\u{0307}.zip"),
            fsutil::fold_rel("\u{0130}.zip"),
        );
        assert_eq!(volume_set(&first).unwrap().paths, vec![first]);
        assert_eq!(fs::read(unrelated).unwrap(), b"independent user volume");
        let one = root.path().join("σ.zip.001");
        let two = root.path().join("ς.zip.002");
        fs::write(&one, b"first").unwrap();
        fs::write(&two, b"second").unwrap();
        assert_eq!(volume_set(&one).unwrap().paths, vec![one, two]);
    }

    #[test]
    fn review_quarantine_common_long_family_stem() {
        let dir = tempfile::tempdir().unwrap();
        let stem = "x".repeat(244);
        let names = vec![
            (stem.clone(), ".part1.rar".to_string()),
            (stem.clone(), ".part10.rar".to_string()),
        ];
        let old = dir.path().join(format!("{stem}.part1.rar"));
        fs::write(&old, b"old user archive").unwrap();
        let targets = quarantine_targets(dir.path(), &names).unwrap().unwrap();
        let one = targets[0].file_name().unwrap().to_str().unwrap();
        let two = targets[1].file_name().unwrap().to_str().unwrap();
        assert!(one.ends_with(".part1.rar"));
        assert!(two.ends_with(".part10.rar"));
        assert_eq!(
            fsutil::split_compound_name(one).0,
            fsutil::split_compound_name(two).0
        );
        assert_eq!(fs::read(old).unwrap(), b"old user archive");
    }

    #[test]
    fn review_placement_common_long_family_stem() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let content = temp.path().join("content");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&content).unwrap();
        let stem = "x".repeat(244);
        let one = format!("{stem}.part1.rar");
        let two = format!("{stem}.part10.rar");
        fs::write(content.join(&one), b"new first").unwrap();
        fs::write(content.join(&two), b"new tenth").unwrap();
        fs::write(root.join(&one), b"old user archive").unwrap();
        let job = review_job(&root, &temp.path().join("task.sqlite3"), 0);
        let targets = plan_volume_renames(
            &job,
            &content,
            &root,
            &HashMap::new(),
            &rules::build_exclusions("").unwrap(),
            &mut GitBoundaries::default(),
        )
        .unwrap();
        let first = targets
            .get(&one)
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let tenth = targets
            .get(&two)
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(first.ends_with(".part1.rar"));
        assert!(tenth.ends_with(".part10.rar"));
        assert_eq!(
            fsutil::split_compound_name(first).0,
            fsutil::split_compound_name(tenth).0
        );
        assert_eq!(fs::read(root.join(&one)).unwrap(), b"old user archive");
    }
}
