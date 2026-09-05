# Plan 001: Make lint + unit tests required on every PR (close the green-PR/red-main gap)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- .github/workflows/ci-run.yml`
> If `ci-run.yml` changed since this plan was written, compare the "Current
> state" excerpts against the live file before proceeding; on a mismatch,
> treat it as a STOP condition.
>
> **This plan changes CI merge-gate policy — it is a judgment call the repo
> maintainer owns, not a pure bug fix.** Do NOT merge it without an explicit
> maintainer OK in the PR (the label-gating was deliberate to save runner
> minutes). Open the PR and request review; do not self-merge.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: MED
- **Depends on**: none (recommended to land first — it makes every other plan's tests actually run pre-merge)
- **Category**: dx
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

On a normal pull request, the `lint`, `test`, `features`, `bench-compile`, and
`e2e` jobs are all skipped (they require the `ci:full` label or a push to
`main`). The `CI Required Gate` treats a skipped job as success, so a PR can go
green having run **only** the smoke `build`. fmt/clippy/unit-tests/e2e first run
on push to `main` — after the merge — so `main` can break immediately after a
"green" merge. This silently negates the test suite as a merge gate. Every other
plan in this set relies on `cargo test`/`clippy` catching regressions before
merge; until this lands, they don't.

## Current state

- `.github/workflows/ci-run.yml` — the CI pipeline. Relevant excerpts (verified at `4d35107`):

  `lint` job (line 43-45):
  ```yaml
  lint:
      name: Lint Gate (Format + Clippy + Strict Delta)
      needs: [changes]
      if: needs.changes.outputs.rust_changed == 'true' && (github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, 'ci:full'))
  ```

  `test` job (line 65-67):
  ```yaml
  test:
      name: Test
      needs: [changes, lint]
      if: needs.changes.outputs.rust_changed == 'true' && (github.event_name != 'pull_request' || contains(github.event.pull_request.labels.*.name, 'ci:full')) && needs.lint.result == 'success'
  ```

  `e2e` job (line 155-157):
  ```yaml
  e2e:
      name: E2E
      needs: [changes, lint]
      if: needs.changes.outputs.rust_changed == 'true' && github.event_name != 'pull_request' && needs.lint.result == 'success'
  ```

  The required-gate helper (line 353-372):
  ```bash
  # Helper: a job that was correctly skipped (no `ci:full` label, or PR-only ...)
  is_ok() {
      case "$1" in
          success|skipped) return 0 ;;
          *) return 1 ;;
      esac
  }
  if [ "$event_name" = "pull_request" ]; then
    if ! is_ok "$build_result"; then ... fi
    # When `ci:full` is set, also require test/features/bench-compile.
    # When it isn't set, these were skipped -> is_ok returns success.
    if ! is_ok "$test_result" || ! is_ok "$features_result" || ! is_ok "$bench_compile_result"; then ... fi
    echo "PR required checks passed."
  ```

- Repo convention: this workflow drives a `changes` job that sets
  `rust_changed` / `docs_only` / `docs_changed` outputs; jobs gate on those so a
  docs-only PR skips Rust jobs. **Keep that behavior** — the goal is only to
  stop `lint`+`test` from being skippable on a *Rust-changing* PR.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Validate YAML | `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci-run.yml'))"` | exit 0, no output |
| Workflow sanity (if actionlint present) | `actionlint .github/workflows/ci-run.yml` | exit 0 (skip if not installed) |

There is no local runner for GitHub Actions logic; verification is YAML-validity
plus careful reading. Do not attempt to run the workflow locally.

## Scope

**In scope** (the only file you should modify):
- `.github/workflows/ci-run.yml`

**Out of scope** (do NOT touch):
- `features`, `bench-compile`, `e2e` job gating — leave these label/push-gated
  (runner cost). Only `lint` and `test` become required on Rust PRs.
- Branch-protection settings (GitHub UI/API) — the maintainer configures which
  checks are "required" in repo settings; call it out in the PR body but do not
  attempt it from code.
- Any other workflow file.

## Git workflow

- Branch: `advisor/001-ci-gate-pr-required-checks`
- One commit; conventional-commit message, e.g.
  `ci: run lint + unit tests on every rust PR (close green-PR/red-main gap)`.
- Do NOT push or open a PR unless the operator instructed it. If they did, open
  the PR and request maintainer review; do not self-merge.

## Steps

### Step 1: Make `lint` run on every Rust-changing PR

In the `lint` job `if:` (line 45), remove the PR escape so it runs whenever Rust
changed, regardless of `ci:full`:

```yaml
    if: needs.changes.outputs.rust_changed == 'true'
```

**Verify**: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci-run.yml'))"` → exit 0.

### Step 2: Make `test` run on every Rust-changing PR

In the `test` job `if:` (line 67), same change but keep the `lint` dependency:

```yaml
    if: needs.changes.outputs.rust_changed == 'true' && needs.lint.result == 'success'
```

**Verify**: YAML still parses (command above) → exit 0.

### Step 3: Make the required gate actually require lint + test on PRs

In the `is_ok()` PR branch (around line 362-372), `lint` and `test` must no
longer be allowed to pass by being skipped. The cleanest change that matches the
existing shell style: on a `pull_request` event where `rust_changed == 'true'`,
require `lint_result == success` and `test_result == success` explicitly (not
via `is_ok`, since `is_ok` still accepts `skipped`). Keep `features`/`bench`
under the existing `is_ok` treatment.

Target shape (adapt to the exact variable names already assigned above in the
step — read them; they are `lint_result`, `test_result`, `build_result`,
`features_result`, `bench_compile_result`, and there is a `rust_changed` value
available from the `changes` job outputs):

```bash
if [ "$event_name" = "pull_request" ]; then
  if ! is_ok "$build_result"; then echo "Smoke build failed."; exit 1; fi
  if [ "$rust_changed" = "true" ]; then
    if [ "$lint_result" != "success" ]; then echo "Lint is required on Rust PRs."; exit 1; fi
    if [ "$test_result" != "success" ]; then echo "Unit tests are required on Rust PRs."; exit 1; fi
  fi
  # features/bench remain optional unless ci:full opted them in
  if ! is_ok "$features_result" || ! is_ok "$bench_compile_result"; then
    echo "Optional job failed."; exit 1
  fi
  echo "PR required checks passed."
  ...
```

If `rust_changed` is not already captured into a shell variable in this step,
add it from `needs.changes.outputs.rust_changed` the same way the other
`*_result` values are captured (read how `docs_changed` is captured near line
312-318 and mirror it).

**Verify**: YAML parses → exit 0. Re-read the whole `ci-required` step and
confirm: (a) a docs-only PR (`rust_changed=false`) still passes, (b) a Rust PR
with failing lint or test now fails.

### Step 4: Update the CI documentation

Update `docs/contributing/ci-map.md` (if it documents which jobs are required on
PRs) to state that `lint` and `test` now run on every Rust-changing PR. If the
file does not mention the required-on-PR set, add one sentence. Keep it factual.

**Verify**: `grep -n "ci:full" docs/contributing/ci-map.md` — confirm any claim
that lint/test are label-gated is corrected.

## Test plan

- No Rust tests change. Verification is YAML validity + a written trace in the
  PR body walking three event scenarios through the gate: docs-only PR, Rust PR
  with green lint/test, Rust PR with red test (must fail).
- If `actionlint` is available in the environment, run it; otherwise note in the
  PR that it was not run.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci-run.yml'))"` exits 0
- [ ] `grep -n "ci:full" .github/workflows/ci-run.yml` shows `ci:full` no longer appears in the `lint` or `test` job `if:` (only in `features`/`bench-compile`/`docs-quality`)
- [ ] The `ci-required` PR branch fails when `lint_result` or `test_result` is not `success` on a `rust_changed` PR (verified by reading the shell)
- [ ] Only `.github/workflows/ci-run.yml` (+ optional `docs/contributing/ci-map.md`) modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The `ci-run.yml` job structure at the cited lines does not match the excerpts
  (workflow drifted since `4d35107`).
- Making `test` required on PRs would obviously blow the runner budget in a way
  you cannot assess — report the tradeoff to the maintainer instead of guessing.
- The `is_ok`/required-gate shell has been restructured such that the described
  edit no longer applies cleanly.

## Maintenance notes

- After this merges, the maintainer must also mark `Lint Gate` and `Test` as
  **required status checks** in GitHub branch protection for the gate to bind —
  the workflow `if:` alone does not enforce merge blocking. State this in the PR.
- If runner cost becomes a problem, the lever is the `test` job's matrix/feature
  scope, not re-adding the `ci:full` skip.
- Watch that the `changes` job's `rust_changed` detection stays correct — if it
  ever false-negatives, this gate would wrongly skip lint/test.
