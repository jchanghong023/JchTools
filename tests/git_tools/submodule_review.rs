use super::*;
// 覆盖 P-03/G-08：配置递归推送也不能访问当前分支upstream之外的子模块远端。
#[test]
fn recursive_push_does_not_update_submodule_remote() {
    let (fix, subremote) = fixture_with_submodule();
    let before = git_ok(&subremote, &["rev-parse", "master"]);
    let child = fix.repo.join("child");
    fs::write(child.join("new.txt"), "submodule change\n").unwrap();
    git_ok(&child, &["add", "new.txt"]);
    git_ok(&child, &["commit", "-q", "-m", "submodule pending"]);
    git_ok(
        &fix.repo,
        &["config", "push.recurseSubmodules", "on-demand"],
    );

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "当前upstream的gitlink更新应正常推送：{}",
        outcome.text
    );
    assert_eq!(
        git_ok(&subremote, &["rev-parse", "master"]),
        before,
        "不得递归推送到upstream之外的子模块远端"
    );
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master:child"]).trim(),
        git_ok(&child, &["rev-parse", "HEAD"]).trim(),
        "只推送superproject中的gitlink更新"
    );
}

// 覆盖 P-03/G-10：fetch不能因递归配置而访问子模块自己的远端。
#[test]
fn recursive_fetch_does_not_update_submodule_tracking_ref() {
    let (fix, subremote) = fixture_with_submodule();
    let child = fix.repo.join("child");
    let before = git_ok(&child, &["rev-parse", "origin/master"]);
    let base = fix.repo.parent().unwrap();
    git_ok(
        base,
        &[
            "clone",
            "-q",
            &subremote.display().to_string(),
            "other-child",
        ],
    );
    let other_child = base.join("other-child");
    fs::write(other_child.join("new.txt"), "remote submodule change\n").unwrap();
    git_ok(&other_child, &["add", "new.txt"]);
    git_ok(&other_child, &["commit", "-q", "-m", "remote submodule"]);
    git_ok(&other_child, &["push", "-q"]);
    let new_oid = git_ok(&other_child, &["rev-parse", "HEAD"]);
    git_ok(
        base,
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    let other = base.join("other");
    git_ok(
        &other,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{},child", new_oid.trim()),
        ],
    );
    git_ok(&other, &["commit", "-q", "-m", "remote gitlink"]);
    git_ok(&other, &["push", "-q"]);
    fs::write(fix.repo.join("local.txt"), "local change\n").unwrap();
    git_ok(&fix.repo, &["config", "fetch.recurseSubmodules", "true"]);

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "当前upstream的新提交应正常合并推送：{}",
        outcome.text
    );
    assert_eq!(
        git_ok(&child, &["rev-parse", "origin/master"]),
        before,
        "不得递归fetch子模块自己的远端"
    );
    assert!(
        remote_log(&fix.remote).contains(&"update: local.txt".to_string()),
        "superproject当前文件应已推送"
    );
}

// F23 / G-11、G-13、G-16：merge 的退出码 0 不能掩盖 autostash 恢复时的真实冲突。
#[test]
fn review_f23_successful_autostash_merge_stops_before_push_on_unmerged_index() {
    let fix = fixture();
    fs::write(fix.repo.join("z.txt"), "base z\n").unwrap();
    git_ok(&fix.repo, &["add", "z.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "base z"]);
    git_ok(&fix.repo, &["push", "-q"]);
    git_ok(&fix.repo, &["config", "merge.autoStash", "true"]);
    let base = fix.repo.parent().unwrap();
    git_ok(
        base,
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    let other = base.join("other");
    fs::write(other.join("z.txt"), "remote z\n").unwrap();
    git_ok(&other, &["add", "z.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote z"]);
    git_ok(&other, &["push", "-q"]);
    let remote_before = git_ok(&fix.remote, &["rev-parse", "master"]);
    fs::write(fix.repo.join("a.txt"), "local a\n").unwrap();
    fs::write(fix.repo.join("z.txt"), "local dirty z\n").unwrap();
    // porcelain 先列已跟踪变更再列未跟踪项：必须先暂存 a，真实工具才会先提交 a，
    // 留下 dirty z 进入 autostash，而不是先提交 z 导致冲突前提消失。
    git_ok(&fix.repo, &["add", "--", "a.txt"]);
    let initial_status = git_ok(&fix.repo, &["status", "--porcelain=v1", "-z"]);
    assert_eq!(
        initial_status, "A  a.txt\0 M z.txt\0",
        "真实被测仓库必须先处理 a，且 z 仍只在工作区修改"
    );

    // 独立副本继承真实工具完全相同的索引和工作区，证明本用例确实具备
    // “merge 成功但索引未合并”的前提；不替换退出码或只调整副本的 status 次序。
    let probe = base.join("autostash-probe");
    copy_tree(&fix.repo, &probe);
    assert_eq!(
        git_ok(&probe, &["status", "--porcelain=v1", "-z"]),
        initial_status,
        "独立 probe 必须与真实工具共享相同的启动前提"
    );
    git_ok(
        &probe,
        &[
            "commit",
            "--only",
            "-q",
            "-m",
            "probe local a",
            "--",
            "a.txt",
        ],
    );
    git_ok(&probe, &["fetch", "-q", "origin", "master"]);
    git_ok(&probe, &["merge", "FETCH_HEAD"]);
    assert_eq!(
        git_ok(&probe, &["diff", "--name-only", "--diff-filter=U", "-z"]),
        "z.txt\0",
        "退出 0 的真实 merge 必须留下 autostash 冲突"
    );

    let outcome = review_run_with_retry_limit(&fix.repo, 1);
    assert_eq!(
        git_ok(&fix.repo, &["diff", "--name-only", "--diff-filter=U", "-z"]),
        "z.txt\0"
    );
    let content = fs::read_to_string(fix.repo.join("z.txt")).unwrap();
    assert!(content.contains("local dirty z") && content.contains("remote z"));
    assert_eq!(
        outcome.shared.state.lock().unwrap().as_str(),
        "冲突",
        "必须在恢复 autostash 后立即报告冲突：{}",
        outcome.text
    );
    assert_eq!(outcome.shared.stage.lock().unwrap().as_str(), "冲突");
    assert_eq!(outcome.shared.current.lock().unwrap().as_str(), "a.txt");
    assert_eq!(
        outcome
            .shared
            .done
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "冲突后的当前文件不得被计为已完成"
    );
    assert!(outcome.text.contains("z.txt"), "必须列出真实冲突路径");
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        remote_before,
        "autostash 恢复冲突后不得继续 push 合并提交"
    );
}

// F24 / G-09、G-12、U-13：父仓库无法提交子模块内部已跟踪文件修改。
#[test]
fn review_f24_submodule_modified_content_without_head_change_is_not_retried() {
    review_assert_dirty_submodule_is_not_retried(false);
}

// F24 / G-09、G-12、U-13：porcelain v1 同样折叠子模块内部未跟踪内容。
#[test]
fn review_f24_submodule_untracked_content_without_head_change_is_not_retried() {
    review_assert_dirty_submodule_is_not_retried(true);
}

// F25 / G-03、G-04、G-06、G-09：删除 slot 不得递归暂存另一个逻辑变更 slot/new.txt。
#[test]
fn review_f25_file_replaced_by_directory_does_not_stage_child_for_parent_deletion() {
    let fix = fixture();
    fs::write(fix.repo.join("slot"), "old tracked file\n").unwrap();
    git_ok(&fix.repo, &["add", "slot"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "prepare slot"]);
    git_ok(&fix.repo, &["push", "-q"]);
    fs::remove_file(fix.repo.join("slot")).unwrap();
    fs::create_dir(fix.repo.join("slot")).unwrap();
    fs::write(fix.repo.join("slot/new.txt"), "different new child\n").unwrap();
    fs::write(fix.repo.join("zz-user.txt"), "user staged content\n").unwrap();
    git_ok(&fix.repo, &["add", "zz-user.txt"]);
    let initial_tree = git_ok(&fix.repo, &["write-tree"]);
    let initial_head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let user_blob = git_ok(&fix.repo, &["rev-parse", ":zz-user.txt"]);
    let control = Arc::new(Control::default());
    let shared = Arc::new(GitShared::new());
    let first_deletion_index = std::cell::RefCell::new(None::<String>);
    let _watchdog = RunWatchdog::new(&control);
    let text = git_tools::run(
        &git_exe(),
        &fix.repo,
        &control,
        &shared,
        &|_| {
            // 在真实 add 已结束、当前删除的 commit 尚未开始时读取真实索引。
            // 不断言调用串或固定实现顺序；提前安全拒绝也满足本安全回归。
            if shared.stage.lock().unwrap().as_str() == "commit"
                && shared.current.lock().unwrap().as_str() == "slot"
                && first_deletion_index.borrow().is_none()
            {
                *first_deletion_index.borrow_mut() = Some(git_ok(
                    &fix.repo,
                    &["diff", "--cached", "--name-only", "-z"],
                ));
                assert_eq!(
                    git_ok(&fix.repo, &["rev-parse", ":zz-user.txt"]),
                    user_blob,
                    "当前文件操作不得修改原有无关 staged 内容"
                );
            }
            if shared.retry.load(std::sync::atomic::Ordering::Relaxed) >= 1 {
                control.cancel();
            }
        },
        &|_| {},
        TEST_UNIT,
    );
    if let Some(index) = first_deletion_index.into_inner() {
        assert!(
            !index.split('\0').any(|path| path == "slot/new.txt"),
            "删除 slot 的 commit 前已提前暂存另一个逻辑文件：{index:?}；{text}"
        );
    }
    assert_eq!(
        shared.retry.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "结构性文件/目录冲突不应无限重试：{text}"
    );
    if git_ok(&fix.repo, &["rev-parse", "HEAD"]) == initial_head {
        assert_eq!(
            git_ok(&fix.repo, &["write-tree"]),
            initial_tree,
            "若安全拒绝且未提交，必须完整保留启动前索引"
        );
    } else {
        assert_eq!(shared.state.lock().unwrap().as_str(), "完成");
        let subjects = remote_log(&fix.remote);
        assert!(subjects.contains(&"update: slot".to_owned()));
        assert!(subjects.contains(&"update: slot/new.txt".to_owned()));
    }
    assert_eq!(
        fs::read_to_string(fix.repo.join("slot/new.txt")).unwrap(),
        "different new child\n"
    );
}

// F26 / G-09、G-11、G-12：续接已解决 merge 时，一次真实 hook 失败必须留在当前阶段重试。
#[test]
fn review_f26_resolved_merge_retries_one_shot_commit_hook_failure() {
    let fix = review_remediation_chinese_conflict_fixture();
    let first = run_tool(&fix.repo);
    assert_eq!(first.shared.state.lock().unwrap().as_str(), "冲突");
    fs::write(fix.repo.join("冲突 文件.txt"), "resolved version\n").unwrap();
    git_ok(&fix.repo, &["add", "冲突 文件.txt"]);
    let resolved_tree = git_ok(&fix.repo, &["write-tree"]);
    let local_parent = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let remote_parent = git_ok(&fix.remote, &["rev-parse", "master"]);
    let hooks = fix.repo.join(".git/review-f26-hooks");
    fs::create_dir(&hooks).unwrap();
    fs::write(
        hooks.join("pre-commit"),
        concat!(
            "#!/bin/sh\n",
            "marker=.git/review-f26-hook-attempts\n",
            "if [ ! -f \"$marker\" ]; then\n",
            "  printf 'first\\n' > \"$marker\"\n",
            "  printf 'one-shot review hook failure\\n' >&2\n",
            "  exit 1\n",
            "fi\n",
            "printf 'retry\\n' >> \"$marker\"\n",
            "exit 0\n"
        ),
    )
    .unwrap();
    git_ok(
        &fix.repo,
        &["config", "core.hooksPath", &hooks.display().to_string()],
    );

    let outcome = review_run_with_retry_limit(&fix.repo, 3);
    assert_eq!(
        outcome.shared.state.lock().unwrap().as_str(),
        "完成",
        "一次可恢复 hook 失败不应要求用户重启：{}",
        outcome.text
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join(".git/review-f26-hook-attempts")).unwrap(),
        "first\nretry\n",
        "应重试同一合并提交且成功后不再重复执行 hook"
    );
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD^1"]), local_parent);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD^2"]), remote_parent);
    assert_eq!(
        git_ok(&fix.repo, &["rev-parse", "HEAD^{tree}"]),
        resolved_tree,
        "重试不得改动已解决的暂存结果"
    );
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        git_ok(&fix.repo, &["rev-parse", "HEAD"]),
        "合并提交成功后必须继续 push"
    );
    assert!(!fix.repo.join(".git/MERGE_HEAD").exists());
    assert!(worktree_clean(&fix.repo));
}

// F26 / G-15：可恢复提交失败的等待可停止；重新启动保留已解决树，不重新 add/merge。
#[test]
fn review_f26_stopping_merge_commit_retry_preserves_resolved_index_for_restart() {
    let fix = review_remediation_chinese_conflict_fixture();
    let first = run_tool(&fix.repo);
    assert_eq!(first.shared.state.lock().unwrap().as_str(), "冲突");
    fs::write(fix.repo.join("冲突 文件.txt"), "resolved for restart\n").unwrap();
    git_ok(&fix.repo, &["add", "冲突 文件.txt"]);
    let tree = git_ok(&fix.repo, &["write-tree"]);
    let head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let remote = git_ok(&fix.remote, &["rev-parse", "master"]);
    let merge_head = git_ok(&fix.repo, &["rev-parse", "MERGE_HEAD"]);
    let hooks = fix.repo.join(".git/review-f26-stop-hooks");
    fs::create_dir(&hooks).unwrap();
    fs::write(
        hooks.join("pre-commit"),
        "#!/bin/sh\nprintf 'attempt\\n' >> .git/review-f26-stop-attempts\nexit 1\n",
    )
    .unwrap();
    git_ok(
        &fix.repo,
        &["config", "core.hooksPath", &hooks.display().to_string()],
    );

    let stopped = review_run_with_retry_limit(&fix.repo, 1);
    assert_eq!(stopped.shared.state.lock().unwrap().as_str(), "已停止");
    assert_eq!(
        stopped
            .shared
            .retry
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join(".git/review-f26-stop-attempts")).unwrap(),
        "attempt\n",
        "收到停止后不得启动第二次 hook 或提交"
    );
    assert_eq!(git_ok(&fix.repo, &["write-tree"]), tree);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "MERGE_HEAD"]), merge_head);
    assert_eq!(git_ok(&fix.remote, &["rev-parse", "master"]), remote);

    fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
    let resumed = review_run_with_retry_limit(&fix.repo, 1);
    assert_eq!(resumed.shared.state.lock().unwrap().as_str(), "完成");
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD^1"]), head);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD^2"]), merge_head);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD^{tree}"]), tree);
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        git_ok(&fix.repo, &["rev-parse", "HEAD"])
    );
    assert!(worktree_clean(&fix.repo));
}

// F26 / G-09：合并提交成功后遇到真实远端 hook 拒绝，只能重试 push，不能重做 commit。
#[test]
fn review_f26_merge_commit_success_is_remembered_during_push_retry() {
    let fix = review_remediation_chinese_conflict_fixture();
    let first = run_tool(&fix.repo);
    assert_eq!(first.shared.state.lock().unwrap().as_str(), "冲突");
    fs::write(
        fix.repo.join("冲突 文件.txt"),
        "resolved before push retry\n",
    )
    .unwrap();
    git_ok(&fix.repo, &["add", "冲突 文件.txt"]);
    let tree = git_ok(&fix.repo, &["write-tree"]);
    let head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let merge_head = git_ok(&fix.repo, &["rev-parse", "MERGE_HEAD"]);
    let hooks = fix.repo.join(".git/review-f26-push-hooks");
    fs::create_dir(&hooks).unwrap();
    fs::write(
        hooks.join("pre-commit"),
        "#!/bin/sh\nprintf 'commit\\n' >> .git/review-f26-commit-attempts\nexit 0\n",
    )
    .unwrap();
    git_ok(
        &fix.repo,
        &["config", "core.hooksPath", &hooks.display().to_string()],
    );
    fs::write(
        fix.remote.join("hooks/pre-receive"),
        concat!(
            "#!/bin/sh\n",
            "cat >/dev/null\n",
            "marker=review-f26-push-attempts\n",
            "if [ ! -f \"$marker\" ]; then\n",
            "  printf 'first\\n' > \"$marker\"\n",
            "  printf 'one-shot receive failure\\n' >&2\n",
            "  exit 1\n",
            "fi\n",
            "printf 'retry\\n' >> \"$marker\"\n",
            "exit 0\n"
        ),
    )
    .unwrap();

    let outcome = review_run_with_retry_limit(&fix.repo, 3);
    assert_eq!(outcome.shared.state.lock().unwrap().as_str(), "完成");
    assert_eq!(
        outcome
            .shared
            .retry
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join(".git/review-f26-commit-attempts")).unwrap(),
        "commit\n",
        "push 重试不得再次调用合并提交 hook"
    );
    assert_eq!(
        fs::read_to_string(fix.remote.join("review-f26-push-attempts")).unwrap(),
        "first\nretry\n"
    );
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD^1"]), head);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD^2"]), merge_head);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD^{tree}"]), tree);
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        git_ok(&fix.repo, &["rev-parse", "HEAD"])
    );
    assert!(worktree_clean(&fix.repo));
}
