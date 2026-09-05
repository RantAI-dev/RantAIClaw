# Plan 104: Gate the KB routes on `enabled`, and fix the test harness that would hide it

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/axi/api.rs tests/kb/api_test.rs src/gateway/mod.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: 102, 103
- **Category**: feature
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

With `enabled` in config, the HTTP surface must honour it. Today all 12 KB
routes are mounted unconditionally whenever the `kb` feature is compiled —
`src/gateway/mod.rs:848-849`:

```rust
    #[cfg(feature = "kb")]
    let app = app.merge(crate::kb::axi::api::router());
```

and `kb` is a default feature (`Cargo.toml:246`). The only thing resembling a
gate is `build_ctx` failing when no key resolves, which reports
`kb_not_configured` (`api.rs:213-217`) — a different condition from "the
operator turned it off".

## Where the gate must live — this is the trap

**Not** at the merge site in `gateway/mod.rs`. The KB test harness builds its
own router and never goes through the gateway — `tests/kb/api_test.rs:216`:

```rust
    let app: Router = Router::new().merge(api::router()).with_state(state);
```

A gate at `gateway/mod.rs:848` would be invisible to every one of the 15+ KB
API tests. Put it in `api::router()` or in the handlers, where the harness
exercises it.

## The blast radius — plan for it up front

`build_state` (`api_test.rs:131-159`) constructs `Config::default()`. Once
`enabled` defaults to `false`, **every KB API test that expects 200 fails**,
including `kb_routes_require_auth_when_pairing_enabled` (`:446`), which is
testing something else entirely. The harness must set
`config.knowledge.enabled = true`. That is one line, but it must be in this
plan or the PR arrives red.

## Current state (verified at 2ca7e59)

- Router: `api.rs:86-121`
- `ensure_kb_ctx` reads keys from `state.config` — `api.rs:159-169`, so the
  handler already has config access
- Error helper: `ApiError::service_unavailable(code, detail)` — `api.rs:284-292`
- `kb_not_configured` / `kb_unavailable` are the existing codes

## Scope

**In scope**: the gate, its error code, and the harness fix.

**Out of scope**: the CLI (plan 107) — the CLI is local and its gate is a UX
question, not an exposure one.

## Git workflow

```bash
git switch -c feat/gate-kb-routes-on-enabled
```

## Steps

### Step 1: Gate inside `ensure_kb_ctx`

Every handler already calls it, so one check covers all 12 routes and cannot be
forgotten on a new route:

```rust
async fn ensure_kb_ctx(state: &crate::gateway::AppState) -> Result<Arc<KbContext>, ApiError> {
    // Deliberate: checked here rather than at the router merge site, because
    // the KB test harness builds its own router (tests/kb/api_test.rs) and a
    // gate in gateway/mod.rs would never be exercised.
    let (enabled, emb, vis) = {
        let c = state.config.lock();
        (c.knowledge.enabled, c.knowledge.embedding_api_key.clone(),
         c.knowledge.vision_api_key.clone())
    };
    if !enabled {
        return Err(ApiError::service_unavailable(
            "kb_disabled",
            "The Knowledge Base is turned off. Activate it in Configuration → \
             Knowledge Base, or run `rantaiclaw kb enable`.".into(),
        ));
    }
    // ... existing cache + build path unchanged
```

Keep `kb_not_configured` for the no-key case — the console needs to tell them
apart (plan 106 renders different screens).

**Verify**: `cargo build --features kb`.

> **Ordering note**: plan 105 also edits `ensure_kb_ctx`, replacing the
> path-only cache key with a credential fingerprint. Land this plan first — the
> gate belongs at the top of the function, before the cache lookup, so a
> disabled KB never serves a cached context.

### Step 2: Fix the harness

In `build_state` (`api_test.rs:131`):

```rust
fn build_state(require_pairing: bool, tokens: &[String]) -> AppState {
    let mut cfg = Config::default();
    // The KB is off by default (plan 102). These tests exercise the KB API
    // itself, so the harness activates it explicitly.
    cfg.knowledge.enabled = true;
    AppState { config: Arc::new(Mutex::new(cfg)), ... }
}
```

**Verify**: `cargo test --features kb --test kb api_test` — all previously
passing tests pass again. If any still fail, the gate is firing somewhere it
should not.

### Step 3: Test the gate in both directions

1. With `enabled = false`, `GET /api/v1/kb/documents` returns `503` with
   `error == "kb_disabled"`.
2. Control: with `enabled = true` and no key, the same route returns `503`
   with `error == "kb_not_configured"` — proving the two states are
   distinguishable and the gate did not swallow the existing behaviour.

The second test is the important one. A single-state test would pass even if
the gate replaced every error with `kb_disabled`.

### Step 4: Document the codes

`docs/reference/kb.md` — add `kb_disabled` beside `kb_not_configured` and say
what each means.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb api_test
cargo test --features kb
```

Manual:

```bash
curl -s localhost:9393/api/v1/kb/documents | python3 -m json.tool   # kb_disabled
curl -s -X PUT localhost:9393/api/v1/config/knowledge -H 'content-type: application/json' -d '{"enabled":true}'
curl -s localhost:9393/api/v1/kb/documents | python3 -m json.tool   # now works
```

## Done criteria

- All 12 routes report `kb_disabled` when off.
- `kb_not_configured` still distinguishable.
- The full KB API suite is green with the harness fix.

## STOP conditions

- More than the `build_state` line is needed to get the suite green — that
  means a test depends on the KB being reachable without config; report which.
- Someone has added a KB route that does not call `ensure_kb_ctx` — it would
  bypass the gate. Find it and route it through, or report.
