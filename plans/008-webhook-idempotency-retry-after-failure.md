# Plan 008: Don't drop legitimate webhook retries — record idempotency key only after successful processing

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/gateway/mod.rs`
> If the file changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.
>
> **REVISED after cold review**: the key-state value MUST retain a timestamp
> (the store uses it for TTL expiry AND LRU eviction) — a bare `KeyState` breaks
> both. Two existing tests call `record_if_new`/read `store.keys` and must be
> migrated. `cargo build` does not compile tests, so use `cargo test --no-run`
> to catch breakage at the step that causes it. Corrected below.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

The webhook handler records the idempotency key **before** doing the work, then
returns `200 {"status":"duplicate"}` for any repeat. But standard webhook clients
retry with the SAME key after a 5xx/timeout. Because the key was already stored,
the retry is answered "duplicate" and the message is **never processed** — a lost
request precisely in the failure case idempotency exists to make safe. A key
should mean "done", not "seen".

## Current state (verified at 4d35107 — no drift)

- `src/gateway/mod.rs:1513-1529` — the idempotency gate, run BEFORE processing:
  ```rust
  if let Some(idempotency_key) = headers.get("X-Idempotency-Key")... {
      if !state.idempotency_store.record_if_new(idempotency_key) {   // :1520
          ... return (StatusCode::OK, Json(json!({"status":"duplicate", ...})));  // :1527
      }
  }
  ```
- `src/gateway/mod.rs:1564` — the work runs after: `run_gateway_chat_with_multimodal(...)`.
  `Ok` arm returns `(StatusCode::OK, Json(body))` at `:1597`; `Err` arm at `:1599`
  returns `(INTERNAL_SERVER_ERROR, Json(err))` at `:1633` — the key is already
  stored, so a retry is rejected.
- `src/gateway/mod.rs:181-219` — the store:
  ```rust
  struct IdempotencyStore { ttl: Duration, max_keys: usize, keys: Mutex<HashMap<String, Instant>> }
  ```
  `record_if_new` (private `fn`) runs `keys.retain(|_, seen| now.duration_since(*seen) < ttl)`
  (TTL expiry) on every call AND LRU-evicts the oldest `seen_at` when
  `len >= max_keys`. It uses the `Instant` value for BOTH. Constructed at `:576`
  from `config.gateway.idempotency_ttl_secs` + max_keys. `new(ttl, max_keys)`
  signature must be preserved (many test AppState builders call it).
- **Only caller of `record_if_new` in production**: the webhook handler (`:1520`).
  `api_v1.rs`/other sites only construct the store. **But two TESTS call it**:
  `mod.rs:2418 idempotency_store_rejects_duplicate_key` (calls at `:2420-2422`)
  and `mod.rs:2439 idempotency_store_bounded_cardinality_evicts_oldest_key`
  (calls at `:2441-2445`, and reads `store.keys.lock()` at `:2447` expecting
  `HashMap<String, Instant>`). These must be migrated when the API changes.
- `headers: HeaderMap` is an owned handler param (`:1453`) that lives across the
  `.await`, so a borrowed `Option<&str>` idempotency key is valid at both return
  points (`Ok` before `:1597`, `Err` before `:1633`).

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Compile incl. tests | `cargo test --no-run gateway` | compiles (catches test breakage) |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Gateway tests | `cargo test gateway` | all pass, incl. new |

Use `cargo test --no-run` (NOT `cargo build`) at each step — `cargo build` skips
`#[cfg(test)]` and hides the existing-test breakage until the very end.

## Scope

**In scope**:
- `src/gateway/mod.rs` — the `IdempotencyStore` type, the webhook handler
  ordering, and the two existing tests at `:2418` and `:2439`.

**Out of scope** (do NOT touch):
- Webhook signature verification (fail-closed HMAC).
- `run_gateway_chat_with_multimodal` internals; other gateway routes.

## Git workflow

- Branch: `advisor/008-webhook-idempotency-retry-after-failure`
- Commit per logical unit (store change + test migration, then handler wiring).
  Messages e.g. `fix(gateway): only mark webhook idempotency key done on success so retries re-run`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Make the store an in-progress/done state map that KEEPS the timestamp

Change `keys` from `HashMap<String, Instant>` to a map whose value retains the
`Instant` (TTL + LRU need it):
```rust
enum KeyState { InProgress, Done }
struct Entry { state: KeyState, seen_at: Instant }
struct IdempotencyStore { ttl: Duration, max_keys: usize, keys: Mutex<HashMap<String, Entry>> }
```
Replace `record_if_new` with three methods, all under the existing mutex (so they
stay atomic), and **apply the TTL `retain` (on `seen_at`) inside `begin`** so
stale `InProgress` AND `Done` entries expire:
```rust
enum BeginOutcome { Started, InProgress, Done }
fn begin(&self, key: &str) -> BeginOutcome {
    let mut m = self.keys.lock();
    let now = Instant::now();
    m.retain(|_, e| now.duration_since(e.seen_at) < self.ttl);   // expiry covers InProgress + Done
    match m.get(key).map(|e| &e.state) {
        Some(KeyState::Done) => BeginOutcome::Done,
        Some(KeyState::InProgress) => BeginOutcome::InProgress,
        None => {
            if m.len() >= self.max_keys { /* evict oldest by seen_at, as record_if_new did */ }
            m.insert(key.into(), Entry { state: KeyState::InProgress, seen_at: now });
            BeginOutcome::Started
        }
    }
}
fn mark_done(&self, key: &str) { /* set state = Done, refresh seen_at */ }
fn abort(&self, key: &str) { /* remove the key so a retry re-runs */ }
```
Preserve the exact LRU eviction (`min_by_key(seen_at)`) `record_if_new` used.
The InProgress-TTL is REQUIRED (not optional): if the handler future is dropped
mid-flight (client disconnect on timeout — the exact scenario this plan targets),
neither `mark_done` nor `abort` runs, so the `retain` in `begin` is what frees a
stranded `InProgress` after `ttl`.

**Verify**: `cargo test --no-run gateway 2>&1 | tail -20` → note it will FAIL to
compile because the two existing tests still call `record_if_new`; that is
expected and fixed in Step 1b.

### Step 1b: Migrate the two existing store tests

Rewrite `mod.rs:2418` and `mod.rs:2439` to the new API:
- `idempotency_store_rejects_duplicate_key`: `begin(k)` → `Started`; `mark_done(k)`;
  `begin(k)` → `Done`.
- `idempotency_store_bounded_cardinality_evicts_oldest_key`: drive `begin` past
  `max_keys` and assert oldest eviction; replace the `store.keys.lock()` read
  (`:2447`) with the equivalent against `HashMap<String, Entry>` (read `e.state`/
  `e.seen_at` or expose a test-only `len()`).

**Verify**: `cargo test --no-run gateway` → compiles.

### Step 2: Rewire the handler ordering

Replace the pre-processing block (`:1513-1529`):
```rust
use serde_json::json;   // or fully-qualify serde_json::json! as the file does
let idem_key = headers.get("X-Idempotency-Key").and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
if let Some(key) = &idem_key {
    match state.idempotency_store.begin(key) {
        BeginOutcome::Done => return (StatusCode::OK, Json(serde_json::json!({"status":"duplicate","idempotent":true}))),
        BeginOutcome::InProgress => return (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"status":"in_progress"}))).into_response_or_tuple(),  // see note
        BeginOutcome::Started => {}
    }
}
```
**Status for `InProgress`**: return **503 SERVICE_UNAVAILABLE with a `Retry-After`
header**, NOT 409. Many webhook senders (Stripe/GitHub-style) treat 4xx as
permanent and stop retrying — a 409 to a concurrent retry could permanently lose
the message. 503 + `Retry-After` tells the sender to retry after the in-flight
request resolves (then it sees `Done` or absent). If adding a header complicates
the `(StatusCode, Json)` return tuple, return a plain 503 tuple and document that
the sender's normal 5xx retry covers it. (Note the file uses fully-qualified
`serde_json::json!` — match that or add `use serde_json::json;`.)

Then in the processing `match` (`:1564`):
- `Ok(result)` arm: `if let Some(k) = &idem_key { state.idempotency_store.mark_done(k); }` before the `:1597` return.
- `Err(e)` arm: `if let Some(k) = &idem_key { state.idempotency_store.abort(k); }` before the `:1633` return.

**Verify**: `cargo test --no-run gateway` → compiles;
`grep -n "record_if_new" src/gateway/mod.rs` → no remaining production caller in
the handler (only the store's own internals / removed).

### Step 3: Confirm no record-then-process path remains

**Verify**: `grep -n "record_if_new" src/gateway/mod.rs` → the old
record-before-process call is gone from the webhook handler.

## Test plan

- Rewritten existing tests (Step 1b) + new store-state tests (inline
  `#[cfg(test)]` in `mod.rs`):
  1. `idempotency_retry_after_failure_reprocesses`: `begin(k)`→`Started`;
     `abort(k)`; `begin(k)`→`Started` (NOT `Done`).
  2. `idempotency_success_then_duplicate`: `begin(k)`→`Started`; `mark_done(k)`;
     `begin(k)`→`Done`.
  3. `idempotency_concurrent_inflight`: `begin(k)`→`Started`; second `begin(k)`→`InProgress`.
  4. `idempotency_inprogress_expires_after_ttl`: construct the store with a tiny
     ttl; `begin(k)`→`Started`; sleep past ttl (or inject an old `seen_at` via a
     test seam); `begin(k)`→`Started` again (the stranded InProgress expired).
- Model after the (now-migrated) existing tests.
- Verification: `cargo test gateway` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test gateway` passes; the retry-after-failure, success-then-duplicate,
      concurrent-inflight, and InProgress-TTL tests exist
- [ ] `grep -n "record_if_new" src/gateway/mod.rs` shows no record-before-process
      call in the webhook handler
- [ ] The two previously-existing tests (`:2418`, `:2439`) are migrated and pass
- [ ] `InProgress` returns 503 (+Retry-After if feasible), NOT 409
- [ ] Only `src/gateway/mod.rs` modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The idempotency store or handler ordering doesn't match the excerpts (drift).
- Another consumer of the store surfaces (`grep -rn "idempotency_store\|record_if_new" src/`)
  with different semantics — enumerate before changing the type.
- Adding a `Retry-After` header to the 503 requires restructuring the handler's
  return type broadly — fall back to a plain 503 and note it; don't refactor the
  whole handler.

## Maintenance notes

- The InProgress-TTL is the safety net for dropped/cancelled handler futures
  (client disconnect). A reviewer must confirm `begin`'s `retain` covers
  `InProgress` entries — without it a stranded key blocks the sender forever.
- LRU eviction can still drop a live `InProgress` key under `max_keys` pressure
  (pre-existing bounded-cardinality behavior); acknowledge this limit — the
  `InProgress` guarantee is best-effort under eviction, not absolute.
- Optional follow-up (deferred): cache the successful response body per key and
  replay it on a `Done` hit, so a retry after success returns the original result
  instead of a bare "duplicate".
