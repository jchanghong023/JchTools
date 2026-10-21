mod alloc;
mod fold;
mod git;
mod moveops;
mod rootops;
mod snapshot;
mod task_lock;
mod validate;
mod workspace;

pub use alloc::{
    ensure_dir, ensure_parent, is_numbered_volume_digits, split_compound_name, suffixed_candidate,
    unique_target,
};
pub use fold::{fold_rel, is_link};
pub use git::{is_git_root, root_inside_git_project};
pub use moveops::{move_file_preserving_times, rename_noreplace, set_created_time};
pub use rootops::{normalize_root, normalize_root_with};
pub use snapshot::{snapshot, snapshot_with};
pub use task_lock::RootGuard;
pub use validate::{
    ensure_plain_entry, path_string, relative_string, safe_join, safe_relative, validate_component,
};
pub use workspace::is_extraction_workspace;

#[cfg(windows)]
pub use rootops::protected_root;

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
