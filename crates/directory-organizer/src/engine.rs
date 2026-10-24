use crate::planner;
use anyhow::{bail, Result};
use jchtools_file_core::{
    config::{self, Config},
    control::Context as TaskContext,
    db::Database,
    fsutil,
    model::ActionKind,
    scan::{io_pool, scan},
    task::{
        create_task_db, observe, preflight_root, record_task_failure, resolve_protection, Job,
        TaskResult,
    },
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

mod actions;
mod cleanup;
mod hash;
mod planning;

pub use planning::recompute_plan;

use actions::{execute_planned_moves, execute_run, execute_sequential, parallel_run_kind};
use cleanup::final_empty_cleanup;
use hash::hash_candidates;
use planning::lock_dir_for;

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
            let mut candidate_identities = HashSet::new();
            in_transaction(
                &mut job,
                "cleanup_plan_rollback",
                "清理计划失败后的事务回滚失败",
                |job| planner::cleanup_candidates(job, &mut candidate_identities),
            )?;
            *stage = "hash";
            hash_candidates(&mut job)?;
            job.context
                .status("生成去重、冲突、归类和清理计划（尚未执行这些操作）");
            *stage = "plan";
            in_transaction(
                &mut job,
                "plan_rollback",
                "计划失败后的事务回滚失败",
                |job| planner::build_after_cleanup(job, &mut candidate_identities),
            )?;
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
    tracing::instrument(target = "perf", name = "organize_apply", skip_all)
)]
pub fn apply(directory: &Path, context: TaskContext) -> Result<TaskResult> {
    let control = context.control.clone();
    observe("organize_apply", Some(&control), |stage| {
        let task_span = tracing::info_span!("file_task", task_id = %jchtools_foundation::logging::safe_error(&directory.file_name().unwrap_or_default().to_string_lossy()));
        let _task_entered = task_span.enter();
        let (_guard, mut job, protected, errors_before_apply) =
            open_ready_task(directory, context, stage)?;
        *stage = "executing_status";
        job.db.set("status", &"executing")?;
        let outcome = execute_plan(&mut job, protected.as_deref(), stage);
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
            &terminal_state(&outcome, &job, errors_before_apply),
        )?;
        tracing::info!(
            event = "task_state_changed",
            state = terminal_state(&outcome, &job, errors_before_apply),
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

/// 打开并校验待执行任务（apply 的全部安全前置，校验先后顺序敏感）：
/// task.sqlite3 存在性 → 开库 → 全局锁 → status 校验 → 根一致性与配置校验 → 组装 Job。
/// Database::open 会在文件缺失时创建一个空库；先确认这是扫描生成的任务目录，
/// 避免把任意目录（甚至写错路径）悄悄变成一个必然失败的空任务。
/// 该检查必须先于 RootGuard：否则写错路径会先在错误位置创建目录并落下锁文件。
/// 锁 guard 由调用方持有：与原实现一致，锁存活到执行收尾，且在 Job（任务库连接）
/// 之后释放。
fn open_ready_task(
    directory: &Path,
    context: TaskContext,
    stage: &mut &'static str,
) -> Result<(fsutil::RootGuard, Job, Option<PathBuf>, u64)> {
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
    let guard = fsutil::RootGuard::acquire(&lock_dir_for(directory, recorded.as_deref())?)?;
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
    let job = Job {
        root,
        config,
        context,
        db,
        summary,
    };
    Ok((guard, job, protected, errors_before_apply))
}

/// 执行计划动作（apply 的 outcome 段）：临时表、分段执行循环与最终实空清理。
/// 出错/取消时把错误原样返回调用方，由主流程统一记录终态与日志。
fn execute_plan(job: &mut Job, protected: Option<&Path>, stage: &mut &'static str) -> Result<()> {
    job.context.control.checkpoint()?;
    if !job.config.recursive {
        // 仅本次连接记录归类实际新建的目标目录，不借固定名称或历史任务
        // 推断所有权；原有后代始终在非递归收尾范围之外（H-05/R-03）。
        job.db
            .conn
            .execute_batch("CREATE TEMP TABLE organizer_created_dirs(rel TEXT PRIMARY KEY);")?;
    }
    // 性能打点（perf-tracing，默认不编译）：计划动作的实际执行区间，
    // 与后面的收尾写库分开计时。
    jchtools_foundation::perf::perf_span!("execute_actions");
    let pool = io_pool()?;
    *stage = "execute_actions";
    let mut moves_executed = false;
    let mut cursor = 0;
    // 本次执行独享，跨页/串并行结算共享；不沿用分析的候选身份集合。
    let mut deleted_identities = HashSet::new();
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
                        execute_planned_moves(job)?;
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
                    execute_sequential(job, &actions[index], &mut deleted_identities)?;
                    index += 1;
                    continue;
                };
                let mut end = index + 1;
                while end < actions.len() && parallel_run_kind(&actions[end]) == Some(kind) {
                    end += 1;
                }
                execute_run(
                    job,
                    &pool,
                    &actions[index..end],
                    kind,
                    &mut deleted_identities,
                )?;
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
    final_empty_cleanup(job, protected)?;
    Ok(())
}

/// 执行终态三态判定，写库与日志共用同一口径（改口径只需改这一处）：
/// 执行阶段有计划行失败（errors 新增）的任务不得标「finished」（C-11/C-01）；
/// 用户取消优先于失败口径（C-10 取消不算失败）。
fn terminal_state(outcome: &Result<()>, job: &Job, errors_before: u64) -> &'static str {
    if outcome.is_ok() && job.summary.errors == errors_before {
        "finished"
    } else if job.context.control.is_cancelled() {
        "cancelled"
    } else {
        "failed"
    }
}

/// 事务包裹：BEGIN IMMEDIATE → step → Ok 则 COMMIT；Err 则 ROLLBACK（回滚自身
/// 失败时按 stage 记 warn）后把原错误返回调用方。仅包「失败即整体放弃」的事务；
/// apply 执行事务「尽力提交保留已完成部分」的语义不同，不并到这里。
fn in_transaction(
    job: &mut Job,
    rollback_stage: &'static str,
    rollback_message: &'static str,
    step: impl FnOnce(&mut Job) -> Result<()>,
) -> Result<()> {
    job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
    match step(job) {
        Ok(()) => job.db.conn.execute_batch("COMMIT")?,
        Err(error) => {
            if job.db.conn.execute_batch("ROLLBACK").is_err() {
                tracing::warn!(
                    event = "task_transaction_cleanup_failed",
                    stage = rollback_stage,
                    error_type = "database",
                    "{}",
                    rollback_message
                );
            }
            return Err(error);
        }
    }
    Ok(())
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
mod lock_tests;
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
mod s05_scope_tests;
