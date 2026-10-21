mod boundaries;
mod names;
mod observe;
mod placement;
mod queue;
mod seven_zip;
mod space;
mod staging;
mod volumes;

pub use queue::extract_queued;
pub use seven_zip::SevenZip;

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
