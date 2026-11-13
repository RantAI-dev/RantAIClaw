#!/usr/bin/env bash
#
# The local Rust quality gate: format, clippy and the identity-string check.
#
# Usage:
#   rust_quality_gate.sh [--strict] [--skip-fmt]
#
#   --strict    clippy denies every warning, not just the correctness group
#   --skip-fmt  skip `cargo fmt --all -- --check`
#
# `--skip-fmt` exists because the CI workflow runs the format check in its
# own cheap job (`fmt`), and running it here as well would check every file
# twice. Without the flag this script is the complete local gate — it is
# what `dev/ci.sh` and `.githooks/pre-push` call — so it formats first.

set -euo pipefail

MODE="correctness"
SKIP_FMT=false

for arg in "$@"; do
    case "$arg" in
        --strict) MODE="strict" ;;
        --skip-fmt) SKIP_FMT=true ;;
        *)
            echo "rust_quality_gate.sh: unknown argument '${arg}'" >&2
            echo "usage: rust_quality_gate.sh [--strict] [--skip-fmt]" >&2
            exit 2
            ;;
    esac
done

if [ "$SKIP_FMT" = "true" ]; then
    echo "==> rust quality: cargo fmt skipped (--skip-fmt)"
else
    echo "==> rust quality: cargo fmt --all -- --check"
    cargo fmt --all -- --check
fi

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
