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
        Self::set_text(&self.stage, stage);
    }
    fn set_state(&self, state: &str) {
        Self::set_text(&self.state, state);
    }
    fn set_current(&self, current: &str) {
        Self::set_text(&self.current, current);
    }
    /// 当前任务终态（供收尾日志读取；锁中毒时返回空串，不影响业务）。
    fn state(&self) -> String {
        self.state
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
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
    /// upstream 远端名（`branch.<b>.remote`；本地 upstream 时为 "."）。
    /// F09：远端/分支不再从 upstream 显示串拆分——分支名可含斜杠，
    /// `origin/feature/demo` 会被 `rsplit_once('/')` 拆成远端「origin/feature」、
    /// 分支「demo」，fetch 必然 128 失败并按 G-09 无限退避；本地 upstream 的
    /// 显示串（如 master）甚至不含斜杠。
    pub upstream_remote: String,
    /// upstream 分支名（`branch.<b>.merge` 去 refs/heads/ 前缀，可含斜杠）
    pub upstream_branch: String,
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
    run_git_with(git, cwd, args, &[], &[])
}

/// [`run_git`] 的底层形态：可附加环境变量与移除环境变量（P-09 的代理注入
/// 与直连回退使用），其余行为一致。
fn run_git_with(
    git: &Path,
    cwd: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
    remove_env: &[&str],
) -> Result<CapturedOutput> {
    let mut command = Command::new(git);
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        // G-04/G-05：传入的路径是 porcelain 输出的字面文件名，必须按字面匹配；
        // 否则含 `[...]` 等字符的合法 Windows 文件名会被 pathspec 的通配语义
        // 解释成字符类，导致 pathspec 不匹配而无限重试、或错误暂存兄弟文件。
        .env("GIT_LITERAL_PATHSPECS", "1");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    for key in remove_env {
        command.env_remove(key);
    }
    process::run_with_timeout(&mut command, GIT_TIMEOUT)
}

/// P-09：网络类 git 命令（fetch / push）的统一入口。系统代理开启时注入
/// 代理环境变量；目标 URL 命中系统代理例外表（`<local>`、`192.168.*` 等
/// `no_proxy` 表达不了的条目）时不注入、按现状直连；命令失败且输出符合连接类
/// 失败特征（或整体超时）时，自动回退直连重试一次——移除全部代理变量（含继承
/// 自用户环境的），直连仍失败按原口径返回失败。系统代理关闭时不注入也不清除，
/// 行为与现状一致。`target_url` 为空（未知目标）时只走注入/回退路径。
fn run_git_network(
    git: &Path,
    cwd: &Path,
    args: &[&str],
    target_url: &str,
    log: &dyn Fn(&str),
    control: &Control,
) -> Result<CapturedOutput> {
    let proxy = crate::system_proxy::read();
    run_git_network_with_proxy(git, cwd, args, target_url, log, control, &proxy)
}

fn run_git_network_with_proxy(
    git: &Path,
    cwd: &Path,
    args: &[&str],
    target_url: &str,
    log: &dyn Fn(&str),
    control: &Control,
    proxy: &crate::system_proxy::SystemProxy,
) -> Result<CapturedOutput> {
    control.check_cancelled()?;
    if !proxy.is_enabled() {
        return run_git(git, cwd, args);
    }
    if !target_url.is_empty() && proxy.bypassed(target_url) {
        // 口径：命中例外表 = 该目标不经**系统代理**；用户在自身环境里显式设置的
        // http_proxy 等变量保持现状、不清除（与 P-09「系统代理关闭时不注入也不
        // 清除」同口径），只有注入/回退路径的直连重试才移除全部代理变量。
        log("目标命中 Windows 系统代理例外表：不经系统代理，直连");
        control.check_cancelled()?;
        return run_git(git, cwd, args);
    }
    let proxy_env = proxy.git_env();
    let injected: Vec<(&str, &str)> = proxy_env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let attempt = run_git_with(git, cwd, args, &injected, &[]);
    let needs_direct_retry = match &attempt {
        // 启动失败或超时：超时多因代理黑洞，直连重试值得一次；启动失败
        // 重试代价仅毫秒级，同样无害。
        Err(_) => true,
        Ok(out) => {
            if out.status.success() {
                false
            } else {
                let text = format!("{}{}", output_text(&out.stderr), output_text(&out.stdout));
                crate::system_proxy::network_failure_signature(&text)
            }
        }
    };
    if !needs_direct_retry || control.is_cancelled() {
        return attempt;
    }
    log("经系统代理连接失败，自动回退直连重试");
    // G-15：首个命令自然结束后，停止请求同样禁止启动代理回退。
    if control.is_cancelled() {
        return attempt;
    }
    run_git_with(
        git,
        cwd,
        args,
        &[],
        &crate::system_proxy::GIT_PROXY_ENV_KEYS,
    )
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

/// 有界截断（F14/G-14）：单条日志保留的最多字符数，防止异常巨大的 git 输出
/// 刷爆界面日志容量。
fn bounded_text(text: &str) -> String {
    const LIMIT: usize = 2000;
    if text.chars().count() > LIMIT {
        let mut cut: String = text.chars().take(LIMIT).collect();
        cut.push_str("\n…（输出过长，已截断）");
        cut
    } else {
        text.to_owned()
    }
}

/// 把一次 git 命令的真实输出整理成日志条目（F14/G-14：成功路径也必须采集真实
/// stdout/stderr，不能只记命令文本）。两流都为空时返回 None（git add 成功通常
/// 无输出，不产生空条目）。
fn output_log_entry(stdout: &str, stderr: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !stdout.trim().is_empty() {
        parts.push(format!("stdout：\n{}", bounded_text(stdout)));
    }
    if !stderr.trim().is_empty() {
        parts.push(format!("stderr：\n{}", bounded_text(stderr)));
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// F10/P-03/G-08：读取可选的合并配置项。未设置（或读取失败）返回 None——预检只
/// 拒绝明确配置出的危险形态，读取不到按未设置处理，不因此拒绝正常仓库。
fn config_get_opt(git: &Path, root: &Path, key: &str) -> Option<String> {
    let out = run_git(git, root, &["config", "--get", key]).ok()?;
    if !out.status.success() {
        return None;
    }
    let value = output_text(&out.stdout).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// 读取 Git 布尔配置，由 Git 规范化 yes/on/1 等真值同义词。
fn config_get_bool(git: &Path, root: &Path, key: &str) -> Option<bool> {
    let out = run_git(git, root, &["config", "--bool", "--get", key]).ok()?;
    if !out.status.success() {
        return None;
    }
    match output_text(&out.stdout).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// F10/P-03/G-08：任务启动前只读预检裸 `git push` 的目标。除 remote 及 refspec
/// 选择外，还核实最终 push URL 与 upstream fetch URL 一致：`remote.<r>.url` /
/// `pushurl` 可重复，`insteadOf` / `pushInsteadOf` 会改写实际地址，必须按 Git
/// 展开的全部 URL 判断，避免访问 upstream 之外的地址或系统代理按错目标判例外。
fn push_target_preflight(git: &Path, root: &Path, info: &RepoInfo) -> Result<()> {
    let branch = &info.branch;
    let upstream_remote = info.upstream_remote.trim();
    let push_remote = config_get_opt(git, root, &format!("branch.{branch}.pushRemote"));
    let push_default_remote = config_get_opt(git, root, "remote.pushDefault");
    let effective = push_remote
        .as_deref()
        .or(push_default_remote.as_deref())
        .unwrap_or(upstream_remote)
        .to_owned();
    if effective != upstream_remote {
        let (key, value) = match (&push_remote, &push_default_remote) {
            (Some(value), _) => (format!("branch.{branch}.pushRemote"), value.clone()),
            _ => (
                "remote.pushDefault".to_string(),
                push_default_remote.clone().unwrap_or_default(),
            ),
        };
        bail!(
            "拒绝启动：{key} 配置为「{value}」，与当前分支 upstream 远端「{upstream_remote}」不一致；\
             裸 git push 会把提交推到 {value} 而不是 upstream 远端，超出 P-03 允许的网络访问范围（G-08）。\
             请先在仓库中修正或清除该配置后重新开始任务"
        );
    }
    if let Some(refspec) = config_get_opt(git, root, &format!("remote.{effective}.push")) {
        bail!(
            "拒绝启动：remote.{effective}.push 配置了自定义推送 refspec（{refspec}）；\
             裸 git push 会按它推送，无法保证只推送当前分支到 upstream（P-03/G-08）。\
             请先移除该配置后重新开始任务"
        );
    }
    // mirror 是 Git 布尔配置，yes/on/1 与无值项同样表示 true；让 Git 负责规范化。
    if config_get_bool(git, root, &format!("remote.{effective}.mirror")) == Some(true) {
        bail!(
            "拒绝启动：remote.{effective}.mirror=true 会让裸 git push 镜像推送全部引用并删除远端多余分支（P-03/G-08）。\
             请先移除该配置后重新开始任务"
        );
    }
    // 比较 Git 展开后的实际 URL，覆盖多值 url/pushurl 以及 insteadOf/pushInsteadOf
    // 改写；`--all` 列出裸 push 将使用的全部地址，fetch 默认使用首个 URL。
    if effective != "." {
        let fetch_url = run_git_ok(git, root, &["remote", "get-url", &effective])
            .with_context(|| format!("读取 upstream 远端「{effective}」的 fetch URL 失败"))?;
        let fetch_url = fetch_url.trim();
        let push_urls = run_git_ok(
            git,
            root,
            &["remote", "get-url", "--push", "--all", &effective],
        )
        .with_context(|| format!("读取 upstream 远端「{effective}」的 push URL 失败"))?;
        let push_urls: Vec<String> = push_urls
            .lines()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_owned)
            .collect();
        if push_urls.is_empty() || push_urls.iter().any(|url| url != fetch_url) {
            bail!(
                "拒绝启动：remote.{effective}.url / remote.{effective}.pushurl 或 url.*.insteadOf/pushInsteadOf\
                 使裸 git push 的实际目标不同于 upstream fetch 地址，可能访问 upstream 之外的地址（P-03/G-08/P-09）。\
                 请修正导致地址分叉的配置后重新开始任务"
            );
        }
    }
    let push_default = config_get_opt(git, root, "push.default");
    match push_default.as_deref() {
        Some("matching") => {
            bail!(
                "拒绝启动：push.default=matching 会让裸 git push 一次推送所有同名分支（多分支推送，P-03/G-08）。\
                 请改为 simple / upstream 等单分支取值后重新开始任务"
            );
        }
        // nothing：裸 push 因「没有 refspec」以 fatal 失败，upstream 永不更新；
        // 该失败会被按可重试错误无限退避（G-09），任务永远无法推进。
        Some("nothing") => {
            bail!(
                "拒绝启动：push.default=nothing 会让裸 git push 因没有 refspec 而必然失败，\
                 upstream 永远不会更新且任务陷入无限重试（G-08/G-09）。\
                 请改为 simple / upstream 等取值后重新开始任务"
            );
        }
        // simple 是现代 Git 的默认值；upstream 名与本地分支不同时，裸 push 会
        // 因名称不匹配而失败并陷入 G-09 无限重试，必须在任何写操作前拒绝。
        Some("simple") | None if info.upstream_branch.as_str() != branch.as_str() => {
            bail!(
                "拒绝启动：push.default=simple（未设置时的默认行为）且本地分支「{branch}」\
                 与 upstream 分支「{}」不同名，裸 git push 会因分支名不匹配而必然失败并无限重试（G-08/G-09）。\
                 请改用 push.default=upstream 或配置同名 upstream 后重新开始任务",
                info.upstream_branch
            );
        }
        // current：推送目标是「远端上的同名分支」。本地分支名与 upstream 分支名
        // 不同时（如 topic 跟踪 origin/main），裸 push 会静默新建 origin/topic，
        // upstream 未更新却报告全部完成（实测复现）。
        Some("current") if info.upstream_branch.as_str() != branch.as_str() => {
            bail!(
                "拒绝启动：push.default=current 且本地分支「{branch}」与 upstream 分支\
                 「{}」不同名，裸 git push 会推送到 {}/{branch} 而不是 upstream（G-08），\
                 并在远端新建分支。请改用 push.default=simple/upstream 或改用同名 upstream 分支后重新开始任务",
                info.upstream_branch,
                effective
            );
        }
        Some("simple" | "current" | "upstream" | "tracking") | None => {}
        Some(value) => {
            bail!(
                "拒绝启动：push.default={value} 不是受支持的 Git 取值，裸 git push 将必然失败并陷入无限重试（G-08/G-09）。\
                 请改为 simple、current、upstream 或 tracking 后重新开始任务"
            );
        }
    }
    Ok(())
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
    /// 本变更的主路径：rename 用新路径（旧路径的展示与暂存语义见各调用点注释）。
    fn path(&self) -> &str {
        match self {
            FileChange::Renamed(_, new) => new,
            FileChange::Added(path) | FileChange::Modified(path) | FileChange::Deleted(path) => {
                path
            }
        }
    }
    /// add 阶段需要暂存的路径集合。rename（git mv 已把删除与新增写入 index）只暂存
    /// 新路径：旧路径在工作树与 index 中均已不存在，`git add -- 旧路径` 会因 pathspec
    /// 无匹配而失败；旧路径的删除由 commit --only 从 HEAD 与工作树状态带入提交。
    fn stage_paths(&self) -> Vec<String> {
        vec![self.path().to_string()]
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
        self.path().to_string()
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

/// 读取当前分支 upstream 的结构化远端与分支名（F09）：远端取
/// `branch.<b>.remote`（本地 upstream 时为 "."），分支取 `branch.<b>.merge`
/// 去掉 `refs/heads/` 前缀（可含斜杠）。不从 upstream 显示串拆分。
fn read_branch_upstream(git: &Path, root: &Path, branch: &str) -> Result<(String, String)> {
    let remote = run_git_ok(
        git,
        root,
        &["config", "--get", &format!("branch.{branch}.remote")],
    )
    .map_err(|error| anyhow::anyhow!("无法读取 branch.{branch}.remote：{error:#}"))?;
    let merge = run_git_ok(
        git,
        root,
        &["config", "--get", &format!("branch.{branch}.merge")],
    )
    .map_err(|error| anyhow::anyhow!("无法读取 branch.{branch}.merge：{error:#}"))?;
    let upstream_branch = merge
        .trim()
        .strip_prefix("refs/heads/")
        .unwrap_or(merge.trim())
        .to_owned();
    Ok((remote.trim().to_owned(), upstream_branch))
}

/// 验证仓库并读取基本信息（G-02）：目录存在、有效仓库、有检出分支、有 upstream。
pub fn inspect(git: &Path, repo: &Path) -> Result<RepoInfo> {
    let top = run_git_ok(git, repo, &["rev-parse", "--show-toplevel"])
        .map_err(|error| anyhow::anyhow!("不是有效的 Git repository：{error:#}"))?;
    let root = PathBuf::from(top.trim());
    let branch = run_git_ok(git, repo, &["symbolic-ref", "--short", "HEAD"]).map_err(|_| {
        anyhow::anyhow!("当前没有检出的 branch（处于 detached HEAD），无法使用本工具")
    })?;
    let branch = branch.trim().to_owned();
    let upstream = run_git_ok(
        git,
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .map_err(|_| anyhow::anyhow!("当前 branch「{branch}」未配置 upstream，无法推送"))?;
    let (upstream_remote, upstream_branch) = read_branch_upstream(git, &root, &branch)?;
    Ok(RepoInfo {
        root,
        branch,
        upstream: upstream.trim().to_owned(),
        upstream_remote,
        upstream_branch,
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
fn try_push(
    git: &Path,
    root: &Path,
    remote_url: &str,
    log: &dyn Fn(&str),
    control: &Control,
) -> PushOutcome {
    // P-03：子模块有各自的远端，不能借递归推送突破当前upstream边界。
    log("git push --porcelain --no-recurse-submodules");
    let out = match run_git_network(
        git,
        root,
        &["push", "--porcelain", "--no-recurse-submodules"],
        remote_url,
        log,
        control,
    ) {
        Ok(out) => out,
        Err(error) => return PushOutcome::Retryable(format!("{error:#}")),
    };
    let stdout = output_text(&out.stdout);
    let stderr = output_text(&out.stderr);
    if out.status.success() && !stdout.lines().any(|line| line.starts_with('!')) {
        // F14/G-14：成功也采集真实输出（push 返回内容），不能只在失败时可见。
        if let Some(entry) = output_log_entry(&stdout, &stderr) {
            log(&format!("git push 输出：\n{entry}"));
        }
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
    /// 用户已停止（F13/G-15）：当前命令自然结束，不再启动后续命令。
    Cancelled,
}

/// fetch + merge 当前 upstream（G-10）。远端与分支来自 `branch.<b>.remote` /
/// `branch.<b>.merge` 结构化配置（F09：不从 upstream 显示串拆分——含斜杠分支与
/// 本地 upstream（remote=="."）都会拆错）。fetch 与 merge 是两个独立阶段（G-13），
/// 每条命令启动前检查停止标志（F13/G-15），成功与失败都把真实输出写入日志
/// （F14/G-14）。
fn fetch_and_merge(
    git: &Path,
    root: &Path,
    remote: &str,
    upstream_branch: &str,
    ctx: &Ctx<'_>,
) -> MergeOutcome {
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    ctx.shared.set_stage("pull/fetch");
    (ctx.log)(&format!(
        "git fetch --no-recurse-submodules {remote} {upstream_branch}"
    ));
    let fetch = match run_git_network(
        git,
        root,
        &["fetch", "--no-recurse-submodules", remote, upstream_branch],
        &ctx.remote_url,
        ctx.log,
        ctx.control,
    ) {
        Ok(out) => out,
        Err(error) => return MergeOutcome::Retryable(format!("{error:#}")),
    };
    let fetch_stdout = output_text(&fetch.stdout);
    let fetch_stderr = output_text(&fetch.stderr);
    if !fetch.status.success() {
        let reason = summarize(&fetch_stderr, &fetch_stdout);
        (ctx.log)(&format!("git fetch 失败：{reason}"));
        return MergeOutcome::Retryable(reason);
    }
    // F14/G-14：成功也采集真实输出
    if let Some(entry) = output_log_entry(&fetch_stdout, &fetch_stderr) {
        (ctx.log)(&format!("git fetch 输出：\n{entry}"));
    }
    // F13/G-15：fetch 自然结束后检查停止标志，不再启动 merge
    if ctx.control.is_cancelled() {
        return MergeOutcome::Cancelled;
    }
    ctx.shared.set_stage("merge");
    // G-10/G-11：只限本次自动合并，清除分支默认策略，避免-Xours/-Xtheirs吞掉冲突。
    // 不修改用户配置，也不覆盖其他继承的Git配置。
    let merge_options = format!("branch.{}.mergeOptions=", ctx.branch);
    (ctx.log)(&format!("git -c {merge_options} merge FETCH_HEAD"));
    let out = match run_git(git, root, &["-c", &merge_options, "merge", "FETCH_HEAD"]) {
        Ok(out) => out,
        Err(error) => return MergeOutcome::Retryable(format!("{error:#}")),
    };
    let stdout = output_text(&out.stdout);
    let stderr = output_text(&out.stderr);
    if out.status.success() {
        // F14/G-14：merge 成功的真实输出（合并统计）也进日志，且作为独立阶段呈现
        if let Some(entry) = output_log_entry(&stdout, &stderr) {
            (ctx.log)(&format!("git merge 输出：\n{entry}"));
        }
        return MergeOutcome::Merged;
    }
    let text = format!("{stderr}{stdout}");
    if text.contains("CONFLICT") {
        // G-14：冲突时必须把 git merge 的输出写进日志（含 CONFLICT 与冲突文件名），
        // 用户要靠它判断卡在哪一步；不能只挑一边流——CONFLICT 行可能落在 stdout，
        // 也不能只给「失败」级别的信息。多行日志条目与 run() 的仓库信息同款。
        (ctx.log)(&format!("git merge 冲突输出：\n{}", bounded_text(&text)));
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

/// 逐文件处理的公共上下文：任务控制、界面共享状态、日志回调、退避基准与
/// upstream 远端的实际 URL（P-09 例外表判断用；解析失败/本地 upstream 为空串）。
struct Ctx<'a> {
    control: &'a Control,
    shared: &'a GitShared,
    log: &'a dyn Fn(&str),
    unit: Duration,
    remote_url: String,
    branch: &'a str,
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
/// `git add -- <paths>` 报 pathspec 不匹配时核实：这些路径相对 HEAD 的删除是否已
/// 完全暂存（porcelain 首列 `D`、次列空，即 `D ` 形态；工作树与索引一致）。
/// 是则无需也无法再 add，直接进入 commit 阶段。
fn deletion_fully_staged(git: &Path, root: &Path, paths: &[String]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let mut args: Vec<&str> = vec!["status", "--porcelain", "--"];
    for path in paths {
        args.push(path.as_str());
    }
    let Ok(out) = run_git(git, root, &args) else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let mut staged = 0usize;
    for line in output_text(&out.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() >= 2 && bytes[0] == b'D' && bytes[1] == b' ' {
            staged += 1;
        } else {
            return false;
        }
    }
    staged == paths.len()
}

/// F11/G-06：检测路径的 index/worktree 分叉（部分暂存）。porcelain 两列 X/Y 都非空
///（MM、AM、RM、MD 等）说明暂存区与工作区各有一份不同的改动：此时 `git add` 会用
/// 工作区内容重写用户已暂存的版本，`commit --only` 也会按工作区内容落提交——两者
/// 都保不住 staged 版本（G-06 不得删除用户 staged 内容）。未跟踪（??）没有暂存
/// 版本可保护，不算分叉。
fn partial_stage_divergence(git: &Path, root: &Path, paths: &[String]) -> Result<bool> {
    if paths.is_empty() {
        return Ok(false);
    }
    let mut args: Vec<&str> = vec!["status", "--porcelain", "--"];
    for path in paths {
        args.push(path.as_str());
    }
    let out = run_git(git, root, &args)
        .with_context(|| "执行 git status 失败（检查部分暂存状态）".to_string())?;
    if !out.status.success() {
        bail!(
            "检查部分暂存状态失败：{}",
            summarize(&output_text(&out.stderr), &output_text(&out.stdout))
        );
    }
    for line in output_text(&out.stdout).lines() {
        let bytes = line.as_bytes();
        if bytes.len() < 2 {
            continue;
        }
        let (x, y) = (bytes[0], bytes[1]);
        if x != b' ' && x != b'?' && y != b' ' && y != b'?' {
            return Ok(true);
        }
    }
    Ok(false)
}
fn process_change(
    git: &Path,
    root: &Path,
    change: &FileChange,
    remote: &str,
    upstream_branch: &str,
    ctx: &Ctx<'_>,
) -> StepOutcome {
    let stage_paths = change.stage_paths();
    let commit_paths = change.commit_paths();
    let display = change.display_path();
    let message = format!("update: {display}");
    // F11/G-06：处理该文件前检测部分暂存分叉——暂存区与工作区对该路径各有一份
    // 不同改动时，add 会用工作区内容覆盖用户已暂存的版本；该文件计为失败并停止
    // 自动处理（保护 staged 版本），不执行 add。
    match partial_stage_divergence(git, root, &stage_paths) {
        Ok(false) => {}
        Ok(true) => {
            let text = format!(
                "{display} 处于部分暂存状态受保护：暂存区与工作区对该文件各有一份不同的改动，\
                 继续处理会覆盖用户已暂存的版本（G-06）；该文件计为失败，未执行 add/commit，\
                 请先自行统一该文件的暂存区与工作区状态后重新开始任务"
            );
            (ctx.log)(&text);
            return StepOutcome::Fatal(text);
        }
        Err(error) => {
            let text = format!("{error:#}（当前文件：{display}，阶段：add 前预检）");
            (ctx.log)(&text);
            return StepOutcome::Fatal(text);
        }
    }
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
                Ok(out) if out.status.success() => {
                    // F14/G-14：成功也采集真实输出（add 通常无输出，空输出不记条目）
                    let stdout = output_text(&out.stdout);
                    let stderr = output_text(&out.stderr);
                    if let Some(entry) = output_log_entry(&stdout, &stderr) {
                        (ctx.log)(&format!("git add 输出：\n{entry}"));
                    }
                    added = true;
                }
                Ok(out) => {
                    let why = summarize(&output_text(&out.stderr), &output_text(&out.stdout));
                    (ctx.log)(&format!("git add 失败：{why}"));
                    // G-06：用户启动前已暂存的删除（git rm，或删除后 git add）在索引中
                    // 已无条目，`git add -- <path>` 必然报 pathspec 不匹配——这是确定性
                    // 状态而非可重试失败。核实这些路径的删除确已完全暂存（porcelain
                    // `D ` 形态）后跳过 add 直接提交（G-04：删除也是合法变更，
                    // `commit --only -- <path>` 对完全暂存删除可直接落提交）。
                    if why.contains("did not match any files")
                        && deletion_fully_staged(git, root, &commit_paths)
                    {
                        (ctx.log)("该删除已由用户预先暂存（索引已无条目），跳过 add 直接提交");
                        added = true;
                        continue;
                    }
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
            // F13/G-15：add 成功后进入 commit 前同样检查停止标志——停止检查不能只在
            // 循环顶：add 与 commit 同轮衔接时，add 执行期间发出的停止请求会漏过。
            if ctx.control.is_cancelled() {
                return StepOutcome::Cancelled;
            }
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
                Ok(out) if out.status.success() => {
                    // F14/G-14：成功也采集真实输出（分支、提交号与文件统计）
                    let stdout = output_text(&out.stdout);
                    let stderr = output_text(&out.stderr);
                    if let Some(entry) = output_log_entry(&stdout, &stderr) {
                        (ctx.log)(&format!("git commit 输出：\n{entry}"));
                    }
                }
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
            // F13/G-15：commit 自然结束后、验证命令启动前同样检查停止标志
            if ctx.control.is_cancelled() {
                return StepOutcome::Cancelled;
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
        let outcome = push_with_retry(git, root, remote, upstream_branch, ctx, &mut attempt);
        if matches!(outcome, StepOutcome::Done) {
            (ctx.log)(&format!("push 成功：{display}"));
        }
        return outcome;
    }
}

/// push 当前分支（G-08~G-11）：NeedMerge 时自动 fetch+merge 后重试 push（不 rebase），
/// 可重试失败按退避无限重试；供 process_change 的单文件流程、冲突续接的合并提交与
/// 只推送补推流程（F12）共用。
fn push_with_retry(
    git: &Path,
    root: &Path,
    remote: &str,
    upstream_branch: &str,
    ctx: &Ctx<'_>,
    attempt: &mut u64,
) -> StepOutcome {
    loop {
        if ctx.control.is_cancelled() {
            return StepOutcome::Cancelled;
        }
        ctx.shared.set_stage("push");
        match try_push(git, root, &ctx.remote_url, ctx.log, ctx.control) {
            PushOutcome::Ok => return StepOutcome::Done,
            PushOutcome::NeedMerge => {
                match fetch_and_merge(git, root, remote, upstream_branch, ctx) {
                    // 合并成功：回到循环重试 push（不消耗重试计数，G-10）
                    MergeOutcome::Merged => {}
                    MergeOutcome::Cancelled => return StepOutcome::Cancelled,
                    MergeOutcome::Conflict(files) => return StepOutcome::Conflict(files),
                    MergeOutcome::BlockedByDirty(reason) => {
                        return StepOutcome::Fatal(format!(
                            "合并被本地未提交变更阻挡：{reason}（阶段：merge）"
                        ));
                    }
                    MergeOutcome::Retryable(reason) => {
                        tracing::warn!(
                            stage = "fetch_merge",
                            reason = %reason,
                            "Git fetch/merge 失败，进入退避重试"
                        );
                        (ctx.log)(&format!("fetch/merge 失败：{reason}"));
                        if !ctx.wait_retry(attempt) {
                            return StepOutcome::Cancelled;
                        }
                    }
                }
            }
            PushOutcome::Retryable(reason) => {
                tracing::warn!(
                    stage = "push",
                    reason = %reason,
                    "Git push 失败，进入退避重试"
                );
                (ctx.log)(&format!("push 失败：{reason}"));
                if !ctx.wait_retry(attempt) {
                    return StepOutcome::Cancelled;
                }
            }
        }
    }
}

/// F12/G-12/G-16：统计本地与 upstream 的提交差集，返回 (远端领先数, 本地领先数)。
/// porcelain 干净只说明工作区没有未提交变更，不代表本地与远端一致——上次任务可能
/// 在 commit 之后、push 之前被停止，本地会领先若干提交。
fn ahead_behind(git: &Path, root: &Path) -> Result<(u64, u64)> {
    let out = run_git(
        git,
        root,
        &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
    )
    .with_context(|| "执行 git rev-list 失败".to_string())?;
    if !out.status.success() {
        bail!(
            "git rev-list 失败：{}",
            summarize(&output_text(&out.stderr), &output_text(&out.stdout))
        );
    }
    let text = output_text(&out.stdout);
    let mut parts = text.split_whitespace();
    let (Some(behind), Some(ahead)) = (parts.next(), parts.next()) else {
        bail!("无法解析 git rev-list --count 输出：{text:?}");
    };
    let behind: u64 = behind
        .parse()
        .with_context(|| format!("无法解析远端领先数 {behind:?}"))?;
    let ahead: u64 = ahead
        .parse()
        .with_context(|| format!("无法解析本地领先数 {ahead:?}"))?;
    Ok((behind, ahead))
}

/// F12/G-12/G-16：工作区没有未提交变更时的收尾。porcelain 为空不代表本地与远端
/// 一致——上次任务可能在 commit 后、push 前被停止。以 upstream 与 HEAD 的差集计数
/// 核实（upstream 解析失败时如实说明无法核实，不得宣称一致）：本地领先则进入
/// 只推送流程（G-09 阶段记忆——只重试 push、不重复 commit）；推送失败或分叉冲突
/// 时如实报告「本地有 N 个提交未推送」，绝不报「工作区与远端一致」。
fn finish_without_changes(git: &Path, info: &RepoInfo, ctx: &Ctx<'_>) -> String {
    let shared = ctx.shared;
    let (behind, ahead) = match ahead_behind(git, &info.root) {
        Ok(pair) => pair,
        Err(error) => {
            let text = format!("没有需要提交的变更，但无法核实与远端的同步状态：{error:#}");
            (ctx.log)(&text);
            shared.set_state("失败");
            return text;
        }
    };
    if ahead == 0 {
        shared.set_state("完成");
        shared.set_stage("完成");
        if behind == 0 {
            return "没有需要提交的变更：工作区与远端一致".into();
        }
        return format!(
            "没有需要提交的变更：本地没有领先远端的提交（远端领先 {behind} 个提交，本工具不自动拉取）"
        );
    }
    (ctx.log)(&format!(
        "工作区没有未提交变更，但本地领先 upstream {ahead} 个提交（可能在 push 前停止）；按 G-09 阶段记忆只重试 push，不重复 commit"
    ));
    let mut attempt = 0u64;
    match push_with_retry(
        git,
        &info.root,
        &info.upstream_remote,
        &info.upstream_branch,
        ctx,
        &mut attempt,
    ) {
        StepOutcome::Done => {
            shared.set_state("完成");
            shared.set_stage("完成");
            let text = format!("没有需要提交的变更：已补推本地领先的 {ahead} 个提交");
            (ctx.log)(&text);
            text
        }
        StepOutcome::Cancelled => {
            shared.set_state("已停止");
            format!("任务已停止：本地有 {ahead} 个提交未推送（可再次启动任务补推）")
        }
        StepOutcome::Conflict(files) => {
            shared.set_state("冲突");
            shared.set_stage("冲突");
            let text = format!(
                "补推时自动合并出现冲突（{} 个文件：{}）；本地有 {ahead} 个提交未推送。已停止并保留冲突现场，请在仓库中解决后重新开始任务（G-11）",
                files.len(),
                files.join("、")
            );
            (ctx.log)(&text);
            text
        }
        StepOutcome::Fatal(reason) => {
            shared.set_state("失败");
            let text = format!("本地有 {ahead} 个提交未推送：{reason}");
            (ctx.log)(&text);
            text
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
    // P-10：Git 工具是关键功能任务，开始/结束统计（含终态与耗时）必须落盘；
    // 逐次 git 子进程的失败语义由下方重试日志与返回文本承载。
    tracing::info!(repo = %repo.display(), "Git 任务开始");
    let started = std::time::Instant::now();
    let result = run_task(git, repo, control, shared, log, status, unit);
    tracing::info!(
        repo = %repo.display(),
        state = %shared.state(),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "Git 任务结束"
    );
    result
}

fn run_task(
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
    // F10/P-03/G-08：任务启动预检——在任何写命令之前核实裸 git push 的实际推送
    // 目标确实是 upstream 远端，且没有会改变推送对象/范围的用户配置（只读检查）。
    if let Err(error) = push_target_preflight(git, repo, &info) {
        let text = format!("{error:#}");
        log(&text);
        shared.set_state("失败");
        return format!("无法开始：{text}");
    }
    // P-09：upstream 远端的实际 URL（fetch/push 的代理例外判断）。读取失败或
    // 本地 upstream（remote 为 "."）时留空——此时只走注入/回退路径，不做例外
    // 前置判断（scp 风格等无 scheme 的 URL 解析不出 host，同样保守走代理）。
    // get-url 取 fetch URL：pushurl 与 fetch URL 不同已在预检拒绝，二者一致时
    // 单一 URL 即同时代表 fetch 与 push 的目标。
    let remote_url = run_git_ok(
        git,
        &info.root,
        &["remote", "get-url", &info.upstream_remote],
    )
    .map(|text| text.trim().to_owned())
    .unwrap_or_default();
    // 中间态处理（G-06/G-11）
    match middle_state(git, repo) {
        Ok(MiddleState::Clean) => {}
        Ok(MiddleState::Merge { unresolved }) => {
            if unresolved.is_empty() {
                // F13/G-15：完成合并提交也是一条会改仓库状态的 git 命令，启动前检查
                // 停止标志——停止后不得再启动任何 git 命令。
                if control.is_cancelled() {
                    shared.set_state("已停止");
                    return "任务已停止：合并已解决但尚未完成合并提交（可重新开始任务继续）".into();
                }
                // 用户已在冲突后解决并暂存：完成合并提交后继续（G-11 续段）。
                // 合并提交必须推送成功才算完成当前任务，然后继续扫描剩余变更。
                // G-06 夹带检测：`git commit --no-edit` 不带 pathspec，会把整个暂存区
                // 一起提交——用户在解决冲突期间另行 stage 的无关文件会被夹带进合并
                // 提交。合并涉及的文件必然在两个 merge 父（HEAD 与 MERGE_HEAD）的
                // 差异集合内；该集合之外的 staged 条目即夹带项，发现即停止，不得
                // 冒险继续（G-06：无法安全保证时停止并显示具体原因）。
                // 两条 diff 统一 --no-renames：rename 检测基于各自内容对的相似度
                // （index↔HEAD 与 HEAD↔MERGE_HEAD 不同），同一 rename 可能一条检出、
                // 另一条不检出，路径集合不对称会把合并内的旧路径误报成范围外；
                // 禁用 rename 折叠后两侧都输出旧+新两个路径，子集检查恢复对称。
                let staged = match run_git_ok(
                    git,
                    repo,
                    &["diff", "--cached", "--name-only", "--no-renames"],
                ) {
                    Ok(text) => text,
                    Err(error) => {
                        let text = format!("检查合并暂存区失败：{error:#}（G-06）");
                        log(&text);
                        shared.set_state("失败");
                        return text;
                    }
                };
                let merge_scope = match run_git_ok(
                    git,
                    repo,
                    &["diff", "--name-only", "--no-renames", "HEAD", "MERGE_HEAD"],
                ) {
                    Ok(text) => text,
                    Err(error) => {
                        let text = format!("无法确定合并涉及的文件范围：{error:#}（G-06）");
                        log(&text);
                        shared.set_state("失败");
                        return text;
                    }
                };
                let in_scope: std::collections::HashSet<&str> = merge_scope
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect();
                let extras: Vec<&str> = staged
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !in_scope.contains(line))
                    .collect();
                if !extras.is_empty() {
                    let text = format!(
                        "仓库存在合并范围之外的已暂存文件（{}）：完成合并提交会把它们一并带入（G-06）。\
                         请先在仓库中取消暂存或另行处理这些文件后重新开始任务",
                        extras.join("、")
                    );
                    log(&text);
                    shared.set_state("失败");
                    return text;
                }
                shared.set_stage("merge");
                log("检测到已解决的合并：完成合并提交（git commit --no-edit）");
                match run_git(git, repo, &["commit", "--no-edit"]) {
                    Ok(out) if out.status.success() => {
                        // F14/G-14：合并提交的真实输出也进日志
                        let stdout = output_text(&out.stdout);
                        let stderr = output_text(&out.stderr);
                        if let Some(entry) = output_log_entry(&stdout, &stderr) {
                            log(&format!("git commit --no-edit 输出：\n{entry}"));
                        }
                    }
                    Ok(out) => {
                        let text = format!(
                            "完成合并提交失败：{}",
                            summarize(&output_text(&out.stderr), &output_text(&out.stdout))
                        );
                        log(&text);
                        shared.set_state("失败");
                        return text;
                    }
                    Err(error) => {
                        let text = format!("完成合并提交失败：{error:#}");
                        log(&text);
                        shared.set_state("失败");
                        return text;
                    }
                }
                let ctx = Ctx {
                    control,
                    shared,
                    log,
                    unit,
                    remote_url: remote_url.clone(),
                    branch: &info.branch,
                };
                let mut attempt = 0u64;
                match push_with_retry(
                    git,
                    repo,
                    &info.upstream_remote,
                    &info.upstream_branch,
                    &ctx,
                    &mut attempt,
                ) {
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
    let ctx = Ctx {
        control,
        shared,
        log,
        unit,
        remote_url,
        branch: &info.branch,
    };
    if changes.is_empty() {
        return finish_without_changes(git, &info, &ctx);
    }
    let mut done = 0u64;
    for change in &changes {
        if control.is_cancelled() {
            break;
        }
        shared.set_current(&change.display_path());
        log(&format!("开始处理：{}", change.display_path()));
        match process_change(
            git,
            &info.root,
            change,
            &info.upstream_remote,
            &info.upstream_branch,
            &ctx,
        ) {
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

#[cfg(test)]
mod tests {
    use super::*;

    // 覆盖 G-15/P-03：代理命令结束后收到停止，不得启动直连回退命令。
    #[test]
    fn network_fallback_does_not_restart_after_stop() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let wrapper = dir.path().join("git-network.cmd");
        let trace = dir.path().join("trace.txt");
        let real_git = find_git()?;
        std::fs::write(
            &wrapper,
            format!(
                "@echo off\r\n>>\"{}\" echo invoked\r\n\"{}\" %*\r\nexit /b %ERRORLEVEL%\r\n",
                trace.display(),
                real_git.display()
            ),
        )?;
        let proxy =
            crate::system_proxy::SystemProxy::from_registry_values(1, Some("127.0.0.1:1"), None);
        let control = Control::default();
        let result = run_git_network_with_proxy(
            &wrapper,
            dir.path(),
            &["ls-remote", "http://127.0.0.1:1/repo.git"],
            "http://127.0.0.1:1/repo.git",
            &|line| {
                if line.contains("回退直连") {
                    control.cancel();
                }
            },
            &control,
            &proxy,
        );
        assert!(control.is_cancelled(), "前置：必须走到代理失败的回退边界");
        assert!(
            result.is_err() || result.as_ref().is_ok_and(|out| !out.status.success()),
            "关闭的本地端口不能被当成成功"
        );
        assert_eq!(
            std::fs::read_to_string(trace)?.lines().count(),
            1,
            "停止后不得启动第二条Git联网命令"
        );
        Ok(())
    }
}
