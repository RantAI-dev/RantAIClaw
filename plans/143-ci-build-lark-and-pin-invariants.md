# Plan 143: CI — build and test `channel-lark`; pin the probe-host and no-op-key invariants

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat f189422..HEAD -- .github/workflows/`
>
> **Line numbers WILL have drifted** if earlier plans merged first. Relocate by symbol
> name and continue. STOP only if the *code itself* no longer matches the "Current
> state" excerpt semantically.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none. **This plan runs FIRST in the execution order** — see `plans/README.md`.
- **Category**: dx
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

**2,942 lines of channel code are never compiled by any CI job.** `channel-lark`
(1,774 lines, 30 tests) and `channel-matrix` (1,168 lines, 31 tests) are opt-in
features, the only test invocation uses default features, and the features matrix runs
`cargo check` — not `test` — on three unrelated flag sets, skipped on PRs without a
label. Those 61 tests read as coverage in the tree and contribute zero signal.

Matrix has a known upstream blocker. **Lark has none** — its only extra dependency is
`prost`, a 12-crate closure with no problems. One line in the features matrix is the
highest leverage-per-hour item in this whole effort, and it is entirely unblocked.

It also matters *now* rather than later: plan 124 rewrites Lark's webhook
authentication, its sender identity and its mention gate. Without this job, nothing
verifies that work compiles, let alone passes.

The second half of this plan pins two invariants whose violations have already
shipped: a provisioner probing a domain the project does not own — a class that was
fixed once in v0.16.1-alpha and **recurred** — and config keys that no code reads.

## Current state

`.github/workflows/ci-run.yml:110` — the only test invocation:

```
cargo test --locked --workspace -- --test-threads=1
```

Default features, so neither gated channel is built.

`.github/workflows/ci-run.yml:126-137` — the features matrix contains
`no-default-features`, `hardware`, `browser-native`; it runs `cargo check`, and it is
skipped on PRs without the `ci:full` label. `:120-125` documents that `--all-features`
is deliberately omitted because of matrix-sdk.

`Cargo.toml:266-267`:

```toml
channel-matrix = ["dep:matrix-sdk"]
channel-lark = ["dep:prost"]
```

`src/channels/mod.rs:28-32` gates both modules on their features.

The probe-host class: `src/onboard/provision/channels/linq.rs:95` probes
`https://api.linq.com/v1/account` with a bearer token while the runtime uses
`https://api.linqapp.com/api/partner/v3` (`src/channels/linq.rs:23`). Plan 132 fixes
the instance and writes a table-driven host test; this plan runs it in CI.

The no-op-key class: `SlackConfig.app_token`, `LarkConfig.encrypt_key` and
`WebhookConfig.port` are prompted by the wizard, documented, redacted as secrets — and
read by no code. Plan 146 resolves the three; this plan adds the guard that stops a
fourth appearing.

CI currently forces `--test-threads=1` (`:103-109` documents why: cross-module env
mutation). Plan 141 removes the blockers on the channels side.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Lark build | `cargo clippy --features channel-lark --all-targets -- -D warnings` | exit 0 |
| Lark tests | `cargo test --features channel-lark --lib channels::lark` | all pass |
| Workflow syntax | your usual YAML lint, or `gh workflow view` after push | valid |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `.github/workflows/ci-run.yml` (and any sibling workflow the checks
belong in), plus the two guard tests if plans 132 and 146 have not already written
them.

**Out of scope**: the matrix-sdk decision — plan 145. Restoring `--test-threads`
parallelism — plan 141 removes the channel-side blockers, but flipping the flag needs
the whole suite verified and is its own change. Fixing whatever the new Lark job
surfaces — see step 2.

## Git workflow

- Branch: `chore/ci-lark-build-and-source-guards`
- Conventional commits, e.g. `ci: build and test channel-lark`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Build and test Lark locally first

Before touching CI, confirm it compiles here:

```
cargo clippy --features channel-lark --all-targets -- -D warnings
cargo test --features channel-lark --lib channels::lark
```

1,774 lines have been unlinted for months, so expect accumulated drift.

**Verify**: both commands exit 0, or you have a concrete list of what fails.

### Step 2: Decide what to do with what it surfaces

If step 1 is clean, skip to step 3.

If it fails: **do not fix unrelated code in this plan.** The failures are the point —
they are what the missing job would have caught. Report them, and either fix only what
is mechanical (an unused import, a renamed symbol) or open a follow-up and land this
plan with the job in place.

A CI job that is added and immediately disabled because it is red teaches the repo the
wrong lesson.

**Verify**: a written list of failures and their disposition in the PR.

### Step 3: Add the job

Add `channel-lark` to the features matrix, and **promote it from `cargo check` to
`cargo test`** — a compile check would not have caught the defects plan 124 is fixing.

Make it run on **every PR**, not only under the `ci:full` label. A job that only runs
on labelled PRs would not have run on any of this effort's Lark changes.

Update the workflow comment at `:120-125` so the Matrix exclusion is stated separately
from Lark's — right now one sentence covers both, and that conflation is why Lark was
never revisited.

**Verify**: read the workflow and confirm the trigger and the command.

### Step 4: Pin the probe-host invariant

**This plan runs before 132, so you write the test.** A table-driven test asserting
every provisioner's probe host equals its channel module's configured base host, run in
the default CI job.

It will **fail on Linq today** — `src/onboard/provision/channels/linq.rs:95` probes
`api.linq.com` while the runtime uses `api.linqapp.com`. That is the finding plan 132
fixes. Do not fix it here; mark the Linq row `#[ignore]` with a comment naming plan
132, so the test lands green and 132's PR is what un-ignores it. That ordering makes
132's fix provable rather than asserted.

**This is the item that closes a recurring class.** The Linq fix is one line; this test
is what stops a third occurrence.

**Verify**: the test runs in the default job and fails when a probe host is changed.

### Step 5: Pin the no-op-key invariant

Add a test asserting every leaf field of `ChannelsConfig` has at least one non-test,
non-schema, non-redaction reader.

A reflective check may not be practical in Rust. An acceptable alternative is a
maintained explicit list plus a grep-based script — but the list **must fail closed**:
a new field that nobody added to the list should fail the check, not pass it silently.
If you cannot make it fail closed, say so and ship a documented grep script instead of
a test that gives false assurance.

**Verify**: the check fails when you temporarily add an unread field.

### Step 6: Show each new check failing

For each of the three checks — the Lark job, the probe-host test, the no-op-key check —
demonstrate it failing on a deliberate violation, then revert. Record the output in the
PR.

## Test plan

The plan's deliverable is CI configuration, so:

1. The Lark job compiles and runs Lark's 30 tests.
2. The probe-host test fails when a provisioner's host is changed.
3. The no-op-key check fails when an unread field is added.
4. All three run on pull requests.

**Verify**: step 6's demonstrations, recorded in the PR.

## Done criteria

- [ ] `cargo clippy --features channel-lark --all-targets -- -D warnings` exits 0
- [ ] `cargo test --features channel-lark --lib channels::lark` passes
- [ ] The features matrix has a `channel-lark` entry running `cargo test`
- [ ] That job's trigger includes pull requests without requiring a label
- [ ] The probe-host test and the no-op-key check run in CI
- [ ] Step 6's three demonstrations are in the PR body
- [ ] The workflow comment states the Matrix exclusion separately from Lark
- [ ] `plans/README.md` status row for 143 updated

## STOP conditions

Stop and report back if:

- `channel-lark` does not compile and the failures are not mechanical. Report the list;
  fixing 1,774 lines of accumulated drift is its own plan.
- Adding a per-PR job pushes CI time past what the repo tolerates. Report the measured
  delta and let the maintainer decide between per-PR and labelled.
- The no-op-key check cannot be made to fail closed. Ship the script, document the
  limitation, and do not present it as a guarantee.
- Any of the three checks cannot be demonstrated failing in step 6. A check nobody has
  seen fail is not known to work.

## Maintenance notes

- **This plan runs first, deliberately.** 124 rewrites Lark's authentication in a file
  no CI job compiles; 132 fixes one instance of a probe-host class that has already
  recurred once. Running 143 first means both land with a net under them. Because it
  runs before 132, **step 4 writes the probe-host test here** rather than waiting.
- **What a reviewer should scrutinise**: that the Lark job runs `test`, not `check` — a
  compile check misses exactly the class 124 is fixing — and that it is not
  label-gated.
- **Deliberately deferred**: `channel-matrix`. It cannot compile at all until the
  matrix-sdk situation is resolved; plan 145 decides that. Do not add a job that is
  known-red.
