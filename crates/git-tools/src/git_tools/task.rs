//! 任务主流程：逐文件 add→commit→验证→push、冲突续接与无变更收尾（G-03~G-16）。
use anyhow::{bail, Context as _, Result};
use std::{
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use jchtools_file_core::control::Control;

use super::command::{output_log_entry, output_text, run_git, run_git_ok, summarize};
use super::merge::complete_merge_commit;
use super::preflight::{
    check_change_structure, check_commit_identity, deletion_fully_staged, partial_stage_divergence,
    push_target_preflight, verify_single_path_commit,
};
use super::push::push_with_retry;
use super::shared::{Ctx, GitShared, StepOutcome};
use super::status::{inspect, middle_state, status_changes, FileChange, MiddleState, RepoInfo};

fn process_change(
    git: &Path,
    root: &Path,
    change: &FileChange,
    remote: &str,
    upstream_branch: &str,
    ctx: &Ctx<'_>,
) -> StepOutcome {
    let operation = jchtools_foundation::logging::operation_span("git", "file_change");
    let _entered = operation.enter();
    let started = std::time::Instant::now();
    tracing::debug!(event = "git_file_started", "开始处理一个 Git 文件变更");
    let result = process_change_inner(git, root, change, remote, upstream_branch, ctx);
    match &result {
        StepOutcome::Done => tracing::debug!(
            event = "git_file_completed",
            state = "success",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 文件变更已提交并推送"
        ),
        StepOutcome::Cancelled => tracing::info!(
            event = "git_file_cancelled",
            state = "cancelled",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 文件变更处理已停止"
        ),
        StepOutcome::Conflict(files) => tracing::error!(
            event = "git_file_failed",
            state = "conflict",
            conflict_count = files.len(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 文件变更处理因冲突停止"
        ),
        StepOutcome::Fatal(_) => tracing::error!(
            event = "git_file_failed",
            state = "failed",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 文件变更处理失败，详见关联阶段"
        ),
    }
    result
}

fn process_change_inner(
    git: &Path,
    root: &Path,
    change: &FileChange,
    remote: &str,
    upstream_branch: &str,
    ctx: &Ctx<'_>,
) -> StepOutcome {
    let stage_paths = change.stage_paths();
    let commit_paths = change.commit_paths();
    let display = change.display_path();
    let message = format!("update: {display}");
    // F11/G-06：处理该文件前检测部分暂存分叉——暂存区与工作区对该路径各有一份
    // 不同改动时，add 会用工作区内容覆盖用户已暂存的版本；该文件计为失败并停止
    // 自动处理（保护 staged 版本），不执行 add。
    match partial_stage_divergence(git, root, &commit_paths) {
        Ok(false) => {}
        Ok(true) => {
            tracing::error!(
                event = "git_file_preflight_failed",
                stage = "partial_stage",
                error_type = "staged_divergence",
                "文件存在部分暂存分叉，停止以保护用户暂存内容"
            );
            let text = format!(
                "{display} 处于部分暂存状态受保护：暂存区与工作区对该文件各有一份不同的改动，\
                 继续处理会覆盖用户已暂存的版本（G-06）；该文件计为失败，未执行 add/commit，\
                 请先自行统一该文件的暂存区与工作区状态后重新开始任务"
            );
            (ctx.log)(&text);
            return StepOutcome::Fatal(text);
        }
        Err(error) => {
            tracing::error!(
                event = "git_file_preflight_failed",
                stage = "partial_stage",
                error_type = "git_query_failed",
                "文件部分暂存检查失败"
            );
            let text = format!("{error:#}（当前文件：{display}，阶段：add 前预检）");
            (ctx.log)(&text);
            return StepOutcome::Fatal(text);
        }
    }
    let mut added = false;
    let mut attempt = 0u64;
    loop {
        let _attempt =
            tracing::info_span!("git_file_attempt", attempt = attempt.saturating_add(1)).entered();
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        if !added {
            if let Err(error) = check_change_structure(git, root, change, ctx.control) {
                if ctx.control.is_cancelled() {
                    return StepOutcome::Cancelled;
                }
                tracing::error!(
                    event = "git_file_preflight_failed",
                    stage = "change_structure",
                    error_type = "unsafe_or_uncommittable_change",
                    "文件结构不能安全逐文件提交，未写入索引"
                );
                let text = format!("{error:#}（当前文件：{display}，阶段：add 前预检）");
                (ctx.log)(&text);
                return StepOutcome::Fatal(text);
            }
        }
        if !added {
            ctx.shared.set_stage("add");
            let mut args: Vec<&str> = vec!["add", "--"];
            for path in &stage_paths {
                args.push(path.as_str());
            }
            (ctx.log)(&format!("git add -- {}", stage_paths.join(" ")));
            if ctx.control.is_cancelled() {
                return StepOutcome::Cancelled;
            }
            match run_git(git, root, &args) {
                Ok(out) if out.status.success() => {
                    // F14/G-14：成功也采集真实输出（add 通常无输出，空输出不记条目）
                    let stdout = output_text(&out.stdout);
                    let stderr = output_text(&out.stderr);
                    if let Some(entry) = output_log_entry(&stdout, &stderr) {
                        (ctx.log)(&format!("git add 输出：\n{entry}"));
                    }
                    added = true;
                }
                Ok(out) => {
                    let stdout = output_text(&out.stdout);
                    let stderr = output_text(&out.stderr);
                    if let Some(entry) = output_log_entry(&stdout, &stderr) {
                        (ctx.log)(&format!("git add 输出：\n{entry}"));
                    }
                    if ctx.control.is_cancelled() {
                        return StepOutcome::Cancelled;
                    }
                    let why = summarize(&stderr, &stdout);
                    (ctx.log)(&format!("git add 失败：{why}"));
                    // G-06：用户启动前已暂存的删除（git rm，或删除后 git add）在索引中
                    // 已无条目，`git add -- <path>` 必然报 pathspec 不匹配——这是确定性
                    // 状态而非可重试失败。核实这些路径的删除确已完全暂存（porcelain
                    // `D ` 形态）后跳过 add 直接提交（G-04：删除也是合法变更，
                    // `commit --only -- <path>` 对完全暂存删除可直接落提交）。
                    if deletion_fully_staged(git, root, &commit_paths) {
                        (ctx.log)("该删除已由用户预先暂存（索引已无条目），跳过 add 直接提交");
                        tracing::debug!(
                            event = "git_add_skipped",
                            reason = "deletion_already_staged",
                            "删除已完全暂存，跳过 add"
                        );
                        added = true;
                        continue;
                    }
                    if !ctx.wait_retry(&mut attempt) {
                        return StepOutcome::Cancelled;
                    }
                    continue;
                }
                Err(error) => {
                    (ctx.log)(&format!("git add 失败：{error:#}"));
                    if !ctx.wait_retry(&mut attempt) {
                        return StepOutcome::Cancelled;
                    }
                    continue;
                }
            }
        }
        {
            // F13/G-15：add 成功后进入 commit 前同样检查停止标志——停止检查不能只在
            // 循环顶：add 与 commit 同轮衔接时，add 执行期间发出的停止请求会漏过。
            if ctx.control.is_cancelled() {
                return StepOutcome::Cancelled;
            }
            ctx.shared.set_stage("commit");
            let mut args: Vec<&str> = vec!["commit", "--only", "-m", &message, "--"];
            for path in &commit_paths {
                args.push(path.as_str());
            }
            (ctx.log)(&format!(
                "git commit --only -m \"{message}\" -- {}",
                commit_paths.join(" ")
            ));
            if ctx.control.is_cancelled() {
                return StepOutcome::Cancelled;
            }
            match run_git(git, root, &args) {
                Ok(out) if out.status.success() => {
                    // F14/G-14：成功也采集真实输出（分支、提交号与文件统计）
                    let stdout = output_text(&out.stdout);
                    let stderr = output_text(&out.stderr);
                    if let Some(entry) = output_log_entry(&stdout, &stderr) {
                        (ctx.log)(&format!("git commit 输出：\n{entry}"));
                    }
                }
                Ok(out) => {
                    let stderr = output_text(&out.stderr);
                    let stdout = output_text(&out.stdout);
                    if let Some(entry) = output_log_entry(&stdout, &stderr) {
                        (ctx.log)(&format!("git commit 输出：\n{entry}"));
                    }
                    if ctx.control.is_cancelled() {
                        return StepOutcome::Cancelled;
                    }
                    // 用 Git 实际解析的身份判定配置错误，不依赖诊断语言。
                    if let Err(error) = check_commit_identity(git, root, ctx.control) {
                        tracing::error!(
                            event = "git_commit_rejected",
                            stage = "commit",
                            error_type = "identity_missing",
                            "Git 提交身份未配置，停止任务"
                        );
                        return if ctx.control.is_cancelled() {
                            StepOutcome::Cancelled
                        } else {
                            StepOutcome::Fatal(format!("{error:#}"))
                        };
                    }
                    if ctx.control.is_cancelled() {
                        return StepOutcome::Cancelled;
                    }
                    // 非零退出也可能发生在实际提交之后；按真实路径状态核实，
                    // 后续仍执行 G-05 验证，不能把“nothing to commit”翻译当作协议。
                    let mut check: Vec<&str> = vec!["status", "--porcelain=v1", "-z", "--"];
                    check.extend(commit_paths.iter().map(String::as_str));
                    match run_git_ok(git, root, &check) {
                        Ok(text) if text.is_empty() => {}
                        Ok(_) => {
                            (ctx.log)(&format!("git commit 失败：{}", summarize(&stderr, &stdout)));
                            if !ctx.wait_retry(&mut attempt) {
                                return StepOutcome::Cancelled;
                            }
                            continue;
                        }
                        Err(error) => {
                            return StepOutcome::Fatal(format!("核实提交状态失败：{error:#}"));
                        }
                    }
                }
                Err(error) => {
                    (ctx.log)(&format!("git commit 失败：{error:#}"));
                    if !ctx.wait_retry(&mut attempt) {
                        return StepOutcome::Cancelled;
                    }
                    continue;
                }
            }
            // F13/G-15：commit 自然结束后、验证命令启动前同样检查停止标志
            if ctx.control.is_cancelled() {
                return StepOutcome::Cancelled;
            }
            // G-05：验证本次 commit 只包含当前文件变更
            tracing::debug!(
                event = "git_stage_started",
                stage = "commit_verify",
                "开始验证 Git 单文件提交范围"
            );
            match verify_single_path_commit(git, root, &commit_paths) {
                Ok(true) => {}
                Ok(false) => return StepOutcome::Fatal("提交验证未通过".into()),
                Err(error) => {
                    return StepOutcome::Fatal(format!(
                        "{error:#}（当前文件：{display}，阶段：commit 验证）"
                    ));
                }
            }
        }
        let outcome = push_with_retry(git, root, remote, upstream_branch, ctx, &mut attempt);
        if matches!(outcome, StepOutcome::Done) {
            (ctx.log)(&format!("push 成功：{display}"));
        }
        return outcome;
    }
}

/// F12/G-12/G-16：统计 HEAD 与本地已知 upstream 引用的提交差集，返回
/// (upstream 领先数, 本地领先数)。不访问远端，因此不能据此确认实际远端一致。
/// porcelain 干净也不代表本地没有尚未推送的提交。
fn ahead_behind(git: &Path, root: &Path) -> Result<(u64, u64)> {
    let out = run_git(
        git,
        root,
        &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
    )
    .with_context(|| "执行 git rev-list 失败".to_string())?;
    if !out.status.success() {
        bail!(
            "git rev-list 失败：{}",
            summarize(&output_text(&out.stderr), &output_text(&out.stdout))
        );
    }
    let text = output_text(&out.stdout);
    let mut parts = text.split_whitespace();
    let (Some(behind), Some(ahead)) = (parts.next(), parts.next()) else {
        bail!("无法解析 git rev-list --count 输出：{text:?}");
    };
    let behind: u64 = behind
        .parse()
        .with_context(|| format!("无法解析远端领先数 {behind:?}"))?;
    let ahead: u64 = ahead
        .parse()
        .with_context(|| format!("无法解析本地领先数 {ahead:?}"))?;
    Ok((behind, ahead))
}

/// F12/G-12/G-16：工作区没有未提交变更时的收尾。只比较本地记录的 upstream 与 HEAD，
/// 不额外 fetch；零差集不证明实际远端一致。本地领先则进入只推送流程（G-09 阶段
/// 记忆——只重试 push、不重复 commit）；推送失败或分叉冲突时如实报告未推送状态。
fn finish_without_changes(git: &Path, info: &RepoInfo, ctx: &Ctx<'_>) -> String {
    let shared = ctx.shared;
    let started = std::time::Instant::now();
    let (behind, ahead) = match ahead_behind(git, &info.root) {
        Ok(pair) => pair,
        Err(error) => {
            tracing::error!(
                component = "git",
                event = "git_sync_status_failed",
                stage = "ahead_behind",
                error_type = "query_or_parse_failed",
                elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
                "Git 本地与 upstream 同步状态核实失败"
            );
            let text = format!("没有需要提交的变更，但无法核实与远端的同步状态：{error:#}");
            (ctx.log)(&text);
            shared.set_state("失败");
            return text;
        }
    };
    tracing::debug!(
        component = "git",
        event = "git_sync_status_completed",
        stage = "ahead_behind",
        behind,
        ahead,
        elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
        "Git 本地与 upstream 提交差异已统计"
    );
    if ahead == 0 {
        shared.set_state("完成");
        shared.set_stage("完成");
        if behind == 0 {
            return "没有需要提交的变更：HEAD 与本地已知 upstream 记录一致，未核实实际远端状态"
                .into();
        }
        return format!(
            "没有需要提交的变更：本地没有领先远端的提交（远端领先 {behind} 个提交，本工具不自动拉取）"
        );
    }
    (ctx.log)(&format!(
        "工作区没有未提交变更，但本地领先 upstream {ahead} 个提交（可能在 push 前停止）；按 G-09 阶段记忆只重试 push，不重复 commit"
    ));
    let mut attempt = 0u64;
    match push_with_retry(
        git,
        &info.root,
        &info.upstream_remote,
        &info.upstream_branch,
        ctx,
        &mut attempt,
    ) {
        StepOutcome::Done => {
            shared.set_state("完成");
            shared.set_stage("完成");
            let text = format!("没有需要提交的变更：已补推本地领先的 {ahead} 个提交");
            (ctx.log)(&text);
            text
        }
        StepOutcome::Cancelled => {
            shared.set_state("已停止");
            format!("任务已停止：本地有 {ahead} 个提交未推送（可再次启动任务补推）")
        }
        StepOutcome::Conflict(files) => {
            shared.set_state("冲突");
            shared.set_stage("冲突");
            let text = format!(
                "补推时自动合并出现冲突（{} 个文件：{}）；本地有 {ahead} 个提交未推送。已停止并保留冲突现场，请在仓库中解决后重新开始任务（G-11）",
                files.len(),
                files.join("、")
            );
            (ctx.log)(&text);
            text
        }
        StepOutcome::Fatal(reason) => {
            shared.set_state("失败");
            let text = format!("本地有 {ahead} 个提交未推送：{reason}");
            (ctx.log)(&text);
            text
        }
    }
}

/// 主入口：逐文件提交并推送（G-03~G-16）。返回收尾文案（状态与统计），
/// 详细过程经 `log`/`status` 回调实时上报（G-14）。
/// `repo` 可位于仓库子目录；验证后统一从仓库根执行，合并暂存区保护覆盖整个仓库。
/// `unit` 为退避基准（生产用 [`BACKOFF_UNIT`]，测试注入小值）。
pub fn run(
    git: &Path,
    repo: &Path,
    control: &Control,
    shared: &Arc<GitShared>,
    log: &dyn Fn(&str),
    status: &dyn Fn(&str),
    unit: Duration,
) -> String {
    let operation = jchtools_foundation::logging::operation_span("git", "task");
    let _entered = operation.enter();
    tracing::info!(event = "git_task_started", "Git 任务开始");
    let started = std::time::Instant::now();
    let result = run_task(git, repo, control, shared, log, status, unit);
    let state = shared.state();
    let total = shared.total.load(Ordering::Relaxed);
    let done = shared.done.load(Ordering::Relaxed);
    let last_retry_attempt = shared.retry.load(Ordering::Relaxed);
    let elapsed_ms = jchtools_foundation::logging::elapsed_ms(started);
    match state.as_str() {
        "完成" => tracing::info!(
            event = "git_task_completed",
            state = "success",
            total,
            done,
            last_retry_attempt,
            elapsed_ms,
            "Git 任务完成"
        ),
        "已停止" => tracing::info!(
            event = "git_task_cancelled",
            state = "cancelled",
            total,
            done,
            last_retry_attempt,
            elapsed_ms,
            "Git 任务已停止"
        ),
        "冲突" => tracing::error!(
            event = "git_task_failed",
            state = "conflict",
            total,
            done,
            last_retry_attempt,
            elapsed_ms,
            "Git 任务因合并冲突停止"
        ),
        _ => tracing::error!(
            event = "git_task_failed",
            state = "failed",
            total,
            done,
            last_retry_attempt,
            elapsed_ms,
            "Git 任务失败，详见关联阶段"
        ),
    }
    result
}

fn run_task(
    git: &Path,
    repo: &Path,
    control: &Control,
    shared: &Arc<GitShared>,
    log: &dyn Fn(&str),
    status: &dyn Fn(&str),
    unit: Duration,
) -> String {
    shared.set_state("检查仓库");
    shared.set_stage("扫描");
    status("正在验证仓库（分支、upstream 与仓库状态）");
    let info = match inspect(git, repo) {
        Ok(info) => info,
        Err(error) => {
            tracing::error!(
                event = "git_preflight_failed",
                stage = "inspect",
                error_type = "repository_invalid",
                "Git 仓库检查失败"
            );
            let text = format!("{error:#}");
            log(&text);
            shared.set_state("失败");
            return format!("无法开始：{text}");
        }
    };
    // G-06：后续命令统一从仓库根执行，避免 diff.relative 将合并范围检查
    // 限制在用户选定的子目录，却由 commit --no-edit 提交整个暂存区。
    let repo = info.root.as_path();
    GitShared::set_text(&shared.repo, &info.root.display().to_string());
    GitShared::set_text(&shared.branch, &info.branch);
    GitShared::set_text(&shared.upstream, &info.upstream);
    log(&format!(
        "仓库：{}\n分支：{}\nupstream：{}",
        info.root.display(),
        info.branch,
        info.upstream
    ));
    // F10/P-03/G-08：任务启动预检——在任何写命令之前核实裸 git push 的实际推送
    // 目标确实是 upstream 远端，且没有会改变推送对象/范围的用户配置（只读检查）。
    tracing::debug!(
        event = "git_stage_started",
        stage = "push_target_preflight",
        "开始验证 Git 推送目标策略"
    );
    if let Err(error) = push_target_preflight(git, repo, &info) {
        tracing::error!(
            event = "git_preflight_failed",
            stage = "push_target",
            error_type = "target_policy",
            "Git 推送目标预检失败"
        );
        let text = format!("{error:#}");
        log(&text);
        shared.set_state("失败");
        return format!("无法开始：{text}");
    }
    tracing::debug!(
        event = "git_stage_completed",
        stage = "push_target_preflight",
        result = "allowed",
        "Git 推送目标策略验证通过"
    );
    // 中间态处理（G-06/G-11）
    tracing::debug!(
        event = "git_stage_started",
        stage = "middle_state",
        "开始检查 Git 仓库中间态"
    );
    match middle_state(git, repo) {
        Ok(MiddleState::Clean) => {}
        Ok(MiddleState::Merge { unresolved }) => {
            if unresolved.is_empty() {
                // F13/G-15：完成合并提交也是一条会改仓库状态的 git 命令，启动前检查
                // 停止标志——停止后不得再启动任何 git 命令。
                if control.is_cancelled() {
                    shared.set_state("已停止");
                    return "任务已停止：合并已解决但尚未完成合并提交（可重新开始任务继续）".into();
                }
                // 用户已在冲突后解决并暂存：完成合并提交后继续（G-11 续段）。
                // 合并提交必须推送成功才算完成当前任务，然后继续扫描剩余变更。
                let ctx = Ctx {
                    control,
                    shared,
                    log,
                    unit,
                };
                let mut attempt = 0u64;
                match complete_merge_commit(git, repo, &ctx, &mut attempt) {
                    StepOutcome::Done => {}
                    StepOutcome::Cancelled => {
                        shared.set_state("已停止");
                        return "任务已停止：已保留合并现场与本地提交，下次按 Git 实际状态继续"
                            .into();
                    }
                    StepOutcome::Conflict(files) => {
                        shared.set_state("冲突");
                        shared.set_stage("冲突");
                        let text = format!(
                            "完成合并提交时发现未解决冲突（{} 个文件：{}）；已停止并保留现场（G-11）",
                            files.len(),
                            files.join("、")
                        );
                        log(&text);
                        return text;
                    }
                    StepOutcome::Fatal(reason) => {
                        shared.set_state("失败");
                        log(&reason);
                        return reason;
                    }
                }
                match push_with_retry(
                    git,
                    repo,
                    &info.upstream_remote,
                    &info.upstream_branch,
                    &ctx,
                    &mut attempt,
                ) {
                    StepOutcome::Done => {
                        log("合并提交已 push 成功；继续扫描剩余变更");
                    }
                    StepOutcome::Cancelled => {
                        shared.set_state("已停止");
                        return "任务已停止：合并提交已在本地完成，尚未推送".into();
                    }
                    StepOutcome::Conflict(files) => {
                        shared.set_state("冲突");
                        shared.set_stage("冲突");
                        let text = format!(
                            "合并提交的推送再次遇到冲突（{} 个文件：{}）；请解决后重新开始任务（G-11）",
                            files.len(),
                            files.join("、")
                        );
                        log(&text);
                        return text;
                    }
                    StepOutcome::Fatal(reason) => {
                        shared.set_state("失败");
                        let text = format!("合并提交推送失败：{reason}");
                        log(&text);
                        return text;
                    }
                }
            } else {
                shared.set_state("冲突");
                shared.set_stage("冲突");
                let text = format!(
                    "仓库存在未解决的合并冲突（{} 个文件：{}）；请在仓库中解决冲突后重新开始任务，已成功推送的文件不会被重复提交（G-11/G-12）",
                    unresolved.len(),
                    unresolved.join("、")
                );
                log(&text);
                return text;
            }
        }
        Ok(MiddleState::Other(kind)) => {
            tracing::error!(
                event = "git_preflight_failed",
                stage = "middle_state",
                error_type = "unfinished_operation",
                operation_kind = kind,
                "仓库处于未完成操作，不能安全逐文件提交"
            );
            let text = format!(
                "仓库处于未完成的 {kind}，无法安全保证单文件提交；请先手动完成或中止（G-06）"
            );
            log(&text);
            shared.set_state("失败");
            return text;
        }
        Err(error) => {
            tracing::error!(
                event = "git_preflight_failed",
                stage = "middle_state",
                error_type = "git_query_failed",
                "Git 仓库中间态检查失败"
            );
            let text = format!("检查仓库状态失败：{error:#}");
            log(&text);
            shared.set_state("失败");
            return text;
        }
    }
    tracing::debug!(
        event = "git_stage_completed",
        stage = "middle_state",
        result = "ready",
        "Git 仓库中间态检查及必要续接完成"
    );
    if control.is_cancelled() {
        shared.set_state("已停止");
        return "任务已停止：尚未开始处理文件".into();
    }
    // 扫描（G-12）
    shared.set_stage("扫描");
    status("正在扫描未提交变更（以 git 状态为准）");
    let changes = match status_changes(git, &info.root) {
        Ok(changes) => changes,
        Err(error) => {
            tracing::error!(
                event = "git_scan_failed",
                stage = "status_scan",
                error_type = "status_query_or_parse",
                "Git 未提交变更扫描失败"
            );
            let text = format!("{error:#}");
            log(&text);
            shared.set_state("失败");
            return format!("扫描失败：{text}");
        }
    };
    let total = changes.len() as u64;
    tracing::debug!(
        event = "git_scan_completed",
        change_count = total,
        "Git 未提交变更扫描完成"
    );
    shared.total.store(total, Ordering::Relaxed);
    shared.done.store(0, Ordering::Relaxed);
    log(&format!("待处理变更：{total} 个逻辑文件变更"));
    let ctx = Ctx {
        control,
        shared,
        log,
        unit,
    };
    if changes.is_empty() {
        return finish_without_changes(git, &info, &ctx);
    }
    let mut done = 0u64;
    for change in &changes {
        if control.is_cancelled() {
            break;
        }
        shared.set_current(&change.display_path());
        log(&format!("开始处理：{}", change.display_path()));
        match process_change(
            git,
            &info.root,
            change,
            &info.upstream_remote,
            &info.upstream_branch,
            &ctx,
        ) {
            StepOutcome::Done => {
                done += 1;
                shared.done.store(done, Ordering::Relaxed);
                status(&format!(
                    "已完成 {done} / {total}：{}",
                    change.display_path()
                ));
            }
            StepOutcome::Cancelled => break,
            StepOutcome::Conflict(files) => {
                shared.set_state("冲突");
                shared.set_stage("冲突");
                let text = format!(
                    "自动合并出现冲突（当前文件：{}，阶段：merge）：冲突文件 {} 个（{}）。已停止自动处理并保留冲突现场；请在仓库中解决冲突后重新开始任务继续（G-11）",
                    change.display_path(),
                    files.len(),
                    files.join("、")
                );
                log(&text);
                return text;
            }
            StepOutcome::Fatal(reason) => {
                shared.set_state("失败");
                let text = format!(
                    "任务停止：{reason}（已完成 {done} / {total}；成功推送的文件保持成功）"
                );
                log(&text);
                return text;
            }
        }
    }
    if control.is_cancelled() {
        shared.set_state("已停止");
        return format!(
            "任务已停止：已完成 {done} / {total}；已成功 commit + push 的文件保持成功状态（G-15）"
        );
    }
    shared.set_state("完成");
    shared.set_stage("完成");
    let text = format!("全部完成：{done} / {total} 个文件已逐个 commit 并 push 成功");
    log(&text);
    text
}
