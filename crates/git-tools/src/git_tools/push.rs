//! push 尝试分类与 NeedMerge 后的退避重试（G-08~G-11）。
use std::path::Path;

use jchtools_file_core::control::Control;

use super::command::{output_log_entry, output_text, run_git_network, summarize};
use super::merge::{fetch_and_merge, MergeOutcome};
use super::shared::{Ctx, StepOutcome};

/// push 的结果分类。
enum PushOutcome {
    Ok,
    /// 远端有新提交（non-fast-forward / fetch first / stale info），需要 fetch + merge。
    NeedMerge,
    /// 可重试失败（网络、认证、超时等）。
    Retryable(String),
}

/// 一次 push 尝试（G-08：不带额外远程/分支参数，用已配置 upstream）。
fn try_push(
    git: &Path,
    root: &Path,
    remote: &str,
    log: &dyn Fn(&str),
    control: &Control,
) -> PushOutcome {
    // P-03：子模块有各自的远端，不能借递归推送突破当前upstream边界。
    log("git push --porcelain --no-recurse-submodules");
    let out = match run_git_network(
        git,
        root,
        remote,
        &["push", "--porcelain", "--no-recurse-submodules"],
        control,
    ) {
        Ok(out) => out,
        Err(error) => return PushOutcome::Retryable(format!("{error:#}")),
    };
    let stdout = output_text(&out.stdout);
    let stderr = output_text(&out.stderr);
    // G-14：每次尝试都展示两条真实输出流，包括失败与转入合并的拒绝结果；
    // 后续错误摘要只用于说明原因，不能代替完整的阶段输出。
    if let Some(entry) = output_log_entry(&stdout, &stderr) {
        log(&format!("git push 输出：\n{entry}"));
    }
    if out.status.success() && !stdout.lines().any(|line| line.starts_with('!')) {
        tracing::debug!(
            event = "git_push_classified",
            result = "success",
            "Git 推送结果为成功"
        );
        return PushOutcome::Ok;
    }
    let text = format!("{stderr}{stdout}");
    if text.contains("fetch first")
        || text.contains("non-fast-forward")
        || text.contains("stale info")
    {
        tracing::warn!(
            event = "git_push_classified",
            result = "need_merge",
            "Git 推送被远端新提交拒绝，转入自动合并"
        );
        return PushOutcome::NeedMerge;
    }
    tracing::warn!(
        event = "git_push_classified",
        result = "retryable",
        "Git 推送失败，允许退避重试"
    );
    PushOutcome::Retryable(summarize(&stderr, &stdout))
}

/// push 当前分支（G-08~G-11）：NeedMerge 时自动 fetch+merge 后重试 push（不 rebase），
/// 可重试失败按退避无限重试；供 process_change 的单文件流程、冲突续接的合并提交与
/// 只推送补推流程（F12）共用。
pub(super) fn push_with_retry(
    git: &Path,
    root: &Path,
    remote: &str,
    upstream_branch: &str,
    ctx: &Ctx<'_>,
    attempt: &mut u64,
) -> StepOutcome {
    loop {
        let _attempt =
            tracing::info_span!("git_push_attempt", attempt = attempt.saturating_add(1)).entered();
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        ctx.shared.set_stage("push");
        match try_push(git, root, remote, ctx.log, ctx.control) {
            PushOutcome::Ok => return StepOutcome::Done,
            PushOutcome::NeedMerge => {
                let mut fetched = false;
                loop {
                    match fetch_and_merge(git, root, remote, upstream_branch, ctx, &mut fetched) {
                        // G-09/G-10：fetch/merge 当前阶段成功后才回到 push。
                        MergeOutcome::Merged => break,
                        MergeOutcome::Cancelled => return StepOutcome::Cancelled,
                        MergeOutcome::Conflict(files) => return StepOutcome::Conflict(files),
                        MergeOutcome::BlockedByDirty(reason) => {
                            return StepOutcome::Fatal(format!(
                                "合并被本地未提交变更阻挡：{reason}（阶段：merge）"
                            ));
                        }
                        MergeOutcome::Fatal(reason) => return StepOutcome::Fatal(reason),
                        MergeOutcome::Retryable(reason) => {
                            tracing::warn!(
                                event = "git_retryable_failure",
                                stage = if fetched { "merge" } else { "fetch" },
                                error_type = "fetch_merge_failed",
                                failed_attempt = *attempt + 1,
                                "Git fetch/merge 失败，进入退避重试"
                            );
                            (ctx.log)(&format!("fetch/merge 失败：{reason}"));
                            if !ctx.wait_retry(attempt) {
                                return StepOutcome::Cancelled;
                            }
                        }
                    }
                }
            }
            PushOutcome::Retryable(reason) => {
                tracing::warn!(
                    event = "git_retryable_failure",
                    stage = "push",
                    error_type = "push_failed",
                    failed_attempt = *attempt + 1,
                    "Git push 失败，进入退避重试"
                );
                (ctx.log)(&format!("push 失败：{reason}"));
                if !ctx.wait_retry(attempt) {
                    return StepOutcome::Cancelled;
                }
            }
        }
    }
}
