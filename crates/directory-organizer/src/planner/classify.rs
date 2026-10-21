use super::dedup::{action, try_detect_extension};
use super::resolve::{fold, parent_of, resolve_all, same_component, same_dir, source_levels, Item};
use anyhow::Result;
use jchtools_file_core::{
    config::DeleteMode,
    db::FILE_COLUMNS,
    fsutil,
    model::{Action, ActionKind},
    rules,
    task::{is_fixed_container_name, Job, GIT_COLLECTION_DIR},
};
use std::path::Path;

/// C-14：Git 项目整体移入所选根下的「Git项目集合」。
/// 返回已计划移动的项目根列表（供文件归类阶段忽略其内部占用）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_git_collection", skip_all)
)]
pub(super) fn git_collection(job: &mut Job) -> Result<Vec<String>> {
    let roots: Vec<String> = {
        let mut statement = job
            .db
            .conn
            .prepare("SELECT rel FROM git_roots ORDER BY rel")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut items: Vec<Item> = Vec::new();
    for rel in &roots {
        // 已位于本次所选根的「Git项目集合」下：不再移动（C-10 幂等 / C-14）。
        if same_component(parent_of(rel), GIT_COLLECTION_DIR) {
            continue;
        }
        let Some(name) = Path::new(rel).file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // S-01：根层固定容器被 Git 项目占用时同样不能挪走项目腾位。
        // 嵌套目录下的同名项目并非本次根的容器占用，仍按 C-14 整体归类。
        if parent_of(rel).is_empty() && is_fixed_container_name(name) {
            job.log(
                "Git归类",
                rel,
                "",
                "保留",
                "固定容器被 Git 项目占用：不挪走占用项目，相关归类保留源项（S-01）",
                0,
            )?;
            continue;
        }
        let parent = parent_of(rel).to_string();
        // 项目来源排除分类层级与直接集合容器（C-14）；项目无大类段可比对剔除。
        let sources = source_levels(&parent, true, false);
        items.push(Item {
            id: 0,
            rel: rel.clone(),
            current_name: name.to_string(),
            stem: name.to_string(),
            extension: String::new(),
            target_dir: GIT_COLLECTION_DIR.to_string(),
            sources,
            in_place: false,
            settled: false,
            failed: None,
            candidate: String::new(),
            k: 0,
            digest_step: 0,
            digest_no_source: false,
            index_suffix: None,
        });
    }
    if items.is_empty() {
        // 没有待移入项目：不创建集合目录，全部项目保持 staying。
        return Ok(Vec::new());
    }
    resolve_all(&mut items, job, &[], None);
    let mut moved: Vec<String> = Vec::new();
    for item in &items {
        if let Some(reason) = &item.failed {
            job.log(
                "Git归类",
                &item.rel,
                "",
                "失败",
                &format!("{reason}；原项目保留在原位置"),
                0,
            )?;
            job.summary.errors += 1;
            continue;
        }
        let target = format!("{GIT_COLLECTION_DIR}/{}", item.candidate);
        if let Err(error) = fsutil::safe_relative(&target) {
            job.log(
                "Git归类",
                &item.rel,
                "",
                "失败",
                &format!("目标名不合法，原项目保留：{error}"),
                0,
            )?;
            job.summary.errors += 1;
            continue;
        }
        let planned = Action {
            id: 0,
            kind: ActionKind::Move,
            source: item.rel.clone(),
            target: Some(target.clone()),
            reason: "Git 项目整体移入「Git项目集合」（C-14：不进入项目内部）".into(),
            expected: None,
            keeper: None,
            hash: None,
            mode: DeleteMode::Keep,
            selected: true,
            state: "pending".into(),
        };
        job.db.add_action(&planned)?;
        job.summary.planned_move += 1;
        job.summary.planned_git += 1;
        moved.push(item.rel.clone());
    }
    if !moved.is_empty() {
        job.log(
            "Git归类",
            "",
            "",
            "提示",
            &format!(
                "识别到 {} 个 Git 项目，其中 {} 个将整体移入「{}」（其余项目保持原位）；项目内部不做任何处理",
                roots.len(),
                moved.len(),
                GIT_COLLECTION_DIR
            ),
            0,
        )?;
    }
    Ok(moved)
}

/// C-05 / C-16～C-21：存活文件的固定归类与命名（目标目录 = 大类，一级结构）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_classify", skip_all)
)]
pub(super) fn classify_files(job: &mut Job, moved_roots: &[String]) -> Result<()> {
    // 保留已有 plan_classify，恢复分类移动计划的稳定性能口径（只增不改名）。
    jchtools_foundation::perf::perf_span!("plan_moves");
    job.context
        .status("按「大类」确定存活文件的归类目标与最终名称");
    // C-05 一级结构：逐文件确定派生名、最终扩展名与大类（C-01 / C-08 / 附录 A），
    // 直接构建定位项，随后统一消解冲突（C-17～C-21）。
    let mut items: Vec<Item> = Vec::new();
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
            let Some(current_name) = Path::new(&file.rel)
                .file_name()
                .and_then(|value| value.to_str())
            else {
                continue;
            };
            let (mut stem, mut extension) = rules::derive_stem_ext(current_name, &job.config);
            // C-08 内容签名修正（默认关）：只在分析阶段判定最终扩展名（C-01）。
            if job.config.fix_extension {
                if let Ok(source) = fsutil::safe_join(&job.root, &file.rel) {
                    if let Some(detected) = try_detect_extension(&source, &job.context.control)? {
                        let old = extension.trim_start_matches('.').to_lowercase();
                        if rules::extension_needs_fix(&old, detected) {
                            extension = format!(".{detected}");
                            job.log(
                                "类型检测",
                                &file.rel,
                                "",
                                "发现",
                                &format!("扩展名 {old}，内容识别为 {detected}，将按识别结果修正"),
                                file.snapshot.size,
                            )?;
                        }
                    }
                }
            }
            // 仅真正改变的完整名授权合法化；单纯归类不剥除既有主体的合法尾点。
            if format!("{stem}{extension}") != current_name {
                stem = rules::stabilize_derived_stem(&stem, &job.config);
            }
            let derived_name = format!("{stem}{extension}");
            if let Err(error) = fsutil::validate_component(&derived_name) {
                job.log(
                    "归类",
                    &file.rel,
                    "",
                    "失败",
                    &format!("派生名称不合法，保留源项：{error}"),
                    0,
                )?;
                job.summary.errors += 1;
                continue;
            }
            let category = if job.config.large_files
                && file.snapshot.size >= job.config.large_threshold_bytes
            {
                "大文件"
            } else {
                rules::category_for(&derived_name.to_lowercase())
            };
            let parent = parent_of(&file.rel).to_string();
            let target_dir = category.to_string();
            let in_place = same_dir(&parent, &target_dir);
            let sources = source_levels(&parent, false, job.config.normalize_names);
            items.push(Item {
                id: file.id,
                rel: file.rel.clone(),
                current_name: current_name.to_string(),
                stem,
                extension,
                target_dir,
                sources,
                in_place,
                settled: false,
                failed: None,
                candidate: String::new(),
                k: 0,
                digest_step: 0,
                digest_no_source: false,
                index_suffix: None,
            });
        }
    }
    resolve_all(&mut items, job, moved_roots, Some(&job.config));
    for item in &items {
        if let Some(reason) = &item.failed {
            job.log(
                "归类",
                &item.rel,
                "",
                "失败",
                &format!("{reason}；已保留源项"),
                0,
            )?;
            job.summary.errors += 1;
            continue;
        }
        if item.settled && same_component(&item.candidate, &item.current_name) {
            // 已就位且名字已是派生形式：保持名称与位置（C-21）。
            continue;
        }
        // settled 但派生名尚未物化的已就位项：以就地改名物化派生名（C-17
        // 「先保护其规范化后名称」），fall through 生成改名计划；候选回落到自身
        // 当前名与当前目录的防御分支在下方同样兜住。
        let target = format!("{}/{}", item.target_dir, item.candidate);
        // 候选回落到自身当前名与当前目录：无需移动（防御性，正常不会出现）。
        if same_dir(&item.target_dir, item.source_parent())
            && same_component(&item.candidate, &item.current_name)
        {
            continue;
        }
        if let Err(error) = fsutil::safe_relative(&target) {
            job.log(
                "归类",
                &item.rel,
                "",
                "失败",
                &format!("目标路径不合法，保留源项：{error}"),
                0,
            )?;
            job.summary.errors += 1;
            continue;
        }
        let file = job.db.file(item.id)?;
        let mut planned = action(
            &file,
            ActionKind::Move,
            "按「大类」归类（同名冲突已统一消解；目标不覆盖）",
            DeleteMode::Keep,
        );
        planned.target = Some(target.clone());
        job.db.add_action(&planned)?;
        job.summary.planned_move += 1;
    }
    Ok(())
}

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan_empty_dirs", skip_all)
)]
pub(super) fn empty_directories(job: &mut Job, moved_roots: &[String]) -> Result<()> {
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
    let mut cursor = 0i64;
    // kind 在库里是 serde_json 序列化的枚举字符串，只可能是这两个值之一，直接内联安全。
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
    // C-14：保持原位的 Git 项目整树仍受 H-06 保护——其祖先目录不得被当成空目录
    //（顺着删祖先等于间接改动该树）；已计划移入集合的项目不再保护，移走后变空的
    // 祖先按 C-07 消失。staying = git_roots − moved（Rust 侧计算）。
    // 前缀比较按 Unicode 折叠（与 fold 同口径）。
    let staying_git_roots: Vec<String> = {
        let mut stmt = job.db.conn.prepare("SELECT rel FROM git_roots")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut roots: Vec<String> = rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|rel| !moved_roots.contains(rel))
            .collect();
        roots = roots.into_iter().map(|rel| fold(&rel)).collect();
        roots.sort();
        roots
    };
    // Windows 前缀比较按 Unicode 折叠（与 under_path 口径一致）。目标清单固定，
    // 在此预折叠一次；折叠必须在排序之前——排序结果要用于二分定位前缀区间，
    // 折叠会改变字符的字典序。折叠后相同的目标去重，每个前缀只需检查一次。
    move_targets = move_targets
        .into_iter()
        .map(|target| fold(&target))
        .collect();
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
            // H-06：本目录或其子树内仍有保持原位的 Git 项目时不按空目录处理。
            let rel_folded = fold(&rel);
            let probe_git = format!("{rel_folded}/");
            let holds_git = staying_git_roots
                .iter()
                .any(|root| *root == rel_folded || root.starts_with(&probe_git));
            if holds_git {
                continue;
            }
            // 污点自底向上传播（empty_order 按 depth DESC，子目录必然先处理）：
            // 自身或任一子目录里存在「盘上可见但未入盘点」的内容（扫描期被过滤/读取
            // 失败，记录于 scan_taint）时，本目录不得按空目录处理，且继续向祖先传播。
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
            let probe = fold(&format!("{rel}/"));
            // 有序目标表中以 probe 为前缀的目标构成连续区间：二分定位第一个 >= probe 的
            // 条目，只需检查它——若任何目标以 probe 开头，字典序最小的命中者必然是它。
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
