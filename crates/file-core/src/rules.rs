mod archive;
mod category;
mod cleanup;
mod dedup;
mod digest;
mod exclusions;
mod extension_fix;
mod naming;
mod signature;

pub use archive::{
    archive_entry_key, archive_name, count_tail_only_old_style_groups, multipart_name,
    numbered_volume_missing_entry_groups, old_style_tail, part_rar_missing_first_groups,
    OldStyleTail,
};
pub use category::category_for;
pub use cleanup::{cleanup_delete, cleanup_hits, cleanup_reason, CleanupKind};
pub use dedup::{compare, identity_proves_same_file};
pub use digest::{
    digest_candidate, path_digest, suffixed_candidate, truncate_utf16_units, CONFLICT_YIELD_UNITS,
    SEGMENT_LIMIT_UNITS,
};
pub use exclusions::{build_exclusions, Exclusions};
pub use extension_fix::extension_needs_fix;
pub use naming::{
    clean_copy_output, copy_key, derive_stem_ext, legalize_derived, normal_key, normalize_stem,
    stabilize_derived_stem,
};
pub use signature::detect_extension;

#[cfg(test)]
pub use dedup::duplicate_allowed;

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
mod tests_archive;

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
mod tests_naming;
