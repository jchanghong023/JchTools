use crate::{config::{ClassifyMode, DeleteMode, DuplicateAction}, db::FILE_COLUMNS,
    engine::Job, fsutil, model::{Action,ActionKind,FileRecord}, rules};
use anyhow::{Context,Result};
use rusqlite::{params, OptionalExtension};
use std::path::{Path,PathBuf};

fn action(file: &FileRecord, kind: ActionKind, reason: &str, mode: DeleteMode) -> Action {
    Action { id: 0,kind,source:file.rel.clone(),target:None,reason:reason.into(),expected:Some(file.snapshot.clone()),
        keeper:None,hash:file.hash.clone(),mode,selected:true,state:"pending".into() }
}
fn remove_candidate(job: &mut Job, file: &FileRecord, keeper: Option<&FileRecord>, reason: &str, mode: DeleteMode, hardlink: bool) -> Result<()> {
    if mode == DeleteMode::Keep { return Ok(()); }
    let mut planned = action(file, if hardlink { ActionKind::Hardlink } else { ActionKind::Delete },reason,mode);
    if let Some(keeper) = keeper { planned.keeper = Some((keeper.rel.clone(),keeper.snapshot.clone())); }
    job.db.add_action(&planned)?;
    job.db.conn.execute("UPDATE files SET active=0 WHERE id=?1",[file.id])?;
    if hardlink { job.summary.planned_link += 1; } else { job.summary.planned_delete += 1; }
    // Already-hardlinked files do not represent distinct physical allocation.
    if file.snapshot.links <= 1 { job.summary.candidate_bytes = job.summary.candidate_bytes.saturating_add(file.snapshot.size); }
    Ok(())
}
fn cleanup_candidates(job: &mut Job) -> Result<()> {
    let mut cursor = 0;
    loop {
        let batch = job.db.files(&format!("SELECT {FILE_COLUMNS} FROM files WHERE id>?1 AND active=1 ORDER BY id LIMIT 256"),[cursor])?;
        if batch.is_empty() { break; }
        for file in batch {
            job.context.control.checkpoint()?; cursor = file.id;
            if let Some(reason) = rules::cleanup_reason(&file.rel,file.snapshot.size,&job.config) {
                let mode = job.config.cleanup_delete.resolve(job.config.global_delete);
                remove_candidate(job,&file,None,reason,mode,false)?;
            }
        }
    }
    Ok(())
}
fn deduplicate(job: &mut Job) -> Result<()> {
    if !job.config.dedup_same_name && !job.config.dedup_copy_names && !job.config.dedup_other_names { return Ok(()); }
    job.context.status("分析相同内容：相同名称 / 副本名称 / 不同名称分别应用规则");
    let order = rules::ordering_sql(job.config.keep_duplicate);
    job.db.conn.execute_batch(&format!("DROP TABLE IF EXISTS duplicate_order; CREATE TEMP TABLE duplicate_order AS SELECT ROW_NUMBER() OVER(ORDER BY hash,{order}) AS seq,id FROM files WHERE active=1 AND hash IS NOT NULL; CREATE INDEX duplicate_order_seq ON duplicate_order(seq); DELETE FROM keepers;"))?;
    let mut cursor = 0i64;
    loop {
        let items = {
            let mut stmt = job.db.conn.prepare("SELECT seq,id FROM duplicate_order WHERE seq>?1 ORDER BY seq LIMIT 256")?;
            let rows = stmt.query_map([cursor],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if items.is_empty() { break; }
        for (seq,id) in items {
            cursor = seq; job.context.control.checkpoint()?;
            let file = job.db.file(id)?;
            let hash = file.hash.as_ref().context("重复候选缺少 Hash")?;
            let keeper_id: Option<i64> = job.db.conn.query_row(
                "SELECT file_id FROM keepers WHERE hash=?1 AND ((name=?2 AND ?4) OR (name<>?2 AND normal=?3 AND ?5) OR (name<>?2 AND normal<>?3 AND ?6)) ORDER BY rowid LIMIT 1",
                params![hash,file.name,file.normalized,job.config.dedup_same_name,job.config.dedup_copy_names,job.config.dedup_other_names],|r|r.get(0)).optional()?;
            if let Some(keeper_id) = keeper_id {
                let keeper = job.db.file(keeper_id)?;
                if keeper.snapshot.identity == file.snapshot.identity {
                    job.log("去重",&file.rel,&keeper.rel,"保留","已经是同一个文件的硬链接，不重复计算可释放空间",file.snapshot.size)?;
                    continue;
                }
                let reason = if file.name == keeper.name { "相同名称且完整 Hash 相同" }
                    else if file.normalized == keeper.normalized { "副本命名且完整 Hash 相同" } else { "名称不同但完整 Hash 相同" };
                let mode = job.config.duplicate_delete.resolve(job.config.global_delete);
                let mut hardlink = job.config.duplicate_action == DuplicateAction::Hardlink;
                // 跨卷硬链接在执行期必然失败，规划阶段就降级为删除，避免计划与结果不符。
                if hardlink {
                    let vol = |id: &str| -> String { id.split(':').next().unwrap_or("").to_string() };
                    let same_volume = vol(&keeper.snapshot.identity) == vol(&file.snapshot.identity);
                    if !same_volume {
                        hardlink = false;
                        job.log("去重",&file.rel,&keeper.rel,"降级","跨卷无法硬链接，改为按删除规则处理",file.snapshot.size)?;
                    }
                }
                remove_candidate(job,&file,Some(&keeper),reason,mode,hardlink)?;
                if mode != DeleteMode::Keep { job.db.conn.execute("UPDATE files SET cleanable=1 WHERE id=?1",[keeper_id])?; }
            } else {
                job.db.conn.execute("INSERT INTO keepers(file_id,hash,name,normal) VALUES(?1,?2,?3,?4)",params![file.id,hash,file.name,file.normalized])?;
            }
        }
    }
    Ok(())
}
fn conflict_groups(job: &mut Job, same_size: bool) -> Result<()> {
    let enabled = if same_size { job.config.same_name_same_size } else { job.config.same_name_different_size };
    if !enabled { return Ok(()); }
    let policy = if same_size { job.config.same_size_keep } else { job.config.different_size_keep };
    let mode = job.config.conflict_delete.resolve(job.config.global_delete);
    if mode == DeleteMode::Keep { return Ok(()); }
    // A grouping table avoids keeping millions of names/paths in RAM.
    job.db.conn.execute_batch("DROP TABLE IF EXISTS conflict_groups; CREATE TEMP TABLE conflict_groups(seq INTEGER PRIMARY KEY,key TEXT,size INTEGER);")?;
    // 分组键语义（与 rules.json 的开关描述一致）：
    // - 目录范围（默认）→ 只比较「同一父目录内的同名文件」。rel 是含父目录的全路径且唯一，
    //   lower(rel) 即等价于「父目录+小写名」；Windows 文件系统不允许同目录存在同名（含大小写
    //   变体）文件，因此该范围在 Windows 上永不触发，作用是防止跨目录同名被误判为版本冲突。
    // - 全局范围 → 只比较小写文件名，跨目录的同名版本取舍由它承担。
    // 注意：任何「父目录+文件名」形式的键都与 lower(rel) 数学等价，无法让本范围更积极。
    let key_expr = if job.config.conflict_scope_directory { "lower(rel)" } else { "lower(name)" };
    let grouping = if same_size {
        format!("INSERT INTO conflict_groups(key,size) SELECT {key_expr},size FROM files WHERE active=1 AND hash IS NOT NULL GROUP BY {key_expr},size HAVING COUNT(DISTINCT hash)>1")
    } else {
        format!("INSERT INTO conflict_groups(key,size) SELECT {key_expr},NULL FROM files WHERE active=1 GROUP BY {key_expr} HAVING COUNT(DISTINCT size)>1")
    };
    job.db.conn.execute(&grouping,[])?;
    let mut group_cursor = 0;
    loop {
        let groups = {
            let mut stmt = job.db.conn.prepare("SELECT seq,key,size FROM conflict_groups WHERE seq>?1 ORDER BY seq LIMIT 128")?;
            let rows = stmt.query_map([group_cursor],|r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<i64>>(2)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if groups.is_empty() { break; }
        for (seq,key,size) in groups {
            job.context.control.checkpoint()?; group_cursor = seq;
            let filter = format!("active=1 AND {key_expr}=?1 {}",if same_size {"AND size=?2 AND hash IS NOT NULL"} else {"AND (?2 IS NULL)"});
            let keep_sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE {filter} ORDER BY {} LIMIT 1",rules::ordering_sql(policy));
            let candidates = job.db.files(&keep_sql,params![key,size])?;
            let Some(keeper) = candidates.first().cloned() else { continue; };
            let mut file_cursor = 0;
            loop {
                let sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE {filter} AND id>?3 AND id<>?4 ORDER BY id LIMIT 256");
                let files = job.db.files(&sql,params![key,size,file_cursor,keeper.id])?;
                if files.is_empty() { break; }
                for file in files {
                    file_cursor = file.id;
                    if same_size && file.hash == keeper.hash { continue; }
                    if !same_size && file.snapshot.size == keeper.snapshot.size { continue; }
                    let reason = if same_size { "同名同大小但 Hash 不同：用户选择的版本保留规则" }
                        else { "同名不同大小：用户选择的版本保留规则（不是内容去重）" };
                    // Keeper metadata is checked again before deleting a conflicting version.
                    let mut planned = action(&file,ActionKind::Delete,reason,mode);
                    planned.keeper = Some((keeper.rel.clone(),keeper.snapshot.clone()));
                    // Different-content conflicts must not go through byte-equality validation.
                    planned.hash = None;
                    job.db.add_action(&planned)?;
                    job.db.conn.execute("UPDATE files SET active=0 WHERE id=?1",[file.id])?;
                    job.summary.planned_delete += 1;
                    if file.snapshot.links <= 1 { job.summary.candidate_bytes = job.summary.candidate_bytes.saturating_add(file.snapshot.size); }
                }
            }
        }
    }
    Ok(())
}
fn directory_target(job: &Job, file: &FileRecord) -> Result<PathBuf> {
    let original = Path::new(&file.rel);
    let mut parent = original.parent().unwrap_or(Path::new("")).to_path_buf();
    if job.config.merge_directories && !parent.as_os_str().is_empty() {
        let mut merged_parent = None;
        for ancestor in parent.ancestors() {
            let Some(name) = ancestor.file_name().and_then(|s|s.to_str()) else { continue; };
            let candidate: Option<String> = job.db.conn.query_row("SELECT rel FROM directories WHERE name=?1 ORDER BY depth,rel LIMIT 1",[name.to_lowercase()],|r|r.get(0)).optional()?;
            if let Some(candidate) = candidate {
                let dest = Path::new(&candidate);
                if dest != ancestor && !dest.starts_with(ancestor) {
                    merged_parent = Some(dest.join(parent.strip_prefix(ancestor)?)); break;
                }
            }
        }
        if let Some(merged) = merged_parent { parent = merged; }
    }
    if job.config.flatten_single_child {
        loop {
            if parent.as_os_str().is_empty() { break; }
            let current = fsutil::safe_join(&job.root,&fsutil::path_string(&parent)?)?;
            // 单个目录读取失败不应中止整个规划，跳过该文件的扁平化即可。
            let entries = match std::fs::read_dir(&current) {
                Ok(rd) => rd.take(2).collect::<std::io::Result<Vec<_>>>().unwrap_or_default(),
                Err(_) => break,
            };
            if entries.len() != 1 { break; }
            parent = parent.parent().unwrap_or(Path::new("")).to_path_buf();
        }
    }
    Ok(parent)
}
/// `path` 是否已位于 `prefix` 之下（逐段比较；Windows 目录不区分大小写，故忽略 ASCII 大小写）。
fn under_path(path: &Path, prefix: &Path) -> bool {
    let mut rest = path.components();
    prefix.components().all(|part| {
        rest.next().is_some_and(|next| {
            if cfg!(windows) { next.as_os_str().eq_ignore_ascii_case(part.as_os_str()) } else { next == part }
        })
    })
}
fn target_will_be_free(job: &Job, path: &Path, rel: &str, source_rel: &str) -> Result<bool> {
    // Windows 大小写不敏感：仅大小写不同的重命名（如 PHOTO.JPE → PHOTO.jpg）时，
    // try_exists 对同一物理文件返回 true，必须视为可腾空，否则会错误生成 " (1)" 后缀。
    if cfg!(windows) && rel.eq_ignore_ascii_case(source_rel) { return Ok(true); }
    // symlink_metadata 不跟随链接：损坏的符号链接也算目录项已存在。
    if std::fs::symlink_metadata(path).is_err() { return Ok(true); }
    // 被计划删除或移走的路径执行后会腾空，可以复用原名，不必生成 " (1)" 后缀。
    let freeing: bool = job.db.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM actions WHERE source=?1 AND kind IN (?2,?3) AND selected=1)",
        params![rel,serde_json::to_string(&ActionKind::Delete)?,serde_json::to_string(&ActionKind::Move)?],|r|r.get(0))?;
    Ok(freeing)
}
fn moves(job: &mut Job) -> Result<()> {
    let categories = rules::parse_categories(&job.config.custom_categories)?;
    let mut cursor = 0;
    loop {
        let batch = job.db.files(&format!("SELECT {FILE_COLUMNS} FROM files WHERE id>?1 AND active=1 ORDER BY id LIMIT 256"),[cursor])?;
        if batch.is_empty() { break; }
        for file in batch {
            cursor = file.id; job.context.control.checkpoint()?;
            let source = fsutil::safe_join(&job.root,&file.rel)?;
            let original = Path::new(&file.rel);
            let Some(original_name) = original.file_name().and_then(|s|s.to_str()) else { continue; };
            let mut name = original_name.to_string();
            if job.config.clean_copy_name && file.cleanable && job.config.duplicate_action != DuplicateAction::Hardlink { name = rules::strip_copy_name(&name); }
            if job.config.normalize_names { name = rules::normalize_name(&name); }
            if job.config.detect_type {
                // 以点开头且无扩展名的文件（如 .gitignore）不应被 set_extension 追加后缀。
                let is_dotfile = name.starts_with('.') && Path::new(&name).extension().is_none();
                if !is_dotfile {
                    match infer::get_from_path(&source) {
                        Ok(Some(kind)) => {
                            let old = Path::new(&name).extension().and_then(|v|v.to_str()).unwrap_or("").to_lowercase();
                            // ZIP-container document formats are not renamed to .zip.
                            let compound = ["docx","xlsx","pptx","epub","odt","ods","odp","jar","apk"].contains(&old.as_str());
                            let equivalent = old == kind.extension() || (old == "jpeg" && kind.extension() == "jpg") || (old == "tiff" && kind.extension() == "tif");
                            if !equivalent && !compound {
                                job.log("类型检测",&file.rel,"","发现",&format!("扩展名 {old}，内容识别为 {}",kind.extension()),file.snapshot.size)?;
                                if job.config.fix_extension {
                                    let mut p = PathBuf::from(&name); p.set_extension(kind.extension()); name = fsutil::path_string(&p)?;
                                }
                            }
                        }
                        Ok(None) => (),
                        Err(error) => { job.log("类型检测",&file.rel,"","跳过",&error.to_string(),0)?; }
                    }
                }
            }
            if let Err(error) = fsutil::validate_component(&name) {
                job.log("命名",&file.rel,"","跳过",&error.to_string(),0)?; continue;
            }
            let mut parent = directory_target(job,&file)?;
            let output_dir = job.config.output_dir.as_str();
            // Output is included in deduplication but never nested under itself on repeated runs.
            let under_output = !output_dir.is_empty() && under_path(original, Path::new(output_dir));
            if !under_output && (job.config.classify != ClassifyMode::Off || job.config.large_files) {
                let extension = Path::new(&name).extension().and_then(|v|v.to_str()).unwrap_or("").to_lowercase();
                let label = if job.config.large_files && job.config.large_threshold_gib > 0 && file.snapshot.size >= job.config.large_threshold_gib * (1<<30) { Some(PathBuf::from("大文件")) }
                    else { match job.config.classify {
                        ClassifyMode::Off => None,
                        ClassifyMode::Extension => Some(PathBuf::from(if extension.is_empty() { "无扩展名".into() } else { extension.to_uppercase() })),
                        ClassifyMode::Category => Some(PathBuf::from(rules::category(&extension))),
                        ClassifyMode::Custom => Some(PathBuf::from(categories.get(&extension).map(String::as_str).unwrap_or("其他"))),
                        ClassifyMode::Date => {
                            let seconds = file.snapshot.modified_ns.div_euclid(1_000_000_000);
                            match chrono::DateTime::from_timestamp(seconds,0) {
                                Some(stamp) => Some(PathBuf::from(stamp.with_timezone(&chrono::Local).format("%Y/%m").to_string())),
                                None => { job.log("日期归类",&file.rel,"","跳过","修改时间超出可表示范围，已跳过日期归类",0)?; None }
                            }
                        }
                    }};
                if let Some(label) = label {
                    // 空 output_dir：分类目录直接建在选定根下；已在该分类目录下的文件不再套一层。
                    // label 可能是多段路径（如日期归类的 2024/03），必须整段前缀比较而不是只比首段。
                    let already = output_dir.is_empty() && under_path(original, &label);
                    if !already {
                        parent = if job.config.preserve_structure {
                            if output_dir.is_empty() { label.join(parent) }
                            else { Path::new(output_dir).join(label).join(parent) }
                        } else if output_dir.is_empty() { label }
                        else { Path::new(output_dir).join(label) };
                    }
                }
            }
            let desired = parent.join(&name);
            let desired_rel = fsutil::path_string(&desired)?.replace('\\',"/");
            if desired_rel == file.rel { continue; }
            let mut target = fsutil::safe_join(&job.root,&desired_rel)?;
            if !target_will_be_free(job,&target,&desired_rel,&file.rel)? || !job.db.reserve_target(&desired_rel,file.id)? {
                let requested = target.clone(); let mut index = 1u64;
                loop {
                    let stem = requested.file_stem().and_then(|s|s.to_str()).context("目标文件名无效")?;
                    let suffix = requested.extension().and_then(|s|s.to_str()).map(|s|format!(".{s}")).unwrap_or_default();
                    target = requested.parent().context("目标缺少目录")?.join(format!("{stem} ({index}){suffix}"));
                    let rel = fsutil::relative_string(&job.root,&target)?;
                    fsutil::safe_join(&job.root,&rel)?;
                    if target_will_be_free(job,&target,&rel,&file.rel)? && job.db.reserve_target(&rel,file.id)? { break; }
                    index += 1; anyhow::ensure!(index < 1_000_000,"目标名称冲突过多");
                }
            }
            let mut planned = action(&file,ActionKind::Move,"按已选择的命名、目录合并、分类规则移动；目标不覆盖",DeleteMode::Keep);
            planned.target = Some(fsutil::relative_string(&job.root,&target)?);
            job.db.add_action(&planned)?; job.summary.planned_move += 1;
        }
    }
    Ok(())
}
/// 目录下是否存在未入库文件（隐藏/系统/排除等）；有则不能按空目录清理。
fn has_unscanned_content(job: &Job, rel: &str) -> Result<bool> {
    let path = fsutil::safe_join(&job.root, rel)?;
    for entry in walkdir::WalkDir::new(&path).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() { continue; }
        job.context.control.checkpoint()?;
        let child = fsutil::relative_string(&job.root, entry.path())?;
        let in_db: i64 = job.db.conn.query_row("SELECT COUNT(1) FROM files WHERE rel=?1",[&child],|r|r.get(0))?;
        if in_db == 0 { return Ok(true); }
    }
    Ok(false)
}
fn empty_directories(job: &mut Job) -> Result<()> {
    let mode = job.config.cleanup_delete.resolve(job.config.global_delete);
    if !job.config.clean_empty_dirs || mode == DeleteMode::Keep { return Ok(()); }
    // 自底向上推算：只把“计划执行后仍会为空”的目录写进计划。
    // 目录为空 = 其下没有会留在原地的文件，且其子目录也都为空。
    // 「会留在原地」= 磁盘上仍会存在：没有选中的删除/移动。分卷源、失败包等 protected 文件
    // 虽 active=0，但仍占目录，不能被算成空目录。
    //
    // 性能：旧实现对每个目录跑三次 `rel LIKE '前缀%'` 全表扫描，目录多的树上会到
    // O(目录数×文件数)。这里预先把文件/目录按“父目录”物化成带索引的临时表，全部
    // 查询退化为等值查找；配合自底向上的处理顺序，深层留驻文件会通过“子目录不在
    // empty_will”逐层向上传播，结果与按全部后代判断完全一致。
    let mut cursor = 0i64;
    // kind 在库里是 serde_json 序列化的枚举字符串，只可能是这四个值之一，直接内联安全。
    let move_kind = serde_json::to_string(&ActionKind::Move)?;
    let delete_kind = serde_json::to_string(&ActionKind::Delete)?;
    job.db.conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS empty_order; DROP TABLE IF EXISTS empty_will;
         DROP TABLE IF EXISTS stay_parents; DROP TABLE IF EXISTS dir_children;
         CREATE TEMP TABLE stay_parents (parent TEXT);
         INSERT INTO stay_parents SELECT rtrim(rtrim(rel,replace(rel,'/','')),'/') FROM files
          WHERE NOT EXISTS (SELECT 1 FROM actions WHERE kind IN ('{move_kind}','{delete_kind}') AND selected=1 AND state='pending' AND source=files.rel);
         CREATE INDEX stay_parents_parent ON stay_parents(parent);
         CREATE TEMP TABLE dir_children (parent TEXT, rel TEXT PRIMARY KEY);
         INSERT INTO dir_children SELECT rtrim(rtrim(rel,replace(rel,'/','')),'/'),rel FROM directories;
         CREATE INDEX dir_children_parent ON dir_children(parent);
         CREATE TEMP TABLE empty_will (rel TEXT PRIMARY KEY);"))?;
    job.db.conn.execute_batch(
        "CREATE TEMP TABLE empty_order AS SELECT ROW_NUMBER() OVER(ORDER BY depth DESC,rel) seq,rel FROM directories;")?;
    loop {
        let batch = {
            let mut statement = job.db.conn.prepare("SELECT seq,rel FROM empty_order WHERE seq>?1 ORDER BY seq LIMIT 256")?;
            let rows = statement.query_map([cursor],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if batch.is_empty() { break; }
        for (seq,rel) in batch {
            cursor = seq; job.context.control.checkpoint()?;
            // 执行后会留在该目录（含其子树）里的文件：深层留驻文件会让对应子目录进不了
            // empty_will，在这里只需检查直接子文件即可得到相同结论。
            let has_file: i64 = job.db.conn.query_row(
                "SELECT COUNT(1) FROM stay_parents WHERE parent=?1",[&rel],|r|r.get(0))?;
            if has_file>0 { continue; }
            // 隐藏/系统/排除文件不会入库，但仍占目录；有这类内容就不能当作空目录。
            if has_unscanned_content(job,&rel)? { continue; }
            // 子目录是否都已判定会为空
            let child_total: i64 = job.db.conn.query_row(
                "SELECT COUNT(1) FROM dir_children WHERE parent=?1",[&rel],|r|r.get(0))?;
            let child_empty: i64 = job.db.conn.query_row(
                "SELECT COUNT(1) FROM dir_children WHERE parent=?1 AND rel IN (SELECT rel FROM empty_will)",[&rel],|r|r.get(0))?;
            if child_total>child_empty { continue; }
            job.db.conn.execute("INSERT OR IGNORE INTO empty_will(rel) VALUES(?1)",[&rel])?;
            job.db.add_action(&Action { id:0,kind:ActionKind::EmptyDirectory,source:rel,target:None,
                reason:"计划执行后该目录将为空；执行时再次确认，只有实际为空才删除".into(),expected:None,keeper:None,hash:None,
                mode,selected:true,state:"pending".into() })?;
            job.summary.planned_empty += 1;
        }
    }
    Ok(())
}
pub fn build(job: &mut Job) -> Result<()> {
    job.db.conn.execute_batch("DELETE FROM actions; DELETE FROM targets; DELETE FROM keepers;")?;
    cleanup_candidates(job)?; // Cleanup candidates must never become the sole duplicate keeper.
    deduplicate(job)?;
    conflict_groups(job,true)?;
    conflict_groups(job,false)?;
    moves(job)?;
    empty_directories(job)?;
    Ok(())
}
