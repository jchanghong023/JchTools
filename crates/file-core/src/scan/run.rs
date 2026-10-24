//! scan() 主流程编排：范围提示与剪枝口径装配（S-04/S-05）、并行扫描调度、
//! 结果去重入库事务与取消/回滚收尾。

use super::emit::scan_emit;
use super::preview3;
use super::records::{ScanChild, ScanSink};
use super::scope::{
    ensure_git_boundary_checked, program_self_directory_hint, special_prefixes, ScopeFilter,
};
use super::walk::{io_pool, walk_dir, WalkCtx};
use crate::{
    config::Config,
    control::Event,
    fsutil, rules,
    task::{observe, Job, QUARANTINE_DIR_NAME},
};
use anyhow::Result;
use rusqlite::params;
use std::{
    collections::HashMap,
    fs,
    path::Path,
    sync::{atomic::Ordering, Mutex},
};

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "scan", skip_all)
)]
pub fn scan(job: &mut Job, enqueue: bool, state: &Path, protected: Option<&Path>) -> Result<()> {
    let control = job.context.control.clone();
    observe("scan", Some(&control), |stage| {
        *stage = "directory_scan";
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
        // S-05：根包含 Windows 系统目录时整树剪枝该子树（相对前缀与状态/程序目录同口径）。
        let protected_prefix = protected.and_then(|dir| fsutil::relative_string(&root, dir).ok());
        let excluded = rules::build_exclusions(&config.exclusions)?;
        let state = fs::canonicalize(state)?;
        let executable_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().and_then(|d| fs::canonicalize(d).ok()));
        if let (Some(directory), Ok(target)) = (&executable_dir, fs::canonicalize(&root)) {
            if target.starts_with(directory) {
                job.log(
                    "扫描",
                    "",
                    "",
                    "提示",
                    &program_self_directory_hint(directory),
                    0,
                )?;
            }
        }
        log_scope_hints(job, &config, protected_prefix.as_deref())?;
        // 状态目录/程序目录的剪枝口径与 count_archives、收尾实空清理共用；选定根位于
        // 它们内部（含重合）时，整棵树按旧口径全部剪枝。
        let (state_prefix, exe_prefix, root_under_special) =
            special_prefixes(&root, Some(&state), executable_dir.as_deref());
        let scope_filter = ScopeFilter {
            excluded: &excluded,
            include_hidden: config.include_hidden,
            include_system: config.include_system,
            quarantine: Some(QUARANTINE_DIR_NAME),
            state_prefix,
            exe_prefix,
            protected_prefix,
            root_under_special,
        };
        let sink = Mutex::new(ScanSink::default());
        let ctx = WalkCtx {
            control: job.context.control.clone(),
            scope: &scope_filter,
            recursive: config.recursive,
            sink: &sink,
        };
        let pool = io_pool()?;
        let scan_span = tracing::Span::current();
        pool.install(|| {
            rayon::scope(|scope| {
                scope.spawn(|scope| {
                    scan_span.in_scope(|| walk_dir(scope, &ctx, &root, String::new(), 0));
                });
            });
        });
        // 用户取消：不做入库与后续阶段（旧实现同样以取消错误中止扫描）。
        job.context.control.checkpoint()?;
        let mut sink = sink
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for note in &sink.notes {
            job.summary.errors += 1;
            job.log("扫描", &note.path, "", "跳过", &note.message, 0)?;
        }
        let git_roots = log_skip_outcomes(job, &mut sink, enqueue)?;
        // 汇总入库：按父目录分组还原深度先序；污点表供 planner 的空目录规划排除，
        // git_roots 供 planner 拒绝把内容归入 Git 工作树（H-06：不归类）。
        let mut by_parent: HashMap<String, Vec<ScanChild>> = HashMap::new();
        for child in sink.children {
            by_parent
                .entry(child.parent.clone())
                .or_default()
                .push(child);
        }
        let mut taint: Vec<String> = sink.taint.into_iter().collect();
        taint.sort();
        persist_scan_results(job, &by_parent, &taint, &git_roots, enqueue)?;
        Ok(())
    })
}

/// S-04：任何缩小处理范围的选择都必须在日志里明确提示，避免静默漏处理；
/// 各项提示合并为一条范围提示日志，含排除规则条目计数（规则文本在界面上可查）。
fn log_scope_hints(job: &mut Job, config: &Config, protected_prefix: Option<&str>) -> Result<()> {
    let mut reduced: Vec<String> = Vec::new();
    if !config.recursive {
        reduced.push("递归已关闭（只处理所选目录的第一层）".into());
    }
    if !config.include_hidden {
        reduced.push("未包含隐藏属性资料".into());
    }
    if !config.include_system {
        reduced.push("未包含系统属性资料".into());
    }
    if protected_prefix.is_some() {
        // S-05 第二句：选择包含系统目录的更高层根时整树排除该系统目录并提示。
        reduced.push(
            "已排除 Windows 系统目录（系统目录及其全部内容不参与本次处理，也不会被改动）".into(),
        );
    }
    // 规则的完整文本可能很长（默认列表就跨多行），这里只报条数，规则本身在界面上可查。
    let exclusion_rules = config
        .exclusions
        .split(';')
        .filter(|part| !part.trim().is_empty())
        .count();
    if exclusion_rules > 0 {
        reduced.push(format!("排除规则生效（{exclusion_rules} 条）"));
    }
    if !reduced.is_empty() {
        job.log(
            "扫描",
            "",
            "",
            "提示",
            &format!(
                "范围提示：{}；这些资料不参与本次处理，也不会被改动",
                reduced.join("；")
            ),
            0,
        )?;
    }
    Ok(())
}

/// 扫描收尾的三类跳过提示：隔离目录（逐个列路径）、解压工作区与 Git 整树（H-06）。
/// Git 提示按流程区分「跳过/识别」双文案（解压入队 vs 只读分析），日志与界面
/// Notice 同时给出。返回去重后的 Git 根清单，供入库与 planner 使用。
fn log_skip_outcomes(job: &mut Job, sink: &mut ScanSink, enqueue: bool) -> Result<Vec<String>> {
    let mut git_roots = std::mem::take(&mut sink.git_skips);
    ensure_git_boundary_checked(sink.git_boundary_error.as_deref())?;
    sink.quarantine_skips.sort();
    sink.quarantine_skips.dedup();
    for rel in &sink.quarantine_skips {
        let shown = if rel.is_empty() {
            job.root.display().to_string()
        } else {
            rel.clone()
        };
        job.log(
            "扫描",
            rel,
            "",
            "提示",
            &format!("已跳过隔离目录「{shown}」：内容不参与本次处理；需要重试时请先移出"),
            0,
        )?;
    }
    sink.workspace_skips.sort();
    sink.workspace_skips.dedup();
    for rel in &sink.workspace_skips {
        job.log(
            "扫描",
            rel,
            "",
            "提示",
            "已跳过带所有权标记的解压工作区；未读取其内容，不自动清理其他任务的资料",
            0,
        )?;
    }
    if !git_roots.is_empty() {
        // H-06：Git 整树保护必须明确提示处置结果（界面蓝条 + 日志），不静默跳过。
        // 解压流程里 Git 项目整树排除；目录整理流程里按 C-14 固定行为整体移入「Git项目集合」，
        // 两者都不读取内容、不进入树内处理，但「处置结果」必须各自说清，不能都说成「跳过」。
        git_roots.sort();
        git_roots.dedup();
        let shown = preview3(&git_roots);
        let (log_message, notice) = if enqueue {
            (
                format!(
                    "已跳过 {} 个 Git 目录及其全部内容（含 .git 的目录整树排除：不读取、不归类、不改名、不删除，不受隐藏/递归/清理开关影响）：{shown}",
                    git_roots.len()
                ),
                format!(
                    "已跳过 {} 个 Git 目录树（含全部后代，未读取内容）",
                    git_roots.len()
                ),
            )
        } else {
            (
                format!(
                    "已识别 {} 个 Git 项目（含 .git 的目录整树保护：不读取内容、不进入树内改名/去重/清理；可归类项目整树移入本次所选根下的「Git项目集合」，已就位或占用根层固定容器的项目原位保留）：{shown}",
                    git_roots.len()
                ),
                format!(
                    "已识别 {} 个 Git 项目（未读取内容；已就位或占用固定容器的项目保留）",
                    git_roots.len()
                ),
            )
        };
        job.log("扫描", "", "", "提示", &log_message, 0)?;
        job.context.emit(Event::Notice(notice));
    }
    Ok(git_roots)
}

/// 汇总结果入库：污点表与 Git 根先写连接内临时表，再按父目录分组深度先序回放插入；
/// 全程单事务（BEGIN IMMEDIATE），失败回滚并在回滚失败时告警，不吞原始错误。
fn persist_scan_results(
    job: &mut Job,
    by_parent: &HashMap<String, Vec<ScanChild>>,
    taint: &[String],
    git_roots: &[String],
    enqueue: bool,
) -> Result<()> {
    job.db.conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        job.db.conn.execute_batch(
            "DROP TABLE IF EXISTS scan_taint; CREATE TEMP TABLE scan_taint(rel TEXT PRIMARY KEY);
         DROP TABLE IF EXISTS git_roots; CREATE TEMP TABLE git_roots(rel TEXT PRIMARY KEY)",
        )?;
        for rel in taint {
            job.db.remember_taint(rel)?;
        }
        {
            let mut statement = job
                .db
                .conn
                .prepare_cached("INSERT OR IGNORE INTO git_roots(rel) VALUES(?1)")?;
            for rel in git_roots {
                statement.execute(params![rel])?;
            }
        }
        if !enqueue {
            // 盘点用 git_roots 是连接内临时表；重算计划需在重新打开任务库后
            // 知道哪些 C-14 Move 未完成，故同时持久化这份边界清单。
            job.db.set("git_roots", &git_roots)?;
        }
        let mut count = 0u64;
        scan_emit(job, by_parent, enqueue, "", 1, &mut count)?;
        Ok::<_, anyhow::Error>(())
    })();
    match result {
        Ok(()) => job.db.conn.execute_batch("COMMIT")?,
        Err(error) => {
            if job.db.conn.execute_batch("ROLLBACK").is_err() {
                tracing::warn!(
                    event = "task_transaction_cleanup_failed",
                    stage = "scan_rollback",
                    error_type = "database",
                    "扫描失败后的事务回滚失败"
                );
            }
            return Err(error);
        }
    }
    Ok(())
}
