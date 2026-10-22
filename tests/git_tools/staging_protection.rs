use super::*;
// 覆盖 G-06（F11 回归：MM 部分暂存——git add 会用工作区内容重写该路径的暂存
// 版本，原 staged 内容永久丢失；修复后该文件计为失败并说明「部分暂存状态受
// 保护」，不执行 add，staged blob、其他暂存项、工作区全部原样保留，也不 push）
#[test]
fn partially_staged_file_is_protected_not_overwritten() {
    let fix = fixture();
    fs::write(fix.repo.join("mm-file.txt"), "v0\n").unwrap();
    git_ok(&fix.repo, &["add", "mm-file.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "base"]);
    git_ok(&fix.repo, &["push", "-q"]);
    fs::write(fix.repo.join("mm-file.txt"), "v1\n").unwrap();
    git_ok(&fix.repo, &["add", "mm-file.txt"]); // 用户暂存 v1
    fs::write(fix.repo.join("mm-file.txt"), "v2\n").unwrap(); // 工作区再改 → MM
                                                              // 另一个已暂存项（按路径排序在 mm-file 之后；若实现错误地继续处理会被改动）
    fs::write(fix.repo.join("zz-staged.txt"), "staged content\n").unwrap();
    git_ok(&fix.repo, &["add", "zz-staged.txt"]);
    // 前置条件守卫：确为 MM 形态
    let status = git_ok(&fix.repo, &["status", "--porcelain", "--", "mm-file.txt"]);
    assert_eq!(status.trim(), "MM mm-file.txt", "前置条件：{status}");

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("部分暂存状态受保护"),
        "必须说明保护原因：{}",
        outcome.text
    );
    // 原 staged 版本仍在暂存区（未被 add 覆盖）
    assert_eq!(
        git_ok(&fix.repo, &["cat-file", "-p", ":mm-file.txt"]),
        "v1\n",
        "staged blob 必须原样保留"
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("mm-file.txt")).unwrap(),
        "v2\n",
        "工作区内容不得被改动"
    );
    // 其他暂存项不受影响：仍是用户暂存的内容
    assert_eq!(
        git_ok(&fix.repo, &["cat-file", "-p", ":zz-staged.txt"]),
        "staged content\n",
        "其他暂存项不得受影响"
    );
    let log = remote_log(&fix.remote);
    assert!(
        !log.iter().any(|s| s.starts_with("update: ")),
        "不得推送任何文件：{log:?}"
    );
}

// 覆盖 G-06（F11：index 有改动而 worktree 内容==HEAD——git add 同样会用 HEAD
// 内容覆盖已暂存的 v1，必须保护）
#[test]
fn staged_change_with_head_worktree_content_is_protected() {
    let fix = fixture();
    fs::write(fix.repo.join("guard.txt"), "v0\n").unwrap();
    git_ok(&fix.repo, &["add", "guard.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "base"]);
    fs::write(fix.repo.join("guard.txt"), "v1\n").unwrap();
    git_ok(&fix.repo, &["add", "guard.txt"]); // 暂存 v1
    fs::write(fix.repo.join("guard.txt"), "v0\n").unwrap(); // 工作区恢复 HEAD 内容
    let status = git_ok(&fix.repo, &["status", "--porcelain", "--", "guard.txt"]);
    assert_eq!(status.trim(), "MM guard.txt", "前置条件：{status}");

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("部分暂存状态受保护"),
        "必须说明保护原因：{}",
        outcome.text
    );
    assert_eq!(
        git_ok(&fix.repo, &["cat-file", "-p", ":guard.txt"]),
        "v1\n",
        "staged 版本必须原样保留"
    );
    let log = remote_log(&fix.remote);
    assert!(
        !log.iter().any(|s| s.contains("guard")),
        "不 push 该文件：{log:?}"
    );
}

// 覆盖 G-06（F11：部分暂存 rename——git mv 已暂存重命名后工作区又改了新路径；
// add -- 新路径会覆盖已暂存的重命名内容，必须保护）
#[test]
fn partially_staged_rename_is_protected() {
    let fix = fixture();
    fs::write(fix.repo.join("old-name.txt"), "original\n").unwrap();
    git_ok(&fix.repo, &["add", "old-name.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "base"]);
    git_ok(&fix.repo, &["push", "-q"]);
    git_ok(&fix.repo, &["mv", "old-name.txt", "new-name.txt"]);
    fs::write(fix.repo.join("new-name.txt"), "worktree-modified\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("部分暂存状态受保护"),
        "必须说明保护原因：{}",
        outcome.text
    );
    // 已暂存的重命名内容保持原样
    assert_eq!(
        git_ok(&fix.repo, &["cat-file", "-p", ":new-name.txt"]),
        "original\n",
        "staged rename 内容必须原样保留"
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("new-name.txt")).unwrap(),
        "worktree-modified\n",
        "工作区内容不得被改动"
    );
    let log = remote_log(&fix.remote);
    assert!(
        !log.iter().any(|s| s.contains("name")),
        "不 push 该文件：{log:?}"
    );
}

// 覆盖 G-09/G-12/G-16（F12 回归：上次任务在 commit 后、push 前停止——porcelain
// 为空但本地领先 1 个提交；修复前直接误报「工作区与远端一致」，修复后进入只
// 推送流程补推，不重复 commit）
#[test]
fn restart_after_commit_before_push_backfills_push_only() {
    let fix = fixture();
    fs::write(fix.repo.join("a.txt"), "a\n").unwrap();
    git_ok(&fix.repo, &["add", "a.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "manual pending push"]);
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("没有需要提交的变更"),
        "收尾文案：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("已补推本地领先的 1 个提交"),
        "必须进入只推送流程补推：{}",
        outcome.text
    );
    let log = remote_log(&fix.remote);
    assert!(
        log.contains(&"manual pending push".to_string()),
        "本地既有提交应被补推：{log:?}"
    );
    assert!(
        !log.iter().any(|s| s.starts_with("update: ")),
        "只推送、不得重复 commit：{log:?}"
    );
}

// 覆盖 G-09/G-12/G-16（F12：本地领先但推送持续失败——重启后必须如实报告
// 「本地有 N 个提交未推送」，绝不报「工作区与远端一致」，也不得新增提交）
#[test]
fn restart_with_unpushable_commits_reports_honestly() {
    let fix = fixture();
    fs::write(fix.repo.join("a.txt"), "a\n").unwrap();
    git_ok(&fix.repo, &["add", "a.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "manual pending push"]);
    // 指向不存在的远端路径：push 以 128 失败并按 G-09 无限重试
    let bogus = fix.repo.parent().unwrap().join("bogus.git");
    git_ok(
        &fix.repo,
        &["remote", "set-url", "origin", &bogus.display().to_string()],
    );
    let control = Arc::new(Control::default());
    let (logs, _shared, handle) = spawn_run(&git_exe(), &fix.repo, &control);
    wait_log_contains(&logs, "push 失败");
    control.cancel();
    let outcome = handle.join().unwrap();
    assert!(
        outcome.text.contains("任务已停止"),
        "收尾文案：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("本地有 1 个提交未推送"),
        "必须如实报告未推送：{}",
        outcome.text
    );
    assert!(
        !outcome.text.contains("工作区与远端一致"),
        "不得误报一致：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "远端不应收到任何提交"
    );
}

// 覆盖 G-12/G-16（F12：本地不领先而远端领先——没有可推送内容时不得宣称
// 「工作区与远端一致」，如实说明远端领先且本工具不自动拉取）
#[test]
fn remote_ahead_without_local_commits_reports_not_in_sync() {
    let fix = fixture();
    let base = fix.repo.parent().unwrap();
    git_ok(
        base,
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    let other = base.join("other");
    fs::write(other.join("remote-side.txt"), "remote change\n").unwrap();
    git_ok(&other, &["add", "remote-side.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote side"]);
    git_ok(&other, &["push", "-q"]);
    git_ok(&fix.repo, &["fetch", "-q", "origin"]); // 本地可见远端领先
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("没有需要提交的变更"),
        "收尾文案：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("远端领先 1 个提交"),
        "必须如实说明远端领先：{}",
        outcome.text
    );
    assert!(
        !outcome.text.contains("工作区与远端一致"),
        "不得误报一致：{}",
        outcome.text
    );
}

// 覆盖 G-09/G-12/G-16（F12：分叉场景——本地领先且远端领先同一文件的不同内容，
// 补推合并冲突时如实报告未推送并保留现场，绝不报「一致」）
#[test]
fn diverged_history_conflict_reports_unpushed_honestly() {
    let fix = fixture();
    fs::write(fix.repo.join("shared.txt"), "base\n").unwrap();
    git_ok(&fix.repo, &["add", "shared.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "base shared"]);
    git_ok(&fix.repo, &["push", "-q"]);
    let base = fix.repo.parent().unwrap();
    git_ok(
        base,
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    let other = base.join("other");
    fs::write(other.join("shared.txt"), "remote version\n").unwrap();
    git_ok(&other, &["add", "shared.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote side"]);
    git_ok(&other, &["push", "-q"]);
    // 本地也有一个未推送提交（改同一文件的不同内容）
    fs::write(fix.repo.join("shared.txt"), "local version\n").unwrap();
    git_ok(&fix.repo, &["add", "shared.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "local side"]);

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("冲突"),
        "补推分叉必须走合并并如实呈现冲突：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("本地有 1 个提交未推送"),
        "必须如实报告未推送：{}",
        outcome.text
    );
    assert!(
        !outcome.text.contains("工作区与远端一致"),
        "不得误报一致：{}",
        outcome.text
    );
    // 冲突现场保留
    let unresolved = git_ok(&fix.repo, &["diff", "--name-only", "--diff-filter=U"]);
    assert!(
        unresolved.contains("shared.txt"),
        "冲突现场必须保留：{unresolved}"
    );
}

// 覆盖 G-04/G-06：暂存删除后重建同名文件会同时出现 D 与 ?? 记录；
// 不得把重建的工作区内容 add 回索引，覆盖用户已暂存的删除。
#[test]
fn staged_deletion_with_recreated_worktree_file_is_protected() {
    let fix = fixture();
    fs::write(fix.repo.join("guard.txt"), "original\n").unwrap();
    git_ok(&fix.repo, &["add", "guard.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "prepare"]);
    git_ok(&fix.repo, &["push", "-q"]);
    git_ok(&fix.repo, &["rm", "-q", "guard.txt"]);
    fs::write(fix.repo.join("guard.txt"), "replacement\n").unwrap();
    let before = git_ok(&fix.repo, &["status", "--porcelain=v1", "-z"]);
    assert!(
        before.contains("D  guard.txt\0") && before.contains("?? guard.txt\0"),
        "{before:?}"
    );
    let tree = git_ok(&fix.repo, &["write-tree"]);
    let head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let control = Control::default();
    let shared = Arc::new(GitShared::new());
    let outcome = git_tools::run(
        &git_exe(),
        &fix.repo,
        &control,
        &shared,
        &|line| {
            // 修复前会误提交 replacement；首次 push 结束即停止，避免第二条重复
            // 路径进入无限 commit 重试，并保留可复现的错误提交供断言定位。
            if line.starts_with("push 成功：") {
                control.cancel();
            }
        },
        &|_| {},
        TEST_UNIT,
    );
    assert!(outcome.contains("部分暂存状态受保护"), "{outcome}");
    assert_eq!(git_ok(&fix.repo, &["write-tree"]), tree, "保留已暂存的删除");
    assert_eq!(
        git_ok(&fix.repo, &["rev-parse", "HEAD"]),
        head,
        "不得提交重建文件"
    );
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        head,
        "不得推送"
    );
    assert_eq!(
        git_ok(&fix.repo, &["status", "--porcelain=v1", "-z"]),
        before
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("guard.txt")).unwrap(),
        "replacement\n"
    );
}

// 覆盖 G-04/G-06：已暂存 rename 的旧路径也属于 commit --only 范围；
// 重建旧路径不能被夹入 rename 提交，必须连同 staged rename 一并保护。
#[test]
fn staged_rename_with_recreated_source_is_protected() {
    let fix = fixture();
    fs::write(fix.repo.join("old.txt"), "original\n").unwrap();
    git_ok(&fix.repo, &["add", "old.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "prepare"]);
    git_ok(&fix.repo, &["push", "-q"]);
    git_ok(&fix.repo, &["mv", "old.txt", "new.txt"]);
    fs::write(fix.repo.join("old.txt"), "replacement\n").unwrap();
    let before = git_ok(&fix.repo, &["status", "--porcelain=v1", "-z"]);
    assert!(
        before.contains("R  new.txt\0old.txt\0") && before.contains("?? old.txt\0"),
        "{before:?}"
    );
    let tree = git_ok(&fix.repo, &["write-tree"]);
    let head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let control = Control::default();
    let shared = Arc::new(GitShared::new());
    let outcome = git_tools::run(
        &git_exe(),
        &fix.repo,
        &control,
        &shared,
        &|line| {
            if line.starts_with("push 成功：") {
                control.cancel();
            }
        },
        &|_| {},
        TEST_UNIT,
    );
    assert!(outcome.contains("部分暂存状态受保护"), "{outcome}");
    assert_eq!(
        git_ok(&fix.repo, &["write-tree"]),
        tree,
        "保留已暂存的 rename"
    );
    assert_eq!(
        git_ok(&fix.repo, &["rev-parse", "HEAD"]),
        head,
        "不得夹带重建的旧路径"
    );
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        head,
        "不得推送"
    );
    assert_eq!(
        git_ok(&fix.repo, &["status", "--porcelain=v1", "-z"]),
        before
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("old.txt")).unwrap(),
        "replacement\n"
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("new.txt")).unwrap(),
        "original\n"
    );
}
