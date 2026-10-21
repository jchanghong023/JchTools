use super::planning::{dir_recheck_empty, path_is_at_or_below, task_git_roots};
use crate::planner;
use anyhow::{Context, Result};
use jchtools_file_core::{
    fsutil,
    model::{Action, ActionKind},
    platform::{self, DeleteResult},
    task::{observe, Job},
};
use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

/// 可并行执行的动作段类别：文件删除彼此独立；空目录删除按深度分层
/// （计划按深度降序生成，同层目录不互为父子，深层已在更早的段/页删除）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParallelKind {
    Delete,
    EmptyDir(usize),
}
pub(super) fn parallel_run_kind(action: &Action) -> Option<ParallelKind> {
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
pub(super) fn execute_sequential(
    job: &mut Job,
    action: &Action,
    identities: &mut HashSet<String>,
) -> Result<()> {
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
        match execute_action(job, action, identities) {
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
pub(super) fn execute_run(
    job: &mut Job,
    pool: &rayon::ThreadPool,
    run: &[Action],
    kind: ParallelKind,
    identities: &mut HashSet<String>,
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
    settle_parallel_results(job, run, results, identities)
}

pub(super) fn settle_parallel_results(
    job: &mut Job,
    run: &[Action],
    results: Vec<Result<Option<DeleteResult>>>,
    identities: &mut HashSet<String>,
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
                record_permanent_delete(job, action, identities)?;
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

fn record_permanent_delete(
    job: &mut Job,
    action: &Action,
    identities: &mut HashSet<String>,
) -> Result<()> {
    job.summary.deleted += 1;
    job.summary.permanent_bytes =
        job.summary
            .permanent_bytes
            .saturating_add(planner::count_logical_bytes(
                action.expected.as_ref(),
                identities,
            ));
    job.db.deactivate_file_rel(&action.source)?;
    Ok(())
}
/// 执行所有归类/项目移动，并处理「目标是另一项源」的依赖。
/// `a -> b` 与 `b -> c` 要先释放依赖源；交换环则先把一个源临时暂存。
/// Git 项目根始终受保护，直到其 C-14 Move 真正成功；失败后拒绝树内目标，独立项继续执行。
pub(super) fn execute_planned_moves(job: &mut Job) -> Result<()> {
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
    // 仅遇到真实交换环才创建暂存；每个环在源目录旁独占建工作区，保证暂存同卷。
    let mut stage_guards = Vec::new();
    let mut staged: HashMap<i64, PathBuf> = HashMap::new();
    while !actions.is_empty() {
        if let Some(index) = actions.iter().position(|action| !action.selected) {
            let action = actions.remove(index);
            execute_sequential(job, &action, &mut HashSet::new())?;
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
        let stage = source
            .parent()
            .context("交换环源路径没有父目录")
            .and_then(MoveStageGuard::new);
        let staged_result = stage.and_then(|guard| {
            let temporary = guard.directory.join("content").join(action.id.to_string());
            let result = fsutil::move_file_preserving_times(&source, &temporary);
            stage_guards.push(guard);
            result.map(|()| temporary)
        });
        if let Err(error) = &staged_result {
            let action = actions.remove(0);
            pending_git_moves.remove(&fsutil::fold_rel(&action.source));
            record_move_failure(job, &action, error)?;
            job.context
                .control
                .completed
                .fetch_add(1, Ordering::Relaxed);
        } else if let Ok(temporary) = staged_result {
            staged.insert(action.id, temporary);
        }
    }
    drop(stage_guards);
    Ok(())
}

pub(super) struct MoveStageGuard {
    pub(super) directory: PathBuf,
    pub(super) owner: String,
    pub(super) initialized: bool,
}

impl MoveStageGuard {
    pub(super) fn new(parent: &Path) -> Result<Self> {
        // 不复用任何既有容器（包括同名用户目录/Git/链接），只清理本次独占创建项。
        let container = (0u64..)
            .find_map(|index| {
                let path = parent.join(if index == 0 {
                    ".jchtools-work".to_string()
                } else {
                    fsutil::suffixed_candidate(".jchtools-work", "", index)
                });
                match fs::create_dir(&path) {
                    Ok(()) => Some(Ok(path)),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .context("无法选择交换环暂存目录")??;
        let owner = uuid::Uuid::new_v4().to_string();
        let directory = container.join(&owner);
        if let Err(error) = fs::create_dir(&directory) {
            let _ = fs::remove_dir(&container);
            return Err(error).context("无法创建交换环暂存工作区");
        }
        // 独占创建后立即持有守卫，OWNER/content 初始化失败也只清理自己的空目录。
        let mut guard = Self {
            directory,
            owner,
            initialized: false,
        };
        fs::write(guard.directory.join("OWNER"), &guard.owner)
            .context("无法写入交换环暂存所有权标记")?;
        fs::create_dir(guard.directory.join("content")).context("无法创建交换环暂存内容目录")?;
        guard.initialized = true;
        Ok(guard)
    }
}

impl Drop for MoveStageGuard {
    fn drop(&mut self) {
        if self.initialized
            && fs::read_to_string(self.directory.join("OWNER"))
                .ok()
                .as_deref()
                != Some(self.owner.as_str())
        {
            return;
        }
        // 失败时 content 可能仍有唯一副本，绝不递归删除；保留完整标记供扫描保护和恢复。
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return;
            };
            if entry.file_name() != "OWNER" && entry.file_name() != "content" {
                return;
            }
        }
        match fs::remove_dir(self.directory.join("content")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return,
        }
        match fs::remove_file(self.directory.join("OWNER")) {
            Ok(()) => {}
            Err(error) if !self.initialized && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return,
        }
        if fs::remove_dir(&self.directory).is_ok() {
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

fn execute_action(
    job: &mut Job,
    action: &Action,
    identities: &mut HashSet<String>,
) -> Result<bool> {
    // P-08：处理期间假定文件不被其他程序改动，执行阶段不再做快照比对；
    // S-03/C-12：删除前 `MUST NOT` 重读文件内容做逐字节复核，去重判定完全依据
    // 分析期算出的整文件哈希，因此执行阶段不读取任何文件内容。
    let source = fsutil::safe_join(&job.root, &action.source)?;
    match action.kind {
        ActionKind::Delete => {
            let size = action.expected.as_ref().map_or(0, |snapshot| snapshot.size);
            job.log("删除", &action.source, "", "准备", &action.reason, size)?;
            let result = platform::remove(&source, action.mode, &job.context.control)?;
            if result == DeleteResult::Permanent {
                record_permanent_delete(job, action, identities)?;
            }
            job.log(
                "删除",
                &action.source,
                "",
                if result == DeleteResult::Permanent {
                    "已永久删除"
                } else {
                    "保留"
                },
                &action.reason,
                size,
            )?;
            Ok(result != DeleteResult::Kept)
        }
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
