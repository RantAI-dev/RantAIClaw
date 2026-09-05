# Plan 140: Handler-level authentication tests for every inbound webhook

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/gateway/mod.rs tests/whatsapp_webhook_security.rs`
>
> **Line numbers WILL have drifted** — plan 130 merges before this one. Relocate by
> symbol name and continue. STOP only if the *code itself* no longer matches the
> "Current state" excerpt semantically.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: plans/130 (this plan pins the production changes 130 makes)
- **Category**: tests
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

The two publicly-reachable webhooks carrying the most traffic — WhatsApp Cloud and
Linq — have their **only** authentication boundary asserted by tests that would
survive its removal.

`tests/whatsapp_webhook_security.rs` claims to validate that "webhooks with valid
signatures are accepted, invalid rejected, missing rejected". All eight of its tests
call the signature function directly; no HTTP handler is ever invoked. Delete the
fail-closed 401 and the signature check from the handler and every one of those tests
still passes while the endpoint becomes unauthenticated.

The correct pattern already exists in this repo. Nextcloud Talk has three
handler-level tests, including one that asserts the provider was **never called** when
no secret is configured. This plan copies that shape to the other two, and to Lark once
plan 124 gives it a check to test.

## Current state

`tests/whatsapp_webhook_security.rs:26`, `:37`, `:48`, `:59` — every test calls the
pure function:

```rust
    assert!(!rantaiclaw::gateway::verify_whatsapp_signature(
```

`grep -rn 'handle_whatsapp_message\|handle_linq_webhook' src/ tests/` returns **only**
the route registrations at `src/gateway/mod.rs:794-795` and the definitions. No test
invokes either.

`src/gateway/mod.rs:1948-1975` — the enforcement that is therefore unpinned: a
fail-closed 401 when no app secret is resolved, and the signature check.
`:2072-2109` — the same for Linq.

`src/gateway/mod.rs:3506` — the pattern to copy:

```rust
    async fn nextcloud_talk_webhook_rejects_when_no_secret_configured() {
```

and it asserts on a provider call counter, not just the status code — which is what
makes it a real test rather than a status assertion.

`src/gateway/mod.rs:3580-3743` adds 14 more pure-function signature tests, which are
fine as far as they go and are not what is missing.

Plan 130 adds a test seam if one is needed; check what it did before building your own.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Gateway tests | `cargo test --lib gateway::` | all pass |
| Webhook suite | `cargo test --test whatsapp_webhook_security` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `tests/whatsapp_webhook_security.rs`, and the gateway's in-module test
section for the handler-level tests.

**Out of scope**: production code in `src/gateway/mod.rs` — plan 130 owns it. If a
handler cannot be tested without a seam that does not exist, **report it**; do not add
the seam here, because that is a production change in a tests-only plan and it would
put two plans in the same file.

## Git workflow

- Branch: `test/webhook-handler-auth-tests`
- Conventional commits, e.g. `test(gateway): assert the webhook handlers reject unauthenticated requests`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Read what plan 130 left you

Plan 130 changed the verifiers to take raw bytes, added rate limiting and idempotency,
and may have moved processing off the ACK path. Read its merged diff before writing
tests, so you assert its actual behaviour rather than what this plan assumed.

If 130 introduced a router-building seam (the repo already has
`build_gateway_router` from an earlier effort), use it.

**Verify**: you can construct a router and issue a request to `/whatsapp` in a test.

### Step 2: Write the table-driven handler tests

For each of `/whatsapp`, `/linq` and `/nextcloud-talk`, assert:

1. **no secret configured** → 401, and the provider was **not** called
2. **missing signature header** → 401, provider not called
3. **wrong signature** → 401, provider not called
4. **valid signature** → 200, and the message was accepted

Assert on a provider call counter in every rejection case, not only the status. A
handler that returns 401 *after* processing would pass a status-only test.

Parameterise over the three endpoints rather than writing three near-identical files —
one table means a fourth endpoint is one row.

**Verify**: `cargo test --lib gateway::` → all pass.

### Step 3: Add the replay and rate-limit cases plan 130 introduced

1. a redelivered platform message id is suppressed and the provider is called once
2. a burst beyond the limit is rejected
3. for Nextcloud Talk, a replayed nonce is rejected

**Verify**: `cargo test --lib gateway::` → all pass.

### Step 4: Prove the suite can fail

For each endpoint, delete its fail-closed branch and its signature check in turn, and
confirm the corresponding test **fails**. Restore after each.

Record the result per endpoint in the PR. This is the assertion the old suite could
not make, and it is the reason this plan exists.

**Verify**: each deletion fails its test; the tree is restored.

### Step 5: Keep the pure-function tests, and say what they cover

Do not delete `tests/whatsapp_webhook_security.rs`'s eight tests — a signature
implementation is worth pinning. Update the file header so it no longer claims to
validate handler behaviour, and point at the new handler tests.

The claim, not the tests, was the problem.

**Verify**: `cargo test --test whatsapp_webhook_security` → all pass.

### Step 6: Cover Lark if it has a check yet

Plan 124 adds authenticity verification to the Lark webhook, which today has none. If
124 has merged, add the same four cases against its endpoint. If not, add the row to
the table with `#[ignore]` and a comment naming plan 124, so the gap is visible rather
than forgotten.

**Verify**: `cargo test --lib gateway::` → all pass.

## Test plan

The plan *is* the test work. What must hold:

1. Every inbound webhook endpoint has all four authentication cases, asserting on a
   provider call counter.
2. Step 4's mutation was performed per endpoint and each failed.
3. The pure-function suite is retained with an honest header.
4. Lark is either covered or visibly pending.

**Verify**: both scoped commands → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] Both scoped test commands pass
- [ ] Step 4's per-endpoint mutation results are recorded in the PR
- [ ] `grep -rn 'handle_whatsapp_message\|handle_linq_webhook' tests/ src/gateway/mod.rs`
      shows test invocations, not only route registrations
- [ ] Every rejection case asserts the provider was not called
- [ ] No production file is modified (`git status`)
- [ ] `plans/README.md` status row for 140 updated

## STOP conditions

Stop and report back if:

- Plan 130 has not merged — you would be pinning behaviour that is about to change.
- A handler cannot be invoked from a test without adding a production seam. Report it;
  adding one here would put this plan and 130 in the same file.
- A test fails against merged code for a reason 130 did not intend. That is a
  regression in 130 — report it rather than adjusting the test to match.
- Step 4's deletion does **not** fail a test you expected it to.

## Maintenance notes

- **What interacts with this**: plan 130 makes the production changes; plan 124 adds
  Lark's check. This plan is the net under both.
- **What a reviewer should scrutinise**: that every rejection asserts the provider
  counter. A status-only assertion is exactly the weakness that let the original suite
  pass while the boundary was removable.
- **Why a tests-only plan is P1**: three publicly-reachable endpoints currently have
  removable authentication. The production fixes land in 130; without this, nothing
  stops the next refactor removing them again.
