//! push 被拒后的 fetch + merge 自动合并、合并阻挡检查与合并提交续接（G-10/G-11）。
use anyhow::{bail, Context as _, Result};
use std::path::Path;

use jchtools_file_core::control::Control;

use super::command::{
    bounded_text, config_get_bool, output_log_entry, output_text, run_git, run_git_network,
    run_git_ok, summarize,
};
use super::preflight::{check_commit_identity, check_merge_staging};
use super::shared::{Ctx, StepOutcome};
use super::status::{
    git_dir, middle_state, status_changes, unresolved_paths, FileChange, MiddleState,
};

/// fetch + merge 的结果分类（G-10）。
pub(super) enum MergeOutcome {
    Merged,
    /// 自动合并冲突：停止并保留现场（G-11）。
    Conflict(Vec<String>),
    /// 被本地未提交变更阻挡（G-10 尾段：停止并显示原因）。
    BlockedByDirty(String),
    /// 确定性失败或无法安全检查现场：停止而非重复修改仓库。
    Fatal(String),
    Retryable(String),
    /// 用户已停止（F13/G-15）：当前命令自然结束，不再启动后续命令。
    Cancelled,
}

/// fetch + merge 当前 upstream（G-10）。远端与分支来自 `branch.<b>.remote` /
/// `branch.<b>.merge` 结构化配置（F09：不从 upstream 显示串拆分——含斜杠分支与
/// 本地 upstream（remote=="."）都会拆错）。fetch 与 merge 是两个独立阶段（G-13），
/// 每条命令启动前检查停止标志（F13/G-15），成功与失败都把真实输出写入日志
/// （F14/G-14）。
pub(super) fn fetch_and_merge(
    git: &Path,
    root: &Path,
    remote: &str,
    upstream_branch: &str,
    ctx: &Ctx<'_>,
    fetched: &mut bool,
) -> MergeOutcome {
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    if !*fetched {
        ctx.shared.set_stage("pull/fetch");
        (ctx.log)(&format!(
            "git fetch --no-recurse-submodules {remote} {upstream_branch}"
        ));
        let fetch = match run_git_network(
            git,
            root,
            &["fetch", "--no-recurse-submodules", remote, upstream_branch],
            ctx.control,
        ) {
            Ok(out) => out,
            Err(error) => return MergeOutcome::Retryable(format!("{error:#}")),
        };
        let fetch_stdout = output_text(&fetch.stdout);
        let fetch_stderr = output_text(&fetch.stderr);
        // G-14：失败也保留实际的两条输出流，错误摘要不能替代阶段输出。
        if let Some(entry) = output_log_entry(&fetch_stdout, &fetch_stderr) {
            (ctx.log)(&format!("git fetch 输出：\n{entry}"));
        }
        if !fetch.status.success() {
            let reason = summarize(&fetch_stderr, &fetch_stdout);
            (ctx.log)(&format!("git fetch 失败：{reason}"));
            return MergeOutcome::Retryable(reason);
        }
        // G-09：merge 可重试失败时不得重新执行已经成功的 fetch。
        *fetched = true;
    }
    // F13/G-15：fetch 自然结束后检查停止标志，不再启动 merge
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    // 隐藏索引标记会让 status 漏掉内容，失败后的 Git 回滚也未必保留原始字节。
    // 先只核对远端实际影响的隐藏路径；普通 dirty 仍交给真实 merge 判断是否安全。
    let incoming = match incoming_merge_paths(git, root, ctx.control) {
        Ok(paths) => paths,
        Err(_) if ctx.control.is_cancelled() => return MergeOutcome::Cancelled,
        Err(error) => return MergeOutcome::Fatal(format!("检查远端合并路径失败：{error:#}")),
    };
    match merge_has_hidden_dirty_blocker(git, root, &incoming, ctx.control) {
        Ok(true) => {
            return MergeOutcome::BlockedByDirty(
                "远端将修改的隐藏索引路径存在本地内容变更；为保留原始内容，未启动合并".to_owned(),
            );
        }
        Ok(false) => {}
        Err(_) if ctx.control.is_cancelled() => return MergeOutcome::Cancelled,
        Err(error) => return MergeOutcome::Fatal(format!("检查隐藏合并路径失败：{error:#}")),
    }
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    ctx.shared.set_stage("merge");
    // G-10/G-11：合并策略由用户 Git 配置控制，Git 报告冲突时保留现场。
    (ctx.log)("git merge FETCH_HEAD");
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    let out = match run_git(git, root, &["merge", "FETCH_HEAD"]) {
        Ok(out) => out,
        Err(error) => return MergeOutcome::Retryable(format!("{error:#}")),
    };
    let stdout = output_text(&out.stdout);
    let stderr = output_text(&out.stderr);
    if let Some(entry) = output_log_entry(&stdout, &stderr) {
        (ctx.log)(&format!("git merge 输出：\n{entry}"));
    }
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    let text = format!("{stderr}{stdout}");
    // 先读真实未合并索引，不依赖会本地化的人类诊断。
    let unresolved = match unresolved_paths(git, root) {
        Ok(paths) => paths,
        Err(error) => return MergeOutcome::Fatal(format!("检查合并冲突失败：{error:#}")),
    };
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    if !unresolved.is_empty() {
        tracing::warn!(
            event = "git_merge_classified",
            result = "conflict",
            "Git 自动合并出现冲突，保留现场"
        );
        (ctx.log)(&format!("git merge 冲突输出：\n{}", bounded_text(&text)));
        return MergeOutcome::Conflict(unresolved);
    }
    // autostash 的恢复可能留下冲突而 merge 仍退出 0；索引确认无冲突后才允许 push。
    if out.status.success() {
        tracing::debug!(
            event = "git_merge_classified",
            result = "merged",
            "Git 自动合并成功，索引无未解决条目"
        );
        return MergeOutcome::Merged;
    }
    match merge_has_dirty_blocker(git, root, &incoming, ctx.control) {
        Ok(true) => return MergeOutcome::BlockedByDirty(summarize(&stderr, &stdout)),
        Ok(false) => {}
        Err(_) if ctx.control.is_cancelled() => return MergeOutcome::Cancelled,
        Err(error) => return MergeOutcome::Fatal(format!("检查合并阻挡状态失败：{error:#}")),
    }
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    if let Err(error) = check_commit_identity(git, root, ctx.control) {
        return if ctx.control.is_cancelled() {
            MergeOutcome::Cancelled
        } else {
            MergeOutcome::Fatal(format!("{error:#}"))
        };
    }
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    let dir = match git_dir(git, root) {
        Ok(dir) => dir,
        Err(error) => return MergeOutcome::Fatal(format!("检查合并现场失败：{error:#}")),
    };
    if dir.join("MERGE_HEAD").exists() {
        return MergeOutcome::Fatal(format!(
            "自动合并未完成，已保留仓库中间态；请处理后重新开始：{}",
            summarize(&stderr, &stdout)
        ));
    }
    if out.status.code() == Some(129) || out.status.code().is_none() {
        return MergeOutcome::Fatal(summarize(&stderr, &stdout));
    }
    MergeOutcome::Retryable(summarize(&stderr, &stdout))
}

/// 只保留远端相对共同祖先有改动、且两端 blob/mode 尚不一致的路径。
/// 两端已提交相同结果时，merge 无需更新该路径，不能阻挡其隐藏工作区修改。
fn incoming_merge_paths(git: &Path, root: &Path, control: &Control) -> Result<String> {
    control.check_cancelled()?;
    let incoming = run_git_ok(
        git,
        root,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            "HEAD...FETCH_HEAD",
            "--",
        ],
    )?;
    control.check_cancelled()?;
    if incoming.is_empty() {
        return Ok(incoming);
    }
    let differing = run_git_ok(
        git,
        root,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            "HEAD",
            "FETCH_HEAD",
            "--",
        ],
    )?;
    control.check_cancelled()?;
    let differing: std::collections::HashSet<_> = differing
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect();
    let mut affected = String::with_capacity(incoming.len());
    for path in incoming.split('\0').filter(|path| !path.is_empty()) {
        if differing.contains(path) {
            affected.push_str(path);
            affected.push('\0');
        }
    }
    Ok(affected)
}

/// 普通 dirty 仅在非零 merge 后分类，允许不受远端影响的本地内容安全合并。
fn merge_has_dirty_blocker(
    git: &Path,
    root: &Path,
    incoming: &str,
    control: &Control,
) -> Result<bool> {
    control.check_cancelled()?;
    let overlaps = |path: &str| {
        incoming
            .split('\0')
            .filter(|other| !other.is_empty())
            .any(|other| {
                path == other
                    || path
                        .strip_prefix(other)
                        .is_some_and(|rest| rest.starts_with('/'))
                    || other
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
    };
    let autostash = config_get_bool(git, root, "merge.autoStash") == Some(true);
    control.check_cancelled()?;
    if autostash {
        // 自动暂存可承接已跟踪内容；未跟踪文件不在其中，仍须保留真正的路径阻挡。
        let untracked = run_git_ok(
            git,
            root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        control.check_cancelled()?;
        return Ok(untracked
            .split('\0')
            .filter(|path| !path.is_empty())
            .any(overlaps));
    }
    let changes = status_changes(git, root)?;
    control.check_cancelled()?;
    let staged = run_git_ok(git, root, &["diff", "--cached", "--name-only", "-z"])?;
    control.check_cancelled()?;
    if !staged.is_empty() {
        return Ok(true);
    }
    Ok(changes.iter().any(|change| {
        overlaps(change.path()) || matches!(change, FileChange::Renamed(old, _) if overlaps(old))
    }))
}

/// merge 前只读核对远端影响范围内的隐藏内容，不改变标记或逐文件提交清单。
fn merge_has_hidden_dirty_blocker(
    git: &Path,
    root: &Path,
    incoming: &str,
    control: &Control,
) -> Result<bool> {
    control.check_cancelled()?;
    if incoming.is_empty() {
        return Ok(false);
    }

    // status 会跳过 assume-unchanged / skip-worktree。只读取远端涉及路径的
    // 索引条目，且只对带隐藏标记、实际存在的候选计算 blob，避免整仓扫描内容。
    // 按最坏引号转义长度限制每批路径，为 Git 固定参数和 Windows .cmd 转发
    // 预留空间；不能让新增的保护检查使大量远端路径突破命令行长度限制。
    const PATH_ARGUMENT_BUDGET: usize = 4096;
    let mut paths = incoming
        .split('\0')
        .filter(|path| !path.is_empty())
        .peekable();
    let mut args = vec!["ls-files", "-v", "--stage", "-z", "--"];
    while paths.peek().is_some() {
        args.truncate(5);
        let mut argument_units = 0;
        while let Some(&path) = paths.peek() {
            let units = path
                .encode_utf16()
                .count()
                .saturating_mul(2)
                .saturating_add(3);
            if args.len() > 5 && argument_units + units > PATH_ARGUMENT_BUDGET {
                break;
            }
            args.push(path);
            argument_units += units;
            paths.next();
        }
        control.check_cancelled()?;
        let entries = run_git_ok(git, root, &args)?;
        control.check_cancelled()?;
        if hidden_index_entries_have_dirty_content(git, root, &entries, control)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn hidden_index_entries_have_dirty_content(
    git: &Path,
    root: &Path,
    entries: &str,
    control: &Control,
) -> Result<bool> {
    for entry in entries.split('\0').filter(|entry| !entry.is_empty()) {
        let Some((header, path)) = entry.split_once('\t') else {
            bail!("解析隐藏索引条目失败：缺少路径分隔符");
        };
        let mut fields = header.split(' ');
        let (Some(flag), Some(mode), Some(oid), Some("0"), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            bail!("解析隐藏索引条目失败：元数据不完整或存在未合并条目");
        };
        let [flag] = flag.as_bytes() else {
            bail!("解析隐藏索引条目失败：索引标记无效");
        };
        if !flag.is_ascii_lowercase() && *flag != b'S' {
            continue;
        }
        control.check_cancelled()?;
        let metadata = match std::fs::symlink_metadata(root.join(path)) {
            Ok(metadata) => metadata,
            // 稀疏检出中 skip-worktree 路径不存在是正常状态；merge 可以安全写入，
            // 不能把不存在的文件当作隐藏的本地内容。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("读取隐藏索引路径的工作区状态失败"),
        };
        if mode == "120000" && metadata.file_type().is_symlink() {
            let target =
                std::fs::read_link(root.join(path)).context("读取隐藏符号链接的目标失败")?;
            let target = target
                .to_str()
                .context("隐藏符号链接的目标不是有效 UTF-8，无法安全核对")?;
            control.check_cancelled()?;
            let expected = run_git_ok(git, root, &["cat-file", "blob", oid])?;
            control.check_cancelled()?;
            if target != expected {
                return Ok(true);
            }
            continue;
        }
        if !matches!(mode, "100644" | "100755" | "120000") {
            bail!("无法安全核对隐藏索引条目的文件类型（mode={mode}）；已保留现场");
        }
        if !metadata.is_file() {
            return Ok(true);
        }
        control.check_cancelled()?;
        let hash = if mode == "120000" {
            // core.symlinks=false 的检出用普通文件保存链接目标，不应用文本过滤器。
            let emulated_link = config_get_bool(git, root, "core.symlinks") == Some(false);
            control.check_cancelled()?;
            if !emulated_link {
                return Ok(true);
            }
            run_git_ok(git, root, &["hash-object", "--no-filters", "--", path])?
        } else {
            // --path 由 Git 应用路径的 clean filter / EOL 规则，不清除索引标记。
            run_git_ok(
                git,
                root,
                &["hash-object", &format!("--path={path}"), "--", path],
            )?
        };
        control.check_cancelled()?;
        if hash.trim() != oid {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 已解决的合并只重试提交阶段；成功后交给调用者补推，不能重新 add 或 merge。
pub(super) fn complete_merge_commit(
    git: &Path,
    root: &Path,
    ctx: &Ctx<'_>,
    attempt: &mut u64,
) -> StepOutcome {
    if ctx.control.is_cancelled() {
        return StepOutcome::Cancelled;
    }
    let parents = match run_git_ok(git, root, &["rev-parse", "HEAD", "MERGE_HEAD"]) {
        Ok(text) => text,
        Err(error) => return StepOutcome::Fatal(format!("读取合并父提交失败：{error:#}")),
    };
    loop {
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        if let Err(error) = check_merge_staging(git, root, ctx.control) {
            return if ctx.control.is_cancelled() {
                StepOutcome::Cancelled
            } else {
                StepOutcome::Fatal(format!("{error:#}"))
            };
        }
        ctx.shared.set_stage("merge");
        (ctx.log)("检测到已解决的合并：完成合并提交（git commit --no-edit）");
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        let reason = match run_git(git, root, &["commit", "--no-edit"]) {
            Ok(out) => {
                let stdout = output_text(&out.stdout);
                let stderr = output_text(&out.stderr);
                if let Some(entry) = output_log_entry(&stdout, &stderr) {
                    (ctx.log)(&format!("git commit --no-edit 输出：\n{entry}"));
                }
                if out.status.success() {
                    return StepOutcome::Done;
                }
                summarize(&stderr, &stdout)
            }
            Err(error) => format!("{error:#}"),
        };
        (ctx.log)(&format!("完成合并提交失败：{reason}"));
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        // 不依据 hook/签名诊断的语言分类。先核对真实现场，避免已经完成提交后
        // 因非零退出再次 commit；其他中间态或新冲突则停止并保留内容。
        let state = middle_state(git, root);
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        match state {
            Ok(MiddleState::Clean) => {
                let tip = match run_git_ok(git, root, &["rev-list", "--parents", "-n", "1", "HEAD"])
                {
                    Ok(text) => text,
                    Err(error) => {
                        return StepOutcome::Fatal(format!("核实合并提交失败：{error:#}"));
                    }
                };
                if tip
                    .split_whitespace()
                    .skip(1)
                    .eq(parents.split_whitespace())
                {
                    return StepOutcome::Done;
                }
                return StepOutcome::Fatal(format!(
                    "合并现场已改变，但 HEAD 不是预期的合并提交；已停止并保留现场：{reason}"
                ));
            }
            Ok(MiddleState::Merge { unresolved }) if !unresolved.is_empty() => {
                return StepOutcome::Conflict(unresolved);
            }
            Ok(MiddleState::Merge { .. }) => {}
            Ok(MiddleState::Other(kind)) => {
                return StepOutcome::Fatal(format!(
                    "合并提交后出现未完成的 {kind}，已停止：{reason}"
                ));
            }
            Err(error) => {
                return StepOutcome::Fatal(format!(
                    "核实合并现场失败：{error:#}；提交失败：{reason}"
                ));
            }
        }
        if let Err(error) = check_commit_identity(git, root, ctx.control) {
            return if ctx.control.is_cancelled() {
                StepOutcome::Cancelled
            } else {
                StepOutcome::Fatal(format!("{error:#}"))
            };
        }
        tracing::warn!(
            event = "git_retryable_failure",
            stage = "merge_commit",
            error_type = "commit_failed",
            failed_attempt = *attempt + 1,
            "已解决合并的提交失败，保留当前阶段退避重试"
        );
        if !ctx.wait_retry(attempt) {
            return StepOutcome::Cancelled;
        }
    }
}
