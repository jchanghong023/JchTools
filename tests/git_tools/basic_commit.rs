use super::*;
// 覆盖 G-02（无效仓库 / 无检出分支 / 无 upstream 的拒绝）
#[test]
fn inspect_rejects_invalid_repositories() {
    let dir = tempfile::tempdir().unwrap();
    // 普通目录不是仓库
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    let error = git_tools::inspect(&git_exe(), &plain).expect_err("普通目录必须被拒绝");
    assert!(
        format!("{error:#}").contains("不是有效的 Git repository"),
        "{error:#}"
    );

    // 有仓库但无 upstream（且无检出分支的主分支刚 init 也算无 upstream）
    let no_upstream = dir.path().join("no-upstream");
    fs::create_dir_all(&no_upstream).unwrap();
    git_ok(&no_upstream, &["init", "-q"]);
    fs::write(no_upstream.join("a.txt"), "x").unwrap();
    git_ok(&no_upstream, &["add", "a.txt"]);
    git_ok(&no_upstream, &["commit", "-q", "-m", "a"]);
    let error = git_tools::inspect(&git_exe(), &no_upstream).expect_err("无 upstream 必须被拒绝");
    assert!(format!("{error:#}").contains("upstream"), "{error:#}");

    // detached HEAD：有 upstream 配置但未检出分支
    let fix = fixture();
    git_ok(&fix.repo, &["checkout", "-q", "--detach", "HEAD"]);
    let error = git_tools::inspect(&git_exe(), &fix.repo).expect_err("detached HEAD 必须被拒绝");
    assert!(format!("{error:#}").contains("branch"), "{error:#}");
}

// 覆盖 G-03~G-08（单个新增文件：add → commit → push，提交信息与单文件验证）
#[test]
fn single_added_file_committed_and_pushed() {
    let fix = fixture();
    fs::create_dir_all(fix.repo.join("src")).unwrap();
    fs::write(fix.repo.join("src").join("new.rs"), "fn main() {}\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "收尾文案：{}",
        outcome.text
    );
    assert!(worktree_clean(&fix.repo), "处理后工作区应干净");
    let log = remote_log(&fix.remote);
    assert!(
        log.contains(&"update: src/new.rs".to_string()),
        "远端提交信息：{log:?}"
    );
    assert_eq!(
        head_paths(&fix.repo),
        vec!["src/new.rs".to_string()],
        "HEAD 只含当前文件"
    );
    assert_eq!(
        outcome
            .shared
            .done
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

// 覆盖 G-04, G-05, G-09（回归：含 `[...]` 的合法 Windows 文件名必须按字面 pathspec
// 处理——修复前 pathspec 被 wildmatch 解释为字符类，`数据[1].txt` 匹配不到自身，
// add 进入无限重试、只能靠看门狗以「已停止」收场）
#[test]
fn bracketed_filename_commits_and_pushes_literally() {
    let fix = fixture();
    // Git 的默认 quotePath 在 CI 上会把非 ASCII 路径转义；固定此差异，
    // head_paths 再用机器可读的原始路径断言真实提交对象。
    git_ok(&fix.repo, &["config", "core.quotePath", "true"]);
    fs::write(
        fix.repo.join("数据[1].txt"),
        "payload
",
    )
    .unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "含字符类字符的文件名必须正常提交推送：{}",
        outcome.text
    );
    assert!(worktree_clean(&fix.repo), "处理后工作区应干净");
    let log = remote_log(&fix.remote);
    assert!(
        log.contains(&"update: 数据[1].txt".to_string()),
        "远端提交信息按字面路径：{log:?}"
    );
    assert_eq!(head_paths(&fix.repo), vec!["数据[1].txt".to_string()]);
}

// 覆盖 G-04, G-06（回归：用户启动前已暂存的删除（git rm）在索引中已无条目，
// `git add -- <path>` 必然报 pathspec 不匹配——核实 `D ` 完全暂存形态后跳过 add
// 直接提交；修复前进入无限重试，只能靠看门狗以「已停止」收场）
#[test]
fn prestaged_deletion_commits_without_add() {
    let fix = fixture();
    fs::write(
        fix.repo.join("gone.txt"),
        "will remove
",
    )
    .unwrap();
    git_ok(&fix.repo, &["add", "gone.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "prepare"]);
    git_ok(&fix.repo, &["rm", "-q", "gone.txt"]);
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "预暂存删除必须正常提交推送：{}",
        outcome.text
    );
    assert!(worktree_clean(&fix.repo), "处理后工作区应干净");
    let log = remote_log(&fix.remote);
    assert!(
        log.contains(&"update: gone.txt".to_string()),
        "远端提交信息：{log:?}"
    );
    assert!(!fix.repo.join("gone.txt").exists(), "删除已生效");
}

// 覆盖 G-04（修改、删除、重命名各一类逻辑变更）
#[test]
fn modified_deleted_and_renamed_changes() {
    let fix = fixture();
    fs::write(fix.repo.join("doc.md"), "v1\n").unwrap();
    fs::write(fix.repo.join("gone.txt"), "will delete\n").unwrap();
    fs::write(fix.repo.join("old_name.txt"), "renamed\n").unwrap();
    git_ok(&fix.repo, &["add", "."]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "prepare"]);
    git_ok(&fix.repo, &["push", "-q"]);
    // 修改 doc.md、删除 gone.txt、git mv 重命名（git 已识别为 R）
    fs::write(fix.repo.join("doc.md"), "v2 with changes\n").unwrap();
    fs::remove_file(fix.repo.join("gone.txt")).unwrap();
    git_ok(&fix.repo, &["mv", "old_name.txt", "new_name.txt"]);
    let outcome = run_tool(&fix.repo);
    assert!(worktree_clean(&fix.repo));
    let log = remote_log(&fix.remote);
    assert!(log.contains(&"update: doc.md".to_string()), "{log:?}");
    assert!(log.contains(&"update: gone.txt".to_string()), "{log:?}");
    assert!(
        log.contains(&"update: new_name.txt".to_string()),
        "rename 用新路径：{log:?}"
    );
    // rename 作为一个逻辑变更：HEAD 提交同时含新旧两路径
    assert_eq!(
        head_paths(&fix.repo),
        vec!["new_name.txt".to_string(), "old_name.txt".to_string()],
        "rename 的 commit 含新旧两个路径"
    );
    // 三类变更 = 3 次提交 + 准备提交
    let updates = log.iter().filter(|s| s.starts_with("update: ")).count();
    assert_eq!(
        updates, 3,
        "三类变更均须实际提交并推送：{log:?}；结果={}；日志={:?}",
        outcome.text, outcome.logs
    );
    assert_eq!(
        git_ok(&fix.remote, &["show", "master:new_name.txt"]),
        "renamed\n",
        "重命名后的文件内容必须在远端可见"
    );
}

// 覆盖 G-03/G-05/G-08（多个待提交文件严格逐个处理：每个 commit 单文件、push 后才下一个）
#[test]
fn multiple_files_are_strictly_serial() {
    let fix = fixture();
    fs::write(fix.repo.join("a.txt"), "a\n").unwrap();
    fs::write(fix.repo.join("b.txt"), "b\n").unwrap();
    fs::write(fix.repo.join("c.txt"), "c\n").unwrap();
    let outcome = run_tool(&fix.repo);
    let log = remote_log(&fix.remote);
    // 每个 update 提交都只包含一个文件：逐条校验远端历史
    let updates: Vec<&String> = log.iter().filter(|s| s.starts_with("update: ")).collect();
    assert_eq!(
        updates.len(),
        3,
        "三个文件均须实际提交并推送：{log:?}；结果={}；日志={:?}",
        outcome.text,
        outcome.logs
    );
    // git status 排序 a < b < c：提交顺序 a、b、c（最新在前 → 逆序读）
    assert_eq!(updates[0], "update: c.txt", "最后提交的最新：{log:?}");
    assert_eq!(updates[1], "update: b.txt");
    assert_eq!(updates[2], "update: a.txt");
    // 远端逐提交验证：每个 update 提交恰好一个文件（subject 行后跟它的文件行）
    let history = git_ok(
        &fix.remote,
        &["log", "--format=%s", "--name-only", "master"],
    );
    let mut current_subject = String::new();
    let mut current_files: Vec<String> = Vec::new();
    let verify = |subject: &str, files: &[String]| {
        if subject.starts_with("update: ") {
            assert_eq!(
                files.len(),
                1,
                "远端提交「{subject}」必须只含一个文件：{files:?}"
            );
            assert_eq!(
                subject.trim_start_matches("update: "),
                files[0],
                "提交信息路径必须与提交内容一致"
            );
        }
    };
    for line in history.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with("update: ") || line == "init" {
            verify(&current_subject, &current_files);
            current_subject = line.to_owned();
            current_files.clear();
        } else {
            current_files.push(line.to_owned());
        }
    }
    verify(&current_subject, &current_files);
    assert!(worktree_clean(&fix.repo));
    // G-03「每个文件 commit 后立即 push、push 成功后才进入下一个文件」必须有随
    // push 时机变化的断言：远端历史在「逐个 commit、最后统一 push」的批量模式下
    // 逐字节相同，只有日志顺序能区分（这些阶段行由生产代码按 G-14 写入日志）。
    let position = |needle: &str| {
        outcome
            .logs
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("日志缺少「{needle}」：{:?}", outcome.logs))
    };
    let (a, a_done) = (position("开始处理：a.txt"), position("push 成功：a.txt"));
    let (b, b_done) = (position("开始处理：b.txt"), position("push 成功：b.txt"));
    let (c, c_done) = (position("开始处理：c.txt"), position("push 成功：c.txt"));
    assert!(
        a < a_done && a_done < b && b < b_done && b_done < c && c < c_done,
        "必须逐文件 commit 后立即 push、成功后才处理下一个（G-03）：{:?}",
        outcome.logs
    );
}

// 覆盖 G-06（保护现有 staged 状态：不带入、不删除）
#[test]
fn preexisting_staged_content_is_protected() {
    let fix = fixture();
    // 用户预先 staged 的内容：两个。porcelain 把索引条目排在未跟踪条目之前——
    // 若实现丢掉按路径限定的 commit 机制改用普通 commit -m，处理首个文件时索引里
    // 还留着另一个 staged 项，立即产生双文件提交被远端逐提交校验拦下。只预置一个
    // staged 文件时，它被单独提交后索引已空，该回归永远触发不了（夹具形态缺陷）。
    fs::write(fix.repo.join("a-staged.txt"), "user staged a\n").unwrap();
    fs::write(fix.repo.join("user-staged.txt"), "user kept this staged\n").unwrap();
    git_ok(&fix.repo, &["add", "a-staged.txt", "user-staged.txt"]);
    // 工具要处理的文件
    fs::write(fix.repo.join("tool-file.txt"), "processed by tool\n").unwrap();
    let outcome = run_tool(&fix.repo);
    // G-06/G-12：staged 的 user-staged 也是待处理变更，但必须作为独立提交处理；
    // 任何一次 commit 都不得把另一个文件混进来（远端逐提交验证单文件）。
    let log = remote_log(&fix.remote);
    let mut subjects: Vec<&String> = log.iter().filter(|s| s.starts_with("update: ")).collect();
    subjects.sort();
    assert_eq!(
        subjects,
        vec![
            &"update: a-staged.txt".to_string(),
            &"update: tool-file.txt".to_string(),
            &"update: user-staged.txt".to_string()
        ],
        "三个变更各一个提交：{log:?}；结果={}；日志={:?}",
        outcome.text,
        outcome.logs
    );
    let history = git_ok(
        &fix.remote,
        &["log", "--format=%s", "--name-only", "master"],
    );
    let mut current_subject = String::new();
    let mut current_files: Vec<String> = Vec::new();
    let verify = |subject: &str, files: &[String]| {
        if subject.starts_with("update: ") {
            assert_eq!(
                files.len(),
                1,
                "提交「{subject}」必须只含一个文件：{files:?}"
            );
            assert_eq!(
                subject.trim_start_matches("update: "),
                files[0],
                "提交信息路径必须与提交内容一致"
            );
        }
    };
    for line in history.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if line.starts_with("update: ") || line == "init" {
            verify(&current_subject, &current_files);
            current_subject = line.to_owned();
            current_files.clear();
        } else {
            current_files.push(line.to_owned());
        }
    }
    verify(&current_subject, &current_files);
    // 全部处理完毕后工作区与暂存区干净
    assert!(worktree_clean(&fix.repo));
    assert_eq!(
        fs::read_to_string(fix.repo.join("user-staged.txt")).unwrap(),
        "user kept this staged\n",
        "用户文件内容不得被删除或覆盖"
    );
}

// 覆盖 G-09（退避序列 5→10→20→40→80→80…；毫秒基准等比缩放）
#[test]
fn backoff_follows_required_sequence() {
    let unit = Duration::from_secs(5);
    let expected = [5, 10, 20, 40, 80, 80, 80, 80];
    for (index, want) in expected.iter().enumerate() {
        let got = git_tools::backoff_duration(u64::try_from(index + 1).unwrap(), unit).as_secs();
        assert_eq!(got, *want, "第 {} 次重试等待应为 {} 秒", index + 1, want);
    }
    // 毫秒基准等比缩放（测试注入）
    assert_eq!(
        git_tools::backoff_duration(3, Duration::from_millis(10)),
        Duration::from_millis(40)
    );
}

// 覆盖 G-15（用户停止：不继续处理、已成功文件保持）
#[test]
fn stop_before_any_file_keeps_repo_untouched() {
    let fix = fixture();
    fs::write(fix.repo.join("a.txt"), "pending\n").unwrap();
    fs::write(fix.repo.join("b.txt"), "pending\n").unwrap();
    let control = Arc::new(Control::default());
    control.cancel();
    let outcome = run_tool_with_control(&fix.repo, &control);
    assert!(
        outcome.text.contains("已停止"),
        "收尾文案：{}",
        outcome.text
    );
    let log = remote_log(&fix.remote);
    assert!(
        !log.iter().any(|s| s.starts_with("update: ")),
        "停止后不得有提交：{log:?}"
    );
    assert!(fs::read(fix.repo.join("a.txt")).is_ok(), "文件不得被动过");
}

// 覆盖 G-04（已暂存 copy：porcelain 两段记录「C 新路径\0旧路径\0」，旧路径段必须被
// 消费并按新增处理为一个逻辑变更，不得串位成幽灵记录导致整次扫描失败）
#[test]
fn staged_copy_two_segment_record_is_handled_as_single_added_change() {
    let fix = fixture();
    fs::write(fix.repo.join("src.txt"), "l1\nl2\nl3\nl4\nl5\nl6\n").unwrap();
    git_ok(&fix.repo, &["add", "src.txt"]);
    git_ok(&fix.repo, &["commit", "-q", "-m", "prepare"]);
    git_ok(&fix.repo, &["push", "-q"]);
    // status.renames=copies 时，「源文件同批修改 + 副本新增」会被 git 识别为 copy
    git_ok(&fix.repo, &["config", "status.renames", "copies"]);
    fs::copy(fix.repo.join("src.txt"), fix.repo.join("dst.txt")).unwrap();
    fs::write(
        fix.repo.join("src.txt"),
        "l1\nl2\nl3\nl4\nl5\nl6-modified\n",
    )
    .unwrap();
    git_ok(&fix.repo, &["add", "src.txt", "dst.txt"]);
    // 前置条件守卫：确认确实构造出了 copy 两段记录（新路径后随 NUL + 旧路径）
    let porcelain = git_ok(
        &fix.repo,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    );
    assert!(
        porcelain.contains("C  dst.txt\0src.txt\0"),
        "前置条件：必须构造出 copy 两段记录：{porcelain:?}"
    );
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "收尾文案：{}",
        outcome.text
    );
    assert!(worktree_clean(&fix.repo));
    let log = remote_log(&fix.remote);
    assert!(log.contains(&"update: dst.txt".to_string()), "{log:?}");
    assert!(log.contains(&"update: src.txt".to_string()), "{log:?}");
    // copy 按新增处理 + 源文件修改：恰好两个逻辑变更（旧路径段不得变成幽灵提交）
    let updates = log.iter().filter(|s| s.starts_with("update: ")).count();
    assert_eq!(updates, 2, "{log:?}");
}

// 覆盖 G-04（工作区重命名：porcelain X=' '、Y='R' 两段记录按一个 Renamed 处理。
// 当前 git 版本的 status 不为未暂存重命名输出 Y='R'，故以合成 porcelain 输入直接
// 验证解析分支；其他 git 配置/版本下可能出现该形状，解析器必须健壮）
#[test]
fn parse_worktree_rename_record_as_one_renamed_change() {
    let changes = git_tools::parse_status(" R wt_new.txt\0wt_old.txt\0").expect("记录必须解析成功");
    assert_eq!(
        changes,
        vec![git_tools::FileChange::Renamed(
            "wt_old.txt".to_string(),
            "wt_new.txt".to_string()
        )]
    );
}

// 覆盖 G-04（MR 记录：先 add 修改再工作区改名，porcelain「MR 新路径\0旧路径\0」
// 两段；旧路径段必须被消费，不得被当成下一条记录头串位误解析）
#[test]
fn parse_staged_modify_plus_worktree_rename_record_without_ghost() {
    let changes = git_tools::parse_status("MR mr_new.txt\0mr_old.txt\0").expect("记录必须解析成功");
    assert_eq!(
        changes,
        vec![git_tools::FileChange::Renamed(
            "mr_old.txt".to_string(),
            "mr_new.txt".to_string()
        )]
    );
}
