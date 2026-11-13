#!/usr/bin/env bash
# Tests for the shared clippy JSON run and the two lint gates that read it.
#
# The CI lint job used to lint the crate twice: once in
# `rust_quality_gate.sh` and once in `rust_strict_delta_gate.sh`, each with
# its own lint flags. It now lints the crate once
# (`scripts/ci/run_clippy_json.sh`) and both gates read the JSON that run
# produced.
#
# The invariant these tests pin: the lint job fails in exactly the cases it
# fails today — any `clippy::correctness` finding no crate attribute lowered
# to a warning, anywhere, and any clippy warning on a line the branch changed.
#
#   1. a correctness error on an unchanged line fails the quality gate
#   2. a warning on a changed line fails the delta gate
#   3. a warning on an unchanged line passes both gates
#   4. a compile error fails
#   5. neither gate runs cargo when it reads a shared JSON file
#   6. CLIPPY_JSON_FILE without CLIPPY_EXIT_CODE fails both gates loudly
#   7. a clean clippy run passes both gates
#   8. a clippy crash with no error-level diagnostic fails the quality gate
#   9. a clippy crash with no parsable diagnostic fails the delta gate
#
# No cargo and no toolchain: the gates are driven with hand-written JSON
# fixtures in the rustc message schema, so the file runs in seconds and can
# be wired into CI.
#
# The delta gate needs a repository with a known changed-line range, so the
# tests build a throwaway git repo with one edited line and run the gate
# inside it with an explicit BASE_SHA.
#
# Usage:
#   bash scripts/ci/tests/test_clippy_json_gates.sh
#
# Exit code:
#   0 on PASS
#   1 on FAIL

set -uo pipefail

# Anchor on script location so the test works from any cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
QUALITY_GATE="${SCRIPT_DIR}/../rust_quality_gate.sh"
DELTA_GATE="${SCRIPT_DIR}/../rust_strict_delta_gate.sh"

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

if [ ! -f "$QUALITY_GATE" ]; then
    printf '%s\n' "quality gate script not found at $QUALITY_GATE" >&2
    exit 1
fi

if [ ! -f "$DELTA_GATE" ]; then
    printf '%s\n' "delta gate script not found at $DELTA_GATE" >&2
    exit 1
fi

if ! command -v git >/dev/null 2>&1; then
    printf '%s\n' "git is required to test the clippy JSON gates" >&2
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    printf '%s\n' "python3 is required to test the clippy JSON gates" >&2
    exit 1
fi

# --- fixtures ----------------------------------------------------------------

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

FIXTURE_DIR="$WORK_DIR/fixtures"
FIXTURE_REPO="$WORK_DIR/repo"
LOG_DIR="$WORK_DIR/logs"
mkdir -p "$FIXTURE_DIR" "$LOG_DIR"

# A cargo that must never run in JSON mode. It records the call and fails,
# so a gate that reaches for cargo in JSON mode fails the test instead of
# quietly running a 16-minute lint.
STUB_DIR="$WORK_DIR/stub"
mkdir -p "$STUB_DIR"
CARGO_STUB_MARKER="$WORK_DIR/cargo-was-called"
cat >"$STUB_DIR/cargo" <<'STUB'
#!/usr/bin/env bash
printf 'cargo was called: %s\n' "$*" >>"${CARGO_STUB_MARKER:-/dev/null}"
exit 99
STUB
chmod +x "$STUB_DIR/cargo"

# The rustc message schema: one JSON object per line. `reason` selects the
# message kind, `message.level` the severity, `message.code.code` the lint
# or error code, `message.spans[]` the location and `message.rendered` the
# human-readable rendering.

# A correctness lint at error level, on an unchanged line. The level in the
# JSON is what the gate reads. This crate's own `#![warn(clippy::all,
# clippy::pedantic)]` attribute lowers a correctness lint to `warning` (that
# is case 3), and the old quality-gate run reported it at that level too.
cat >"$FIXTURE_DIR/correctness_error.jsonl" <<'JSON'
{"reason":"build-finished","success":false}
{"reason":"compiler-message","message":{"level":"error","message":"this loop never actually loops","code":{"code":"clippy::never_loop"},"spans":[{"file_name":"src/lib.rs","byte_start":120,"byte_end":140,"line_start":3,"line_end":5,"is_primary":true,"label":"this loop never actually loops"}],"children":[],"rendered":"error: this loop never actually loops\n --> src/lib.rs:3:5\n  |\n3 | /     loop {\n  | |_____- this loop never actually loops\n  |\n  = note: `#[warn(clippy::never_loop)]` on by default\n"}}
JSON

# A lint at warning level, on the changed line (line 10).
cat >"$FIXTURE_DIR/warning_changed.jsonl" <<'JSON'
{"reason":"build-finished","success":true}
{"reason":"compiler-message","message":{"level":"warning","message":"this let binding could be const","code":{"code":"clippy::missing_const_for_fn"},"spans":[{"file_name":"src/lib.rs","byte_start":300,"byte_end":320,"line_start":10,"line_end":10,"is_primary":true,"label":"this let binding could be const"}],"children":[],"rendered":"warning: this let binding could be const\n --> src/lib.rs:10:5\n  |\n10 |     let binding = 1;\n   |     ^^^^^^^^^ could be const\n  |\n  = note: `#[warn(clippy::missing_const_for_fn)]` on by default\n"}}
JSON

# The same lint on an unchanged line (line 3).
cat >"$FIXTURE_DIR/warning_unchanged.jsonl" <<'JSON'
{"reason":"build-finished","success":true}
{"reason":"compiler-message","message":{"level":"warning","message":"this let binding could be const","code":{"code":"clippy::missing_const_for_fn"},"spans":[{"file_name":"src/lib.rs","byte_start":120,"byte_end":140,"line_start":3,"line_end":3,"is_primary":true,"label":"this let binding could be const"}],"children":[],"rendered":"warning: this let binding could be const\n --> src/lib.rs:3:5\n  |\n3 |     let binding = 1;\n   |     ^^^^^^^^^ could be const\n  |\n  = note: `#[warn(clippy::missing_const_for_fn)]` on by default\n"}}
JSON

# A compile error on the changed line. No `rendered` field, so the gate has
# to fall back to the message and the code.
cat >"$FIXTURE_DIR/compile_error.jsonl" <<'JSON'
{"reason":"build-finished","success":false}
{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/lib.rs","byte_start":300,"byte_end":320,"line_start":10,"line_end":10,"is_primary":true,"label":"expected `i32`, found `&str`"}],"children":[]}}
JSON

# A clippy run that failed without emitting a single diagnostic: the ICE /
# link-error / cargo-failure case. The exit code says clippy failed, but the
# JSON cannot say why.
cat >"$FIXTURE_DIR/crash_no_diagnostics.jsonl" <<'JSON'
{"reason":"build-finished","success":false}
JSON

# A clean run: no diagnostics at all.
cat >"$FIXTURE_DIR/clean.jsonl" <<'JSON'
{"reason":"build-finished","success":true}
JSON

# --- the fixture repository --------------------------------------------------
#
# One Rust file with twenty lines, and an uncommitted edit on line 10 only.
# The delta gate computes the changed range from `git diff`, so this is the
# range the fixtures are written against.
#
# The quality gate runs the identity-string check through a path relative to
# the working directory, so the gate runs in this throwaway repository rather
# than in the checkout: the `changes` job must not go red because the
# repository holds an identity string — the `identity` job owns that finding.

mkdir -p "$FIXTURE_REPO/src"
(
    cd "$FIXTURE_REPO" || exit 1
    git init -q -b main
    git config user.email "test@example.com"
    git config user.name "Test"
    for line in $(seq 1 20); do
        printf 'pub fn fixture_line_%02d() {}\n' "$line"
    done >src/lib.rs
    git add .
    git commit -q -m "initial fixture"
)
FIXTURE_BASE_SHA="$(git -C "$FIXTURE_REPO" rev-parse HEAD)"
sed -i '10s/.*/pub fn fixture_line_10() { let _changed = 1; }/' "$FIXTURE_REPO/src/lib.rs"

# The identity check the quality gate calls at the end of its run. It is
# copied in untracked, so the check has nothing of its own to scan.
mkdir -p "$FIXTURE_REPO/scripts/ci"
cp "$REPO_ROOT/scripts/ci/check_identity_strings.sh" "$FIXTURE_REPO/scripts/ci/"

# --- runners -----------------------------------------------------------------

# Run the quality gate. $1 is the JSON file ("" leaves CLIPPY_JSON_FILE
# unset), $2 the clippy exit code ("" leaves CLIPPY_EXIT_CODE unset), $3 the
# log file. Prints the gate's exit code.
#
# The gate runs the identity-string check through a path relative to the
# working directory, so it runs in the fixture repository.
run_quality_gate() {
    local json_file="$1"
    local exit_code="$2"
    local log="$3"

    rm -f "$CARGO_STUB_MARKER"
    (
        cd "$FIXTURE_REPO" || exit 1
        export PATH="$STUB_DIR:$PATH"
        export CARGO_STUB_MARKER="$CARGO_STUB_MARKER"
        if [ -n "$json_file" ]; then
            export CLIPPY_JSON_FILE="$json_file"
        else
            unset CLIPPY_JSON_FILE
        fi
        if [ -n "$exit_code" ]; then
            export CLIPPY_EXIT_CODE="$exit_code"
        else
            unset CLIPPY_EXIT_CODE
        fi
        bash "$QUALITY_GATE" --skip-fmt
    ) >"$log" 2>&1
    echo $?
}

# Run the delta gate. Same arguments as run_quality_gate. The gate runs
# inside the fixture repository, so its changed-line range is the one the
# fixtures were written against.
run_delta_gate() {
    local json_file="$1"
    local exit_code="$2"
    local log="$3"

    rm -f "$CARGO_STUB_MARKER"
    (
        cd "$FIXTURE_REPO" || exit 1
        export PATH="$STUB_DIR:$PATH"
        export CARGO_STUB_MARKER="$CARGO_STUB_MARKER"
        export BASE_SHA="$FIXTURE_BASE_SHA"
        if [ -n "$json_file" ]; then
            export CLIPPY_JSON_FILE="$json_file"
        else
            unset CLIPPY_JSON_FILE
        fi
        if [ -n "$exit_code" ]; then
            export CLIPPY_EXIT_CODE="$exit_code"
        else
            unset CLIPPY_EXIT_CODE
        fi
        bash "$DELTA_GATE"
    ) >"$log" 2>&1
    echo $?
}

# --- assertions --------------------------------------------------------------

assert_exit() {
    local want="$1" got="$2" label="$3"
    if [ "$got" = "$want" ]; then
        log_pass "$label"
    else
        log_fail "$label (expected exit $want, got $got)"
    fi
}

assert_no_cargo_call() {
    local label="$1"
    if [ ! -f "$CARGO_STUB_MARKER" ]; then
        log_pass "$label"
    else
        log_fail "$label (a gate ran cargo while reading a shared JSON file)"
    fi
}

assert_log_contains() {
    local needle="$1" file="$2" label="$3"
    if grep -qF -- "$needle" "$file"; then
        log_pass "$label"
    else
        log_fail "$label (not found in the gate output: ${needle})"
    fi
}

# --- case 1: a correctness error on an unchanged line ------------------------

printf '\n=== case 1: correctness error on an unchanged line ===\n'
LOG1="$LOG_DIR/case1-quality.log"
EC1="$(run_quality_gate "$FIXTURE_DIR/correctness_error.jsonl" "1" "$LOG1")"
cat "$LOG1"
assert_exit "1" "$EC1" "quality gate fails on a correctness error"
assert_log_contains "clippy::never_loop" "$LOG1" "quality gate names the correctness lint"
assert_no_cargo_call "quality gate did not run cargo"

LOG1B="$LOG_DIR/case1-delta.log"
EC1B="$(run_delta_gate "$FIXTURE_DIR/correctness_error.jsonl" "1" "$LOG1B")"
cat "$LOG1B"
assert_exit "0" "$EC1B" "delta gate does not block on a correctness error outside the changed lines"
assert_no_cargo_call "delta gate did not run cargo"

# --- case 2: a warning on a changed line -------------------------------------

printf '\n=== case 2: warning on a changed line ===\n'
LOG2="$LOG_DIR/case2-delta.log"
EC2="$(run_delta_gate "$FIXTURE_DIR/warning_changed.jsonl" "0" "$LOG2")"
cat "$LOG2"
assert_exit "1" "$EC2" "delta gate fails on a warning on a changed line"
assert_log_contains "Strict lint issues introduced on changed Rust lines" "$LOG2" "delta gate reports the blocking warning"
assert_no_cargo_call "delta gate did not run cargo"

LOG2B="$LOG_DIR/case2-quality.log"
EC2B="$(run_quality_gate "$FIXTURE_DIR/warning_changed.jsonl" "0" "$LOG2B")"
cat "$LOG2B"
assert_exit "0" "$EC2B" "quality gate passes on a warning (correctness level only)"
assert_no_cargo_call "quality gate did not run cargo"

# --- case 3: a warning on an unchanged line ----------------------------------

printf '\n=== case 3: warning on an unchanged line ===\n'
LOG3="$LOG_DIR/case3-quality.log"
EC3="$(run_quality_gate "$FIXTURE_DIR/warning_unchanged.jsonl" "0" "$LOG3")"
cat "$LOG3"
assert_exit "0" "$EC3" "quality gate passes on a warning outside the changed lines"

LOG3B="$LOG_DIR/case3-delta.log"
EC3B="$(run_delta_gate "$FIXTURE_DIR/warning_unchanged.jsonl" "0" "$LOG3B")"
cat "$LOG3B"
assert_exit "0" "$EC3B" "delta gate passes on a warning outside the changed lines"
assert_no_cargo_call "delta gate did not run cargo"

# --- case 4: a compile error -------------------------------------------------

printf '\n=== case 4: compile error ===\n'
LOG4="$LOG_DIR/case4-quality.log"
EC4="$(run_quality_gate "$FIXTURE_DIR/compile_error.jsonl" "101" "$LOG4")"
cat "$LOG4"
assert_exit "1" "$EC4" "quality gate fails on a compile error"
assert_log_contains "E0308" "$LOG4" "quality gate names the compile error"
assert_log_contains "mismatched types" "$LOG4" "quality gate reports the compile error without a rendered field"

LOG4B="$LOG_DIR/case4-delta.log"
EC4B="$(run_delta_gate "$FIXTURE_DIR/compile_error.jsonl" "101" "$LOG4B")"
cat "$LOG4B"
assert_exit "1" "$EC4B" "delta gate fails on a compile error on a changed line"

# --- case 5: neither gate runs cargo in JSON mode ----------------------------

printf '\n=== case 5: no cargo call in JSON mode ===\n'
LOG5="$LOG_DIR/case5-quality.log"
run_quality_gate "$FIXTURE_DIR/warning_unchanged.jsonl" "0" "$LOG5" >/dev/null
assert_no_cargo_call "quality gate did not run cargo in JSON mode"

LOG5B="$LOG_DIR/case5-delta.log"
run_delta_gate "$FIXTURE_DIR/warning_unchanged.jsonl" "0" "$LOG5B" >/dev/null
assert_no_cargo_call "delta gate did not run cargo in JSON mode"

# --- case 6: the two variables have to be set together ------------------------

printf '\n=== case 6: CLIPPY_JSON_FILE without CLIPPY_EXIT_CODE ===\n'
LOG6="$LOG_DIR/case6-quality.log"
EC6="$(run_quality_gate "$FIXTURE_DIR/warning_unchanged.jsonl" "" "$LOG6")"
cat "$LOG6"
assert_exit "2" "$EC6" "quality gate fails when the clippy exit code is missing"
assert_log_contains "must be set together" "$LOG6" "quality gate says the two variables belong to one run"

LOG6B="$LOG_DIR/case6-delta.log"
EC6B="$(run_delta_gate "$FIXTURE_DIR/warning_unchanged.jsonl" "" "$LOG6B")"
cat "$LOG6B"
assert_exit "2" "$EC6B" "delta gate fails when the clippy exit code is missing"
assert_log_contains "must be set together" "$LOG6B" "delta gate says the two variables belong to one run"

# --- case 7: a clean clippy run ----------------------------------------------

printf '\n=== case 7: clean clippy run ===\n'
LOG7="$LOG_DIR/case7-quality.log"
EC7="$(run_quality_gate "$FIXTURE_DIR/clean.jsonl" "0" "$LOG7")"
cat "$LOG7"
assert_exit "0" "$EC7" "quality gate passes on a clean clippy run"

LOG7B="$LOG_DIR/case7-delta.log"
EC7B="$(run_delta_gate "$FIXTURE_DIR/clean.jsonl" "0" "$LOG7B")"
cat "$LOG7B"
assert_exit "0" "$EC7B" "delta gate passes on a clean clippy run"

# --- case 8: a clippy crash with no error-level diagnostic -------------------

printf '\n=== case 8: clippy crash with no error diagnostic ===\n'
LOG8="$LOG_DIR/case8-quality.log"
EC8="$(run_quality_gate "$FIXTURE_DIR/crash_no_diagnostics.jsonl" "101" "$LOG8")"
cat "$LOG8"
assert_exit "2" "$EC8" "quality gate fails safe when clippy exits non-zero with no error diagnostic"
assert_log_contains "no error-level diagnostic explains it" "$LOG8" "quality gate names the fail-safe reason"

# --- case 9: a clippy crash with no parsable diagnostic ----------------------

printf '\n=== case 9: clippy crash with nothing parsable ===\n'
LOG9="$LOG_DIR/case9-delta.log"
EC9="$(run_delta_gate "$FIXTURE_DIR/crash_no_diagnostics.jsonl" "101" "$LOG9")"
cat "$LOG9"
assert_exit "2" "$EC9" "delta gate fails safe when the shared run exits non-zero with no parsable diagnostic"
assert_log_contains "without parsable diagnostics" "$LOG9" "delta gate names the fail-safe reason"

# --- summary -----------------------------------------------------------------

printf '\n=== summary ===\n'
printf 'Result: %d passed, %d failed\n' "$PASS" "$FAIL"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
