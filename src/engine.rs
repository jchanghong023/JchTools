use crate::{
    archive::{self, SevenZip},
    config::{self, Config, DeleteMode},
    control::{Context as TaskContext, Event},
    db::Database,
    fsutil, hashing,
    model::{Action, ActionKind, Snapshot, Summary},
    planner,
    platform::{self, DeleteResult},
    rules,
};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use rusqlite::params;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

pub struct Job {
    pub root: PathBuf,
    pub config: Config,
    pub context: TaskContext,
    pub db: Database,
    pub summary: Summary,
    pub archive_override: Option<crate::config::ConflictPolicy>,
}
/// 哈希阶段一次从任务库取多少条候选。与哈希线程数解耦（线程数只决定并行度，
/// 批量只决定分页次数），避免「调线程数」同时改变两个量而无法判断。
const HASH_BATCH: usize = 256;
/// 执行阶段每批动作数：一批一个事务。每动作 4 条语句若各自自动提交，实测单条约 40µs
/// （2 万项执行阶段 9.2s 里约 3.6s 花在提交上）；合批后每动作提交摊到 7µs。
/// 64 是实测折中：256 批次只再快 6%，但崩溃/取消时未提交记录的窗口放大 4 倍；
/// 64 与界面计划页（101 行）同量级，用户看到的进度滞后不超过一页。
const APPLY_BATCH: usize = 64;
impl Job {
    pub fn log(
        &self,
        phase: &str,
        source: &str,
        target: &str,
        result: &str,
        reason: &str,
        size: u64,
    ) -> Result<()> {
        self.db.log(phase, source, target, result, reason, size)?;
        self.context.emit(Event::Log(format!(
            "[{phase}] {result} | {source}{} | {reason}",
            if target.is_empty() {
                String::new()
            } else {
                format!(" → {target}")
            }
        )));
        Ok(())
    }
    pub fn delete_path(
        &mut self,
        path: &Path,
        expected: Option<&Snapshot>,
        mode: DeleteMode,
        reason: &str,
        physical_free: bool,
    ) -> Result<DeleteResult> {
        let relative = fsutil::relative_string(&self.root, path)?;
        fsutil::safe_join(&self.root, &relative)?;
        let size = expected.map_or(0, |s| s.size);
        // Record intent before a mutation. This is an audit trail, not a recovery journal.
        self.log("删除", &relative, "", "准备", reason, size)?;
        let result = platform::remove(path, mode, &self.context.control)?;
        match result {
            DeleteResult::Kept => {
                self.summary.skipped += 1;
            }
            // 多硬链接源的内容仍由其他链接持有：逻辑大小与 candidate_bytes 同口径，
            // physical_free=false（硬链接替换）时不计入 permanent_bytes，
            // 避免「已永久删除字节」虚高（S-06）。
            DeleteResult::Permanent => {
                self.summary.deleted += 1;
                if physical_free && expected.is_none_or(|s| s.links <= 1) {
                    self.summary.permanent_bytes =
                        self.summary.permanent_bytes.saturating_add(size);
                }
            }
        }
        self.log(
            "删除",
            &relative,
            "",
            match result {
                DeleteResult::Kept => "保留",
                DeleteResult::Permanent => "已永久删除",
            },
            reason,
            size,
        )?;
        // 已经不在磁盘上的文件不能再参与后续按名/按大小的查找：否则同名成员合入时会去读取
        // 一个刚被删除的路径，把整包解压误判为失败。
        if result != DeleteResult::Kept {
            self.db
                .conn
                .execute("UPDATE files SET active=0 WHERE rel=?1", [&relative])?;
        }
        Ok(result)
    }
}
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub directory: PathBuf,
    pub summary: Summary,
}
/// 清理硬链接执行的崩溃残留（.jchtools-link-{uuid}）：崩溃发生在「源已删、改名回
/// 原路径前」时残留无法自愈，扫描对其永久剪枝且无其它回收路径。残留是指向 keeper
/// 内容的硬链接，删除后内容仍由保留文件持有。24 小时阈值与 clean_orphan_staging
/// 一致，避免误删并发任务的临时文件。
fn clean_orphan_link_temps(root: &Path) -> usize {
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(".jchtools-link-") || !entry.file_type().is_file() {
            continue;
        }
        let stale = fs::symlink_metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age.as_secs() > 24 * 3600);
        if !stale {
            continue;
        }
        // 只有仍是硬链接（链接数 >= 2，内容另有链接持有，与崩溃残留的不变量一致）才清扫；
        // 普通同名文件可能是用户文件或从压缩包解出的同名成员，静默删除即数据丢失。
        let residue = fsutil::snapshot(path).is_ok_and(|s| s.links >= 2);
        if residue && fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    removed
}
pub fn prepare(root: &Path, config: Config, context: TaskContext) -> Result<TaskResult> {
    prepare_at(root, config, context, &config::state_dir()?)
}
/// 独立状态目录使核心可在无 GUI 下测试。目录整理不解压（C-01），不再接受引擎注入；
/// 真实引擎用例走 extract_run_at（E-02：引擎只能按固定顺序获得，不向用户提供路径参数）。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "organize_analyze", skip_all, err)
)]
pub fn prepare_at(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
) -> Result<TaskResult> {
    config.validate()?;
    let root = fsutil::normalize_root(root)?;
    let _guard = fsutil::RootGuard::acquire(state)?;
    let directory = state.join("tasks").join(format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        uuid::Uuid::new_v4()
    ));
    let db = Database::create(&directory)?;
    db.set("root", &fsutil::path_string(&root)?)?;
    db.set("config", &config)?;
    db.set("status", &"analyzing")?;
    db.set("summary", &Summary::default())?;
    db.set("created", &chrono::Utc::now().to_rfc3339())?;
    // 记录本次的全局锁目录：apply 若按任务目录当前位置推导锁位置，任务目录被移动后
    // 会与这里的互斥失效（apply_with 会优先锁这个记录值）。
    db.set("state_dir", &fsutil::path_string(state)?)?;
    let mut job = Job {
        root,
        config,
        context,
        db,
        summary: Summary::default(),
        archive_override: None,
    };
    let result = (|| {
        // C-01：分析阶段只读，不做任何清扫（含本工具崩溃残留的硬链接临时文件——
        // 它们被扫描永久剪枝，不影响计划正确性；清扫统一在 apply_with 执行前进行）。
        // C-01：目录整理的分析阶段只读——不解压（解压职责整体移交「递归解压」工具，X-01），
        // 扫描即终态；树里既有压缩包按普通文件参与后续去重/归类。
        scan(&mut job, false, state)?;
        hash_candidates(&mut job)?;
        job.context
            .status("生成去重、冲突、归类和清理计划（尚未执行这些操作）");
        job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
        match planner::build(&mut job) {
            Ok(()) => job.db.conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = job.db.conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
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
        crate::perf::analyze_done(
            job.summary.scanned,
            job.summary.scanned_bytes,
            job.summary.errors,
        );
        Ok(TaskResult {
            directory: directory.clone(),
            summary: job.summary.clone(),
        })
    })();
    if let Err(error) = &result {
        // 用户主动取消不是失败：任务库状态要能区分"已取消"和"失败"，否则事后检查会误判。
        let cancelled = job.context.control.is_cancelled();
        let _ = job.db.set("summary", &job.summary);
        let _ = job
            .db
            .set("status", &if cancelled { "cancelled" } else { "failed" });
        let _ = job.db.log(
            "任务",
            "",
            "",
            if cancelled { "已取消" } else { "失败" },
            &format!("{error:#}"),
            0,
        );
    }
    result
}
/// 「递归解压」工具的一段式执行（X-02）：确认后连续运行到结束——扫描登记压缩包、
/// 就地解压（X-03）、成功原包按处置策略处理（X-05 默认回收站）、失败原包移入
/// 「解压失败」子目录（X-06）。没有计划审核环节，也不生成整理计划。
pub fn extract_run(root: &Path, config: Config, context: TaskContext) -> Result<TaskResult> {
    extract_run_at(root, config, context, &config::state_dir()?, None)
}
/// 测试注入变体：独立状态目录 + 显式引擎路径（与 prepare_at 同口径，E-02 不向用户提供）。
pub fn extract_run_at(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
    engine_path: Option<&Path>,
) -> Result<TaskResult> {
    extract_run_with(root, config, context, state, engine_path)
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "archive_extract", skip_all, err)
)]
fn extract_run_with(
    root: &Path,
    config: Config,
    context: TaskContext,
    state: &Path,
    engine_path: Option<&Path>,
) -> Result<TaskResult> {
    config.validate()?;
    let root = fsutil::normalize_root(root)?;
    let _guard = fsutil::RootGuard::acquire(state)?;
    let directory = state.join("tasks").join(format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        uuid::Uuid::new_v4()
    ));
    let db = Database::create(&directory)?;
    db.set("root", &fsutil::path_string(&root)?)?;
    db.set("config", &config)?;
    db.set("status", &"executing")?;
    db.set("summary", &Summary::default())?;
    db.set("created", &chrono::Utc::now().to_rfc3339())?;
    db.set("state_dir", &fsutil::path_string(state)?)?;
    let mut job = Job {
        root,
        config,
        context,
        db,
        summary: Summary::default(),
        archive_override: None,
    };
    let result = (|| {
        scan(&mut job, true, state)?;
        let count: i64 = job.db.conn.query_row(
            "SELECT COUNT(*) FROM archives WHERE state='pending'",
            [],
            |r| r.get(0),
        )?;
        if count > 0 {
            let engine = match engine_path {
                Some(path) => SevenZip::with_executable(path)?,
                None => SevenZip::from_bundle()?,
            };
            archive::extract_queued(&mut job, &engine)?;
        } else {
            job.log(
                "解压",
                "",
                "",
                "提示",
                "所选目录（按当前扫描范围）没有发现压缩包；未做任何改动",
                0,
            )?;
        }
        job.db.set("summary", &job.summary)?;
        job.log(
            "任务",
            "",
            "",
            "完成",
            &format!(
                "解压结束：成功 {} 包；失败并移入「{}」 {} 包",
                job.summary.archives_ok,
                archive::QUARANTINE_DIR_NAME,
                job.summary.archives_failed
            ),
            0,
        )?;
        #[cfg(feature = "perf-tracing")]
        crate::perf::extract_done(
            job.summary.scanned,
            job.summary.archives_ok,
            job.summary.archives_failed,
        );
        Ok(TaskResult {
            directory: directory.clone(),
            summary: job.summary.clone(),
        })
    })();
    if let Err(error) = &result {
        let cancelled = job.context.control.is_cancelled();
        let _ = job.db.set("summary", &job.summary);
        let _ = job
            .db
            .set("status", &if cancelled { "cancelled" } else { "failed" });
        let _ = job.db.log(
            "任务",
            "",
            "",
            if cancelled { "已取消" } else { "失败" },
            &format!("{error:#}"),
            0,
        );
    } else {
        job.db.set("status", &"finished")?;
    }
    result
}
/// 确认框用的压缩包计数（X-02）：只读快速清点，与正式扫描同一套过滤口径
/// （递归/隐藏/系统/排除规则/跳过「解压失败」，X-07）。失败即报错，不回退猜测值。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "count_archives", skip_all)
)]
pub fn count_archives(root: &Path, config: &Config) -> Result<u64> {
    config.validate()?;
    let root = fsutil::normalize_root(root)?;
    let excluded = rules::build_exclusions(&config.exclusions)?;
    let quarantine = root.join(archive::QUARANTINE_DIR_NAME);
    let mut count = 0u64;
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .min_depth(1)
        .max_depth(if config.recursive { usize::MAX } else { 1 })
        .into_iter()
        .filter_entry(|entry| {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                return true;
            };
            if fsutil::is_link(&meta) {
                return false;
            }
            let Ok(rel) = fsutil::relative_string(&root, entry.path()) else {
                return false;
            };
            if rel == ".jchtools-work" || rel.starts_with(".jchtools-work/") {
                return false;
            }
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".jchtools-link-")
            {
                return false;
            }
            if rel == archive::QUARANTINE_DIR_NAME
                || rel.starts_with(&format!("{}/", archive::QUARANTINE_DIR_NAME))
            {
                return false;
            }
            if quarantine.is_dir() && entry.path().starts_with(&quarantine) {
                return false;
            }
            if excluded.is_match(&rel) || excluded.is_match(format!("{rel}/")) {
                return false;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if !config.include_hidden && meta.file_attributes() & 2 != 0 {
                    return false;
                }
                if !config.include_system && meta.file_attributes() & 4 != 0 {
                    return false;
                }
            }
            #[cfg(not(windows))]
            if !config.include_hidden && entry.file_name().to_string_lossy().starts_with('.') {
                return false;
            }
            true
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if rules::archive_name(name) {
            count += 1;
        }
    }
    Ok(count)
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "scan", skip_all)
)]
fn scan(job: &mut Job, enqueue: bool, state: &Path) -> Result<()> {
    job.context.status(if enqueue {
        "扫描所选目录，登记待解压的压缩包"
    } else {
        "扫描所选目录（只读分析）"
    });
    job.db
        .conn
        .execute_batch("DELETE FROM files; DELETE FROM directories;")?;
    job.summary.scanned = 0;
    job.summary.scanned_bytes = 0;
    job.context.control.scanned.store(0, Ordering::Relaxed);
    let root = job.root.clone();
    let config = job.config.clone();
    let excluded = rules::build_exclusions(&config.exclusions)?;
    let state = fs::canonicalize(state)?;
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().and_then(|d| fs::canonicalize(d).ok()));
    if let (Some(directory), Ok(target)) = (&executable_dir, fs::canonicalize(&root)) {
        if target.starts_with(directory) {
            job.log("扫描","","","提示",&format!("所选目录位于程序自身目录（{}）内；为避免误处理程序文件，扫描会跳过这个目录里的条目",directory.display()),0)?;
        }
    }
    // X-07 / C-09：「解压失败」是「递归解压」的失败暂存区，两个工具默认都不碰它；
    // 目录存在时明确提示（C-09），没有则不打扰。
    let quarantine = job.root.join(archive::QUARANTINE_DIR_NAME);
    if quarantine.is_dir() {
        job.log(
            "扫描",
            "",
            "",
            "提示",
            &format!(
                "已跳过「{}」子目录：失败暂存区不参与本次处理（补救后把包移出即可重新处理）",
                archive::QUARANTINE_DIR_NAME
            ),
            0,
        )?;
    }
    let walk = walkdir::WalkDir::new(&root)
        .follow_links(false)
        .min_depth(1)
        .max_depth(if config.recursive { usize::MAX } else { 1 })
        .into_iter()
        .filter_entry(|entry| {
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                return true;
            };
            if fsutil::is_link(&meta) {
                return false;
            }
            if entry.path().starts_with(&state) {
                return false;
            }
            if executable_dir
                .as_ref()
                .is_some_and(|d| entry.path().starts_with(d))
            {
                return false;
            }
            let Ok(rel) = fsutil::relative_string(&root, entry.path()) else {
                return false;
            };
            if rel == ".jchtools-work" || rel.starts_with(".jchtools-work/") {
                return false;
            }
            // 硬链接执行时的临时替换文件（.jchtools-link-{uuid}）：崩溃残留不应进入扫描与计划。
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".jchtools-link-")
            {
                return false;
            }
            if rel == archive::QUARANTINE_DIR_NAME
                || rel.starts_with(&format!("{}/", archive::QUARANTINE_DIR_NAME))
            {
                return false;
            }
            if excluded.is_match(&rel) || excluded.is_match(format!("{rel}/")) {
                return false;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if !config.include_hidden && meta.file_attributes() & 2 != 0 {
                    return false;
                }
                if !config.include_system && meta.file_attributes() & 4 != 0 {
                    return false;
                }
            }
            #[cfg(not(windows))]
            if !config.include_hidden && entry.file_name().to_string_lossy().starts_with('.') {
                return false;
            }
            true
        });
    job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let mut count = 0;
        for entry in walk {
            job.context.control.checkpoint()?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    job.summary.errors += 1;
                    job.log("扫描", "", "", "失败", &error.to_string(), 0)?;
                    continue;
                }
            };
            let scan_one = (|| {
                let relative = fsutil::relative_string(&root, entry.path())?;
                fsutil::safe_join(&root, &relative)?;
                if entry.file_type().is_dir() {
                    let name = entry
                        .file_name()
                        .to_str()
                        .context("目录名称不能无损表示")?
                        .to_lowercase();
                    job.db.conn.execute(
                        "INSERT INTO directories(rel,name,depth) VALUES(?1,?2,?3)",
                        params![relative, name, crate::convert::usize_as_i64(entry.depth())],
                    )?;
                    return Ok(());
                }
                if !entry.file_type().is_file() {
                    return Ok(());
                }
                let snapshot = fsutil::snapshot(entry.path())?;
                let name = entry
                    .file_name()
                    .to_str()
                    .context("文件名不能无损表示")?
                    .to_string();
                job.db.insert_file(
                    &relative,
                    &name.to_lowercase(),
                    &rules::normal_name(&name),
                    &snapshot,
                )?;
                job.summary.scanned += 1;
                job.summary.scanned_bytes = job.summary.scanned_bytes.saturating_add(snapshot.size);
                job.context.control.scanned.fetch_add(1, Ordering::Relaxed);
                if enqueue && rules::archive_name(&name) {
                    archive::enqueue(job, entry.path(), 0)?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            if let Err(error) = scan_one {
                job.summary.errors += 1;
                job.log(
                    "扫描",
                    &entry.path().display().to_string(),
                    "",
                    "跳过",
                    &format!("{error:#}"),
                    0,
                )?;
            }
            count += 1;
            if count % 2048 == 0 {
                job.db.conn.execute_batch("COMMIT; BEGIN IMMEDIATE;")?;
            }
        }
        Ok::<_, anyhow::Error>(())
    })();
    match result {
        Ok(()) => job.db.conn.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = job.db.conn.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    Ok(())
}
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "hash", skip_all)
)]
fn hash_candidates(job: &mut Job) -> Result<()> {
    if !job.config.dedup_same_name && !job.config.dedup_copy_names && !job.config.dedup_other_names
    {
        return Ok(());
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(job.config.hash_workers)
        .thread_name(|i| format!("organizer-hash-{i}"))
        .build()?;
    job.context.status("计算候选文件的完整 Hash");
    job.db.conn.execute_batch("DROP TABLE IF EXISTS hash_candidates; CREATE TEMP TABLE hash_candidates(id INTEGER PRIMARY KEY);")?;
    // C-12：候选范围只依据扫描期已获得的信息（大小与名称关系），完整哈希在一次遍历内
    // 完成，不设预哈希/完整哈希两阶段。内容相同必然同尺寸，所以「同尺寸不止一个」
    // 永远是安全下界；默认（不同名去重关闭）还可再收窄——去重配对只可能发生在
    // 「名称完全相同」（planner 比较 files.name）或「归一化名称完全相同」
    // （planner 比较 files.normal）两组之间，取两种分组并集仍是安全下界。
    if job.config.dedup_other_names {
        job.db.conn.execute("INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND size IN (SELECT size FROM files WHERE active=1 GROUP BY size HAVING COUNT(*)>1)",[])?;
    } else {
        for key in ["name", "normal"] {
            job.db.conn.execute(&format!("INSERT OR IGNORE INTO hash_candidates SELECT id FROM files WHERE active=1 AND (size,{key}) IN (SELECT size,{key} FROM files WHERE active=1 GROUP BY size,{key} HAVING COUNT(*)>1)"),[])?;
        }
    }
    let mut cursor = 0;
    loop {
        job.context.control.checkpoint()?;
        // 候选表按主键游标推进，并用 CROSS JOIN 固定 hash_candidates 为外层扫描表：
        // 旧的 `id IN (SELECT id FROM hash_candidates)` 会让每次分页都重扫整个候选集合
        // （实测每次调用成本随游标位置线性增长，累计平方级）。取数批量与哈希线程数解耦，
        // 避免「调线程数」同时改变两个量。
        let sql=format!("SELECT {} FROM hash_candidates AS c CROSS JOIN files AS f ON f.id=c.id WHERE c.id>?1 AND f.active=1 ORDER BY c.id LIMIT ?2", crate::db::file_columns_qualified("f"));
        let batch = job.db.files(
            &sql,
            params![cursor, crate::convert::usize_as_i64(HASH_BATCH)],
        )?;
        let Some(last) = batch.last() else {
            break;
        };
        cursor = last.id;
        let root = &job.root;
        let control = &job.context.control;
        let results: Vec<_> = pool.install(|| {
            batch
                .par_iter()
                .map(|file| {
                    let result = (|| {
                        let path = fsutil::safe_join(root, &file.rel)?;
                        hashing::full_hash(&path, &file.snapshot, control)
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
            for (id, rel, result) in results {
                match result {
                    Ok(hash) => {
                        job.db
                            .conn
                            .execute("UPDATE files SET hash=?1 WHERE id=?2", params![hash, id])?;
                    }
                    Err(error) => {
                        job.summary.errors += 1;
                        job.db
                            .conn
                            .execute("UPDATE files SET active=0 WHERE id=?1", [id])?;
                        job.log("Hash", &rel, "", "跳过", &format!("{error:#}"), 0)?;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })();
        match update {
            Ok(()) => job.db.conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = job.db.conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
    }
    Ok(())
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
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "organize_apply", skip_all, err)
)]
pub fn apply(directory: &Path, context: TaskContext) -> Result<TaskResult> {
    // Database::open 会在文件缺失时创建一个空库；先确认这是扫描生成的任务目录，
    // 避免把任意目录（甚至写错路径）悄悄变成一个必然失败的空任务。
    // 该检查必须先于 RootGuard：否则写错路径会先在错误位置创建目录并落下锁文件。
    anyhow::ensure!(
        directory.join("task.sqlite3").is_file(),
        "目录里没有 task.sqlite3；请选择「开始解压与分析」生成的任务目录"
    );
    let db = Database::open(directory)?;
    // 锁位置：prepare 已把当时的全局锁目录记进任务库（决策规则见 lock_dir_for）。
    let recorded: Option<PathBuf> = db.get::<String>("state_dir").ok().map(PathBuf::from);
    let _guard = fsutil::RootGuard::acquire(&lock_dir_for(directory, recorded.as_deref())?)?;
    let status: String = db.get("status")?;
    if status != "ready" {
        bail!("任务不是待确认状态（{status}）；请重新扫描，不会盲目重放旧计划");
    }
    let root_text: String = db.get("root")?;
    let root = fsutil::normalize_root(Path::new(&root_text))?;
    // prepare 记录的是当时的规范化路径；apply 重新解析 canonicalize（会解析 junction）。
    // 两者不一致说明目录被移动或被替换成指向别处的链接，继续执行会把整理动作落到另一棵树上。
    anyhow::ensure!(
        fsutil::path_string(&root)? == root_text,
        "目录位置已改变或被链接替换（{root_text}）；请重新扫描生成新计划后再执行"
    );
    let config = db.config()?;
    config.validate()?;
    let summary = db.summary()?;
    let mut job = Job {
        root,
        config,
        context,
        db,
        summary,
        archive_override: None,
    };
    job.db.set("status", &"executing")?;
    let outcome = (|| {
        // 上次执行崩溃可能残留硬链接临时文件（.jchtools-link-*）：执行前清理。
        let removed_link_temps = clean_orphan_link_temps(&job.root);
        if removed_link_temps > 0 {
            job.log(
                "任务",
                "",
                "",
                "提示",
                &format!("已清理 {removed_link_temps} 个上次执行崩溃残留的硬链接临时文件"),
                0,
            )?;
        }
        // 性能打点（perf-tracing，默认不编译）：计划动作的实际执行区间；与前面的
        // 崩溃残留清理、后面的收尾写库分开计时。
        crate::perf::perf_span!("execute_actions");
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
                for action in actions {
                    cursor = action.id;
                    job.context.control.checkpoint()?;
                    if !action.selected {
                        job.summary.skipped += 1;
                        job.db.mark_action(action.id, "unselected")?;
                        // 未勾选不计入 completed：GUI 分母 count_selected_pending 只含 selected+pending，
                        // 分子若含 unselected 会出现 done>planned、提前 100% 的口径分裂。
                        continue;
                    }
                    job.context
                        .status(format!("执行 {:?}：{}", action.kind, action.source));
                    match execute_action(&mut job, &action) {
                        Ok(true) => job.db.mark_action(action.id, "done")?,
                        Ok(false) => {
                            job.summary.skipped += 1;
                            job.db.mark_action(action.id, "skipped")?;
                        }
                        Err(error) => {
                            // 用户主动取消不是失败：不计 errors、不标 failed，与 prepare 阶段取消口径一致。
                            if job.context.control.is_cancelled() {
                                job.context.control.check_cancelled()?;
                            }
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
                            job.context.control.check_cancelled()?;
                        }
                    }
                    job.context
                        .control
                        .completed
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            })();
            match batch {
                Ok(()) => job.db.conn.execute_batch("COMMIT")?,
                Err(error) => {
                    // 尽力提交：SQLite 已因致命错误自行回滚时 COMMIT 会失败，此时无需处理。
                    let _ = job.db.conn.execute_batch("COMMIT");
                    return Err(error);
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    })();
    job.db.set("summary", &job.summary)?;
    job.db.set(
        "status",
        &if outcome.is_ok() {
            "finished"
        } else if job.context.control.is_cancelled() {
            "cancelled"
        } else {
            "failed"
        },
    )?;
    outcome?;
    #[cfg(feature = "perf-tracing")]
    crate::perf::apply_done(
        job.summary.deleted,
        job.summary.moved,
        job.summary.linked,
        job.summary.skipped,
        job.summary.errors,
    );
    Ok(TaskResult {
        directory: directory.to_path_buf(),
        summary: job.summary,
    })
}
fn execute_action(job: &mut Job, action: &Action) -> Result<bool> {
    // P-08：处理期间假定文件不被其他程序改动，执行阶段不再做快照比对；
    // S-03/C-12：删除前 `MUST NOT` 重读文件内容做逐字节复核，去重判定完全依据
    // 分析期算出的整文件哈希，因此执行阶段不读取任何文件内容。
    let source = fsutil::safe_join(&job.root, &action.source)?;
    // keeper 仅用于硬链接动作（同卷硬链接的链接源）；内容去重删除不再需要它。
    let keeper = if action.kind == ActionKind::Hardlink {
        let (relative, _) = action.keeper.as_ref().context("硬链接缺少保留文件")?;
        Some(fsutil::safe_join(&job.root, relative)?)
    } else {
        None
    };
    match action.kind {
        ActionKind::Delete => Ok(job.delete_path(
            &source,
            action.expected.as_ref(),
            action.mode,
            &action.reason,
            true,
        )? != DeleteResult::Kept),
        ActionKind::Move => {
            let target = fsutil::safe_join(
                &job.root,
                action.target.as_deref().context("移动操作缺少目标")?,
            )?;
            fsutil::ensure_parent(&job.root, &target)?;
            job.log(
                "移动",
                &action.source,
                action.target.as_deref().unwrap_or(""),
                "准备",
                &action.reason,
                action.expected.as_ref().map_or(0, |s| s.size),
            )?;
            fsutil::rename_noreplace(&source, &target)?;
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
        ActionKind::Hardlink => {
            let keeper = keeper.context("硬链接缺少保留文件")?;
            let temporary = source
                .parent()
                .context("路径缺少父目录")?
                .join(format!(".jchtools-link-{}", uuid::Uuid::new_v4()));
            // Create the replacement link before removing any source data. Cross-volume links fail safely here.
            fs::hard_link(&keeper, &temporary).context("此位置不支持硬链接，原文件未删除")?;
            // physical_free=false：内容经临时硬链接原样保留，物理占用不变，不得计入永久删除字节。
            let removed = job.delete_path(
                &source,
                action.expected.as_ref(),
                action.mode,
                &action.reason,
                false,
            );
            match removed {
                Ok(DeleteResult::Kept) => {
                    let _ = fs::remove_file(&temporary);
                    return Ok(false);
                }
                Err(error) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(error);
                }
                _ => (),
            }
            if let Err(error) = fsutil::rename_noreplace(&temporary, &source) {
                // Keep the replacement link under a visible name if a competing file appeared.
                let emergency = fsutil::unique_target(&job.root, &source);
                match emergency {
                    Ok(emergency) => match fsutil::rename_noreplace(&temporary, &emergency) {
                        Ok(()) => {
                            // 原路径已被竞争文件占用：链接落到应急名称，用户承诺的原路径不再可用。
                            let emergency_rel = fsutil::relative_string(&job.root, &emergency)
                                .unwrap_or_else(|_| emergency.display().to_string());
                            job.log("硬链接",&action.source,&emergency_rel,"警告",
                                &format!("原路径被竞争占用，链接已改用应急名称保留（承诺的原路径不再可用）：{error}"),0)?;
                            job.summary.linked += 1;
                            return Ok(true);
                        }
                        // 兜底改名也失败时移除临时硬链接：内容仍由保留文件持有，不会丢数据。
                        // 源路径已被删除且没有留下任何链接：必须报错并标 failed，不得记 skipped
                        //（否则与已入账的 deleted 口径分裂）。
                        Err(inner) => {
                            let _ = fs::remove_file(&temporary);
                            job.log("硬链接",&action.source,"","失败",
                                &format!("硬链接失败，原路径已删除且未留下链接；内容仍由保留文件持有：{inner}（首次改名失败：{error}）"),0)?;
                            anyhow::bail!("硬链接失败：源路径已删除且未留下链接；内容仍由保留文件持有（{inner}）");
                        }
                    },
                    Err(inner) => {
                        let _ = fs::remove_file(&temporary);
                        return Err(inner).context("目标被占用，且无法为保留链接副本分配名称");
                    }
                }
            }
            job.summary.linked += 1;
            Ok(true)
        }
        ActionKind::EmptyDirectory => {
            if !source.try_exists()? || !source.is_dir() || fs::read_dir(&source)?.next().is_some()
            {
                return Ok(false);
            }
            Ok(
                job.delete_path(&source, None, action.mode, &action.reason, true)?
                    != DeleteResult::Kept,
            )
        }
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;
    // 平台门禁原因：仅 Windows 门禁测试使用（UNC/盘符前缀拼接），非 Windows 无使用者。
    #[cfg(windows)]
    const BSLASH: char = std::path::MAIN_SEPARATOR; // Windows 下为反斜杠
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
}
