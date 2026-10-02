#!/usr/bin/env bash
# scripts/ci/run_filtered_lib_tests.sh — wrapper around `cargo test --lib <filter>`
# that refuses to silently pass when the filter selects no test.
#
# `cargo test --lib <filter>` exits 0 when a name filter matches no test.
# That is the exact failure mode the per-store migration gate had on the
# release workflow for several releases (`pub-release.yml`'s own comment
# records the v0.6.47 story: two positional filters silently broke it).
# This script turns the silent pass into a hard failure by listing the
# selection first and refusing to run the tests when the count is zero.
#
# Usage:
#   bash scripts/ci/run_filtered_lib_tests.sh [CARGO_ARGS...]
#
# The cargo arguments are whatever the workflow passes today, including any
# `--` block with multiple positional filters and `--skip` flags:
#
#   bash scripts/ci/run_filtered_lib_tests.sh --locked --lib config::migrations
#   bash scripts/ci/run_filtered_lib_tests.sh --locked --lib -- migration:: memory::snapshot:: --skip profile::migration
#   bash scripts/ci/run_filtered_lib_tests.sh --locked --features channel-lark --lib channels::lark
#
# The script derives a `--list` invocation from the same args:
#   * if the args contain a standalone `--`, append `--list` at the very end;
#   * otherwise append `-- --list` so libtest gets the `--list` flag.
#
# It then runs the listing against the real `cargo` (inheriting the
# workflow's env), counts lines ending in `: test`, and either refuses to
# run (count == 0, with a clear error) or execs the original command
# verbatim — argument order and the `--` block preserved exactly.
#
# Exit codes:
#   0  on success (tests ran and passed)
#   1  when the listing selects zero tests, the cargo invocation itself
#      fails, or the tests themselves fail
#
# Must work under Git Bash on windows-latest (Git Bash ships bash 4.4+ but
# the script avoids bash-4-only features like associative arrays and
# mapfile on purpose). No GNU-only flags; portable POSIX-ish bash only.

set -uo pipefail

# Anchor on script location for a stable log prefix.
SCRIPT_NAME="${BASH_SOURCE[0]##*/}"

# Keep every argument exactly as received. The script must exec the
# original command verbatim so a test invocation that worked before this
# wrapper still works byte-for-byte.
cargo_args=("$@")

if [ "${#cargo_args[@]}" -eq 0 ]; then
    printf '%s: at least one cargo argument is required\n' "$SCRIPT_NAME" >&2
    exit 1
fi

# Build the listing invocation. libtest prints each selected test on its
# own line ending in `: test`; counting those lines is robust to any
# "N tests, M benchmarks" header / footer lines a future libtest might
# emit.
has_dashdash=no
for arg in "${cargo_args[@]}"; do
    if [ "$arg" = "--" ]; then
        has_dashdash=yes
        break
    fi
done

if [ "$has_dashdash" = "yes" ]; then
    list_args=("${cargo_args[@]}" --list)
else
    list_args=("${cargo_args[@]}" -- --list)
fi

# Run the listing. Errors here are real failures, not the "no tests" case.
list_output="$(cargo "${list_args[@]}" 2>&1)" || {
    printf '%s: cargo --list failed:\n' "$SCRIPT_NAME" >&2
    printf '%s\n' "$list_output" >&2
    exit 1
}

# Count lines ending in `: test`. That is the only piece of libtest output
# the script depends on; future header/footer formatting changes do not
# change the per-test lines.
count=$(printf '%s\n' "$list_output" | grep -c ': test$' || true)

if [ "$count" -eq 0 ]; then
    {
        printf '%s: filter selected 0 tests — the gate would have passed silently, refusing to run.\n' "$SCRIPT_NAME"
        printf '  args:'
        for arg in "${cargo_args[@]}"; do
            printf ' %s' "$arg"
        done
        printf '\n'
    } >&2
    exit 1
fi

printf '%s: %s test(s) match the filter.\n' "$SCRIPT_NAME" "$count"
exec cargo "${cargo_args[@]}"