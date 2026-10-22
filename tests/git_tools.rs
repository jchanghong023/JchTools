#![warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    future_incompatible,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]

//! Git 工具核心逻辑测试（合同 G 分区）：逐文件提交并推送。
//! 用本地 bare 仓库模拟 upstream 远端（不经网络，P-03 例外范围内的真实 git 行为）；
//! 退避基准注入毫秒级，验证重试不拖慢测试。
// 测试代码允许 unwrap/expect：断言失败即测试失败，属合理用法
// （与 clippy.toml 的 allow-*-in-tests 策略一致，集成测试 crate 不在其覆盖范围内）。
#![allow(clippy::unwrap_used, clippy::expect_used)]
use jchtools_file_core::control::Control;
use jchtools_git_tools::git_tools::{self, GitShared};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc, Arc, Mutex, OnceLock},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[path = "git_tools/basic_commit.rs"]
mod basic_commit;
#[path = "git_tools/fetch_merge.rs"]
mod fetch_merge;
#[path = "git_tools/push_config.rs"]
mod push_config;
#[path = "git_tools/review_remediation.rs"]
mod review_remediation;
#[path = "git_tools/staging_protection.rs"]
mod staging_protection;
#[path = "git_tools/stop_control.rs"]
mod stop_control;
#[path = "git_tools/submodule_review.rs"]
mod submodule_review;

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

/// 看门狗只服务当前运行；正常结束或 panic 时唤醒并回收线程，避免每次运行
/// 留下休眠三分钟的线程、以及在结束后对复用的 Control 发出迟到的取消。
struct RunWatchdog {
    stop: mpsc::Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl RunWatchdog {
    fn new(control: &Arc<Control>) -> Self {
        let (stop, receiver) = mpsc::channel();
        let control = Arc::clone(control);
        let handle = std::thread::spawn(move || {
            if matches!(
                receiver.recv_timeout(Duration::from_secs(180)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                // 观测记录（GT_TRACE 复现，2026-10-23）：整目标并行时 cmd 包装进程偶发创建饥饿，
                // 挂起用例被本看门狗取消后，工具文本呈现「任务已停止」，属测试基础设施超时，
                // 不是产品把临时失败误判为阻挡。取消前显式留痕，避免再次误读。
                eprintln!("[git-tools] RunWatchdog：180 秒超时，取消控制（测试基础设施超时，非产品判定）");
                control.cancel();
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for RunWatchdog {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
    // G-09 的真实故障可能无限重试；超时只用于让断言失败，不作为成功路径。
    let _watchdog = RunWatchdog::new(control);
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
        .expect("运行结束后日志 sink 必须已释放")
        .into_inner()
        .unwrap();
    RunOutcome { text, shared, logs }
}

/// 同步断言失败也必须停止并回收后台运行；裸 JoinHandle 在 Drop 时只会脱离，
/// 可能让 Git 继续操作即将被 TempDir 清理的仓库（G-15 / P-13）。
struct RunHandle {
    handle: Option<JoinHandle<RunOutcome>>,
    control: Arc<Control>,
    hold_dir: Option<PathBuf>,
}

impl RunHandle {
    fn join(mut self) -> std::thread::Result<RunOutcome> {
        self.handle.take().unwrap().join()
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.control.cancel();
            // 仅移除本用例包装器自有的暂停标记；自然结束当前命令后再回收。
            if let Some(dir) = &self.hold_dir {
                for sub in ["add", "fetch", "merge", "commit"] {
                    let flag = dir.join(format!("hold-{sub}.flag"));
                    if let Err(error) = fs::remove_file(&flag) {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            eprintln!("释放测试暂停标记 {} 失败：{error}", flag.display());
                        }
                    }
                }
            }
            let _ = handle.join();
        }
    }
}

/// 后台启动一次工具运行（F13/F14 回归用）：返回日志与共享状态的并发句柄和
/// 可 join、且在断言失败时自动清场的 RunHandle。
fn spawn_run(
    git: &Path,
    repo: &Path,
    control: &Arc<Control>,
) -> (Arc<Mutex<Vec<String>>>, Arc<GitShared>, RunHandle) {
    let shared = Arc::new(GitShared::new());
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    // 看门狗在运行线程内保活，随该次运行退出回收。
    let sink = Arc::clone(&logs);
    let repo = repo.to_path_buf();
    let hold_dir = if git.file_name() == Some(std::ffi::OsStr::new("gitp.cmd")) {
        git.parent().map(Path::to_path_buf)
    } else {
        None
    };
    let git = git.to_path_buf();
    let control_for_run = Arc::clone(control);
    let shared_for_run = Arc::clone(&shared);
    let handle = std::thread::spawn(move || {
        let _watchdog = RunWatchdog::new(&control_for_run);
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
    (
        logs,
        shared,
        RunHandle {
            handle: Some(handle),
            control: Arc::clone(control),
            hold_dir,
        },
    )
}

/// 轮询等待日志中出现包含 needle 的条目（最多 120 秒；超时 panic 带全部日志）。
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

/// 合并是否发生由真实提交的父关系判断，不依赖可变的提交标题（G-10/G-11）。
fn assert_merge_tip(repo: &Path, branch: &str) {
    let parents = git_ok(repo, &["rev-list", "--parents", "-n", "1", branch]);
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "{branch} 顶端必须是具有两个父提交的合并提交：{parents}"
    );
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
    "set /p REALGIT=<\"%WDIR%real-git.txt\"\r\n",
    "set \"SUB=%~1\"\r\n",
    "if /I \"%SUB%\"==\"--literal-pathspecs\" set \"SUB=%~2\"\r\n",
    "if /I \"%~1\"==\"--literal-pathspecs\" (\r\n",
    "  >>\"%WDIR%trace.txt\" echo %SUB% %*\r\n",
    ") else (\r\n",
    "  >>\"%WDIR%trace.txt\" echo %*\r\n",
    ")\r\n",
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
    // 预建自有轨迹文件，使后续读取失败不能伪装成「没有启动后续命令」。
    fs::write(dir.join("trace.txt"), b"").unwrap();
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
        .expect("必须读到包装器命令轨迹，读取失败不等于没有后续命令")
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

fn fixture_with_submodule() -> (Fixture, PathBuf) {
    let fix = fixture();
    let subremote = add_second_remote(&fix, "subremote");
    git_ok(&fix.repo, &["push", "-q", "subremote", "master"]);
    git_ok(
        &fix.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &subremote.display().to_string(),
            "child",
        ],
    );
    git_ok(&fix.repo, &["commit", "-q", "-m", "prepare submodule"]);
    git_ok(&fix.repo, &["push", "-q"]);
    (fix, subremote)
}

// 包装器仅让指定阶段首败一次，其余命令透传真实 Git，轨迹可核对阶段记忆。
fn fail_once_wrapper(dir: &Path, stage: &str) -> PathBuf {
    let wrapper = git_wrapper(dir);
    let injected = format!(
        "if /I \"%SUB%\"==\"{stage}\" if not exist \"%WDIR%failed.flag\" (\r\n\
           >\"%WDIR%failed.flag\" echo failed\r\n\
           echo TRANSIENT-OUT-MARK\r\n\
           echo transient stage failure 1>&2\r\n\
           exit /b 1\r\n\
         )\r\n"
    );
    fs::write(
        &wrapper,
        GIT_WRAPPER_CMD.replace(
            "\"%REALGIT%\" %*\r\n",
            &format!("{injected}\"%REALGIT%\" %*\r\n"),
        ),
    )
    .unwrap();
    wrapper
}

// P2-01：实际 Git 改动索引/工作区，只替换失败的人类诊断；不改用户或 hooks 的语言。
#[cfg(windows)]
fn review_remediation_localized_git(dir: &Path, stage: &str, diagnostic: &str) -> PathBuf {
    let wrapper = git_wrapper(dir);
    fs::write(dir.join("localized.txt"), diagnostic.as_bytes()).unwrap();
    let script = GIT_WRAPPER_CMD.replace(
        "\"%REALGIT%\" %*\r\n",
        &format!(
            concat!(
                "if /I \"%SUB%\"==\"{stage}\" goto localized\r\n",
                "\"%REALGIT%\" %*\r\n",
                "exit /b %ERRORLEVEL%\r\n",
                ":localized\r\n",
                "\"%REALGIT%\" %* >\"%WDIR%actual-out.txt\" 2>\"%WDIR%actual-err.txt\"\r\n",
                "set \"RESULT=%ERRORLEVEL%\"\r\n",
                "if not \"%RESULT%\"==\"0\" (\r\n",
                "  type \"%WDIR%localized.txt\" 1>&2\r\n",
                ") else (\r\n",
                "  type \"%WDIR%actual-out.txt\"\r\n",
                "  type \"%WDIR%actual-err.txt\" 1>&2\r\n",
                ")\r\n",
                "exit /b %RESULT%\r\n"
            ),
            stage = stage,
        ),
    );
    fs::write(&wrapper, script).unwrap();
    wrapper
}

// P2-01：无限重试是当前缺陷，首次退避即停止，避免红测靠三分钟看门狗收尾。
#[cfg(windows)]
fn review_remediation_run_without_retry(git: &Path, repo: &Path) -> RunOutcome {
    let control = Arc::new(Control::default());
    let shared = Arc::new(GitShared::new());
    let logs = std::cell::RefCell::new(Vec::new());
    let _watchdog = RunWatchdog::new(&control);
    let text = git_tools::run(
        git,
        repo,
        &control,
        &shared,
        &|line| {
            logs.borrow_mut().push(line.to_owned());
            if line.contains("次失败：等待") {
                control.cancel();
            }
        },
        &|_| {},
        TEST_UNIT,
    );
    RunOutcome {
        text,
        shared,
        logs: logs.into_inner(),
    }
}

fn review_remediation_chinese_conflict_fixture() -> Fixture {
    let fix = fixture();
    git_ok(&fix.repo, &["config", "core.quotePath", "true"]);
    fs::write(fix.repo.join("冲突 文件.txt"), "local version\n").unwrap();
    let base = fix.repo.parent().unwrap();
    git_ok(
        base,
        &["clone", "-q", &fix.remote.display().to_string(), "other"],
    );
    let other = base.join("other");
    fs::write(other.join("冲突 文件.txt"), "remote version\n").unwrap();
    git_ok(&other, &["add", "冲突 文件.txt"]);
    git_ok(&other, &["commit", "-q", "-m", "remote conflict"]);
    git_ok(&other, &["push", "-q"]);
    fix
}

fn review_remediation_assert_conflict_preserved(fix: &Fixture) {
    let unresolved = git_ok(&fix.repo, &["diff", "--name-only", "--diff-filter=U", "-z"]);
    assert_eq!(unresolved, "冲突 文件.txt\0");
    let content = fs::read_to_string(fix.repo.join("冲突 文件.txt")).unwrap();
    assert!(
        content.contains("local version") && content.contains("remote version"),
        "{content}"
    );
    assert!(fix.repo.join(".git/MERGE_HEAD").is_file());
    assert_eq!(
        git_ok(&fix.repo, &["log", "-1", "--format=%s"]).trim(),
        "update: 冲突 文件.txt"
    );
    assert_eq!(
        git_ok(&fix.remote, &["log", "-1", "--format=%s"]).trim(),
        "remote conflict"
    );
}

/// review F23～F26：使用真实 Git 与既有毫秒退避注入；达到指定退避次数便同步取消，
/// 避免不可提交状态的红测依赖墙钟超时。正常路径不触发取消。
fn review_run_with_retry_limit(repo: &Path, retry_limit: u64) -> RunOutcome {
    let control = Arc::new(Control::default());
    let shared = Arc::new(GitShared::new());
    let logs = std::cell::RefCell::new(Vec::new());
    let _watchdog = RunWatchdog::new(&control);
    let text = git_tools::run(
        &git_exe(),
        repo,
        &control,
        &shared,
        &|line| {
            logs.borrow_mut().push(line.to_owned());
            if shared.retry.load(std::sync::atomic::Ordering::Relaxed) >= retry_limit {
                control.cancel();
            }
        },
        &|_| {},
        TEST_UNIT,
    );
    RunOutcome {
        text,
        shared,
        logs: logs.into_inner(),
    }
}

fn review_assert_dirty_submodule_is_not_retried(untracked: bool) {
    let (fix, _) = fixture_with_submodule();
    let child = fix.repo.join("child");
    let child_head = git_ok(&child, &["rev-parse", "HEAD"]);
    let parent_head = git_ok(&fix.repo, &["rev-parse", "HEAD"]);
    let remote_before = git_ok(&fix.remote, &["rev-parse", "master"]);
    let path = if untracked { "new.txt" } else { "README.md" };
    fs::write(child.join(path), "child-only dirty content\n").unwrap();
    fs::write(fix.repo.join("z-after.txt"), "pending parent change\n").unwrap();
    assert_eq!(git_ok(&child, &["rev-parse", "HEAD"]), child_head);
    assert!(
        git_ok(&fix.repo, &["status", "--porcelain=v1", "--", "child"])
            .contains("child"),
        "父仓库必须可观察到内部 dirty"
    );

    let outcome = review_run_with_retry_limit(&fix.repo, 1);
    assert_eq!(
        outcome.shared.retry.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "gitlink 未变的内部 dirty 不可由父仓库提交，不得进入可重试 commit：{}",
        outcome.text
    );
    assert_eq!(outcome.shared.state.lock().unwrap().as_str(), "失败");
    assert!(outcome.text.contains("child"), "必须定位不能提交的子模块");
    assert_eq!(git_ok(&child, &["rev-parse", "HEAD"]), child_head);
    assert_eq!(git_ok(&fix.repo, &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(
        git_ok(&fix.remote, &["rev-parse", "master"]),
        remote_before
    );
    assert_eq!(
        fs::read_to_string(child.join(path)).unwrap(),
        "child-only dirty content\n"
    );
    assert!(
        git_ok(&fix.repo, &["diff", "--cached", "--name-only", "-z"]).is_empty(),
        "不得试图把子模块内部内容或后续父仓库文件暂存"
    );
    assert_eq!(
        git_ok(&fix.repo, &["status", "--porcelain=v1", "--", "z-after.txt"]).trim(),
        "?? z-after.txt",
        "必须停止而非跳过子模块处理后续文件"
    );
}
