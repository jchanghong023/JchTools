//! MD 整理核心逻辑（合同 M 分区）：「合并 MD」与「拆分 MD」。
//! 独立于 GUI：合并按 M-03 排序、M-04/M-05 标题下移、M-06 代码块保护，逐文件逐行流式处理；
//! 拆分按 M-09/M-10 在 UTF-8 安全边界分片，分片按编号顺序二进制拼接可无损还原原文件。
//! 所有原始文件只读；输出一律写入用户指定的新文件（M-02/M-08/M-11）。
//! 扫描不越过链接边界（M-02，与 S-04 同口径）；合并输出先写任务独有的临时文件、
//! 全部成功后改名落盘（M-07：失败或取消不留半成品输出）。

mod heading;
mod merge;
mod output;
mod progress;
mod scan;
mod split;

#[cfg(test)]
use jchtools_file_core::control::Control;
pub use merge::{merge_markdown_with_events, MergeStats};
pub use output::{conflicting_outputs, output_entry_exists, split_names};
pub use progress::MdProgress;
pub use scan::{natural_cmp, scan_markdown, MergeEntry};
#[cfg(test)]
use scan::{same_file_path, sort_rows};
pub use split::{plan_splits, run_split, run_split_with_events, SplitPlan};
#[cfg(test)]
use std::time::SystemTime;

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
mod tests;
