# CI Workflow Map

This document explains what each GitHub workflow does, when it runs, and whether it should block merges.

For event-by-event delivery behavior across PR, merge, push, and release, see [`.github/workflows/main-branch-flow.md`](../../.github/workflows/main-branch-flow.md).

## Merge-Blocking vs Optional

Merge-blocking checks should stay small and deterministic. Optional checks are useful for automation and maintenance, but should not block normal development.

### Merge-Blocking

- `.github/workflows/ci-run.yml` (`CI`)
    - Purpose: single consolidated Rust quality gate with internal stages.
        - `lint` — `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D clippy::correctness`, plus strict delta clippy on changed Rust lines (`scripts/ci/rust_strict_delta_gate.sh`).
        - `test` — `cargo nextest run --locked --workspace`.
        - `features` — matrix `cargo check`: `no-default-features`, `hardware-only`, `browser-native`.
        - `e2e` — `cargo test --test agent_e2e --locked` (push to `main` only; not on PRs).
        - `bench-compile` — `cargo bench --no-run --locked` (verifies criterion benches build).
        - `build` — `cargo build --profile release-fast --locked` smoke + binary-size guard (`scripts/ci/check_binary_size.sh`).
        - `channel-lark` — `cargo test --features channel-lark --lib channels::lark`.
        - `docs-quality` — incremental `markdownlint` on changed lines + offline `lychee` on links added on changed lines.
        - `lint-feedback` — posts actionable failure comment when lint/docs gates fail on a PR.
    - PR gating: every Rust stage — `lint`, `test`, `channel-lark`, `features`, `bench-compile`, `e2e`, `build` — runs on every Rust-changing PR and is required by `CI Required Gate`. `docs-quality` runs whenever docs change and is required. No stage is label-gated: `ci:full` no longer changes what runs, and `e2e` is no longer push-only.
    - Merge gate: `CI Required Gate` runs `scripts/ci/required_gate.sh`, which reads every stage result from the environment. The decision table lives in that script and is checked by `required_gate.sh --self-test`, which the same job runs first — a stage the gate stops reading fails the self-test instead of quietly becoming advisory.
- `.github/workflows/workflow-sanity.yml` (`Workflow Sanity`)
    - Purpose: lint GitHub workflow files (`actionlint`, tab checks).
    - Checks: `Workflow Sanity (tabs)`, `Workflow Sanity (actionlint)`. Runs on every PR — see the required-check rule below.
- `.github/workflows/pr-intake-checks.yml` (`PR Intake Checks`)
    - Purpose: pre-CI PR checks (template completeness, added-line tabs/trailing-whitespace/conflict markers) with sticky feedback comment.
- `.github/workflows/pr-title-lint.yml` (`PR Title Lint`)
    - Purpose: enforce conventional-commit PR titles per `CLAUDE.md` §9 (type + optional scope + colon + summary).

### Non-Blocking but Important

- `.github/workflows/pub-docker-img.yml` (`Docker`)
    - Purpose: PR Docker smoke check and publish images on `main` pushes (build-input paths), tag pushes (`v*`), and manual dispatch.
- `.github/workflows/sec-audit.yml` (`Security Audit`)
    - Purpose: dependency advisories (`rustsec/audit-check`, pinned SHA) and policy/license checks (`cargo deny`). Runs on PR + push to main + weekly Mon 06:00 UTC.
- `.github/workflows/sec-codeql.yml` (`CodeQL Analysis`)
    - Purpose: scheduled/manual static analysis for security findings.
- `.github/workflows/pub-release.yml` (`Release`)
    - Purpose: build release artifacts in verification mode (manual/scheduled) and publish GitHub releases on tag push or manual publish mode.
- `.github/workflows/test-fuzz.yml` (`Fuzz`)
    - Purpose: weekly `cargo fuzz` over `fuzz/fuzz_targets/`. Default 300s/target; opens an issue on crash.

### Optional Repository Automation

- `.github/workflows/pr-labeler.yml` (`PR Labeler`)
    - Purpose: size/risk/scope/module/`provider:*` labels, contributor-tier label by merged-PR count, auto-correct on manual label edits.
    - Manual governance: supports `workflow_dispatch` with `mode=audit|repair` to inspect/fix managed label metadata drift across the whole repository.
    - High-risk heuristic paths: `src/security/**`, `src/runtime/**`, `src/gateway/**`, `src/tools/**`, `.github/workflows/**`.
- `.github/dependabot.yml` (`Dependabot`)
    - Purpose: grouped, rate-limited dependency update PRs (Cargo + GitHub Actions + Docker).

## Trigger Map

- `CI`: push to `main`, PRs to `main`.
- `Docker`: push to `main` when Docker build inputs change, tag push (`v*`), matching PRs, manual dispatch.
- `Release`: tag push (`v*`), weekly schedule (verification-only), manual dispatch (verification or publish).
- `Security Audit`: push to `main`, PRs to `main`, weekly schedule.
- `CodeQL`: weekly schedule, manual dispatch.
- `Workflow Sanity`: PR/push when `.github/workflows/**`, `.github/*.yml`, or `.github/*.yaml` change.
- `PR Intake Checks`: `pull_request_target` on opened/reopened/synchronize/edited/ready_for_review.
- `PR Title Lint`: `pull_request_target` on opened/reopened/edited/synchronize.
- `PR Labeler`: `pull_request_target` lifecycle events.
- `Fuzz`: weekly schedule, manual dispatch.
- `Dependabot`: daily dependency maintenance windows.

## Fast Triage Guide

1. `CI Required Gate` failing: the job prints every stage result it read before deciding; find the non-`success` one, then look at that stage's own logs (`lint`, `test`, `channel-lark`, `features`, `e2e`, `bench-compile`, `build`, `docs-quality`).
2. Docker failures on PRs: inspect `.github/workflows/pub-docker-img.yml` `pr-smoke` job.
3. Release failures (tag/manual/scheduled): inspect `.github/workflows/pub-release.yml` and the `prepare` job outputs.
4. Security failures: inspect `.github/workflows/sec-audit.yml` and `deny.toml`.
5. Workflow syntax/lint failures: inspect `.github/workflows/workflow-sanity.yml`.
6. PR intake failures: inspect `.github/workflows/pr-intake-checks.yml` sticky comment and run logs.
7. PR title check failing: ensure title matches Conventional Commits (`feat|fix|chore|docs|refactor|perf|test|build|ci|style|revert`, optional scope, colon, summary).
8. Strict delta lint failures in CI: inspect `lint-strict-delta` step logs and compare with `BASE_SHA` diff scope.

## Maintenance Rules

- **A required status check must never be `paths:`-filtered.** On a run where the filter does not match, GitHub reports nothing at all, and a required check that never reports leaves the pull request pending forever. `workflow-sanity` and `sec-audit` are unfiltered on `pull_request` for exactly this reason; keep them that way, and do not add a `paths:` filter to any workflow named in the ruleset.
- Checks safe to require today, because each one reports on every pull request: `CI Required Gate`, `Intake Checks`, `Conventional Commit Title`, `Workflow Sanity (tabs)`, `Workflow Sanity (actionlint)`, `Security Audit`, `License & Supply Chain`. Check names are the contract — renaming a job breaks the ruleset silently, so rename first and update the ruleset in the same change.
- Branch protection should require only `CI Required Gate` for the Rust pipeline — internal stages can change without touching protection settings. That only holds while the gate actually reads every stage: adding a job to `ci-run.yml` means adding it to the gate's `needs:`, its `env:` block, and a `--self-test` case in `scripts/ci/required_gate.sh`.
- Keep merge-blocking checks deterministic and reproducible (`--locked` where applicable).
- Follow `docs/contributing/release-process.md` for verify-before-publish release cadence and tag discipline.
- Keep merge-blocking rust quality policy aligned across `.github/workflows/ci-run.yml`, `dev/ci.sh`, and `.githooks/pre-push` (`./scripts/ci/rust_quality_gate.sh` + `./scripts/ci/rust_strict_delta_gate.sh`).
- Use `./scripts/ci/rust_strict_delta_gate.sh` (or `./dev/ci.sh lint-delta`) as the incremental strict merge gate for changed Rust lines.
- Run full strict lint audits regularly via `./scripts/ci/rust_quality_gate.sh --strict` (for example through `./dev/ci.sh lint-strict`) and track cleanup in focused PRs.
- Keep docs markdown gating incremental via `./scripts/ci/docs_quality_gate.sh` (block changed-line issues, report baseline issues separately).
- Keep docs link gating incremental via `./scripts/ci/collect_changed_links.py` + lychee (check only links added on changed lines).
- Prefer explicit workflow permissions (least privilege).
- Keep Actions source policy restricted to approved allowlist patterns (see `docs/contributing/actions-source-policy.md`).
- Use path filters for expensive workflows when practical.
