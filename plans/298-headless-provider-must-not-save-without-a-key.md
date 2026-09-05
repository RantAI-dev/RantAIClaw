# Plan 298: Headless provider setup must not save a config it knows is unusable

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat d5a1bba..HEAD -- src/main.rs src/onboard/provision/provider.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P2 (follow-up to W1-6 / plan 296)
- **Effort**: S
- **Risk**: LOW
- **Category**: bug / operator honesty
- **Planned at**: commit `d5a1bba`, 2026-09-05
- **Origin**: found during Wave 1 execution and deliberately left out of #707 as a design
  call rather than an execution detail. This plan makes that call.

## Why this matters

Plan 296 fixed the general headless contract: failures exit non-zero and only a `Configured`
outcome saves. The provider section slips through it. A provider can complete "successfully"
with no API key — the provisioner considers the selection done — so the run reports success,
writes the config, and exits zero.

The result is the failure mode this project already shipped once: an install that looks
finished, and an agent that cannot send a single message. An installer or CI job has no way
to tell. It is the same class as the release that was reported done while the tag was never
pushed — the outcome that gets reported is not the outcome that matters.

## Current state (verified at `d5a1bba`)

`src/main.rs` `run_provisioner_headless` now distinguishes outcomes and returns non-zero on
`Err`/`Aborted`/timeout (#707). What it does not do is judge whether a `Configured` provider
is actually usable.

`src/onboard/provision/provider.rs` treats a provider with no credential as a completed
selection: the operator may intend to supply the key by environment variable later.

Note the credential resolution subtlety the audit already recorded: `create_provider` accepts
env-supplied credentials (`OPENROUTER_API_KEY` and friends) that
`resolve_key_for_provider` does not see. "No key in config" therefore does **not** mean
"unusable" — which is exactly why this needs a decision rather than a naive guard.

## Steps

1. **Decide the rule and write it in the PR body first.** Recommended: in a headless run,
   a provider section that ends with no credential reachable **by any means the agent will
   actually use at send time** is an `Aborted`, not a `Configured`. Reuse
   `providers::has_usable_credential` (which exists for exactly this question and currently
   has one caller) rather than re-deriving the check.
   **Verify**: read `has_usable_credential` and confirm it covers env and OAuth paths.

2. **Apply it at the headless boundary only.** Interactive setup may legitimately let an
   operator configure a provider now and add the key later; do not change that path.
   **Verify**: the interactive flow's behaviour is untouched.

3. **Make the message actionable.** State which provider, that no credential was found in
   config or environment, and name the environment variable that would satisfy it.

4. **Tests.** (a) Headless provider setup with no key anywhere → non-zero exit, config
   unchanged. (b) Headless provider setup with the key supplied via environment → succeeds
   and saves. (b) is the one that stops this becoming a false positive.
   **Verify**: `cargo test --test setup_e2e` passes; test (a) fails if step 2 is reverted.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --test setup_e2e`, `cargo test --lib onboard` pass with both new tests.
- No headless run reports success for a provider the agent could not send with.

## STOP conditions

- `has_usable_credential` turns out not to cover a credential path the agent uses → STOP and
  report; fixing that function is the real task and affects `doctor` too.
- A bootstrap script depends on configuring a provider before the key exists → STOP;
  `rg -n 'setup provider' scripts/ .github/` before starting.

## Test plan

Two tests in `setup_e2e`, which already drives the real binary.

## Maintenance note

The rule this encodes: a setup step may only report success for a state the runtime can
actually use. `doctor` asks the same question and should give the same answer — if these two
ever disagree, one of them is wrong.

## Rollback

One commit. Behaviour-only, no schema change.
