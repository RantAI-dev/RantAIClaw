#!/usr/bin/env bash

set -euo pipefail

MODE="correctness"
if [ "${1:-}" = "--strict" ]; then
    MODE="strict"
fi

echo "==> rust quality: cargo fmt --all -- --check"
cargo fmt --all -- --check

if [ "$MODE" = "strict" ]; then
    echo "==> rust quality: cargo clippy --locked --all-targets -- -D warnings"
    cargo clippy --locked --all-targets -- -D warnings
else
    echo "==> rust quality: cargo clippy --locked --all-targets -- -D clippy::correctness"
    cargo clippy --locked --all-targets -- -D clippy::correctness
fi

# CLAUDE.md §9.1 identity gate. It is called from here, rather than as its own
# workflow step, because this effort may not edit .github/workflows/**. This
# script already runs in the PR lint job, so the check runs on pull requests —
# but only when Rust changed. Moving it to its own step alongside the other
# source-level guards would also cover docs-only PRs.
echo "==> identity strings: scripts/ci/check_identity_strings.sh"
./scripts/ci/check_identity_strings.sh --self-test
