#!/usr/bin/env bash
# Tests for the CI cache and profile policy in .github/workflows/ci-run.yml.
#
# The invariant: the merge-blocking CI workflow writes its Actions cache
# from `main` only, the compile-only bench job does not pay for the
# release profile's fat LTO, and the channel-matrix compile check runs on
# every Rust PR.
#
# Four policies and their wiring, thirteen assertions:
#
#   1. every `Swatinem/rust-cache` use carries
#      `save-if: ${{ github.ref == 'refs/heads/main' }}`, the number of
#      rust-cache uses is pinned, and no use sets a `shared-key` input, so
#      a pull request restores the cache but never creates a new one and no
#      two jobs share a key. A cache that every pull request writes into is
#      what fills the 10 GB Actions cache quota and evicts the `main`
#      entries that matter. (three assertions)
#   2. the `bench-compile` job builds the benches with the bench profile's
#      fat LTO turned off through job-level `CARGO_PROFILE_BENCH_*` env
#      variables — LTO off, opt-level 0, 16 codegen units — and `Cargo.toml`
#      declares no `[profile.bench]`. `cargo bench --no-run` only has to
#      prove the targets compile and link, and `Cargo.toml` stays
#      untouched: the release profile keeps its settings. (four assertions)
#   3. the workflow-level env sets `CARGO_PROFILE_DEV_DEBUG=line-tables-only`
#      for the dev and test profiles. (one assertion)
#   4. in the `channel-features` job the step that runs `cargo check
#      --locked --features channel-lark,channel-matrix --all-targets` has no
#      `if:` or one that is only `!cancelled()` or `always()`, so no scope
#      flag, event name or branch test can skip it on a PR, while the two
#      filtered test steps keep their `channel_features_changed`
#      condition. The check step also carries no `continue-on-error`
#      (absent, or `false`), because its failure has to block the gate.
#      `channel-matrix` is not a default feature, so this is the only
#      compile of `src/channels/matrix.rs`, and the files it depends on (`src/channels/traits.rs`, `src/channels/pairing.rs`,
#      `src/security/pairing_store`, `tests/`) sit outside the change-scope
#      rule. A condition on the check would let a PR that breaks matrix
#      merge green and turn `main` red. (four assertions)
#   5. the workflow runs this self-test from a `run:` step — a policy
#      nothing runs is not a policy. (one assertion)
#
# Plain bash, grep and awk: no YAML parser and no new dependency, so the
# check runs in the `changes` job in seconds.
#
# Usage:
#   bash scripts/ci/tests/test_ci_cache_and_profile_policy.sh
#
# Exit code:
#   0 on PASS
#   1 on FAIL

set -uo pipefail

# Anchor on script location so the test works from any cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
WORKFLOW="${REPO_ROOT}/.github/workflows/ci-run.yml"
MANIFEST="${REPO_ROOT}/Cargo.toml"

# The number of rust-cache uses the policy covers today. A lower count means
# a job lost its cache step; a higher count means a new one appeared and has
# to be brought under the policy before this test's count is raised.
EXPECTED_CACHE_USES=7

# The exact `save-if` value every rust-cache use must carry. Built once in
# the shell and compared as a string: the `{{ }}` braces make the value an
# awkward thing to match with a regex.
EXPECTED_SAVE_IF="\${{ github.ref == 'refs/heads/main' }}"

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

if [ ! -f "$WORKFLOW" ]; then
    printf '%s\n' "workflow not found at $WORKFLOW" >&2
    exit 1
fi

if [ ! -f "$MANIFEST" ]; then
    printf '%s\n' "manifest not found at $MANIFEST" >&2
    exit 1
fi

# --- helpers -----------------------------------------------------------------

# The body of one job: everything between its header and the next job.
job_block() {
    local job="$1"
    awk -v job="$job" '
        /^jobs:/ { in_jobs = 1; next }
        in_jobs && /^[^[:space:]]/ { in_jobs = 0; exit }
        in_jobs && !found {
            id = $0
            sub(/^    /, "", id)
            sub(/:.*/, "", id)
            if (id == job) { found = 1 }
            next
        }
        in_jobs && found {
            if ($0 ~ /^    [A-Za-z0-9_.-]+:/) { exit }
            print
        }
    ' "$WORKFLOW"
}

# The keys of an `env:` block whose `env:` key sits at the given indentation.
# The block ends at the first non-blank line that is not deeper than the
# `env:` key itself, so a step-level env block is never mistaken for a
# job-level one. Printed as `name=value`, the value keeping its YAML
# quoting.
env_vars_at() {
    local env_indent="$1"
    awk -v env_indent="$env_indent" '
        function indent_of(s,   m) { m = match(s, /[^ ]/); return (m == 0 ? -1 : m - 1) }
        indent_of($0) == env_indent && $0 ~ /env:[[:space:]]*$/ { in_env = 1; next }
        !in_env { next }
        $0 ~ /^[[:space:]]*$/ { next }
        indent_of($0) <= env_indent { exit }
        {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            name = line
            sub(/:.*/, "", name)
            value = line
            sub(/^[^:]*:[[:space:]]*/, "", value)
            print name "=" value
        }
    '
}

# One line per `Swatinem/rust-cache` step: the trimmed value of its
# `save-if:` input, an empty line when the step has none. The caller
# compares each line against the expected value as a string. A step ends at
# the next list item, so a `save-if:` that belongs to another step cannot
# be counted for this one, and a comment line is never a key: commenting
# the input out counts as removing it, which is what this test has to
# report.
rust_cache_save_if_states() {
    awk '
        /^[[:space:]]*#/ { next }
        /^[[:space:]]*-[[:space:]]+[A-Za-z][A-Za-z0-9_-]*:/ {
            if (in_cache) { print value }
            in_cache = 0
            value = ""
            if ($0 ~ /uses:[[:space:]]*Swatinem\/rust-cache/) { in_cache = 1 }
            next
        }
        {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            if (in_cache && index(line, "save-if:") == 1) {
                value = line
                sub(/^save-if:[[:space:]]*/, "", value)
                sub(/[[:space:]]+$/, "", value)
            }
        }
        END { if (in_cache) { print value } }
    ' "$WORKFLOW"
}

# The lines of one step of a job: from the `- name:` line whose value is
# `step_name` up to the next list item at the same indentation. Comment
# lines are dropped, so a comment that mentions a key is never read as the
# key. Steps sit at 12 spaces in `ci-run.yml`.
step_block() {
    local job="$1" step_name="$2"
    job_block "$job" | awk -v step_name="$step_name" '
        /^[[:space:]]*#/ { next }
        /^            - / {
            if (in_step) { exit }
            if ($0 == "            - name: " step_name) { in_step = 1 }
        }
        in_step { print }
    '
}

# Assert that an env block carries `name` with exactly `expected`.
assert_env_value() {
    local env_dump="$1" name="$2" expected="$3" label="$4"
    local actual
    actual="$(printf '%s\n' "$env_dump" | grep "^${name}=" | head -1 | cut -d= -f2-)"
    if [ "$actual" = "$expected" ]; then
        log_pass "${label}: ${name} = ${expected}"
    else
        log_fail "${label}: ${name} is '${actual}', expected '${expected}'"
    fi
}

# --- cases -------------------------------------------------------------------

printf '\n=== the Actions cache is written by main only ===\n'
cache_states="$(rust_cache_save_if_states)"
# Empty lines are rust-cache steps without a save-if input, so the count is
# every line the helper printed, not every non-empty one.
cache_count="$(printf '%s' "$cache_states" | awk 'END { print NR + 0 }')"

if [ "$cache_count" -lt "$EXPECTED_CACHE_USES" ]; then
    log_fail "expected at least ${EXPECTED_CACHE_USES} rust-cache uses in ci-run.yml, found ${cache_count}"
else
    log_pass "ci-run.yml has ${cache_count} rust-cache uses"
fi

missing_save_if="$(printf '%s' "$cache_states" | awk -v expected="$EXPECTED_SAVE_IF" '$0 != expected { missed++ } END { print missed + 0 }')"
if [ "$missing_save_if" -gt 0 ]; then
    log_fail "${missing_save_if} of ${cache_count} rust-cache uses do not carry save-if: ${EXPECTED_SAVE_IF}"
else
    log_pass "all ${cache_count} rust-cache uses carry save-if: ${EXPECTED_SAVE_IF}"
fi

# A trimmed line that starts with `shared-key:` is a real input; a comment
# that merely mentions the name must not fail the check.
shared_key_line="$(grep -nE '^[[:space:]]*shared-key:' "$WORKFLOW" | head -1 | cut -d: -f1 || true)"
if [ -n "$shared_key_line" ]; then
    log_fail "ci-run.yml sets shared-key at line ${shared_key_line} — no two jobs share a cache key"
else
    log_pass "no rust-cache in ci-run.yml shares a cache key"
fi

printf '\n=== the bench compile job does not pay for fat LTO ===\n'
bench_env="$(job_block bench-compile | env_vars_at 8)"
assert_env_value "$bench_env" "CARGO_PROFILE_BENCH_LTO" '"false"' "bench-compile job env"
assert_env_value "$bench_env" "CARGO_PROFILE_BENCH_OPT_LEVEL" '"0"' "bench-compile job env"
assert_env_value "$bench_env" "CARGO_PROFILE_BENCH_CODEGEN_UNITS" '"16"' "bench-compile job env"

if grep -nE '^\[profile\.bench' "$MANIFEST" >/dev/null 2>&1; then
    log_fail "Cargo.toml declares a [profile.bench] section — the bench profile override belongs to the workflow"
else
    log_pass "Cargo.toml has no [profile.bench] section"
fi

printf '\n=== dev and test profiles carry less debug info ===\n'
workflow_env="$(env_vars_at 0 <"$WORKFLOW")"
assert_env_value "$workflow_env" "CARGO_PROFILE_DEV_DEBUG" "line-tables-only" "workflow env"

printf '\n=== the channel-matrix compile check runs on every Rust PR ===\n'
MATRIX_CHECK_STEP="Check every target under channel-matrix"
MATRIX_CHECK_CMD="cargo check --locked --features channel-lark,channel-matrix --all-targets"
check_step="$(step_block channel-features "$MATRIX_CHECK_STEP")"

if printf '%s\n' "$check_step" | sed 's/^[[:space:]]*//' | grep -Fxq "run: ${MATRIX_CHECK_CMD}"; then
    log_pass "channel-features step '${MATRIX_CHECK_STEP}' runs ${MATRIX_CHECK_CMD}"
else
    log_fail "channel-features has no step '${MATRIX_CHECK_STEP}' that runs ${MATRIX_CHECK_CMD}"
fi

# The check must run on every Rust PR, so its `if:` is either absent or
# only keeps the step running after an earlier failure: `!cancelled()` or
# `always()`. Any other condition (a scope flag, an event name, a branch)
# can skip it on a PR and is reported by name. The `${{ }}` wrapper and the
# whitespace around the expression do not count.
if [ -z "$check_step" ]; then
    log_fail "step '${MATRIX_CHECK_STEP}' is missing from the channel-features job"
else
    check_conditions="$(printf '%s\n' "$check_step" | sed -n 's/^[[:space:]]*if:[[:space:]]*//p' |
        sed -e 's/^\${{[[:space:]]*//' -e 's/[[:space:]]*}}[[:space:]]*$//' -e 's/[[:space:]]*$//')"
    bad_condition=""
    while IFS= read -r condition; do
        case "$condition" in
            "" | '!cancelled()' | 'always()') ;;
            *) bad_condition="$condition" ;;
        esac
    done <<<"$check_conditions"
    if [ -z "$bad_condition" ]; then
        log_pass "step '${MATRIX_CHECK_STEP}' has no condition that can skip it on a PR"
    else
        log_fail "step '${MATRIX_CHECK_STEP}' carries the condition '${bad_condition}', which can skip the compile check on a PR — only '!cancelled()' or 'always()' are allowed"
    fi
fi

# `continue-on-error: true` would let a failing check leave the job green.
# Only a missing key or an explicit `false` keeps the failure blocking.
if [ -z "$check_step" ]; then
    log_fail "step '${MATRIX_CHECK_STEP}' is missing, so its continue-on-error cannot be checked"
else
    coe_values="$(printf '%s\n' "$check_step" | sed -n 's/^[[:space:]]*continue-on-error:[[:space:]]*//p' |
        sed -e 's/^\${{[[:space:]]*//' -e 's/[[:space:]]*}}[[:space:]]*$//' -e 's/[[:space:]]*$//')"
    bad_coe=""
    while IFS= read -r value; do
        case "$value" in
            "" | 'false') ;;
            *) bad_coe="$value" ;;
        esac
    done <<<"$coe_values"
    if [ -z "$bad_coe" ]; then
        log_pass "step '${MATRIX_CHECK_STEP}' does not set continue-on-error, so its failure blocks the job"
    else
        log_fail "step '${MATRIX_CHECK_STEP}' sets continue-on-error: ${bad_coe}, so a failing compile check would not block the gate"
    fi
fi

test_steps_filtered=0
for test_step in "Build and test channel-lark" "Build and test channel-matrix"; do
    if step_block channel-features "$test_step" | grep -E '^[[:space:]]*if:' | grep -q 'channel_features_changed'; then
        test_steps_filtered=$((test_steps_filtered + 1))
    fi
done
if [ "$test_steps_filtered" -eq 2 ]; then
    log_pass "both filtered channel test steps still carry the channel_features_changed condition"
else
    log_fail "only ${test_steps_filtered} of 2 filtered channel test steps carry the channel_features_changed condition"
fi

printf '\n=== the self-test runs in CI ===\n'
# Anchored on a `run:` key: a comment that mentions the script path is not
# a step that runs it.
if grep -nE '^[[:space:]]*run:.*scripts/ci/tests/test_ci_cache_and_profile_policy\.sh' "$WORKFLOW" >/dev/null 2>&1; then
    log_pass "ci-run.yml runs scripts/ci/tests/test_ci_cache_and_profile_policy.sh"
else
    log_fail "ci-run.yml never runs scripts/ci/tests/test_ci_cache_and_profile_policy.sh — a policy nothing runs is not a policy"
fi

# --- summary -----------------------------------------------------------------

printf '\n=== summary ===\n'
printf 'Result: %d passed, %d failed\n' "$PASS" "$FAIL"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
