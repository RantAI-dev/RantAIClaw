# Plan 225: Stop rebuilding the world on every chat request — cache the session store, MCP discovery, and skills; make rate limiting fair behind the BFF

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 8503328..HEAD -- src/gateway/api_v1.rs src/gateway/mod.rs src/agent/agent.rs src/mcp/discover.rs src/config/schema.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED — introduces caching keyed on the config fingerprint and changes the rate-limit key; both have clear invalidation/fallback and their own tests. Each step is a separate commit.
- **Depends on**: none, but **overlaps plan 221 step 6** (both read `[gateway]` config at `build_app`) and **plan 221 step 5** (`session_exists`). Land 221 first if both are in flight; otherwise rebase carefully.
- **Category**: perf
- **Planned at**: commit `8503328`, 2026-08-24
- **Branch**: `perf/gateway-per-request-cost`
- **One PR**: commit per step.

## Why this matters

Every `POST /api/v1/agent/chat` (sync and streaming) calls `Agent::from_config_with_observer`, which — per request — resolves the active profile, builds a `SecurityPolicy`, opens the memory backend, builds the full tool registry, **spawns every configured MCP server and runs `tools/list` on it**, and scans skills off disk. On an install with MCP servers, each console message pays subprocess spawn + handshake (hundreds of ms to seconds) before the first token, and a burst of chats spawns N×servers processes. The session store is opened twice per streamed turn, each open re-running the WAL pragma and a migration probe. And the `/api/v1` rate limiter keys on the peer IP — behind the Next BFF (the console's only supported topology) every browser presents `127.0.0.1`, so all console users share one 600/min bucket and one user's tab can 429 everyone.

This plan caches the immutable-per-config parts in `AppState`, holds one session store, and keys the api-tier limiter on the authenticated principal.

## Current state

### Per-request agent — `src/agent/agent.rs:427-580`, called at `src/gateway/api_v1.rs:505` (sync) and `:608` (stream)

`from_config_with_observer` does, in order: `create_runtime` (`:431`), `ProfileManager::active()` (`:433`), `SecurityPolicy::from_config_with_policy_dir` (`:436`), a fresh `PendingApprovals` (`:444`), memory backend (`:447`), tool registry, `discover_mcp_tools(&config.mcp_servers)` (`:488`), skills load (`:566`), provider construction (`:528`).

`src/mcp/discover.rs` (first lines of `discover_mcp_tools`): `if servers.is_empty() { return … }` — so a **zero-MCP install pays nothing** for MCP; the cost is only for installs that configure MCP servers. No process-level client cache exists; each call `McpClient::connect`s per server.

### Session store — `src/gateway/api_v1.rs:268-274`

```rust
fn open_session_store() -> anyhow::Result<crate::sessions::SessionStore> {
    let path = crate::profile::ProfileManager::active()?.sessions_db_path();
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    crate::sessions::SessionStore::open(&path)
}
```

Called from `load_session_history`, both persist sites, and every `sessions_*` handler. Resolves the path from `HOME`/active profile, ignoring `state.config`.

### Rate-limit key — `src/gateway/mod.rs:367-381`, applied at `:200-233`

```rust
fn client_key_from_request(peer_addr, headers, trust_forwarded_headers) -> String {
    if trust_forwarded_headers { if let Some(ip) = forwarded_client_ip(headers) { return ip.to_string(); } }
    peer_addr.map(|a| a.ip().to_string()).unwrap_or_else(|| "unknown".to_string())
}
```

`trust_forwarded_headers` defaults `false`. The limiter (`allow_api`) uses this key; default 600/min. `AppState` is built at `src/gateway/mod.rs:~477-773`; the config is in scope in `build_app`.

### Config fingerprint — `src/gateway/mod.rs:781` (`config_fingerprint` on `AppState`), bumped by the reloader (`spawn_config_reloader`, `:1195`)

This is the existing signal for "config changed" — use it as the cache key.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Gateway tests | `cargo test --lib gateway::` | pass |
| MCP tests | `cargo test --lib mcp::` | pass |
| Full lib | `cargo test --lib` | pass |
| Never | bare `cargo test` | — disk-constrained |

## Scope

**In scope**:
- `src/gateway/mod.rs` (AppState fields: cached session store, cached MCP/skills bundle, principal-keyed limiter; invalidation on fingerprint change)
- `src/gateway/api_v1.rs` (use the cached store; use the cached bundle when building the per-request agent)
- `src/agent/agent.rs` (a constructor variant that accepts pre-discovered MCP tools + skills instead of rediscovering — additive)
- `src/mcp/discover.rs` (only if a cache handle needs a small accessor; avoid if possible)

**Out of scope**:
- The per-request `SecurityPolicy` and memory wiring — cheap relative to MCP; leave per-request so autonomy/scope stay correct per turn.
- Making MCP servers hot-reconnect on config change — out of scope; a fingerprint change simply drops the cache and the next request rebuilds.
- The sync vs stream persist logic (plan 221/222).

## Git workflow

- Branch: `perf/gateway-per-request-cost`.
- Commits: `perf(gateway): hold one session store in AppState`, `perf(gateway): cache MCP discovery and skills across chat requests`, `fix(gateway): key api rate limiting on the authenticated principal behind a proxy`.
- No `Co-Authored-By: Claude`. Do not push/PR unless instructed.

## Steps

### Step 1: One session store in AppState

1. Resolve the DB path once in `build_app` from the same config the gateway was built with (not the ambient profile): `config.workspace-or-profile sessions_db_path`. If the only available resolver is `ProfileManager::active().sessions_db_path()`, use it once here and note that the store now follows the *startup* profile for the process lifetime.
2. Add `session_store: Arc<tokio::sync::Mutex<crate::sessions::SessionStore>>` (or a tiny 2–4 connection pool if `SessionStore` is cheap to clone-open; a single `Mutex` is acceptable given WAL + `busy_timeout`) to `AppState`, opened once.
3. Replace `open_session_store()` call sites with `state.session_store.lock().await` (the handlers are async). `load_session_history` is a free function today (`api_v1.rs:296`) — thread the store in, or inline its two callers. Keep the behaviour identical.
4. Delete `open_session_store()` once it has no callers.

**Verify**: `cargo test --lib api_v1::tests` → the existing SSE/session tests pass (they set `HOME` via `HomeGuard`; the store is now opened from `AppState` built inside the test's `test_state()`, so ensure `test_state()` builds the store under the pinned `HOME` — update `test_state()` to open the store from the same resolved path, still under `ENV_LOCK`). If the env-pin tests (`sse_chat_emits_chunk_then_done`) can no longer prove the pin because the store is built at state construction, keep the proof-of-pin assertion and build `test_state()` after the `HomeGuard` is set.

### Step 2: Cache MCP discovery + skills, keyed on the fingerprint

1. Add to `AppState`: `agent_cache: Arc<tokio::sync::Mutex<Option<CachedAgentParts>>>` where
   ```rust
   struct CachedAgentParts { fingerprint: String, mcp_tools: Vec<Box<dyn Tool>>, mcp_health: Vec<McpServerHealth>, mcp_tools_by_server: HashMap<String, Vec<String>>, skills: Vec<Skill> }
   ```
   (These are the immutable-per-config parts. `Box<dyn Tool>` is not `Clone`; hold the *discovery result* and clone what the agent needs, or store `Arc<[…]>`. If `Tool` cannot be shared across agents because each holds live MCP client handles, then cache the `McpDiscovery` in an `Arc` and have the agent hold `Arc<McpDiscovery>` — check `src/mcp/discover.rs` `McpDiscovery` ownership. If tools genuinely cannot be shared, STOP and report; caching skills alone is still worth a smaller step.)
2. Add `Agent::from_config_with_cached_parts(config, observer, parts: &CachedAgentParts)` (additive) that skips `discover_mcp_tools` and the skills disk scan and splices the cached tools/skills instead. Everything else (policy, memory, provider) stays per-request.
3. In both chat handlers, before building the agent: lock `agent_cache`; if `Some` and `fingerprint == *state.config_fingerprint.lock()`, reuse; else run discovery once, store it, and use it. The reloader already bumps the fingerprint on config change, so a config edit drops the cache naturally.

**Verify**: `cargo test --lib` → passes. Add `cached_parts_are_reused_within_a_fingerprint` if the parts can be constructed in a test without real MCP servers (they can with an empty `mcp_servers` map — discovery returns empty, and the test asserts the second call reuses the same cached object by identity/fingerprint). If MCP tools cannot be built in a unit test, assert at least that skills are cached (a skills dir under a `HomeGuard`).

### Step 3: Fair rate limiting behind the proxy

The console's supported topology is browser → BFF → gateway, all traffic from one IP. Key the **api tier** on the authenticated principal instead:

1. In `api_rate_limit` (`src/gateway/mod.rs:208-233`), derive the key: if a bearer token is present and valid (the request is authenticated), use a stable hash of the token (`format!("tok:{}", short_hash(token))`); else fall back to `client_key_from_request(...)` (the current IP key) for unauthenticated routes.
2. Do not change `client_key_from_request` itself (webhook/other tiers still key on IP). Add the principal derivation in the api-tier middleware only.
3. When `require_pairing` is false (console login off, single implicit principal), all requests share one bucket as before — acceptable, since there is one operator; document that.

**Verify**: `cargo test --lib gateway::` → existing `client_key_*` tests pass. Add `api_rate_limit_keys_on_token_not_ip` — two requests with different bearer tokens from the same peer IP get different buckets (drive `api_rate_limit` with a stub `next` and assert the limiter saw two distinct keys, or unit-test the key-derivation fn directly).

### Step 4: Format, lint, full suite

`cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --lib`.

## Test plan

Named per step. The load-bearing correctness property is that **cached parts never leak conversation/approval state between requests** — the cache holds only tools/skills/health, never history, never `PendingApprovals`, never `conversation_id`. Assert in review that `CachedAgentParts` has no per-turn field.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib` exits 0 with the new tests
- [ ] `rtk proxy grep -n "fn open_session_store" src/gateway/api_v1.rs` returns nothing (deleted)
- [ ] `rtk proxy grep -n "discover_mcp_tools" src/gateway/api_v1.rs` returns nothing (the handlers use the cache; discovery happens once behind it)
- [ ] `rtk proxy grep -n "session_store" src/gateway/mod.rs` shows the AppState field
- [ ] `CachedAgentParts` (or equivalent) holds no history/approval/conversation field (reviewer confirms)
- [ ] No files outside the in-scope list modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

- Cited excerpts do not match live code.
- `Box<dyn Tool>` MCP tool objects cannot be shared across per-request agents (each owns a live child-process handle that must not be double-driven) — report it, ship only the session-store (step 1) and skills-cache half, and leave MCP discovery per-request with a `// TODO: cache once tools are shareable` note.
- Holding one `Mutex<SessionStore>` serializes all session access and a test deadlocks or times out — switch to a small connection pool (open N connections to the same WAL DB) rather than a single mutex; if that is more than a day's work, ship a pool of 1 and note the follow-up.
- Keying the limiter on the token changes behaviour for a legitimate multi-tab single-operator case in a way the operator would notice (each tab is the same token → same bucket, which is fine) — if tokens are per-tab rather than per-operator, report before changing the key.
- Plan 221 has already refactored `open_session_store`/the timeout in a conflicting way — rebase on it; do not duplicate.
- A step's verification fails twice after a reasonable fix.

## Maintenance notes

- The cache invalidates on `config_fingerprint` change only. A change that does **not** bump the fingerprint (if any exists) would serve stale MCP tools — verify the reloader bumps the fingerprint for `mcp_servers` and `skills` edits; if it does not, that is a separate fix.
- The session store now follows the startup profile. A runtime profile switch (if the gateway supports one) must rebuild the store handle — check whether profile switching is a thing on the gateway before assuming it is not.
- Reviewer focus: no per-turn state in the cache; the limiter fallback to IP for unauthenticated routes; the env-pin test discipline after the store moved into `AppState`.
