//! Git 工具核心逻辑（合同 G 分区）：逐文件提交并推送。
//! 严格串行：一个文件 → 单独 git add → 单独 commit（按路径限定提交范围，保护既有
//! staged 内容，G-06）→ 验证单文件（G-05）→ 单独 push（G-08）→ 成功后才处理下一个
//! 文件（G-03）。push 被远端新提交拒绝时自动 fetch + merge（G-10，不 rebase）；
//! 冲突停止并保留现场（G-11）；可重试失败按 5→10→20→40→80→80… 秒无限退避（G-09），
//! 用户可随时停止（G-15）；以 git 状态为唯一进度来源（G-12）。
//! 网络访问仅限当前分支 upstream（P-03 的 Git 工具例外）。

use anyhow::{bail, Context as _, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use crate::{
    control::Control,
    process::{self, CapturedOutput},
};

/// 单条 git 命令的总超时：超时按可重试失败处理（G-09），不永久挂死任务。
const GIT_TIMEOUT: Duration = Duration::from_secs(600);
/// 生产退避基准（第 1 次失败等待 5 秒，之后逐次翻倍，第 5 次起固定 80 秒；测试注入更小值）。
pub const BACKOFF_UNIT: Duration = Duration::from_secs(5);

/// 界面共享进度（G-13）：worker 线程写、GUI 轮询读。
#[derive(Default)]
pub struct GitShared {
    pub repo: Mutex<String>,
    pub branch: Mutex<String>,
    pub upstream: Mutex<String>,
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub current: Mutex<String>,
    pub stage: Mutex<String>,
    pub retry: AtomicU64,
    /// 下一次重试的剩余秒数（等待期间倒计时）
    pub retry_wait: AtomicU64,
    pub state: Mutex<String>,
}

impl GitShared {
    pub fn new() -> Self {
        Self {
            repo: Mutex::new(String::new()),
            branch: Mutex::new(String::new()),
            upstream: Mutex::new(String::new()),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            stage: Mutex::new("扫描".into()),
            retry: AtomicU64::new(0),
            retry_wait: AtomicU64::new(0),
            state: Mutex::new("未开始".into()),
        }
    }
    fn set_stage(&self, stage: &str) {
        if let Ok(mut slot) = self.stage.lock() {
            slot.clear();
            slot.push_str(stage);
        }
    }
    fn set_state(&self, state: &str) {
        if let Ok(mut slot) = self.state.lock() {
            slot.clear();
            slot.push_str(state);
        }
    }
    fn set_current(&self, current: &str) {
        if let Ok(mut slot) = self.current.lock() {
            slot.clear();
            slot.push_str(current);
        }
    }
    fn set_text(slot: &Mutex<String>, value: &str) {
        if let Ok(mut guard) = slot.lock() {
            guard.clear();
            guard.push_str(value);
        }
    }
}

/// 仓库基本信息（G-02 验证结果）。
#[derive(Debug)]
pub struct RepoInfo {
    /// 仓库根目录（git rev-parse --show-toplevel）
    pub root: PathBuf,
    /// 当前检出的分支名
    pub branch: String,
    /// 当前分支已配置的 upstream（如 origin/main）
    pub upstream: String,
}

/// 解析 git 可执行文件绝对路径：优先常见安装位置，再遍历 PATH；
/// 不依赖当前目录，避免仓库内同名程序劫持。
pub fn find_git() -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(program_files) = std::env::var("ProgramFiles") {
        candidates.push(
            Path::new(&program_files)
                .join("Git")
                .join("cmd")
                .join("git.exe"),
        );
    }
    if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
        candidates.push(
            Path::new(&program_files_x86)
                .join("Git")
                .join("cmd")
                .join("git.exe"),
        );
    }
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        candidates.push(
            Path::new(&local_appdata)
                .join("Programs")
                .join("Git")
                .join("cmd")
                .join("git.exe"),
        );
    }
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    if let Ok(paths) = std::env::var("PATH") {
        for dir in paths.split(';') {
            let dir = dir.trim();
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("git.exe");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!("找不到 git.exe：请安装 Git for Windows 后重试")
}

/// 运行一条 git 命令并捕获输出（无 shell；禁终端提示防无 tty 挂死；
/// 认证经 credential manager 等 GUI 途径正常交互）。
fn run_git(git: &Path, cwd: &Path, args: &[&str]) -> Result<CapturedOutput> {
    let mut command = Command::new(git);
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0");
    process::run_with_timeout(&mut command, GIT_TIMEOUT)
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// 运行并要求退出码为 0，否则把 stderr 摘要并入错误（U-13 可定位）。
fn run_git_ok(git: &Path, cwd: &Path, args: &[&str]) -> Result<String> {
    let subcommand = args.first().copied().unwrap_or("");
    let out = run_git(git, cwd, args).with_context(|| format!("执行 git {subcommand} 失败"))?;
    if !out.status.success() {
        bail!(
            "git {subcommand} 失败（退出码 {}）：{}",
            out.status.code().unwrap_or(-1),
            summarize(&output_text(&out.stderr), &output_text(&out.stdout))
        );
    }
    Ok(output_text(&out.stdout))
}

/// 失败摘要：优先 stderr，保留完整阶段的可读上下文（G-14）。
fn summarize(stderr: &str, stdout: &str) -> String {
    let pick = |text: &str| {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(8)
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let chosen = pick(stderr);
    if chosen.is_empty() {
        pick(stdout)
    } else {
        chosen
    }
}

/// 一个逻辑文件变更（G-04）：Git 已识别的 rename 作为一个变更。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Added(String),
    Modified(String),
    Deleted(String),
    /// （旧路径, 新路径）
    Renamed(String, String),
}

impl FileChange {
    /// add 阶段需要暂存的路径集合。rename（git mv 已把删除与新增写入 index）只暂存
    /// 新路径：旧路径在工作树与 index 中均已不存在，`git add -- 旧路径` 会因 pathspec
    /// 无匹配而失败；旧路径的删除由 commit --only 从 HEAD 与工作树状态带入提交。
    fn stage_paths(&self) -> Vec<String> {
        match self {
            FileChange::Renamed(_, new) => vec![new.clone()],
            FileChange::Added(path) | FileChange::Modified(path) | FileChange::Deleted(path) => {
                vec![path.clone()]
            }
        }
    }
    /// commit 阶段（--only）与单文件验证（G-05）使用的路径集合：rename 含新旧两路径。
    fn commit_paths(&self) -> Vec<String> {
        match self {
            FileChange::Renamed(old, new) => vec![old.clone(), new.clone()],
            FileChange::Added(path) | FileChange::Modified(path) | FileChange::Deleted(path) => {
                vec![path.clone()]
            }
        }
    }
    /// commit message 使用的路径（G-07：相对仓库根目录；rename 用新路径）。
    fn display_path(&self) -> String {
        match self {
            FileChange::Renamed(_, new) => new.clone(),
            FileChange::Added(path) | FileChange::Modified(path) | FileChange::Deleted(path) => {
                path.clone()
            }
        }
    }
}

/// 仓库中间态（G-06/G-11）：影响能否安全保证单文件提交。
pub enum MiddleState {
    Clean,
    /// merge 进行中；unresolved 为未合并条目（空 = 用户已解决、等待完成合并提交）
    Merge {
        unresolved: Vec<String>,
    },
    Other(&'static str),
}

fn git_dir(git: &Path, repo: &Path) -> Result<PathBuf> {
    let text = run_git_ok(git, repo, &["rev-parse", "--git-path", "."])?;
    let dir = PathBuf::from(text.trim());
    Ok(if dir.is_absolute() {
        dir
    } else {
        repo.join(dir)
    })
}

/// 检查仓库中间态（G-06：无法安全保证时拒绝；G-11：merge 冲突等待用户）。
pub fn middle_state(git: &Path, repo: &Path) -> Result<MiddleState> {
    let dir = git_dir(git, repo)?;
    if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
        return Ok(MiddleState::Other("rebase"));
    }
    if dir.join("CHERRY_PICK_HEAD").exists() {
        return Ok(MiddleState::Other("cherry-pick"));
    }
    if dir.join("REVERT_HEAD").exists() {
        return Ok(MiddleState::Other("revert"));
    }
    if dir.join("MERGE_HEAD").exists() {
        let text = run_git_ok(git, repo, &["diff", "--name-only", "--diff-filter=U"])?;
        let unresolved = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        return Ok(MiddleState::Merge { unresolved });
    }
    Ok(MiddleState::Clean)
}

/// 验证仓库并读取基本信息（G-02）：目录存在、有效仓库、有检出分支、有 upstream。
pub fn inspect(git: &Path, repo: &Path) -> Result<RepoInfo> {
    let top = run_git_ok(git, repo, &["rev-parse", "--show-toplevel"])
        .map_err(|error| anyhow::anyhow!("不是有效的 Git repository：{error:#}"))?;
    let root = PathBuf::from(top.trim());
    let branch = run_git_ok(git, repo, &["symbolic-ref", "--short", "HEAD"]).map_err(|_| {
        anyhow::anyhow!("当前没有检出的 branch（处于 detached HEAD），无法使用本工具")
    })?;
    let upstream = run_git_ok(
        git,
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .map_err(|_| anyhow::anyhow!("当前 branch「{}」未配置 upstream，无法推送", branch.trim()))?;
    Ok(RepoInfo {
        root,
        branch: branch.trim().to_owned(),
        upstream: upstream.trim().to_owned(),
    })
}

/// 扫描当前未提交变更（G-12：以 git status 为准，不做深度文件遍历）。
/// untracked 目录按 `--untracked-files=all` 展开到文件。
pub fn status_changes(git: &Path, root: &Path) -> Result<Vec<FileChange>> {
    let out = run_git(
        git,
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !out.status.success() {
        bail!(
            "git status 失败：{}",
            summarize(&output_text(&out.stderr), &output_text(&out.stdout))
        );
    }
    parse_status(&output_text(&out.stdout))
}

/// 取 rename/copy 记录的第二段（旧路径）：porcelain -z 中此类记录形如
/// 「XY 新路径\0旧路径\0」，缺段说明输出异常，拒绝继续解析。
fn take_origin_path(segments: &[&str], index: &mut usize, new_path: &str) -> Result<String> {
    match segments.get(*index) {
        Some(old) if !old.is_empty() => {
            *index += 1;
            Ok((*old).to_owned())
        }
        _ => bail!("解析 git status 的 rename/copy 记录失败：{new_path}"),
    }
}

/// 解析 `git status --porcelain=v1 -z` 的输出（G-12）。
/// pub 供集成测试以合成输入验证各类记录形状（部分形状无法用真实 git 命令构造）。
pub fn parse_status(data: &str) -> Result<Vec<FileChange>> {
    let segments: Vec<&str> = data.split('\0').filter(|s| !s.is_empty()).collect();
    let mut changes = Vec::new();
    let mut index = 0;
    while index < segments.len() {
        let record = segments[index];
        index += 1;
        let bytes = record.as_bytes();
        if bytes.len() < 4 {
            continue;
        }
        let (x, y) = (bytes[0], bytes[1]);
        let path = record[3..].to_owned();
        if x == b'U' || y == b'U' || (x == b'A' && y == b'A') || (x == b'D' && y == b'D') {
            bail!("仓库存在未解决的冲突条目：{path}；请先解决冲突并完成或中止合并（G-11）");
        }
        let change = match (x, y) {
            (b'?', b'?') => FileChange::Added(path),
            // G-04：Git 已识别的 rename 作为一个逻辑变更处理。两列都可能给出 R：
            // X='R' 是已暂存重命名（git mv 等），Y='R' 是工作区重命名（如先 add
            // 修改再在资源管理器改名的 MR 记录）。porcelain -z 中此类记录形如
            // 「XY 新路径\0旧路径\0」，旧路径段必须消费，否则会被当成下一条
            // 记录头串位误解析。
            (b'R', _) | (_, b'R') => {
                let old = take_origin_path(&segments, &mut index, &path)?;
                FileChange::Renamed(old, path)
            }
            // 已暂存 copy（status.renames=copies）同样是「新路径\0旧路径\0」两段：
            // 消费旧路径段，按 G-04 归类为新增，避免串位。
            (b'C', _) => {
                take_origin_path(&segments, &mut index, &path)?;
                FileChange::Added(path)
            }
            (b' ', other) => match other {
                b'D' => FileChange::Deleted(path),
                b'M' | b'T' => FileChange::Modified(path),
                b' ' => continue,
                other => bail!("git status 出现未处理的变更状态：{}", char::from(other)),
            },
            (first, _) => match first {
                b'A' => FileChange::Added(path),
                b'D' => FileChange::Deleted(path),
                b'M' | b'T' => FileChange::Modified(path),
                other => bail!("git status 出现未处理的变更状态：{}", char::from(other)),
            },
        };
        changes.push(change);
    }
    Ok(changes)
}

/// 第 n 次重试的等待时长（G-09：5→10→20→40→80→80… 秒）。
/// `unit` 是第 1 次失败的等待基准（生产 [`BACKOFF_UNIT`] = 5 秒，封顶 80 秒 = 16×unit；
/// 测试注入更小基准等比缩放）。
pub fn backoff_duration(attempt: u64, unit: Duration) -> Duration {
    let factor = match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        _ => 16,
    };
    unit.saturating_mul(factor)
}

/// 可中断等待：100ms 粒度检查取消；按秒刷新剩余等待（G-13 的下一次重试等待时间）。
/// 返回 false 表示用户已取消。
fn cancellable_wait(control: &Control, wait: Duration, shared: &GitShared) -> bool {
    let step = Duration::from_millis(100);
    shared.retry_wait.store(
        wait.as_secs().max(u64::from(!wait.is_zero())),
        Ordering::Relaxed,
    );
    let mut elapsed = Duration::ZERO;
    while elapsed < wait {
        if control.is_cancelled() {
            shared.retry_wait.store(0, Ordering::Relaxed);
            return false;
        }
        let nap = step.min(wait.saturating_sub(elapsed));
        std::thread::sleep(nap);
        elapsed += nap;
        shared
            .retry_wait
            .store(wait.saturating_sub(elapsed).as_secs(), Ordering::Relaxed);
    }
    shared.retry_wait.store(0, Ordering::Relaxed);
    true
}

/// push 的结果分类。
enum PushOutcome {
    Ok,
    /// 远端有新提交（non-fast-forward / fetch first / stale info），需要 fetch + merge。
    NeedMerge,
    /// 可重试失败（网络、认证、超时等）。
    Retryable(String),
}

/// 一次 push 尝试（G-08：不带额外远程/分支参数，用已配置 upstream）。
fn try_push(git: &Path, root: &Path, log: &dyn Fn(&str)) -> PushOutcome {
    log("git push");
    let out = match run_git(git, root, &["push", "--porcelain"]) {
        Ok(out) => out,
        Err(error) => return PushOutcome::Retryable(format!("{error:#}")),
    };
    let stdout = output_text(&out.stdout);
    let stderr = output_text(&out.stderr);
    if out.status.success() && !stdout.lines().any(|line| line.starts_with('!')) {
        return PushOutcome::Ok;
    }
    let text = format!("{stderr}{stdout}");
    if text.contains("fetch first")
        || text.contains("non-fast-forward")
        || text.contains("stale info")
    {
        return PushOutcome::NeedMerge;
    }
    PushOutcome::Retryable(summarize(&stderr, &stdout))
}

/// fetch + merge 的结果分类（G-10）。
enum MergeOutcome {
    Merged,
    /// 自动合并冲突：停止并保留现场（G-11）。
    Conflict(Vec<String>),
    /// 被本地未提交变更阻挡（G-10 尾段：停止并显示原因）。
    BlockedByDirty(String),
    Retryable(String),
}

fn fetch_and_merge(git: &Path, root: &Path, upstream: &str, log: &dyn Fn(&str)) -> MergeOutcome {
    let Some((remote, branch)) = upstream.rsplit_once('/') else {
        return MergeOutcome::Retryable(format!("无法解析 upstream「{upstream}」"));
    };
    log(&format!("git fetch {remote} {branch}"));
    if let Err(error) = run_git_ok(git, root, &["fetch", remote, branch]) {
        return MergeOutcome::Retryable(format!("{error:#}"));
    }
    log("git merge FETCH_HEAD");
    let out = match run_git(git, root, &["merge", "FETCH_HEAD"]) {
        Ok(out) => out,
        Err(error) => return MergeOutcome::Retryable(format!("{error:#}")),
    };
    if out.status.success() {
        return MergeOutcome::Merged;
    }
    let stderr = output_text(&out.stderr);
    let stdout = output_text(&out.stdout);
    let text = format!("{stderr}{stdout}");
    if text.contains("CONFLICT") {
        // G-14：冲突时必须把 git merge 的输出写进日志（含 CONFLICT 与冲突文件名），
        // 用户要靠它判断卡在哪一步；不能只挑一边流——CONFLICT 行可能落在 stdout，
        // 也不能只给「失败」级别的信息。多行日志条目与 run() 的仓库信息同款。
        log(&format!("git merge 冲突输出：\n{stderr}{stdout}"));
        let unresolved = run_git_ok(git, root, &["diff", "--name-only", "--diff-filter=U"])
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return MergeOutcome::Conflict(unresolved);
    }
    if text.contains("local changes") || text.contains("would be overwritten by merge") {
        return MergeOutcome::BlockedByDirty(summarize(&stderr, &stdout));
    }
    MergeOutcome::Retryable(summarize(&stderr, &stdout))
}

/// 验证 HEAD 提交只包含指定路径（G-05 硬性要求）。
fn verify_single_path_commit(git: &Path, root: &Path, paths: &[String]) -> Result<bool> {
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
        return Ok(true);
    }
    bail!(
        "提交验证失败（G-05）：本次 commit 实际包含 {} 个路径（{:?}），预期只包含 {:?}",
        seen.len(),
        seen,
        expected
    )
}

/// 单个文件变更的处理结果。
enum StepOutcome {
    Done,
    Cancelled,
    Conflict(Vec<String>),
    Fatal(String),
}

/// 逐文件处理的公共上下文：任务控制、界面共享状态、日志回调与退避基准。
struct Ctx<'a> {
    control: &'a Control,
    shared: &'a GitShared,
    log: &'a dyn Fn(&str),
    unit: Duration,
}

impl Ctx<'_> {
    /// 失败后按退避等待；返回 false 表示用户已取消（G-09/G-15）。
    fn wait_retry(&self, attempt: &mut u64) -> bool {
        *attempt += 1;
        self.shared.retry.store(*attempt, Ordering::Relaxed);
        self.shared.set_stage("等待重试");
        let wait = backoff_duration(*attempt, self.unit);
        let attempt = *attempt;
        let secs = wait.as_secs_f64();
        (self.log)(&format!(
            "第 {attempt} 次失败：等待 {secs:.1} 秒后重试（G-09 无限重试，可随时停止）"
        ));
        let ok = cancellable_wait(self.control, wait, self.shared);
        if ok {
            self.shared.set_stage("重试中");
        }
        ok
    }
}

/// 处理一个文件变更：add → commit（--only，保护既有 staged，G-06）→ 验证（G-05）→
/// push（含 NeedMerge 的 fetch+merge 与无限退避重试，G-08~G-11）。
fn process_change(
    git: &Path,
    root: &Path,
    change: &FileChange,
    upstream: &str,
    ctx: &Ctx<'_>,
) -> StepOutcome {
    let stage_paths = change.stage_paths();
    let commit_paths = change.commit_paths();
    let display = change.display_path();
    let message = format!("update: {display}");
    let mut added = false;
    let mut attempt = 0u64;
    loop {
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        if !added {
            ctx.shared.set_stage("add");
            let mut args: Vec<&str> = vec!["add", "--"];
            for path in &stage_paths {
                args.push(path.as_str());
            }
            (ctx.log)(&format!("git add -- {}", stage_paths.join(" ")));
            match run_git(git, root, &args) {
                Ok(out) if out.status.success() => added = true,
                Ok(out) => {
                    let why = summarize(&output_text(&out.stderr), &output_text(&out.stdout));
                    (ctx.log)(&format!("git add 失败：{why}"));
                    if !ctx.wait_retry(&mut attempt) {
                        return StepOutcome::Cancelled;
                    }
                    continue;
                }
                Err(error) => {
                    (ctx.log)(&format!("git add 失败：{error:#}"));
                    if !ctx.wait_retry(&mut attempt) {
                        return StepOutcome::Cancelled;
                    }
                    continue;
                }
            }
        }
        {
            ctx.shared.set_stage("commit");
            let mut args: Vec<&str> = vec!["commit", "--only", "-m", &message, "--"];
            for path in &commit_paths {
                args.push(path.as_str());
            }
            (ctx.log)(&format!(
                "git commit --only -m \"{message}\" -- {}",
                commit_paths.join(" ")
            ));
            match run_git(git, root, &args) {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    let stderr = output_text(&out.stderr);
                    let stdout = output_text(&out.stdout);
                    let text = format!("{stderr}{stdout}");
                    // 身份未配置是确定性配置错误，重试永远不会成功（不属于 G-09 的
                    // 可重试失败）：停止并给出可操作的修复提示。
                    if text.contains("Author identity unknown")
                        || text.contains("Committer identity unknown")
                        || text.contains("Please tell me who you are")
                    {
                        return StepOutcome::Fatal(
                            "git 未配置提交身份（user.name / user.email），无法提交；请先在仓库或全局配置 git 身份后重新开始任务"
                                .into(),
                        );
                    }
                    // 命令超时误判等场景：提交可能实际已完成。按 git 状态核实：
                    // 该路径不再有未提交变更即视为已提交（G-09 阶段记忆）。
                    let settled = text.contains("no changes added to commit")
                        || text.contains("nothing to commit")
                        || text.contains("nothing added to commit");
                    if settled {
                        let mut check: Vec<&str> = vec!["status", "--porcelain", "--"];
                        for path in &commit_paths {
                            check.push(path.as_str());
                        }
                        match run_git_ok(git, root, &check) {
                            Ok(text) if text.trim().is_empty() => {}
                            Ok(_) => {
                                (ctx.log)("commit 未生效（路径仍有未提交变更），准备重试");
                                if !ctx.wait_retry(&mut attempt) {
                                    return StepOutcome::Cancelled;
                                }
                                continue;
                            }
                            Err(error) => {
                                return StepOutcome::Fatal(format!("核实提交状态失败：{error:#}"));
                            }
                        }
                    } else {
                        (ctx.log)(&format!("git commit 失败：{}", summarize(&stderr, &stdout)));
                        if !ctx.wait_retry(&mut attempt) {
                            return StepOutcome::Cancelled;
                        }
                        continue;
                    }
                }
                Err(error) => {
                    (ctx.log)(&format!("git commit 失败：{error:#}"));
                    if !ctx.wait_retry(&mut attempt) {
                        return StepOutcome::Cancelled;
                    }
                    continue;
                }
            }
            // G-05：验证本次 commit 只包含当前文件变更
            match verify_single_path_commit(git, root, &commit_paths) {
                Ok(true) => {}
                Ok(false) => return StepOutcome::Fatal("提交验证未通过".into()),
                Err(error) => {
                    return StepOutcome::Fatal(format!(
                        "{error:#}（当前文件：{display}，阶段：commit 验证）"
                    ));
                }
            }
        }
        let outcome = push_with_retry(git, root, upstream, ctx, &mut attempt);
        if matches!(outcome, StepOutcome::Done) {
            (ctx.log)(&format!("push 成功：{display}"));
        }
        return outcome;
    }
}

/// push 当前分支（G-08~G-11）：NeedMerge 时自动 fetch+merge 后重试 push（不 rebase），
/// 可重试失败按退避无限重试；供 process_change 的单文件流程与冲突续接的合并提交共用。
fn push_with_retry(
    git: &Path,
    root: &Path,
    upstream: &str,
    ctx: &Ctx<'_>,
    attempt: &mut u64,
) -> StepOutcome {
    loop {
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        ctx.shared.set_stage("push");
        match try_push(git, root, ctx.log) {
            PushOutcome::Ok => return StepOutcome::Done,
            PushOutcome::NeedMerge => {
                ctx.shared.set_stage("pull/fetch");
                match fetch_and_merge(git, root, upstream, ctx.log) {
                    // 合并成功：回到循环重试 push（不消耗重试计数，G-10）
                    MergeOutcome::Merged => {}
                    MergeOutcome::Conflict(files) => return StepOutcome::Conflict(files),
                    MergeOutcome::BlockedByDirty(reason) => {
                        return StepOutcome::Fatal(format!(
                            "合并被本地未提交变更阻挡：{reason}（阶段：merge）"
                        ));
                    }
                    MergeOutcome::Retryable(reason) => {
                        (ctx.log)(&format!("fetch/merge 失败：{reason}"));
                        if !ctx.wait_retry(attempt) {
                            return StepOutcome::Cancelled;
                        }
                    }
                }
            }
            PushOutcome::Retryable(reason) => {
                (ctx.log)(&format!("push 失败：{reason}"));
                if !ctx.wait_retry(attempt) {
                    return StepOutcome::Cancelled;
                }
            }
        }
    }
}

/// 主入口：逐文件提交并推送（G-03~G-16）。返回收尾文案（状态与统计），
/// 详细过程经 `log`/`status` 回调实时上报（G-14）。
/// `unit` 为退避基准（生产用 [`BACKOFF_UNIT`]，测试注入小值）。
pub fn run(
    git: &Path,
    repo: &Path,
    control: &Control,
    shared: &Arc<GitShared>,
    log: &dyn Fn(&str),
    status: &dyn Fn(&str),
    unit: Duration,
) -> String {
    shared.set_state("检查仓库");
    shared.set_stage("扫描");
    status("正在验证仓库（分支、upstream 与仓库状态）");
    let info = match inspect(git, repo) {
        Ok(info) => info,
        Err(error) => {
            let text = format!("{error:#}");
            log(&text);
            shared.set_state("失败");
            return format!("无法开始：{text}");
        }
    };
    GitShared::set_text(&shared.repo, &info.root.display().to_string());
    GitShared::set_text(&shared.branch, &info.branch);
    GitShared::set_text(&shared.upstream, &info.upstream);
    log(&format!(
        "仓库：{}\n分支：{}\nupstream：{}",
        info.root.display(),
        info.branch,
        info.upstream
    ));
    // 中间态处理（G-06/G-11）
    match middle_state(git, repo) {
        Ok(MiddleState::Clean) => {}
        Ok(MiddleState::Merge { unresolved }) => {
            if unresolved.is_empty() {
                // 用户已在冲突后解决并暂存：完成合并提交后继续（G-11 续段）。
                // 合并提交必须推送成功才算完成当前任务，然后继续扫描剩余变更。
                shared.set_stage("merge");
                log("检测到已解决的合并：完成合并提交（git commit --no-edit）");
                if let Err(error) = run_git_ok(git, repo, &["commit", "--no-edit"]) {
                    let text = format!("完成合并提交失败：{error:#}");
                    log(&text);
                    shared.set_state("失败");
                    return text;
                }
                let ctx = Ctx {
                    control,
                    shared,
                    log,
                    unit,
                };
                let mut attempt = 0u64;
                match push_with_retry(git, repo, &info.upstream, &ctx, &mut attempt) {
                    StepOutcome::Done => {
                        log("合并提交已 push 成功；继续扫描剩余变更");
                    }
                    StepOutcome::Cancelled => {
                        shared.set_state("已停止");
                        return "任务已停止：合并提交已在本地完成，尚未推送".into();
                    }
                    StepOutcome::Conflict(files) => {
                        shared.set_state("冲突");
                        shared.set_stage("冲突");
                        let text = format!(
                            "合并提交的推送再次遇到冲突（{} 个文件：{}）；请解决后重新开始任务（G-11）",
                            files.len(),
                            files.join("、")
                        );
                        log(&text);
                        return text;
                    }
                    StepOutcome::Fatal(reason) => {
                        shared.set_state("失败");
                        let text = format!("合并提交推送失败：{reason}");
                        log(&text);
                        return text;
                    }
                }
            } else {
                shared.set_state("冲突");
                shared.set_stage("冲突");
                let text = format!(
                    "仓库存在未解决的合并冲突（{} 个文件：{}）；请在仓库中解决冲突后重新开始任务，已成功推送的文件不会被重复提交（G-11/G-12）",
                    unresolved.len(),
                    unresolved.join("、")
                );
                log(&text);
                return text;
            }
        }
        Ok(MiddleState::Other(kind)) => {
            let text = format!(
                "仓库处于未完成的 {kind}，无法安全保证单文件提交；请先手动完成或中止（G-06）"
            );
            log(&text);
            shared.set_state("失败");
            return text;
        }
        Err(error) => {
            let text = format!("检查仓库状态失败：{error:#}");
            log(&text);
            shared.set_state("失败");
            return text;
        }
    }
    if control.is_cancelled() {
        shared.set_state("已停止");
        return "任务已停止：尚未开始处理文件".into();
    }
    // 扫描（G-12）
    shared.set_stage("扫描");
    status("正在扫描未提交变更（以 git 状态为准）");
    let changes = match status_changes(git, &info.root) {
        Ok(changes) => changes,
        Err(error) => {
            let text = format!("{error:#}");
            log(&text);
            shared.set_state("失败");
            return format!("扫描失败：{text}");
        }
    };
    let total = changes.len() as u64;
    shared.total.store(total, Ordering::Relaxed);
    shared.done.store(0, Ordering::Relaxed);
    log(&format!("待处理变更：{total} 个逻辑文件变更"));
    if changes.is_empty() {
        shared.set_state("完成");
        shared.set_stage("完成");
        return "没有需要提交的变更：工作区与远端一致".into();
    }
    let mut done = 0u64;
    let ctx = Ctx {
        control,
        shared,
        log,
        unit,
    };
    for change in &changes {
        if control.is_cancelled() {
            break;
        }
        shared.set_current(&change.display_path());
        log(&format!("开始处理：{}", change.display_path()));
        match process_change(git, &info.root, change, &info.upstream, &ctx) {
            StepOutcome::Done => {
                done += 1;
                shared.done.store(done, Ordering::Relaxed);
                status(&format!(
                    "已完成 {done} / {total}：{}",
                    change.display_path()
                ));
            }
            StepOutcome::Cancelled => break,
            StepOutcome::Conflict(files) => {
                shared.set_state("冲突");
                shared.set_stage("冲突");
                let text = format!(
                    "自动合并出现冲突（当前文件：{}，阶段：merge）：冲突文件 {} 个（{}）。已停止自动处理并保留冲突现场；请在仓库中解决冲突后重新开始任务继续（G-11）",
                    change.display_path(),
                    files.len(),
                    files.join("、")
                );
                log(&text);
                return text;
            }
            StepOutcome::Fatal(reason) => {
                shared.set_state("失败");
                let text = format!(
                    "任务停止：{reason}（已完成 {done} / {total}；成功推送的文件保持成功）"
                );
                log(&text);
                return text;
            }
        }
    }
    if control.is_cancelled() {
        shared.set_state("已停止");
        return format!(
            "任务已停止：已完成 {done} / {total}；已成功 commit + push 的文件保持成功状态（G-15）"
        );
    }
    shared.set_state("完成");
    shared.set_stage("完成");
    let text = format!("全部完成：{done} / {total} 个文件已逐个 commit 并 push 成功");
    log(&text);
    text
}
