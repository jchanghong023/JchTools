//! Bounded, cancellable process I/O; no shell and no unbounded Command::output buffers.
mod capture;
mod pipe;
mod stream;

pub use capture::{
    join_with_deadline, run_to_completion, run_with_timeout, run_with_timeout_cancel,
    run_with_timeout_control_limit, CapturedOutput,
};
pub use stream::{run, run_with_idle_timeout, DEFAULT_IDLE_TIMEOUT};

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
mod tests_runtime;
