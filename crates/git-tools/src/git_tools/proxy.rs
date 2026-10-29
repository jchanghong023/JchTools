//! G-17：只为 Git HTTP(S) 子进程提供系统代理缺省值，不写持久配置。
use anyhow::Result;
use std::path::Path;

use super::command::run_git_ok;

/// 交给 Git/libcurl 自身处理 http.proxy、URL/remote 配置、空值及 NO_PROXY 的优先级；
/// 这里只补充缺失的协议代理环境变量，避免用 -c 覆盖用户已配置的策略。
pub(super) fn environment(
    git: &Path,
    root: &Path,
    remote: &str,
    push: bool,
) -> Result<Option<(&'static str, String)>> {
    if remote == "." {
        return Ok(None);
    }
    let proxy = jchtools_foundation::system_proxy::read();
    if !proxy.is_enabled() {
        return Ok(None);
    }
    // remote get-url 由 Git 展开 insteadOf / pushInsteadOf，不能按配置原文选择例外表。
    // P-03 预检已保证全部 push URL 与 upstream fetch URL 相同。
    let args: &[&str] = if push {
        &["remote", "get-url", "--push", remote]
    } else {
        &["remote", "get-url", remote]
    };
    let url = run_git_ok(git, root, args)?;
    let url = url.trim();
    let Some((scheme, _)) = url.split_once("://") else {
        return Ok(None);
    };
    let (variable, uppercase) = if scheme.eq_ignore_ascii_case("https") {
        ("https_proxy", "HTTPS_PROXY")
    } else if scheme.eq_ignore_ascii_case("http") {
        ("http_proxy", "HTTP_PROXY")
    } else {
        return Ok(None);
    };
    if [variable, uppercase, "all_proxy", "ALL_PROXY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        tracing::debug!(
            event = "git_proxy_inherited",
            "Git 保留继承环境中的代理策略"
        );
        return Ok(None);
    }
    let endpoint = proxy.endpoint_for_url(url);
    if endpoint.is_some() {
        tracing::info!(
            event = "git_system_proxy_default",
            "已为 Git 提供 Windows 系统代理缺省值，Git 显式配置和环境绕过规则优先"
        );
    } else {
        tracing::debug!(
            event = "git_system_proxy_bypassed",
            "Git 目标命中系统代理例外表或没有可用端点，不补充代理"
        );
    }
    Ok(endpoint.map(|endpoint| (variable, endpoint)))
}
