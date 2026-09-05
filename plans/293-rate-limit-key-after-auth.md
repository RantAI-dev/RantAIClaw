# Plan 293: Key the API rate limiter on an authenticated principal, and trust the right proxy hop

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/gateway/mod.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P1 (ledger W1-5, part a)
- **Effort**: S–M
- **Risk**: LOW
- **Category**: security
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

The rate-limit bucket is derived from whatever bearer token the request presented, and the
middleware runs **before** authentication. A caller who has no valid token gets a fresh
bucket for every random token they invent, so the limiter never restrains them — and it can
be made to churn the key map. When `require_pairing` is false, that limiter is the only guard
in front of `agent/chat`, which spends money per request.

Separately, when `trust_forwarded_headers` is enabled the **leftmost** `X-Forwarded-For`
entry is used. That entry is supplied by the client; the trustworthy one is the entry your own
proxy appended, at the right. The same value keys the `/pair` and `/login` lockouts, so a
6-digit pairing code becomes brute-forceable from behind a proxy.

## Current state (verified at `4b8f61e`)

```rust
// src/gateway/mod.rs:376
fn api_rate_limit_key(
// src/gateway/mod.rs:400
fn client_key_from_request(
```

`api_rate_limit_key` hashes the presented bearer and returns `tok:<hash>` before any
authentication has happened; otherwise it falls back to `client_key_from_request`, which
returns the first parsable `X-Forwarded-For` entry when forwarded headers are trusted.

**A test currently encodes the behaviour being changed**:
`src/gateway/mod.rs:3209` `api_rate_limit_key_prefers_the_bearer_token_over_the_peer_ip`.
Updating it is part of this plan, not a regression — say so in the PR body.

## Steps

1. **Key on the token only once it is known to be valid.** Either move the limiter behind
   authentication, or have it consult the same validation the auth layer uses. An
   unauthenticated request must fall back to the network identity.
   **Verify**: read how `check_auth` validates so the limiter and the gate agree on what
   "valid" means; do not re-implement the check.

2. **Take the rightmost `X-Forwarded-For` entry** when forwarded headers are trusted, or make
   the trusted hop count configurable. Leftmost is client-controlled.
   **Verify**: `src/gateway/mod.rs:400` no longer returns the first entry.

3. **Update the existing test and add the two that matter.** Update `:3209` to the new
   contract. Add: (a) a request with an invalid bearer is keyed on the network identity, not
   the token; (b) with forwarded headers trusted, a client-supplied leftmost entry does not
   change the key.
   **Verify**: `cargo test --lib gateway` passes; both new tests fail if reverted.

4. **Check the lockout paths.** `/pair` and `/login` use the same key derivation; confirm the
   change reaches them and that the lockout still works from a single real client.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib gateway` passes with the updated and new tests.
- An unauthenticated caller rotating tokens shares one bucket.

## STOP conditions

- Moving the limiter behind auth would let unauthenticated requests reach expensive work
  before being limited → STOP and report; the ordering then needs a design decision.
- The console's own behaviour changes (its token is valid, so it should be unaffected) →
  STOP and re-check step 1.

## Test plan

Three tests in the `gateway` module: updated existing one plus the two negatives.

## Maintenance note

The rule is: never derive a security-relevant key from unvalidated request data. Both defects
here are the same mistake in two shapes.

## Rollback

One commit, one file plus tests.
