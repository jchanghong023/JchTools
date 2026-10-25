mod classify;
mod dedup;
mod resolve;

pub(crate) use dedup::{cleanup_candidates, count_logical_bytes};
// 折叠排序表的前缀区间二分命中谓词与 engine::planning 共用（原两处逐行同构的
// 复制实现，靠人工保持同步；统一后见 resolve::sorted_prefix_hit 的口径注释）。
pub(crate) use resolve::sorted_prefix_hit;

use anyhow::Result;
use classify::{classify_files, empty_directories, git_collection};
use dedup::deduplicate;
use jchtools_file_core::task::Job;
use std::collections::HashSet;

#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan", skip_all)
)]
pub fn build(job: &mut Job) -> Result<()> {
    job.db.conn.execute_batch("DELETE FROM actions;")?;
    let mut identities = HashSet::new();
    job.summary.candidate_bytes = 0;
    cleanup_candidates(job, &mut identities)?; // 永久清理项不能成为唯一去重保留者。
    build_after_cleanup(job, &mut identities)
}

/// C-01：分析入口已在哈希前确定清理项；这里只规划存活文件，不重置或重复清理。
#[cfg_attr(
    feature = "perf-tracing",
    tracing::instrument(target = "perf", name = "plan", skip_all)
)]
pub(crate) fn build_after_cleanup(job: &mut Job, identities: &mut HashSet<String>) -> Result<()> {
    deduplicate(job, identities)?;
    // C-14 先于文件归类：项目移动先行执行，腾出的目录名可被分类层级复用；
    // 空目录规划依赖 git_roots.staying 标记，必须在集合规划之后。
    let moved_roots = git_collection(job)?;
    classify_files(job, &moved_roots)?;
    empty_directories(job, &moved_roots)?;
    Ok(())
}

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod signature_control_tests;

#[rustfmt::skip]
#[warn(
    warnings,
    unused_must_use,
    unreachable_patterns,
    unreachable_pub,
    unused_variables,
    rust_2018_idioms,
    explicit_outlives_requirements,
    elided_lifetimes_in_paths,
    single_use_lifetimes
)]
#[cfg(test)]
mod resolve_tests;
