use super::in_transaction;
use anyhow::Result;
use jchtools_file_core::{
    fsutil,
    hash_cache::HashCache,
    hashing,
    model::FileRecord,
    task::{observe, Job},
};
use rayon::prelude::*;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;

/// 哈希阶段一次从任务库取多少条候选。与哈希线程数解耦（线程数只决定并行度，
/// 批量只决定分页次数），避免「调线程数」同时改变两个量而无法判断。
const HASH_BATCH: usize = 256;

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "hash", skip_all)
)]
pub(super) fn hash_candidates(job: &mut Job) -> Result<()> {
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
        let mut cache = open_hash_cache(job)?;
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
            // C-13：先查跨运行缓存，命中的候选不再读取文件内容。
            let (reused, compute) = partition_cache_hits(&mut cache, job, &batch)?;
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
            store_batch_hashes(job, &reused, &results)?;
            // C-13：把本次新算出的哈希写入跨运行缓存。
            cache_new_hashes(&mut cache, job, &batch, &results)?;
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

/// C-13：跨运行哈希缓存放在状态目录根（每次 prepare 都新建任务库，缓存必须
/// 跨任务存活）。打开失败只降级为「无缓存、全部重算」，不得阻断分析。
fn open_hash_cache(job: &Job) -> Result<Option<HashCache>> {
    if let Ok(dir) = job.db.get::<String>("state_dir") {
        match HashCache::open(Path::new(&dir)) {
            Ok(cache) => Ok(Some(cache)),
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
                Ok(None)
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
        Ok(None)
    }
}

/// C-13：先查跨运行缓存，命中的候选不再读取文件内容。缓存故障一次性
/// 降级为「其后全部重算」——缓存只是加速，正确性不依赖它。
/// resume 是首个未消化的候选下标：缓存不可用/读取失败时从它起全部转入重算。
/// lookup 失败时把 cache 置 None 传回调用方，其后每页都直接转入重算。
/// 分流结果：reused 为已命中的 (记录 ID, 折叠键) 列表，compute 为待重算候选。
type CacheSplit<'a> = (Vec<(i64, String)>, Vec<&'a FileRecord>);

fn partition_cache_hits<'a>(
    cache: &mut Option<HashCache>,
    job: &Job,
    batch: &'a [FileRecord],
) -> Result<CacheSplit<'a>> {
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
                *cache = None;
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
    Ok((reused, compute))
}

/// 候选哈希统一落库：缓存命中的与刚算出的走同一条事务路径（取消拦截、
/// BEGIN IMMEDIATE / COMMIT / 失败 ROLLBACK），planner 无感知。
fn store_batch_hashes(
    job: &mut Job,
    reused: &[(i64, String)],
    results: &[(i64, String, Result<String>)],
) -> Result<()> {
    in_transaction(
        job,
        "hash_rollback",
        "哈希失败后的事务回滚失败",
        |job| {
            // 用户取消时并行哈希的每个成员都会返回取消错误；先在这里拦截，
            // 否则每个候选都被计成 error 并逐条写日志，取消任务的错误数虚高。
            job.context.control.checkpoint()?;
            // C-13：缓存命中的哈希与刚算出的走完全相同的落库路径，planner 无感知。
            for (id, hash) in reused {
                job.db.set_file_hash(*id, hash)?;
            }
            for (id, rel, result) in results {
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
            Ok(())
        },
    )
}

/// C-13：把本次新算出的哈希写入跨运行缓存；写失败同样只降级、不失败
///（禁用后续复用，把 cache 置 None 传回调用方）。
fn cache_new_hashes(
    cache: &mut Option<HashCache>,
    job: &Job,
    batch: &[FileRecord],
    results: &[(i64, String, Result<String>)],
) -> Result<()> {
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
            *cache = None;
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
    Ok(())
}
