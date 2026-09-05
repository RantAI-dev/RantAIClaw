# Plan 134: Provisioning — one shared IO layer and smoke tests that can fail

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/onboard/provision/`
>
> **Line numbers in this plan WILL have drifted** — plans 132 and 133 merge before it.
> That is expected and is not a stop condition. Relocate by symbol name and continue.
> STOP only if the *code itself* no longer matches the "Current state" excerpt
> semantically.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: plans/133 (serialized over the provisioners)
- **Category**: tests
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

The smoke tests for all fifteen channel provisioners **cannot fail**.

`test_responses_for` supplies an empty string as the first response for every
channel, and every channel's first prompt is a required field that early-returns. The
assertion helper accepts `Failed` as a pass. The test config is dropped without
asserting anything about `channels_config`. So not one line of credential collection,
probing, allowlist parsing or config writing is ever executed — which is precisely how
a bulk fifteen-module drop shipped with every defect plans 132 and 133 fix still
intact.

Underneath, ~600 of the subsystem's 4,149 lines are copy-paste: three IO helpers
defined identically in all fifteen modules, plus thirty tests that assert a string
constant equals itself. That duplication is why every structural defect in this
subsystem appeared uniformly in all fifteen places and had to be fixed fifteen times.

This plan makes the tests able to fail and removes the duplication that made the
previous two plans fifteen times larger than they needed to be.

## Current state

`src/onboard/provision/registry.rs:290-320` — every channel's first response is empty:

```rust
        // Text(String::new()) as the first response for all 15 channels
```

`src/onboard/provision/smoke.rs:111-123` — `assert_terminal_event` accepts `Failed` as
a pass. `smoke.rs:53` creates a local `Config::default()` that is dropped with no
assertion on `config.channels_config`.

Individual smoke modules exist for only 6 of 15 channels (`smoke.rs:308-416`), and
their comments describe the wrong prompts — `slack` says "signing secret" for what is
the app-level token; `matrix` lists "user_id / password" in the wrong order.

`send`, `recv_text` and `recv_selection` are defined identically in all fifteen
channel modules — `telegram.rs:191-221`, `slack.rs:213-243`, `email.rs:305-335`,
`qq.rs:197-227`, and eleven more. `recv_selection` is unused in seven of them. The
`use crate::onboard::provision::ProvisionerCategory;` import sits mid-file after the
impl block in all fifteen.

Each module carries the same two tests — `provisioner_name_is_x` and
`provisioner_description_is_non_empty` — thirty tests that assert a constant equals
itself, and which are the **only** per-module tests, so coverage tooling reports every
module as tested.

Zero integration tests reference any channel provisioner: `tests/setup_e2e.rs` and
`tests/setup_orchestration.rs` only assert the string `"channels"` appears in a
section list; `tests/tui_setup_overlay.rs` covers `whatsapp-web` state only;
`tests/provision_whatsapp_web.rs` is `#[ignore]`d with a stale reason — the feature it
says it needs is in the default set and the file is already cfg-gated.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Provision tests | `cargo test --lib onboard::provision` | all pass |
| Provisioning e2e | `cargo test --test provision_whatsapp_web` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/onboard/provision/**` — the shared IO layer, the registry's test
responses, the smoke harness, and the per-module tests.

**Out of scope**: `src/onboard/wizard.rs` (plans 132/133/142 own it); the production
behaviour of any provisioner — this plan changes structure and tests, not what a
provisioner does. If extracting the helpers changes behaviour anywhere, that is a bug
in the extraction.

## Git workflow

- Branch: `test/provisioning-shared-io-and-smoke`
- **Two commits minimum**: one for the extraction, one for the test harness. They are
  reviewed differently — the first must be behaviour-preserving, the second is new
  assertions.
- Conventional commits, e.g. `refactor(onboard): extract the shared provisioner IO helpers`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Record the baseline

Record the test count before you start and paste it into the PR:

```
cargo test --lib onboard::provision 2>&1 | tail -3
```

You will be deleting thirty tautological tests and adding real ones, so the count will
move — but you must be able to account for the delta exactly.

### Step 2: Extract the three IO helpers

Move `send`, `recv_text` and `recv_selection` into `src/onboard/provision/io.rs` (or
onto the existing `ProvisionIo` type as an extension trait) as the single definition.
Delete all fifteen copies. Drop `recv_selection` from the seven modules that never use
it rather than importing it unused.

Move the misplaced mid-file import to the top of each file while you are there — that
is the one cosmetic change worth bundling, because you are touching every file anyway.

**This step must be behaviour-preserving.** `git diff --stat` should show a large
deletion and a small addition.

**Verify**: `cargo test --lib onboard::provision` → all pass; `cargo clippy
--all-targets -- -D warnings` → exit 0.

### Step 3: Delete the thirty tautological tests

Remove `provisioner_name_is_x` and `provisioner_description_is_non_empty` from all
fifteen modules. Replace them with **one** table-driven test over
`registry::available()` asserting every registered provisioner has a non-empty name
and description and a unique key.

That single test does what thirty did, and unlike them it fails when a provisioner is
added without registration.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 4: Give the smoke harness real inputs

Replace `test_responses_for`'s empty first response with a genuine happy-path response
set per channel: placeholder credentials of the right **shape** (never a realistic
value — GitHub push protection has rejected this repo's fixtures for that before), a
selection for each `Choose`, an allowlist entry, and whatever else that channel's
prompt sequence needs.

Where a probe would fire, point it at a local mock server. Check first whether a mock
dependency is already available — if not, make the probe base URL injectable and
assert on the constructed request instead. Do not add a dependency for this; this
project treats dependency weight as a product goal.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 5: Split the assertion so an abort cannot pass as success

Replace `assert_terminal_event` with two helpers:

- `assert_configured(config, channel)` — the run reached `Configured` **and**
  `config.channels_config.<channel>` is `Some` with the expected fields
- `assert_aborted(config, channel)` — the run reported an abort **and** the config
  section is still `None`

Then give every one of the fifteen channels both cases: a happy path and an
abort path. Correct the six existing smoke modules' comments, which name the wrong
prompts.

Plan 133 introduced the typed outcome these assert on; if its shape differs from what
this plan assumes, follow 133 — it is merged.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 6: Resolve the ignored provisioning test

`tests/provision_whatsapp_web.rs` is `#[ignore]`d for a reason that no longer holds,
and its body asserts nothing (`let _ = saw_qr;`). Either:

- un-ignore it and assert a concrete property — the stream terminates within its
  budget and emits either a QR or a timeout, never diverging; or
- delete it and record the gap in the PR.

A skipped, assertion-free test is worse than an acknowledged gap. Do not leave it as
it is.

**Verify**: `cargo test --test provision_whatsapp_web` → passes or is gone.

## Test plan

The plan *is* the test work. What must be true when it is done:

1. Every one of the fifteen channels has a happy-path smoke test that asserts its
   config section is written with the expected fields.
2. Every one of the fifteen has an abort test that asserts the section stays `None`.
3. One table-driven registry test replaces the thirty tautological ones.
4. The `whatsapp-web` provisioning test either asserts something or is gone.

**Mutation check (required).** This is the whole point of the plan — prove the new
harness can fail:

- Break one provisioner's config write (drop the assignment) and confirm its
  happy-path test **fails**.
- Make one provisioner return `Configured` on an abort and confirm its abort test
  **fails**.
- Restore both.

If either mutation passes, the harness is still decorative and the plan is not done.

**Verify**: `cargo test --lib onboard::provision` → all pass, with the test-count delta
accounted for in the PR.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib onboard::provision` passes; the count delta from step 1 is
      explained in the PR
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -rc 'async fn recv_text' src/onboard/provision/channels/ | grep -v ':0'`
      returns nothing — no per-module copies remain
- [ ] `grep -rn 'provisioner_name_is_' src/onboard/provision/` returns nothing
- [ ] All fifteen channels have both a happy-path and an abort test
- [ ] No fixture contains a realistic-looking credential
- [ ] `git log --oneline` shows the extraction and the harness as separate commits
- [ ] `plans/README.md` status row for 134 updated

## STOP conditions

Stop and report back if:

- Plan 133 has not merged — this is serialized over the same files, and step 5 asserts
  on the outcome type 133 introduces.
- The extraction in step 2 changes behaviour anywhere. It must not; if a module's copy
  had drifted from the others, that drift is a finding — report it rather than
  silently normalising it.
- A happy-path smoke test requires a real network call that cannot be mocked without a
  new dependency. Make the base URL injectable instead, or leave that channel with the
  abort test only and say which.
- Either mutation check still passes.

## Maintenance notes

- **What interacts with this**: plans 132 and 133 changed provisioner behaviour with
  **no covering tests** — this plan is what retroactively pins their work. If a smoke
  test you write here fails against merged code, you have found a regression in one of
  them; report it rather than adjusting the test.
- **What a reviewer should scrutinise**: that step 2's diff is a move, and that step 4
  did not weaken any assertion to make a channel pass. A channel that cannot be
  smoke-tested honestly should be reported, not accommodated.
- **Why this comes after 132/133 rather than before**: the harness asserts on the typed
  outcome 133 introduces. Building it first would have meant writing it twice.
