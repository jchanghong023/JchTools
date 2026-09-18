#!/usr/bin/env bash
# Core-only validation. Requires the pinned Rust toolchain (rust-toolchain.toml), GCC/Clang, and resolved Cargo dependencies.
set -euo pipefail
cd -- "$(dirname "${BASH_SOURCE[0]}")/.."
command -v cargo >/dev/null || { printf '%s\n' 'Rust toolchain is required on the test machine.' >&2; exit 1; }
# 格式门禁：全仓库必须 rustfmt-clean（不合格时本地运行 cargo fmt --all 即可修复）。
cargo fmt --all -- --check
# clippy 预算棘轮（替代原 cargo check：clippy 是其严格超集）。
# 预算含少量余量覆盖仅 Unix 编译的代码；首轮 CI 实测后应按输出计数收紧至零余量。
python3 scripts/clippy_budget.py --budget 225 -- --no-default-features --all-targets
cargo test --no-default-features --all-targets
if [[ -n "${JCHTOOLS_TEST_7ZIP:-}" ]]; then
    cargo test --no-default-features --test archive -- --ignored --test-threads=1
fi
