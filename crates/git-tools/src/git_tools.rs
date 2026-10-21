//! Git 工具核心逻辑（合同 G 分区）：逐文件提交并推送。
//! 严格串行：一个文件 → 单独 git add → 单独 commit（按路径限定提交范围，保护既有
//! staged 内容，G-06）→ 验证单文件（G-05）→ 单独 push（G-08）→ 成功后才处理下一个
//! 文件（G-03）。push 被远端新提交拒绝时自动 fetch + merge（G-10，不 rebase）；
//! 冲突停止并保留现场（G-11）；可重试失败按 5→10→20→40→80→80… 秒无限退避（G-09），
//! 用户可随时停止（G-15）；以 git 状态为唯一进度来源（G-12）。
//! 网络访问仅限当前分支 upstream（P-03 的 Git 工具例外）。

mod command;
mod merge;
mod preflight;
mod push;
mod shared;
mod status;
mod task;

pub use command::find_git;
pub use shared::{backoff_duration, GitShared, BACKOFF_UNIT};
pub use status::{
    inspect, middle_state, parse_status, status_changes, FileChange, MiddleState, RepoInfo,
};
pub use task::run;
