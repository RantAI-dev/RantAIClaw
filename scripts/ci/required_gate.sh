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
#   EVENT_NAME            github.event_name
#   RUST_CHANGED          "true" when the diff touches Rust
#   DOCS_ONLY             "true" when the diff is documentation only
#   DOCS_CHANGED          "true" when the diff touches documentation
#   IDENTITY_RESULT       result of each job, as GitHub reports it:
#   LINT_RESULT             success | failure | cancelled | skipped
#   TEST_RESULT
#   CHANNEL_LARK_RESULT
#   FEATURES_RESULT
#   E2E_RESULT
#   BENCH_COMPILE_RESULT
#   BUILD_RESULT
#   DOCS_RESULT
set -euo pipefail

# A job that was correctly skipped — label-gated behind `ci:full`, or a
# push-only job on a pull request — counts as passing. Jobs that are never
# skippable are checked with `= success` instead, so a skip there fails.
is_ok() {
  case "${1:-skipped}" in
    success | skipped) return 0 ;;
    *) return 1 ;;
  esac
}

run_gate() {
  local event_name="${EVENT_NAME:-}"
  local rust_changed="${RUST_CHANGED:-false}"
  local docs_only="${DOCS_ONLY:-false}"
  local docs_changed="${DOCS_CHANGED:-false}"

  local identity_result="${IDENTITY_RESULT:-skipped}"
  local lint_result="${LINT_RESULT:-skipped}"
  local test_result="${TEST_RESULT:-skipped}"
  local channel_lark_result="${CHANNEL_LARK_RESULT:-skipped}"
  local features_result="${FEATURES_RESULT:-skipped}"
  local e2e_result="${E2E_RESULT:-skipped}"
  local bench_compile_result="${BENCH_COMPILE_RESULT:-skipped}"
  local build_result="${BUILD_RESULT:-skipped}"
  local docs_result="${DOCS_RESULT:-skipped}"

  echo "event=${event_name} rust_changed=${rust_changed} docs_only=${docs_only} docs_changed=${docs_changed}"
  echo "identity=${identity_result}"
  echo "lint=${lint_result}"
  echo "test=${test_result}"
  echo "channel_lark=${channel_lark_result}"
  echo "features=${features_result}"
  echo "e2e=${e2e_result}"
  echo "bench_compile=${bench_compile_result}"
  echo "build=${build_result}"
  echo "docs=${docs_result}"

  # Checked before every fast path below: §9.1 applies to docs-only and
  # non-Rust changes too, so an early return must not skip it.
  if [ "$identity_result" != "success" ]; then
    echo "Identity-strings gate did not pass (CLAUDE.md 9.1)."
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

  if [ "$event_name" = "pull_request" ]; then
    if ! is_ok "$build_result"; then
      echo "Required PR build job did not pass."
      return 1
    fi
    # lint, test and channel-lark share one `if:` — Rust changed and lint
    # succeeded — so none of them is skippable on a Rust PR. A skip is a
    # failure here, not an opt-out, which is why they are not run through
    # `is_ok`.
    if [ "$lint_result" != "success" ]; then
      echo "Lint is required on Rust PRs."
      return 1
    fi
    if [ "$test_result" != "success" ]; then
      echo "Unit tests are required on Rust PRs."
      return 1
    fi
    if [ "$channel_lark_result" != "success" ]; then
      echo "channel-lark build/test is required on Rust PRs."
      return 1
    fi
    # features/bench-compile remain gated behind `ci:full`; a skip is fine.
    if ! is_ok "$features_result" || ! is_ok "$bench_compile_result"; then
      echo "Required PR jobs (features/bench-compile) did not pass."
      return 1
    fi
    echo "PR required checks passed."
    return 0
  fi

  if ! is_ok "$lint_result" || ! is_ok "$test_result" \
    || ! is_ok "$channel_lark_result" || ! is_ok "$features_result" \
    || ! is_ok "$e2e_result" || ! is_ok "$bench_compile_result" \
    || ! is_ok "$build_result"; then
    echo "Required push CI jobs did not pass."
    return 1
  fi

  echo "Push required checks passed."
  return 0
}

# --self-test drives the decision table. Each case names the shape it pins;
# the two marked "regression" are the gaps this file was extracted to close.
if [ "${1:-}" = "--self-test" ]; then
  failures=0

  expect() {
    local want="$1" label="$2"
    shift 2
    local got=0
    # Run in a subshell so each case gets a clean environment.
    (
      unset EVENT_NAME RUST_CHANGED DOCS_ONLY DOCS_CHANGED IDENTITY_RESULT \
        LINT_RESULT TEST_RESULT CHANNEL_LARK_RESULT FEATURES_RESULT \
        E2E_RESULT BENCH_COMPILE_RESULT BUILD_RESULT DOCS_RESULT
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
    EVENT_NAME=pull_request RUST_CHANGED=true IDENTITY_RESULT=success
    LINT_RESULT=success TEST_RESULT=success CHANNEL_LARK_RESULT=success
    BUILD_RESULT=success FEATURES_RESULT=skipped BENCH_COMPILE_RESULT=skipped
  )

  expect 0 "a green Rust PR merges" "${pr_rust_green[@]}"

  # regression: channel-lark ran on every Rust PR but was not in the gate's
  # `needs:`, so a failure there never blocked the merge.
  expect 1 "a failing channel-lark blocks a Rust PR" \
    "${pr_rust_green[@]}" CHANNEL_LARK_RESULT=failure
  expect 1 "a skipped channel-lark blocks a Rust PR" \
    "${pr_rust_green[@]}" CHANNEL_LARK_RESULT=skipped

  # regression: docs were only checked on pushes.
  expect 1 "failing docs lint blocks a Rust PR" \
    "${pr_rust_green[@]}" DOCS_CHANGED=true DOCS_RESULT=failure
  expect 1 "failing docs lint blocks a docs-only PR" \
    EVENT_NAME=pull_request DOCS_ONLY=true DOCS_CHANGED=true \
    IDENTITY_RESULT=success DOCS_RESULT=failure
  expect 0 "a docs-only PR whose lint passed merges" \
    EVENT_NAME=pull_request DOCS_ONLY=true DOCS_CHANGED=true \
    IDENTITY_RESULT=success DOCS_RESULT=success

  expect 1 "failing lint blocks a Rust PR" "${pr_rust_green[@]}" LINT_RESULT=failure
  expect 1 "failing tests block a Rust PR" "${pr_rust_green[@]}" TEST_RESULT=failure
  expect 1 "a failing build blocks a Rust PR" "${pr_rust_green[@]}" BUILD_RESULT=failure
  expect 1 "a failing identity gate blocks everything" \
    "${pr_rust_green[@]}" IDENTITY_RESULT=failure
  expect 1 "a failing feature matrix blocks a Rust PR" \
    "${pr_rust_green[@]}" FEATURES_RESULT=failure

  expect 0 "a non-Rust, non-docs PR merges" \
    EVENT_NAME=pull_request IDENTITY_RESULT=success

  expect 0 "a green push merges" \
    EVENT_NAME=push RUST_CHANGED=true IDENTITY_RESULT=success LINT_RESULT=success \
    TEST_RESULT=success CHANNEL_LARK_RESULT=success FEATURES_RESULT=success \
    E2E_RESULT=success BENCH_COMPILE_RESULT=success BUILD_RESULT=success
  expect 1 "a failing e2e blocks a push" \
    EVENT_NAME=push RUST_CHANGED=true IDENTITY_RESULT=success LINT_RESULT=success \
    TEST_RESULT=success CHANNEL_LARK_RESULT=success FEATURES_RESULT=success \
    E2E_RESULT=failure BENCH_COMPILE_RESULT=success BUILD_RESULT=success

  if [ "$failures" -gt 0 ]; then
    echo "required_gate self-test: $failures case(s) failed"
    exit 1
  fi
  echo "required_gate self-test: all cases passed"
  exit 0
fi

run_gate
