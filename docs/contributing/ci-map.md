# CI Workflow Map

This document explains what each GitHub workflow does, when it runs, and whether it should block merges.

For event-by-event delivery behavior across PR, merge, push, and release, see [`.github/workflows/main-branch-flow.md`](../../.github/workflows/main-branch-flow.md).

## Merge-Blocking vs Optional

Merge-blocking checks should stay small and deterministic. Optional checks are useful for automation and maintenance, but should not block normal development.

### Merge-Blocking

- `.github/workflows/ci-run.yml` (`CI`)
    - Purpose: single consolidated Rust quality gate with internal stages.
        - `fmt` — `cargo fmt --all -- --check` on its own, so a formatting mistake is reported in seconds instead of behind the whole lint job. It needs no cargo cache (rustfmt compiles nothing) and it is what the heavy jobs wait for.
        - `lint` — one clippy run whose JSON both gates read: `cargo clippy --locked --all-targets --keep-going --message-format=json -- -D clippy::correctness` (`scripts/ci/run_clippy_json.sh`). The run used to happen twice — once in the quality gate and once in the strict delta gate — and the second one re-checked every target for findings the first had already produced. `scripts/ci/rust_quality_gate.sh --skip-fmt` reads that JSON and fails on any error-level finding (a compile error, or a correctness lint that no crate attribute lowered to a warning); `scripts/ci/rust_strict_delta_gate.sh` reads the same JSON and fails on any clippy warning on a line the branch changed. The format check used to live here too; it moved to the `fmt` job, so the lint job calls the quality gate with `--skip-fmt`.
        - `test` — `cargo test --locked --workspace -- --test-threads=1`. Not `nextest`: it was tried and the runner SIGTERM'd or hit the job cap on this codebase. `--test-threads=1` because many unit tests mutate process-global env behind per-file locks that do not serialize across modules. The `tests/agent_e2e.rs` integration target is auto-discovered by the root package, so it runs here; the separate `e2e` job that re-ran the same tests is gone.
        - `msrv` — `cargo check --all-targets` at the toolchain `Cargo.toml`'s `rust-version` field declares, read out of the manifest rather than pinned in the workflow so the job cannot defend a number the manifest no longer says.
        - `features` — one job, three checks in sequence: `no-default-features`, `observability-otel`, `kb-office`. Each step runs under `if: !cancelled()`, so a failing combination does not hide the other two — the property the old `fail-fast: false` matrix gave.
        - `bench-compile` — `cargo bench --no-run --locked` (verifies criterion benches build).
        - `build` — `cargo build --profile release-fast --locked` smoke + binary-size guard (`scripts/ci/check_binary_size.sh`).
        - `channel-features` — one job for both channel features: `bash scripts/ci/run_filtered_lib_tests.sh test --locked --features channel-lark,channel-matrix --lib channels::lark`, the same wrapper call for `--lib channels::matrix`, and `cargo check --locked --features channel-lark,channel-matrix --all-targets`. **Pinned to rustc 1.96.0 — the only job in this repository that does not use the 1.92.0 pin every other job uses.** `matrix-sdk` 0.19 declares `rust-version = "1.96"`, so the feature builds only on 1.96 or newer. The crate's own MSRV for default features stays 1.91. The pin is confined to this job on purpose: moving the whole repo off 1.92.0 would touch every other workflow pin, the release build and the crate's declared MSRV, which is a product decision rather than a side effect of a channel fix. **Anyone raising or lowering a toolchain pin has to account for this job separately.** Matrix is not a default feature, so an unactivated optional dependency does not impose its `rust-version` on the default build. The wrapper refuses to run when the filter matches no test. `--all-targets` and not `--lib` alone: turning the feature on pulls `decancer`, whose `impl Add<Translation> for String` makes `String + &String` ambiguous, which broke two integration tests (`tests/kb/chunk_test.rs`) that no `--lib` run touches.
        - `channel-features` skip rule: the job's three test steps run only when the change scope says a channel-feature file changed (`scripts/ci/detect_change_scope.sh` output `channel_features_changed`) or on a push to `main`. The scope covers `src/channels/lark.rs` and `src/channels/lark/`, `src/channels/matrix.rs` and `src/channels/matrix/`, `src/channels/mod.rs` (the channel factory and the feature gates), `Cargo.toml`, `Cargo.lock`, `.github/workflows/ci-run.yml` and everything under `scripts/ci/`, plus any changed file whose content carries a `channel-lark` / `channel-matrix` cargo-feature gate, matched at both the diff's head and its base so a gate that was removed or a file that was deleted counts too. `channel-matrix` is not a default feature, so no other job compiles such a body, and a file that carries one must not be able to reach `main` unbuilt. The job itself is never skipped, only its steps, so it still reports `success` on a pull request that did not touch the channel code — the only shape the merge gate accepts for it.
        - `docs-quality` — incremental `markdownlint` on changed lines + offline `lychee` on links added on changed lines.
        - `lint-feedback` — posts actionable failure comment when lint/docs gates fail on a PR.
    - PR gating: every Rust stage — `fmt`, `lint`, `test`, `msrv`, `channel-features`, `features`, `bench-compile`, `build` — runs on every Rust-changing PR and is required by `CI Required Gate`. That list is the one `scripts/ci/required_gate.sh` actually reads; if it and this line ever disagree, the script is right. The heavy stages wait for `fmt`, not for `lint`, so a formatting failure no longer holds up the whole pipeline. `docs-quality` runs whenever docs change and is required. No stage is label-gated: `ci:full` no longer changes what runs.
    - Merge gate: `CI Required Gate` runs `scripts/ci/required_gate.sh`, which reads every stage result from the environment. The decision table lives in that script and is checked by `required_gate.sh --self-test`, which the same job runs first — a stage the gate stops reading fails the self-test instead of quietly becoming advisory. The self-test also compares the workflow with the gate (bash and awk only, no YAML parser): every job in `ci-run.yml` has to be in the gate's `needs:`, every result variable the gate reads has to be bound in the gate job's `env:` block, every job that builds or lints Rust has to be read by the gate, and some job has to run `cargo fmt`.
- `.github/workflows/workflow-sanity.yml` (`Workflow Sanity`)
    - Purpose: lint GitHub workflow files (`actionlint`, tab checks).
    - Checks: `Workflow Sanity (tabs)`, `Workflow Sanity (actionlint)`. Runs on every PR — see the required-check rule below.
- `.github/workflows/pr-intake-checks.yml` (`PR Intake Checks`)
    - Purpose: pre-CI PR checks (template completeness, added-line tabs/trailing-whitespace/conflict markers) with sticky feedback comment.
- `.github/workflows/pr-title-lint.yml` (`PR Title Lint`)
    - Purpose: enforce conventional-commit PR titles per `CLAUDE.md` §9 (type + optional scope + colon + summary).

### Non-Blocking but Important

- `.github/workflows/pub-docker-img.yml` (`Docker`)
    - Purpose: PR Docker smoke check on its build-input paths, and publish a multi-arch `latest`+`sha-<12>` image on every push to `main` (the `push` trigger is not path-scoped; only `pull_request` is). Manual dispatch triggers the same build + publish. There is no `v*` tag trigger on this workflow — the release Docker image for a tag is published by `pub-release.yml`'s `publish` job.
- `.github/workflows/sec-audit.yml` (`Security Audit`)
    - Purpose: dependency advisories (`rustsec/audit-check`, pinned SHA) and policy/license checks (`cargo deny`). Runs on PR + push to main + weekly Mon 06:00 UTC.
- `.github/workflows/sec-codeql.yml` (`CodeQL Analysis`)
    - Purpose: scheduled/manual static analysis for security findings.
- `.github/workflows/pub-release.yml` (`Release`)
    - Purpose: build release artifacts in verification mode (manual/scheduled) and publish GitHub releases on tag push or manual publish mode.
    - The `windows-io-tests` job runs the `migration::` and `memory::snapshot::` lib tests on `windows-latest`, the only place a test runs on Windows. `publish` needs it, so a red run blocks the release.
    - Permissions are least-privilege: only `publish` holds write scopes (`contents: write`, `packages: write`, `id-token: write`, `attestations: write`); every other job inherits `contents: read`, and `windows-io-tests` narrows it explicitly (so a regression inside `prepare`, `verify-update-cycle`, `build-release` or `verify-artifacts` cannot write to the repository, packages, or attestations).
    - The filtered lib-test steps in `verify-update-cycle` and `windows-io-tests` run `scripts/ci/run_filtered_lib_tests.sh`, which lists the selection first and refuses to run on an empty selection. A typo in a filter name fails the gate instead of passing silently.
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
- `Docker`: push to `main` (no path filter on the `push` trigger; only `pull_request` is path-scoped on Dockerfile build inputs), PRs to `main` when Dockerfile build inputs change, manual dispatch. No `v*` tag trigger.
- `Release`: tag push (`v*`), weekly schedule (verification-only), manual dispatch (verification or publish).
- `Security Audit`: push to `main` when Cargo paths change, PRs to `main` (no path filter), weekly schedule.
- `CodeQL`: weekly schedule, manual dispatch.
- `Workflow Sanity`: every PR (no path filter), push to `main` when `.github/workflows/**`, `.github/*.yml`, or `.github/*.yaml` change.
- `PR Intake Checks`: `pull_request_target` on opened/reopened/synchronize/edited/ready_for_review.
- `PR Title Lint`: `pull_request_target` on opened/reopened/edited/synchronize.
- `PR Labeler`: `pull_request_target` lifecycle events.
- `Fuzz`: weekly schedule, manual dispatch.
- `Dependabot`: daily dependency maintenance windows.

## Fast Triage Guide

1. `CI Required Gate` failing: the job prints every stage result it read before deciding; find the non-`success` one, then look at that stage's own logs (`fmt`, `lint`, `test`, `msrv`, `channel-features`, `features`, `bench-compile`, `build`, `docs-quality`). The gate also prints `channel_features_changed`, the scope flag that says whether the channel-feature tests ran or were skipped by design.
2. Docker failures on PRs: inspect `.github/workflows/pub-docker-img.yml` `pr-smoke` job.
3. Release failures (tag/manual/scheduled): inspect `.github/workflows/pub-release.yml` and the `prepare` job outputs. A red `windows-io-tests` job means a file I/O test failed on Windows; a test that asserts Unix-only behaviour belongs under `#[cfg(unix)]`, and any other failure is a bug in the code.
4. Security failures: inspect `.github/workflows/sec-audit.yml` and `deny.toml`.
5. Workflow syntax/lint failures: inspect `.github/workflows/workflow-sanity.yml`.
6. PR intake failures: inspect `.github/workflows/pr-intake-checks.yml` sticky comment and run logs.
7. PR title check failing: ensure title matches Conventional Commits (`feat|fix|chore|docs|refactor|perf|test|build|ci|style|revert`, optional scope, colon, summary).
8. Strict delta lint failures in CI: inspect `lint-strict-delta` step logs and compare with `BASE_SHA` diff scope.

## Maintenance Rules

- **A required status check must never be `paths:`-filtered.** On a run where the filter does not match, GitHub reports nothing at all, and a required check that never reports leaves the pull request pending forever. `workflow-sanity` and `sec-audit` are unfiltered on `pull_request` for exactly this reason; keep them that way, and do not add a `paths:` filter to any workflow named in the ruleset.
- Checks safe to require today, because each one reports on every pull request: `CI Required Gate`, `Intake Checks`, `Conventional Commit Title`, `Workflow Sanity (tabs)`, `Workflow Sanity (actionlint)`, `Security Audit`, `License & Supply Chain`. Check names are the contract — renaming a job breaks the ruleset silently, so rename first and update the ruleset in the same change.
- Branch protection should require only `CI Required Gate` for the Rust pipeline — internal stages can change without touching protection settings. That only holds while the gate actually reads every stage: adding a job to `ci-run.yml` means adding it to the gate's `needs:`, its `env:` block, and a `--self-test` case in `scripts/ci/required_gate.sh`. The gate's own self-test checks the first two mechanically, so a job that is added without them fails the gate job instead of turning advisory.
- A stage that is skipped by design needs an explicit, tested rule in `scripts/ci/required_gate.sh`, not a silent gap: today that is the `channel-features` job, whose test steps are skipped when `channel_features_changed` is false while the job still reports `success`. A `failure` or a `skipped` result blocks in every case.
- Keep merge-blocking checks deterministic and reproducible (`--locked` where applicable).
- Follow `docs/contributing/release-process.md` for verify-before-publish release cadence and tag discipline.
- Keep merge-blocking rust quality policy aligned across `.github/workflows/ci-run.yml`, `dev/ci.sh`, and `.githooks/pre-push` (`./scripts/ci/rust_quality_gate.sh` + `./scripts/ci/rust_strict_delta_gate.sh`). The local script stays the complete gate: it formats unless `--skip-fmt` is passed, and only the CI lint job passes that flag, because the workflow runs the format check in the `fmt` job.
- To reproduce the CI lint job locally, lint the crate once and hand the same JSON to both gates. `CLIPPY_JSON_FILE` and `CLIPPY_EXIT_CODE` describe that one run and have to be set together:

  ```bash
  out="$(mktemp)"
  code=0
  cargo clippy --locked --all-targets --keep-going --message-format=json -- -D clippy::correctness >"$out" || code=$?
  CLIPPY_JSON_FILE="$out" CLIPPY_EXIT_CODE="$code" ./scripts/ci/rust_quality_gate.sh --skip-fmt
  CLIPPY_JSON_FILE="$out" CLIPPY_EXIT_CODE="$code" ./scripts/ci/rust_strict_delta_gate.sh
  ```

  With `CLIPPY_JSON_FILE` unset, both gates run clippy themselves, so the plain `./scripts/ci/rust_quality_gate.sh` and `./scripts/ci/rust_strict_delta_gate.sh` calls keep working on their own.
- Use `./scripts/ci/rust_strict_delta_gate.sh` (or `./dev/ci.sh lint-delta`) as the incremental strict merge gate for changed Rust lines.
- Run full strict lint audits regularly via `./scripts/ci/rust_quality_gate.sh --strict` (for example through `./dev/ci.sh lint-strict`) and track cleanup in focused PRs.
- Keep docs markdown gating incremental via `./scripts/ci/docs_quality_gate.sh` (block changed-line issues, report baseline issues separately).
- Keep docs link gating incremental via `./scripts/ci/collect_changed_links.py` + lychee (check only links added on changed lines).
- Prefer explicit workflow permissions (least privilege).
- Keep Actions source policy restricted to approved allowlist patterns (see `docs/contributing/actions-source-policy.md`).
- Use path filters for expensive workflows when practical.
