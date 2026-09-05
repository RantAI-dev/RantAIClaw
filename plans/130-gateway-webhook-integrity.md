# Plan 130: Gateway webhooks — verify what you parse, rate-limit, deduplicate

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/gateway/mod.rs src/channels/linq.rs src/channels/nextcloud_talk.rs`
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

Three publicly-reachable inbound webhooks — WhatsApp Cloud, Linq, Nextcloud Talk —
share two defects.

Two of them authenticate a **lossily-decoded copy** of the request while acting on
the raw bytes. `from_utf8_lossy` collapses every invalid sequence to `U+FFFD`, so the
string that was verified is a many-to-one projection of the body that was parsed.
Today the practical effect is mostly false rejections, which is fail-closed — but
"verify exactly what you parse" is broken and no test pins it, so the next change to
either side moves it in the unsafe direction silently. The WhatsApp handler already
does it correctly, in the same file, which makes the divergence easy to fix and easy
to miss.

All three lack rate limiting and idempotency, while the generic `/webhook` endpoint
in the same file has both. Each handler also runs the full LLM turn **before**
returning 200 — so a slow turn blows past Meta's ACK deadline, Meta retries, and the
same message is processed again. That is a duplicate reply and duplicate token spend
on the ordinary path, no attacker required. A captured signed POST is also replayable
indefinitely for WhatsApp and Nextcloud (no timestamp or nonce is enforced) and for
five minutes on Linq.

## Current state

`src/gateway/mod.rs:2066` and `:2113` — verified bytes ≠ parsed bytes:

```rust
    let body_str = String::from_utf8_lossy(&body);
```

```rust
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
```

The same pair appears at `:2218` and `:2263` for Nextcloud Talk.
`verify_linq_signature` (`src/channels/linq.rs:460`) and
`verify_nextcloud_talk_signature` (`src/channels/nextcloud_talk.rs:351`) both take
`body: &str`.

`src/gateway/mod.rs:1963` — WhatsApp, done right:

```rust
    if !verify_whatsapp_signature(app_secret, &body, signature) {
```

`:1931-2047`, `:2054-2199`, `:2206-2348` — the three handlers. None calls
`state.rate_limiter.allow_webhook(..)` or `state.idempotency_store.begin(..)`, and
each awaits `process_channel_chat(..)` before returning 200.

Contrast `:1662` and `:2361`, where `/webhook` and `/triggers/*` rate-limit, and
`:1731-1750`, where `/webhook` uses the idempotency store with a
`begin` / `mark_done` / `abort` protocol. Both stores already exist in `AppState`
(`:456-457`).

`verify_whatsapp_signature` (`:1906-1928`) takes no timestamp or nonce.
`verify_nextcloud_talk_signature` receives the `X-Nextcloud-Talk-Random` nonce and
never records it. `verify_linq_signature` enforces a 300-second window but no nonce.

`src/channels/nextcloud_talk.rs:356-361` has a missing-nonce guard that no test
covers — deleting it leaves all three signature tests green.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Gateway tests | `cargo test --lib gateway::` | all pass |
| Webhook security | `cargo test --test whatsapp_webhook_security` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**:
- `src/gateway/mod.rs` — the three handlers
- `src/channels/linq.rs` and `src/channels/nextcloud_talk.rs` — **only** the
  signature-verifier function signatures (`&str` → `&[u8]`) and their in-module
  verifier tests

**Out of scope**:
- Everything else in `linq.rs` and `nextcloud_talk.rs`. Plan 129 owns those files,
  including the Linq recipient-URL encoding and their allowlist storage. Touching
  more than the verifier signature here creates the exact conflict the plan
  partition exists to prevent — if a fix seems to need it, stop and report.
- Handler-level authentication **tests** — plan 140 owns those, and it depends on
  this plan. Write the production change here; 140 writes the test suite.
- The Lark webhook, which has no authenticity check at all — plan 124, in its own
  file.

## Git workflow

- Branch: `fix/gateway-webhook-integrity`
- Conventional commits, e.g. `fix(gateway): verify webhook signatures over the raw body`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Verify the bytes you parse

Change `verify_linq_signature` and `verify_nextcloud_talk_signature` to take
`body: &[u8]`, feed them `&body` directly, and delete both `from_utf8_lossy` calls.

Port the existing in-module verifier tests with `.as_bytes()`.

**Verify**: `cargo test --lib gateway::` and
`cargo test --lib channels::linq channels::nextcloud_talk` → all pass.

### Step 2: Rate-limit the three handlers

Add `client_key_from_request` + `allow_webhook` at the top of each handler,
following `src/gateway/mod.rs:1660-1670`. Return the same status shape `/webhook`
returns when limited.

**Verify**: `cargo test --lib gateway::` → all pass.

### Step 3: Suppress replays and redeliveries

Run each request's **platform message id** through the idempotency store after
signature verification, using the `begin` / `mark_done` / `abort` protocol at
`:1731-1826` as the model. Return 200 on `Done` without reprocessing.

Ids: WhatsApp `messages[].id`, Nextcloud Talk `random`, Linq `message.id`.

Note plan 129 separately fixes four channels that discard the platform id and mint a
UUID instead. Here you are reading the id from the **payload**, not from
`ChannelMessage`, so the two are independent — but say so in the PR so a reviewer
does not assume one blocks the other.

For Nextcloud Talk, also record the nonce so a captured request cannot be replayed;
for WhatsApp, note in the PR that no timestamp or nonce exists in the scheme, so
idempotency is the only replay control available.

**Verify**: `cargo test --lib gateway::` → all pass.

### Step 4: Acknowledge before processing

Move the LLM turn off the ACK path: return 200 once the request is verified and
accepted, then spawn the turn.

This is the riskiest step in the plan. An in-flight failure after the ACK must not be
silently dropped — carry the existing `abort` semantics through so a failed turn
releases its idempotency entry and can be retried. If you cannot preserve that,
implement steps 1–3 and stop; a duplicate reply is better than a silently lost one.

**Verify**: `cargo test --lib gateway::` → all pass.

## Test plan

Handler-level tests are plan 140's deliverable. This plan writes the **unit-level**
tests that pin its own production changes:

1. `linq_signature_verifies_over_raw_bytes` — a body containing a non-UTF-8 byte is
   handled identically by the verifier and the parser. Assert they agree; today they
   cannot.
2. `nextcloud_signature_verifies_over_raw_bytes` — same.
3. `nextcloud_rejects_a_missing_nonce` — the guard at
   `src/channels/nextcloud_talk.rs:356-361` that no test currently covers.
4. `nextcloud_rejects_a_replayed_nonce`.
5. `idempotency_suppresses_a_redelivered_message_id` — per handler.
6. `rate_limiter_rejects_a_burst` — per handler.

**Mutation check (required).** For test 3, delete the missing-nonce guard and confirm
it **fails**. For test 1, restore `from_utf8_lossy` and confirm it **fails**.
Restore both.

**Verify**: `cargo test --lib gateway::` and
`cargo test --test whatsapp_webhook_security` → all pass.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib gateway::` and `cargo test --test whatsapp_webhook_security` pass,
      including the six new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n 'from_utf8_lossy' src/gateway/mod.rs` returns nothing
- [ ] All three handlers call the rate limiter and the idempotency store
- [ ] The step-4 decision (moved off the ACK path, or deferred) is stated in the PR
- [ ] `git diff --stat` shows only the verifier signature and its tests changed in
      `linq.rs` / `nextcloud_talk.rs` — nothing else in those files
- [ ] `plans/README.md` status row for 130 updated

## STOP conditions

Stop and report back if:

- Fixing the verifier requires touching more of `linq.rs` or `nextcloud_talk.rs` than
  the function signature and its tests. Plan 129 owns those files.
- Moving the LLM turn off the ACK path (step 4) cannot preserve the abort-and-retry
  semantics. Ship steps 1–3 and report.
- The idempotency store's key space is shared with `/webhook` in a way that lets a
  channel message id collide with a generic webhook key. That would be a new defect;
  report it rather than namespacing it in passing.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 140 adds the handler-level authentication tests
  this plan's production changes need — until it lands, the fail-closed 401 branches
  remain unpinned. Plan 124 fixes the Lark webhook, which is a fourth inbound
  endpoint with no verification at all.
- **What a reviewer should scrutinise**: that step 1 changed the *verifier's* input
  type rather than converting the body twice, and that step 3's idempotency key is
  the platform id rather than anything derived from content (a content hash would
  suppress legitimate repeated messages).
- **Deliberately deferred**: adding a timestamp or nonce to the WhatsApp scheme —
  Meta's signature carries neither, so idempotency is the only control available on
  our side. Record that limitation in the PR so nobody later assumes replay is
  fully closed.
