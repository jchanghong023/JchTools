//! 仓库检查、upstream 解析与未提交变更扫描（G-02/G-04/G-06/G-12）。
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

use super::command::{output_text, run_git, run_git_ok, summarize};

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
    pub(super) fn path(&self) -> &str {
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
    pub(super) fn stage_paths(&self) -> Vec<String> {
        vec![self.path().to_string()]
    }
    /// commit 阶段（--only）与单文件验证（G-05）使用的路径集合：rename 含新旧两路径。
    pub(super) fn commit_paths(&self) -> Vec<String> {
        match self {
            FileChange::Renamed(old, new) => vec![old.clone(), new.clone()],
            FileChange::Added(path) | FileChange::Modified(path) | FileChange::Deleted(path) => {
                vec![path.clone()]
            }
        }
    }
    /// commit message 使用的路径（G-07：相对仓库根目录；rename 用新路径）。
    pub(super) fn display_path(&self) -> String {
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

pub(super) fn git_dir(git: &Path, repo: &Path) -> Result<PathBuf> {
    let text = run_git_ok(git, repo, &["rev-parse", "--git-path", "."])?;
    let dir = PathBuf::from(text.trim());
    Ok(if dir.is_absolute() {
        dir
    } else {
        repo.join(dir)
    })
}

pub(super) fn unresolved_paths(git: &Path, repo: &Path) -> Result<Vec<String>> {
    let text = run_git_ok(git, repo, &["diff", "--name-only", "--diff-filter=U", "-z"])?;
    Ok(text
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
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
    let unresolved = unresolved_paths(git, repo)?;
    if !unresolved.is_empty() || dir.join("MERGE_HEAD").exists() {
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
    if let Some(note) = out.truncation_note() {
        bail!("git status 扫描失败：{note}；拒绝处理不完整的文件列表");
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
