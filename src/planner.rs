use crate::{
    config::{ClassifyMode, DeleteMode, DuplicateAction},
    db::FILE_COLUMNS,
    engine::Job,
    fsutil,
    model::{Action, ActionKind, FileRecord},
    rules,
};
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};

fn action(file: &FileRecord, kind: ActionKind, reason: &str, mode: DeleteMode) -> Action {
    Action {
        id: 0,
        kind,
        source: file.rel.clone(),
        target: None,
        reason: reason.into(),
        expected: Some(file.snapshot.clone()),
        keeper: None,
        hash: file.hash.clone(),
        mode,
        selected: true,
        state: "pending".into(),
    }
}
fn remove_candidate(
    job: &mut Job,
    file: &FileRecord,
    keeper: Option<&FileRecord>,
    reason: &str,
    mode: DeleteMode,
    hardlink: bool,
) -> Result<()> {
    // Hardlink 不销毁内容，只是换名去重：即使删除方式为 Keep 也应生成计划。
    if mode == DeleteMode::Keep && !hardlink {
        return Ok(());
    }
    let mode = if hardlink && mode == DeleteMode::Keep {
        DeleteMode::Permanent
    } else {
        mode
    };
    let mut planned = action(
        file,
        if hardlink {
            ActionKind::Hardlink
        } else {
            ActionKind::Delete
        },
        reason,
        mode,
    );
    if let Some(keeper) = keeper {
        planned.keeper = Some((keeper.rel.clone(), keeper.snapshot.clone()));
    }
    job.db.add_action(&planned)?;
    job.db.deactivate_file_id(file.id)?;
    if hardlink {
        job.summary.planned_link += 1;
    } else {
        job.summary.planned_delete += 1;
    }
    // Already-hardlinked files do not represent distinct physical allocation.
    if file.snapshot.links <= 1 {
        job.summary.candidate_bytes = job
            .summary
            .candidate_bytes
            .saturating_add(file.snapshot.size);
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_cleanup", skip_all)
)]
fn cleanup_candidates(job: &mut Job) -> Result<()> {
    let mut cursor = 0;
    loop {
        let batch = job.db.files(
            &format!(
                "SELECT {FILE_COLUMNS} FROM files WHERE id>?1 AND active=1 ORDER BY id LIMIT 256"
            ),
            [cursor],
        )?;
        if batch.is_empty() {
            break;
        }
        for file in batch {
            job.context.control.checkpoint()?;
            cursor = file.id;
            if let Some((kind, reason)) =
                rules::cleanup_reason(&file.rel, file.snapshot.size, &job.config)
            {
                // C-08：三类清理各自独立覆盖删除方式，未覆盖时跟随全局文件删除方式。
                let mode =
                    rules::cleanup_delete(&job.config, kind).resolve(job.config.global_delete);
                remove_candidate(job, &file, None, reason, mode, false)?;
            }
        }
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_dedup", skip_all)
)]
fn deduplicate(job: &mut Job) -> Result<()> {
    if !job.config.dedup_same_name && !job.config.dedup_copy_names && !job.config.dedup_other_names
    {
        return Ok(());
    }
    job.context
        .status("分析相同内容：相同名称 / 副本名称 / 不同名称分别应用规则");
    let order = rules::ordering_sql(job.config.keep_duplicate);
    job.db.conn.execute_batch(&format!("DROP TABLE IF EXISTS duplicate_order; CREATE TEMP TABLE duplicate_order AS SELECT ROW_NUMBER() OVER(ORDER BY hash,{order}) AS seq,id FROM files WHERE active=1 AND hash IS NOT NULL; CREATE INDEX duplicate_order_seq ON duplicate_order(seq); DELETE FROM keepers;"))?;
    let mut cursor = 0i64;
    loop {
        // 一条 JOIN 语句取整页（duplicate_order 游标 × files 全列），替代逐候选的
        // file(id) 主键单行查询；keeper 查找保留逐行——它依赖本页内已注册的 keepers。
        // 写语句无需页内事务：engine 在 planner::build 外层已包一个整体事务。
        let items = job.db.duplicate_page(cursor, 256)?;
        if items.is_empty() {
            break;
        }
        for (seq, file) in items {
            cursor = seq;
            job.context.control.checkpoint()?;
            let hash = file.hash.as_ref().context("重复候选缺少 Hash")?;
            let keeper_id: Option<i64> = {
                let mut statement = job.db.conn.prepare_cached(
                    "SELECT file_id FROM keepers WHERE hash=?1 AND ((name=?2 AND ?4) OR (name<>?2 AND normal=?3 AND ?5) OR (name<>?2 AND normal<>?3 AND ?6)) ORDER BY rowid LIMIT 1")?;
                statement
                    .query_row(
                        params![
                            hash,
                            file.name,
                            file.normalized,
                            job.config.dedup_same_name,
                            job.config.dedup_copy_names,
                            job.config.dedup_other_names
                        ],
                        |r| r.get(0),
                    )
                    .optional()?
            };
            if let Some(keeper_id) = keeper_id {
                let keeper = job.db.file(keeper_id)?;
                // C-04：只有可靠标识 + 两侧链接数证明是同一物理文件时才跳过；标识退化
                // （如 Windows 卷不提供索引）时不得据此跳过去重，也不得重复建链。
                if rules::identity_proves_same_file(&keeper, &file) {
                    job.log(
                        "去重",
                        &file.rel,
                        &keeper.rel,
                        "保留",
                        "已经是同一个文件的硬链接，不重复计算可释放空间",
                        file.snapshot.size,
                    )?;
                    continue;
                }
                let reason = if file.name == keeper.name {
                    "相同名称且完整 Hash 相同"
                } else if file.normalized == keeper.normalized {
                    "副本命名且完整 Hash 相同"
                } else {
                    "名称不同但完整 Hash 相同"
                };
                let mode = job
                    .config
                    .duplicate_delete
                    .resolve(job.config.global_delete);
                let mut hardlink = job.config.duplicate_action == DuplicateAction::Hardlink;
                // 跨卷硬链接在执行期必然失败，规划阶段就降级为删除，避免计划与结果不符。
                if hardlink {
                    let vol =
                        |id: &str| -> String { id.split(':').next().unwrap_or("").to_string() };
                    let same_volume =
                        vol(&keeper.snapshot.identity) == vol(&file.snapshot.identity);
                    if !same_volume {
                        hardlink = false;
                        if mode == DeleteMode::Keep {
                            job.log(
                                "去重",
                                &file.rel,
                                &keeper.rel,
                                "跳过",
                                "跨卷无法硬链接，且删除方式为保留",
                                file.snapshot.size,
                            )?;
                        } else {
                            job.log(
                                "去重",
                                &file.rel,
                                &keeper.rel,
                                "降级",
                                "跨卷无法硬链接，改为按删除规则处理",
                                file.snapshot.size,
                            )?;
                        }
                    }
                }
                // 清理命中且该类清理的删除方式为「保留」的文件由清理规则管辖（保留承诺）：
                // cleanup 阶段已让其保持 active，这里若无守卫，同组 keeper 先注册时它会按
                // duplicate_delete 被删，结果随排序翻转。
                if rules::cleanup_reason(&file.rel, file.snapshot.size, &job.config).is_some() {
                    job.log(
                        "去重",
                        &file.rel,
                        &keeper.rel,
                        "跳过",
                        "文件命中清理规则且清理方式为保留；不按重复规则删除",
                        file.snapshot.size,
                    )?;
                    continue;
                }
                remove_candidate(job, &file, Some(&keeper), reason, mode, hardlink)?;
                if mode != DeleteMode::Keep {
                    job.db.mark_cleanable(keeper_id)?;
                }
            } else if rules::cleanup_reason(&file.rel, file.snapshot.size, &job.config).is_none() {
                // 清理命中文件即使该类的删除方式为「保留」（remove_candidate 直接返回、文件仍 active=1）
                // 也不得进入 keepers 成为去重唯一保留者：否则正常副本反被删除，只留下垃圾文件。
                job.db
                    .insert_keeper(file.id, hash, &file.name, &file.normalized)?;
            }
        }
    }
    Ok(())
}

fn directory_target(job: &Job, file: &FileRecord, under_output: bool) -> Result<PathBuf> {
    let original = Path::new(&file.rel);
    let mut parent = original.parent().unwrap_or(Path::new("")).to_path_buf();
    // 已在输出目录之下的文件不再参与合并与扁平化：输出目录内的分类子目录与树中
    // 同名外部目录重名时，合并会把已归类文件拉回外部目录，下一轮归类又移回来，
    // 跨运行往复移动、计划永不收敛（under_output 只在此处豁免，不影响归类跳过逻辑）。
    if job.config.merge_directories && !parent.as_os_str().is_empty() && !under_output {
        let mut merged_parent = None;
        for ancestor in parent.ancestors() {
            let Some(name) = ancestor.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let mut statement = job
                .db
                .conn
                .prepare("SELECT rel FROM directories WHERE name=?1 ORDER BY depth,rel")?;
            let candidates = statement
                .query_map([name.to_lowercase()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // 目录名入库时已统一小写（供 Windows 大小写折叠匹配）；非 Windows 大小写
            // 敏感，必须按 rel 的实际文件名精确比对，否则 Photos/photos 会被误并。
            let candidate = candidates.into_iter().find(|rel| {
                cfg!(windows) || Path::new(rel).file_name().and_then(|n| n.to_str()) == Some(name)
            });
            if let Some(candidate) = candidate {
                let dest = Path::new(&candidate);
                if dest != ancestor && !dest.starts_with(ancestor) {
                    merged_parent = Some(dest.join(parent.strip_prefix(ancestor)?));
                    break;
                }
            }
        }
        if let Some(merged) = merged_parent {
            parent = merged;
        }
    }
    if job.config.flatten_single_child && !under_output {
        loop {
            if parent.as_os_str().is_empty() {
                break;
            }
            let current = fsutil::safe_join(&job.root, &fsutil::path_string(&parent)?)?;
            // 单个目录读取失败不应中止整个规划，跳过该文件的扁平化即可。
            let entries = match std::fs::read_dir(&current) {
                Ok(rd) => rd
                    .take(2)
                    .collect::<std::io::Result<Vec<_>>>()
                    .unwrap_or_default(),
                Err(_) => break,
            };
            if entries.len() != 1 {
                break;
            }
            parent = parent.parent().unwrap_or(Path::new("")).to_path_buf();
        }
    }
    Ok(parent)
}
/// `path` 是否已位于 `prefix` 之下（逐段比较；Windows 目录不区分大小写，按 Unicode 折叠，
/// 与 archive.rs 的 NTFS 口径一致——ASCII 折叠会把非 ASCII 仅大小写不同的路径当成两条）。
fn under_path(path: &Path, prefix: &Path) -> bool {
    let mut rest = path.components();
    prefix.components().all(|part| {
        rest.next().is_some_and(|next| {
            if cfg!(windows) {
                next.as_os_str().to_string_lossy().to_lowercase()
                    == part.as_os_str().to_string_lossy().to_lowercase()
            } else {
                next == part
            }
        })
    })
}
fn target_will_be_free(job: &Job, path: &Path, rel: &str, source_rel: &str) -> Result<bool> {
    // Windows 大小写不敏感（Unicode 折叠口径，同 under_path）：仅大小写不同的重命名
    // （如 PHOTO.JPE → PHOTO.jpg）时，try_exists 对同一物理文件返回 true，必须视为可腾空，
    // 否则会错误生成 " (1)" 后缀。
    if cfg!(windows) && rel.to_lowercase() == source_rel.to_lowercase() {
        return Ok(true);
    }
    // symlink_metadata 不跟随链接：损坏的符号链接也算目录项已存在。
    // 仅 NotFound 视为空闲；权限/IO 错误不得假定目标不存在，否则计划与执行不一致。
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    }
    // 被计划删除或移走的路径执行后会腾空，可以复用原名，不必生成 " (1)" 后缀。
    let freeing: bool = job.db.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM actions WHERE source=?1 AND kind IN (?2,?3) AND selected=1)",
        params![
            rel,
            serde_json::to_string(&ActionKind::Delete)?,
            serde_json::to_string(&ActionKind::Move)?
        ],
        |r| r.get(0),
    )?;
    Ok(freeing)
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_moves", skip_all)
)]
fn moves(job: &mut Job) -> Result<()> {
    let categories = rules::parse_categories(&job.config.custom_categories)?;
    // H-06：引擎在扫描事务里把每个被剪枝的 Git 根（直接含 .git 的目录）写进会话临时表
    // git_roots；被剪枝的目录不在 directories 表里，只有它能回答「目标是否落在 Git 树内」。
    // 表由同一次扫描建立：缺失属真实错误，不做静默兜底。
    let git_roots: Vec<String> = {
        let mut statement = job.db.conn.prepare("SELECT rel FROM git_roots")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut cursor = 0;
    loop {
        let batch = job.db.files(
            &format!(
                "SELECT {FILE_COLUMNS} FROM files WHERE id>?1 AND active=1 ORDER BY id LIMIT 256"
            ),
            [cursor],
        )?;
        if batch.is_empty() {
            break;
        }
        for file in batch {
            cursor = file.id;
            job.context.control.checkpoint()?;
            let source = fsutil::safe_join(&job.root, &file.rel)?;
            let original = Path::new(&file.rel);
            let Some(original_name) = original.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let mut name = original_name.to_string();
            if job.config.clean_copy_name
                && file.cleanable
                && job.config.duplicate_action != DuplicateAction::Hardlink
            {
                name = rules::strip_copy_name(&name);
            }
            if job.config.normalize_names {
                name = rules::normalize_name(&name);
            }
            if job.config.detect_type {
                // 以点开头且无扩展名的文件（如 .gitignore）不应被 set_extension 追加后缀。
                let is_dotfile = name.starts_with('.') && Path::new(&name).extension().is_none();
                if !is_dotfile {
                    match infer::get_from_path(&source) {
                        Ok(Some(kind)) => {
                            let old = Path::new(&name)
                                .extension()
                                .and_then(|v| v.to_str())
                                .unwrap_or("")
                                .to_lowercase();
                            // C-08 的保守判定集中在 rules::extension_needs_fix：
                            // 容器类识别结果只说明外层容器，OOXML/ODF 等家族的专用扩展名与
                            // 同义别名都不得按更粗的识别结果改名。
                            if rules::extension_needs_fix(&old, kind.extension()) {
                                job.log(
                                    "类型检测",
                                    &file.rel,
                                    "",
                                    "发现",
                                    &format!("扩展名 {old}，内容识别为 {}", kind.extension()),
                                    file.snapshot.size,
                                )?;
                                if job.config.fix_extension {
                                    let mut p = PathBuf::from(&name);
                                    p.set_extension(kind.extension());
                                    name = fsutil::path_string(&p)?;
                                }
                            }
                        }
                        Ok(None) => (),
                        Err(error) => {
                            job.log("类型检测", &file.rel, "", "跳过", &error.to_string(), 0)?;
                        }
                    }
                }
            }
            if let Err(error) = fsutil::validate_component(&name) {
                job.log("命名", &file.rel, "", "跳过", &error.to_string(), 0)?;
                continue;
            }
            let output_dir = job.config.output_dir.as_str();
            // Output is included in deduplication but never nested under itself on repeated runs.
            let under_output =
                !output_dir.is_empty() && under_path(original, Path::new(output_dir));
            let mut parent = directory_target(job, &file, under_output)?;
            if !under_output && (job.config.classify != ClassifyMode::Off || job.config.large_files)
            {
                let extension = Path::new(&name)
                    .extension()
                    .and_then(|v| v.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let label = if job.config.large_files
                    && job.config.large_threshold_gib > 0
                    && file.snapshot.size >= job.config.large_threshold_gib * (1 << 30)
                {
                    Some(PathBuf::from("大文件"))
                } else {
                    match job.config.classify {
                        ClassifyMode::Off => None,
                        ClassifyMode::Extension => Some(PathBuf::from(if extension.is_empty() {
                            "无扩展名".into()
                        } else {
                            extension.to_uppercase()
                        })),
                        ClassifyMode::Category => Some(PathBuf::from(rules::category(&extension))),
                        ClassifyMode::Custom => Some(PathBuf::from(
                            categories.get(&extension).map_or("其他", String::as_str),
                        )),
                        ClassifyMode::Date => {
                            let seconds = file.snapshot.modified_ns.div_euclid(1_000_000_000);
                            if let Some(stamp) = chrono::DateTime::from_timestamp(seconds, 0) {
                                Some(PathBuf::from(
                                    stamp
                                        .with_timezone(&chrono::Local)
                                        .format("%Y/%m")
                                        .to_string(),
                                ))
                            } else {
                                job.log(
                                    "日期归类",
                                    &file.rel,
                                    "",
                                    "跳过",
                                    "修改时间超出可表示范围，已跳过日期归类",
                                    0,
                                )?;
                                None
                            }
                        }
                    }
                };
                if let Some(label) = label {
                    // 分类目录段同样过 Windows 保留名校验：命中则跳过本文件，不得整次 build 失败。
                    if let Err(error) =
                        fsutil::safe_relative(&fsutil::path_string(&label)?.replace('\\', "/"))
                    {
                        job.log(
                            "归类",
                            &file.rel,
                            "",
                            "跳过",
                            &format!("分类目录名不合法：{error}"),
                            0,
                        )?;
                        continue;
                    }
                    // 空 output_dir：分类目录直接建在选定根下；已在该分类目录下的文件不再套一层。
                    // label 可能是多段路径（如日期归类的 2024/03），必须整段前缀比较而不是只比首段。
                    // 合并/扁平调整后的父目录同样参与判定：合并把文件带进同名的分类目录
                    // （如 other/图片/photo.png → 图片/）时就地归类，绝不写成 图片/图片/…，
                    // 否则分类目录会嵌套自身，跨轮往复移动破坏幂等（C-05/C-06/C-10）。
                    let already = output_dir.is_empty()
                        && (under_path(original, &label) || under_path(&parent, &label));
                    if already {
                        // 已在分类目录内的文件必须稳定：flatten 可能把恰好只剩一个文件的
                        // 分类目录整层抽到分类目录之外（parent 不再位于 label 之下），
                        // 此时归位到原始父目录：改名类调整（规范化/扩展名修正）照常生效，位置不动。
                        // 合并后的 parent 仍位于 label 之下时保留合并结果，让合并真正生效。
                        if !under_path(&parent, &label) {
                            parent = original.parent().unwrap_or(Path::new("")).to_path_buf();
                        }
                    } else {
                        parent = if job.config.preserve_structure {
                            if output_dir.is_empty() {
                                label.join(parent)
                            } else {
                                Path::new(output_dir).join(label).join(parent)
                            }
                        } else if output_dir.is_empty() {
                            label
                        } else {
                            Path::new(output_dir).join(label)
                        };
                    }
                }
            }
            let desired = parent.join(&name);
            let desired_rel = fsutil::path_string(&desired)?.replace('\\', "/");
            if let Err(error) = fsutil::safe_relative(&desired_rel) {
                job.log("命名", &file.rel, "", "跳过", &error.to_string(), 0)?;
                continue;
            }
            if desired_rel == file.rel {
                continue;
            }
            // H-06：Git 目录树整树排除，且不得通过移动/改名间接改变它。分类目录、输出目录
            // 或合并目标可能与既有 Git 工作树同名（Git 树不在 directories 表里，无法靠库内
            // 行判断），引擎在扫描事务里把每个被剪枝的 Git 根记进 git_roots，这里逐项比对：
            // 目标目录位于任一 Git 根之下即跳过该项并留日志，绝不写入该树。
            if let Some(git_root) = git_roots.iter().find(|root| {
                let target_dir = desired.parent().unwrap_or(Path::new(""));
                under_path(target_dir, Path::new(root.as_str()))
            }) {
                job.log(
                    "归类",
                    &file.rel,
                    "",
                    "跳过",
                    &format!("目标目录位于 Git 目录树（{git_root}）内，已跳过"),
                    0,
                )?;
                continue;
            }
            // 目标路径含链接（典型：与分类目录同名的 junction/符号链接）时跳过本文件：
            // 与上方「分类目录名不合法」分支同口径——拒绝写穿链接是安全属性，但粒度必须是
            // 该项而不是整次 build（C-10「对应项跳过或失败」）。
            let mut target = match fsutil::safe_join(&job.root, &desired_rel) {
                Ok(target) => target,
                Err(error) => {
                    job.log(
                        "归类",
                        &file.rel,
                        "",
                        "跳过",
                        &format!("目标路径不可用：{error:#}"),
                        0,
                    )?;
                    continue;
                }
            };
            if !target_will_be_free(job, &target, &desired_rel, &file.rel)?
                || !job.db.reserve_target(&desired_rel, file.id)?
            {
                let requested = target.clone();
                let mut index = 1u64;
                let mut skip_move = false;
                loop {
                    job.context.control.checkpoint()?;
                    // 主体/扩展名切分与 unique_target、strip_copy_name 同一口径：序号插在
                    // 完整扩展名之前（资料.tar.gz → 资料 (1).tar.gz），不得拆散 .tar.* 与
                    // 编号分卷 .7z.001（H-07）。借用原串，不额外分配。
                    let name = requested
                        .file_name()
                        .and_then(|s| s.to_str())
                        .context("目标文件名无效")?;
                    let (stem, suffix) = fsutil::split_compound_name(name);
                    // 基础名接近 255 个 UTF-16 单元时，后缀候选名超限会让整次归类失败；
                    // suffixed_candidate 负责截断 stem 保持组件合法。
                    target = requested
                        .parent()
                        .context("目标缺少目录")?
                        .join(fsutil::suffixed_candidate(stem, suffix, index));
                    let rel = fsutil::relative_string(&job.root, &target)?;
                    if let Err(error) = fsutil::safe_join(&job.root, &rel) {
                        // 候选名落在链接上（含既有链接文件占名）：换下一个候选名，不整次失败。
                        job.log(
                            "命名",
                            &file.rel,
                            "",
                            "跳过",
                            &format!("候选目标路径不可用：{error:#}"),
                            0,
                        )?;
                        index += 1;
                        anyhow::ensure!(index < 1_000_000, "目标名称冲突过多");
                        continue;
                    }
                    // 回退候选撞回源文件自身当前名称（如剥离副本名后原名被其它内容占用再回退）：
                    // 源自己占着这个名字且不会腾空，视为已就位，不生成 source==target 的空转移动。
                    if rel == file.rel
                        || (cfg!(windows) && rel.to_lowercase() == file.rel.to_lowercase())
                    {
                        skip_move = true;
                        break;
                    }
                    if target_will_be_free(job, &target, &rel, &file.rel)?
                        && job.db.reserve_target(&rel, file.id)?
                    {
                        break;
                    }
                    index += 1;
                    anyhow::ensure!(index < 1_000_000, "目标名称冲突过多");
                }
                if skip_move {
                    continue;
                }
            }
            let mut planned = action(
                &file,
                ActionKind::Move,
                "按已选择的命名、目录合并、分类规则移动；目标不覆盖",
                DeleteMode::Keep,
            );
            planned.target = Some(fsutil::relative_string(&job.root, &target)?);
            job.db.add_action(&planned)?;
            job.summary.planned_move += 1;
        }
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_empty_dirs", skip_all)
)]
fn empty_directories(job: &mut Job) -> Result<()> {
    // C-07/H-05：空目录清理是强制步骤——没有开关，也不受文件删除方式（全局或按类别覆盖）
    // 影响；删除对象只可能是执行期复查后实际为空的目录，不涉及任何文件内容。
    // 递归关闭时子目录内容未知（扫描未下钻），不得据库内条目判定为空目录。
    if !job.config.recursive {
        return Ok(());
    }
    let mode = DeleteMode::Permanent;
    // 自底向上推算：只把“计划执行后仍会为空”的目录写进计划。
    // 目录为空 = 其下没有会留在原地的文件，且其子目录也都为空。
    // 「会留在原地」= 磁盘上仍会存在：没有选中的删除/移动。分卷源、失败包等 protected 文件
    // 虽 active=0，但仍占目录，不能被算成空目录。
    //
    // 性能：旧实现对每个目录跑三次 `rel LIKE '前缀%'` 全表扫描，目录多的树上会到
    // O(目录数×文件数)。这里预先把文件/目录按“父目录”物化成带索引的临时表，全部
    // 查询退化为等值查找；配合自底向上的处理顺序，深层留驻文件会通过“子目录不在
    // empty_will”逐层向上传播，结果与按全部后代判断完全一致。
    //
    // 性能（2026-09-21 实测）：判断“文件是否有待执行的删除/移动”此前写成对 actions 的
    // 相关子查询，SQLite 选 `idx_actions_state(state=?)` 做内层探测，每个文件都要重扫
    // 全部待执行动作——20k 文件 × 19.75k 动作的实测耗时 53s（计划阶段总耗时 26s 的
    // 绝大部分）。改成先把待执行动作的 source 物化成带主键的临时表再反连接后，同一
    // 数据集实测 0.016s，结果集完全一致。
    let mut cursor = 0i64;
    // kind 在库里是 serde_json 序列化的枚举字符串，只可能是这四个值之一，直接内联安全。
    let move_kind = serde_json::to_string(&ActionKind::Move)?;
    let delete_kind = serde_json::to_string(&ActionKind::Delete)?;
    job.db.conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS empty_order; DROP TABLE IF EXISTS empty_will;
         DROP TABLE IF EXISTS stay_parents; DROP TABLE IF EXISTS dir_children;
         DROP TABLE IF EXISTS doomed_sources; DROP TABLE IF EXISTS tainted_will;
         CREATE TEMP TABLE doomed_sources (rel TEXT PRIMARY KEY);
         INSERT OR IGNORE INTO doomed_sources SELECT source FROM actions
          WHERE kind IN ('{move_kind}','{delete_kind}') AND selected=1 AND state='pending';
         CREATE TEMP TABLE stay_parents (parent TEXT);
         INSERT INTO stay_parents SELECT rtrim(rtrim(rel,replace(rel,'/','')),'/') FROM files
          WHERE NOT EXISTS (SELECT 1 FROM doomed_sources WHERE doomed_sources.rel=files.rel);
         CREATE INDEX stay_parents_parent ON stay_parents(parent);
         CREATE TEMP TABLE dir_children (parent TEXT, rel TEXT PRIMARY KEY);
         INSERT INTO dir_children SELECT rtrim(rtrim(rel,replace(rel,'/','')),'/'),rel FROM directories;
         CREATE INDEX dir_children_parent ON dir_children(parent);
         CREATE TEMP TABLE empty_will (rel TEXT PRIMARY KEY);
         CREATE TEMP TABLE tainted_will (rel TEXT PRIMARY KEY);"))?;
    job.db.conn.execute_batch(
        // seq 是本表唯一的游标列，CREATE TABLE AS SELECT 不会继承任何约束或索引；
        // 缺索引时分页会退化成「每次重扫全表 + 临时 B 树排序」（实测 20 万目录 4.9s vs 0.07s）。
        "CREATE TEMP TABLE empty_order AS SELECT ROW_NUMBER() OVER(ORDER BY depth DESC,rel) seq,rel FROM directories;
         CREATE INDEX empty_order_seq ON empty_order(seq);")?;
    // stay_parents 只覆盖「无 Delete/Move、执行后仍在原地」的文件。同目录改名
    // （A/x → A/y）与迁入新建子目录（A/x → A/分类/x，分类在规划时可能尚未入库、
    // 不在 directories 表里）会让 source 带 Move 而离开 stay_parents，若只看
    // stay_parents 会把仍被占用的源目录误标进 empty_will。预取全部待执行 Move
    // 的目标：凡落点在该目录（或其子树）下的，执行后该目录仍非空。
    let mut move_targets: Vec<String> = {
        let mut stmt = job.db.conn.prepare(
            "SELECT target FROM actions WHERE kind=?1 AND selected=1 AND state='pending' AND target IS NOT NULL")?;
        let rows = stmt.query_map([&move_kind], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Windows 前缀比较按 Unicode 折叠（与 under_path 口径一致）。目标清单固定，
    // 在此预折叠一次；折叠必须在排序之前——排序结果要用于二分定位前缀区间，
    // 折叠会改变字符的字典序。折叠后相同的目标去重，每个前缀只需检查一次。
    if cfg!(windows) {
        move_targets = move_targets
            .into_iter()
            .map(|target| target.to_lowercase())
            .collect();
    }
    move_targets.sort_unstable();
    move_targets.dedup();
    loop {
        let batch = {
            let mut statement = job.db.conn.prepare_cached(
                "SELECT seq,rel FROM empty_order WHERE seq>?1 ORDER BY seq LIMIT 256",
            )?;
            let rows = statement.query_map([cursor], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if batch.is_empty() {
            break;
        }
        for (seq, rel) in batch {
            cursor = seq;
            job.context.control.checkpoint()?;
            // 污点自底向上传播（empty_order 按 depth DESC，子目录必然先处理）：
            // 自身或任一子目录里存在「盘上可见但未入盘点」的内容（扫描期被过滤/读取
            // 失败，记录于 scan_taint）时，本目录不得按空目录处理，且继续向祖先传播。
            // 语义与旧 has_unscanned_content（对该目录整个子树重新走盘核对）一致，
            // 免去逐目录的文件系统遍历。
            let tainted: bool = {
                let mut self_stmt = job
                    .db
                    .conn
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM scan_taint WHERE rel=?1)")?;
                let direct: bool = self_stmt.query_row([&rel], |r| r.get(0))?;
                let mut child_stmt = job.db.conn.prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM dir_children AS c JOIN tainted_will AS t ON t.rel=c.rel WHERE c.parent=?1)")?;
                let via_child: bool = child_stmt.query_row([&rel], |r| r.get(0))?;
                direct || via_child
            };
            if tainted {
                let mut insert = job
                    .db
                    .conn
                    .prepare_cached("INSERT OR IGNORE INTO tainted_will(rel) VALUES(?1)")?;
                insert.execute([&rel])?;
                continue;
            }
            // 执行后该目录（含子树）将接收被 Move 进来的内容时，不能按空目录处理。
            // Windows 下目录 rel 与目标都已按 Unicode 折叠（目标在上方预折叠一次）。
            let probe = if cfg!(windows) {
                format!("{rel}/").to_lowercase()
            } else {
                format!("{rel}/")
            };
            // 有序目标表中以 probe 为前缀的目标构成连续区间：二分定位第一个 >= probe 的
            // 条目，只需检查它——若任何目标以 probe 开头，字典序最小的命中者必然是它，
            // 复杂度从「目录数×目标数」降为「目录数×log 目标数」。
            let receives_move = {
                let index = move_targets.partition_point(|t| t.as_str() < probe.as_str());
                move_targets
                    .get(index)
                    .is_some_and(|t| t.starts_with(probe.as_str()))
            };
            if receives_move {
                continue;
            }
            // 执行后会留在该目录（含其子树）里的文件：深层留驻文件会让对应子目录进不了
            // empty_will，在这里只需检查直接子文件即可得到相同结论。
            let has_file: bool = {
                let mut statement = job
                    .db
                    .conn
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM stay_parents WHERE parent=?1)")?;
                statement.query_row([&rel], |r| r.get(0))?
            };
            if has_file {
                continue;
            }
            // 子目录是否都已判定会为空
            let child_total: i64 = job.db.conn.query_row(
                "SELECT COUNT(1) FROM dir_children WHERE parent=?1",
                [&rel],
                |r| r.get(0),
            )?;
            let child_empty: i64 = job.db.conn.query_row(
                "SELECT COUNT(1) FROM dir_children WHERE parent=?1 AND rel IN (SELECT rel FROM empty_will)",[&rel],|r|r.get(0))?;
            if child_total > child_empty {
                continue;
            }
            {
                let mut statement = job
                    .db
                    .conn
                    .prepare_cached("INSERT OR IGNORE INTO empty_will(rel) VALUES(?1)")?;
                statement.execute([&rel])?;
            }
            job.db.add_action(&Action {
                id: 0,
                kind: ActionKind::EmptyDirectory,
                source: rel,
                target: None,
                reason: "计划执行后该目录将为空；执行时再次确认，只有实际为空才删除".into(),
                expected: None,
                keeper: None,
                hash: None,
                mode,
                selected: true,
                state: "pending".into(),
            })?;
            job.summary.planned_empty += 1;
        }
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan", skip_all)
)]
pub fn build(job: &mut Job) -> Result<()> {
    job.db
        .conn
        .execute_batch("DELETE FROM actions; DELETE FROM targets; DELETE FROM keepers;")?;
    cleanup_candidates(job)?; // Cleanup candidates must never become the sole duplicate keeper.
    deduplicate(job)?;
    moves(job)?;
    empty_directories(job)?;
    Ok(())
}
