# Plan 103: Config API carries `enabled`; validate the key live on activation

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/gateway/config_api.rs`
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
- **Depends on**: 102
- **Category**: feature
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

Two gaps on the same endpoint.

**No toggle.** `GET /config/knowledge` reports presence only
(`config_api.rs:845-864`) and `PUT` writes keys only (`:871-903`). With plan 102
adding `enabled`, both need to carry it or the field is unreachable.

**No validation.** `set_knowledge` accepts any string and persists it. A typo
is stored happily; the failure appears later as
`502 embedding_upstream — upstream embedding API returned status 401`
(`kb/axi/api.rs:349-355`) on every subsequent KB call, far from the action that
caused it.

The repo already has the right pattern next door: `connect_telegram`
(`config_api.rs:~600`) probes the token against Telegram's `getMe` and fails
closed, so a bad credential is never saved. KB should behave the same.

## Current state (verified at 2ca7e59)

- `KnowledgeBody { embedding_api_key, vision_api_key }` — `config_api.rs:821-826`
- `get_knowledge` returns `embedding_configured`, `vision_configured`, `source`
- `knowledge_source` reports `env` / `config` / `none` — `:830-841`
- Plan 086 removed the `schedule_daemon_reload()` call; `clear_kb_ctx()` stays

## Scope

**In scope**: `enabled` on both verbs, and a live key probe on activation.

**Out of scope**: route gating (104) and console UI (106).

## Git workflow

```bash
git switch -c feat/kb-config-api-enabled
```

## Steps

### Step 1: Carry `enabled` on GET

```rust
    Ok(Json(json!({
        "enabled": cfg.knowledge.enabled,
        "embedding_configured": emb_src != "none",
        "vision_configured": vis_src != "none",
        "source": emb_src,
    })))
```

### Step 2: Accept `enabled` on PUT

Add `enabled: Option<bool>` to `KnowledgeBody`. An omitted field leaves the
current value — same contract the key fields already have.

### Step 3: Probe the key before activating

Validate only when the request would leave the KB **enabled with a key** — so
deactivating, or clearing a key, never makes a network call.

Use the configured embedding endpoint and model so the probe tests the real
path:

```rust
/// Probe the configured embedding endpoint with a one-token input. Returns
/// `Err(message)` on a 4xx — a credential the provider rejects must not be
/// saved (mirrors the `getMe` probe in `connect_telegram`). Transport errors
/// are NOT fatal: an operator configuring the KB while offline should still be
/// able to store a key. Only an explicit auth rejection fails closed.
async fn probe_embedding_key(cfg: &KbConfig, key: &str) -> Result<(), String>
```

Map a rejection to `400` with a message naming the status, and never echo the
key or the upstream body.

**Verify**: a deliberately wrong key returns 400 and `config.toml` is unchanged.

### Step 4: Response and cache invalidation

Return `{enabled, embedding_configured, vision_configured}`. Keep the
`clear_kb_ctx()` call — a key or toggle change must drop the cached context.

### Step 5: Tests

In `tests/` (gateway config-api harness, see plan 013's `build_gateway_router`
seam if it has landed):

1. PUT `{"enabled": true}` with no key configured → 400, nothing persisted
2. PUT a key the stubbed endpoint rejects with 401 → 400, nothing persisted
3. PUT a key the stubbed endpoint accepts → 200, `enabled` true
4. PUT `{"enabled": false}` → 200, **key still present** (this is the whole
   point of the feature)
5. GET reflects `enabled` in both states

Test 4 is the one that proves deactivate is not delete.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb config_api
cargo test --features kb
```

Manual:

```bash
curl -s -X PUT localhost:9393/api/v1/config/knowledge \
  -H 'content-type: application/json' \
  -d '{"embedding_api_key":"sk-obviously-wrong","enabled":true}'
# expect 400; then confirm config.toml has no new key
curl -s localhost:9393/api/v1/config/knowledge
```

## Done criteria

- `enabled` readable and writable.
- A rejected key is never persisted.
- Deactivating keeps credentials.
- An offline operator can still store a key.

## STOP conditions

- Plan 102 has not landed — `cfg.knowledge.enabled` will not compile.
- The probe cannot distinguish "wrong key" from "endpoint unreachable": stop
  rather than failing closed on transport errors, which would make the console
  unusable behind a proxy.
- **Blocked on the open question**: whether the default embedding endpoint and
  model actually serve requests (see `plans/README.md` note for this batch).
  If the default is not viable, the probe rejects every key and the feature is
  unusable. Resolve that first.
