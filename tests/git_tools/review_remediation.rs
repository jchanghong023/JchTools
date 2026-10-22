use super::*;
// P2-01/G-09：工作区还有无关文件时，merge 暂时失败仍只能重试 merge；
// 不能仅凭 status 非空就宣称这些文件阻挡合并。
#[cfg(windows)]
#[test]
fn review_remediation_unrelated_dirty_file_does_not_block_merge_retry() {
    let fix = fixture();
    remote_pushes_ahead(&fix.remote, "master", fix.repo.parent().unwrap());
    fs::write(fix.repo.join("a.txt"), "first local file\n").unwrap();
    fs::write(fix.repo.join("z.txt"), "unrelated local file\n").unwrap();
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
    assert_eq!(phases, ["push", "fetch", "merge", "merge", "push", "push"]);
    assert_eq!(
        git_ok(&fix.remote, &["show", "master:z.txt"]),
        "unrelated local file\n"
    );
}

// P2-01/G-09/G-10：z 只在本地历史中变化，远端并未触碰它；
// HEAD 与 FETCH_HEAD 的双边差异不能把 z 的后续 dirty 误判为 merge 阻挡。
#[cfg(windows)]
#[test]
fn review_remediation_local_only_committed_dirty_file_retries_merge() {
    let fix = fixture();
    // a 必须先进入共同历史，随后与 z 同为 tracked dirty；否则 Git 会先列
    // tracked z、后列 untracked a，无法覆盖“提交 a 时保留 dirty z”的路径。
    fs::write(fix.repo.join("a.txt"), "shared committed a\n").unwrap();
    git_ok(&fix.repo, &["add", "a.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "shared a"]);
    git_ok(&fix.repo, &["push", "-q"]);
    let common = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    fs::write(fix.repo.join("z.txt"), "local committed z\n").unwrap();
    git_ok(&fix.repo, &["add", "z.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "local-only z"]);
    let local_z_commit = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let committed_z = git_ok(&fix.repo, &["show", "HEAD:z.txt"]);
    remote_pushes_ahead(&fix.remote, "master", fix.repo.parent().unwrap());
    assert_eq!(
        git_ok(
            &fix.repo,
            &["diff", "--name-only", common.trim(), "HEAD", "--"]
        ),
        "z.txt\n",
        "必须存在仅本地已提交的 z 差异"
    );
    assert_eq!(
        git_ok(
            &fix.remote,
            &["diff", "--name-only", common.trim(), "master", "--"]
        ),
        "remote-side.txt\n",
        "远端只改另一文件，不得触碰 z"
    );
    let dirty_z = b"local committed z, then edited again\n";
    fs::write(fix.repo.join("z.txt"), dirty_z).unwrap();
    fs::write(fix.repo.join("a.txt"), "first local file\n").unwrap();
    assert_eq!(
        git_ok(&fix.repo, &["status", "--porcelain", "--", "z.txt"]).trim(),
        "M z.txt",
        "z 必须是 tracked 文件再次修改，而非无关 untracked 文件"
    );
    assert_eq!(
        git_ok(
            &fix.repo,
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        " M a.txt\n M z.txt\n",
        "运行工具前必须证明 a、z 都是 tracked dirty，且真实 status 先列 a 再列 z"
    );
    let dir = tempfile::tempdir().unwrap();
    let wrapper = fail_once_wrapper(dir.path(), "merge");
    let outcome = run_tool_with_control_and_git(&wrapper, &fix.repo, &Arc::new(Control::default()));
    assert!(
        outcome.text.contains("全部完成"),
        "临时 merge 失败不能误归 dirty：{}",
        outcome.text
    );
    let phases: Vec<String> = trace_lines(dir.path())
        .into_iter()
        .filter(|line| {
            line.starts_with("push") || line.starts_with("fetch") || line.starts_with("merge")
        })
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect();
    assert_eq!(
        phases,
        ["push", "fetch", "merge", "merge", "push", "push"],
        "仅首个 merge 临时失败一次，只重试 merge"
    );
    assert!(dir.path().join("failed.flag").is_file());
    assert_eq!(
        outcome
            .logs
            .iter()
            .filter(|line| line.contains("git merge 输出") && line.contains("TRANSIENT-OUT-MARK"))
            .count(),
        1,
        "首败一次后必须透传真实 Git"
    );
    assert_eq!(
        fs::read(fix.repo.join("z.txt")).unwrap(),
        dirty_z,
        "z 原始工作树字节不得被合并或逐文件提交改写"
    );
    assert_eq!(
        git_ok(&fix.remote, &["show", "master:z.txt"]).as_bytes(),
        dirty_z
    );
    assert_eq!(
        git_ok(&fix.remote, &["show", "master:a.txt"]),
        "first local file\n"
    );
    assert_eq!(
        git_ok(&fix.remote, &["log", "-1", "--format=%s"]).trim(),
        "update: z.txt"
    );
    assert_eq!(head_paths(&fix.remote), ["z.txt"]);
    assert_merge_tip(&fix.remote, "master^");
    assert_eq!(
        git_ok(&fix.remote, &["show", "master^:z.txt"]),
        committed_z,
        "合并完成时仍只能包含 z 原已提交内容"
    );
    assert_eq!(
        git_ok(&fix.remote, &["log", "-1", "--format=%s", "master^^"]).trim(),
        "update: a.txt",
        "a 必须先独立提交"
    );
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master^^^"]),
        local_z_commit,
        "保留本地独有提交的祖先关系"
    );
    assert_eq!(
        git_ok(&fix.repo, &["rev-parse", "HEAD"]),
        git_ok(&fix.remote, &["rev-parse", "master"])
    );
    assert!(worktree_clean(&fix.repo));
}

// P2-01/G-04/G-06：本地化 pathspec 失败不能阻止已经完整暂存的删除提交。
#[cfg(windows)]
#[test]
fn review_remediation_localized_prestaged_deletion_is_pushed() {
    let fix = fixture();
    fs::write(fix.repo.join("gone.txt"), "remove me\n").unwrap();
    git_ok(&fix.repo, &["add", "gone.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "prepare deletion"]);
    git_ok(&fix.repo, &["rm", "-q", "gone.txt"]);
    let dir = tempfile::tempdir().unwrap();
    let wrapper = review_remediation_localized_git(
        dir.path(),
        "add",
        "致命错误：路径规格 'gone.txt' 未匹配任何文件\n",
    );
    let outcome = review_remediation_run_without_retry(&wrapper, &fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "合法删除必须完成而非重试：{}",
        outcome.text
    );
    assert!(worktree_clean(&fix.repo));
    assert_eq!(
        git_ok(&fix.remote, &["log", "-1", "--format=%s"]).trim(),
        "update: gone.txt"
    );
    assert!(!git_ok(&fix.remote, &["ls-tree", "--name-only", "master"]).contains("gone.txt"));
}

// P2-01/G-10：真实 merge 被未提交内容阻挡，诊断语言不能把确定性阻挡降为重试。
#[cfg(windows)]
#[test]
fn review_remediation_localized_dirty_merge_stops_with_reason() {
    let fix = fixture();
    fs::write(fix.repo.join("f.txt"), "local f\n").unwrap();
    fs::write(fix.repo.join("g.txt"), "local dirty g\n").unwrap();
    let base = fix.repo.parent().unwrap();
    git_ok(
        base,
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    let other = base.join("other");
    fs::write(other.join("g.txt"), "remote g\n").unwrap();
    git_ok(&other, &["add", "g.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote g"]);
    git_ok(&other, &["push", "-q"]);
    let remote_before = git_ok(&fix.remote, &["rev-parse", "master"]);
    let dir = tempfile::tempdir().unwrap();
    let wrapper = review_remediation_localized_git(
        dir.path(),
        "merge",
        "错误：您对下列文件的本地修改将被合并操作覆盖：\n\tg.txt\n请在合并前提交或贮藏您的修改。\n",
    );
    let outcome = review_remediation_run_without_retry(&wrapper, &fix.repo);
    assert!(
        outcome.text.contains("本地未提交变更阻挡"),
        "必须说明阻挡而非等待重试：{}",
        outcome.text
    );
    assert_eq!(
        fs::read_to_string(fix.repo.join("g.txt")).unwrap(),
        "local dirty g\n"
    );
    assert_eq!(git_ok(&fix.remote, &["rev-parse", "master"]), remote_before);
    assert!(!fix.repo.join(".git/MERGE_HEAD").exists());
}

// P2-01/G-10：assume-unchanged 隐藏 tracked dirty；真实 merge 的失败回滚可能改写它。
// 必须保留索引标记，在危险 merge 启动前核对并停止，而不是失败后才检查内容。
#[cfg(windows)]
#[test]
fn review_remediation_assume_unchanged_dirty_merge_stops_without_retry() {
    let fix = fixture();
    fs::write(fix.repo.join("z.txt"), "shared committed z\n").unwrap();
    git_ok(&fix.repo, &["add", "z.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "shared z"]);
    git_ok(&fix.repo, &["push", "-q"]);
    let local_before = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let committed_z = git_ok(&fix.repo, &["show", "HEAD:z.txt"]);
    let base = fix.repo.parent().unwrap();
    git_ok(
        base,
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    let other = base.join("other");
    fs::write(other.join("z.txt"), "remote replacement z\n").unwrap();
    git_ok(&other, &["add", "z.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote z"]);
    git_ok(&other, &["push", "-q"]);
    let remote_before = git_ok(&fix.remote, &["rev-parse", "master"]);
    git_ok(
        &fix.repo,
        &["update-index", "--assume-unchanged", "--", "z.txt"],
    );
    let dirty_z = b"hidden local dirty z must survive unchanged\n";
    fs::write(fix.repo.join("z.txt"), dirty_z).unwrap();
    fs::write(fix.repo.join("a.txt"), "first local file\n").unwrap();
    let index_z = git_ok(&fix.repo, &["ls-files", "--stage", "-z", "--", "z.txt"]);
    let flags_z = git_ok(&fix.repo, &["ls-files", "-v", "--", "z.txt"]);
    assert_eq!(flags_z, "h z.txt\n", "必须真实设置 assume-unchanged");
    assert_eq!(
        git_ok(
            &fix.repo,
            &["status", "--porcelain", "--untracked-files=all"]
        ),
        "?? a.txt\n",
        "status 必须隐藏真实存在的 tracked z 修改"
    );
    assert_ne!(
        fs::read(fix.repo.join("z.txt")).unwrap(),
        committed_z.as_bytes()
    );
    let dir = tempfile::tempdir().unwrap();
    let wrapper =
        review_remediation_localized_git(dir.path(), "merge", "合并无法继续：本地内容需要保留。\n");
    let outcome = review_remediation_run_without_retry(&wrapper, &fix.repo);
    let trace = trace_lines(dir.path());
    assert!(
        !outcome
            .logs
            .iter()
            .any(|line| line.contains("次失败：等待")),
        "隐藏内容保护必须直接停止，不能到首次 retry 取消边界：{}\nlogs={:#?}\nwrapper trace={:#?}",
        outcome.text,
        outcome.logs,
        trace
    );
    assert!(
        outcome.text.contains("本地未提交变更阻挡"),
        "必须识别隐藏的 dirty 阻挡：{}\nlogs={:#?}\nwrapper trace={:#?}",
        outcome.text,
        outcome.logs,
        trace
    );
    assert!(
        !dir.path().join("actual-out.txt").exists() && !dir.path().join("actual-err.txt").exists(),
        "必须在危险 merge 的真实执行边界前停止，不能依赖失败后的内容检查"
    );
    let phases: Vec<String> = trace
        .into_iter()
        .filter(|line| {
            line.starts_with("push") || line.starts_with("fetch") || line.starts_with("merge")
        })
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect();
    assert_eq!(
        phases,
        ["push", "fetch"],
        "保护原始隐藏内容时不得启动危险 merge、retry 或再次 push"
    );
    assert_eq!(fs::read(fix.repo.join("z.txt")).unwrap(), dirty_z);
    assert_eq!(
        git_ok(&fix.repo, &["ls-files", "--stage", "-z", "--", "z.txt"]),
        index_z,
        "保留 z 的原始索引条目"
    );
    assert_eq!(
        git_ok(&fix.repo, &["ls-files", "-v", "--", "z.txt"]),
        flags_z,
        "不得清除索引标记来使测试通过"
    );
    assert_eq!(
        git_ok(&fix.repo, &["show", "HEAD:z.txt"]),
        committed_z,
        "不得误提交隐藏的 z 修改"
    );
    assert_eq!(
        git_ok(&fix.repo, &["rev-parse", "HEAD^"]),
        local_before,
        "只允许先提交 a"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "-1", "--format=%s"]).trim(),
        "update: a.txt"
    );
    assert_eq!(head_paths(&fix.repo), ["a.txt"]);
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        remote_before,
        "不得覆盖远端"
    );
    assert_eq!(
        git_ok(&fix.remote, &["show", "master:z.txt"]),
        "remote replacement z\n"
    );
    assert!(!fix.repo.join(".git/MERGE_HEAD").exists());
    assert!(!fix.repo.join(".git/index.lock").exists());
}

// P2-01/G-11：中文 merge 诊断下仍须用真实未合并索引识别冲突，保留双方内容。
#[cfg(windows)]
#[test]
fn review_remediation_localized_merge_conflict_is_not_retried() {
    let fix = review_remediation_chinese_conflict_fixture();
    let dir = tempfile::tempdir().unwrap();
    let wrapper = review_remediation_localized_git(
        dir.path(),
        "merge",
        "冲突（添加/添加）：合并冲突于 冲突 文件.txt\n自动合并失败，修正冲突然后提交修正的结果。\n",
    );
    let outcome = review_remediation_run_without_retry(&wrapper, &fix.repo);
    assert_eq!(
        outcome.shared.state.lock().unwrap().as_str(),
        "冲突",
        "{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("自动合并出现冲突"),
        "{}",
        outcome.text
    );
    review_remediation_assert_conflict_preserved(&fix);
}

// P3-10/G-11/G-14：当前文件名本身已在摘要中，必须断言独立的冲突列表而非 contains。
#[test]
fn review_remediation_first_merge_conflict_lists_original_chinese_path() {
    let fix = review_remediation_chinese_conflict_fixture();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("冲突文件 1 个（冲突 文件.txt）"),
        "冲突列表须为原文：{}",
        outcome.text
    );
    review_remediation_assert_conflict_preserved(&fix);
}

// P3-10/G-11：第二次启动走 middle_state，不能继续把 quotePath 的八进制引号暴露给用户。
#[test]
fn review_remediation_restart_conflict_lists_original_chinese_path() {
    let fix = review_remediation_chinese_conflict_fixture();
    let first = run_tool(&fix.repo);
    assert!(first.text.contains("冲突"), "{}", first.text);
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("1 个文件：冲突 文件.txt）"),
        "重启冲突列表须为原文：{}",
        outcome.text
    );
    review_remediation_assert_conflict_preserved(&fix);
}

// P3-09/G-04/G-12：8MiB 前缀恰好以合法 porcelain NUL 记录结束，不能冒充完整扫描。
#[cfg(windows)]
#[test]
fn review_remediation_truncated_status_fails_before_any_commit_or_push() {
    let fix = fixture();
    fs::write(fix.repo.join("item"), "must remain uncommitted\n").unwrap();
    let local_before = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let remote_before = git_ok(&fix.remote, &["rev-parse", "master"]);
    let dir = tempfile::tempdir().unwrap();
    let wrapper = git_wrapper(dir.path());
    let record = b"?? item\0";
    let cap = 8 * 1024 * 1024;
    assert_eq!(cap % record.len(), 0, "截断边界必须正好在 NUL 后");
    fs::write(
        dir.path().join("status.bin"),
        record.repeat(cap / record.len() + 1),
    )
    .unwrap();
    let script = GIT_WRAPPER_CMD.replace(
        "\"%REALGIT%\" %*\r\n",
        concat!(
            "if /I \"%SUB%\"==\"status\" goto synthetic_status\r\n",
            "\"%REALGIT%\" %*\r\n",
            "exit /b %ERRORLEVEL%\r\n",
            ":synthetic_status\r\n",
            "type \"%WDIR%status.bin\"\r\n",
            "exit /b 0\r\n"
        ),
    );
    fs::write(&wrapper, script).unwrap();
    let control = Arc::new(Control::default());
    let shared = Arc::new(GitShared::new());
    let _watchdog = RunWatchdog::new(&control);
    let text = git_tools::run(
        &wrapper,
        &fix.repo,
        &control,
        &shared,
        &|_| {},
        &|status| {
            // 只在缺陷版本已经实际提交推送后止损，避免百万条重复记录拖延红测。
            if status.starts_with("已完成 ") {
                control.cancel();
            }
        },
        TEST_UNIT,
    );
    assert!(
        text.contains("扫描失败"),
        "不完整输出必须明确扫描失败：{text}"
    );
    assert_eq!(shared.state.lock().unwrap().as_str(), "失败");
    assert!(!text.contains("全部完成"));
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD"]), local_before);
    assert_eq!(git_ok(&fix.remote, &["rev-parse", "master"]), remote_before);
    assert_eq!(
        fs::read_to_string(fix.repo.join("item")).unwrap(),
        "must remain uncommitted\n"
    );
}

// P2-02/G-15：保留真实消费者回归：停止期间 merge 必须自然完成，
// 不再保留已删除的短 deadline 环境接缝或隔离自启动路径。
#[cfg(windows)]
#[test]
fn review_remediation_git_finishes_naturally_after_stop() {
    let fix = fixture();
    remote_pushes_ahead(&fix.remote, "master", fix.repo.parent().unwrap());
    fs::write(fix.repo.join("a.txt"), "first local file\n").unwrap();
    fs::write(fix.repo.join("z.txt"), "next file must remain\n").unwrap();
    let remote_before = git_ok(&fix.remote, &["rev-parse", "master"]);
    let dir = tempfile::tempdir().unwrap();
    let wrapper = git_wrapper(dir.path());
    hold(dir.path(), "merge");
    let control = Arc::new(Control::default());
    let (_logs, _shared, handle) = spawn_run(&wrapper, &fix.repo, &control);
    wait_trace(dir.path(), "merge");
    control.cancel();
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        !handle.handle.as_ref().unwrap().is_finished(),
        "停止请求不能强杀仍在执行的 merge"
    );
    release(dir.path(), "merge");
    let outcome = handle.join().unwrap();
    assert!(outcome.text.contains("已停止"), "{}", outcome.text);
    assert_merge_tip(&fix.repo, "HEAD");
    assert_eq!(
        git_ok(&fix.repo, &["show", "HEAD:remote-side.txt"]),
        "remote change\n",
        "自然合并的 blob 必须精确正确"
    );
    assert_eq!(
        git_ok(&fix.repo, &["show", ":remote-side.txt"]),
        "remote change\n",
        "索引必须与已合并 blob 一致"
    );
    assert_eq!(
        git_ok(
            &fix.repo,
            &["hash-object", "--path=remote-side.txt", "remote-side.txt"]
        ),
        git_ok(&fix.repo, &["rev-parse", ":remote-side.txt"]),
        "按继承的 Git clean/EOL 配置读取真实工作树后必须精确匹配索引 blob"
    );
    assert_eq!(
        git_ok(
            &fix.repo,
            &[
                "diff",
                "--exit-code",
                "--no-ext-diff",
                "--no-textconv",
                "--",
                "remote-side.txt"
            ]
        ),
        "",
        "真实工作树对索引不得有差异，不能固定 core.autocrlf 来规避 G-17"
    );
    assert!(
        !fix.repo.join(".git/index.lock").exists(),
        "自然结束不得残留锁"
    );
    assert!(!fix.repo.join(".git/MERGE_HEAD").exists(), "自然完成合并");
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        remote_before,
        "停止后不得再次 push"
    );
    assert_eq!(
        git_ok(&fix.repo, &["status", "--porcelain", "--", "z.txt"]).trim(),
        "?? z.txt",
        "不得处理下一文件"
    );
    let phases: Vec<String> = trace_lines(dir.path())
        .into_iter()
        .filter(|line| {
            line.starts_with("push") || line.starts_with("fetch") || line.starts_with("merge")
        })
        .map(|line| line.split_whitespace().next().unwrap().to_owned())
        .collect();
    assert_eq!(
        phases,
        ["push", "fetch", "merge"],
        "自然结束后不得 push 或重试"
    );
}
