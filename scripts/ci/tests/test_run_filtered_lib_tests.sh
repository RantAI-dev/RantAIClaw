#!/usr/bin/env bash
# Tests for scripts/ci/run_filtered_lib_tests.sh.
#
# Validates the three behaviours the wrapper must keep:
#
#   1. Two tests match → script exits 0 and runs `cargo` exactly once with
#      the original arguments (no `--list`).
#   2. Empty list → script exits non-zero and never runs `cargo`.
#   3. `--`-form args (`--locked --lib -- migration:: memory::snapshot::
#      --skip profile::migration`) produce a listing invocation that
#      appends `--list` at the end, not after a fresh `--` (the existing
#      `--` is already the libtest separator).
#
# The test puts a fake `cargo` first on PATH so the wrapper invokes the
# fake rather than the real toolchain. The fake binary inspects its
# arguments: `--list` selects the canned listing output; anything else
# records that the test run happened (marker file + arg log).
#
# Each case uses its own hermetic temp directory so a failure in one case
# cannot poison the next.
#
# Usage:
#   bash scripts/ci/tests/test_run_filtered_lib_tests.sh
#
# Exit code:
#   0 on PASS
#   1 on FAIL

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="${SCRIPT_DIR}/../run_filtered_lib_tests.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

PASS=0
FAIL=0

log_pass() {
    printf '%b\n' "${GREEN}PASS${NC} $1"
    PASS=$((PASS + 1))
}

log_fail() {
    printf '%b\n' "${RED}FAIL${NC} $1"
    FAIL=$((FAIL + 1))
}

# --- preflight ---------------------------------------------------------------

if [ ! -f "$SCRIPT" ]; then
    printf 'script not found at %s\n' "$SCRIPT" >&2
    exit 1
fi

# --- helpers -----------------------------------------------------------------

# Build a fake `cargo` in $workdir/bin that records every invocation and
# prints the canned listing when called with --list.
make_fake_cargo() {
    local workdir="$1"
    local marker="$2"
    local args_log="$3"
    local listing="$4"

    rm -rf "$workdir"
    mkdir -p "$workdir/bin"
    cat > "$workdir/bin/cargo" <<EOF
#!/usr/bin/env bash
# Fake cargo for testing run_filtered_lib_tests.sh.
# Two modes, decided from the args:
#   * --list appears among the args  -> print the canned listing, log LIST
#   * else                            -> write a run marker, log RUN
listing=
seen_list=no
for arg in "\$@"; do
    if [ "\$arg" = "--list" ]; then
        seen_list=yes
        break
    fi
done

if [ "\$seen_list" = "yes" ]; then
    printf '%s\n' "$listing"
    {
        printf 'LIST'
        for arg in "\$@"; do
            printf ' %s' "\$arg"
        done
        printf '\n'
    } >> "$args_log"
    exit 0
fi

{
    printf 'RUN'
    for arg in "\$@"; do
        printf ' %s' "\$arg"
    done
    printf '\n'
} >> "$args_log"
: > "$marker"
exit 0
EOF
    chmod +x "$workdir/bin/cargo"
}

# --- cases -------------------------------------------------------------------

printf '\n=== case 1: two listed tests ===\n'
WORK1="$(mktemp -d)"
trap 'rm -rf "$WORK1"' EXIT
MARKER1="$WORK1/ran.marker"
ARGS_LOG1="$WORK1/args.log"
LISTING1="config::migrations::tests::a: test
config::migrations::tests::b: test
2 tests, 0 benchmarks"
make_fake_cargo "$WORK1" "$MARKER1" "$ARGS_LOG1" "$LISTING1"

# Run the wrapper with a filter that the fake lists as a hit. PATH is
# scoped to this case via the env prefix.
(
    cd "$WORK1"
    PATH="$WORK1/bin:$PATH" bash "$SCRIPT" --locked --lib config::migrations
) >"$WORK1/stdout.log" 2>"$WORK1/stderr.log"
EC1=$?

if [ "$EC1" -eq 0 ]; then
    log_pass "two listed tests: script exits 0 (exit=$EC1)"
else
    log_fail "two listed tests: expected exit 0 (got $EC1)"
    printf 'stdout: %s\n' "$(cat "$WORK1/stdout.log")"
    printf 'stderr: %s\n' "$(cat "$WORK1/stderr.log")"
fi

if [ -f "$MARKER1" ]; then
    log_pass "two listed tests: fake cargo's run marker is present"
else
    log_fail "two listed tests: fake cargo's run marker is missing"
fi

# The original args must have been passed verbatim, once.
if grep -q '^RUN --locked --lib config::migrations$' "$ARGS_LOG1"; then
    log_pass "two listed tests: original args passed exactly once, verbatim"
else
    log_fail "two listed tests: expected one RUN line for the original args"
    printf 'args log:\n%s\n' "$(cat "$ARGS_LOG1")"
fi

# Listing invocation must not contain the trailing `--list` flag alone —
# it must be preceded by `-- --list` (no `--` block in the original args).
if grep -q '^LIST --locked --lib config::migrations -- --list$' "$ARGS_LOG1"; then
    log_pass "two listed tests: listing invocation has -- --list appended"
else
    log_fail "two listed tests: listing invocation should end in '-- --list'"
    printf 'args log:\n%s\n' "$(cat "$ARGS_LOG1")"
fi

printf '\n=== case 2: empty list ===\n'
WORK2="$(mktemp -d)"
trap 'rm -rf "$WORK1" "$WORK2"' EXIT
MARKER2="$WORK2/ran.marker"
ARGS_LOG2="$WORK2/args.log"
LISTING2="0 tests, 0 benchmarks"
make_fake_cargo "$WORK2" "$MARKER2" "$ARGS_LOG2" "$LISTING2"

(
    cd "$WORK2"
    PATH="$WORK2/bin:$PATH" bash "$SCRIPT" --locked --lib no_such_module_xyz
) >"$WORK2/stdout.log" 2>"$WORK2/stderr.log"
EC2=$?

if [ "$EC2" -ne 0 ]; then
    log_pass "empty list: script exits non-zero (exit=$EC2)"
else
    log_fail "empty list: expected non-zero exit (got $EC2)"
    printf 'stdout: %s\n' "$(cat "$WORK2/stdout.log")"
    printf 'stderr: %s\n' "$(cat "$WORK2/stderr.log")"
fi

if [ ! -f "$MARKER2" ]; then
    log_pass "empty list: fake cargo did NOT run the tests (no marker)"
else
    log_fail "empty list: fake cargo's run marker is present — script ran tests on an empty selection"
fi

# The wrapper's stderr must name the script and the args so a CI log
# points an operator at the cause.
if grep -q "filter selected 0 tests" "$WORK2/stderr.log"; then
    log_pass "empty list: stderr names the silent-pass failure"
else
    log_fail "empty list: stderr should mention 'filter selected 0 tests'"
    printf 'stderr: %s\n' "$(cat "$WORK2/stderr.log")"
fi

printf '\n=== case 3: --form args ===\n'
WORK3="$(mktemp -d)"
trap 'rm -rf "$WORK1" "$WORK2" "$WORK3"' EXIT
MARKER3="$WORK3/ran.marker"
ARGS_LOG3="$WORK3/args.log"
LISTING3="migration::tests::m: test
memory::snapshot::tests::s: test
2 tests, 0 benchmarks"
make_fake_cargo "$WORK3" "$MARKER3" "$ARGS_LOG3" "$LISTING3"

(
    cd "$WORK3"
    PATH="$WORK3/bin:$PATH" bash "$SCRIPT" \
        --locked --lib -- migration:: memory::snapshot:: --skip profile::migration
) >"$WORK3/stdout.log" 2>"$WORK3/stderr.log"
EC3=$?

if [ "$EC3" -eq 0 ]; then
    log_pass "--form args: script exits 0 (exit=$EC3)"
else
    log_fail "--form args: expected exit 0 (got $EC3)"
    printf 'stdout: %s\n' "$(cat "$WORK3/stdout.log")"
    printf 'stderr: %s\n' "$(cat "$WORK3/stderr.log")"
fi

# Listing invocation must end with `--list` (the existing `--` is the
# libtest separator, so a fresh `--` would double up). The flag must come
# AFTER `--skip profile::migration`, exactly like the original args, plus
# `--list`.
if grep -q '^LIST --locked --lib -- migration:: memory::snapshot:: --skip profile::migration --list$' "$ARGS_LOG3"; then
    log_pass "--form args: listing invocation appends --list at the very end"
else
    log_fail "--form args: listing invocation should end with ' --list' (no fresh --)"
    printf 'args log:\n%s\n' "$(cat "$ARGS_LOG3")"
fi

# Original args must have been preserved verbatim on the run.
if grep -q '^RUN --locked --lib -- migration:: memory::snapshot:: --skip profile::migration$' "$ARGS_LOG3"; then
    log_pass "--form args: original --block passed exactly once, verbatim"
else
    log_fail "--form args: expected one RUN line for the original --block"
    printf 'args log:\n%s\n' "$(cat "$ARGS_LOG3")"
fi

# --- summary -----------------------------------------------------------------

printf '\n=== summary ===\n'
printf 'Result: %d passed, %d failed\n' "$PASS" "$FAIL"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0