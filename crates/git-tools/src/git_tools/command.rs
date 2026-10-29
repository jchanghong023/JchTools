//! git 子进程执行、输出整理与配置读取（G-04/G-05/G-14）。
use anyhow::{bail, Context as _, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

use jchtools_file_core::{
    control::Control,
    process::{self, CapturedOutput},
};

#[cfg(test)]
mod tests;

/// 按系统 PATH 顺序解析 git.exe 的绝对路径；
/// 跳过空目录和相对目录，避免当前仓库内同名程序劫持。
pub fn find_git() -> Result<PathBuf> {
    let span = jchtools_foundation::logging::operation_span("git", "resolve_executable");
    let _entered = span.enter();
    let started = std::time::Instant::now();
    tracing::debug!(event = "git_resolve_started", "开始查找 Git 可执行程序");
    let result = (|| {
        if let Some(paths) = std::env::var_os("PATH") {
            for directory in std::env::split_paths(&paths) {
                if !directory.is_absolute() {
                    continue;
                }
                let candidate = directory.join("git.exe");
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        bail!("找不到 git.exe：请安装 Git for Windows 并将其加入系统 PATH")
    })();
    if result.is_ok() {
        tracing::debug!(
            event = "git_resolve_completed",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "已找到 Git 可执行程序"
        );
    } else {
        tracing::error!(
            event = "git_resolve_failed",
            error_type = "executable_missing",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "未找到 Git 可执行程序"
        );
    }
    result
}

fn command_stage(args: &[&str]) -> &'static str {
    match args.first().copied() {
        Some("rev-parse") => "rev_parse",
        Some("symbolic-ref") => "symbolic_ref",
        Some("config") => "config",
        Some("status") => "status",
        Some("diff") => "diff",
        Some("diff-tree") => "diff_tree",
        Some("rev-list") => "rev_list",
        Some("remote") => "remote",
        Some("add") => "add",
        Some("commit") => "commit",
        Some("push") => "push",
        Some("fetch") => "fetch",
        Some("merge") => "merge",
        _ => "other",
    }
}

/// 运行一条 git 命令并捕获输出（无 shell，继承用户配置与环境）。
pub(super) fn run_git(git: &Path, cwd: &Path, args: &[&str]) -> Result<CapturedOutput> {
    run_git_with_proxy(git, cwd, args, None)
}

fn run_git_with_proxy(
    git: &Path,
    cwd: &Path,
    args: &[&str],
    proxy: Option<(&str, &str)>,
) -> Result<CapturedOutput> {
    let operation = jchtools_foundation::logging::operation_span("git", "command");
    let _entered = operation.enter();
    let stage = command_stage(args);
    let started = std::time::Instant::now();
    tracing::debug!(event = "git_command_started", stage, "开始执行 Git 子命令");
    let mut command = Command::new(git);
    // G-04/G-05：porcelain 输出的文件名按字面匹配，避免 pathspec 通配语义
    // 导致错误暂存兄弟文件；使用 Git 参数，不改用户环境或配置。
    command
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(cwd);
    if let Some((variable, endpoint)) = proxy {
        // G-17：仅设置本条 HTTP(S) 命令的缺省代理，不改宿主环境或 Git 配置。
        command.env(variable, endpoint);
    }
    // G-15：停止只在命令边界生效，不能因宿主 deadline 强杀 Git/hook/filter。
    let result = process::run_to_completion(&mut command);
    match &result {
        Ok(out) if out.status.success() => tracing::debug!(
            event = "git_command_completed",
            stage,
            exit_code = out.status.code(),
            stdout_bytes = out.stdout.len(),
            stderr_bytes = out.stderr.len(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 子命令成功"
        ),
        Ok(out) => tracing::warn!(
            event = "git_command_failed",
            stage,
            error_type = "nonzero_exit",
            exit_code = out.status.code(),
            stdout_bytes = out.stdout.len(),
            stderr_bytes = out.stderr.len(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 子命令返回失败状态"
        ),
        Err(_) => tracing::warn!(
            event = "git_command_failed",
            stage,
            error_type = "subprocess_error",
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 子命令执行失败，详见子进程阶段"
        ),
    }
    result
}

/// 网络类 git 命令（fetch / push）：检查取消，按 G-17 补充系统代理缺省值。
pub(super) fn run_git_network(
    git: &Path,
    cwd: &Path,
    remote: &str,
    args: &[&str],
    control: &Control,
) -> Result<CapturedOutput> {
    let operation = jchtools_foundation::logging::operation_span("git", "network");
    let _entered = operation.enter();
    let stage = command_stage(args);
    let started = std::time::Instant::now();
    control.check_cancelled().inspect_err(|_| {
        tracing::warn!(
            event = "git_network_cancelled",
            stage = "before_command",
            "Git 网络操作启动前已停止"
        );
    })?;
    tracing::debug!(event = "git_network_started", stage, "开始 Git 网络操作");
    let result = (|| {
        let proxy = super::proxy::environment(git, cwd, remote, args.first() == Some(&"push"))?;
        control.check_cancelled()?;
        run_git_with_proxy(
            git,
            cwd,
            args,
            proxy
                .as_ref()
                .map(|(name, endpoint)| (*name, endpoint.as_str())),
        )
    })();
    match &result {
        Ok(out) if out.status.success() => tracing::debug!(
            event = "git_network_completed",
            stage,
            exit_code = out.status.code(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 网络操作成功"
        ),
        Ok(out) => tracing::warn!(
            event = "git_network_failed",
            stage,
            error_type = "nonzero_exit",
            exit_code = out.status.code(),
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 网络操作失败"
        ),
        Err(_) => tracing::warn!(
            event = "git_network_failed",
            stage,
            error_type = if control.is_cancelled() {
                "cancelled"
            } else {
                "subprocess_error"
            },
            elapsed_ms = jchtools_foundation::logging::elapsed_ms(started),
            "Git 网络操作未完成"
        ),
    }
    result
}

pub(super) fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// 运行并要求退出码为 0，否则把 stderr 摘要并入错误（U-13 可定位）。
pub(super) fn run_git_ok(git: &Path, cwd: &Path, args: &[&str]) -> Result<String> {
    let subcommand = args.first().copied().unwrap_or("");
    let out = run_git(git, cwd, args).with_context(|| format!("执行 git {subcommand} 失败"))?;
    if !out.status.success() {
        bail!(
            "git {subcommand} 失败（退出码 {}）：{}",
            out.status.code().unwrap_or(-1),
            summarize(&output_text(&out.stderr), &output_text(&out.stdout))
        );
    }
    if let Some(note) = out.truncation_note() {
        bail!("git {subcommand} 输出不完整：{note}；拒绝解析");
    }
    Ok(output_text(&out.stdout))
}

/// 失败摘要：优先 stderr，保留完整阶段的可读上下文（G-14）。
pub(super) fn summarize(stderr: &str, stdout: &str) -> String {
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
pub(super) fn bounded_text(text: &str) -> String {
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
pub(super) fn output_log_entry(stdout: &str, stderr: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !stdout.trim().is_empty() {
        parts.push(format!("stdout：\n{}", bounded_text(stdout)));
    }
    if !stderr.trim().is_empty() {
        parts.push(format!("stderr：\n{}", bounded_text(stderr)));
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// 把一次 git 命令的真实输出按「{label} 输出：」条目写入界面日志（F14/G-14）。
/// 内部复用 [`output_log_entry`]：两流都为空时不记条目（如 add 成功通常无输出）。
pub(super) fn log_command_output(log: &dyn Fn(&str), label: &str, out: &CapturedOutput) {
    let stdout = output_text(&out.stdout);
    let stderr = output_text(&out.stderr);
    if let Some(entry) = output_log_entry(&stdout, &stderr) {
        (log)(&format!("{label} 输出：\n{entry}"));
    }
}

/// F10/P-03/G-08：读取可选的合并配置项。未设置（或读取失败）返回 None——预检只
/// 拒绝明确配置出的危险形态，读取不到按未设置处理，不因此拒绝正常仓库。
pub(super) fn config_get_opt(git: &Path, root: &Path, key: &str) -> Option<String> {
    let out = run_git(git, root, &["config", "--get", key]).ok()?;
    if !out.status.success() {
        return None;
    }
    let value = output_text(&out.stdout).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// 读取 Git 布尔配置，由 Git 规范化 yes/on/1 等真值同义词。
pub(super) fn config_get_bool(git: &Path, root: &Path, key: &str) -> Option<bool> {
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
