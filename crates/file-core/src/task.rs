use crate::{
    config::{Config, DeleteMode},
    control::{Context as TaskContext, Control, Event},
    db::Database,
    fsutil,
    model::{Snapshot, Summary},
    platform::{self, DeleteResult},
};
use anyhow::Result;
use rusqlite::params;
use std::path::{Path, PathBuf};

/// 「解压失败」子目录名（X-06）：建在所选目录根下，收纳未能完全解开的原包。
/// 扫描（两工具）与重跑计数默认排除它（X-07 / C-09）。
pub const QUARANTINE_DIR_NAME: &str = "解压失败";
/// C-14 固定集合容器名（建在本次所选根下）。
pub const GIT_COLLECTION_DIR: &str = "Git项目集合";

pub struct Job {
    pub root: PathBuf,
    pub config: Config,
    pub context: TaskContext,
    pub db: Database,
    pub summary: Summary,
}
/// H-06：选定根目录直接含 .git（目录或文件）时整次处理不执行的统一提示。
/// 只允许为识别边界做必要的目录项检查，识别后不读取、不改动任何内容。
const ROOT_GIT_MESSAGE: &str = "所选根目录直接含 .git（Git 仓库或工作树）；按 H-06 整次处理不执行。请改选不含 .git 的子目录后重试。";
/// X-07：所选根本身名为「解压失败」时不开始解压的统一提示（确认框清点与解压一致拒绝）。
const ROOT_QUARANTINE_MESSAGE: &str = "所选目录本身名为「解压失败」（隔离容器）；按 X-07 不开始解压。请先将待重试的包移出隔离容器后重试。";
/// X-07：所选根本身名为「解压失败」（不区分大小写）时拒绝开始解压。
/// 容器名本身没有大小写变体，ASCII 折叠与 archive.rs 的组件名判定同口径。
pub(crate) fn root_is_quarantine(root: &Path) -> bool {
    root.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(QUARANTINE_DIR_NAME))
}
/// 只观察任务边界；错误正文可能来自文件或子进程，因此仅输出类型与系统码。
pub fn observe<T>(
    operation: &'static str,
    control: Option<&Control>,
    run: impl FnOnce(&mut &'static str) -> Result<T>,
) -> Result<T> {
    let span = jchtools_foundation::logging::operation_span("engine", operation);
    let _entered = span.enter();
    let started = std::time::Instant::now();
    let mut stage = "input";
    tracing::debug!(event = "file_task_started", operation, "文件任务开始");
    let result = run(&mut stage);
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match &result {
        Ok(_) => tracing::debug!(
            event = "file_task_completed",
            operation,
            stage,
            elapsed_ms,
            "文件任务完成"
        ),
        Err(_) if control.is_some_and(Control::is_cancelled) => {
            tracing::info!(
                event = "file_task_cancelled",
                operation,
                stage,
                elapsed_ms,
                "文件任务已取消"
            );
        }
        Err(error) => {
            let io = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<std::io::Error>());
            tracing::error!(
                event = "file_task_failed",
                operation,
                stage,
                elapsed_ms,
                error_type = if io.is_some() {
                    "io"
                } else if error
                    .chain()
                    .any(<dyn std::error::Error + 'static>::is::<rusqlite::Error>)
                {
                    "database"
                } else {
                    "task_boundary"
                },
                error_code = io.and_then(std::io::Error::raw_os_error),
                "文件任务失败"
            );
        }
    }
    result
}
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
        let task_id = self
            .db
            .directory
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        if result == "失败" || result.contains("失败") {
            tracing::warn!(event = "task_item_failed", task_id = %task_id, phase,
                source = %jchtools_foundation::logging::safe_error(source),
                target = %jchtools_foundation::logging::safe_error(target), size,
                reason = %jchtools_foundation::logging::safe_error(reason),
                result, error_type = "task_item_boundary", "关键任务逐项失败");
        } else {
            tracing::debug!(event = "task_item_result", task_id = %task_id, phase,
                source = %jchtools_foundation::logging::safe_error(source),
                target = %jchtools_foundation::logging::safe_error(target), result, size, "任务逐项结果");
        }
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
        let control = self.context.control.clone();
        observe("delete_path", Some(&control), |stage| {
            *stage = "delete_validation_or_audit";
            let relative = fsutil::relative_string(&self.root, path)?;
            fsutil::safe_join(&self.root, &relative)?;
            let size = expected.map_or(0, |s| s.size);
            // Record intent before a mutation. This is an audit trail, not a recovery journal.
            self.log("删除", &relative, "", "准备", reason, size)?;
            *stage = "filesystem_delete";
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
            *stage = "delete_database_finalize";
            if result != DeleteResult::Kept {
                self.db.deactivate_file_rel(&relative)?;
            }
            Ok(result)
        })
    }
}
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub directory: PathBuf,
    pub summary: Summary,
}
/// 解析 S-05 的受保护目录：Windows 读取 SystemRoot 并规范化（缺失或不可访问时
/// 报错拒绝开始，不得静默失去系统目录保护）；非 Windows 无此概念，返回 None。
#[cfg(windows)]
pub fn resolve_protection() -> Result<Option<PathBuf>> {
    Ok(Some(fsutil::protected_root()?))
}
#[cfg(not(windows))]
pub fn resolve_protection() -> Result<Option<PathBuf>> {
    Ok(None)
}
/// 三条任务入口（prepare / extract_run / count_archives）共用的安全闸门，顺序固定：
/// 配置校验 → S-04 原始路径链接边界 → 规范化 →（仅解压链路）X-07 隔离区名拒绝 →
/// H-06 根含 .git 拒绝 → H-06 祖先含 .git 拒绝。任一闸门失败都不创建任务库；
/// `check_quarantine` 只在解压链路为 true（目录整理不解压，C-01）。
/// 返回规范化后的根目录与 S-05 受保护目录（供扫描剪枝继续使用）。
pub fn preflight_root(
    root: &Path,
    config: &Config,
    protected: Option<&Path>,
    check_quarantine: bool,
) -> Result<(PathBuf, Option<PathBuf>)> {
    if check_quarantine {
        config.validate_extract()?;
    } else {
        config.validate_organizer()?;
    }
    // S-04：先在用户原始路径上检查链接边界（canonicalize 会解析掉 reparse 身份）；
    // 拒绝时不创建任务库、不扫描、不解压。
    fsutil::ensure_plain_entry(root)?;
    let (root, protected) = fsutil::normalize_root_with(root, protected)?;
    // X-07：所选根本身名为「解压失败」时不开始解压，提示先移出待重试的包。
    if check_quarantine {
        anyhow::ensure!(!root_is_quarantine(&root), ROOT_QUARANTINE_MESSAGE);
    }
    // H-06：选定根目录直接含 .git 时整次处理不执行，明确提示且不创建任务库。
    anyhow::ensure!(!fsutil::is_git_root(&root)?, ROOT_GIT_MESSAGE);
    // H-06：祖先直接含 .git 同样拒绝整次处理（不拆散项目子树）。
    fsutil::root_inside_git_project(&root)?;
    Ok((root, protected))
}
/// prepare 与 extract 共用的任务库创建：持全局锁 → 任务目录命名 → 建库 →
/// 初始化事务写入 root/config/status/summary/created/state_dir → 提交。
/// 返回的锁守卫必须由调用方持有到任务结束（drop 即释放互斥）。
pub fn create_task_db(
    state: &Path,
    root: &Path,
    config: Config,
    context: TaskContext,
    status: &str,
) -> Result<(fsutil::RootGuard, Job)> {
    let guard = fsutil::RootGuard::acquire(state)?;
    let directory = state.join("tasks").join(format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        uuid::Uuid::new_v4()
    ));
    let db = Database::create(&directory)?;
    let initialization = db.conn.unchecked_transaction()?;
    db.set("root", &fsutil::path_string(root)?)?;
    db.set("config", &config)?;
    db.set("status", &status)?;
    db.set("summary", &Summary::default())?;
    db.set("created", &chrono::Utc::now().to_rfc3339())?;
    // 记录本次的全局锁目录：apply 若按任务目录当前位置推导锁位置，任务目录被移动后
    // 会与这里的互斥失效（apply_with 会优先锁这个记录值）。
    db.set("state_dir", &fsutil::path_string(state)?)?;
    initialization.commit()?;
    let job = Job {
        root: root.to_path_buf(),
        config,
        context,
        db,
        summary: Summary::default(),
    };
    Ok((guard, job))
}
/// 任务失败收尾（prepare 与 extract 同口径）：用户主动取消不是失败——任务库状态
/// 必须能区分「已取消」和「失败」，否则事后检查会误判；三个容错写库失败均不掩盖原错误。
pub fn record_task_failure(job: &mut Job, error: &anyhow::Error) {
    let cancelled = job.context.control.is_cancelled();
    tracing::info!(
        event = "task_state_changed",
        state = if cancelled { "cancelled" } else { "failed" },
        scanned = job.summary.scanned,
        bytes = job.summary.scanned_bytes,
        deleted = job.summary.deleted,
        moved = job.summary.moved,
        skipped = job.summary.skipped,
        permanent_bytes = job.summary.permanent_bytes,
        archives_ok = job.summary.archives_ok,
        archives_failed = job.summary.archives_failed,
        errors = job.summary.errors,
        "记录任务终态"
    );
    if job.db.set("summary", &job.summary).is_err() {
        tracing::warn!(
            event = "task_failure_record_failed",
            stage = "summary",
            error_type = "database",
            "任务失败汇总写入失败"
        );
    }
    if job
        .db
        .set("status", &if cancelled { "cancelled" } else { "failed" })
        .is_err()
    {
        tracing::warn!(
            event = "task_failure_record_failed",
            stage = "status",
            error_type = "database",
            "任务失败状态写入失败"
        );
    }
    if job
        .db
        .log(
            "任务",
            "",
            "",
            if cancelled { "已取消" } else { "失败" },
            &format!("{error:#}"),
            0,
        )
        .is_err()
    {
        tracing::warn!(
            event = "task_failure_record_failed",
            stage = "audit",
            error_type = "database",
            "任务失败审计写入失败"
        );
    }
}
pub fn enqueue_archive(job: &Job, archive: &Path, depth: u32) -> Result<()> {
    let snapshot = fsutil::snapshot(archive)?;
    let relative = fsutil::relative_string(&job.root, archive)?;
    let fingerprint = format!(
        "{}:{}:{}:{}",
        relative, snapshot.size, snapshot.modified_ns, snapshot.identity
    );
    // 同一路径的包可能已被扫描按旧内容入队、随后被其他包的成员覆盖：清掉未处理的旧行再入队。
    // 唯一键在 fingerprint 上，INSERT OR IGNORE 挡不住同 rel 的新内容；旧行残留会在其
    // 删除该包后让新行 snapshot 失败，把「解压失败 N 包」与错误计数虚高。
    job.db.conn.execute(
        "DELETE FROM archives WHERE rel=?1 AND state='pending'",
        params![relative],
    )?;
    job.db.conn.execute(
        "INSERT OR IGNORE INTO archives(rel,fingerprint,depth) VALUES(?1,?2,?3)",
        params![relative, fingerprint, depth],
    )?;
    Ok(())
}
/// 附录 A 大类名 + 「大文件」的固定容器集合（C-18 来源链识别用）。
pub fn is_category_label(name: &str) -> bool {
    matches!(
        name,
        "视频" | "音频" | "图片" | "文档" | "压缩包" | "程序" | "其他" | "大文件"
    )
}
/// S-01：固定容器占用保护与来源链识别复用同一套大类名及既有容器常量。
pub fn is_fixed_container_name(name: &str) -> bool {
    is_category_label(name)
        || name.eq_ignore_ascii_case(GIT_COLLECTION_DIR)
        || name.eq_ignore_ascii_case(QUARANTINE_DIR_NAME)
}
