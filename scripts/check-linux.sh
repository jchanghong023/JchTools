#!/usr/bin/env bash
# Core-only validation. Requires Rust stable, GCC/Clang, and resolved Cargo dependencies.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
command -v cargo >/dev/null || { printf '%s\n' 'Rust toolchain is required on the test machine.' >&2; exit 1; }
cargo check --no-default-features --all-targets
cargo test --no-default-features --all-targets
if [[ -n "${JCHTOOLS_TEST_7ZIP:-}" ]]; then
    cargo test --no-default-features --test archive -- --ignored --test-threads=1
fi
