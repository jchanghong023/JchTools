//! 任务启动预检、写索引前的安全检查与单文件提交验证（F10/F11/G-05/G-06/G-08）。
use anyhow::{bail, Context as _, Result};
use std::path::Path;

use jchtools_file_core::control::Control;

use super::command::{
    config_get_bool, config_get_opt, output_text, run_git, run_git_ok, summarize,
};
use super::status::{FileChange, RepoInfo};

/// F10/P-03/G-08：任务启动前只读预检裸 `git push` 的目标。除 remote 及 refspec
/// 选择外，还核实最终 push URL 与 upstream fetch URL 一致：`remote.<r>.url` /
/// `pushurl` 可重复，`insteadOf` / `pushInsteadOf` 会改写实际地址，必须按 Git
/// 展开的全部 URL 判断，避免访问 upstream 之外的地址。
pub(super) fn push_target_preflight(git: &Path, root: &Path, info: &RepoInfo) -> Result<()> {
    let branch = &info.branch;
    let upstream_remote = info.upstream_remote.trim();
    let push_remote = config_get_opt(git, root, &format!("branch.{branch}.pushRemote"));
    let push_default_remote = config_get_opt(git, root, "remote.pushDefault");
    let effective = push_remote
        .as_deref()
        .or(push_default_remote.as_deref())
        .unwrap_or(upstream_remote)
        .to_owned();
    if effective != upstream_remote {
        let (key, value) = match (&push_remote, &push_default_remote) {
            (Some(value), _) => (format!("branch.{branch}.pushRemote"), value.clone()),
            _ => (
                "remote.pushDefault".to_string(),
                push_default_remote.clone().unwrap_or_default(),
            ),
        };
        bail!(
            "拒绝启动：{key} 配置为「{value}」，与当前分支 upstream 远端「{upstream_remote}」不一致；\
             裸 git push 会把提交推到 {value} 而不是 upstream 远端，超出 P-03 允许的网络访问范围（G-08）。\
             请先在仓库中修正或清除该配置后重新开始任务"
        );
    }
    if let Some(refspec) = config_get_opt(git, root, &format!("remote.{effective}.push")) {
        bail!(
            "拒绝启动：remote.{effective}.push 配置了自定义推送 refspec（{refspec}）；\
             裸 git push 会按它推送，无法保证只推送当前分支到 upstream（P-03/G-08）。\
             请先移除该配置后重新开始任务"
        );
    }
    // mirror 是 Git 布尔配置，yes/on/1 与无值项同样表示 true；让 Git 负责规范化。
    if config_get_bool(git, root, &format!("remote.{effective}.mirror")) == Some(true) {
        bail!(
            "拒绝启动：remote.{effective}.mirror=true 会让裸 git push 镜像推送全部引用并删除远端多余分支（P-03/G-08）。\
             请先移除该配置后重新开始任务"
        );
    }
    // 比较 Git 展开后的实际 URL，覆盖多值 url/pushurl 以及 insteadOf/pushInsteadOf
    // 改写；`--all` 列出裸 push 将使用的全部地址，fetch 默认使用首个 URL。
    if effective != "." {
        let fetch_url = run_git_ok(git, root, &["remote", "get-url", &effective])
            .with_context(|| format!("读取 upstream 远端「{effective}」的 fetch URL 失败"))?;
        let fetch_url = fetch_url.trim();
        let push_urls = run_git_ok(
            git,
            root,
            &["remote", "get-url", "--push", "--all", &effective],
        )
        .with_context(|| format!("读取 upstream 远端「{effective}」的 push URL 失败"))?;
        let push_urls: Vec<String> = push_urls
            .lines()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .collect();
        if push_urls.is_empty() || push_urls.iter().any(|url| url != fetch_url) {
            bail!(
                "拒绝启动：remote.{effective}.url / remote.{effective}.pushurl 或 url.*.insteadOf/pushInsteadOf\
                 使裸 git push 的实际目标不同于 upstream fetch 地址，可能访问 upstream 之外的地址（P-03/G-08/P-09）。\
                 请修正导致地址分叉的配置后重新开始任务"
            );
        }
    }
    let push_default = config_get_opt(git, root, "push.default");
    match push_default.as_deref() {
        Some("matching") => {
            bail!(
                "拒绝启动：push.default=matching 会让裸 git push 一次推送所有同名分支（多分支推送，P-03/G-08）。\
                 请改为 simple / upstream 等单分支取值后重新开始任务"
            );
        }
        // nothing：裸 push 因「没有 refspec」以 fatal 失败，upstream 永不更新；
        // 该失败会被按可重试错误无限退避（G-09），任务永远无法推进。
        Some("nothing") => {
            bail!(
                "拒绝启动：push.default=nothing 会让裸 git push 因没有 refspec 而必然失败，\
                 upstream 永远不会更新且任务陷入无限重试（G-08/G-09）。\
                 请改为 simple / upstream 等取值后重新开始任务"
            );
        }
        // simple 是现代 Git 的默认值；upstream 名与本地分支不同时，裸 push 会
        // 因名称不匹配而失败并陷入 G-09 无限重试，必须在任何写操作前拒绝。
        Some("simple") | None if info.upstream_branch.as_str() != branch.as_str() => {
            bail!(
                "拒绝启动：push.default=simple（未设置时的默认行为）且本地分支「{branch}」\
                 与 upstream 分支「{}」不同名，裸 git push 会因分支名不匹配而必然失败并无限重试（G-08/G-09）。\
                 请改用 push.default=upstream 或配置同名 upstream 后重新开始任务",
                info.upstream_branch
            );
        }
        // current：推送目标是「远端上的同名分支」。本地分支名与 upstream 分支名
        // 不同时（如 topic 跟踪 origin/main），裸 push 会静默新建 origin/topic，
        // upstream 未更新却报告全部完成（实测复现）。
        Some("current") if info.upstream_branch.as_str() != branch.as_str() => {
            bail!(
                "拒绝启动：push.default=current 且本地分支「{branch}」与 upstream 分支\
                 「{}」不同名，裸 git push 会推送到 {}/{branch} 而不是 upstream（G-08），\
                 并在远端新建分支。请改用 push.default=simple/upstream 或改用同名 upstream 分支后重新开始任务",
                info.upstream_branch,
                effective
            );
        }
        Some("simple" | "current" | "upstream" | "tracking") | None => {}
        Some(value) => {
            bail!(
                "拒绝启动：push.default={value} 不是受支持的 Git 取值，裸 git push 将必然失败并陷入无限重试（G-08/G-09）。\
                 请改为 simple、current、upstream 或 tracking 后重新开始任务"
            );
        }
    }
    Ok(())
}

/// 验证 HEAD 提交只包含指定路径（G-05 硬性要求）。
pub(super) fn verify_single_path_commit(git: &Path, root: &Path, paths: &[String]) -> Result<bool> {
    let text = run_git_ok(
        git,
        root,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-status",
            "-r",
            "--root",
            "-z",
            "HEAD",
        ],
    )?;
    let segments: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
    let mut seen: Vec<String> = Vec::new();
    let mut index = 0;
    while index < segments.len() {
        let status = segments[index];
        index += 1;
        let first = status.chars().next().unwrap_or('M');
        if first == 'R' || first == 'C' {
            // rename/copy：两个路径
            if let Some(path) = segments.get(index) {
                seen.push((*path).to_owned());
            }
            index += 1;
        }
        if let Some(path) = segments.get(index) {
            seen.push((*path).to_owned());
        }
        index += 1;
    }
    let mut expected = paths.to_vec();
    seen.sort();
    expected.sort();
    if seen == expected {
        tracing::debug!(
            event = "git_commit_verification_completed",
            stage = "commit_verify",
            path_count = seen.len(),
            "Git 单文件提交范围验证成功"
        );
        return Ok(true);
    }
    tracing::error!(
        event = "git_commit_verification_failed",
        stage = "commit_verify",
        actual_paths = seen.len(),
        expected_paths = expected.len(),
        error_type = "path_scope_mismatch",
        "Git 提交包含的路径不符合单文件范围"
    );
    bail!(
        "提交验证失败（G-05）：本次 commit 实际包含 {} 个路径（{:?}），预期只包含 {:?}",
        seen.len(),
        seen,
        expected
    )
}

/// Git 自己解析环境、配置及默认身份；不翻译诊断、不覆盖 hooks/filters 的环境。
pub(super) fn check_commit_identity(git: &Path, root: &Path, control: &Control) -> Result<()> {
    for identity in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
        control.check_cancelled()?;
        let out = run_git(git, root, &["var", identity])?;
        if !out.status.success() {
            bail!(
                "git 未配置有效提交身份（user.name / user.email），无法提交；请先配置 git 身份后重新开始任务：{}",
                summarize(&output_text(&out.stderr), &output_text(&out.stdout))
            );
        }
    }
    Ok(())
}

/// 处理一个文件变更：add → commit（--only，保护既有 staged，G-06）→ 验证（G-05）→
/// push（含 NeedMerge 的 fetch+merge 与无限退避重试，G-08~G-11）。
/// `git add -- <paths>` 报 pathspec 不匹配时核实：这些路径相对 HEAD 的删除是否已
/// 完全暂存（porcelain 首列 `D`、次列空，即 `D ` 形态；工作树与索引一致）。
/// 是则无需也无法再 add，直接进入 commit 阶段。
pub(super) fn deletion_fully_staged(git: &Path, root: &Path, paths: &[String]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let mut args: Vec<&str> = vec!["status", "--porcelain=v1", "-z", "--"];
    for path in paths {
        args.push(path.as_str());
    }
    let Ok(out) = run_git(git, root, &args) else {
        return false;
    };
    if !out.status.success() || out.stdout_truncated || out.stderr_truncated {
        return false;
    }
    let mut staged = 0usize;
    for record in output_text(&out.stdout).split('\0') {
        if record.is_empty() {
            continue;
        }
        let bytes = record.as_bytes();
        if bytes.len() >= 2 && bytes[0] == b'D' && bytes[1] == b' ' {
            staged += 1;
        } else {
            return false;
        }
    }
    staged == paths.len()
}

/// F11/G-06：检测路径的 index/worktree 分叉（部分暂存）。porcelain 两列 X/Y 都非空
///（MM、AM、RM、MD 等）说明暂存区与工作区各有一份不同的改动；已暂存删除后重建
/// 同名文件则分成「D 路径」与「?? 路径」两条记录，也必须保护。检查 commit 的全部
/// 路径（包括 rename 旧路径），避免 add 或 commit --only 用工作区重写用户暂存状态。
pub(super) fn partial_stage_divergence(git: &Path, root: &Path, paths: &[String]) -> Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    // 不折叠 rename，使旧路径的 staged 删除与重建的未跟踪文件能按同一路径比较。
    let mut args: Vec<&str> = vec![
        "status",
        "--porcelain=v1",
        "-z",
        "--no-renames",
        "--untracked-files=all",
        "--",
    ];
    for path in paths {
        args.push(path.as_str());
    }
    let out = run_git(git, root, &args)
        .with_context(|| "执行 git status 失败（检查部分暂存状态）".to_string())?;
    if !out.status.success() {
        bail!(
            "检查部分暂存状态失败：{}",
            summarize(&output_text(&out.stderr), &output_text(&out.stdout))
        );
    }
    let text = output_text(&out.stdout);
    for line in text.split('\0') {
        let bytes = line.as_bytes();
        if bytes.len() < 2 {
            continue;
        }
        let (x, y) = (bytes[0], bytes[1]);
        if x != b' ' && x != b'?' && y != b' ' && y != b'?' {
            return Ok(true);
        }
        if x == b'D'
            && y == b' '
            && text
                .split('\0')
                .any(|other| other.strip_prefix("?? ") == line.get(3..))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 写索引前拒绝父仓库无法安全逐文件提交的结构性状态，不用 add 探测后再回滚。
pub(super) fn check_change_structure(
    git: &Path,
    root: &Path,
    change: &FileChange,
    control: &Control,
) -> Result<()> {
    let removed_path = match change {
        FileChange::Deleted(path) | FileChange::Renamed(path, _) => Some(path),
        _ => None,
    };
    if let Some(path) = removed_path {
        control.check_cancelled()?;
        match std::fs::symlink_metadata(root.join(path)) {
            Ok(metadata) if metadata.is_dir() => {
                bail!(
                    "路径「{path}」原文件已被目录替换，无法安全单独提交删除；\
                     未执行 add/commit，已保留原暂存区和目录内容，请先手动处理（G-04/G-06）"
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("检查删除路径「{path}」的类型失败"));
            }
        }
    }
    if !matches!(change, FileChange::Modified(_)) {
        return Ok(());
    }
    let path = change.path();
    control.check_cancelled()?;
    let entry = run_git_ok(git, root, &["ls-tree", "-z", "HEAD", "--", path])?;
    control.check_cancelled()?;
    for record in entry.split('\0').filter(|record| !record.is_empty()) {
        let Some((header, entry_path)) = record.split_once('\t') else {
            bail!("解析路径「{path}」的 HEAD 条目失败");
        };
        if entry_path != path {
            continue;
        }
        let mut fields = header.split_whitespace();
        if fields.next() != Some("160000") {
            continue;
        }
        let (Some("commit"), Some(oid), None) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("解析子模块「{path}」的 gitlink 失败");
        };
        let head = run_git_ok(git, &root.join(path), &["rev-parse", "HEAD"])
            .with_context(|| format!("检查子模块「{path}」的 HEAD 失败"))?;
        control.check_cancelled()?;
        if head.trim() == oid {
            bail!(
                "子模块「{path}」的 HEAD 未改变，仅有内部修改或未跟踪内容，父仓库无法提交这些内容；\
                 未执行 add/commit，请先在子模块中自行处理后重新开始任务（G-09/G-12）"
            );
        }
    }
    Ok(())
}

/// 合并提交不带 pathspec；每次尝试都检查范围，保护等待期间用户另行暂存的文件。
pub(super) fn check_merge_staging(git: &Path, root: &Path, control: &Control) -> Result<()> {
    control.check_cancelled()?;
    // 两侧统一禁用 rename 折叠，避免 index↔HEAD 与 HEAD↔MERGE_HEAD 的
    // 相似度识别不同，导致同一 rename 的旧路径被误判为范围外。
    let staged = run_git_ok(
        git,
        root,
        &["diff", "--cached", "--name-only", "--no-renames", "-z"],
    )
    .context("检查合并暂存区失败（G-06）")?;
    control.check_cancelled()?;
    let merge_scope = run_git_ok(
        git,
        root,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            "HEAD",
            "MERGE_HEAD",
        ],
    )
    .context("无法确定合并涉及的文件范围（G-06）")?;
    control.check_cancelled()?;
    let in_scope: std::collections::HashSet<&str> = merge_scope
        .split('\0')
        .filter(|line| !line.is_empty())
        .collect();
    let extras: Vec<&str> = staged
        .split('\0')
        .filter(|line| !line.is_empty() && !in_scope.contains(line))
        .collect();
    if !extras.is_empty() {
        tracing::error!(
            event = "git_merge_scope_rejected",
            stage = "merge_scope",
            error_type = "unrelated_staged_paths",
            extra_path_count = extras.len(),
            "合并暂存区存在范围外文件，停止以保护用户内容"
        );
        bail!(
            "仓库存在合并范围之外的已暂存文件（{}）：完成合并提交会把它们一并带入（G-06）。\
             请先在仓库中取消暂存或另行处理这些文件后重新开始任务",
            extras.join("、")
        );
    }
    Ok(())
}
