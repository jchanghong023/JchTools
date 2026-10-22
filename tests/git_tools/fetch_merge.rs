use super::*;
// 覆盖 G-10（远端新提交：自动 fetch + merge 后继续 push，不 rebase）
#[test]
fn remote_ahead_triggers_fetch_merge_then_push() {
    let fix = fixture();
    // 本地准备待提交文件
    fs::write(fix.repo.join("local.txt"), "local change\n").unwrap();
    // 另一个克隆直接向远端推进新提交（制造 non-fast-forward）
    let other = fix.repo.parent().unwrap().join("other");
    git_ok(
        fix.repo.parent().unwrap(),
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    fs::write(other.join("remote-side.txt"), "remote change\n").unwrap();
    git_ok(&other, &["add", "remote-side.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote side"]);
    git_ok(&other, &["push", "-q"]);

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "收尾文案：{}",
        outcome.text
    );
    let log = remote_log(&fix.remote);
    assert!(
        log.contains(&"update: local.txt".to_string()),
        "本地提交最终推送成功：{log:?}"
    );
    // G-10 的 merge 语义以提交拓扑为证：合并提交的标题可受 Git 版本、
    // 本地配置和语言影响，不能用 "Merge branch" 文案替代两个父提交。
    assert_merge_tip(&fix.remote, "master");
    assert!(
        fs::read(fix.repo.join("remote-side.txt")).is_ok(),
        "合并后远端内容在工作区可见"
    );
}

// 覆盖 G-10 尾段（合并被本地未提交变更阻挡：停止并显示原因）
#[test]
fn merge_blocked_by_dirty_worktree_stops_with_reason() {
    let fix = fixture();
    // f.txt 由工具处理（排序在前）；g.txt 留在工作区作为阻挡源
    fs::write(fix.repo.join("f.txt"), "local f\n").unwrap();
    fs::write(fix.repo.join("g.txt"), "local dirty g\n").unwrap();
    // 远端直接推进 g.txt 的修改
    let other = fix.repo.parent().unwrap().join("other");
    git_ok(
        fix.repo.parent().unwrap(),
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    fs::write(other.join("g.txt"), "remote g\n").unwrap();
    git_ok(&other, &["add", "g.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote g"]);
    git_ok(&other, &["push", "-q"]);

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("本地未提交变更阻挡"),
        "必须停止并说明阻挡原因：{}",
        outcome.text
    );
    // 用户内容不被覆盖
    assert_eq!(
        fs::read_to_string(fix.repo.join("g.txt")).unwrap(),
        "local dirty g\n",
        "不得覆盖用户未提交内容"
    );
}

// 覆盖 G-11（冲突：停止自动处理、保留现场、不自动选择任何一方）
#[test]
fn merge_conflict_stops_and_preserves_state() {
    let fix = fixture();
    fs::write(fix.repo.join("conflict.txt"), "local version\n").unwrap();
    // 远端推进同一文件的不同内容
    let other = fix.repo.parent().unwrap().join("other");
    git_ok(
        fix.repo.parent().unwrap(),
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    fs::write(other.join("conflict.txt"), "remote version\n").unwrap();
    git_ok(&other, &["add", "conflict.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote conflict"]);
    git_ok(&other, &["push", "-q"]);

    let outcome = run_tool(&fix.repo);
    assert!(outcome.text.contains("冲突"), "收尾文案：{}", outcome.text);
    assert!(
        outcome.shared.state.lock().unwrap().contains("冲突"),
        "总体状态：{}",
        outcome.shared.state.lock().unwrap()
    );
    // 保留 Git 冲突现场：未合并条目存在、不自动解决
    let unresolved = git_ok(&fix.repo, &["diff", "--name-only", "--diff-filter=U"]);
    assert!(
        unresolved.contains("conflict.txt"),
        "冲突现场必须保留: {unresolved}"
    );
    let content = fs::read_to_string(fix.repo.join("conflict.txt")).unwrap();
    assert!(
        content.contains("local version") && content.contains("remote version"),
        "不得自动选择 ours 或 theirs: {content}"
    );
    // G-14：日志必须包含实际阶段与 git 输出细节（CONFLICT 行），不能只说「失败」。
    // 「开始处理：conflict.txt」「git add -- conflict.txt」这类行任何实现都会写，
    // 不得作为 git 输出被记录的替代证据（对 rebase/merge 与否、stderr 是否转写均不敏感）。
    let joined = outcome.logs.join(
        "
",
    );
    assert!(
        joined.contains("CONFLICT"),
        "日志必须包含 git merge 的 CONFLICT 细节（G-14）: {joined}"
    );
    // 本地的单文件提交保留（未回滚），等待冲突解决后继续
    let local_head = git_ok(&fix.repo, &["log", "-1", "--format=%s"]);
    assert_eq!(
        local_head.trim(),
        "update: conflict.txt",
        "已成功的 commit 不回滚"
    );
}

// 覆盖 G-11 续段 + G-12（冲突解决后重启：自动完成合并提交并推送；不重复提交已 push 文件）
#[test]
fn conflict_resolved_resume_pushes_and_does_not_recommit() {
    let fix = fixture();
    fs::write(fix.repo.join("conflict.txt"), "local version\n").unwrap();
    let other = fix.repo.parent().unwrap().join("other");
    git_ok(
        fix.repo.parent().unwrap(),
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    fs::write(other.join("conflict.txt"), "remote version\n").unwrap();
    git_ok(&other, &["add", "conflict.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote conflict"]);
    git_ok(&other, &["push", "-q"]);
    let first = run_tool(&fix.repo);
    assert!(first.text.contains("冲突"), "先制造冲突：{}", first.text);

    // 用户在外部解决冲突并暂存
    fs::write(fix.repo.join("conflict.txt"), "resolved version\n").unwrap();
    git_ok(&fix.repo, &["add", "conflict.txt"]);
    // 重启任务：自动完成合并提交 → push；不再重复提交 conflict.txt
    let second = run_tool(&fix.repo);
    assert!(
        second.text.contains("全部完成") || second.text.contains("没有需要提交的变更"),
        "续接收尾文案：{}",
        second.text
    );
    let log = remote_log(&fix.remote);
    let updates = log.iter().filter(|s| **s == "update: conflict.txt").count();
    assert_eq!(updates, 1, "已 push 的文件不得重复提交：{log:?}");
    assert_merge_tip(&fix.remote, "master");
    // 远端最终内容是解决后的版本
    let remote_content = git_ok(&fix.remote, &["show", "master:conflict.txt"]);
    assert_eq!(remote_content, "resolved version\n", "合并结果必须完整推送");
}

// 覆盖 G-06（回归：用户解决冲突后另行 stage 的无关文件不得被夹带进合并提交；
// 修复前 `git commit --no-edit` 不带 pathspec，会把整个暂存区一起提交）
#[test]
fn conflict_resume_refuses_extra_staged_files() {
    let fix = fixture();
    fs::write(fix.repo.join("conflict.txt"), "local version\n").unwrap();
    let other = fix.repo.parent().unwrap().join("other");
    git_ok(
        fix.repo.parent().unwrap(),
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    fs::write(other.join("conflict.txt"), "remote version\n").unwrap();
    git_ok(&other, &["add", "conflict.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote conflict"]);
    git_ok(&other, &["push", "-q"]);
    let first = run_tool(&fix.repo);
    assert!(first.text.contains("冲突"), "先制造冲突：{}", first.text);

    // 用户解决冲突并暂存冲突文件，随后又另行暂存了一个无关文件
    fs::write(fix.repo.join("conflict.txt"), "resolved version\n").unwrap();
    git_ok(&fix.repo, &["add", "conflict.txt"]);
    fs::write(fix.repo.join("unrelated.txt"), "unrelated\n").unwrap();
    git_ok(&fix.repo, &["add", "unrelated.txt"]);
    let protected_tree = git_ok(&fix.repo, &["write-tree"]).trim().to_owned();
    let protected_head = git_ok(&fix.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let protected_remote = git_ok(&fix.remote, &["rev-parse", "master"])
        .trim()
        .to_owned();

    // 重启任务：必须检测到合并范围之外的暂存文件并停止，不得完成合并提交
    let second = run_tool(&fix.repo);
    assert!(
        second.text.contains("unrelated.txt"),
        "错误信息必须点名夹带文件：{}",
        second.text
    );
    assert!(
        second.text.contains("G-06"),
        "错误信息必须说明 G-06 保护原因：{}",
        second.text
    );
    assert!(
        fix.repo.join(".git").join("MERGE_HEAD").is_file(),
        "合并现场必须保留（MERGE_HEAD 仍在），等待用户处理暂存区"
    );
    assert_eq!(
        git_ok(&fix.repo, &["write-tree"]).trim(),
        protected_tree,
        "拒绝续接必须完整保留用户已解决和无关的 staged 内容"
    );
    assert_eq!(
        git_ok(&fix.repo, &["rev-parse", "HEAD"]).trim(),
        protected_head,
        "拒绝续接不得产生提交"
    );
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]).trim(),
        protected_remote,
        "拒绝续接不得推送"
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("conflict.txt")).unwrap(),
        "resolved version\n",
        "拒绝续接不得覆盖已解决的工作区内容"
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("unrelated.txt")).unwrap(),
        "unrelated\n",
        "拒绝续接不得改写无关工作区文件"
    );

    // 用户取消暂存无关文件后重启：自动完成合并提交并推送，提交不得包含 unrelated.txt
    git_ok(&fix.repo, &["restore", "--staged", "unrelated.txt"]);
    let third = run_tool(&fix.repo);
    assert!(
        third.text.contains("全部完成") || third.text.contains("没有需要提交的变更"),
        "清理暂存区后续接应完成：{}",
        third.text
    );
    // 合并提交本身（两个父提交的那次）不得包含无关文件；用户取消暂存后
    // unrelated.txt 回到未跟踪状态，由 G-12 的重新扫描按流程单独提交推送，
    // 这是正常续接行为，与「夹带进合并提交」必须区分。
    let merge_commit = git_ok(&fix.repo, &["rev-list", "--merges", "-n", "1", "HEAD"])
        .trim()
        .to_owned();
    assert!(!merge_commit.is_empty(), "续接后应存在合并提交");
    let first_parent = format!("{merge_commit}^1");
    let merge_files = git_ok(
        &fix.repo,
        &["diff", "--name-only", &first_parent, &merge_commit],
    );
    assert!(
        !merge_files.contains("unrelated.txt"),
        "合并提交不得包含无关文件：{merge_files}"
    );
    let log = remote_log(&fix.remote);
    assert!(
        log.contains(&"update: unrelated.txt".to_string()),
        "取消暂存后的无关文件应作为独立提交推送（G-12）：{log:?}"
    );
}

// 覆盖 G-02/G-10（F09 回归：upstream 不得从显示串 rsplit_once('/') 拆分——
// origin/feature/demo 会拆成远端「origin/feature」、分支「demo」，git fetch 以该
// 假远端执行必然 128 失败并按 G-09 无限退避；必须走 branch.<b>.remote /
// branch.<b>.merge 结构化配置，含斜杠分支名保持完整）
#[test]
fn slashed_branch_upstream_fetches_merges_and_pushes() {
    for branch in ["feature/demo", "feature/deep/nested/name"] {
        let fix = fixture_on_branch(branch);
        let base = fix.repo.parent().unwrap();
        remote_pushes_ahead(&fix.remote, branch, base);
        fs::write(fix.repo.join("local.txt"), "local change\n").unwrap();
        let outcome = run_tool(&fix.repo);
        assert!(
            outcome.text.contains("全部完成"),
            "分支 {branch} 的远端领先场景必须经 fetch+merge 后推送成功：{}",
            outcome.text
        );
        let log = remote_branch_log(&fix.remote, branch);
        assert!(
            log.contains(&"update: local.txt".to_string()),
            "分支 {branch} 的远端应收到本地提交：{log:?}"
        );
        assert_merge_tip(&fix.remote, branch);
        assert!(
            fs::read(fix.repo.join("remote-side.txt")).is_ok(),
            "合并后远端内容在工作区可见"
        );
    }
}

// 覆盖 G-02/G-10/P-03（F09：非 origin 远端作为 upstream——fetch/merge/push 都必须
// 走 branch.<b>.remote 指向的 other，origin 完全不被访问）
#[test]
fn non_origin_upstream_fetches_from_configured_remote() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    git_ok(&fix.repo, &["push", "-q", "other", "master"]);
    git_ok(&fix.repo, &["config", "branch.master.remote", "other"]);
    git_ok(
        &fix.repo,
        &["config", "branch.master.merge", "refs/heads/master"],
    );
    // other 侧领先一个提交
    let base = fix.repo.parent().unwrap();
    git_ok(
        base,
        &["clone", "-q", &other.display().to_string(), "other-clone"],
    );
    let clone = base.join("other-clone");
    fs::write(clone.join("remote-side.txt"), "remote change\n").unwrap();
    git_ok(&clone, &["add", "remote-side.txt"]);
    git_ok(&clone, &["commit", "-q", "-m", "remote side"]);
    git_ok(&clone, &["push", "-q"]);

    fs::write(fix.repo.join("local.txt"), "local change\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "非 origin upstream 的远端领先场景：{}",
        outcome.text
    );
    let other_log = remote_branch_log(&other, "master");
    assert!(
        other_log.contains(&"update: local.txt".to_string()),
        "other 应收到本地提交：{other_log:?}"
    );
    assert_merge_tip(&other, "master");
    // origin 完全未被触碰（fetch/merge/push 都走 other）
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应被访问"
    );
}

// 覆盖 G-02/G-10（F09：本地 upstream（branch.<b>.remote="."）——显示串是
// 「master」不含斜杠，旧实现 rsplit_once('/') 直接无法解析进入无限退避；
// 修复后按配置取 remote="."、分支 master，git fetch . master 与 merge 正常。
// 裸 git push 在分支名与 upstream 名不同时需要 push.default=upstream 才推得上，
// 该取值不属于 F10 预检的拒绝范围（仅 matching 被拒））
#[test]
fn local_upstream_dot_remote_fetches_merges_and_pushes() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q"]);
    fs::write(repo.join("README.md"), "init\n").unwrap();
    git_ok(&repo, &["add", "README.md"]);
    git_ok(&repo, &["commit", "-q", "-m", "init"]);
    git_ok(&repo, &["config", "user.name", "JchTools Test"]);
    git_ok(&repo, &["config", "user.email", "test@jchtools.local"]);
    git_ok(&repo, &["checkout", "-q", "-b", "topic"]);
    git_ok(&repo, &["config", "branch.topic.remote", "."]);
    git_ok(
        &repo,
        &["config", "branch.topic.merge", "refs/heads/master"],
    );
    git_ok(&repo, &["config", "push.default", "upstream"]);
    // 让 master（upstream 侧）领先一个提交
    git_ok(&repo, &["checkout", "-q", "master"]);
    fs::write(repo.join("remote-side.txt"), "remote change\n").unwrap();
    git_ok(&repo, &["add", "remote-side.txt"]);
    git_ok(&repo, &["commit", "-q", "-m", "remote side"]);
    git_ok(&repo, &["checkout", "-q", "topic"]);
    // 本地待提交变更
    fs::write(repo.join("local.txt"), "local change\n").unwrap();

    let outcome = run_tool(&repo);
    assert!(
        outcome.text.contains("全部完成"),
        "本地 upstream 的远端领先场景：{}",
        outcome.text
    );
    let master_log: Vec<String> = git_ok(&repo, &["log", "--format=%s", "master"])
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    assert!(
        master_log.contains(&"update: local.txt".to_string()),
        "master（upstream）应收到本地提交：{master_log:?}"
    );
    assert_merge_tip(&repo, "master");
    assert_eq!(
        git_ok(&repo, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "topic",
        "不得切换分支"
    );
}

// 覆盖 G-06/G-11（独立审查发现：夹带检测的两条 diff 各自做 rename 检测且基准
// 内容对不同，rename 冲突被用户以低相似度内容整体重写解决时，`--cached` 侧
// 检出不了 rename，旧路径被误报为「合并范围之外的已暂存文件」阻断续接；
// 统一 --no-renames 后两侧路径集合对称，重写式解决可正常完成合并提交）
#[test]
fn conflict_resume_accepts_rewritten_rename_resolution() {
    let fix = fixture();
    let base_lines: Vec<String> = (0..20)
        .map(|i| format!("line {i} shared content padding\n"))
        .collect();
    let base = base_lines.join("");
    fs::write(fix.repo.join("file.txt"), &base).unwrap();
    git_ok(&fix.repo, &["add", "file.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "seed file"]);
    git_ok(&fix.repo, &["push", "-q"]);
    // 远端：rename + 改第一行。
    let other = fix.repo.parent().unwrap().join("other");
    git_ok(
        fix.repo.parent().unwrap(),
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    let mut remote_lines = base_lines.clone();
    remote_lines[0] = "remote rewritten first line\n".to_string();
    fs::rename(other.join("file.txt"), other.join("renamed.txt")).unwrap();
    fs::write(other.join("renamed.txt"), remote_lines.join("")).unwrap();
    git_ok(&other, &["add", "-A"]);
    git_ok(&other, &["commit", "-q", "-m", "remote rename"]);
    git_ok(&other, &["push", "-q"]);
    // 本地：同一行改成不同内容（rename + modify 同行冲突）。
    let mut local_lines = base_lines.clone();
    local_lines[0] = "local rewritten first line\n".to_string();
    fs::write(fix.repo.join("file.txt"), local_lines.join("")).unwrap();
    let first = run_tool(&fix.repo);
    assert!(
        first.text.contains("冲突"),
        "前置：先制造 rename 冲突：{}",
        first.text
    );
    // 用户解决：新路径写与 HEAD 旧内容相似度低于 rename 检测阈值的全重写内容，
    // 删除旧路径，一并暂存（此时 --cached 侧 diff 不再折叠出 rename）。
    let resolved_content =
        "completely different resolved content with no shared lines at all\n".repeat(20);
    fs::write(fix.repo.join("renamed.txt"), &resolved_content).unwrap();
    let _ = fs::remove_file(fix.repo.join("file.txt"));
    git_ok(&fix.repo, &["add", "-A"]);
    let resolved_tree = git_ok(&fix.repo, &["write-tree"]).trim().to_owned();
    let second = run_tool(&fix.repo);
    assert!(
        second.text.contains("全部完成") || second.text.contains("没有需要提交的变更"),
        "重写式 rename 解决不得被夹带检测误拒：{}",
        second.text
    );
    let merge_commit = git_ok(&fix.repo, &["rev-list", "--merges", "-n", "1", "HEAD"])
        .trim()
        .to_owned();
    assert!(!merge_commit.is_empty(), "续接后应存在合并提交");
    // 显式比较两个树：diff-tree -m 会逐父输出，同一路径可能出现多次，
    // 不能用原始输出行的数量判断是否存在额外修改。
    let first_parent = format!("{merge_commit}^1");
    let merge_files = git_ok(
        &fix.repo,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            &first_parent,
            &merge_commit,
        ],
    );
    let mut paths: Vec<&str> = merge_files.lines().collect();
    paths.sort_unstable();
    assert_eq!(
        paths,
        vec!["file.txt", "renamed.txt"],
        "合并相对本地父提交必须只删除旧路径并写入新路径：{merge_files}"
    );
    assert_eq!(
        git_ok(&fix.repo, &["rev-parse", "HEAD^{tree}"]).trim(),
        resolved_tree,
        "续接不得改变用户解决并暂存的树"
    );
    assert_eq!(
        git_ok(&fix.repo, &["ls-tree", "-r", "--name-only", "HEAD"]).trim(),
        "README.md\nrenamed.txt",
        "旧路径必须删除，既有文件保留且不得带入额外文件"
    );
    assert_eq!(
        git_ok(&fix.repo, &["show", "HEAD:renamed.txt"]),
        resolved_content,
        "合并提交必须保留用户重写的解决内容"
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("renamed.txt")).unwrap(),
        resolved_content,
        "续接不得覆盖已解决的工作区内容"
    );
    assert_merge_tip(&fix.repo, "HEAD");
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]).trim(),
        merge_commit,
        "本地真实合并提交必须成功推送到 bare upstream"
    );
    assert!(
        git_ok(&fix.repo, &["status", "--porcelain"])
            .trim()
            .is_empty(),
        "续接后工作区和暂存区必须干净"
    );
    assert!(
        !fix.repo.join(".git").join("MERGE_HEAD").exists(),
        "成功续接必须结束合并中间态"
    );
}

// 覆盖 G-06/G-11：合并范围按字面路径核实，不能把前导空格剥除后误认成冲突文件。
#[test]
fn conflict_resume_refuses_whitespace_distinct_staged_path() {
    let fix = fixture();
    fs::write(fix.repo.join("conflict.txt"), "local version\n").unwrap();
    let other = fix.repo.parent().unwrap().join("other");
    git_ok(
        fix.repo.parent().unwrap(),
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    fs::write(other.join("conflict.txt"), "remote version\n").unwrap();
    git_ok(&other, &["add", "conflict.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote conflict"]);
    git_ok(&other, &["push", "-q"]);
    assert!(run_tool(&fix.repo).text.contains("冲突"));
    fs::write(fix.repo.join("conflict.txt"), "resolved\n").unwrap();
    fs::write(fix.repo.join(" conflict.txt"), "unrelated staged\n").unwrap();
    git_ok(&fix.repo, &["add", "--", "conflict.txt", " conflict.txt"]);
    let tree = git_ok(&fix.repo, &["write-tree"]);
    let head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let remote = git_ok(&fix.remote, &["rev-parse", "master"]);

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("合并范围之外") && outcome.text.contains(" conflict.txt"),
        "前导空格不同的路径必须拒绝夹带：{}",
        outcome.text
    );
    assert_eq!(git_ok(&fix.repo, &["write-tree"]), tree, "暂存树必须保留");
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD"]), head, "不得提交");
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        remote,
        "不得推送"
    );
    assert!(
        fix.repo.join(".git/MERGE_HEAD").is_file(),
        "保留已解决的合并现场"
    );
}

// 覆盖 G-02/G-06：从仓库子目录续接合并且 diff.relative=true 时，
// 范围检查仍须检查根目录全部 staged 路径，不能把子目录之外的文件夹带进合并提交。
#[test]
fn conflict_resume_from_subdirectory_preserves_outside_staged_files() {
    let fix = fixture();
    fs::write(fix.repo.join("conflict.txt"), "local version\n").unwrap();
    let base = fix.repo.parent().unwrap();
    let other = base.join("other");
    git_ok(
        base,
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    fs::write(other.join("conflict.txt"), "remote version\n").unwrap();
    git_ok(&other, &["add", "conflict.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote conflict"]);
    git_ok(&other, &["push", "-q"]);
    let first = run_tool(&fix.repo);
    assert!(first.text.contains("冲突"), "先制造冲突：{}", first.text);

    fs::write(fix.repo.join("conflict.txt"), "resolved version\n").unwrap();
    fs::write(fix.repo.join("unrelated.txt"), "protected staged content\n").unwrap();
    git_ok(&fix.repo, &["add", "conflict.txt", "unrelated.txt"]);
    git_ok(&fix.repo, &["config", "diff.relative", "true"]);
    let subdir = fix.repo.join("selected-subdirectory");
    fs::create_dir(&subdir).unwrap();
    let protected_tree = git_ok(&fix.repo, &["write-tree"]);
    let protected_head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let protected_remote = git_ok(&fix.remote, &["rev-parse", "master"]);

    let outcome = run_tool(&subdir);
    assert!(
        outcome.text.contains("unrelated.txt") && outcome.text.contains("G-06"),
        "必须识别子目录之外的无关暂存文件并拒绝合并提交：{}",
        outcome.text
    );
    assert!(
        fix.repo.join(".git/MERGE_HEAD").is_file(),
        "必须保留合并现场"
    );
    assert_eq!(git_ok(&fix.repo, &["write-tree"]), protected_tree);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD"]), protected_head);
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        protected_remote
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("unrelated.txt")).unwrap(),
        "protected staged content\n"
    );
}
