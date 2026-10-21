use anyhow::Result;
use jchtools_file_core::{
    config::DeleteMode,
    fsutil,
    platform::{self, DeleteResult},
    rules,
    scan::{special_prefixes, ScopeFilter},
    task::{observe, Job},
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

/// 整理收尾的实空清理（H-05/C-07）：自底向上删除实际为空的目录及空目录链。
/// 「总是执行」：没有配置开关可关闭，也不受文件清理/删除方式选择影响——空目录清理
/// 不属于 C-08 的六类清理项，H-05 不提供关闭这一步的选项。只尊重：选定根目录、
/// Git 整树排除（H-06）、递归范围、用户排除与隐藏/系统开关、状态目录/程序目录/
/// 工具工作目录、Windows 系统目录子树（S-05），以及取消；一律永久删除（S-02）。
/// 与扫描共用同一套范围口径（[`ScopeFilter`]），所以「解压失败」暂存区内实际为空的
/// 目录也按 H-05 清理（C-09），但其内容一律不碰——判定只看目录项是否为空。
pub(super) fn final_empty_cleanup(job: &mut Job, protected: Option<&Path>) -> Result<()> {
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
