# Plan 013: Build a gateway test harness and cover Config-API auth + secret redaction end-to-end

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/gateway/ tests/config_api.rs`
> If any changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.
>
> **REVISED after cold review**: there is NO existing router/AppState seam — you
> must add one, and it must construct `AppState` from a `Config` via the PUBLIC
> provider/memory factories (the cheap mocks are `#[cfg(test)]`-only and
> unreachable from `tests/`). Route paths are all under `/api/v1/` and some the
> earlier draft named do not exist. All corrected below.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none (its harness is reused by plan 019)
- **Category**: tests
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

The Live Config API mutates live config and returns secret-redacted state over
HTTP, yet `tests/config_api.rs` is a single assert-nothing placeholder — no test
proves an unauthenticated request is rejected or that redaction holds. A
regression dropping `check_auth` on a route, or weakening redaction, would pass
CI. The blocker is a missing gateway test harness (bind on port 0, deterministic
token). Once it exists, the endpoint tests are quick — and the harness is reused
to test-enforce the `/api/v1` contract (plan 019).

## Current state (verified at 4d35107 — no drift)

- `tests/config_api.rs` — still just `config_api_test_placeholder()` (asserts
  nothing) with a TODO listing the harness requirements.

- **No seam exists.** `run_gateway` (`src/gateway/mod.rs:356`) is the ONLY entry:
  it binds the listener internally (`:374`), builds `AppState` inline
  (`:650-681`), builds the `Router` inline (`:693-744`), serves inline (`:749`),
  and never returns the OS-assigned port (`actual_port` at `:375` is only
  printed). There is NO `build_router` fn and NO public `AppState` constructor.
  The `.merge(api_v1::router())` / `.merge(config_api::router())` calls are at
  `mod.rs:724-725`.

- **AppState is 25 fields** incl. `provider: Arc<dyn Provider>` and
  `mem: Arc<dyn Memory>`. The only cheap construction (mocks) is in
  `#[cfg(test)] mod tests` (`mod.rs:2283-2357`, `MockProvider` `:2711`,
  `MockMemory` `:2658`) — **unreachable from integration tests** (which compile
  the crate without `cfg(test)`). Public factories that DO work from `tests/`:
  - provider: `providers::create_resilient_provider_with_options(...)` (used at
    `mod.rs:380`; accepts `credential: None`, builds without network).
  - memory: `memory::create_memory_with_storage(&config.memory, …, &config.workspace_dir, …)`
    (`src/memory/mod.rs:180`; needs a temp `workspace_dir`).
  - observer: `create_observer(...)` (`src/observability/mod.rs:25`).
  - tools: `tools_registry: Arc::new(Vec::new())` (config-api handlers never
    touch tools).
  Read `run_gateway`'s `AppState { … }` literal (`mod.rs:650-681`) to get the
  full, current field list — mirror it in the seam.

- **Auth mechanism**: `check_auth` (`config_api.rs:59`) short-circuits to `Ok`
  when `require_pairing()` is FALSE (`config_api.rs:60-62`). The guard is built
  from `PairingGuard::new(config.gateway.require_pairing, &config.gateway.paired_tokens)`
  (`mod.rs:559`). A plaintext token in `paired_tokens` is SHA-256-hashed on load
  (`src/security/pairing.rs:60-70,289`); `is_authenticated` hashes the incoming
  bearer and compares (`pairing.rs:175-182`). So for deterministic auth tests set
  `gateway.require_pairing = true` and `gateway.paired_tokens = vec!["test-not-a-real-token".into()]`,
  then send `Authorization: Bearer test-not-a-real-token`. NO token file/store is
  involved. (401 tests REQUIRE `require_pairing = true`, else auth is bypassed.)

- **Real config routes** (`config_api.rs:33-45`): `GET /api/v1/config`,
  `PUT /api/v1/config/model`, `PUT /api/v1/config/autonomy`,
  `GET|PUT /api/v1/secrets`, `POST|DELETE /api/v1/config/mcp_servers/{name}`,
  `POST|DELETE /api/v1/channels/telegram`. There is NO `GET /config/channels` or
  `GET /config/mcp-servers` in this module. Channel status is
  `GET /api/v1/channels` in `api_v1.rs:56` (`channels_list`). `check_auth` call
  sites: `config_api.rs:106,273,348,417,444,577,663,769,781,821,848`.

- **Redaction**: `GET /api/v1/config` runs `redact_config_secrets`
  (`config_api.rs:178`) → recursive `redact_secrets_in_json` (`config_api.rs:132-174`),
  invoked in `get_config` (`config_api.rs:110,117`). (`get_secrets` at `:765-772`
  is presence-only, a different thing.)

- **Mutation handlers persist to the GLOBAL config path.** `set_model` etc. call
  `lock_and_load()` → `Config::load_or_init()` (`config_api.rs:232-238`) and
  `persist_and_swap()` → `cfg.save()` (`:244-246`), resolving the path from
  `RANTAICLAW_CONFIG_DIR` (`schema.rs:3518`, default `~/.rantaiclaw`) — NOT from
  `state.config`. So PUT/POST tests MUST set `RANTAICLAW_CONFIG_DIR` to a temp
  dir or they read+write the developer's real config. GET tests (`get_config`/
  `get_secrets`) are hermetic (read `state.config.lock()` only).

- Deps: `reqwest` (with `blocking`+`json`) and `tokio` are REGULAR deps
  (`Cargo.toml:32`, `:24`), usable from tests; `tempfile` is a dev-dep
  (`Cargo.toml:307`). `crate::test_env::ENV_LOCK` is `pub(crate)` in a private
  module (`src/lib.rs:84`) — **NOT reachable from integration tests**; do not
  reference it.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| The new integration tests | `cargo test --test config_api -- --test-threads=1` | all pass |

Run this test binary single-threaded (`--test-threads=1`) because mutation tests
set the process-global `RANTAICLAW_CONFIG_DIR`.

## Scope

**In scope**:
- `src/gateway/mod.rs` — add a crate-internal seam (see Step 1). This is the one
  necessary `src/` change; keep it minimal and note it in the PR.
- `tests/config_api.rs` — the harness + tests.

**Out of scope** (do NOT touch):
- Auth logic, redaction logic, route handlers — this plan adds TESTS. If a test
  reveals a real auth/redaction bug, STOP and report it (don't fix inline).
- Starting real channels/MCP servers — the harness starts only the HTTP gateway.

## Git workflow

- Branch: `advisor/013-config-api-auth-integration-tests`
- Commit per logical unit (seam, then tests). Messages e.g.
  `test(gateway): add port-0 test harness; cover config-api auth + redaction`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Add a router-building seam

Extract the `AppState` construction (`mod.rs:650-681`) and the `Router::new()…`
builder (`mod.rs:693-744`) into a new crate-internal async fn, e.g.:
```rust
// in src/gateway/mod.rs — pub(crate) so integration tests reach it via the crate
pub(crate) async fn build_gateway_router(config: Config) -> anyhow::Result<axum::Router> {
    // construct AppState from `config` using the SAME public factories run_gateway uses:
    //   provider = providers::create_resilient_provider_with_options(..., credential: None, ...)
    //   mem      = memory::create_memory_with_storage(&config.memory, ..., &config.workspace_dir, ...)
    //   observer = create_observer(...)
    //   tools_registry = Arc::new(Vec::new())
    //   pairing  = PairingGuard::new(config.gateway.require_pairing, &config.gateway.paired_tokens)
    //   ... every other AppState field exactly as mod.rs:650-681 builds it ...
    // then return the Router::new()... .merge(api_v1::router()).merge(config_api::router()).with_state(state)
}
```
Refactor `run_gateway` to call `build_gateway_router` so there is ONE source of
truth (bind + serve stay in `run_gateway`). This keeps production behavior
identical and gives the test a router it can serve on its own port-0 listener.
`pub(crate)` is NOT enough for a `tests/` integration crate — it is a separate
crate. Make `build_gateway_router` `pub` (documented as test/embedding seam) OR
put the integration tests behind the crate as a `#[cfg(test)]`-gated in-`src`
integration module. **Prefer `pub`** with a doc comment "exposed for embedding
and integration tests."

In `tests/config_api.rs`, write `spawn_test_gateway(config) -> (String base_url)`:
bind `tokio::net::TcpListener::bind("127.0.0.1:0")`, read `local_addr()` for the
port, `tokio::spawn(axum::serve(listener, build_gateway_router(config).await?))`,
return `http://127.0.0.1:<port>`.

**Verify**: `cargo build --tests 2>&1 | tail -5` → compiles;
`cargo build 2>&1 | tail -5` → the refactored `run_gateway` still compiles.

### Step 2: Build the test Config (temp dirs, deterministic token)

Helper that returns a minimal `Config` with: a `tempfile::TempDir` for
`workspace_dir`; `gateway.require_pairing = true`;
`gateway.paired_tokens = vec!["test-not-a-real-token".into()]`; a memory backend
that needs no network (the default sqlite under the temp workspace); a provider
config that builds offline (`create_resilient_provider_with_options` with
`credential: None`). For MUTATION tests, ALSO set the env
`RANTAICLAW_CONFIG_DIR` to a `TempDir` before issuing the request (the handlers
persist there, not to `state.config`).

**Verify**: `cargo build --tests 2>&1 | tail -5` → compiles.

### Step 3: Write the auth + redaction tests (real routes)

Use ASYNC `reqwest` inside `#[tokio::test]` (a blocking client on the serve
runtime thread panics). Tests:
1. `get_config_without_auth_returns_401` — `GET /api/v1/config`, no Authorization
   → 401 (works because `require_pairing = true`).
2. `get_config_with_auth_returns_200_json` — `GET /api/v1/config` with
   `Bearer test-not-a-real-token` → 200, body parses as JSON.
3. `get_config_redacts_secrets` — seed the temp config with a secret-named field
   set to a neutral placeholder (e.g. a fake token in
   `channels_config.telegram.bot_token` or an mcp env), GET `/api/v1/config` with
   auth, assert the response body does NOT contain the placeholder value (redacted).
4. `put_model_with_auth_returns_200` — `PUT /api/v1/config/model` with a valid
   body and auth → 200 (set `RANTAICLAW_CONFIG_DIR` to a temp dir first).
5. `put_model_without_auth_returns_401` — `PUT /api/v1/config/model` no token → 401.
6. `get_channels_returns_200` — `GET /api/v1/channels` with auth → 200 status map
   (this is the real channel-status route; the earlier `GET /config/channels` and
   `GET /config/mcp-servers` do NOT exist — do not use them).
- Placeholders MUST be neutral (`"test-not-a-real-token"`) — never a real credential.

**Verify**: `cargo test --test config_api -- --test-threads=1` → all pass.

### Step 4: Remove the placeholder

Delete `config_api_test_placeholder`.

**Verify**: `grep -n "placeholder" tests/config_api.rs` → no matches.

## Test plan

- The six tests above are the deliverable. Hermetic: temp workspace, temp
  `RANTAICLAW_CONFIG_DIR` for mutations, port 0, async reqwest, no real
  channels/MCP/network. Single-threaded test binary (env is process-global).
- Verification: `cargo test --test config_api -- --test-threads=1` → all pass;
  run twice for stability.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --test config_api -- --test-threads=1` passes with the 6 tests
- [ ] `grep -n "placeholder" tests/config_api.rs` → no matches
- [ ] `get_config_without_auth_returns_401` and `get_config_redacts_secrets` exist and pass
- [ ] All test routes are real (`/api/v1/config`, `PUT /api/v1/config/model`, `GET /api/v1/channels`); no `GET /config/channels` or `/config/mcp-servers`
- [ ] `run_gateway` still builds and behaves identically (only refactored to call the seam)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- Extracting `build_gateway_router` requires threading a field whose construction
  pulls a real channel/MCP/network dependency you can't satisfy with a temp
  config — report the offending `AppState` field and its constructor.
- A test reveals an endpoint is NOT auth-gated or redaction leaks a secret — STOP
  and report as a security finding; do not fix behavior in this test-only plan.
- `create_resilient_provider_with_options` or `create_memory_with_storage` do not
  build offline with `credential: None` / a temp workspace (they hit the network
  at construction) — report; the harness needs a no-network construction path.

## Maintenance notes

- Keep `build_gateway_router` and `spawn_test_gateway()` reusable — plan 019
  test-enforces the `/api/v1` contract with them. When the second consumer lands,
  move `spawn_test_gateway` to a shared `tests/support` module.
- Reviewer should confirm the seam refactor left `run_gateway`'s production path
  byte-equivalent (same AppState, same routes) and that the harness starts ONLY
  the HTTP gateway.
- Every new config route must get a 401-without-token test here — state that in
  the PR.
