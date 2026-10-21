#[cfg(not(windows))]
use super::boundaries::has_hidden_component;
use super::boundaries::{
    excluded_destination, git_boundary_prefix, inside_git_subtree, member_excluded,
    DestinationParents, GitBoundaries,
};
use super::names::{staged_files, DecodedFileName, DecoderNames, Listing};
use super::observe::{member_rejected, observe_archive, stops_extraction, StopExtraction};
use super::placement::{
    mapped_entry_rel, member_unique_target, merge_extracted, plan_directory_renames,
    plan_volume_renames, MergeOutcome,
};
use super::queue::{delete_successful_source, normalize_new_member_attributes};
use super::space::{
    check_expansion_space, skip_git_root, validate_staging, volume_bytes, volume_scope_allowed,
};
#[cfg(windows)]
use super::space::{engine_space_error_line, engine_space_messages};
use super::staging::Staging;
use super::volumes::{
    composite_stream, compressed_stream_kind, enqueue_nested_groups, potential_archive_member,
    stream_member_name, volume_set, x10_volume_precheck, ArchiveVolumes, VolumeScheme,
};
use anyhow::{bail, ensure, Context, Result};
use jchtools_file_core::{
    fsutil,
    model::bytes,
    process, rules,
    task::{Job, QUARANTINE_DIR_NAME},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub struct SevenZip {
    pub(super) executable: PathBuf,
}
impl SevenZip {
    pub fn from_bundle() -> Result<Self> {
        // 随包目录与内嵌引擎统一走 resolve_executable：随包路径与 AGENTS §3.1 一致
        // 仅警告放行（LGPL 允许替换外部 7-Zip 引擎），内嵌释放路径保留硬校验。
        // 不再对随包路径做硬哈希 bail——那会拒绝用户自备/替换的引擎，与 LGPL 冲突。
        Ok(Self {
            executable: jchtools_file_core::engine_bundle::resolve_executable()?,
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
    pub(super) fn list(&self, archive: &Path, job: &mut Job) -> Result<Listing> {
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
                    // X-08：根目录本身无需落盘，但仍是归档列出的条目，重复根条目同样计数。
                    count = count.checked_add(1).context("条目计数溢出")?;
                    if count > cfg.max_entries {
                        member_rejected("entry_limit", &raw);
                        bail!("压缩包条目数量超过用户设置的上限");
                    }
                    if raw == "." || raw == "./" {
                        fields.clear();
                        return Ok(());
                    }
                    let relative = fsutil::safe_relative(&raw).inspect_err(|_| {
                        member_rejected("invalid_member_path", &raw);
                    })?;
                    let raw = fsutil::path_string(&relative)?.replace('\\', "/");
                    if raw.split('/').any(|s| {
                        s.eq_ignore_ascii_case("$RECYCLE.BIN")
                            || s.eq_ignore_ascii_case("System Volume Information")
                    }) {
                        member_rejected("protected_member_path", &raw);
                        bail!("拒绝压缩包中的系统目录条目：{raw}");
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
                    total = total.checked_add(size).context("解压总大小溢出")?;
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
                tracing::debug!(
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
                tracing::debug!(
                    event = "archive_volume_result",
                    count = info.count,
                    "归档实际分卷数量"
                );
            }
            result
        })
    }
    fn decode_into(
        &self,
        job: &Job,
        archive: &Path,
        output: &Path,
        label: &str,
    ) -> Result<HashMap<String, DecodedFileName>> {
        let control = job.context.control.clone();
        observe_archive("decode", &control, |boundary_stage| {
            *boundary_stage = "decoder_process_and_space";
            let mut names = DecoderNames::default();
            #[cfg(windows)]
            let space_messages = engine_space_messages();
            #[cfg(windows)]
            let mut system_error = false;
            let mut command = self.command();
            command
                .args([
                    "x",
                    "-aou",
                    "-y",
                    "-bb1",
                    "-bsp2",
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
                    #[cfg(windows)]
                    if err {
                        if let Some(error) =
                            engine_space_error_line(&mut system_error, &space_messages, line)
                        {
                            return Err(error)
                                .context("7-Zip 报告目标写入空间不足；保留源包并停止本次任务");
                        }
                    }
                    if !err {
                        if let Some(member) = line.strip_prefix("- ") {
                            return names.record(member);
                        }
                    }
                    if line.contains('%') {
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
            .with_context(|| "解压失败（可能已损坏、加密或格式不受支持）")?;
            for name in names.renamed.values() {
                job.context.control.checkpoint()?;
                let path = output.join(fsutil::safe_relative(&name.staged)?);
                ensure!(
                    fs::symlink_metadata(path)
                        .is_ok_and(|metadata| metadata.is_file() && !fsutil::is_link(&metadata)),
                    "解码成员改名与引擎输出不一致，未合入任何成员"
                );
            }
            Ok(names.renamed)
        })
    }

    pub(super) fn extract_one(&self, job: &mut Job, archive_rel: &str, depth: u32) -> Result<bool> {
        let control = job.context.control.clone();
        observe_archive("extract_one", &control, |boundary_stage| {
            *boundary_stage = "input_volumes";
            let archive = fsutil::safe_join(&job.root, archive_rel)?;
            job.context.status(format!("检查压缩包：{archive_rel}"));
            // 文件名仅用于发现候选；删除与展开比例只能使用引擎实际打开的连续分卷。
            // 隔离仍使用独立的可逆宽匹配，不能把该集合复用为永久删除授权。
            let named = volume_set(&archive)?;
            if !volume_scope_allowed(job, archive_rel, &named.paths)? {
                return Ok(false);
            }
            x10_volume_precheck(archive_rel, &archive, &named)?;
            let volumes = if named.paths.len() > 1
                || matches!(named.scheme, VolumeScheme::RarParts | VolumeScheme::OldRar)
            {
                let info = self.archive_volume_count(&archive, job)?;
                named.scheme.actual_paths(&archive, &info)?
            } else {
                named.paths
            };
            ensure!(
                volume_scope_allowed(job, archive_rel, &volumes)?,
                "实际分卷超出处理范围，整组原位保留"
            );
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
            let mut stage = Staging::new(&job.root, &job.context)?;
            *boundary_stage = "decode";
            let mut decoded_names = self.decode_into(job, &archive, &stage.content, archive_rel)?;
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
                    let mut next = Staging::new(&job.root, &job.context)?;
                    decoded_names = self.decode_into(job, &member, &next.content, archive_rel)?;
                    decoded = validate_staging(
                        job,
                        &next.content,
                        (tar && listing.sizes_complete).then_some(listing.total),
                        packed,
                        decoded,
                    )?;
                    // 共用容器的清理权随最终工作区转移；失败时仍由原工作区负责。
                    // 不在解码前转移，避免失败退出时新工作区先清理、旧工作区仍占用容器。
                    if stage.remove_container && stage.directory.parent() == next.directory.parent()
                    {
                        next.remove_container = true;
                        stage.remove_container = false;
                    }
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
            let mut parents = DestinationParents::default();
            let base = archive.parent().context("压缩包缺少父目录")?;
            // X-04：目录落盘名规划。压缩包目录的默认落盘名被普通文件、链接或 junction
            // 占用时，为新目录选最小未占用序号（`目录 (1)`、`目录 (2)`），该目录及全部
            // 后代成员整体映射到新目录；既有文件一律不动，与既有普通目录同名则合入。
            // 规划必须在成员合入前完成：文件成员的父链与空目录条目共用这一映射。
            let dir_renames = plan_directory_renames(
                &job.context.control,
                &stage.content,
                base,
                &mut parents.reserved_directories,
            )?;
            let volume_renames = plan_volume_renames(
                job,
                &stage.content,
                base,
                &dir_renames,
                &decoded_names,
                &exclusions,
                &mut git,
            )?;
            // One archive is decoded once, including solid archives. Final placement is rename, never copy.
            *boundary_stage = "member_output";
            let mut nested_members: BTreeMap<PathBuf, BTreeMap<String, PathBuf>> = BTreeMap::new();
            let merge_result: Result<()> = (|| {
                for entry in staged_files(&stage.content, &decoded_names, &job.context.control) {
                    let (entry, relative) = entry?;
                    let meta = fs::symlink_metadata(entry.path())?;
                    if fsutil::is_link(&meta) {
                        bail!("解压结果出现链接，已停止合入");
                    }
                    if !meta.is_file() {
                        bail!("解压结果含非普通文件");
                    }
                    let decoded_name = decoded_names.get(&fsutil::fold_rel(&relative));
                    let original =
                        decoded_name.map_or(relative.as_str(), |name| name.original.as_str());
                    let mut destination = base.join(fsutil::safe_relative(original)?);
                    // X-04：祖先目录因被既有文件占用而整体改名时，成员落盘路径跟随映射。
                    if let Some(mapped) = mapped_entry_rel(&dir_renames, original) {
                        destination = base.join(fsutil::safe_relative(&mapped)?);
                    }
                    if let Some(target) = volume_renames.get(&relative) {
                        destination.clone_from(target);
                    }
                    // 与源包同名的成员（含 gzip 头记录的原名）也只按 H-07 改主体，保留完整后缀：
                    // 绝不覆盖仍在使用的源包，也不通过剥掉压缩扩展名改变后续嵌套处理。
                    // 无成员名的流已在清单/解码阶段使用去掉一层压缩后缀的默认名。
                    // 路径相等判断仅在 Windows 上忽略大小写（NTFS 不区分）；其他平台区分大小写。
                    // 与 Windows 文件系统相同的序数键比较，Unicode lowercase 会错误展开主体。
                    let collides_with_source = if cfg!(windows) {
                        fsutil::fold_rel(&fsutil::path_string(&destination)?)
                            == fsutil::fold_rel(&fsutil::path_string(&archive)?)
                    } else {
                        destination == archive
                    };
                    if collides_with_source {
                        destination = member_unique_target(
                            &job.root,
                            &destination,
                            &parents.reserved_directories,
                        )?;
                    }
                    let destination_rel = fsutil::relative_string(&job.root, &destination)?;
                    // 最终名的冲突由 merge_extracted 改名；父链先校验但不创建，
                    // 同包后续成员复用已经检查过的祖先。
                    parents.validate(&job.root, destination.parent().context("目标缺少父目录")?)?;
                    if inside_git_subtree(&git_subtrees, original) {
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
                        match merge_extracted(&job.root, entry.path(), &destination, &mut parents)?
                        {
                            MergeOutcome::Placed(path) => (path, false),
                            MergeOutcome::Renamed(path) => (path, true),
                        };
                    job.summary.extracted += 1;
                    let final_rel = fsutil::relative_string(&job.root, &final_path)?;
                    // X-08：继续处理本次解出的嵌套压缩包（层数上限沿用 max_depth）；
                    // H-06：落点若位于 Git 目录树内则不处理该包（该树整树排除，不解压）。
                    if potential_archive_member(
                        final_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .context("成员文件名无效")?,
                    ) && !git
                        .blocked(&job.root, final_path.parent().context("成员缺少父目录")?)?
                    {
                        let excluded_tree = original
                            .split('/')
                            .chain(final_rel.split('/'))
                            .any(|component| component.eq_ignore_ascii_case(QUARANTINE_DIR_NAME));
                        let below_root =
                            final_path.parent().is_some_and(|parent| parent != job.root);
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
                            let parent =
                                final_path.parent().context("成员缺少父目录")?.to_path_buf();
                            let name = final_path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .context("成员文件名无效")?
                                .to_ascii_lowercase();
                            nested_members
                                .entry(parent)
                                .or_default()
                                .insert(name, final_path.clone());
                        }
                    }
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
                        parents.validate(&job.root, dest.parent().context("目标缺少父目录")?)?;
                        // 只匹配真实无尾分隔符路径及其真实祖先，与扫描剪枝口径一致。
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
                            if let Err(error) = parents.ensure(&job.root, &dest) {
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
                            continue;
                        }
                        // 文件与空目录共用同一缓存；既有目录也要首次检查链接身份。
                        parents.validate(&job.root, &dest)?;
                        // 容器名也是合法普通成员名：一旦作为正式目录合入，即使为空也不能清理。
                        // 暂存容器直属所选根，名称仅含 ASCII；比较沿用 Windows 的大小写语义。
                        if stage.remove_container
                            && stage
                                .directory
                                .parent()
                                .and_then(Path::file_name)
                                .and_then(|name| name.to_str())
                                .is_some_and(|name| {
                                    root_rel == name
                                        || (cfg!(windows) && root_rel.eq_ignore_ascii_case(name))
                                })
                        {
                            stage.remove_container = false;
                        }
                    }
                }
                Ok(())
            })();
            // 已发布集合跨越合入错误存活；取消/空间故障不启动后续嵌套包。
            if !job.context.control.is_cancelled()
                && !merge_result.as_ref().is_err_and(stops_extraction)
            {
                *boundary_stage = "nested_queue";
                if let Err(queue_error) = enqueue_nested_groups(job, nested_members, depth + 1) {
                    return match merge_result {
                        Ok(()) => Err(queue_error),
                        Err(error) => {
                            Err(error.context(format!("已发布嵌套包登记失败：{queue_error:#}")))
                        }
                    };
                }
            }
            merge_result?;
            // X-05：只有整包解码与校验成功、全部成员（含空目录）完整落盘后，才永久删除
            // 原包及其实际分卷；失败、部分解开或取消都不启动删除，也不删除仅同主干的文件。
            if complete {
                *boundary_stage = "source_delete";
                delete_successful_source(job, archive_rel, &volumes)?;
            }
            // 「解压成功」包数由调用方按 complete 口径累加：未完全解开的包要计入失败
            // 并移入「解压失败」（X-06），不得在解压层无条件先记一次成功。
            tracing::debug!(
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
