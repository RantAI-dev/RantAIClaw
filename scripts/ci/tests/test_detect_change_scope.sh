#!/usr/bin/env bash
# Tests for scripts/ci/detect_change_scope.sh.
#
# Validates the change-scope classifier. The invariant: every tracked file
# that changes what a Rust job in ci-run.yml builds, lints, or checks must
# set rust_changed=true; a docs-only change must keep rust_changed=false
# and set docs_only=true.
#
# The second invariant is the narrower `channel_features_changed` flag: it
# is true exactly for the files that can change what the channel-feature
# build compiles or which filter the channel-features job runs. A pull
# request that touches none of them skips the channel-feature test steps
# by design, so a false negative here would silently drop that coverage.
# That set is not only the named paths: a file whose content carries a
# `channel-lark` / `channel-matrix` cargo-feature gate belongs to it too,
# because `channel-matrix` is not a default feature and no other job
# compiles such a body. The flag also has to fail closed: with no
# resolvable base commit there is no diff to classify, so every scope that
# can skip a check stays "changed" and runs.
#
# The test builds a throwaway git repository so it does not depend on the
# caller's working tree. For each case it:
#
#   1. resets HEAD back to BASE_SHA (a clean tree);
#   2. makes a single file change and commits it;
#   3. runs the script with GITHUB_OUTPUT, EVENT_NAME and BASE_SHA set;
#   4. reads the rust_changed / docs_only / channel_features_changed values
#      out of the output file;
#   5. asserts them against the expected values.
#
# Cases cover both the newly added paths (must be Rust changes) and the
# paths the script must NOT treat as Rust (fuzz/ for the weekly scheduled
# job only, any *.md under these directories for the docs-only fast lane).
#
# Usage:
#   bash scripts/ci/tests/test_detect_change_scope.sh
#
# Exit code:
#   0 on PASS
#   1 on FAIL

set -uo pipefail

# Anchor on script location so the test works from any cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="${SCRIPT_DIR}/../detect_change_scope.sh"

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
    printf '%s\n' "script not found at $SCRIPT" >&2
    exit 1
fi

if ! command -v git >/dev/null 2>&1; then
    printf '%s\n' "git is required to test the change-scope script" >&2
    exit 1
fi

# --- fixture -----------------------------------------------------------------
# Build a throwaway repo with every directory the script's Rust-set and
# docs-set mention, so each case can touch exactly the file it cares about
# without the diff dragging other paths in.

REPO_DIR="$(mktemp -d)"
trap 'rm -rf "$REPO_DIR"' EXIT

(
    cd "$REPO_DIR"
    git init -q -b main
    git config user.email "test@example.com"
    git config user.name "Test"

    mkdir -p \
        src \
        src/channels \
        tests \
        scripts/ci \
        docs \
        fuzz/fuzz_targets \
        benches \
        examples \
        .github/workflows \
        .cargo

    # Tracked placeholders so the directories themselves are part of the
    # diff's empty tree at BASE_SHA. `git diff` of an untracked directory
    # would not surface anything.
    : > src/lib.rs
    : > src/channels/lark.rs
    : > src/channels/matrix.rs
    : > src/channels/mod.rs
    : > src/channels/telegram.rs
    # A file that carries a channel-feature gate but sits outside the
    # script's explicit path list, so the content rule is what puts it in
    # the channel-feature scope. It carries the gate in the initial commit
    # so the base-side lookup finds it too.
    cat > src/channels/factory.rs <<'GATE'
#[cfg(feature = "channel-matrix")]
mod matrix_channel {
}
GATE
    : > tests/lib.rs
    : > scripts/ci/empty.sh
    : > docs/x.md
    : > fuzz/fuzz_targets/x.rs
    : > benches/empty.rs
    : > benches/README.md
    : > examples/empty.rs
    : > Cargo.toml
    : > Cargo.lock
    : > .github/workflows/ci-run.yml
    : > .github/workflows/empty.yml
    : > .cargo/config.toml
    : > clippy.toml
    : > rustfmt.toml

    git add .
    git commit -q -m "initial base"
)

BASE_SHA="$(git -C "$REPO_DIR" rev-parse HEAD)"

# --- runner ------------------------------------------------------------------

# Reset HEAD to BASE_SHA, make exactly one file change, commit it, and run
# the script. Reads the requested key out of the captured GITHUB_OUTPUT
# file. The script also writes several other keys, so callers can ask for
# any of them.
#
# Optional third argument: the base SHA the script diffs against. It
# defaults to the fixture's base commit; an unresolvable value drives the
# script into its fail-closed branch.
# Optional fourth argument: content that replaces the file wholesale
# instead of the append the normal case makes.
run_case() {
    local file="$1"
    local key="$2"
    local base="${3:-$BASE_SHA}"
    local replace="${4:-}"

    (
        cd "$REPO_DIR"
        git reset --hard "$BASE_SHA" >/dev/null 2>&1
        mkdir -p "$(dirname "$file")"
        if [ -n "$replace" ]; then
            printf '%s\n' "$replace" > "$file"
        elif [ -f "$file" ]; then
            # Append when present (preserves the placeholder), create otherwise.
            printf '\ncase-edit\n' >> "$file"
        else
            printf 'case-edit\n' > "$file"
        fi
        git add "$file"
        git commit -q -m "modify $file"
    )

    local output="$REPO_DIR/github_output"
    : > "$output"

    (
        cd "$REPO_DIR"
        BASE_SHA="$base" \
        EVENT_NAME="pull_request" \
        GITHUB_OUTPUT="$output" \
        bash "$SCRIPT" >/dev/null
    )

    # The script writes `key=value` lines to GITHUB_OUTPUT. For the values
    # the script emits (`true`/`false`) there is no quoting to strip.
    local value
    value="$(grep "^${key}=" "$output" | head -1 | cut -d= -f2-)"
    printf '%s' "$value"
}

assert_rust_changed() {
    local file="$1"
    local expected="$2"
    local actual
    actual="$(run_case "$file" "rust_changed")"
    if [ "$actual" = "$expected" ]; then
        log_pass "$file → rust_changed=$expected"
    else
        log_fail "$file → rust_changed=$expected (got '$actual')"
    fi
}

assert_docs_only() {
    local file="$1"
    local expected="$2"
    local actual
    actual="$(run_case "$file" "docs_only")"
    if [ "$actual" = "$expected" ]; then
        log_pass "$file → docs_only=$expected"
    else
        log_fail "$file → docs_only=$expected (got '$actual')"
    fi
}

assert_channel_features_changed() {
    local file="$1"
    local expected="$2"
    local actual
    actual="$(run_case "$file" "channel_features_changed")"
    if [ "$actual" = "$expected" ]; then
        log_pass "$file → channel_features_changed=$expected"
    else
        log_fail "$file → channel_features_changed=$expected (got '$actual')"
    fi
}

# The same flag, but the changed file's content is replaced instead of
# appended to. Used for a file that carried a feature gate at the base
# commit and lost it at HEAD.
assert_channel_features_changed_with_content() {
    local file="$1"
    local content="$2"
    local expected="$3"
    local actual
    actual="$(run_case "$file" "channel_features_changed" "" "$content")"
    if [ "$actual" = "$expected" ]; then
        log_pass "$file → channel_features_changed=$expected (content replaced)"
    else
        log_fail "$file → channel_features_changed=$expected (got '$actual', content replaced)"
    fi
}

# The script must fail closed when the base commit does not resolve: there
# is no diff to classify, so every scope that can skip a check has to stay
# "changed" and run.
assert_fail_closed_base() {
    local base="$1"
    local file="$2"
    local actual_channel actual_rust
    actual_channel="$(run_case "$file" "channel_features_changed" "$base")"
    actual_rust="$(run_case "$file" "rust_changed" "$base")"
    if [ "$actual_channel" = "true" ] && [ "$actual_rust" = "true" ]; then
        log_pass "unresolvable BASE_SHA → channel_features_changed=true, rust_changed=true"
    else
        log_fail "unresolvable BASE_SHA → channel_features_changed=true, rust_changed=true (got channel_features_changed='$actual_channel', rust_changed='$actual_rust')"
    fi
}

# --- cases -------------------------------------------------------------------

printf '\n=== newly added paths are Rust changes ===\n'
assert_rust_changed "benches/agent_benchmarks.rs" "true"
assert_rust_changed "examples/stream_test.rs" "true"
assert_rust_changed ".cargo/config.toml" "true"
assert_rust_changed "clippy.toml" "true"
assert_rust_changed "rustfmt.toml" "true"
assert_rust_changed ".github/workflows/ci-run.yml" "true"

printf '\n=== existing Rust paths still match ===\n'
assert_rust_changed "src/lib.rs" "true"
assert_rust_changed "tests/lib.rs" "true"
assert_rust_changed "scripts/ci/empty.sh" "true"
assert_rust_changed "Cargo.toml" "true"
assert_rust_changed "Cargo.lock" "true"
assert_rust_changed "deny.toml" "true"
assert_rust_changed "build.rs" "true"

printf '\n=== scheduled/fuzz-only paths stay non-Rust ===\n'
assert_rust_changed "fuzz/fuzz_targets/x.rs" "false"

printf '\n=== workflow files other than ci-run.yml stay non-Rust ===\n'
# Pinning case for the .github/workflows/ clause: only ci-run.yml (which
# selects what the Rust pipeline builds, lints and checks) should route to
# the Rust gate; everything else is workflow-only and must not pull in the
# full Rust pipeline.
assert_rust_changed ".github/workflows/pr-labeler.yml" "false"

printf '\n=== docs-only fast lane ===\n'
assert_rust_changed "docs/x.md" "false"
assert_docs_only "docs/x.md" "true"
# A `.md` under one of the new Rust paths must NOT promote to rust_changed:
# the docs branch runs first, so a documentation edit there is still docs-only.
assert_rust_changed "benches/README.md" "false"
assert_docs_only "benches/README.md" "true"
assert_rust_changed "examples/README.md" "false"
assert_docs_only "examples/README.md" "true"

printf '\n=== channel-feature scope flag ===\n'
# The set the channel-features job depends on: the Lark and Matrix channel
# modules and their directories, the channel factory, the manifest and
# lockfile that decide the feature set, the workflow that selects the job
# and the CI scripts it calls. Everything else cannot change what that
# build compiles, so its test steps are skipped by design.
assert_channel_features_changed "src/channels/lark.rs" "true"
assert_channel_features_changed "src/channels/matrix.rs" "true"
assert_channel_features_changed "src/channels/mod.rs" "true"
assert_channel_features_changed "Cargo.toml" "true"
assert_channel_features_changed "Cargo.lock" "true"
assert_channel_features_changed ".github/workflows/ci-run.yml" "true"
assert_channel_features_changed "scripts/ci/empty.sh" "true"

printf '\n=== files outside the channel-feature scope ===\n'
assert_channel_features_changed "src/lib.rs" "false"
assert_channel_features_changed "src/channels/telegram.rs" "false"
assert_channel_features_changed "docs/x.md" "false"
assert_channel_features_changed ".github/workflows/pr-labeler.yml" "false"

printf '\n=== channel-feature scope: a file carrying a feature gate ===\n'
# `src/channels/factory.rs` is not one of the script's explicit paths, but
# it carries a `#[cfg(feature = "channel-matrix")]` gate, so the content
# rule puts it in the channel-feature scope. `channel-matrix` is not a
# default feature, so no other job compiles that body.
assert_channel_features_changed "src/channels/factory.rs" "true"

printf '\n=== channel-feature scope: a gate removed at HEAD ===\n'
# The file carried the gate at the base commit and lost it at HEAD. That
# still changes what the channel-feature build compiles, so the base-side
# lookup has to see it.
assert_channel_features_changed_with_content "src/channels/factory.rs" "no feature gate here" "true"

printf '\n=== fail closed on an unresolvable base commit ===\n'
# With no resolvable base commit there is no diff to classify, so every
# scope that can skip a check has to stay "changed" and run. `src/lib.rs`
# would be false under a resolvable base, so this case pins the fail-closed
# branch and not the ordinary classification.
assert_fail_closed_base "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" "src/lib.rs"

# --- summary -----------------------------------------------------------------

printf '\n=== summary ===\n'
printf 'Result: %d passed, %d failed\n' "$PASS" "$FAIL"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0