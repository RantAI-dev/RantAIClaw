#!/usr/bin/env bash
# Tests for scripts/ci/rust_strict_delta_gate.sh.
#
# Validates three behaviours the gate must keep:
#
#   1. Empty diff (clean working tree, no committed Rust changes since BASE_SHA):
#      exit 0 with the unambiguous "skipping" message.
#   2. Uncommitted Rust edit with BASE_SHA == HEAD: the gate must NOT print
#      "skipping" — it must proceed to lint the working-tree file.
#   3. The script passes `--keep-going` to `cargo clippy` so lints in tests
#      under `lib test` are not skipped when an earlier target fails.
#
# The test runs against a throwaway git repo so it does not depend on the
# caller's working tree. Tests 2 relies on `cargo clippy` being available;
# if it is missing the test skips with a clear message instead of failing.
#
# Usage:
#   bash scripts/ci/tests/test_strict_delta_gate.sh
#
# Exit code:
#   0 on PASS
#   1 on FAIL
#   3 if a dependency (cargo) is unavailable

set -uo pipefail

# Anchor on script location so the test works from any cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="${SCRIPT_DIR}/../rust_strict_delta_gate.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

PASS=0
FAIL=0
SKIPPED=0

log_pass() {
    printf '%b\n' "${GREEN}PASS${NC} $1"
    PASS=$((PASS + 1))
}

log_fail() {
    printf '%b\n' "${RED}FAIL${NC} $1"
    FAIL=$((FAIL + 1))
}

log_skip() {
    printf '%b\n' "${YELLOW}SKIP${NC} $1"
    SKIPPED=$((SKIPPED + 1))
}

# --- preflight ---------------------------------------------------------------

if [ ! -f "$GATE" ]; then
    printf '%s\n' "gate script not found at $GATE" >&2
    exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
    log_skip "cargo not on PATH; skipping integration tests"
    printf '\nResult: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIPPED"
    exit 3
fi

# --- fixture -----------------------------------------------------------------

setup_repo() {
    local repo_dir="$1"
    rm -rf "$repo_dir"
    mkdir -p "$repo_dir"
    (
        cd "$repo_dir"
        git init -q -b main
        git config user.email "test@example.com"
        git config user.name "Test"

        # Minimal Cargo project that builds cleanly with `cargo clippy`.
        cat > Cargo.toml <<'TOML'
[package]
name = "strict-delta-test-fixture"
version = "0.0.0"
edition = "2021"
TOML
        mkdir -p src
        cat > src/lib.rs <<'RS'
pub fn answer() -> i32 {
    42
}
RS

        git add .
        git commit -q -m "initial"
    )
}

REPO_DIR="$(mktemp -d)"
trap 'rm -rf "$REPO_DIR"' EXIT
setup_repo "$REPO_DIR"

run_gate() {
    # Run the gate inside the fixture repo with an explicit BASE_SHA.
    # Captures stdout+stderr separately so we can inspect both.
    local base_sha="$1"
    local log="$REPO_DIR/gate.log"
    (
        cd "$REPO_DIR"
        BASE_SHA="$base_sha" bash "$GATE"
    ) >"$log" 2>&1
    echo $?
    cat "$log"
}

# --- test 1: empty diff ------------------------------------------------------

printf '\n=== test 1: empty diff ===\n'
OUT1="$(cd "$REPO_DIR" && BASE_SHA="$(git -C "$REPO_DIR" rev-parse HEAD)" bash "$GATE")"
EC1=$?
echo "$OUT1"

if [ "$EC1" -eq 0 ] && printf '%s\n' "$OUT1" | grep -q "skipping strict delta gate"; then
    log_pass "empty diff exits 0 with 'skipping' message"
else
    log_fail "empty diff should exit 0 with 'skipping' message (got exit=$EC1)"
fi

# --- test 2: uncommitted edit ------------------------------------------------

printf '\n=== test 2: uncommitted edit ===\n'
# Add an uncommitted Rust change that triggers a clippy/rustc lint.
# A comment-only edit is lint-clean: clippy would exit 0 and the gate would
# print "Strict delta gate passed: no strict warnings/errors.", conflating
# "gate ran and found nothing" with "gate silently skipped". The line below
# introduces a never-referenced local inside a never-called function, so the
# `-D warnings` flag promotes both `dead_code` (for `unused_fn`) and
# `unused_variables` (for `unused_var`) to hard errors and the gate exits
# non-zero on a working-tree edit.
printf '\nfn unused_fn() { let unused_var: u32 = 5; }\n' >> "$REPO_DIR/src/lib.rs"

LOG2="$REPO_DIR/gate2.log"
(
    cd "$REPO_DIR"
    BASE_SHA="$(git -C "$REPO_DIR" rev-parse HEAD)" bash "$GATE"
) >"$LOG2" 2>&1
EC2=$?
echo "exit=$EC2"
cat "$LOG2"

if grep -q "No Rust source files changed" "$LOG2"; then
    log_fail "gate silently skipped with an uncommitted Rust edit"
elif grep -q "Strict delta linting changed Rust files" "$LOG2"; then
    log_pass "uncommitted edit is linted (gate proceeded past skip check)"
else
    log_fail "gate output is ambiguous — neither 'skipping' nor 'linting' line found"
fi

# The exact exit code is clippy-dependent. We only require that it is not a
# silent success on an uncommitted edit.
if [ "$EC2" -eq 0 ]; then
    log_fail "uncommitted edit exited 0 — gate may still be silent on dirty trees"
else
    log_pass "uncommitted edit exits non-zero (EC=$EC2)"
fi

# --- test 3: --keep-going is passed to clippy --------------------------------

printf '\n=== test 3: --keep-going flag ===\n'
if grep -q -- "--keep-going" "$GATE"; then
    log_pass "--keep-going is present in the gate script"
else
    log_fail "--keep-going missing from the gate script"
fi

# --- test 4: uncommitted edit on a different line than a baseline warning ----

printf '\n=== test 4: uncommitted edit + baseline warning on an untouched line ===\n'
# A separate fixture: a file with a committed baseline warning (dead_code on
# line 1, from a never-called function) and an uncommitted edit that appends
# a new function further down. The pre-fix gate coalesced the file into
# `[[1, line_count]]` on any dirty edit, which reported the baseline warning
# as blocking. The fix is to keep the working-tree diff ranges only, so the
# baseline warning stays in the non-blocking bucket. Pre-generate Cargo.lock
# so the gate's `cargo clippy --locked` can run on the throwaway fixture.
BASELINE_REPO="$(mktemp -d)"
(
    cd "$BASELINE_REPO"
    git init -q -b main
    git config user.email "test@example.com"
    git config user.name "Test"
    cat > Cargo.toml <<'TOML'
[package]
name = "strict-delta-baseline-fixture"
version = "0.0.0"
edition = "2021"
TOML
    mkdir -p src
    cat > src/lib.rs <<'RS'
fn unused_baseline() { let _x = 5; }

pub fn answer() -> i32 {
    42
}
RS
    git add .
    git commit -q -m "initial with baseline warning"
    cargo generate-lockfile -q >/dev/null 2>&1 || true
    if [ -f Cargo.lock ]; then
        git add Cargo.lock
        git commit -q -m "lockfile" || true
    fi
)

BASELINE_LOG="$BASELINE_REPO/gate.log"
(
    cd "$BASELINE_REPO"
    printf '\npub fn new_answer() -> i32 {\n    43\n}\n' >> src/lib.rs
    BASE_SHA="$(git rev-parse HEAD)" bash "$GATE"
) >"$BASELINE_LOG" 2>&1
EC4=$?
echo "exit=$EC4"
cat "$BASELINE_LOG"

if [ "$EC4" -eq 0 ] \
    && grep -q "Existing strict lint issues outside changed Rust lines" "$BASELINE_LOG" \
    && ! grep -q "Strict lint issues introduced on changed Rust lines" "$BASELINE_LOG"; then
    log_pass "baseline warning on untouched line is non-blocking"
else
    log_fail "baseline warning on untouched line should be non-blocking (EC=$EC4)"
fi
rm -rf "$BASELINE_REPO"

# --- test 5: committed warning + uncommitted prepend that shifts its line ----

printf '\n=== test 5: committed warning + uncommitted prepend shifts its line ===\n'
# A separate fixture with two commits: an initial clean file, then a commit
# that adds an unused function (a committed warning at HEAD line N). The
# working tree then prepends M uncommitted lines, which shifts the warning
# to working-tree line N+M. The pre-fix classifier concatenated ranges from
# `git diff BASE..HEAD` (HEAD coordinates) with ranges from `git diff HEAD`
# (working-tree coordinates) — two coordinate systems, both treated as one.
# The committed ranges pointed at HEAD lines, clippy reported the warning in
# working-tree coordinates, and a prepend could shift the warning off every
# range, so the gate exited 0 on what is a real new warning. The fix uses one
# diff of BASE against the working tree for dirty files, which gives all
# ranges in working-tree coordinates (clippy's own) and covers the shifted
# warning. This test fails on the concatenation and passes on the unified
# coordinate system.
PREPEND_REPO="$(mktemp -d)"
(
    cd "$PREPEND_REPO"
    git init -q -b main
    git config user.email "test@example.com"
    git config user.name "Test"
    cat > Cargo.toml <<'TOML'
[package]
name = "strict-delta-prepend-fixture"
version = "0.0.0"
edition = "2021"
TOML
    mkdir -p src
    cat > src/lib.rs <<'RS'
pub fn answer() -> i32 {
    42
}
RS
    git add .
    git commit -q -m "initial clean"
    cargo generate-lockfile -q >/dev/null 2>&1 || true
    if [ -f Cargo.lock ]; then
        git add Cargo.lock
        git commit -q -m "lockfile" || true
    fi
    # Commit a warning-introducing change on top of the clean base.
    printf '\nfn unused_committed() { let _x = 5; }\n' >> src/lib.rs
    git add src/lib.rs
    git commit -q -m "introduce committed warning"
)
# Capture the SHA of the commit BEFORE the warning-introducing commit. The
# gate must treat the committed warning as "on a changed line", so its BASE
# has to predate the commit — test 4 used BASE_SHA=HEAD on a single commit,
# which only works when the changed lines are uncommitted. Here the warning
# is committed, so HEAD itself is the wrong base.
PREPEND_BASE="$(git -C "$PREPEND_REPO" rev-parse HEAD~1)"

PREPEND_LOG="$PREPEND_REPO/gate.log"
(
    cd "$PREPEND_REPO"
    # Uncommitted prepend shifts the committed warning's line numbers down.
    # Comments at the top are lint-clean, so they don't add their own
    # blocking warnings that would mask the shifted-line defect.
    {
        echo '// pre1'
        echo '// pre2'
        echo '// pre3'
        cat src/lib.rs
    } > src/lib.rs.new
    mv src/lib.rs.new src/lib.rs
    BASE_SHA="$PREPEND_BASE" bash "$GATE"
) >"$PREPEND_LOG" 2>&1
EC5=$?
echo "exit=$EC5"
cat "$PREPEND_LOG"

if [ "$EC5" -ne 0 ] \
    && grep -q "Strict lint issues introduced on changed Rust lines" "$PREPEND_LOG"; then
    log_pass "committed warning stays blocking after uncommitted prepend shifts its line"
else
    log_fail "committed warning should remain blocking after uncommitted prepend shifts its line (EC=$EC5)"
fi
rm -rf "$PREPEND_REPO"

# --- summary -----------------------------------------------------------------

printf '\n=== summary ===\n'
printf 'Result: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIPPED"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0