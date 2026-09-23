//! Git 工具核心逻辑测试（合同 G 分区）：逐文件提交并推送。
//! 用本地 bare 仓库模拟 upstream 远端（不经网络，P-03 例外范围内的真实 git 行为）；
//! 退避基准注入毫秒级，验证重试不拖慢测试。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools::{
    control::Control,
    git_tools::{self, GitShared},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

/// 测试退避基准：10ms（首败等 10ms，逐次翻倍封顶 160ms），验证重试节奏而不拖慢测试。
const TEST_UNIT: Duration = Duration::from_millis(10);

fn git_exe() -> PathBuf {
    git_tools::find_git().expect("测试环境需要 git")
}

/// 在 cwd 执行 git（带测试身份与固定默认分支，输出进 panic 消息便于定位）。
fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let mut command = Command::new(git_exe());
    command
        .current_dir(cwd)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "user.name=JchTools Test",
            "-c",
            "user.email=test@jchtools.local",
            "-c",
            "init.defaultBranch=master",
        ])
        .args(args);
    let out = command.output().expect("启动 git 失败");
    assert!(
        out.status.success(),
        "git {args:?} 失败（{}）：{}{}",
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Fixture {
    repo: PathBuf,
    remote: PathBuf,
    #[allow(dead_code)]
    _dir: tempfile::TempDir,
}

/// seed 仓库（含初始提交）→ bare 远端 → clone 出带 upstream 的工作仓库。
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let seed = base.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git_ok(&seed, &["init", "-q"]);
    fs::write(seed.join("README.md"), "init\n").unwrap();
    git_ok(&seed, &["add", "README.md"]);
    git_ok(&seed, &["commit", "-q", "-m", "init"]);
    let remote = base.join("remote.git");
    git_ok(
        base,
        &["init", "-q", "--bare", &remote.display().to_string()],
    );
    git_ok(
        &seed,
        &["push", "-q", &remote.display().to_string(), "master"],
    );
    let repo = base.join("repo");
    git_ok(
        base,
        &["clone", "-q", &remote.display().to_string(), "repo"],
    );
    // 仓库级提交身份：工具进程的 git commit 不带 -c 覆盖，CI runner 没有全局
    // user.name/user.email 时提交必然失败。写进仓库本地配置对任何调用方生效。
    git_ok(&repo, &["config", "user.name", "JchTools Test"]);
    git_ok(&repo, &["config", "user.email", "test@jchtools.local"]);
    Fixture {
        repo,
        remote,
        _dir: dir,
    }
}

struct RunOutcome {
    text: String,
    shared: Arc<GitShared>,
    logs: Vec<String>,
}

fn run_tool(repo: &Path) -> RunOutcome {
    let control = Arc::new(Control::default());
    run_tool_with_control(repo, &control)
}

fn run_tool_with_control(repo: &Path, control: &Arc<Control>) -> RunOutcome {
    let shared = Arc::new(GitShared::new());
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    // 看门狗：环境异常导致工具进入无限重试（G-09 真实故障下不会自行结束）时，
    // 3 分钟后取消任务让本用例带着「已停止」文案失败退出，而不是挂死整个测试进程。
    let trip = Arc::clone(control);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(180));
        trip.cancel();
    });
    let text = {
        // sink 的克隆在本块结束时销毁，之后的 try_unwrap 才能拿回 Vec
        let sink = Arc::clone(&logs);
        git_tools::run(
            &git_exe(),
            repo,
            control,
            &shared,
            &|line| {
                let line = line.to_string();
                if std::env::var_os("GT_TRACE").is_some() {
                    eprintln!("[git-tools] {line}");
                }
                sink.lock().unwrap().push(line);
            },
            &|_| {},
            TEST_UNIT,
        )
    };
    let logs = Arc::try_unwrap(logs)
        .map(|guard| guard.into_inner().unwrap())
        .unwrap_or_default();
    RunOutcome { text, shared, logs }
}

/// 远端指定分支的提交列表（ subject 一行一个，最新在前）。
fn remote_log(remote: &Path) -> Vec<String> {
    git_ok(remote, &["log", "--format=%s", "master"])
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// HEAD 提交涉及的路径集合（diff-tree）。
fn head_paths(repo: &Path) -> Vec<String> {
    let text = git_ok(
        repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            "HEAD",
        ],
    );
    let mut paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths
}

fn worktree_clean(repo: &Path) -> bool {
    git_ok(repo, &["status", "--porcelain"]).trim().is_empty()
}

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
    assert!(
        outcome.text.contains("全部完成"),
        "收尾文案：{}",
        outcome.text
    );
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
    assert_eq!(updates, 3, "{log:?}");
    assert!(
        fs::read(fix.repo.join("new_name.txt")).is_ok(),
        "重命名后的文件在远端可见"
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
    assert!(outcome.text.contains("3 / 3"), "完成计数：{}", outcome.text);
    let log = remote_log(&fix.remote);
    // 每个 update 提交都只包含一个文件：逐条校验远端历史
    let updates: Vec<&String> = log.iter().filter(|s| s.starts_with("update: ")).collect();
    assert_eq!(updates.len(), 3, "{log:?}");
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
}

// 覆盖 G-06（保护现有 staged 状态：不带入、不删除）
#[test]
fn preexisting_staged_content_is_protected() {
    let fix = fixture();
    // 用户预先 staged 的内容（不属于本工具要处理的顺序首位）
    fs::write(fix.repo.join("user-staged.txt"), "user kept this staged\n").unwrap();
    git_ok(&fix.repo, &["add", "user-staged.txt"]);
    // 工具要处理的文件
    fs::write(fix.repo.join("tool-file.txt"), "processed by tool\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "收尾文案：{}",
        outcome.text
    );
    // G-06/G-12：staged 的 user-staged 也是待处理变更，但必须作为独立提交处理；
    // 任何一次 commit 都不得把另一个文件混进来（远端逐提交验证单文件）。
    let log = remote_log(&fix.remote);
    let mut subjects: Vec<&String> = log.iter().filter(|s| s.starts_with("update: ")).collect();
    subjects.sort();
    assert_eq!(
        subjects,
        vec![
            &"update: tool-file.txt".to_string(),
            &"update: user-staged.txt".to_string()
        ],
        "两个变更各一个提交：{log:?}"
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
    assert!(
        fs::read(fix.repo.join("user-staged.txt")).is_ok(),
        "用户文件不得被删除"
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
    // merge 不改写历史：本地 init 提交仍在远端历史中
    assert!(
        log.iter().any(|s| s == "init"),
        "不 rebase、不改写历史：{log:?}"
    );
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
    // G-14：日志必须包含实际阶段与 git 输出细节（CONFLICT、冲突文件），不能只说「失败」
    let joined = outcome.logs.join(
        "
",
    );
    assert!(
        joined.contains("CONFLICT") || joined.contains("conflict.txt"),
        "日志必须包含冲突细节: {joined}"
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
    assert!(
        log.iter()
            .any(|s| s == "Merge made by the 'ort' strategy." || s.starts_with("Merge branch")),
        "远端应包含合并提交：{log:?}"
    );
    // 远端最终内容是解决后的版本
    let remote_content = git_ok(&fix.remote, &["show", "master:conflict.txt"]);
    assert!(
        remote_content.contains("resolved version"),
        "合并结果推送成功: {remote_content}"
    );
}

// 覆盖 G-12（以 git 为状态来源：成功 push 后重启不重复提交）
#[test]
fn completed_files_not_reprocessed_on_restart() {
    let fix = fixture();
    fs::write(fix.repo.join("a.txt"), "a\n").unwrap();
    let first = run_tool(&fix.repo);
    assert!(first.text.contains("全部完成"));
    let second = run_tool(&fix.repo);
    assert!(
        second.text.contains("没有需要提交的变更"),
        "重启后按 git 状态判定：{}",
        second.text
    );
    let log = remote_log(&fix.remote);
    assert_eq!(
        log.iter().filter(|s| **s == "update: a.txt").count(),
        1,
        "不得重复提交：{log:?}"
    );
}
