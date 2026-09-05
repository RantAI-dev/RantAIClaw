# Plan 012: Reuse the existing proxy-aware client factory on the KB-retrieval + http_request hot paths

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/config/ src/kb/retrieve/ src/tools/http_request.rs`
> If any changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.
>
> **REVISED after cold review**: an equivalent cached, proxy-aware,
> hot-reload-invalidated client factory ALREADY EXISTS in this codebase. Do NOT
> build a new one — reuse it. The earlier draft of this plan proposed a new
> `LazyLock<Mutex<HashMap>>` cache; that was wrong (it would not be invalidated
> on proxy hot-reload, and a single key would collapse three different timeouts).

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

A `reqwest::Client` owns a connection pool. The KB retrieval stages
(query-expansion, standalone-query, contextual-prefix) each build a **fresh**
client inside their per-retrieval `fetch_*` fns, and they build it with
`reqwest::Client::builder()` directly — bypassing the runtime proxy config, so
they also ignore any configured proxy. Constructing a new client per call defeats
keep-alive (fresh DNS+TCP+TLS per call). The codebase already has a shared,
pooled, proxy-aware, auto-invalidated client factory; the fix is to route the
hot paths through it. This gets connection reuse AND proxy consistency for free.

## Current state

- **The existing factory to reuse** (verified by cold review at `4d35107`):
  - `src/config/schema.rs:1751` — `build_runtime_proxy_client(service_key)`
  - `src/config/schema.rs:1766` — `build_runtime_proxy_client_with_timeouts(service_key, timeout_secs, connect_timeout_secs)`
  - Both cache clients in `RUNTIME_PROXY_CLIENT_CACHE` (`schema.rs:47`, a
    `std::sync::OnceLock<RwLock<..>>`) keyed by `service_key|timeout|connect_timeout`
    (`schema.rs:1689`) and reuse on repeat calls (test at `schema.rs:6786`).
  - Both are re-exported from `src/config/mod.rs` (the `pub use` block; the free
    fn `apply_runtime_proxy_to_builder` is at `schema.rs:1744`, also re-exported).
    Confirm the exact re-exports: `grep -n "build_runtime_proxy_client\|apply_runtime_proxy_to_builder" src/config/mod.rs`.
  - **Hot-reload invalidation already works**: `set_runtime_proxy_config`
    (`schema.rs:1724`) calls `clear_runtime_proxy_client_cache()` (`schema.rs:1734`);
    proven by test `set_runtime_proxy_config_clears_runtime_proxy_client_cache`
    (`schema.rs:6807`). The proxy is mutated at runtime by the `proxy_config` tool
    (`src/tools/proxy_config.rs:249/274/312`) and by `Config` apply
    (`schema.rs:4247`). So reusing this cache means proxy changes invalidate
    cached clients automatically — no extra wiring needed.

- **What the existing helpers do NOT set**: a redirect policy.
  `build_runtime_proxy_client_with_timeouts` sets timeout + connect_timeout +
  proxy only (`schema.rs:1777-1780`). The `http_request` tool needs
  `redirect(Policy::none())` for SSRF safety, so it needs a redirect-capable
  variant (added in Step 1).

- `src/tools/http_request.rs:110-122` — builds a fresh client per request (already
  applies the proxy builder, but not pooled, and disables redirects):
  ```rust
  let builder = reqwest::Client::builder()
      .timeout(Duration::from_secs(self.timeout_secs))
      .connect_timeout(Duration::from_secs(10))
      .redirect(reqwest::redirect::Policy::none());
  let builder = crate::config::apply_runtime_proxy_to_builder(builder, "tool.http_request");
  let client = builder.build()?;   // per call
  ```

- KB retrieval sites — each builds a fresh client per call via
  `reqwest::Client::builder().timeout(TIMEOUT).build().map_err(|e| format!("client build: {e}"))?`,
  and each has a DIFFERENT `TIMEOUT` (verified):
  - `src/kb/retrieve/query_expansion.rs:127-130`, `TIMEOUT` = 8s (`:22`)
  - `src/kb/retrieve/standalone_query.rs:179-182`, `TIMEOUT` = 6s (`:29`)
  - `src/kb/retrieve/contextual.rs:107-110`, `TIMEOUT` = 30s (`:15`)
  None sets a `connect_timeout` today.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint (kb) | `cargo clippy --features kb --all-targets -- -D warnings` | exit 0 |
| KB retrieve tests | `cargo test --features kb retrieve` | all pass |
| http_request tests | `cargo test http_request` | all pass |
| Proxy-cache tests still green | `cargo test runtime_proxy` | all pass |

## Scope

**In scope**:
- `src/config/schema.rs` — add ONE redirect-capable variant next to
  `build_runtime_proxy_client_with_timeouts`, reusing `RUNTIME_PROXY_CLIENT_CACHE`
  (cache key must include the redirect setting so it doesn't collide).
- `src/tools/http_request.rs` — use that variant; keep per-request timeout via
  `RequestBuilder::timeout`.
- `src/kb/retrieve/query_expansion.rs`, `standalone_query.rs`, `contextual.rs` —
  use `build_runtime_proxy_client_with_timeouts` with **per-site service keys**.

**Out of scope** (do NOT touch):
- Do NOT create a new `src/config/http_client.rs` or a second client cache.
- Do NOT change `RUNTIME_PROXY_CLIENT_CACHE`'s invalidation logic.
- The other ~45 ad-hoc `reqwest::Client` sites — a follow-up migrates them.
- The SSRF guards in `http_request` (`assert_host_resolves_to_public`, redirect
  policy) — preserve exactly.

## Git workflow

- Branch: `advisor/012-shared-http-client-factory`
- Commit per logical unit; messages e.g.
  `perf(http): reuse runtime-proxy client cache on KB retrieval + http_request`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Add a redirect-capable variant that reuses the cache

In `src/config/schema.rs`, next to `build_runtime_proxy_client_with_timeouts`
(`:1766`), add e.g.
`build_runtime_proxy_client_no_redirect(service_key, timeout_secs, connect_timeout_secs) -> reqwest::Client`
that builds the client with `.redirect(reqwest::redirect::Policy::none())` in
addition to the timeouts + proxy, and stores it in `RUNTIME_PROXY_CLIENT_CACHE`.
**The cache key MUST include a redirect discriminator** (e.g. append
`|redirect=none` to the existing `service_key|timeout|connect_timeout` key at
`schema.rs:1689`) so a no-redirect client and a normal client for the same
service/timeouts don't alias. Read `build_runtime_proxy_client_with_timeouts`'s
body and mirror it exactly, adding the redirect line and the key discriminator.

Re-export it from `src/config/mod.rs` alongside the existing helpers.

**Verify**: `cargo build 2>&1 | tail -5` → compiles;
`cargo test runtime_proxy` → existing cache/invalidation tests still pass.

### Step 2: Convert `http_request` to the shared no-redirect client

Replace the per-call builder in `execute_request` (`http_request.rs:117-122`)
with:
```rust
let client = crate::config::build_runtime_proxy_client_no_redirect(
    "tool.http_request", self.timeout_secs, 10,
);
let mut request = client
    .request(method, url)
    .timeout(Duration::from_secs(self.timeout_secs));  // per-request timeout on the shared client
```
Because `self.timeout_secs` is already in the cache key, requests with different
timeouts get distinct cached clients — but also set the per-request timeout so
behavior is identical to today. Keep `assert_host_resolves_to_public` and every
other SSRF check unchanged. Note: the factory returns a `reqwest::Client` (not a
`Result`), so drop the `?` on the build.

**Verify**: `cargo test http_request` → all pass;
`grep -n "Client::builder" src/tools/http_request.rs` → no per-call builder in
`execute_request`; redirect `Policy::none()` is still in effect (now set inside
the factory variant — confirm by reading Step 1's addition).

### Step 3: Convert the three KB-retrieval sites (per-site keys, preserve timeouts)

In each site, replace
`reqwest::Client::builder().timeout(TIMEOUT).build().map_err(|e| format!("client build: {e}"))?`
with a call to the existing factory using a **distinct** service key so the three
different timeouts don't collapse:
- `query_expansion.rs`: `crate::config::build_runtime_proxy_client_with_timeouts("kb.retrieve.query_expansion", 8, 10)`
- `standalone_query.rs`: `... ("kb.retrieve.standalone_query", 6, 10)`
- `contextual.rs`: `... ("kb.retrieve.contextual", 30, 10)`

Use each site's real `TIMEOUT` value (read it; don't hardcode if it's a named
const — pass `TIMEOUT.as_secs()` or the literal the const uses). The factory
returns a `reqwest::Client`, NOT a `Result` — **remove the `.map_err(...)?`** and
the surrounding error handling on the build (the subsequent `.post(...).send()`
still returns a `Result` as before). Adding a 10s `connect_timeout` where there
was none is a minor, acceptable behavior improvement — note it in the PR.

**Verify**:
`grep -rn "Client::builder\|Client::new" src/kb/retrieve/query_expansion.rs src/kb/retrieve/standalone_query.rs src/kb/retrieve/contextual.rs`
→ no matches; `cargo test --features kb retrieve` → all pass.

## Test plan

- The existing `runtime_proxy` cache tests + the `retrieve` and `http_request`
  tests are the primary guard (they must all still pass).
- Add ONE test for the new variant (in `schema.rs` `#[cfg(test)]`, next to the
  existing cache tests):
  - `build_runtime_proxy_client_no_redirect_is_cached`: two calls with the same
    key return cached instances (assert the cache has one entry for the key, like
    the existing `schema.rs:6786` test does), and a normal client for the same
    service/timeouts is a SEPARATE cache entry (redirect discriminator works).
- Do NOT add network dependence.
- Verification: `cargo test runtime_proxy`, `cargo test http_request`,
  `cargo test --features kb retrieve` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --features kb --all-targets -- -D warnings` exits 0
- [ ] No new client cache module exists (`ls src/config/http_client.rs` → not found; the fix lives in `schema.rs`)
- [ ] `grep -rn "Client::builder\|Client::new" src/kb/retrieve/` returns no matches in the three converted files
- [ ] `grep -n "Client::builder" src/tools/http_request.rs` shows no per-call builder in `execute_request`
- [ ] `http_request` still disables redirects (via the factory variant) and applies the per-request timeout
- [ ] The three KB sites use three DISTINCT service keys (timeouts not collapsed)
- [ ] `cargo test runtime_proxy http_request` pass and `cargo test --features kb retrieve` passes
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `build_runtime_proxy_client_with_timeouts` / `RUNTIME_PROXY_CLIENT_CACHE` /
  `clear_runtime_proxy_client_cache` are not where the excerpts say (drift).
- Adding the redirect discriminator to the cache key would change the key format
  used by existing callers in a way that breaks their cache hits — report the key
  structure before changing it (append, don't restructure).
- A KB retrieve site's existing test asserts a specific timeout/no-connect-timeout
  behavior that the factory changes — report rather than loosening the test.

## Maintenance notes

- Follow-up (deferred): migrate the remaining ~45 ad-hoc client sites to the
  factory, one subsystem per PR — that's where proxy-consistency + pooling wins
  compound. List them: `grep -rn "Client::builder\|Client::new" src/ | grep -v test`.
- Reviewer should confirm the redirect discriminator prevents a no-redirect
  client from being handed to a caller expecting redirects (and vice-versa), and
  that `http_request` keeps `Policy::none()`.
- Because the reused cache is invalidated on proxy change, no stale-proxy bug is
  introduced — unlike a standalone cache would have been.
