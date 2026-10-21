//! 转 Markdown 的只读扫描、任务编排与结果落盘（T-07～T-12、T-22～T-25）。

mod formats;
mod media;
mod naming;
mod output;
mod run;
mod scan;
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

pub use formats::FormatGroup;
pub use media::{e2e_convert_media, test_media_dir_name};
pub use output::test_write_new_markdown;
pub use run::{page_readiness, readiness, readiness_for_groups, run, Event, Options, Summary};
pub use scan::{test_scan_count_with_cancel, test_scan_media_dirs};

#[cfg(test)]
pub(crate) use run::platform_preflight;

#[cfg(test)]
use {
    formats::{parse_formats, run_formats_probe, selected_xberg_extension},
    media::media_dir_name,
    naming::{
        case_fold_key, compare_paths, compare_paths_insensitive, occupancy_key, OccupiedIndex,
    },
    output::write_new_markdown,
    run::confirmed_cancellation,
    scan::scan,
};
