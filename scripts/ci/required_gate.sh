#!/usr/bin/env bash
#
# Usage: required_gate.sh [--self-test]
#
# The one status check `main`'s branch ruleset requires. Reads the outcome of
# every other job in `ci-run.yml` (passed in as environment variables) and
# decides whether this run may merge.
#
# Why this exists as a file
# -------------------------
# It used to be ~90 lines of shell inlined in `ci-run.yml`, which is why nobody
# noticed it was not reading two of the jobs it claimed to enforce:
#
#   * `channel-lark` ran on every Rust PR and was missing from the gate's
#     `needs:`, so its result was never read — the check went red on the PR and
#     the merge stayed green.
#   * `docs-quality` was only checked on pushes, so a pull request could break
#     the docs lint and merge anyway.
#
# Both are the same class of bug: a gate that runs a job it does not read is
# advisory while looking mandatory. Living in a file, with `--self-test` wired
# into the job itself, is what makes the next one of these fail loudly.
#
# Inputs (all optional; absent is treated as "skipped")
# ----------------------------------------------------
#   EVENT_NAME                 github.event_name
#   RUST_CHANGED               "true" when the diff touches Rust
#   DOCS_ONLY                  "true" when the diff is documentation only
#   DOCS_CHANGED               "true" when the diff touches documentation
#   CHANGES_RESULT             result of the job that computes the scope values
#                              listed here. They are empty when that job did
#                              not finish, so the gate requires `success`
#                              before it trusts any of them.
#   CHANNEL_FEATURES_CHANGED   "true" when the diff can change what the
#                              channel-feature build compiles
#   IDENTITY_RESULT            result of each job, as GitHub reports it:
#   FMT_RESULT                   success | failure | cancelled | skipped
#   LINT_RESULT
#   TEST_RESULT
#   MSRV_RESULT
#   CHANNEL_FEATURES_RESULT
#   FEATURES_RESULT
#   BENCH_COMPILE_RESULT
#   BUILD_RESULT
#   DOCS_RESULT
set -euo pipefail

# Every variable this gate reads, in one place. The self-test derives its
# `unset` list from it, so a variable that is read but not cleared between
# cases cannot happen again: `CHANNEL_MATRIX_RESULT` went missing from the
# old hand-written list, which is how a value leaked from one case into the
# next.
GATE_VARS=(
    EVENT_NAME
    RUST_CHANGED
    DOCS_ONLY
    DOCS_CHANGED
    CHANNEL_FEATURES_CHANGED
    CHANGES_RESULT
    IDENTITY_RESULT
    FMT_RESULT
    LINT_RESULT
    TEST_RESULT
    MSRV_RESULT
    CHANNEL_FEATURES_RESULT
    FEATURES_RESULT
    BENCH_COMPILE_RESULT
    BUILD_RESULT
    DOCS_RESULT
)

# Every Rust stage shares one condition — Rust changed and the format check
# passed — so on a run that reaches the checks below, a skip is a failure.
# There is no longer any stage this gate has to forgive for being skipped,
# which is why the old `is_ok` helper is gone: it existed to wave through
# `ci:full`-gated and push-only jobs, and neither exists any more.
require_success() {
  local label="$1" result="${2:-skipped}"
  if [ "$result" != "success" ]; then
    echo "$label did not pass (result: $result)."
    return 1
  fi
}

run_gate() {
  local event_name="${EVENT_NAME:-}"
  local rust_changed="${RUST_CHANGED:-false}"
  local docs_only="${DOCS_ONLY:-false}"
  local docs_changed="${DOCS_CHANGED:-false}"
  local channel_features_changed="${CHANNEL_FEATURES_CHANGED:-false}"

  local changes_result="${CHANGES_RESULT:-skipped}"
  local identity_result="${IDENTITY_RESULT:-skipped}"
  local fmt_result="${FMT_RESULT:-skipped}"
  local lint_result="${LINT_RESULT:-skipped}"
  local test_result="${TEST_RESULT:-skipped}"
  local msrv_result="${MSRV_RESULT:-skipped}"
  local channel_features_result="${CHANNEL_FEATURES_RESULT:-skipped}"
  local features_result="${FEATURES_RESULT:-skipped}"
  local bench_compile_result="${BENCH_COMPILE_RESULT:-skipped}"
  local build_result="${BUILD_RESULT:-skipped}"
  local docs_result="${DOCS_RESULT:-skipped}"

  echo "event=${event_name} rust_changed=${rust_changed} docs_only=${docs_only} docs_changed=${docs_changed}"
  echo "changes=${changes_result}"
  echo "identity=${identity_result}"
  echo "fmt=${fmt_result}"
  echo "lint=${lint_result}"
  echo "test=${test_result}"
  echo "msrv=${msrv_result}"
  echo "channel_features=${channel_features_result} channel_features_changed=${channel_features_changed}"
  echo "features=${features_result}"
  echo "bench_compile=${bench_compile_result}"
  echo "build=${build_result}"
  echo "docs=${docs_result}"

  # Checked before every fast path below: §9.1 applies to docs-only and
  # non-Rust changes too, so an early return must not skip it.
  if [ "$identity_result" != "success" ]; then
    echo "Identity-strings gate did not pass (CLAUDE.md 9.1)."
    return 1
  fi

  # Also before every fast path. rust_changed, docs_only and docs_changed are
  # outputs of the `changes` job and are empty when it fails, is cancelled or
  # is skipped. An empty rust_changed reads as "no Rust changed", so without
  # this check a broken scope job would skip every Rust job and the gate
  # would pass on the non-Rust fast path with nothing linted, tested or built.
  if [ "$changes_result" != "success" ]; then
    echo "Change-scope job did not pass; scope outputs cannot be trusted."
    return 1
  fi

  # Same reasoning for docs, and the reason this moved out of the per-branch
  # blocks: it used to be guarded by `event_name != pull_request`, so a pull
  # request could break the docs lint and still merge.
  if [ "$docs_changed" = "true" ] && [ "$docs_result" != "success" ]; then
    echo "Docs changed, but docs-quality did not pass."
    return 1
  fi

  if [ "$docs_only" = "true" ]; then
    echo "Docs-only fast path passed."
    return 0
  fi

  if [ "$rust_changed" != "true" ]; then
    echo "Non-rust fast path passed."
    return 0
  fi

  # Format first: it is the cheapest Rust stage and the one the heavy jobs
  # now wait for, so a formatting mistake is reported as such instead of
  # hiding behind "tests did not pass".
  require_success "Format" "$fmt_result" || return 1
  # Lint next: the heavy jobs do not wait for it (they wait for `fmt`), so a
  # lint failure leaves their results green or red on their own merits, and
  # reporting "tests did not pass" for it would send the reader to the wrong
  # log.
  require_success "Lint" "$lint_result" || return 1
  require_success "Unit tests" "$test_result" || return 1
  require_success "MSRV check" "$msrv_result" || return 1
  # The channel-features job skips its own test steps when the change scope
  # says the channel code did not change. The job still reports `success`
  # then, and that is the only shape this gate accepts for it: the scope
  # flag never loosens the requirement, so a `failure` or a `skipped`
  # result blocks either way. The flag is printed above so the log says
  # why the channel tests did or did not run.
  require_success "channel-features build/test" "$channel_features_result" || return 1
  require_success "Feature checks" "$features_result" || return 1
  require_success "Bench compile" "$bench_compile_result" || return 1
  require_success "Build smoke" "$build_result" || return 1

  if [ "$event_name" = "pull_request" ]; then
    echo "PR required checks passed."
  else
    echo "Push required checks passed."
  fi
  return 0
}

# --- workflow agreement -----------------------------------------------------
#
# The gate once ran jobs it never read, so the shape of the workflow is
# asserted here instead of trusted. Only bash, grep and awk: no YAML parser
# and no new dependency.
#
# The workflow path is resolved from this script's own location, so the check
# works from any cwd. A missing workflow file fails the check loudly instead
# of skipping it.

gate_script_path="${BASH_SOURCE[0]}"
case "$gate_script_path" in
    /*) ;;
    *) gate_script_path="$PWD/$gate_script_path" ;;
esac
GATE_SCRIPT_DIR="$(cd "$(dirname "$gate_script_path")" && pwd)"
GATE_REPO_ROOT="$(cd "$GATE_SCRIPT_DIR/../.." && pwd)"
WORKFLOW_PATH="$GATE_REPO_ROOT/.github/workflows/ci-run.yml"

# Job ids declared under `jobs:` in the workflow.
workflow_job_ids() {
    awk '
        /^jobs:/ { in_jobs = 1; next }
        in_jobs && /^[^[:space:]]/ { in_jobs = 0 }
        in_jobs && /^    [A-Za-z0-9_.-]+:/ {
            id = $0
            sub(/^    /, "", id)
            sub(/:.*/, "", id)
            print id
        }
    ' "$WORKFLOW_PATH"
}

# The body of one job: everything between its header and the next job.
workflow_job_block() {
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
    ' "$WORKFLOW_PATH"
}

# The job ids listed in the `ci-required` job's `needs:`.
gate_needs_ids() {
    workflow_job_block ci-required | awk '
        /^[[:space:]]+needs:/ { in_needs = 1 }
        in_needs {
            line = $0
            sub(/^[[:space:]]+needs:[[:space:]]*/, "", line)
            gsub(/[\[\],]/, " ", line)
            n = split(line, parts, /[[:space:]]+/)
            for (i = 1; i <= n; i++) if (parts[i] != "") print parts[i]
            if ($0 ~ /\]/) { in_needs = 0 }
            next
        }
    '
}

# `NAME=VALUE` pairs from the `ci-required` job's step-level `env:` block.
gate_env_bindings() {
    workflow_job_block ci-required | awk '
        function indent(s,   m) { m = match(s, /[^ ]/); return (m == 0 ? 0 : m - 1) }
        /^[[:space:]]+env:/ { in_env = 1; env_indent = indent($0); next }
        in_env {
            if ($0 ~ /^[[:space:]]*$/) { next }
            if (indent($0) <= env_indent) { in_env = 0; next }
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

# Job ids whose step commands run cargo or one of the Rust gate scripts.
workflow_jobs_running() {
    local pattern="$1"
    awk -v pattern="$pattern" '
        /^jobs:/ { in_jobs = 1; next }
        in_jobs && /^[^[:space:]]/ { in_jobs = 0 }
        in_jobs && /^    [A-Za-z0-9_.-]+:/ {
            id = $0
            sub(/^    /, "", id)
            sub(/:.*/, "", id)
            current = id
            next
        }
        in_jobs && current != "" {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            if (line ~ /^#/) { next }
            if (line ~ pattern) { print current; current = "" }
        }
    ' "$WORKFLOW_PATH" | sort -u
}

# Jobs listed in `ci-required`'s `needs:` whose result the gate deliberately
# does not read. Every other job in `needs:` must be bound to a *_RESULT
# variable (check 4b in check_workflow_agreement), so a job cannot become
# advisory by being listed there and then ignored.
#
#   docs-only, non-rust  Echo-only jobs. Each prints one line when the scope
#                        job picked its fast path. The decision they mirror
#                        is taken by the gate from the `changes` result and
#                        its outputs, and neither job does more than echo.
#   lint-feedback        Posts a pull-request comment that summarises lint
#                        and docs failures. It is a reporter, not a check:
#                        the jobs it reports on are read by the gate
#                        directly, and a failure to post a comment must not
#                        block a merge.
GATE_INFORMATIONAL_JOBS=(
    docs-only
    non-rust
    lint-feedback
)

# The workflow and the gate must agree. Each problem prints one line and
# counts as one failure.
check_workflow_agreement() {
    local failures=0
    local id var job value binding
    local needs_ids env_bindings read_ids cargo_jobs fmt_jobs

    if [ ! -f "$WORKFLOW_PATH" ]; then
        echo "FAIL: workflow not found at $WORKFLOW_PATH — the gate cannot be checked against it"
        return 1
    fi

    needs_ids="$(gate_needs_ids)"
    env_bindings="$(gate_env_bindings)"

    # 1. every job id the gate depends on exists in the workflow
    while read -r id; do
        [ -z "$id" ] && continue
        if ! workflow_job_ids | grep -qx -- "$id"; then
            echo "FAIL: ci-required needs '$id', which is not a job in ci-run.yml"
            failures=$((failures + 1))
        fi
    done <<< "$needs_ids"

    # 2. every job in the workflow is a dependency of the gate job, so the
    #    gate cannot miss a job that exists. The gate job itself is the
    #    only exception: it cannot depend on itself.
    while read -r id; do
        [ -z "$id" ] && continue
        [ "$id" = "ci-required" ] && continue
        if ! printf '%s\n' "$needs_ids" | grep -qx -- "$id"; then
            echo "FAIL: job '$id' exists in ci-run.yml but is not in ci-required's needs list"
            failures=$((failures + 1))
        fi
    done <<< "$(workflow_job_ids)"

    # 3. every variable the gate reads is bound in the gate job's env block,
    #    and every *_RESULT is bound to that job's own result
    for var in "${GATE_VARS[@]}"; do
        binding="$(printf '%s\n' "$env_bindings" | grep "^${var}=" | head -1 || true)"
        if [ -z "$binding" ]; then
            echo "FAIL: the gate reads $var, but ci-required does not bind it"
            failures=$((failures + 1))
            continue
        fi
        value="${binding#*=}"
        case "$var" in
        *_RESULT)
            if ! printf '%s' "$value" \
                | grep -qE '^\$\{\{[[:space:]]*needs\.[A-Za-z0-9_.-]+\.result[[:space:]]*\}\}$'; then
                echo "FAIL: $var is bound to '$value', expected \${{ needs.<job-id>.result }}"
                failures=$((failures + 1))
                continue
            fi
            job="$(printf '%s' "$value" \
                | sed -E 's/.*needs\.([A-Za-z0-9_.-]+)\.result.*/\1/')"
            if ! printf '%s\n' "$needs_ids" | grep -qx -- "$job"; then
                echo "FAIL: $var reads job '$job', which is not in ci-required's needs list"
                failures=$((failures + 1))
            fi
            ;;
        esac
    done

    # 4. every job that builds or lints Rust is read by the gate
    read_ids="$(printf '%s\n' "$env_bindings" | awk -F= '
        $2 ~ /needs[.][A-Za-z0-9_.-]+[.]result/ {
            line = $2
            sub(/^.*needs[.]/, "", line)
            sub(/[.]result.*/, "", line)
            print line
        }
    ')"
    # 3b. every variable bound in the gate job's env block must be one the
    #     gate reads (GATE_VARS). A variable bound there that the gate does
    #     not read is a value it looks at and ignores; a variable the gate
    #     reads that is missing from the list is not cleared between
    #     self-test cases, which is how a stale value leaks from one case
    #     into the next.
    while read -r var; do
        [ -z "$var" ] && continue
        if ! printf '%s\n' "${GATE_VARS[@]}" | grep -qx -- "$var"; then
            echo "FAIL: $var is bound in ci-required, but the gate does not read it"
            failures=$((failures + 1))
        fi
    done <<< "$(printf '%s\n' "$env_bindings" | cut -d= -f1)"
    cargo_jobs="$(workflow_jobs_running \
        '(cargo[[:space:]]|rust_quality_gate[.]sh|rust_strict_delta_gate[.]sh|scripts/ci/run_filtered_lib_tests[.]sh)')"
    while read -r id; do
        [ -z "$id" ] && continue
        if ! printf '%s\n' "$read_ids" | grep -qx -- "$id"; then
            echo "FAIL: job '$id' builds or lints Rust, but the gate does not read its result"
            failures=$((failures + 1))
        fi
    done <<< "$cargo_jobs"

    # 4b. every job in ci-required's needs list is either read by the gate or
    #    on the informational list above. `needs:` alone only orders the
    #    jobs: a job that is listed there and read nowhere looks mandatory
    #    and is not.
    while read -r id; do
        [ -z "$id" ] && continue
        if printf '%s\n' "$read_ids" | grep -qx -- "$id"; then
            continue
        fi
        if printf '%s\n' "${GATE_INFORMATIONAL_JOBS[@]}" | grep -qx -- "$id"; then
            continue
        fi
        echo "FAIL: ci-required needs '$id', but the gate neither reads its result nor lists it as informational"
        failures=$((failures + 1))
    done <<< "$needs_ids"

    # 5. the format check still exists somewhere in the workflow
    fmt_jobs="$(workflow_jobs_running 'cargo[[:space:]]+fmt')"
    if [ -z "$fmt_jobs" ]; then
        echo "FAIL: no job in ci-run.yml runs 'cargo fmt' — the format check is gone"
        failures=$((failures + 1))
    fi

    return "$failures"
}

# --self-test drives the decision table. Each case names the shape it pins;
# the two marked "regression" are the gaps this file was extracted to close.
if [ "${1:-}" = "--self-test" ]; then
  failures=0

  expect() {
    local want="$1" label="$2"
    shift 2
    local got=0
    # Run in a subshell so each case gets a clean environment. The list of
    # variables to clear comes from GATE_VARS — the single list of what
    # this gate reads — so a variable that is read but not cleared cannot
    # leak from one case into the next.
    (
      unset "${GATE_VARS[@]}"
      export "$@"
      run_gate >/dev/null 2>&1
    ) || got=$?
    if [ "$got" != "$want" ]; then
      echo "FAIL: $label — expected exit $want, got $got"
      failures=$((failures + 1))
    else
      echo "ok: $label"
    fi
  }

  pr_rust_green=(
    EVENT_NAME=pull_request RUST_CHANGED=true CHANGES_RESULT=success IDENTITY_RESULT=success
    FMT_RESULT=success LINT_RESULT=success TEST_RESULT=success MSRV_RESULT=success
    CHANNEL_FEATURES_RESULT=success CHANNEL_FEATURES_CHANGED=true
    FEATURES_RESULT=success BENCH_COMPILE_RESULT=success BUILD_RESULT=success
  )

  expect 0 "a green Rust PR merges" "${pr_rust_green[@]}"

  # Every stage this gate requires on a Rust run, with both arms: a failure
  # and a skip. No Rust stage is label-gated or push-only any more, so a
  # skip means something went wrong upstream — never an opt-out.
  expect 1 "a failing fmt blocks a Rust PR" "${pr_rust_green[@]}" FMT_RESULT=failure
  expect 1 "a skipped fmt blocks a Rust PR" "${pr_rust_green[@]}" FMT_RESULT=skipped
  expect 1 "a failing lint blocks a Rust PR" "${pr_rust_green[@]}" LINT_RESULT=failure
  expect 1 "a skipped lint blocks a Rust PR" "${pr_rust_green[@]}" LINT_RESULT=skipped
  expect 1 "a failing test blocks a Rust PR" "${pr_rust_green[@]}" TEST_RESULT=failure
  expect 1 "a skipped test blocks a Rust PR" "${pr_rust_green[@]}" TEST_RESULT=skipped
  expect 1 "a failing MSRV check blocks a Rust PR" "${pr_rust_green[@]}" MSRV_RESULT=failure
  expect 1 "a skipped MSRV check blocks a Rust PR" "${pr_rust_green[@]}" MSRV_RESULT=skipped
  expect 1 "a failing channel-features build/test blocks a Rust PR" \
    "${pr_rust_green[@]}" CHANNEL_FEATURES_RESULT=failure
  expect 1 "a skipped channel-features build/test blocks a Rust PR" \
    "${pr_rust_green[@]}" CHANNEL_FEATURES_RESULT=skipped
  expect 1 "a failing feature check blocks a Rust PR" "${pr_rust_green[@]}" FEATURES_RESULT=failure
  expect 1 "a skipped feature check blocks a Rust PR" "${pr_rust_green[@]}" FEATURES_RESULT=skipped
  expect 1 "a failing bench compile blocks a Rust PR" \
    "${pr_rust_green[@]}" BENCH_COMPILE_RESULT=failure
  expect 1 "a skipped bench compile blocks a Rust PR" \
    "${pr_rust_green[@]}" BENCH_COMPILE_RESULT=skipped
  expect 1 "a failing build blocks a Rust PR" "${pr_rust_green[@]}" BUILD_RESULT=failure
  expect 1 "a skipped build blocks a Rust PR" "${pr_rust_green[@]}" BUILD_RESULT=skipped

  # regression: channel-lark ran on every Rust PR but was not in the gate's
  # `needs:`, so a failure there never blocked the merge. The two channel
  # jobs are one job now, and both arms are pinned: a job that reports
  # nothing is how a required check silently stops guarding, and the Matrix
  # channel exists precisely because it was guarded by nothing for months.
  expect 0 "a Rust PR whose channel code did not change merges" \
    "${pr_rust_green[@]}" CHANNEL_FEATURES_CHANGED=false
  expect 1 "a failing channel-features blocks a Rust PR whose channel code did not change" \
    "${pr_rust_green[@]}" CHANNEL_FEATURES_CHANGED=false CHANNEL_FEATURES_RESULT=failure
  expect 1 "a skipped channel-features blocks a Rust PR whose channel code did not change" \
    "${pr_rust_green[@]}" CHANNEL_FEATURES_CHANGED=false CHANNEL_FEATURES_RESULT=skipped

  # regression: docs were only checked on pushes.
  expect 1 "failing docs lint blocks a Rust PR" \
    "${pr_rust_green[@]}" DOCS_CHANGED=true DOCS_RESULT=failure
  expect 1 "failing docs lint blocks a docs-only PR" \
    EVENT_NAME=pull_request DOCS_ONLY=true DOCS_CHANGED=true \
    CHANGES_RESULT=success IDENTITY_RESULT=success DOCS_RESULT=failure
  expect 0 "a docs-only PR whose lint passed merges" \
    EVENT_NAME=pull_request DOCS_ONLY=true DOCS_CHANGED=true \
    CHANGES_RESULT=success IDENTITY_RESULT=success DOCS_RESULT=success

  # The identity gate is a merge gate on its own terms (CLAUDE.md 9.1), so
  # it blocks every path, including the two fast paths.
  expect 1 "a failing identity gate blocks a Rust PR" \
    "${pr_rust_green[@]}" IDENTITY_RESULT=failure
  expect 1 "a skipped identity gate blocks a Rust PR" \
    "${pr_rust_green[@]}" IDENTITY_RESULT=skipped
  expect 1 "a failing identity gate blocks a docs-only PR" \
    EVENT_NAME=pull_request DOCS_ONLY=true CHANGES_RESULT=success IDENTITY_RESULT=failure
  expect 1 "a failing identity gate blocks a non-Rust PR" \
    EVENT_NAME=pull_request CHANGES_RESULT=success IDENTITY_RESULT=failure

  expect 0 "a non-Rust, non-docs PR merges" \
    EVENT_NAME=pull_request CHANGES_RESULT=success IDENTITY_RESULT=success

  expect 0 "a green push merges" \
    EVENT_NAME=push RUST_CHANGED=true CHANGES_RESULT=success IDENTITY_RESULT=success FMT_RESULT=success \
    LINT_RESULT=success TEST_RESULT=success MSRV_RESULT=success \
    CHANNEL_FEATURES_RESULT=success CHANNEL_FEATURES_CHANGED=true \
    FEATURES_RESULT=success BENCH_COMPILE_RESULT=success BUILD_RESULT=success
  expect 1 "a failing channel-features blocks a push" \
    EVENT_NAME=push RUST_CHANGED=true CHANGES_RESULT=success IDENTITY_RESULT=success FMT_RESULT=success \
    LINT_RESULT=success TEST_RESULT=success MSRV_RESULT=success \
    CHANNEL_FEATURES_RESULT=failure CHANNEL_FEATURES_CHANGED=true \
    FEATURES_RESULT=success BENCH_COMPILE_RESULT=success BUILD_RESULT=success

  # The scope outputs (RUST_CHANGED, DOCS_ONLY, DOCS_CHANGED,
  # CHANNEL_FEATURES_CHANGED) are empty when the `changes` job does not
  # finish, and an empty RUST_CHANGED reads as "no Rust changed". Without a
  # check on the job's own result, a broken scope job turned every Rust job
  # into a skip and the gate into a pass. Every arm below must block, on
  # the Rust path and on both fast paths.
  expect 1 "a failed change-scope job blocks a PR whose scope outputs are empty" \
    EVENT_NAME=pull_request CHANGES_RESULT=failure IDENTITY_RESULT=success
  expect 1 "a cancelled change-scope job blocks a PR whose scope outputs are empty" \
    EVENT_NAME=pull_request CHANGES_RESULT=cancelled IDENTITY_RESULT=success
  expect 1 "a skipped change-scope job blocks a PR whose scope outputs are empty" \
    EVENT_NAME=pull_request CHANGES_RESULT=skipped IDENTITY_RESULT=success
  expect 1 "an absent change-scope result blocks a PR whose scope outputs are empty" \
    EVENT_NAME=pull_request IDENTITY_RESULT=success
  expect 1 "a failed change-scope job blocks a docs-only PR" \
    EVENT_NAME=pull_request DOCS_ONLY=true DOCS_CHANGED=true CHANGES_RESULT=failure \
    IDENTITY_RESULT=success DOCS_RESULT=success
  expect 1 "a failed change-scope job blocks a Rust PR whose other jobs are green" \
    "${pr_rust_green[@]}" CHANGES_RESULT=failure
  expect 1 "a failed change-scope job blocks a green push" \
    EVENT_NAME=push RUST_CHANGED=true CHANGES_RESULT=failure IDENTITY_RESULT=success \
    FMT_RESULT=success LINT_RESULT=success TEST_RESULT=success MSRV_RESULT=success \
    CHANNEL_FEATURES_RESULT=success CHANNEL_FEATURES_CHANGED=true \
    FEATURES_RESULT=success BENCH_COMPILE_RESULT=success BUILD_RESULT=success
  expect 0 "a PR with a green change-scope job and no Rust change takes the non-Rust fast path" \
    EVENT_NAME=pull_request RUST_CHANGED=false CHANGES_RESULT=success IDENTITY_RESULT=success

  # The workflow and the gate must agree. A job the gate does not read is
  # advisory while looking mandatory, which is the bug this file was
  # extracted to fix; a format check that quietly disappeared is the same
  # bug wearing a different hat.
  structural_failures=0
  check_workflow_agreement || structural_failures=$?
  failures=$((failures + structural_failures))

  if [ "$failures" -gt 0 ]; then
    echo "required_gate self-test: $failures case(s) failed"
    exit 1
  fi
  echo "required_gate self-test: all cases passed"
  exit 0
fi

run_gate
