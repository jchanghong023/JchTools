use crate::planner::sorted_prefix_hit;
use anyhow::{Context, Result};
use jchtools_file_core::{
    db::Database,
    fsutil,
    model::ActionKind,
    task::{observe, GIT_COLLECTION_DIR},
};
use rusqlite::{params, OptionalExtension};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

/// apply 的锁目录决策：优先 prepare 记录的全局锁目录；记录缺失或失效时，仅当任务
/// 目录仍处于「…/tasks/<任务>」布局才退回上一级推导（搬到别的机器后放回新机器的
/// tasks/ 布局仍可执行），其余位置一律拒绝——不得在陌生位置创建锁文件。
/// 布局判定成立时 derived 必然已存在（它包含 tasks/ 这一级），无需再查 is_dir。
pub(super) fn lock_dir_for(directory: &Path, recorded: Option<&Path>) -> Result<PathBuf> {
    if let Some(recorded) = recorded.filter(|p| p.is_dir()) {
        return Ok(recorded.to_path_buf());
    }
    let derived = directory
        .parent()
        .and_then(Path::parent)
        .context("任务目录结构无效")?;
    // 盘根（X:\）与 UNC 共享根（\\server\share\）不是状态目录：在那里落锁会留下
    // 陌生位置的锁文件。判定：derived 不含任何 Normal 组件（盘根/共享根只有
    // Prefix+RootDir；UNC 前缀不可拆分，不能用祖先层数统计，否则会误拒共享盘上
    // 正常深度的状态目录）。
    let root_level = !derived
        .components()
        .any(|c| matches!(c, std::path::Component::Normal(_)));
    anyhow::ensure!(
        !root_level&&directory.parent().and_then(Path::file_name).is_some_and(|name|name=="tasks"),
        "任务库缺少有效的全局锁目录记录且任务目录已离开原位置；为避免互斥失效，请重新「解压与分析」后再执行");
    Ok(derived.to_path_buf())
}
/// 执行期实空复查：目录消失、类型改变或已非空时返回 false，不得按空目录清理
/// （计划与执行之间的盘面变化以盘面实况为准）。执行段与串行路径共用同一谓词。
pub(super) fn dir_recheck_empty(path: &Path) -> Result<bool> {
    Ok(path.try_exists()? && path.is_dir() && fs::read_dir(path)?.next().is_none())
}
pub(super) fn path_is_at_or_below(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
pub(super) fn task_git_roots(db: &Database) -> Result<Vec<String>> {
    let move_kind = serde_json::to_string(&ActionKind::Move)?;
    let mut roots: Vec<String> = if let Some(value) = db
        .conn
        .query_row(
            "SELECT value FROM metadata WHERE key=?1",
            ["git_roots"],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        serde_json::from_str(&value)?
    } else {
        // 兼容此前生成的待确认任务：C-14 Move 的目标进入固定集合容器。
        let collection_target = format!("{GIT_COLLECTION_DIR}/%");
        let mut statement = db
            .conn
            .prepare("SELECT source FROM actions WHERE kind=?1 AND target LIKE ?2")?;
        let rows = statement.query_map(params![move_kind, collection_target], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    roots = roots
        .into_iter()
        .map(|rel| fsutil::fold_rel(&rel))
        .collect();
    roots.sort_unstable();
    roots.dedup();
    Ok(roots)
}
/// C-01：取消勾选后仅用既有分析资料重算受影响的计划。被取消且未执行的
/// Move/Delete 项原位置视为占用，被取消的空目录清理行自身同样占位（子目录
/// 不删，父目录不再变空）；selected=1 的空目录清理行若其目录（含子树）内
/// 存在占位项，依赖失效、转为未勾选——禁止删除依赖于已取消保留者移动的
/// 其他项。用户已取消的行不翻回，不新增用户勾选，不读取文件内容。
/// 返回本次重算取消的行数；执行期的实空复查与最终空目录清理仍独立兜底。
/// H-06：保留原位的 Git 根及其后代/祖先不作为可写目标或空目录清理项。
pub fn recompute_plan(directory: &Path) -> Result<usize> {
    observe("recompute_plan", None, |stage| {
        *stage = "plan_database";
        let result: Result<usize> = (|| {
            // open_existing：任务库文件消失时不静默创建空库（apply 同口径）。
            let db = Database::open_existing(directory)?;
            let status: String = db.get("status")?;
            anyhow::ensure!(
                status == "ready",
                "任务不是待确认状态（{status}），不重算受影响计划"
            );
            let move_kind = serde_json::to_string(&ActionKind::Move)?;
            let delete_kind = serde_json::to_string(&ActionKind::Delete)?;
            let empty_kind = serde_json::to_string(&ActionKind::EmptyDirectory)?;
            // 占位项 = 取消且未执行的 Move/Delete 源 + 取消且未执行的空目录行自身。
            // 消费方只做等值与前缀匹配、与行序无关，三种 kind 合并为一条查询。
            let mut occupied: Vec<String> = {
                let mut statement = db.conn.prepare(
        "SELECT source FROM actions WHERE kind IN (?1, ?2, ?3) AND selected=0 AND state='pending'",
    )?;
                let rows = statement
                    .query_map(params![move_kind, delete_kind, empty_kind], |row| {
                        row.get::<_, String>(0)
                    })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            // 取消一个 Move 后，其源仍留在盘上，可能阻挡另一条原本选中的 Move 目标。
            // 以固定点逐轮取消，保证用户取消一条链中的任意一项都不会留下必然撞名的
            // 执行计划；不新增勾选，也不读取文件内容。
            let mut cancelled = 0usize;
            let mut pending_moves: Vec<(i64, String, String)> = {
                let mut statement = db.conn.prepare(
        "SELECT id, source, target FROM actions WHERE kind=?1 AND selected=1 AND state='pending' AND target IS NOT NULL",
    )?;
                let rows = statement.query_map(params![move_kind], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    ))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let git_roots = task_git_roots(&db)?;
            // 计划中的项目 Move 尚未执行时可释放原路径；取消其 Move 后即转为 staying，
            // 其根目录及后代目标不得再被其他 Move/空目录清理项触碰。
            let mut pending_git_moves: HashSet<String> = pending_moves
                .iter()
                .map(|(_, source, _)| fsutil::fold_rel(source))
                .filter(|source| git_roots.binary_search(source).is_ok())
                .collect();
            let mut staying_git_roots: Vec<String> = git_roots
                .iter()
                .filter(|root| !pending_git_moves.contains(root.as_str()))
                .cloned()
                .collect();
            while let Some(index) = pending_moves.iter().position(|(_, _, target)| {
                let target = fsutil::fold_rel(target);
                occupied
                    .iter()
                    .any(|source| fsutil::fold_rel(source) == target)
                    || staying_git_roots
                        .iter()
                        .any(|root| path_is_at_or_below(&target, root))
            }) {
                let (id, source, _) = pending_moves.remove(index);
                db.set_selected(id, false)?;
                let folded_source = fsutil::fold_rel(&source);
                if pending_git_moves.remove(&folded_source) {
                    staying_git_roots.push(folded_source);
                }
                occupied.push(source);
                cancelled += 1;
            }
            // 与 planner 的前缀区间判定同口径：Windows 折叠大小写后排序去重，二分定位。
            occupied = occupied
                .into_iter()
                .map(|rel| fsutil::fold_rel(&rel))
                .collect();
            occupied.sort_unstable();
            occupied.dedup();
            let rows: Vec<(i64, String)> = {
                let mut statement = db.conn.prepare(
        "SELECT id, source FROM actions WHERE kind=?1 AND selected=1 AND state='pending'",
    )?;
                let rows = statement.query_map(params![empty_kind], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (id, rel) in rows {
                let folded = fsutil::fold_rel(&rel);
                let probe = format!("{folded}/");
                let blocked_by_occupied = sorted_prefix_hit(&occupied, &probe);
                let blocked_by_git = staying_git_roots.iter().any(|root| {
                    path_is_at_or_below(&folded, root) || path_is_at_or_below(root, &folded)
                });
                let blocked = blocked_by_occupied || blocked_by_git;
                if blocked {
                    db.set_selected(id, false)?;
                    cancelled += 1;
                }
            }
            Ok(cancelled)
        })();
        if let Ok(count) = &result {
            tracing::debug!(
                event = "plan_recomputed",
                deselected = count,
                "计划重算完成"
            );
        }
        result
    })
}
