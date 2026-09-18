#!/usr/bin/env bash
# Core-only validation. Requires the pinned Rust toolchain (rust-toolchain.toml), GCC/Clang, and resolved Cargo dependencies.
set -euo pipefail
cd -- "$(dirname "${BASH_SOURCE[0]}")/.."
command -v cargo >/dev/null || { printf '%s\n' 'Rust toolchain is required on the test machine.' >&2; exit 1; }
# 格式门禁：全仓库必须 rustfmt-clean（不合格时本地运行 cargo fmt --all 即可修复）。
cargo fmt --all -- --check
# clippy 零告警硬门禁（替代原 cargo check：clippy 是其严格超集）。
# [lints] 已全量 deny，-D warnings 再兜底未归类告警；任何告警即失败。
cargo clippy --no-default-features --all-targets -- -D warnings
cargo test --no-default-features --all-targets
if [[ -n "${JCHTOOLS_TEST_7ZIP:-}" ]]; then
    cargo test --no-default-features --test archive -- --ignored --test-threads=1
fi
