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
    sync::{Arc, Mutex, OnceLock},
    thread::JoinHandle,
    time::{Duration, Instant},
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
    // [quality-baseline approved 2026-10-03] TempDir 保活字段（有意不读取），经用户裁定保留
    #[allow(dead_code)]
    _dir: tempfile::TempDir,
}

/// 模板仓库对：每测试二进制构建一次，逐用例以目录复制替代 7 个 git 进程建仓
/// （2026-10-03 计时优化：原 fixture 约 0.7s/用例，复制为毫秒级）。建仓序列与
/// 原 fixture 逐字相同（seed→bare→clone→仓库级身份）；TempDir 随静态存活到
/// 进程退出，模板只读、用例只拿副本。
struct RepoTemplate {
    repo: PathBuf,
    remote: PathBuf,
    _keep: tempfile::TempDir,
}

static TEMPLATE: OnceLock<RepoTemplate> = OnceLock::new();

fn template() -> &'static RepoTemplate {
    TEMPLATE.get_or_init(|| {
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
        RepoTemplate {
            repo,
            remote,
            _keep: dir,
        }
    })
}

/// 递归复制目录（模板仓库只含普通文件与目录，无符号链接）。
fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// seed 仓库（含初始提交）→ bare 远端 → clone 出带 upstream 的工作仓库。
/// 2026-10-03 计时优化：改为复制 [`template`] 的副本，并把副本仓库 origin 的
/// 绝对路径改写到本用例自己的远端。git 在 config 里按转义双反斜杠存储 Windows
/// 绝对路径（实测形态 `url = C:\\...\\remote.git`），三种可能形态都做替换。
fn fixture() -> Fixture {
    let tmpl = template();
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let remote = base.join("remote.git");
    let repo = base.join("repo");
    copy_tree(&tmpl.remote, &remote);
    copy_tree(&tmpl.repo, &repo);
    let config = repo.join(".git").join("config");
    let text = fs::read_to_string(&config).unwrap();
    let old_slash = tmpl.remote.display().to_string();
    let new_slash = remote.display().to_string();
    let patched = text
        .replace(
            &old_slash.replace('\\', "\\\\"),
            &new_slash.replace('\\', "\\\\"),
        )
        .replace(&old_slash, &new_slash)
        .replace(&old_slash.replace('\\', "/"), &new_slash.replace('\\', "/"));
    assert!(
        patched != text,
        "模板 origin 路径未出现在 .git/config：{}",
        config.display()
    );
    fs::write(&config, patched).unwrap();
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
    run_tool_with_control_and_git(&git_exe(), repo, control)
}

fn run_tool_with_control_and_git(git: &Path, repo: &Path, control: &Arc<Control>) -> RunOutcome {
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
            git,
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

/// 后台启动一次工具运行（F13/F14 回归用）：返回日志与共享状态的并发句柄和
/// 等待最终文案的 JoinHandle；主线程可在运行期间请求停止或轮询日志。
fn spawn_run(
    git: &Path,
    repo: &Path,
    control: &Arc<Control>,
) -> (
    Arc<Mutex<Vec<String>>>,
    Arc<GitShared>,
    JoinHandle<RunOutcome>,
) {
    let shared = Arc::new(GitShared::new());
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let trip = Arc::clone(control);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(180));
        trip.cancel();
    });
    let sink = Arc::clone(&logs);
    let repo = repo.to_path_buf();
    let git = git.to_path_buf();
    let control_for_run = Arc::clone(control);
    let shared_for_run = Arc::clone(&shared);
    let handle = std::thread::spawn(move || {
        let text = git_tools::run(
            &git,
            &repo,
            &control_for_run,
            &shared_for_run,
            &|line| {
                let line = line.to_string();
                if std::env::var_os("GT_TRACE").is_some() {
                    eprintln!("[git-tools] {line}");
                }
                sink.lock().unwrap().push(line);
            },
            &|_| {},
            TEST_UNIT,
        );
        RunOutcome {
            text,
            shared: shared_for_run,
            logs: Vec::new(),
        }
    });
    (logs, shared, handle)
}

/// 轮询等待日志中出现包含 needle 的条目（最多 30 秒；超时 panic 带全部日志）。
fn wait_log_contains(logs: &Arc<Mutex<Vec<String>>>, needle: &str) {
    // 与 wait_trace 同口径：真实 git 链路在本机时延风暴下需要更大的同步预算。
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let found = logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(needle));
        if found {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "等待日志「{needle}」超时；当前日志：{:#?}",
            logs.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
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
            "-c",
            "core.quotePath=false",
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
            &"update: a-staged.txt".to_string(),
            &"update: tool-file.txt".to_string(),
            &"update: user-staged.txt".to_string()
        ],
        "三个变更各一个提交：{log:?}"
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
    // G-10「不 rebase、不改写历史」：init 是早已推送的公共祖先，任何 rebase 都不会
    // 使它从远端历史消失，该断言对 merge 与 rebase 恒真；能区分两种实现的是合并
    // 提交本身——rebase 不产生合并提交（判定方式与 conflict_resumed 的用例一致）。
    assert!(
        log.iter().any(|s| s.starts_with("Merge branch")),
        "fetch+merge 必须按 merge 语义合并（远端历史应含合并提交，rebase 不产生）：{log:?}"
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

// 覆盖 G-08（回归：本地 topic 跟踪 origin/master 且 push.default=current 时，
// 裸 git push 会静默创建并推送 origin/topic，upstream 未更新却报告全部完成；
// 修复后启动预检拒绝并点名配置，远端不出现任何新分支）
#[test]
fn push_default_current_with_renamed_upstream_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["checkout", "-q", "-b", "topic"]);
    git_ok(&fix.repo, &["config", "branch.topic.remote", "origin"]);
    git_ok(
        &fix.repo,
        &["config", "branch.topic.merge", "refs/heads/master"],
    );
    git_ok(&fix.repo, &["config", "push.default", "current"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("push.default") && outcome.text.contains("current"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    // 远端不得出现 origin/topic，也不得收到任何提交
    let heads = git_ok(&fix.repo, &["ls-remote", "--heads", "origin"]);
    assert!(!heads.contains("topic"), "不得在远端创建同名分支：{heads}");
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
    );
}

// 覆盖 G-08（正控：push.default=current 且 upstream 分支与本地同名时，裸 push
// 的目标就是 upstream，不属于危险配置，预检放行且任务正常完成）
#[test]
fn push_default_current_with_matching_upstream_name_proceeds() {
    let fix = fixture();
    git_ok(&fix.repo, &["checkout", "-q", "-b", "topic"]);
    git_ok(&fix.repo, &["config", "branch.topic.remote", "origin"]);
    git_ok(
        &fix.repo,
        &["config", "branch.topic.merge", "refs/heads/topic"],
    );
    git_ok(&fix.repo, &["config", "push.default", "current"]);
    // 在远端建立同名分支，使 @{u} 可解析（G-02 的 upstream 验证依赖
    // remote-tracking 引用存在；仅配置 merge 目标时它指向不存在的远端分支）。
    git_ok(
        &fix.repo,
        &["push", "-q", "origin", "topic:refs/heads/topic"],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("全部完成"),
        "同名 upstream 不应被预检拒绝：{}",
        outcome.text
    );
    let heads = git_ok(&fix.repo, &["ls-remote", "--heads", "origin"]);
    assert!(heads.contains("topic"), "应推送到 upstream：{heads}");
}
// 覆盖 G-08/G-09：push.default=simple 在本地与 upstream 分支异名时必然拒绝裸 push；
// 应在提交前识别该确定性失败，而不是无限重试 push。
#[test]
fn push_default_simple_with_renamed_upstream_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["checkout", "-q", "-b", "topic"]);
    git_ok(&fix.repo, &["config", "branch.topic.remote", "origin"]);
    git_ok(
        &fix.repo,
        &["config", "branch.topic.merge", "refs/heads/master"],
    );
    git_ok(&fix.repo, &["config", "push.default", "simple"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始")
            && outcome.text.contains("push.default")
            && outcome.text.contains("simple"),
        "必须在写入前拒绝必然失败的推送配置：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "预检拒绝后不得推送"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "预检拒绝后不得提交"
    );
    assert!(
        git_ok(&fix.repo, &["status", "--porcelain"]).contains("?? a.txt"),
        "预检拒绝后工作区文件保持未处理"
    );
}

// 覆盖 G-09：非法 push.default 会使裸 git push 稳定失败；不得在本地提交后无限重试。
#[test]
fn unsupported_push_default_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["config", "push.default", "unsupported"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始")
            && outcome.text.contains("push.default")
            && outcome.text.contains("unsupported"),
        "必须在写入前拒绝非法 push.default：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "非法推送配置不得更改远端"
    );
    // git 对非法 push.default 值在任何读取配置的命令（含本测试后续 log/status）
    // 中都致命失败；git config 自身的写/删不做语义校验（上面写入已证明），
    // 先还原配置再做未提交/未改动验证。
    git_ok(&fix.repo, &["config", "--unset", "push.default"]);
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "预检拒绝后不得提交"
    );
    assert!(
        git_ok(&fix.repo, &["status", "--porcelain"]).contains("?? a.txt"),
        "预检拒绝后工作区文件保持未处理"
    );
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
    let merge_files = git_ok(
        &fix.repo,
        &[
            // -m --first-parent：merge 提交不带它时 diff-tree 输出恒为空（断言
            // 变恒真）；按第一父展开才得到合并实际带入的路径集合。
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-m",
            "--first-parent",
            &merge_commit,
        ],
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

// ===================== F09~F14 回归 =====================

/// 在指定分支上建立 fixture：seed（master）→ bare 远端 → 推送目标分支 →
/// bare HEAD 指向目标分支 → `clone -b` 出带 upstream 的工作仓库。
fn fixture_on_branch(branch: &str) -> Fixture {
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
    git_ok(&seed, &["checkout", "-q", "-b", branch]);
    git_ok(
        &seed,
        &[
            "push",
            "-q",
            &remote.display().to_string(),
            &format!("HEAD:refs/heads/{branch}"),
        ],
    );
    // bare HEAD 指向目标分支，克隆才干净（否则 clone 警告 HEAD 不存在）
    git_ok(
        &remote,
        &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
    );
    let repo = base.join("repo");
    git_ok(
        base,
        &[
            "clone",
            "-q",
            "-b",
            branch,
            &remote.display().to_string(),
            "repo",
        ],
    );
    git_ok(&repo, &["config", "user.name", "JchTools Test"]);
    git_ok(&repo, &["config", "user.email", "test@jchtools.local"]);
    Fixture {
        repo,
        remote,
        _dir: dir,
    }
}

/// 让 bare 远端的指定分支领先一个提交（另一次克隆推送 remote-side.txt）。
fn remote_pushes_ahead(remote: &Path, branch: &str, parent: &Path) {
    git_ok(
        parent,
        &[
            "clone",
            "-q",
            "-b",
            branch,
            &remote.display().to_string(),
            "other",
        ],
    );
    let other = parent.join("other");
    fs::write(other.join("remote-side.txt"), "remote change\n").unwrap();
    git_ok(&other, &["add", "remote-side.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote side"]);
    git_ok(&other, &["push", "-q"]);
}

/// 远端指定分支的提交列表（subject 一行一个，最新在前）。
fn remote_branch_log(remote: &Path, branch: &str) -> Vec<String> {
    git_ok(remote, &["log", "--format=%s", branch])
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// 为 fixture 增加第二个 bare 远端 `<name>`（不含任何提交），返回其路径。
fn add_second_remote(fix: &Fixture, name: &str) -> PathBuf {
    let other = fix.repo.parent().unwrap().join(format!("{name}.git"));
    git_ok(
        fix.repo.parent().unwrap(),
        &["init", "-q", "--bare", &other.display().to_string()],
    );
    git_ok(
        &fix.repo,
        &["remote", "add", name, &other.display().to_string()],
    );
    other
}

/// F10 断言：拒绝启动后两个远端都未收到任何东西，本地也没有新提交，
/// 工作区文件保持未处理。
fn assert_start_refused_and_nothing_moved(fix: &Fixture, other: &Path) {
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
    let refs = git_ok(other, &["for-each-ref", "--format=%(refname)"]);
    assert!(refs.trim().is_empty(), "第二远端不应收到任何引用：{refs}");
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
    );
    assert!(
        git_ok(&fix.repo, &["status", "--porcelain"]).contains("?? a.txt"),
        "文件保持未跟踪，未被 add/commit"
    );
}

// 可控 git 包装器脚本（F13/F14 回归）。必须 CRLF：cmd 的 goto/标签解析在 LF-only
// 文件下会报「系统找不到指定的批处理标签」。
const GIT_WRAPPER_CMD: &str = concat!(
    "@echo off\r\n",
    "setlocal\r\n",
    "set \"WDIR=%~dp0\"\r\n",
    ">>\"%WDIR%trace.txt\" echo %*\r\n",
    "set /p REALGIT=<\"%WDIR%real-git.txt\"\r\n",
    "set \"SUB=%~1\"\r\n",
    "if /I \"%SUB%\"==\"add\" call :mark ADD\r\n",
    "if /I \"%SUB%\"==\"commit\" call :mark COMMIT\r\n",
    "if /I \"%SUB%\"==\"push\" call :mark PUSH\r\n",
    "if /I \"%SUB%\"==\"fetch\" call :mark FETCH\r\n",
    "if /I \"%SUB%\"==\"merge\" call :mark MERGE\r\n",
    "call :hold add\r\n",
    "call :hold fetch\r\n",
    "call :hold merge\r\n",
    "call :hold commit\r\n",
    "\"%REALGIT%\" %*\r\n",
    "exit /b %ERRORLEVEL%\r\n",
    ":mark\r\n",
    "echo %1-OUT-MARK\r\n",
    "echo %1-ERR-MARK 1>&2\r\n",
    "exit /b 0\r\n",
    ":hold\r\n",
    "if /I not \"%SUB%\"==\"%1\" exit /b 0\r\n",
    ":holdwait\r\n",
    "if exist \"%WDIR%hold-%1.flag\" (\r\n",
    "  ping -n 2 127.0.0.1 >nul 2>&1\r\n",
    "  goto holdwait\r\n",
    ")\r\n",
    "exit /b 0\r\n",
);

/// 构造可控 git 包装器目录：`gitp.cmd`（透传执行真实 git；每次调用把参数追加到
/// trace.txt 记录命令轨迹；add/commit/push/fetch/merge 先向 stdout/stderr 各写
/// 唯一标记供 F14 验证真实输出采集；存在 hold-<子命令>.flag 时在执行前等待标志
/// 消失，供 F13 在命令边界制造可控暂停窗口）。返回 gitp.cmd 路径。
fn git_wrapper(dir: &Path) -> PathBuf {
    // 不带换行：批处理 set /p 会保留行尾 CR，破坏路径
    fs::write(
        dir.join("real-git.txt"),
        git_exe().display().to_string().as_bytes(),
    )
    .unwrap();
    fs::write(dir.join("gitp.cmd"), GIT_WRAPPER_CMD).unwrap();
    dir.join("gitp.cmd")
}

/// 放置 hold 标志：下一次 <sub> 子命令在 wrapper 内暂停，直到标志被移除。
fn hold(dir: &Path, sub: &str) {
    fs::write(dir.join(format!("hold-{sub}.flag")), b"").unwrap();
}

/// 释放 hold 标志：让暂停中的 <sub> 子命令继续执行。
fn release(dir: &Path, sub: &str) {
    fs::remove_file(dir.join(format!("hold-{sub}.flag"))).unwrap();
}

/// 读取 wrapper 的命令轨迹（trace.txt，每行一次调用，按时间追加）。
fn trace_lines(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("trace.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// 等待命令轨迹中出现以 prefix 开头的行（最多 120 秒），返回其行号。预算按
/// Windows 本机偶发的子进程时延风暴（杀软扫描/负载尖峰下每条 git 命令可达 ~2 秒，
/// 任务前置探查就有 15+ 条）留足余量；健康机器整条链路 3 秒内完成。
fn wait_trace(dir: &Path, prefix: &str) -> usize {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let lines = trace_lines(dir);
        if let Some(index) = lines.iter().position(|l| l.starts_with(prefix)) {
            return index;
        }
        assert!(
            Instant::now() < deadline,
            "等待命令「{prefix}」启动超时；当前轨迹：{lines:#?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
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
        assert!(
            log.iter().any(|s| s.starts_with("Merge")),
            "分支 {branch} 必须按 merge 语义合并（远端历史含合并提交）：{log:?}"
        );
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
    assert!(
        other_log.iter().any(|s| s.starts_with("Merge")),
        "other 应包含合并提交：{other_log:?}"
    );
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
    assert!(
        master_log.iter().any(|s| s.starts_with("Merge")),
        "master 应包含合并提交：{master_log:?}"
    );
    assert_eq!(
        git_ok(&repo, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "topic",
        "不得切换分支"
    );
}

// 覆盖 P-03/G-08（F10：branch.<b>.pushRemote 指向 other 时裸 git push 会推到
// 非 upstream 远端；启动前预检必须拒绝，两个远端都收不到任何东西）
#[test]
fn push_remote_config_mismatch_refuses_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    git_ok(&fix.repo, &["config", "branch.master.pushRemote", "other"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("branch.master.pushRemote"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_start_refused_and_nothing_moved(&fix, &other);
}

// 覆盖 P-03/G-08（F10：remote.pushDefault=other 同样使裸 git push 偏离 upstream）
#[test]
fn push_default_remote_config_refuses_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    git_ok(&fix.repo, &["config", "remote.pushDefault", "other"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("remote.pushDefault"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_start_refused_and_nothing_moved(&fix, &other);
}

// 覆盖 P-03/G-08（F10：remote.<r>.push 自定义 refspec 会改变裸 push 的推送对象）
#[test]
fn remote_push_refspec_refuses_to_start() {
    let fix = fixture();
    git_ok(
        &fix.repo,
        &[
            "config",
            "remote.origin.push",
            "refs/heads/master:refs/heads/other-branch",
        ],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("remote.origin.push"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    // 单一远端也不能收到任何东西
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
    );
}

// 覆盖 P-03/G-08（F10：push.default=matching 会一次推送所有同名分支（多分支））
#[test]
fn push_default_matching_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["config", "push.default", "matching"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("push.default") && outcome.text.contains("matching"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
}

// 覆盖 P-03/G-08（独立审查发现：remote.origin.mirror=true 使裸 push 变镜像
// 推送——全部引用推到远端并删除远端多余分支，退出码 0 被当成功报告；
// 预检必须拒绝，远端零改动）
#[test]
fn mirror_remote_config_refuses_to_start() {
    let fix = fixture();
    git_ok(&fix.repo, &["config", "remote.origin.mirror", "true"]);
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("mirror"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不得被镜像推送改动"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
    );
}
// 覆盖 P-03/G-08（remote.<name>.mirror 的布尔真值允许 Git 配置同义词；
// 只识别字面 true 会放过 yes/on/1，随后裸 push 会镜像全部引用）
#[test]
fn mirror_remote_config_boolean_alias_refuses_to_start() {
    for value in ["yes", "on", "1"] {
        let fix = fixture();
        git_ok(&fix.repo, &["config", "remote.origin.mirror", value]);
        fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

        let outcome = run_tool(&fix.repo);
        assert!(
            outcome.text.contains("无法开始") && outcome.text.contains("mirror"),
            "remote.origin.mirror={value} 必须在写入前被拒绝：{}",
            outcome.text
        );
        assert_eq!(
            remote_log(&fix.remote),
            vec!["init".to_string()],
            "镜像推送不得改动远端"
        );
        assert_eq!(
            git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
            "init",
            "预检拒绝后不得产生本地提交"
        );
    }
}

// 覆盖 P-03/G-08（remote.<name>.url 可重复；Git push 会推送到全部 URL，
// 预检只看首个 fetch URL 会遗漏 upstream 之外的额外推送地址）
#[test]
fn multiple_upstream_urls_refuse_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    git_ok(
        &fix.repo,
        &[
            "remote",
            "set-url",
            "--add",
            "origin",
            &other.display().to_string(),
        ],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始") && outcome.text.contains("pushurl"),
        "必须拒绝 upstream remote 的额外推送 URL：{}",
        outcome.text
    );
    assert_start_refused_and_nothing_moved(&fix, &other);
}
// 覆盖 P-03/P-09/G-08：url.<base>.pushInsteadOf 会重写裸 push 的真实目标；
// 预检必须按 Git 展开后的 push URL 拒绝 upstream 之外的目标。
#[test]
fn push_instead_of_rewrite_refuses_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    let upstream_url = "https://upstream.invalid/repo.git";
    let other_url = git_ok(&fix.repo, &["remote", "get-url", "other"])
        .trim()
        .replace('\\', "/");
    let other_url = format!("file:///{}", other_url.trim_start_matches('/'));
    git_ok(&fix.repo, &["remote", "set-url", "origin", upstream_url]);
    git_ok(
        &fix.repo,
        &[
            "config",
            "--add",
            &format!("url.\"{other_url}\".pushInsteadOf"),
            upstream_url,
        ],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();

    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始") && outcome.text.contains("pushInsteadOf"),
        "必须拒绝被 pushInsteadOf 改写到 upstream 之外的 URL：{}",
        outcome.text
    );
    assert_start_refused_and_nothing_moved(&fix, &other);
}

// 覆盖 P-03/G-08（独立审查发现：pushurl 指向 upstream 之外的地址时，裸 push
// 实际推到另一远端且退出码 0；预检必须拒绝，两个远端都零改动）
#[test]
fn pushurl_config_refuses_to_start() {
    let fix = fixture();
    let other = add_second_remote(&fix, "other");
    let other_url = git_ok(&fix.repo, &["remote", "get-url", "other"])
        .trim()
        .to_owned();
    assert!(!other_url.is_empty(), "前置：取 other 远端 URL");
    git_ok(
        &fix.repo,
        &["remote", "set-url", "--push", "origin", &other_url],
    );
    fs::write(fix.repo.join("a.txt"), "x\n").unwrap();
    let outcome = run_tool(&fix.repo);
    assert!(
        outcome.text.contains("无法开始"),
        "必须拒绝启动：{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("pushurl"),
        "错误信息必须点名涉及配置项：{}",
        outcome.text
    );
    assert_eq!(
        remote_log(&fix.remote),
        vec!["init".to_string()],
        "origin 不应收到任何提交"
    );
    assert!(
        !other.join("refs/heads").exists()
            || git_ok(&fix.repo, &["ls-remote", "--heads", "other"])
                .trim()
                .is_empty(),
        "other 不应收到任何提交"
    );
    assert_eq!(
        git_ok(&fix.repo, &["log", "--format=%s"]).trim(),
        "init",
        "本地不得有新提交"
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
    fs::write(
        fix.repo.join("renamed.txt"),
        "completely different resolved content with no shared lines at all\n".repeat(20),
    )
    .unwrap();
    let _ = fs::remove_file(fix.repo.join("file.txt"));
    git_ok(&fix.repo, &["add", "-A"]);
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
    // 合并提交确实处理了旧路径删除与新路径内容（--no-renames 语义下的两个路径）。
    let merge_files = git_ok(
        &fix.repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-m",
            "--first-parent",
            &merge_commit,
        ],
    );
    assert!(
        merge_files.contains("file.txt") && merge_files.contains("renamed.txt"),
        "合并提交应同时覆盖旧路径删除与新路径内容：{merge_files}"
    );
}

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
    assert!(
        git_ok(&fix.repo, &["log", "-1", "--format=%s"])
            .trim()
            .starts_with("Merge"),
        "merge 自然结束后保留本地合并提交"
    );
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
    assert!(
        entry(&["git push"], "PUSH-OUT-MARK", "PUSH-ERR-MARK").is_some(),
        "git push 成功必须采集真实输出：{:#?}",
        outcome.logs
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

    // 失败路径：push 失败的真实 stderr 也要进日志
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
    let failed: Vec<String> = logs
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("push 失败"))
        .cloned()
        .collect();
    assert!(
        failed
            .iter()
            .any(|line| line.contains("PUSH-ERR-MARK") || line.contains("PUSH-OUT-MARK")),
        "push 失败必须携带真实 stderr/stdout：{failed:#?}"
    );
    control.cancel();
    let failed_outcome = handle.join().unwrap();
    assert!(
        failed_outcome.text.contains("任务已停止"),
        "失败路径收尾：{}",
        failed_outcome.text
    );
}
