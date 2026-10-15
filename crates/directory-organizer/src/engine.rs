use crate::planner;
use anyhow::{bail, Context as _, Result};
#[cfg(test)]
use jchtools_file_core::model::Summary;
use jchtools_file_core::{
    config::{self, Config, DeleteMode},
    control::Context as TaskContext,
    db::Database,
    fsutil, hashing,
    model::{Action, ActionKind},
    platform::{self, DeleteResult},
    rules,
    scan::{io_pool, scan, special_prefixes, ScopeFilter},
    task::{
        create_task_db, observe, preflight_root, record_task_failure, resolve_protection, Job,
        TaskResult, GIT_COLLECTION_DIR,
    },
};
use rayon::prelude::*;
use rusqlite::{params, OptionalExtension};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

/// 哈希阶段一次从任务库取多少条候选。与哈希线程数解耦（线程数只决定并行度，
/// 批量只决定分页次数），避免「调线程数」同时改变两个量而无法判断。
const HASH_BATCH: usize = 256;
/// 执行阶段每批动作数：一批一个事务。每动作 4 条语句若各自自动提交，实测单条约 40µs
/// （2 万项执行阶段 9.2s 里约 3.6s 花在提交上）；合批后每动作提交摊到 7µs。
/// 64 是实测折中：256 批次只再快 6%，但崩溃/取消时未提交记录的窗口放大 4 倍；
/// 64 与界面计划页（101 行）同量级，用户看到的进度滞后不超过一页。
const APPLY_BATCH: usize = 64;
pub fn prepare(root: &Path, config: Config, context: TaskContext) -> Result<TaskResult> {
    prepare_at(root, config, context, &config::state_dir()?)
}
/// 独立状态目录使核心可在无 GUI 下测试。目录整理不解压（C-01），不再接受引擎注入；
/// 真实引擎用例走 extract_run_at（E-02：引擎只能按固定顺序获得，不向用户提供路径参数）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "organize_analyze", skip_all)
)]
pub fn prepare_at(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
) -> Result<TaskResult> {
    let control = context.control.clone();
    observe("organize_preflight", Some(&control), |stage| {
        *stage = "protection";
        let protected = resolve_protection()?;
        prepare_at_with(root, config, context, state, protected.as_deref())
    })
}
/// S-05 受保护目录参数化变体（测试注入合成受保护目录），其余行为与 [`prepare_at`] 一致。
fn prepare_at_with(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
    protected: Option<&Path>,
) -> Result<TaskResult> {
    let control = context.control.clone();
    observe("organize_analyze", Some(&control), |stage| {
        let (root, protected) = preflight_root(root, &config, protected, false)?;
        *stage = "task_database";
        let (_guard, mut job) = create_task_db(state, &root, config, context, "analyzing")?;
        let directory = job.db.directory.clone();
        let task_span = tracing::info_span!("file_task", task_id = %directory.file_name().unwrap_or_default().to_string_lossy());
        let _task_entered = task_span.enter();
        tracing::info!(
            event = "task_created",
            state = "analyzing",
            "分析任务已建立"
        );
        let result = (|| {
            // C-01：目录整理的分析阶段只读——不解压（解压职责整体移交「递归解压」工具，X-01），
            // 扫描即终态；树里既有压缩包按普通文件参与后续去重/归类。
            *stage = "scan";
            scan(&mut job, false, state, protected.as_deref())?;
            // C-01：先确定永久清理项并退出活动候选，再计算存活项的完整哈希。
            *stage = "cleanup_plan";
            job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
            match planner::cleanup_candidates(&mut job) {
                Ok(()) => job.db.conn.execute_batch("COMMIT")?,
                Err(error) => {
                    if job.db.conn.execute_batch("ROLLBACK").is_err() {
                        tracing::warn!(
                            event = "task_transaction_cleanup_failed",
                            stage = "cleanup_plan_rollback",
                            error_type = "database",
                            "清理计划失败后的事务回滚失败"
                        );
                    }
                    return Err(error);
                }
            }
            *stage = "hash";
            hash_candidates(&mut job)?;
            job.context
                .status("生成去重、冲突、归类和清理计划（尚未执行这些操作）");
            *stage = "plan";
            job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
            match planner::build_after_cleanup(&mut job) {
                Ok(()) => job.db.conn.execute_batch("COMMIT")?,
                Err(error) => {
                    if job.db.conn.execute_batch("ROLLBACK").is_err() {
                        tracing::warn!(
                            event = "task_transaction_cleanup_failed",
                            stage = "plan_rollback",
                            error_type = "database",
                            "计划失败后的事务回滚失败"
                        );
                    }
                    return Err(error);
                }
            }
            job.context.control.checkpoint()?;
            *stage = "finalize_database";
            job.db.set("summary", &job.summary)?;
            job.db.set("status", &"ready")?;
            job.log(
                "计划",
                "",
                "",
                "待确认",
                "分析完成（只读，未改动任何文件）；去重、移动和清理等待确认",
                0,
            )?;
            #[cfg(feature = "perf-tracing")]
            jchtools_foundation::perf::analyze_done(
                job.summary.scanned,
                job.summary.scanned_bytes,
                job.summary.errors,
            );
            tracing::info!(
                event = "organize_analyze_summary",
                scanned = job.summary.scanned,
                bytes = job.summary.scanned_bytes,
                errors = job.summary.errors,
                "目录整理分析完成"
            );
            Ok(TaskResult {
                directory: directory.clone(),
                summary: job.summary.clone(),
            })
        })();
        if let Err(error) = &result {
            record_task_failure(&mut job, error);
        }
        result
    })
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "hash", skip_all)
)]
fn hash_candidates(job: &mut Job) -> Result<()> {
    let control = job.context.control.clone();
    observe("hash_candidates", Some(&control), |stage| {
        *stage = "hash_analysis";
        if !job.config.dedup_same_name
            && !job.config.dedup_copy_names
            && !job.config.dedup_other_names
        {
            return Ok(());
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(job.config.hash_workers)
            .thread_name(|i| format!("organizer-hash-{i}"))
            .build()?;
        job.context.status("计算候选文件的完整 Hash");
        job.db.conn.execute_batch("DROP TABLE IF EXISTS hash_candidates; CREATE TEMP TABLE hash_candidates(id INTEGER PRIMARY KEY);")?;
        // C-12：候选范围只依据扫描期已获得的信息（大小与启用的名称关系），完整哈希在一次
        // 遍历内完成，不设预哈希/完整哈希两阶段。同尺寸是安全下界；仅在不同名去重开启时
        // 扩到全部同尺寸项。副本名桶须含不同名称，合格桶仍保留全部同名桥接节点。
        if job.config.dedup_other_names {
            // 整条 SQL 保持单个字符串字面量（不含花括号）：static_check 的 sql_syntax
            // 逐字面量抽取校验，跨字面量拼接会让尾段单独受检而误报。
            const SQL: &str = "INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND size IN (SELECT size FROM files WHERE active=1 GROUP BY size HAVING COUNT(*)>1)";
            job.db.conn.execute(SQL, [])?;
        } else {
            const SAME_NAME: &str = "INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND (size,name) IN (SELECT size,name FROM files WHERE active=1 GROUP BY size,name HAVING COUNT(*)>1)";
            const COPY_NAME: &str = "INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND (size,normal) IN (SELECT size,normal FROM files WHERE active=1 GROUP BY size,normal HAVING COUNT(DISTINCT name)>1)";
            if job.config.dedup_same_name {
                job.db.conn.execute(SAME_NAME, [])?;
            }
            if job.config.dedup_copy_names {
                job.db.conn.execute(COPY_NAME, [])?;
            }
        }
        // C-13：跨运行哈希缓存放在状态目录根（每次 prepare 都新建任务库，缓存必须
        // 跨任务存活）。打开失败只降级为「无缓存、全部重算」，不得阻断分析。
        let mut cache = if let Ok(dir) = job.db.get::<String>("state_dir") {
            match jchtools_file_core::hash_cache::HashCache::open(Path::new(&dir)) {
                Ok(cache) => Some(cache),
                Err(error) => {
                    tracing::warn!(
                        event = "hash_cache_fallback",
                        stage = "open",
                        fallback = "recompute_all",
                        error_type = "cache_database",
                        "哈希缓存不可用，全部重新计算"
                    );
                    job.log(
                        "Hash",
                        "",
                        "",
                        "提示",
                        &format!("哈希缓存不可用，本次全部重新计算（{error:#}）"),
                        0,
                    )?;
                    None
                }
            }
        } else {
            tracing::warn!(
                event = "hash_cache_fallback",
                stage = "state_directory",
                fallback = "recompute_all",
                error_type = "task_database",
                "缓存目录信息不可用，全部重新计算"
            );
            None
        };
        let mut reused_total: u64 = 0;
        let mut cursor = 0;
        // 分页 SQL 整个哈希阶段逐字不变：循环外构造一次，配合 db::files 的 prepare_cached
        // 让每页都命中语句缓存（不再逐页 format! 与解析）。
        let sql = format!(
            "SELECT {} FROM hash_candidates AS c CROSS JOIN files AS f ON f.id=c.id \
     WHERE c.id>?1 AND f.active=1 ORDER BY c.id LIMIT ?2",
            jchtools_file_core::db::file_columns_qualified("f")
        );
        loop {
            job.context.control.checkpoint()?;
            // 候选表按主键游标推进，并用 CROSS JOIN 固定 hash_candidates 为外层扫描表：
            // 旧的 `id IN (SELECT id FROM hash_candidates)` 会让每次分页都重扫整个候选集合
            // （实测每次调用成本随游标位置线性增长，累计平方级）。取数批量与哈希线程数解耦，
            // 避免「调线程数」同时改变两个量。
            let batch = job.db.files(
                &sql,
                params![
                    cursor,
                    jchtools_file_core::convert::usize_as_i64(HASH_BATCH)
                ],
            )?;
            let Some(last) = batch.last() else {
                break;
            };
            cursor = last.id;
            let root = &job.root;
            let control = &job.context.control;
            // C-13：先查跨运行缓存，命中的候选不再读取文件内容。缓存故障一次性
            // 降级为「其后全部重算」——缓存只是加速，正确性不依赖它。
            // resume 是首个未消化的候选下标：缓存不可用/读取失败时从它起全部转入重算。
            let mut reused: Vec<(i64, String)> = Vec::new();
            let mut compute: Vec<&_> = Vec::new();
            let mut resume = batch.len();
            for (index, file) in batch.iter().enumerate() {
                let Some(cache_conn) = cache.as_ref() else {
                    resume = index;
                    break;
                };
                let Ok(size) = i64::try_from(file.snapshot.size) else {
                    compute.push(file);
                    continue;
                };
                if jchtools_file_core::hash_cache::identity_is_degenerate(&file.snapshot.identity) {
                    compute.push(file);
                    continue;
                }
                match cache_conn.lookup(&file.snapshot.identity, size, file.snapshot.modified_ns) {
                    Ok(Some(hash)) => reused.push((file.id, hash)),
                    Ok(None) => compute.push(file),
                    Err(error) => {
                        cache = None;
                        tracing::warn!(
                            event = "hash_cache_fallback",
                            stage = "lookup",
                            fallback = "recompute_remaining",
                            error_type = "cache_database",
                            "哈希缓存读取失败，剩余候选重新计算"
                        );
                        job.log(
                            "Hash",
                            "",
                            "",
                            "提示",
                            &format!("哈希缓存读取失败，其后全部重新计算（{error:#}）"),
                            0,
                        )?;
                        resume = index;
                        break;
                    }
                }
            }
            compute.extend(batch[resume..].iter());
            reused_total = reused_total.saturating_add(u64::try_from(reused.len()).unwrap_or(0));
            let hash_span = tracing::Span::current();
            let results: Vec<_> = pool.install(|| {
                compute
                    .par_iter()
                    .map(|file| {
                        let _entered = hash_span.enter();
                        let item_span = tracing::info_span!("hash_item", file_id = file.id);
                        let _item_entered = item_span.enter();
                        let result = (|| {
                            let path = fsutil::safe_join(root, &file.rel)?;
                            hashing::full_hash(&path, control)
                        })();
                        (file.id, file.rel.clone(), result)
                    })
                    .collect()
            });
            job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
            let update = (|| {
                // 用户取消时并行哈希的每个成员都会返回取消错误；先在这里拦截，
                // 否则每个候选都被计成 error 并逐条写日志，取消任务的错误数虚高。
                job.context.control.checkpoint()?;
                // C-13：缓存命中的哈希与刚算出的走完全相同的落库路径，planner 无感知。
                for (id, hash) in &reused {
                    job.db.set_file_hash(*id, hash)?;
                }
                for (id, rel, result) in &results {
                    match result {
                        Ok(hash) => {
                            job.db.set_file_hash(*id, hash)?;
                        }
                        Err(error) => {
                            tracing::warn!(event = "hash_item_failed", file_id = id,
                        source = %jchtools_foundation::logging::safe_error(rel), stage = "full_hash", error_type = "hash_io_boundary",
                        error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
                        "文件哈希计算失败，取消该候选");
                            job.summary.errors += 1;
                            job.db.deactivate_file_id(*id)?;
                            job.log("Hash", rel, "", "跳过", &format!("{error:#}"), 0)?;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            })();
            match update {
                Ok(()) => job.db.conn.execute_batch("COMMIT")?,
                Err(error) => {
                    if job.db.conn.execute_batch("ROLLBACK").is_err() {
                        tracing::warn!(
                            event = "task_transaction_cleanup_failed",
                            stage = "hash_rollback",
                            error_type = "database",
                            "哈希失败后的事务回滚失败"
                        );
                    }
                    return Err(error);
                }
            }
            // C-13：把本次新算出的哈希写入跨运行缓存；写失败同样只降级、不失败。
            if let Some(cache_conn) = cache.as_ref() {
                let by_id: HashMap<i64, &_> = batch.iter().map(|file| (file.id, file)).collect();
                let entries: Vec<(String, i64, i64, String)> = results
                    .iter()
                    .filter_map(|(id, _, result)| {
                        let hash = result.as_ref().ok()?;
                        let file = by_id.get(id)?;
                        let size = i64::try_from(file.snapshot.size).ok()?;
                        Some((
                            file.snapshot.identity.clone(),
                            size,
                            file.snapshot.modified_ns,
                            hash.clone(),
                        ))
                    })
                    .collect();
                if let Err(error) = cache_conn.store(&entries) {
                    cache = None;
                    tracing::warn!(
                        event = "hash_cache_fallback",
                        stage = "store",
                        fallback = "disable_reuse",
                        error_type = "cache_database",
                        "哈希缓存写入失败，禁用后续复用"
                    );
                    job.log(
                        "Hash",
                        "",
                        "",
                        "提示",
                        &format!("哈希缓存写入失败，其后不再复用（{error:#}）"),
                        0,
                    )?;
                }
            }
        }
        if reused_total > 0 {
            job.log(
                "Hash",
                "",
                "",
                "提示",
                &format!(
            "复用上次运行的哈希 {reused_total} 个（文件标识/大小/修改时间未变，未重读内容）"
        ),
                0,
            )?;
        }
        Ok(())
    })
}
/// apply 的锁目录决策：优先 prepare 记录的全局锁目录；记录缺失或失效时，仅当任务
/// 目录仍处于「…/tasks/<任务>」布局才退回上一级推导（搬到别的机器后放回新机器的
/// tasks/ 布局仍可执行），其余位置一律拒绝——不得在陌生位置创建锁文件。
/// 布局判定成立时 derived 必然已存在（它包含 tasks/ 这一级），无需再查 is_dir。
fn lock_dir_for(directory: &Path, recorded: Option<&Path>) -> Result<PathBuf> {
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
fn dir_recheck_empty(path: &Path) -> Result<bool> {
    Ok(path.try_exists()? && path.is_dir() && fs::read_dir(path)?.next().is_none())
}
fn path_is_at_or_below(path: &str, root: &str) -> bool {
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
fn task_git_roots(db: &Database) -> Result<Vec<String>> {
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
                let index = occupied.partition_point(|item| item.as_str() < probe.as_str());
                let blocked_by_occupied = occupied
                    .get(index)
                    .is_some_and(|item| item.starts_with(probe.as_str()));
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
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "organize_apply", skip_all)
)]
pub fn apply(directory: &Path, context: TaskContext) -> Result<TaskResult> {
    let control = context.control.clone();
    observe("organize_apply", Some(&control), |stage| {
        let task_span = tracing::info_span!("file_task", task_id = %jchtools_foundation::logging::safe_error(&directory.file_name().unwrap_or_default().to_string_lossy()));
        let _task_entered = task_span.enter();
        // Database::open 会在文件缺失时创建一个空库；先确认这是扫描生成的任务目录，
        // 避免把任意目录（甚至写错路径）悄悄变成一个必然失败的空任务。
        // 该检查必须先于 RootGuard：否则写错路径会先在错误位置创建目录并落下锁文件。
        anyhow::ensure!(
            directory.join("task.sqlite3").is_file(),
            "目录里没有 task.sqlite3；请选择「开始解压与分析」生成的任务目录"
        );
        *stage = "task_database";
        let db = Database::open(directory)?;
        // 锁位置：prepare 已把当时的全局锁目录记进任务库（决策规则见 lock_dir_for）。
        let recorded: Option<PathBuf> = if let Ok(path) = db.get::<String>("state_dir") {
            Some(PathBuf::from(path))
        } else {
            tracing::warn!(
                event = "task_lock_fallback",
                stage = "recorded_directory",
                fallback = "task_layout",
                error_type = "task_database",
                "任务锁目录记录不可用，按既有任务布局推导"
            );
            None
        };
        *stage = "task_lock";
        let _guard = fsutil::RootGuard::acquire(&lock_dir_for(directory, recorded.as_deref())?)?;
        let status: String = db.get("status")?;
        if status != "ready" {
            bail!("任务不是待确认状态（{status}）；请重新扫描，不会盲目重放旧计划");
        }
        *stage = "input_validation";
        let root_text: String = db.get("root")?;
        let protection = resolve_protection()?;
        let (root, protected) =
            fsutil::normalize_root_with(Path::new(&root_text), protection.as_deref())?;
        // prepare 记录的是当时的规范化路径；apply 重新解析 canonicalize（会解析 junction）。
        // 两者不一致说明目录被移动或被替换成指向别处的链接，继续执行会把整理动作落到另一棵树上。
        anyhow::ensure!(
            fsutil::path_string(&root)? == root_text,
            "目录位置已改变或被链接替换（{root_text}）；请重新扫描生成新计划后再执行"
        );
        let config = db.config()?;
        config.validate_organizer()?;
        let summary = db.summary()?;
        // 执行阶段新增错误的判定基线：分析阶段已确认的错误（扫描跳过、派生名失败等）
        // 不把「执行全部成功」的任务改判失败，但执行/清理阶段新增任一错误不得标
        let errors_before_apply = summary.errors;
        let mut job = Job {
            root,
            config,
            context,
            db,
            summary,
        };
        *stage = "executing_status";
        job.db.set("status", &"executing")?;
        let outcome = (|| {
            job.context.control.checkpoint()?;
            if !job.config.recursive {
                // 仅本次连接记录归类实际新建的目标目录，不借固定名称或历史任务
                // 推断所有权；原有后代始终在非递归收尾范围之外（H-05/R-03）。
                job.db.conn.execute_batch(
                    "CREATE TEMP TABLE organizer_created_dirs(rel TEXT PRIMARY KEY);",
                )?;
            }
            // 性能打点（perf-tracing，默认不编译）：计划动作的实际执行区间，
            // 与后面的收尾写库分开计时。
            jchtools_foundation::perf::perf_span!("execute_actions");
            let pool = io_pool()?;
            *stage = "execute_actions";
            let mut moves_executed = false;
            let mut cursor = 0;
            loop {
                let actions = job.db.actions_page(cursor, APPLY_BATCH)?;
                if actions.is_empty() {
                    break;
                }
                // 一批一个事务（见 APPLY_BATCH）。事务里写库只影响「任务库记录的可见时机」，
                // 文件改动本身不受事务保护，因此：
                // - 出错/取消时提交已完成的部分（文件已经删了/移了，丢掉记录只会让计划行
                //   状态与磁盘不一致，C-11）；SQLite 语句级失败不会中断事务，提交是安全的；
                // - 崩溃/强杀时最多丢失当前一批（<64 项）的状态与审计行，此时任务不可能从
                //   「执行中」恢复（C-10 无断点恢复），不存在重复删除的可能。
                job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
                let batch = (|| -> Result<()> {
                    // 分段执行：文件删除彼此独立、空目录删除按深度分层（计划按深度降序生成，
                    // 同层目录不互为父子，更深层已在此前的段/页删除）——这两类连续动作在
                    // IO 池上并行；移动/硬链接/未勾选保持逐项串行，语义与旧实现一致。
                    let mut index = 0;
                    while index < actions.len() {
                        // 移动动作不能只按计划行号执行：当一个目标正是另一个移动的
                        // 源时，必须先释放源；交换环则由 execute_planned_moves 临时
                        // 暂存一个源后完成。该预处理只执行一次，之后本页及后续页的
                        // Move 行已经标记 done/failed，跳过即可。
                        if actions[index].kind == ActionKind::Move {
                            if !moves_executed {
                                execute_planned_moves(&mut job)?;
                                moves_executed = true;
                            }
                            index += 1;
                            continue;
                        }
                        if actions[index].state != "pending" {
                            index += 1;
                            continue;
                        }
                        let Some(kind) = parallel_run_kind(&actions[index]) else {
                            execute_sequential(&mut job, &actions[index])?;
                            index += 1;
                            continue;
                        };
                        let mut end = index + 1;
                        while end < actions.len() && parallel_run_kind(&actions[end]) == Some(kind)
                        {
                            end += 1;
                        }
                        execute_run(&mut job, &pool, &actions[index..end], kind)?;
                        index = end;
                    }
                    Ok(())
                })();
                match batch {
                    Ok(()) => job.db.conn.execute_batch("COMMIT")?,
                    Err(error) => {
                        // 尽力提交：SQLite 已因致命错误自行回滚时 COMMIT 会失败，此时无需处理。
                        if job.db.conn.execute_batch("COMMIT").is_err() {
                            tracing::warn!(
                                event = "task_transaction_cleanup_failed",
                                stage = "partial_apply_commit",
                                error_type = "database",
                                "执行失败后保存已完成事务的尝试失败"
                            );
                        }
                        return Err(error);
                    }
                }
                cursor = actions[actions.len() - 1].id;
            }
            // H-05/C-07：计划动作执行完总是做最终实空清理（不可关闭、不受文件清理/删除
            // 方式选择影响）：既覆盖本次新产生的空目录与空目录链，也覆盖从未入库的目录
            // （例如实际为空的「解压失败」暂存区、归类新建但没落文件的目录）。
            *stage = "empty_cleanup";
            final_empty_cleanup(&mut job, protected.as_deref())?;
            Ok::<_, anyhow::Error>(())
        })();
        if outcome.is_err() {
            if job.context.control.is_cancelled() {
                tracing::info!(
                    event = "apply_execution_cancelled",
                    stage = *stage,
                    errors = job.summary.errors,
                    "整理执行已取消，开始保存已有结果"
                );
            } else {
                tracing::error!(
                    event = "apply_execution_failed",
                    stage = *stage,
                    errors = job.summary.errors,
                    "整理执行未完成，开始保存已有结果"
                );
            }
        }
        *stage = "finalize_database";
        job.db.set("summary", &job.summary)?;
        job.db.set(
            "status",
            // C-11/C-01：执行阶段有计划行失败（errors 新增）的任务不得标「finished」；
            // 用户取消优先于失败口径（C-10 取消不算失败）。
            &if outcome.is_ok() && job.summary.errors == errors_before_apply {
                "finished"
            } else if job.context.control.is_cancelled() {
                "cancelled"
            } else {
                "failed"
            },
        )?;
        tracing::info!(
            event = "task_state_changed",
            state = if outcome.is_ok() && job.summary.errors == errors_before_apply {
                "finished"
            } else if job.context.control.is_cancelled() {
                "cancelled"
            } else {
                "failed"
            },
            deleted = job.summary.deleted,
            moved = job.summary.moved,
            skipped = job.summary.skipped,
            errors = job.summary.errors,
            "整理执行终态已记录"
        );
        *stage = "execution_outcome";
        outcome?;
        #[cfg(feature = "perf-tracing")]
        jchtools_foundation::perf::apply_done(
            job.summary.deleted,
            job.summary.moved,
            job.summary.skipped,
            job.summary.errors,
        );
        tracing::info!(
            event = "organize_apply_summary",
            outcome = if job.summary.errors == errors_before_apply {
                "success"
            } else {
                "with_failures"
            },
            deleted = job.summary.deleted,
            moved = job.summary.moved,
            skipped = job.summary.skipped,
            errors = job.summary.errors,
            "目录整理执行完成"
        );
        Ok(TaskResult {
            directory: directory.to_path_buf(),
            summary: job.summary,
        })
    })
}
/// 整理收尾的实空清理（H-05/C-07）：自底向上删除实际为空的目录及空目录链。
/// 「总是执行」：没有配置开关可关闭，也不受文件清理/删除方式选择影响——空目录清理
/// 不属于 C-08 的六类清理项，H-05 不提供关闭这一步的选项。只尊重：选定根目录、
/// Git 整树排除（H-06）、递归范围、用户排除与隐藏/系统开关、状态目录/程序目录/
/// 工具工作目录、Windows 系统目录子树（S-05），以及取消；一律永久删除（S-02）。
/// 与扫描共用同一套范围口径（[`ScopeFilter`]），所以「解压失败」暂存区内实际为空的
/// 目录也按 H-05 清理（C-09），但其内容一律不碰——判定只看目录项是否为空。
fn final_empty_cleanup(job: &mut Job, protected: Option<&Path>) -> Result<()> {
    let control = job.context.control.clone();
    observe("final_empty_cleanup", Some(&control), |stage| {
        *stage = "empty_directory_sweep";
        let deleted_before = job.summary.deleted;
        let result: Result<()> = (|| {
            job.context.control.checkpoint()?;
            let errors_before_cleanup = job.summary.errors;
            let excluded = rules::build_exclusions(&job.config.exclusions)?;
            let executable_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().and_then(|d| fs::canonicalize(d).ok()));
            let state = job
                .db
                .get::<String>("state_dir")
                .ok()
                .map(PathBuf::from)
                .and_then(|directory| fs::canonicalize(directory).ok());
            let (state_prefix, exe_prefix, root_under_special) =
                special_prefixes(&job.root, state.as_deref(), executable_dir.as_deref());
            if root_under_special {
                // 选定根位于状态目录/程序目录内：扫描整树剪枝，这里同样不处理任何条目。
                return Ok(());
            }
            // S-05：根包含 Windows 系统目录时该子树整树剪枝，清理与扫描同一口径。
            let protected_prefix =
                protected.and_then(|dir| fsutil::relative_string(&job.root, dir).ok());
            let scope_filter = ScopeFilter {
                excluded: &excluded,
                include_hidden: job.config.include_hidden,
                include_system: job.config.include_system,
                // C-09：暂存区内容不参与处理，但实际为空的目录仍按 H-05 清理。
                quarantine: None,
                state_prefix,
                exe_prefix,
                protected_prefix,
                root_under_special,
            };
            let recursive = job.config.recursive;
            let created_dirs: HashSet<String> = if recursive {
                HashSet::new()
            } else {
                let mut statement = job
                    .db
                    .conn
                    .prepare("SELECT rel FROM organizer_created_dirs")?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let mut stack: Vec<SweepFrame> = Vec::new();
            // 选定根目录本身绝不删除（H-05）：它的条目决定深度 1 的候选。
            let (children, keep, notes) =
                sweep_entries(&scope_filter, recursive, &created_dirs, &job.root, "");
            for note in notes {
                job.summary.errors += 1;
                job.log("清理", "", "", "跳过", &note, 0)?;
            }
            stack.push(SweepFrame {
                path: job.root.clone(),
                rel: String::new(),
                children,
                next: 0,
                keep,
            });
            while !stack.is_empty() {
                // 后序遍历：先把全部子目录处理完，再决定当前目录是否实际为空。
                let index = stack.len() - 1;
                if stack[index].next < stack[index].children.len() {
                    let (path, rel) = stack[index].children[stack[index].next].clone();
                    stack[index].next += 1;
                    let (children, keep, notes) =
                        sweep_entries(&scope_filter, recursive, &created_dirs, &path, &rel);
                    for note in notes {
                        job.summary.errors += 1;
                        job.log("清理", &rel, "", "跳过", &note, 0)?;
                    }
                    stack.push(SweepFrame {
                        path,
                        rel,
                        children,
                        next: 0,
                        keep,
                    });
                    continue;
                }
                let Some(frame) = stack.pop() else {
                    break;
                };
                if frame.rel.is_empty() {
                    // 选定根目录本身绝不删除（H-05）。
                    continue;
                }
                job.context.control.checkpoint()?;
                if !sweep_remove(job, &frame)? {
                    // 目录没能删除（非空/失败/超出范围）：父目录因此不算实际为空。
                    if let Some(parent) = stack.last_mut() {
                        parent.keep = true;
                    }
                }
            }
            anyhow::ensure!(
                job.summary.errors == errors_before_cleanup,
                "空目录清理未完成：有目录无法读取或删除，详情见进度与日志"
            );
            Ok(())
        })();
        tracing::debug!(
            event = "empty_cleanup_summary",
            deleted = job.summary.deleted.saturating_sub(deleted_before),
            errors = job.summary.errors,
            "整理收尾空目录清理统计"
        );
        result
    })
}
/// 删除一个收尾清理候选目录，返回是否真的删掉了（没删掉时父目录不算实际为空）。
/// 一律永久删除（S-02）；这里不看用户勾选状态——H-05 的清理是强制步骤。
fn sweep_remove(job: &mut Job, frame: &SweepFrame) -> Result<bool> {
    if frame.keep {
        return Ok(false);
    }
    // 执行期复查（与计划动作同口径）：platform::remove 还会再确认一次目录为空。
    match platform::remove(&frame.path, DeleteMode::Permanent, &job.context.control) {
        Ok(DeleteResult::Permanent) => {
            job.summary.deleted += 1;
            job.log(
                "清理",
                &frame.rel,
                "",
                "已永久删除",
                "整理收尾：目录实际为空（H-05）",
                0,
            )?;
            Ok(true)
        }
        Ok(DeleteResult::Kept) => {
            job.summary.skipped += 1;
            job.log("清理", &frame.rel, "", "保留", "删除方式为保留", 0)?;
            Ok(false)
        }
        Err(error) => {
            // 用户主动取消不是失败（C-10）：直接以取消错误收尾，不记为错误。
            if job.context.control.is_cancelled() {
                job.context.control.check_cancelled()?;
            }
            tracing::warn!(event = "empty_cleanup_item_failed", stage = "directory_remove",
                source = %jchtools_foundation::logging::safe_error(&frame.rel), error_type = "filesystem_boundary",
                error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
                "整理收尾空目录删除失败，保留该目录");
            job.summary.errors += 1;
            job.log("清理", &frame.rel, "", "失败", &format!("{error:#}"), 0)?;
            Ok(false)
        }
    }
}
/// 收尾清理的一个待处理目录：读条目、处理完全部子目录后再决定它是否实际为空。
struct SweepFrame {
    path: PathBuf,
    rel: String,
    /// 需要下钻处理的子目录（按目录项枚举序）。
    children: Vec<(PathBuf, String)>,
    next: usize,
    /// 该目录里存在不会随本次清理消失的条目（普通文件、链接、被范围剪枝的条目、
    /// Git 目录、不可读条目，或递归范围之外的子目录）：存在这样的条目即「不是空目录」。
    keep: bool,
}
/// 读取收尾清理候选目录的条目：返回（需下钻的子目录、是否确定不会为空、记录的跳过原因）。
/// `collect` 为假时只接纳本轮实际新建的目标目录；原有后代不下钻、不清理，
/// 其条目仍会使父目录不算空（H-05/R-03）。
fn sweep_entries(
    scope_filter: &ScopeFilter<'_>,
    collect: bool,
    created_dirs: &HashSet<String>,
    dir: &Path,
    rel: &str,
) -> (Vec<(PathBuf, String)>, bool, Vec<String>) {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) => {
            tracing::warn!(
                component = "engine",
                event = "empty_cleanup_item_failed",
                stage = "directory_read",
                source = %jchtools_foundation::logging::safe_error(rel),
                error_type = "io",
                error_code = error.raw_os_error(),
                "整理收尾目录读取失败，按保留处理"
            );
            return (Vec::new(), true, vec![format!("{error:#}")]);
        }
    };
    let mut children = Vec::new();
    let mut keep = false;
    let mut notes = Vec::new();
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                // 枚举中断：该目录未必为空，按保留处理。
                tracing::warn!(
                    component = "engine",
                    event = "empty_cleanup_item_failed",
                    stage = "directory_entry",
                    source = %jchtools_foundation::logging::safe_error(rel),
                    error_type = "io",
                    error_code = error.raw_os_error(),
                    "整理收尾目录项读取失败，按保留处理"
                );
                keep = true;
                notes.push(format!("{error:#}"));
                continue;
            }
        };
        let Ok(name) = entry.file_name().into_string() else {
            tracing::warn!(
                component = "engine",
                event = "empty_cleanup_item_failed",
                stage = "file_name",
                source = %jchtools_foundation::logging::safe_error(&entry.path().to_string_lossy()),
                error_type = "invalid_utf8",
                "整理收尾目录项名称不能无损表示，按保留处理"
            );
            keep = true;
            continue;
        };
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(metadata) = entry.metadata() else {
            tracing::warn!(
                component = "engine",
                event = "empty_cleanup_item_failed",
                stage = "metadata",
                source = %jchtools_foundation::logging::safe_error(&entry.path().to_string_lossy()),
                error_type = "metadata_unavailable",
                "整理收尾目录项元数据不可用，按保留处理"
            );
            keep = true;
            continue;
        };
        if scope_filter.prunes(&child_rel, &name, &metadata) {
            // 超出本次处理范围的条目仍在盘上：本目录不是实际为空，也不会被删除。
            keep = true;
            continue;
        }
        if !metadata.is_dir() {
            keep = true;
            continue;
        }
        if !collect && !created_dirs.contains(&child_rel) {
            // 未纳入的既有后代保留；分类本轮新建目标目录仍须收尾（H-05）。
            keep = true;
            continue;
        }
        match fsutil::is_git_root(&entry.path()) {
            // H-06：目录直接含 .git 时整树排除，不遍历内部，也不清理它。
            Ok(true) => keep = true,
            Ok(false) => match fsutil::is_extraction_workspace(&entry.path()) {
                // 仅标记完整的本工具工作区受保护；普通同名用户目录照常清理实空目录。
                Ok(true) => keep = true,
                Ok(false) => children.push((entry.path(), child_rel)),
                Err(error) => {
                    tracing::warn!(
                        component = "engine",
                        event = "empty_cleanup_item_failed",
                        stage = "workspace_boundary",
                        source = %jchtools_foundation::logging::safe_error(&child_rel),
                        error_type = "filesystem_boundary",
                        error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
                        "整理收尾无法判定工作区边界，按保留处理"
                    );
                    keep = true;
                    notes.push(format!("{}：{error:#}", entry.path().display()));
                }
            },
            Err(error) => {
                tracing::warn!(
                    component = "engine",
                    event = "empty_cleanup_item_failed",
                    stage = "git_boundary",
                    source = %jchtools_foundation::logging::safe_error(&child_rel),
                    error_type = "filesystem_boundary",
                    error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
                    "整理收尾无法判定Git边界，按保留处理"
                );
                keep = true;
                notes.push(format!("{}：{error:#}", entry.path().display()));
            }
        }
    }
    (children, keep, notes)
}
/// 可并行执行的动作段类别：文件删除彼此独立；空目录删除按深度分层
/// （计划按深度降序生成，同层目录不互为父子，深层已在更早的段/页删除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParallelKind {
    Delete,
    EmptyDir(usize),
}
fn parallel_run_kind(action: &Action) -> Option<ParallelKind> {
    if !action.selected {
        return None;
    }
    match action.kind {
        ActionKind::Delete => Some(ParallelKind::Delete),
        ActionKind::EmptyDirectory => Some(ParallelKind::EmptyDir(
            action.source.bytes().filter(|byte| *byte == b'/').count(),
        )),
        ActionKind::Move => None,
    }
}
/// 单个动作的串行执行（移动/硬链接/未勾选）：语义与旧逐项循环一致。
/// 动作失败结算：用户主动取消不算失败（不计 errors、不标 failed，与 prepare
/// 阶段取消口径一致）；其余错误计 errors、落 failed、写统一格式的失败日志。
/// 串行路径与并行段结算共用同一口径，防止两处取消/失败判定漂移。
fn record_action_failure(job: &mut Job, action: &Action, error: &anyhow::Error) -> Result<()> {
    if job.context.control.is_cancelled() {
        job.context.control.check_cancelled()?;
    }
    tracing::error!(event = "action_failed", action_id = action.id, action_kind = ?action.kind,
        stage = "execute", error_type = "action_boundary",
        error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
        "计划动作执行失败");
    job.summary.errors += 1;
    job.db.mark_action(action.id, "failed")?;
    // 失败日志统一 phase「执行」、带目标，便于按口径筛选。
    job.log(
        "执行",
        &action.source,
        action.target.as_deref().unwrap_or(""),
        "失败",
        &format!("{error:#}"),
        0,
    )?;
    job.context.control.check_cancelled()?;
    Ok(())
}
fn execute_sequential(job: &mut Job, action: &Action) -> Result<()> {
    let span =
        tracing::info_span!("task_action", action_id = action.id, action_kind = ?action.kind);
    let _entered = span.enter();
    let control = job.context.control.clone();
    observe("execute_action", Some(&control), |stage| {
        *stage = "action_execution_and_record";
        job.context.control.checkpoint()?;
        if !action.selected {
            job.summary.skipped += 1;
            job.db.mark_action(action.id, "unselected")?;
            // 未勾选不计入 completed：GUI 分母 count_selected_pending 只含 selected+pending，
            // 分子若含 unselected 会出现 done>planned、提前 100% 的口径分裂。
            return Ok(());
        }
        job.context
            .status(format!("执行 {:?}：{}", action.kind, action.source));
        match execute_action(job, action) {
            Ok(true) => {
                job.db.mark_action(action.id, "done")?;
            }
            Ok(false) => {
                job.summary.skipped += 1;
                job.db.mark_action(action.id, "skipped")?;
            }
            Err(error) => record_action_failure(job, action, &error)?,
        }
        job.context
            .control
            .completed
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    })
}
/// 并行执行一段相互独立的删除类动作：先整段记审计（意图先于任何变更落账），
/// 再在 IO 池上并行做文件系统变更，最后按段内顺序串行结算（日志、计数、任务库状态）。
/// 路径只用字符串级校验（safe_relative）：P-08 假定处理期间文件不被其他程序改动，
/// 且 platform::remove 自身复查链接并拒绝非空目录；写入类动作仍走 safe_join 全校验。
fn execute_run(
    job: &mut Job,
    pool: &rayon::ThreadPool,
    run: &[Action],
    kind: ParallelKind,
) -> Result<()> {
    job.context.control.checkpoint()?;
    job.context.status(if run.len() > 1 {
        format!(
            "执行 {:?}：{} 等 {} 项",
            run[0].kind,
            run[0].source,
            run.len()
        )
    } else {
        format!("执行 {:?}：{}", run[0].kind, run[0].source)
    });
    let size = |action: &Action| action.expected.as_ref().map_or(0, |snapshot| snapshot.size);
    for action in run {
        let action_span =
            tracing::info_span!("task_action", action_id = action.id, action_kind = ?action.kind);
        let _action_entered = action_span.enter();
        job.log(
            "删除",
            &action.source,
            "",
            "准备",
            &action.reason,
            size(action),
        )?;
    }
    let mut targets = Vec::with_capacity(run.len());
    for action in run {
        let path = job.root.join(fsutil::safe_relative(&action.source)?);
        targets.push((path, action.mode, action.id));
    }
    let control = job.context.control.clone();
    let parent_span = tracing::Span::current();
    let results: Vec<_> = pool.install(|| {
        targets
            .par_iter()
            .map(|(path, mode, action_id)| {
                let _entered = parent_span.enter();
                let action_span = tracing::info_span!("task_action", action_id = action_id);
                let _action_entered = action_span.enter();
                observe("parallel_action", Some(&control), |stage| {
                    *stage = "delete_or_empty_recheck";
                    match kind {
                        ParallelKind::Delete => platform::remove(path, *mode, &control).map(Some),
                        ParallelKind::EmptyDir(_) => {
                            // 执行期实空复查（同旧 EmptyDirectory 分支）；同段目录互不嵌套，可并行。
                            if !dir_recheck_empty(path)? {
                                return Ok(None);
                            }
                            platform::remove(path, *mode, &control).map(Some)
                        }
                    }
                })
            })
            .collect()
    });
    settle_parallel_results(job, run, results)
}

fn settle_parallel_results(
    job: &mut Job,
    run: &[Action],
    results: Vec<Result<Option<DeleteResult>>>,
) -> Result<()> {
    let size = |action: &Action| action.expected.as_ref().map_or(0, |snapshot| snapshot.size);
    let mut cancellation = None;
    for (action, result) in run.iter().zip(results) {
        let action_span =
            tracing::info_span!("task_action", action_id = action.id, action_kind = ?action.kind);
        let _action_entered = action_span.enter();
        match result {
            Err(error) if job.context.control.is_cancelled() => {
                // 并行文件操作已全部返回；先登记同批成功项，再按取消收尾。
                // 未启动/未完成的取消项保持 pending，不计失败或完成数。
                cancellation.get_or_insert(error);
                continue;
            }
            Err(error) => {
                if let Err(error) = record_action_failure(job, action, &error) {
                    if !job.context.control.is_cancelled() {
                        return Err(error);
                    }
                    cancellation.get_or_insert(error);
                    continue;
                }
            }
            // 空目录实空复查未过（已不存在/非目录/非空）：按旧口径计跳过、不写结果日志。
            Ok(None) => {
                job.summary.skipped += 1;
                job.db.mark_action(action.id, "skipped")?;
            }
            Ok(Some(DeleteResult::Kept)) => {
                job.summary.skipped += 1;
                job.log(
                    "删除",
                    &action.source,
                    "",
                    "保留",
                    &action.reason,
                    size(action),
                )?;
                job.db.mark_action(action.id, "skipped")?;
            }
            Ok(Some(DeleteResult::Permanent)) => {
                job.summary.deleted += 1;
                // 硬链接源（links>1）的内容仍由其他链接持有，不计入 permanent_bytes（S-06）；
                // 空目录（expected=None）逻辑大小为 0。
                if action
                    .expected
                    .as_ref()
                    .is_none_or(|snapshot| snapshot.links <= 1)
                {
                    job.summary.permanent_bytes =
                        job.summary.permanent_bytes.saturating_add(size(action));
                }
                job.db.deactivate_file_rel(&action.source)?;
                job.log(
                    "删除",
                    &action.source,
                    "",
                    "已永久删除",
                    &action.reason,
                    size(action),
                )?;
                job.db.mark_action(action.id, "done")?;
            }
        }
        job.context
            .control
            .completed
            .fetch_add(1, Ordering::Relaxed);
    }
    if let Some(error) = cancellation {
        return Err(error);
    }
    Ok(())
}
/// 执行所有归类/项目移动，并处理「目标是另一项源」的依赖。
/// `a -> b` 与 `b -> c` 要先释放依赖源；交换环则先把一个源临时暂存。
/// Git 项目根始终受保护，直到其 C-14 Move 真正成功；失败后拒绝树内目标，独立项继续执行。
fn execute_planned_moves(job: &mut Job) -> Result<()> {
    let mut actions = Vec::new();
    let mut cursor = 0;
    loop {
        let page = job.db.actions_page_filtered(cursor, 256, Some("move"))?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |action| action.id);
        actions.extend(page.into_iter().filter(|action| action.state == "pending"));
    }
    if actions.is_empty() {
        return Ok(());
    }
    let mut protected_git_roots: HashSet<String> = task_git_roots(&job.db)?.into_iter().collect();
    // 包含尚未移动、未勾选或无法完成归类的根；只有成功移动后才从保护集中移除。
    let mut pending_git_moves: HashSet<String> = actions
        .iter()
        .filter(|action| action.selected)
        .map(|action| fsutil::fold_rel(&action.source))
        .filter(|source| protected_git_roots.contains(source))
        .collect();
    // 环形移动暂存在本次任务的状态目录：扫描已排除状态目录，用户根下同名
    // 普通目录仍会参与整理。OWNER 标记限制清理范围，失败时保留唯一副本。
    let owner = uuid::Uuid::new_v4().to_string();
    let work = fsutil::safe_join(&job.db.directory, "move-stage")?;
    fs::create_dir_all(&work)?;
    let stage = work.join(&owner);
    fs::create_dir(&stage)?;
    fs::write(stage.join("OWNER"), &owner)?;
    let stage_guard = MoveStageGuard {
        directory: stage.clone(),
        owner,
    };
    let mut staged: HashMap<i64, PathBuf> = HashMap::new();
    while !actions.is_empty() {
        if let Some(index) = actions.iter().position(|action| !action.selected) {
            let action = actions.remove(index);
            execute_sequential(job, &action)?;
            continue;
        }
        let available = actions.iter().enumerate().find(|(index, action)| {
            let Some(target) = action.target.as_deref() else {
                return true;
            };
            let target = fsutil::fold_rel(target);
            let source = fsutil::fold_rel(&action.source);
            // 普通 Move 暂候选中 Git 根的 C-14 Move；成功后其原路径方可接收归类目标。
            let waits_for_git_move = pending_git_moves
                .iter()
                .any(|root| source.as_str() != root.as_str() && path_is_at_or_below(&target, root));
            !waits_for_git_move
                && !actions.iter().enumerate().any(|(other, candidate)| {
                    other != *index
                        && !staged.contains_key(&candidate.id)
                        && fsutil::fold_rel(&candidate.source) == target
                })
        });
        if let Some((index, _)) = available {
            let action = actions.remove(index);
            let staged_source = staged.remove(&action.id);
            let had_stage = staged_source.is_some();
            let source = match staged_source {
                Some(path) => path,
                None => fsutil::safe_join(&job.root, &action.source)?,
            };
            let folded_source = fsutil::fold_rel(&action.source);
            pending_git_moves.remove(&folded_source);
            let moved = execute_move_sequential(
                job,
                &action,
                &source,
                !had_stage && staged.is_empty(),
                had_stage,
                &protected_git_roots,
            )?;
            if moved {
                protected_git_roots.remove(&folded_source);
            }
            continue;
        }
        // 所有目标都被另一项源占用，形成环；暂存第一项源后，后续动作连续收敛。
        job.context.control.checkpoint()?;
        let action = &actions[0];
        let source = fsutil::safe_join(&job.root, &action.source)?;
        let temporary = stage.join(action.id.to_string());
        if let Err(error) = fsutil::move_file_preserving_times(&source, &temporary) {
            let action = actions.remove(0);
            pending_git_moves.remove(&fsutil::fold_rel(&action.source));
            record_move_failure(job, &action, &error)?;
            job.context
                .control
                .completed
                .fetch_add(1, Ordering::Relaxed);
        } else {
            staged.insert(action.id, temporary);
        }
    }
    drop(stage_guard);
    Ok(())
}

struct MoveStageGuard {
    directory: PathBuf,
    owner: String,
}

impl Drop for MoveStageGuard {
    fn drop(&mut self) {
        if fs::read_to_string(self.directory.join("OWNER"))
            .ok()
            .as_deref()
            != Some(self.owner.as_str())
        {
            return;
        }
        // 暂存恢复失败时目录中可能仍有唯一副本。Drop 绝不能递归删除它；只有 OWNER
        // 是目录内唯一条目时，才逐项删除标记并删除空目录，保留失败路径供恢复。
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return;
        };
        let mut has_payload = false;
        for entry in entries.flatten() {
            if entry.file_name() != "OWNER" {
                has_payload = true;
                break;
            }
        }
        if has_payload {
            return;
        }
        if fs::remove_file(self.directory.join("OWNER")).is_ok()
            && fs::remove_dir(&self.directory).is_ok()
        {
            if let Some(parent) = self.directory.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
}

fn execute_move_sequential(
    job: &mut Job,
    action: &Action,
    source: &Path,
    checkpoint: bool,
    restore_on_error: bool,
    protected_git_roots: &HashSet<String>,
) -> Result<bool> {
    let span =
        tracing::info_span!("task_action", action_id = action.id, action_kind = ?action.kind);
    let _entered = span.enter();
    let control = job.context.control.clone();
    observe("execute_move", Some(&control), |stage| {
        *stage = "move_execution_and_record";
        if checkpoint {
            job.context.control.checkpoint()?;
        }
        job.context
            .status(format!("执行 {:?}：{}", action.kind, action.source));
        let protected_root = action.target.as_deref().and_then(|target| {
            let target = fsutil::fold_rel(target);
            protected_git_roots
                .iter()
                .find(|root| path_is_at_or_below(&target, root))
        });
        let result = match protected_root {
            Some(root) => Err(anyhow::anyhow!(
                "移动目标位于仍受保护的 Git 项目「{root}」内，按 H-06 保留源项"
            )),
            None => execute_move_at(job, action, source),
        };
        let moved = match result {
            Ok(true) => {
                job.db.mark_action(action.id, "done")?;
                true
            }
            Ok(false) => {
                job.summary.skipped += 1;
                job.db.mark_action(action.id, "skipped")?;
                false
            }
            Err(error) => {
                if restore_on_error {
                    let original = fsutil::safe_join(&job.root, &action.source)?;
                    if !original.exists() && source.exists() {
                        if let Err(restore_error) =
                            fsutil::move_file_preserving_times(source, &original)
                        {
                            job.log(
                        "移动",
                        &action.source,
                        "",
                        "恢复失败",
                        &format!(
                            "移动目标失败后，临时暂存无法恢复到原位置：{restore_error:#}；暂存路径：{}",
                            source.display()
                        ),
                        0,
                    )?;
                        }
                    } else if original.exists() && source.exists() {
                        job.log(
                            "移动",
                            &action.source,
                            "",
                            "恢复失败",
                            &format!(
                                "原位置已被占用，无法放回暂存中的文件；请从暂存路径恢复：{}",
                                source.display()
                            ),
                            0,
                        )?;
                    } else if !original.exists() {
                        job.log(
                            "移动",
                            &action.source,
                            "",
                            "恢复失败",
                            &format!(
                                "移动目标失败后原位置与临时暂存均不存在；暂存路径：{}",
                                source.display()
                            ),
                            0,
                        )?;
                    }
                }
                record_move_failure(job, action, &error)?;
                job.context.control.check_cancelled()?;
                false
            }
        };
        job.context
            .control
            .completed
            .fetch_add(1, Ordering::Relaxed);
        Ok(moved)
    })
}

fn record_move_failure(job: &mut Job, action: &Action, error: &anyhow::Error) -> Result<()> {
    tracing::error!(event = "action_failed", action_id = action.id, action_kind = ?action.kind,
        stage = "move", error_type = "action_boundary",
        error_code = error.chain().find_map(|cause| cause.downcast_ref::<std::io::Error>()).and_then(std::io::Error::raw_os_error),
        "计划移动执行失败");
    job.summary.errors += 1;
    job.db.mark_action(action.id, "failed")?;
    job.log(
        "执行",
        &action.source,
        action.target.as_deref().unwrap_or(""),
        "失败",
        &format!("{error:#}"),
        0,
    )?;
    Ok(())
}

fn execute_move_at(job: &mut Job, action: &Action, source: &Path) -> Result<bool> {
    let target = fsutil::safe_join(
        &job.root,
        action.target.as_deref().context("移动操作缺少目标")?,
    )?;
    let parent_created = fsutil::ensure_parent(&job.root, &target)?;
    if parent_created && !job.config.recursive {
        let parent = target.parent().context("移动目标没有父目录")?;
        let rel = fsutil::relative_string(&job.root, parent)?;
        job.db.conn.execute(
            "INSERT OR IGNORE INTO organizer_created_dirs(rel) VALUES(?1)",
            [&rel],
        )?;
    }
    job.log(
        "移动",
        &action.source,
        action.target.as_deref().unwrap_or(""),
        "准备",
        &action.reason,
        action.expected.as_ref().map_or(0, |snapshot| snapshot.size),
    )?;
    fsutil::move_file_preserving_times(source, &target)?;
    job.summary.moved += 1;
    job.log(
        "移动",
        &action.source,
        action.target.as_deref().unwrap_or(""),
        "成功",
        &action.reason,
        0,
    )?;
    Ok(true)
}

fn execute_action(job: &mut Job, action: &Action) -> Result<bool> {
    // P-08：处理期间假定文件不被其他程序改动，执行阶段不再做快照比对；
    // S-03/C-12：删除前 `MUST NOT` 重读文件内容做逐字节复核，去重判定完全依据
    // 分析期算出的整文件哈希，因此执行阶段不读取任何文件内容。
    let source = fsutil::safe_join(&job.root, &action.source)?;
    match action.kind {
        ActionKind::Delete => Ok(job.delete_path(
            &source,
            action.expected.as_ref(),
            action.mode,
            &action.reason,
            true,
        )? != DeleteResult::Kept),
        ActionKind::Move => execute_move_at(job, action, &source),
        ActionKind::EmptyDirectory => {
            if !dir_recheck_empty(&source)? {
                return Ok(false);
            }
            Ok(
                job.delete_path(&source, None, action.mode, &action.reason, true)?
                    != DeleteResult::Kept,
            )
        }
    }
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
mod lock_tests {
    use super::*;
    // 平台门禁原因：仅 Windows 门禁测试使用（UNC/盘符前缀拼接），非 Windows 无使用者。
    #[cfg(windows)]
    const BSLASH: char = std::path::MAIN_SEPARATOR; // Windows 下为反斜杠

    // 覆盖 C-10/C-11：取消不丢失同批已完成的删除结果。
    #[test]
    fn cancelled_parallel_run_records_completed_deletions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("cancelled.txt"), b"keep").unwrap();
        fs::write(root.join("completed.txt"), b"done").unwrap();
        let db = Database::create(&temp.path().join("state")).unwrap();
        let mut actions = Vec::new();
        for source in ["cancelled.txt", "completed.txt"] {
            let mut action = Action {
                source: source.into(),
                expected: Some(fsutil::snapshot(&root.join(source)).unwrap()),
                ..Action::default()
            };
            action.id = db.add_action(&action).unwrap();
            actions.push(action);
        }
        fs::remove_file(root.join("completed.txt")).unwrap();
        let context = TaskContext::default();
        context.control.cancel();
        let mut job = Job {
            root: root.clone(),
            config: Config::default(),
            context,
            db,
            summary: Summary::default(),
        };
        let result = settle_parallel_results(
            &mut job,
            &actions,
            vec![
                Err(anyhow::anyhow!("任务已取消")),
                Ok(Some(DeleteResult::Permanent)),
            ],
        );
        assert!(result.is_err(), "任务仍应按取消收尾");
        assert_eq!(job.db.action(actions[1].id).unwrap().state, "done");
        assert_eq!(job.db.action(actions[0].id).unwrap().state, "pending");
        assert_eq!(job.summary.deleted, 1);
        assert_eq!(job.summary.permanent_bytes, 4);
        assert_eq!(job.summary.errors, 0);
        assert_eq!(job.context.control.completed.load(Ordering::Relaxed), 1);
        assert!(root.join("cancelled.txt").exists());
        assert!(!root.join("completed.txt").exists());
    }
    #[test]
    fn git_root_protection_uses_path_component_boundaries() {
        assert!(path_is_at_or_below("图片", "图片"));
        assert!(path_is_at_or_below("图片/logo.png", "图片"));
        assert!(!path_is_at_or_below("图片旧/logo.png", "图片"));
        assert!(!path_is_at_or_below("other/图片/logo.png", "图片"));
    }

    #[test]
    fn failed_git_move_blocks_tree_targets_but_not_independent_moves() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        std::fs::create_dir_all(root.join("图片/.git")).unwrap();
        std::fs::write(root.join("图片/.git/HEAD"), b"ref").unwrap();
        std::fs::write(root.join("photo.png"), b"photo").unwrap();
        std::fs::write(root.join("other.txt"), b"other").unwrap();

        let state = temp.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let db = Database::create(&state.join("task")).unwrap();
        db.set("git_roots", &vec!["图片".to_string()]).unwrap();
        let git_id = db
            .add_action(&Action {
                kind: ActionKind::Move,
                source: "图片".into(),
                target: Some("图片/inner".into()),
                reason: "test Git move failure".into(),
                ..Action::default()
            })
            .unwrap();
        let photo_id = db
            .add_action(&Action {
                kind: ActionKind::Move,
                source: "photo.png".into(),
                target: Some("图片/photo.png".into()),
                reason: "test protected-tree target".into(),
                ..Action::default()
            })
            .unwrap();
        let other_id = db
            .add_action(&Action {
                kind: ActionKind::Move,
                source: "other.txt".into(),
                target: Some("文档/other.txt".into()),
                reason: "independent move".into(),
                ..Action::default()
            })
            .unwrap();
        let mut job = Job {
            root: root.clone(),
            config: Config::default(),
            context: TaskContext::default(),
            db,
            summary: Summary::default(),
        };

        execute_planned_moves(&mut job).unwrap();

        assert_eq!(job.db.action(git_id).unwrap().state, "failed");
        assert_eq!(job.db.action(photo_id).unwrap().state, "failed");
        assert_eq!(job.db.action(other_id).unwrap().state, "done");
        assert!(root.join("图片/.git/HEAD").exists());
        assert!(root.join("photo.png").exists());
        assert!(!root.join("图片/photo.png").exists());
        assert!(root.join("文档/other.txt").exists());
    }
    #[test]
    fn move_stage_guard_keeps_payload_when_cleanup_is_not_safe() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(".jchtools-move-work").join("owner");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("OWNER"), "owner").unwrap();
        std::fs::write(directory.join("唯一暂存副本"), b"payload").unwrap();
        {
            let _guard = MoveStageGuard {
                directory: directory.clone(),
                owner: "owner".into(),
            };
        }
        assert!(directory.join("唯一暂存副本").is_file());
        assert!(directory.join("OWNER").is_file());
    }
    // apply 锁目录决策：recorded 有效优先；失效时仅 tasks/ 布局可回退推导。
    #[test]
    fn recorded_state_dir_takes_priority() {
        let temp = tempfile::tempdir().unwrap();
        let tasks = temp.path().join("tasks").join("t1");
        std::fs::create_dir_all(&tasks).unwrap();
        assert_eq!(
            lock_dir_for(&tasks, Some(temp.path())).unwrap(),
            *temp.path()
        );
    }
    #[test]
    fn invalid_record_with_tasks_layout_falls_back_to_parent() {
        let temp = tempfile::tempdir().unwrap();
        let tasks = temp.path().join("tasks").join("t1");
        std::fs::create_dir_all(&tasks).unwrap();
        let gone = temp.path().join("gone"); // 不存在 → recorded 过滤失效
        assert_eq!(lock_dir_for(&tasks, Some(&gone)).unwrap(), *temp.path());
        assert_eq!(lock_dir_for(&tasks, None).unwrap(), *temp.path());
    }
    // 平台门禁原因：断言依赖 Windows 路径语义（盘符前缀与 \server\share UNC 前缀的
    // components 解析），Unix 上分隔符不同、该解析不存在；OS 无关的三条决策测试保持全平台。
    #[cfg(windows)]
    #[test]
    fn tasks_layout_at_drive_root_is_rejected() {
        // 盘根/共享根不是状态目录：在那里落锁会留下陌生位置的锁文件（engine 不挂载
        // 任何盘，这里用纯路径验证决策逻辑，不触碰文件系统；UNC 前缀用 BSLASH 拼接，
        // 避免源码转义歧义）。
        assert!(
            lock_dir_for(Path::new(r"Q:\tasks\t1"), None).is_err(),
            "盘根 tasks 布局必须拒绝"
        );
        let unc = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "tasks"
            + &BSLASH.to_string()
            + "t1";
        assert!(
            lock_dir_for(Path::new(&unc), None).is_err(),
            "UNC 共享根 tasks 布局必须拒绝"
        );
    }
    // 平台门禁原因：同上，断言 Windows 前缀路径的解析结果。
    #[cfg(windows)]
    #[test]
    fn tasks_layout_deep_on_unc_share_is_allowed() {
        // UNC 上正常深度的状态目录（如 \\server\share\JchTools\data）必须放行：
        // 前缀 \\server\share 不可拆分，不能用「祖先层数少」误判为根。
        let unc = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "JchTools"
            + &BSLASH.to_string()
            + "data"
            + &BSLASH.to_string()
            + "tasks"
            + &BSLASH.to_string()
            + "t1";
        let expected = [BSLASH, BSLASH].iter().collect::<String>()
            + "server"
            + &BSLASH.to_string()
            + "share"
            + &BSLASH.to_string()
            + "JchTools"
            + &BSLASH.to_string()
            + "data";
        assert_eq!(
            lock_dir_for(Path::new(&unc), None).unwrap(),
            PathBuf::from(expected),
            "UNC 深目录 tasks 布局必须放行"
        );
        let win = Path::new(r"C:\state\tasks\t1");
        assert_eq!(lock_dir_for(win, None).unwrap(), PathBuf::from(r"C:\state"));
    }
    #[test]
    fn non_tasks_layout_without_record_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("t1");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            lock_dir_for(&dir, None).is_err(),
            "非 tasks 布局且无记录必须拒绝"
        );
    }
    fn r01_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let state = temp.path().join("state");
        (temp, root, state)
    }
    // 覆盖 S-04/H-05/附录 C：真实公开入口验证不附加虚构末尾分隔符。
    #[test]
    fn r01_exclusions_use_actual_relative_paths() {
        let (_temp, root, state) = r01_fixture();
        fs::create_dir(root.join("cache")).unwrap();
        fs::write(root.join("keep.txt"), b"retained").unwrap();
        let task = prepare_at(&root, Config {
            exclusions: "cache/*;keep.txt/".into(),
            ..Config::default()
        }, TaskContext::default(), &state).unwrap();
        assert_eq!(task.summary.scanned, 1);
        assert_eq!(task.summary.planned_empty, 1);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert!(!root.join("cache").exists());
        assert_eq!(fs::read(root.join("文档/keep.txt")).unwrap(), b"retained");
    }

    // 覆盖 C-09：选择隔离容器本身也不能纳入其普通文件。
    #[test]
    fn r01_selected_quarantine_root_keeps_payloads() {
        let (_temp, root, state) = r01_fixture();
        let selected = root.join("解压失败");
        fs::create_dir(&selected).unwrap();
        fs::write(selected.join("Thumbs.db"), b"original").unwrap();
        fs::write(selected.join("report.txt"), b"original").unwrap();
        let context = TaskContext::default();
        let control = context.control.clone();
        let task = prepare_at(&selected, Config {
            dedup_other_names: true,
            ..Config::default()
        }, context, &state).unwrap();
        assert_eq!(task.summary.scanned, 0);
        assert_eq!(task.summary.planned_delete, 0);
        assert_eq!(task.summary.planned_move, 0);
        assert_eq!(control.read_bytes.load(Ordering::Relaxed), 0);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert_eq!(fs::read(selected.join("Thumbs.db")).unwrap(), b"original");
        assert_eq!(fs::read(selected.join("report.txt")).unwrap(), b"original");
    }

    // 覆盖 C-09：真实扫描产生的跳过提示包含深层隔离目录路径。
    #[test]
    fn r01_nested_quarantine_skip_reports_path() {
        let (_temp, root, state) = r01_fixture();
        fs::create_dir_all(root.join("outer/解压失败")).unwrap();
        fs::write(root.join("outer/解压失败/Thumbs.db"), b"original").unwrap();
        let task = prepare_at(&root, Config::default(), TaskContext::default(), &state).unwrap();
        assert_eq!(task.summary.scanned, 0);
        let db = Database::open_existing(&task.directory).unwrap();
        let notices: i64 = db.conn.query_row(
            "SELECT count(*) FROM events WHERE source=?1 AND reason LIKE '%跳过%'",
            ["outer/解压失败"], |row| row.get(0),
        ).unwrap();
        assert!(notices > 0, "必须明确提示发现的隔离路径");
        drop(db);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert_eq!(fs::read(root.join("outer/解压失败/Thumbs.db")).unwrap(), b"original");
    }

    // 覆盖 C-19/C-20：长度退让后的真实计划只扩展摘要、不恢复来源。
    #[test]
    fn r01_conflict_yield_keeps_source_free_digest() {
        let (_temp, root, state) = r01_fixture();
        let first = format!("{}/x/report.txt", "a".repeat(35));
        let second = format!("{}/x/report.txt", "b".repeat(35));
        for (name, bytes) in [(&first, b"first".as_slice()), (&second, b"second".as_slice())] {
            fs::create_dir_all(root.join(name).parent().unwrap()).unwrap();
            fs::write(root.join(name), bytes).unwrap();
        }
        fs::create_dir(root.join("文档")).unwrap();
        let fixed = format!("文档/report_{}.txt", rules::path_digest(&first, 8));
        fs::write(root.join(&fixed), b"occupied").unwrap();
        let task = prepare_at(&root, Config {
            global_delete: DeleteMode::Keep,
            dedup_same_name: false,
            dedup_copy_names: false,
            dedup_other_names: false,
            ..Config::default()
        }, TaskContext::default(), &state).unwrap();
        let db = Database::open_existing(&task.directory).unwrap();
        let expected = format!("文档/report_{}.txt", rules::path_digest(&first, 10));
        let moves = db.actions_page_filtered(0, 100, Some("move")).unwrap();
        assert_eq!(moves.iter().find(|action| action.source == first).unwrap().target.as_deref(), Some(expected.as_str()));
        drop(db);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert_eq!(fs::read(root.join(expected)).unwrap(), b"first");
        assert_eq!(fs::read(root.join(fixed)).unwrap(), b"occupied");
    }

    // 覆盖 C-01/C-12：清理判断先于哈希，单一存活者无需读取内容。
    #[test]
    fn r01_cleanup_precedes_candidate_hashing() {
        let (_temp, root, state) = r01_fixture();
        fs::write(root.join("keep.txt"), b"identical").unwrap();
        fs::write(root.join("temporary.tmp"), b"identical").unwrap();
        let context = TaskContext::default();
        let control = context.control.clone();
        let task = prepare_at(&root, Config {
            clean_temp: true,
            dedup_other_names: true,
            ..Config::default()
        }, context, &state).unwrap();
        assert_eq!(control.read_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(task.summary.planned_delete, 1);
        apply(&task.directory, TaskContext::default()).unwrap();
        assert_eq!(fs::read(root.join("文档/keep.txt")).unwrap(), b"identical");
        assert!(!root.join("temporary.tmp").exists());
    }
    // 覆盖 P-10/S-01：独立进程安装真实磁盘logger，目标容器被文件占用仍保留源项。
    #[test]
    fn r01_failed_item_reason_is_written_to_isolated_disk_log() {
        const CHILD_ENV: &str = "JCH_R01_DIAGNOSTIC_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "engine::lock_tests::r01_failed_item_reason_is_written_to_isolated_disk_log", "--nocapture"])
                .env(CHILD_ENV, "1")
                .output().unwrap();
            assert!(output.status.success(), "隔离子进程失败：{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        let (_temp, root, state) = r01_fixture();
        let guard = jchtools_foundation::logging::init(&state).expect("独立测试进程必须初始化隔离磁盘日志");
        fs::write(root.join("文档"), b"container occupied").unwrap();
        fs::write(root.join("blocked.txt"), b"source preserved").unwrap();
        let planned = prepare_at(&root, Config::default(), TaskContext::default(), &state).unwrap();
        assert!(planned.summary.errors > 0);
        assert_eq!(planned.summary.planned_move, 0);
        let applied = apply(&planned.directory, TaskContext::default()).unwrap();
        assert!(applied.summary.errors > 0);
        assert_eq!(fs::read(root.join("blocked.txt")).unwrap(), b"source preserved");
        assert_eq!(fs::read(root.join("文档")).unwrap(), b"container occupied");
        drop(guard);
        let found = fs::read_dir(state.join(jchtools_foundation::logging::LOG_DIR)).unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("log"))
            .any(|entry| fs::read_to_string(entry.path()).unwrap().lines().any(|line| {
                line.contains("task_item_failed") && line.contains("blocked.txt")
                    && line.contains("reason") && line.contains("同名文件占用")
                    && line.contains("已保留源项")
            }));
        assert!(found, "实际磁盘逐文件失败日志必须包含去敏原因，不能只给source/result");
    }
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
mod s05_scope_tests {
    use super::*;
    use jchtools_file_core::control::Context as TaskContext;
    use jchtools_file_core::db::Database;
    /// S-05 合成受保护子树夹具：root/WinFake 内含「系统样」文件，root 内含普通文件。
    /// 不用真实系统目录做破坏性实验；返回（临时目录、根、受保护目录的规范化路径）。
    fn synthetic_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        fs::create_dir_all(root.join("WinFake/System32")).unwrap();
        fs::write(root.join("WinFake/System32/a.dll"), b"sys").unwrap();
        fs::write(root.join("normal.txt"), b"n").unwrap();
        let protected = fs::canonicalize(root.join("WinFake")).unwrap();
        (temp, root, protected)
    }
    // 覆盖 S-05（端到端：合成受保护子树位于普通根内——分析扫描不进入、
    // 确认计数只含范围内的文件、范围提示写明已排除系统目录）
    #[test]
    fn prepare_excludes_synthetic_protected_subtree() {
        let (temp, root, protected) = synthetic_fixture();
        let state = temp.path().join("state");
        let result = prepare_at_with(
            &root,
            Config::default(),
            TaskContext::default(),
            &state,
            Some(&protected),
        )
        .unwrap();
        assert_eq!(result.summary.scanned, 1, "受保护子树内文件不入盘点");
        let db = Database::open_existing(&result.directory).unwrap();
        let text = db
            .conn
            .query_row(
                "SELECT group_concat(reason, ' | ') FROM events",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(
            text.contains("已排除 Windows 系统目录"),
            "范围提示必须写明已排除系统目录：{text}"
        );
    }
    // 覆盖 S-05, H-05（收尾空目录清理不进入受保护子树；范围外普通空目录仍正常清理）
    #[test]
    fn final_cleanup_keeps_synthetic_protected_subtree() {
        let (temp, root, protected) = synthetic_fixture();
        fs::create_dir_all(root.join("WinFake/内空")).unwrap();
        fs::create_dir_all(root.join("普通空")).unwrap();
        let canonical_root = fs::canonicalize(&root).unwrap();
        let state = temp.path().join("state");
        let mut job = Job {
            root: canonical_root,
            config: Config::default(),
            context: TaskContext::default(),
            db: Database::create(&state).unwrap(),
            summary: jchtools_file_core::model::Summary::default(),
        };
        final_empty_cleanup(&mut job, Some(&protected)).unwrap();
        assert!(
            root.join("WinFake/System32/a.dll").is_file(),
            "受保护子树内容零改动"
        );
        assert!(
            root.join("WinFake/内空").is_dir(),
            "受保护子树内的空目录同样不得清理"
        );
        assert!(
            !root.join("普通空").exists(),
            "范围外的普通空目录仍按 H-05 清理"
        );
    }
}
