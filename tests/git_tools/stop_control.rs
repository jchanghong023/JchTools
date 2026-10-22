use super::*;
// 覆盖 G-15（F13 回归：add 成功后同轮直接 commit——修复前停止检查只在循环顶，
// add 与 commit 之间没有检查；wrapper 在 add 边界暂停并请求停止后，add 自然
// 结束，不得再启动 commit）
#[test]
fn stop_between_add_and_commit_does_not_start_commit() {
    let fix = fixture();
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let wdir = tempfile::tempdir().unwrap();
    let wrapper = git_wrapper(wdir.path());
    hold(wdir.path(), "add");
    let control = Arc::new(Control::default());
    let (_logs, _shared, handle) = spawn_run(&wrapper, &fix.repo, &control);
    let add_index = wait_trace(wdir.path(), "add");
    control.cancel();
    release(wdir.path(), "add");
    let outcome = handle.join().unwrap();
    assert!(
        outcome.text.contains("已停止"),
        "收尾文案：{}",
        outcome.text
    );
    let lines = trace_lines(wdir.path());
    for line in &lines[add_index + 1..] {
        assert!(
            !line.starts_with("commit"),
            "add 之后不得再启动 commit（G-15）：{lines:?}"
        );
    }
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "不得产生本地提交"
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "远端不应收到任何提交"
    );
}

// 覆盖 G-15/G-13（F13：fetch 成功后立即 merge——停止请求发生在 fetch 执行期间
// 时，fetch 自然结束后不得启动 merge；同时 fetch 执行期间阶段名必须是独立的
// 「pull/fetch」）
#[test]
fn stop_between_fetch_and_merge_does_not_start_merge() {
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
    fs::write(fix.repo.join("local.txt"), "local change\n").unwrap();

    let wdir = tempfile::tempdir().unwrap();
    let wrapper = git_wrapper(wdir.path());
    hold(wdir.path(), "fetch");
    let control = Arc::new(Control::default());
    let (_logs, shared, handle) = spawn_run(&wrapper, &fix.repo, &control);
    let fetch_index = wait_trace(wdir.path(), "fetch");
    assert_eq!(
        shared.stage.lock().unwrap().as_str(),
        "pull/fetch",
        "fetch 执行期间阶段名必须是 pull/fetch（G-13）"
    );
    control.cancel();
    release(wdir.path(), "fetch");
    let outcome = handle.join().unwrap();
    assert!(
        outcome.text.contains("已停止"),
        "收尾文案：{}",
        outcome.text
    );
    let lines = trace_lines(wdir.path());
    for line in &lines[fetch_index + 1..] {
        assert!(
            !line.starts_with("merge"),
            "fetch 之后不得再启动 merge（G-15）：{lines:?}"
        );
    }
    assert!(
        !fix.repo.join(".git").join("MERGE_HEAD").exists(),
        "不得进入合并中间态"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "-1", "--format=%s"]).trim(),
        "update: local.txt",
        "已成功的 commit 不回滚（G-15）"
    );
    assert!(
        !remote_log(&fix.remote)
            .iter()
            .any(|s| s.starts_with("update: ")),
        "远端不应收到提交（push 已被拒且未重试成功）"
    );
}

// 覆盖 G-15/G-13（F13：merge 自然结束后不得启动下一次 push；同时 merge 执行
// 期间阶段名必须是独立的「merge」，不再笼统归入 pull/fetch）
#[test]
fn stop_after_merge_does_not_push_again_and_stage_is_merge() {
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
    fs::write(fix.repo.join("local.txt"), "local change\n").unwrap();

    let wdir = tempfile::tempdir().unwrap();
    let wrapper = git_wrapper(wdir.path());
    hold(wdir.path(), "merge");
    let control = Arc::new(Control::default());
    let (_logs, shared, handle) = spawn_run(&wrapper, &fix.repo, &control);
    let merge_index = wait_trace(wdir.path(), "merge");
    assert_eq!(
        shared.stage.lock().unwrap().as_str(),
        "merge",
        "merge 执行期间阶段名必须是独立的 merge（G-13）"
    );
    control.cancel();
    release(wdir.path(), "merge");
    let outcome = handle.join().unwrap();
    assert!(
        outcome.text.contains("已停止"),
        "收尾文案：{}",
        outcome.text
    );
    let lines = trace_lines(wdir.path());
    for line in &lines[merge_index + 1..] {
        assert!(
            !line.starts_with("push"),
            "merge 之后不得再启动 push（G-15）：{lines:?}"
        );
    }
    // 本地已形成合并提交（自然结束不回滚），但远端没收到任何东西
    assert_merge_tip(&fix.repo, "HEAD");
    assert!(
        !remote_log(&fix.remote)
            .iter()
            .any(|s| s.starts_with("update: ")),
        "远端不应收到本地提交"
    );
}

// 覆盖 G-13/G-14（F14 回归：成功与失败路径都必须把真实 stdout/stderr 采集进
// 日志——修复前成功分支丢弃真实输出（push 只在失败时可见输出、add/commit 只记
// 命令文本、fetch/merge 成功完全无输出且 merge 从不作为独立阶段）。wrapper 向
// 两条流写唯一标记逐一验证）
#[test]
fn real_output_captured_for_success_and_failure_paths() {
    // 成功路径：add/commit/push/fetch/merge 的真实输出都要进日志
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
    fs::write(fix.repo.join("local.txt"), "local change\n").unwrap();
    let wdir = tempfile::tempdir().unwrap();
    let wrapper = git_wrapper(wdir.path());
    let outcome = run_tool_with_control_and_git(&wrapper, &fix.repo, &Arc::new(Control::default()));
    assert!(
        outcome.text.contains("全部完成"),
        "成功路径收尾：{}",
        outcome.text
    );
    let entry = |names: &[&str], out_marker: &str, err_marker: &str| {
        outcome.logs.iter().find(|line| {
            names.iter().all(|n| line.contains(n))
                && line.contains(out_marker)
                && line.contains(err_marker)
        })
    };
    assert!(
        entry(&["git add"], "ADD-OUT-MARK", "ADD-ERR-MARK").is_some(),
        "git add 成功必须采集真实 stdout/stderr：{:#?}",
        outcome.logs
    );
    assert!(
        entry(&["git commit"], "COMMIT-OUT-MARK", "COMMIT-ERR-MARK").is_some(),
        "git commit 成功必须采集真实输出：{:#?}",
        outcome.logs
    );
    let push_outputs: Vec<&String> = outcome
        .logs
        .iter()
        .filter(|line| line.contains("git push 输出"))
        .collect();
    let push_attempts = trace_lines(wdir.path())
        .iter()
        .filter(|line| line.starts_with("push"))
        .count();
    assert!(
        push_attempts >= 2,
        "前置：远端领先必须先拒绝，再在合并后成功推送"
    );
    assert_eq!(
        push_outputs.len(),
        push_attempts,
        "每次真实 push（拒绝与成功）都必须采集输出：{:#?}",
        outcome.logs
    );
    assert!(
        push_outputs.iter().all(|line| {
            line.contains("stdout：")
                && line.contains("PUSH-OUT-MARK")
                && line.contains("stderr：")
                && line.contains("PUSH-ERR-MARK")
        }),
        "每次 push 都必须保留子进程实际发出的两条输出流：{:#?}",
        outcome.logs
    );
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]).trim(),
        git_ok(&fix.repo, &["rev-parse", "HEAD"]).trim(),
        "成功路径必须实际推送合并后的本地提交"
    );
    assert!(
        entry(&["git fetch"], "FETCH-OUT-MARK", "FETCH-ERR-MARK").is_some(),
        "git fetch 成功必须采集真实输出（独立日志条目）：{:#?}",
        outcome.logs
    );
    assert!(
        entry(&["git merge"], "MERGE-OUT-MARK", "MERGE-ERR-MARK").is_some(),
        "git merge 成功必须采集真实输出（独立日志条目）：{:#?}",
        outcome.logs
    );

    // 失败路径：真实 stdout/stderr 与失败原因必须都可见，不能用错误摘要替代输出。
    let fix2 = fixture();
    fs::write(fix2.repo.join("a.txt"), "x\n").unwrap();
    let bogus = fix2.repo.parent().unwrap().join("bogus.git");
    git_ok(
        &fix2.repo,
        &["remote", "set-url", "origin", &bogus.display().to_string()],
    );
    let wdir2 = tempfile::tempdir().unwrap();
    let wrapper2 = git_wrapper(wdir2.path());
    let control = Arc::new(Control::default());
    let (logs, _shared, handle) = spawn_run(&wrapper2, &fix2.repo, &control);
    wait_log_contains(&logs, "push 失败");
    control.cancel();
    let failed_outcome = handle.join().unwrap();
    let failed_logs = Arc::try_unwrap(logs).unwrap().into_inner().unwrap();
    assert!(
        failed_logs.iter().any(|line| {
            line.contains("git push")
                && line.contains("stdout：")
                && line.contains("PUSH-OUT-MARK")
                && line.contains("stderr：")
                && line.contains("PUSH-ERR-MARK")
                && line.contains("Could not read from remote repository.")
        }),
        "push 失败必须采集子进程真实的两条流和 Git 错误：{:#?}",
        failed_logs
    );
    assert!(
        failed_logs.iter().any(|line| {
            line.contains("push 失败") && line.contains("Could not read from remote repository.")
        }),
        "真实 push 失败必须报告可定位的原因：{:#?}",
        failed_logs
    );
    assert!(
        failed_outcome.text.contains("任务已停止"),
        "失败路径收尾：{}",
        failed_outcome.text
    );
}

// 覆盖 G-09/G-10/G-14：fetch 首败只重试 fetch；stdout/stderr 的实际内容不能被摘要丢弃。
#[test]
fn fetch_failure_retries_fetch_without_repeating_push() {
    let fix = fixture();
    remote_pushes_ahead(&fix.remote, "master", fix.repo.parent().unwrap());
    fs::write(fix.repo.join("local.txt"), "local\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wrapper = fail_once_wrapper(dir.path(), "fetch");
    let outcome = run_tool_with_control_and_git(&wrapper, &fix.repo, &Arc::new(Control::default()));
    assert!(outcome.text.contains("全部完成"), "{}", outcome.text);
    let phases: Vec<String> = trace_lines(dir.path())
        .into_iter()
        .filter(|line| {
            line.starts_with("push") || line.starts_with("fetch") || line.starts_with("merge")
        })
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect();
    assert_eq!(
        phases,
        ["push", "fetch", "fetch", "merge", "push"],
        "只重试失败阶段"
    );
    assert!(
        outcome.logs.iter().any(|line| {
            line.contains("git fetch 输出")
                && line.contains("TRANSIENT-OUT-MARK")
                && line.contains("transient stage failure")
        }),
        "fetch 失败必须保留两条真实输出流"
    );
    assert_eq!(
        git_ok(&fix.remote, &["show", "master:local.txt"]),
        "local\n"
    );
}

// 覆盖 G-09/G-10：merge 首败只重试 merge，不重复已成功的 fetch 或被拒绝的 push。
#[test]
fn merge_failure_retries_merge_without_repeating_fetch_or_push() {
    let fix = fixture();
    remote_pushes_ahead(&fix.remote, "master", fix.repo.parent().unwrap());
    fs::write(fix.repo.join("local.txt"), "local\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wrapper = fail_once_wrapper(dir.path(), "merge");
    let outcome = run_tool_with_control_and_git(&wrapper, &fix.repo, &Arc::new(Control::default()));
    assert!(outcome.text.contains("全部完成"), "{}", outcome.text);
    let phases: Vec<String> = trace_lines(dir.path())
        .into_iter()
        .filter(|line| {
            line.starts_with("push") || line.starts_with("fetch") || line.starts_with("merge")
        })
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect();
    assert_eq!(
        phases,
        ["push", "fetch", "merge", "merge", "push"],
        "只重试失败阶段"
    );
    assert_merge_tip(&fix.remote, "master");
    assert_eq!(
        git_ok(&fix.remote, &["show", "master:local.txt"]),
        "local\n"
    );
}

// 覆盖 G-15：已解决合并的范围检查结束后收到停止，不能再启动合并提交。
#[test]
fn stop_before_resolved_merge_commit_preserves_index_and_head() {
    let fix = fixture();
    fs::write(fix.repo.join("conflict.txt"), "local\n").unwrap();
    let other = fix.repo.parent().unwrap().join("other");
    git_ok(
        fix.repo.parent().unwrap(),
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    fs::write(other.join("conflict.txt"), "remote\n").unwrap();
    git_ok(&other, &["add", "conflict.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote conflict"]);
    git_ok(&other, &["push", "-q"]);
    assert!(run_tool(&fix.repo).text.contains("冲突"));
    fs::write(fix.repo.join("conflict.txt"), "resolved\n").unwrap();
    git_ok(&fix.repo, &["add", "conflict.txt"]);
    let tree = git_ok(&fix.repo, &["write-tree"]);
    let head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let remote = git_ok(&fix.remote, &["rev-parse", "master"]);
    let control = Control::default();
    let shared = Arc::new(GitShared::new());
    let outcome = git_tools::run(
        &git_exe(),
        &fix.repo,
        &control,
        &shared,
        &|line| {
            if line.contains("检测到已解决的合并") {
                control.cancel();
            }
        },
        &|_| {},
        TEST_UNIT,
    );
    assert!(control.is_cancelled(), "必须抵达提交前停止边界");
    assert!(outcome.contains("已停止"), "{outcome}");
    assert_eq!(git_ok(&fix.repo, &["write-tree"]), tree, "暂存树保留");
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD"]), head, "不得提交");
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        remote,
        "不得推送"
    );
    assert!(fix.repo.join(".git/MERGE_HEAD").is_file(), "保留合并现场");
}

// 覆盖 G-15：日志发送与命令启动之间的停止也有效，不能只在上一阶段结束时检查。
#[test]
fn stop_at_write_command_log_does_not_start_that_command() {
    for stage in ["add", "commit", "merge"] {
        let fix = fixture();
        fs::write(fix.repo.join("local.txt"), "local\n").unwrap();
        if stage == "merge" {
            remote_pushes_ahead(&fix.remote, "master", fix.repo.parent().unwrap());
        }
        let control = Control::default();
        let shared = Arc::new(GitShared::new());
        let outcome = git_tools::run(
            &git_exe(),
            &fix.repo,
            &control,
            &shared,
            &|line| {
                let stop = match stage {
                    "add" => line.starts_with("git add --"),
                    "commit" => line.starts_with("git commit --only -m"),
                    "merge" => line == "git merge FETCH_HEAD",
                    _ => unreachable!(),
                };
                if stop {
                    control.cancel();
                }
            },
            &|_| {},
            TEST_UNIT,
        );
        assert!(control.is_cancelled(), "必须抵达 {stage} 命令启动边界");
        assert!(outcome.contains("已停止"), "{stage}: {outcome}");
        assert_eq!(
            fs::read_to_string(fix.repo.join("local.txt")).unwrap(),
            "local\n"
        );
        if stage == "merge" {
            assert_eq!(
                git_ok(&fix.repo, &["log", "-1", "--format=%s"]).trim(),
                "update: local.txt",
                "停止后不能形成合并提交"
            );
            assert!(
                !fix.repo.join("remote-side.txt").exists(),
                "不能落盘合并结果"
            );
            assert!(!fix.repo.join(".git/MERGE_HEAD").exists(), "不能启动合并");
        } else {
            assert_eq!(
                git_ok(&fix.repo, &["log", "-1", "--format=%s"]).trim(),
                "init"
            );
            if stage == "add" {
                assert!(
                    git_ok(&fix.repo, &["status", "--porcelain"]).contains("?? local.txt"),
                    "add 尚未启动，文件应保持未跟踪"
                );
            } else {
                assert_eq!(
                    git_ok(&fix.repo, &["show", ":local.txt"]),
                    "local\n",
                    "成功的 add 保留，不回滚"
                );
            }
        }
        assert!(
            !remote_log(&fix.remote)
                .iter()
                .any(|s| s.starts_with("update: ")),
            "停止前未成功 push，远端不得收到本地文件"
        );
    }
}

// 覆盖 G-09/G-14：add/commit 首败后只重试当前阶段，两条真实输出流都必须
// 在界面日志可见；只保留 stderr 摘要会丢失 stdout 的诊断信息。
#[test]
fn add_and_commit_failures_preserve_both_output_streams() {
    for stage in ["add", "commit"] {
        let fix = fixture();
        fs::write(fix.repo.join("local.txt"), "local\n").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wrapper = fail_once_wrapper(dir.path(), stage);
        let outcome =
            run_tool_with_control_and_git(&wrapper, &fix.repo, &Arc::new(Control::default()));
        assert!(
            outcome.text.contains("全部完成"),
            "{stage}: {}",
            outcome.text
        );
        let label = format!("git {stage} 输出");
        assert!(
            outcome.logs.iter().any(|line| {
                line.contains(&label)
                    && line.contains("stdout：")
                    && line.contains("TRANSIENT-OUT-MARK")
                    && line.contains("stderr：")
                    && line.contains("transient stage failure")
            }),
            "{stage} 失败必须保留两条真实输出流：{:#?}",
            outcome.logs
        );
        let phases: Vec<String> = trace_lines(dir.path())
            .into_iter()
            .filter(|line| {
                line.starts_with("add") || line.starts_with("commit") || line.starts_with("push")
            })
            .map(|line| line.split_whitespace().next().unwrap().to_owned())
            .collect();
        let expected = if stage == "add" {
            vec!["add", "add", "commit", "push"]
        } else {
            vec!["add", "commit", "commit", "push"]
        };
        assert_eq!(phases, expected, "只重试当前失败阶段");
        assert_eq!(
            git_ok(&fix.remote, &["show", "master:local.txt"]),
            "local\n"
        );
    }
}
