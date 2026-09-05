# Plan 221: Harden the sessions and chat handlers — searchable queries, sanitized errors, no resurrected sessions, a real cancel guard, a wired request timeout, and SQL-backed insights

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 8503328..HEAD -- src/gateway/api_v1.rs src/gateway/mod.rs src/sessions/store.rs src/sessions/cli.rs src/config/schema.rs docs/reference/config.md docs/reference/api-v1.md`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M (seven small, independent fixes + tests in one PR)
- **Risk**: LOW — each step is local and reversible; the timeout step changes a default (120 s → 300 s) and is called out in the PR.
- **Depends on**: none (plan 220 touches the same file but different functions; rebase order does not matter)
- **Category**: bug
- **Planned at**: commit `8503328`, 2026-08-24
- **Branch**: `fix/sessions-api-hardening`
- **One PR**: commit per step.

## Why this matters

Seven confirmed defects in the sessions/chat handlers, all small, all in the same two files:

1. `POST /api/v1/sessions/search` binds the user's text straight into an FTS5 `MATCH`, so a stray `"` or `*` is a **500** with the raw SQLite message.
2. `err_500` returns the full `anyhow` chain to the browser — for the sessions routes that includes the operator's home directory and profile name (`failed to open session db at /home/…`).
3. `temperature` from the request body is copied unvalidated (NaN, 99.0, …) into the provider call.
4. Session paging orders by `started_at` only (second granularity) — tied rows can duplicate or skip across pages.
5. Deleting a session while a turn is streaming on it **resurrects** it: `record_api_turn` adopts the caller-supplied UUID and inserts a fresh row.
6. The stream's `CancelOnDrop` guard is created *inside* the `async_stream::stream!` body, so a client that disconnects before the body is first polled leaves the turn running; and the `TimeoutLayer` hardcodes 120 s while `[gateway] request_timeout_secs` (default 300) is **read by nothing** — a dead config knob.
7. `GET /api/v1/insights` loads 10,000 session rows to count them in Rust (wrong totals past 10k; `count_sessions()` already exists in SQL); the CLI `sessions insights` duplicates the bug, and the CLI slices session ids by byte.

None of the sessions handlers has a test, and neither `agent_chat` nor `resolve_approval` has a 401 test (every other route family does).

## Current state

### 1. FTS search — `src/sessions/store.rs:572-582` and `src/gateway/api_v1.rs:967-995`

```rust
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.session_id, s.title, m.id, m.role, m.content, m.timestamp, \
             bm25(messages_fts) as rank \
             FROM messages_fts \
             JOIN messages m ON messages_fts.rowid = m.id \
             JOIN sessions s ON m.session_id = s.id \
             WHERE messages_fts MATCH ?1 \
             ORDER BY rank \
             LIMIT ?2",
        )?;
```

The handler passes `body.query` unchanged (`api_v1.rs:978`), after rejecting only an empty query (`:973`) and capping `limit` at 200 (`:977`). The CLI `sessions search` (`src/sessions/cli.rs:85`) calls the same `store.search`.

### 2. `err_500` — `src/gateway/api_v1.rs:169-178`

```rust
fn err_500(e: anyhow::Error) -> (StatusCode, Json<ErrorBody>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorBody {
            error: "internal_error".into(),
            matches: None,
            detail: Some(format!("{e:#}")),
        }),
    )
}
```

`src/sessions/store.rs:124-126` produces `format!("failed to open session db at {}", path.display())`. The chat path already scrubs via `crate::providers::sanitize_api_error` (`api_v1.rs:527-530`, defined at `src/providers/mod.rs:812`); the sessions handlers (`:906, 940, 976, 1009, 1030`) do not.

### 3. Temperature — `src/gateway/api_v1.rs:865-877`

```rust
fn chat_config_from_body(state: &AppState, body: &ChatRequestBody) -> crate::config::Config {
    let mut config = state.config.lock().clone();
    if let Some(p) = body.provider.clone() { config.default_provider = Some(p); }
    if let Some(m) = body.model.clone() { config.default_model = Some(m); }
    if let Some(t) = body.temperature { config.default_temperature = t; }
    config
}
```

Called from `agent_chat_sync` (~491) and `agent_chat_stream` (~583). Client-selected `provider`/`model` is by design (single-owner console) — leave those.

### 4. Paging — `src/sessions/store.rs:549-553`

```rust
            "SELECT id, title, model, started_at, message_count \
             FROM sessions ORDER BY started_at DESC LIMIT ?1 OFFSET ?2",
```

`started_at` is `chrono::Utc::now().timestamp()` (seconds). The paging test at `store.rs:849-875` inserts 25 distinct timestamps, so it cannot see a tie.

### 5. Resurrection — `src/sessions/store.rs:432-471` (inside `record_api_turn`)

```rust
        let existing = match session_id {
            Some(sid) if !sid.is_empty() => { /* SELECT 1 FROM sessions WHERE id = ?1 → Some(sid) or None */ }
            _ => None,
        };
        let (id, is_new) = match existing {
            Some(id) => (id, false),
            None => {
                // Honour a caller-supplied id when it is UUID-shaped ...
                let id = session_id
                    .filter(|sid| is_uuid_shaped(sid))
                    .map_or_else(|| Uuid::new_v4().to_string(), str::to_string);
                ...
                tx.execute("INSERT INTO sessions (id, model, started_at, source) VALUES (?1, ?2, ?3, ?4)", ...)?;
                (id, true)
            }
        };
```

The adoption is needed (the console names its session before the first turn — see the comment there and `docs/reference/api-v1.md:191-197`). The handlers already know whether the session existed when the turn *started*: `load_session_history(...)` (`api_v1.rs:299-310`) returned non-empty history. `sessions_delete` (`:1024-1036`) takes no lock. The persist sites are `api_v1.rs:536-560` (sync) and `:771-796` (stream). `SessionStore::get_session(&self, id) -> Result<Option<Session>>` exists (`store.rs:~172`).

### 6. Cancel guard and timeout — `src/gateway/api_v1.rs:730-731, 879-885` and `src/gateway/mod.rs:49-53, 840-844`

```rust
    let stream = async_stream::stream! {
        let _cancel_on_drop = CancelOnDrop(cancel_for_stream);
        let mut buffered_text = String::new();
```

```rust
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop { fn drop(&mut self) { self.0.cancel(); } }
```

```rust
pub const MAX_BODY_SIZE: usize = 65_536;
pub const REQUEST_TIMEOUT_SECS: u64 = 120;
...
        .layer(RequestBodyLimitLayer::new(MAX_BODY_SIZE))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(REQUEST_TIMEOUT_SECS),
        ));
```

`src/config/schema.rs:972-978` declares `pub request_timeout_secs: u64` on `GatewayConfig` ("default: 300 … Increase for workloads with long-running tool calls") with `default_gateway_request_timeout_secs() -> 300` at `:1040`. `rtk proxy grep -rn request_timeout_secs src/` finds **no reader** outside `schema.rs`. `tower_http::timeout::TimeoutLayer` wraps the response future only, so the SSE body is unaffected; the sync handler *is* the response future and is dropped at the deadline before it persists.

### 7. Insights — `src/gateway/api_v1.rs:1046-1063` and `src/sessions/cli.rs:133-140`

```rust
    let store = open_session_store().map_err(err_500)?;
    let sessions = store.list_sessions(10_000).map_err(err_500)?;
    let total_sessions = sessions.len();
    let total_messages: i64 = sessions.iter().map(|s| s.message_count).sum();
    let avg = if total_sessions > 0 { total_messages as f64 / total_sessions as f64 } else { 0.0 };
    Ok(Json(serde_json::json!({
        "total_sessions": total_sessions,
        "total_messages": total_messages,
        "avg_messages_per_session": avg,
        "latest_session_id": sessions.first().map(|s| s.id.clone()),
        "latest_session_started_at": sessions.first().map(|s| s.started_at),
    })))
```

`count_sessions()` at `store.rs:537` already does `SELECT COUNT(*)`. `src/sessions/cli.rs:37, :93, :154` slice ids with `&s.id[..s.id.len().min(8)]` (bytes).

### Conventions to match

- Error helpers `err_400`/`err_404`/`err_500` (`api_v1.rs:169-200`).
- Store methods return `anyhow::Result`, use `params![]`, and have unit tests in `store.rs` `mod tests` using `SessionStore::open_in_memory()` (see `store.rs:849-875` for the paging test shape).
- Handler tests live in `api_v1.rs` `mod tests`; any test that opens the store takes `crate::test_env::ENV_LOCK` and pins `HomeGuard::set(tmp.path())` with the proof-of-pin assertion, exactly as `sse_chat_emits_chunk_then_done` (`api_v1.rs:2290-2315`). 401 tests use `paired_state("tok")` (`:2524-2532`).
- Config docs: `docs/reference/config.md` documents `[gateway]` keys; add `request_timeout_secs` there.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Store tests | `cargo test --lib sessions::store` | all pass |
| Handler tests | `cargo test --lib api_v1::tests` | all pass |
| Full lib suite | `cargo test --lib` | all pass |
| Never | bare `cargo test` | — disk-constrained |

## Scope

**In scope**:
- `src/sessions/store.rs`, `src/sessions/cli.rs`
- `src/gateway/api_v1.rs`, `src/gateway/mod.rs`
- `docs/reference/config.md` (one key), `docs/reference/api-v1.md` (search + insights wording)

**Out of scope**:
- `src/sessions/store.rs::split_session` — it is currently unused by production code but plan 231 builds the session-fork route on it. **Do not delete it.**
- `open_session_store()` per-request opening and its `HOME` resolution — plan 225 moves the store into `AppState`; here, keep calling `open_session_store()`.
- Client-selected `provider`/`model` — by design.
- Approval scoping (plan 220) and the chat request/response contract (plan 222).

## Git workflow

- Branch: `fix/sessions-api-hardening`.
- Commit per step: `fix(api): quote session search terms so FTS syntax cannot 500`, `fix(api): sanitize err_500 details and redact profile paths`, `fix(api): reject non-finite or out-of-range temperature`, `fix(sessions): break started_at ties by id when paging`, `fix(api): do not resurrect a session deleted mid-turn`, `fix(gateway): arm the SSE cancel guard before first poll; honor [gateway] request_timeout_secs`, `fix(api): compute insights in SQL`, `test(api): cover sessions handlers and chat/approval auth`.
- No `Co-Authored-By: Claude` trailer. Do not push/open a PR unless instructed.

## Steps

### Step 1: Quote search terms before `MATCH`

In `src/sessions/store.rs`, add (private):

```rust
/// Turn free text into an FTS5 query that matches it literally: each
/// whitespace token becomes a quoted phrase (inner `"` doubled), joined by
/// implicit AND. `"`, `*`, `(`, `NEAR` and other FTS operators in user input
/// therefore never reach the parser as syntax.
fn fts_literal_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}
```

In `search`, bind `fts_literal_query(query)` instead of `query`. Keep the empty-query guard in the handler; additionally, if `fts_literal_query` returns an empty string (whitespace-only input), return `Ok(Vec::new())` from `search`.

Tests in `store.rs`: `search_with_quote_and_star_does_not_error` (insert a message `hello "world"*`, search `"` → `Ok`, search `*` → `Ok`, search `hello` → 1 hit); `search_is_literal_not_boolean` (message `alpha beta`, query `alpha OR gamma` → 0 hits because `OR` is now a literal token).

**Verify**: `cargo test --lib sessions::store::tests::search_` → new tests pass; existing `search_*` tests pass.

### Step 2: Sanitize `err_500`

Replace the body of `err_500`:

```rust
fn err_500(e: anyhow::Error) -> (StatusCode, Json<ErrorBody>) {
    let full = format!("{e:#}");
    tracing::error!(error = %full, "api_v1 internal error");
    let detail = crate::providers::sanitize_api_error(&redact_profile_paths(&full));
    (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorBody { error: "internal_error".into(), matches: None, detail: Some(detail) }))
}

/// Replace the active profile root and the home directory with placeholders so
/// an error about a file never tells a browser where the operator's files are.
fn redact_profile_paths(s: &str) -> String {
    let mut out = s.to_string();
    if let Ok(p) = crate::profile::ProfileManager::active() {
        out = out.replace(&p.root.display().to_string(), "<profile>");
    }
    if let Some(home) = std::env::var_os("HOME") {
        out = out.replace(&home.to_string_lossy().to_string(), "~");
    }
    out
}
```

The chat handlers' explicit `sanitize_api_error` wrapping (`:527-530`, `:717`) can stay (double sanitize is idempotent).

Test: `err_500_redacts_profile_root_and_home` — under `ENV_LOCK` + `HomeGuard`, build `anyhow!("failed to open session db at {}", profile.sessions_db_path().display())`, assert `detail` contains `<profile>` and not the temp path.

**Verify**: `cargo test --lib api_v1::tests::err_500_` → pass.

### Step 3: Validate `temperature`

Change `chat_config_from_body` to return `Result<Config, (StatusCode, Json<ErrorBody>)>`:

```rust
    if let Some(t) = body.temperature {
        if !t.is_finite() || !(0.0..=2.0).contains(&t) {
            return Err(err_400("temperature must be a finite number between 0.0 and 2.0"));
        }
        config.default_temperature = t;
    }
```

Update both callers (`agent_chat_sync` ~491, `agent_chat_stream` ~583) with `?` (the stream handler returns a `Result` already — check its signature; if it returns `impl IntoResponse`, map the error to a JSON 400 response before spawning). Document the range in `docs/reference/api-v1.md:186` (`"temperature": 0.7` → add "0.0–2.0, finite; else 400").

Test: `chat_config_rejects_non_finite_temperature` (NaN → 400) and `chat_config_accepts_in_range_temperature` (0.3 → Ok, `default_temperature == 0.3`).

**Verify**: `cargo test --lib api_v1::tests::chat_config_` → pass.

### Step 4: Deterministic paging order

`store.rs:551`: `ORDER BY started_at DESC, id DESC`. Change the paging test fixture (`store.rs:849-875`) so five of the 25 rows share one `started_at`, and keep the assertion that the union of pages is 25 distinct ids with no overlap.

**Verify**: `cargo test --lib sessions::store::tests` → pass. Then temporarily revert the `, id DESC` and confirm the tied-fixture test **fails** (mutation check), then restore.

### Step 5: Do not resurrect a deleted session

Add to `SessionStore`:

```rust
    /// Whether a session row exists. Used by the API to refuse to re-create a
    /// session that was deleted while a turn was in flight.
    pub fn session_exists(&self, id: &str) -> Result<bool> {
        Ok(self.get_session(id)?.is_some())
    }
```

In `api_v1.rs`, in both handlers, capture `let session_existed_at_start = !prior.is_empty();` right after `load_session_history`. At each persist site, before `record_api_turn`:

```rust
if session_existed_at_start
    && !store.session_exists(req_session_id.as_deref().unwrap_or_default()).unwrap_or(true)
{
    tracing::warn!(session_id = ?req_session_id, "session deleted mid-turn; not persisting");
    // leave `session_id` empty in the response/`done` event
} else {
    match store.record_api_turn(...) { ... }
}
```

(For the sync path the variable is `body.session_id`; for the stream it is `req_session_id` — both already in scope.) The `done` event then carries `"session_id": ""`, which the console treats as "no session" (plan 227 also disables Delete while streaming).

Test (handler-level, `ENV_LOCK` + `HomeGuard`): `stream_does_not_recreate_a_session_deleted_mid_turn` — seed a session via `record_api_turn`, call `sessions_delete` for it, then call `record`-path logic… Simplest deterministic form: unit-test the guard by extracting it into `fn should_persist(existed_at_start: bool, exists_now: bool) -> bool` and asserting the four combinations; plus a store test `session_exists_reports_deleted_rows_as_absent`.

**Verify**: `cargo test --lib api_v1::tests::should_persist` and `cargo test --lib sessions::store::tests::session_exists` → pass.

### Step 6: Arm the cancel guard before the stream; wire the timeout

1. In `agent_chat_stream`, move the guard construction above the `stream!`:
   ```rust
   let cancel_guard = CancelOnDrop(cancel_for_stream);
   let stream = async_stream::stream! {
       // Owned by the generator from construction, so dropping an unpolled
       // stream (client gone before first poll) still cancels the turn.
       let _cancel_on_drop = cancel_guard;
       let mut buffered_text = String::new();
   ```
   (Referencing `cancel_guard` inside the `stream!` block moves it into the generator's captured environment at construction time.)
   Test: `dropping_an_unpolled_chat_stream_cancels_the_turn` — construct a `CancellationToken`, build a minimal `async_stream::stream! { let _g = CancelOnDrop(tok.clone()); yield 1; }`-style stream in the test (or call a small helper extracted from the handler), drop it without polling, assert `tok.is_cancelled()`.
2. In `src/gateway/mod.rs` where the layers are applied (~840-844), replace `REQUEST_TIMEOUT_SECS` with `config.gateway.request_timeout_secs.max(5)` (the `Config` is in scope in `build_app`; confirm the variable name). Keep the `pub const REQUEST_TIMEOUT_SECS` only if something else reads it (`rtk proxy grep -rn REQUEST_TIMEOUT_SECS src/`); otherwise delete it. Update the doc comment on the field in `schema.rs:972-976` to say it applies to `/api/v1`, `/api/v1/config`, `/api/v1/cron` (the merged routers this layer wraps — confirm from the comment at `mod.rs:832-836`) and **not** to SSE bodies.
3. `docs/reference/config.md`: add `request_timeout_secs = 300` under `[gateway]` with one sentence: "Response deadline for `/api/v1/*` requests. Streaming chat is not cut by it; the sync `POST /api/v1/agent/chat` is — prefer streaming for long tool-using turns."

**Verify**: `cargo test --lib api_v1::tests::dropping_an_unpolled` → pass; `rtk proxy grep -rn "request_timeout_secs" src/gateway/mod.rs` → 1 match.

### Step 7: Insights in SQL

Add to `SessionStore`:

```rust
pub struct SessionStats { pub total_sessions: usize, pub total_messages: i64, pub latest_session_id: Option<String>, pub latest_session_started_at: Option<i64> }

pub fn stats(&self) -> Result<SessionStats> {
    let (total_sessions, total_messages): (i64, i64) = self.conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(message_count), 0) FROM sessions", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let latest = self.conn.query_row(
        "SELECT id, started_at FROM sessions ORDER BY started_at DESC, id DESC LIMIT 1", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).optional()?;
    Ok(SessionStats { total_sessions: usize::try_from(total_sessions).unwrap_or(0), total_messages, latest_session_id: latest.as_ref().map(|l| l.0.clone()), latest_session_started_at: latest.map(|l| l.1) })
}
```

(`rusqlite::OptionalExtension` for `.optional()`.) Use it in `api_v1::insights` (same JSON keys) and `sessions::cli::insights`. Replace the three byte slices in `cli.rs` (`:37, :93, :154`) with `s.id.chars().take(8).collect::<String>()`. Update `docs/reference/api-v1.md:~374` (drop the "scans up to 10,000 sessions" sentence).

Store test: `stats_counts_all_rows_not_a_page` — insert 3 sessions with message counts 2, 4, 6 → `total_sessions == 3`, `total_messages == 12`, latest is the newest `started_at`.

**Verify**: `cargo test --lib sessions::store::tests::stats_` → pass; `rtk proxy grep -n "list_sessions(10_000)" src/` → no match.

### Step 8: Handler tests (the regression net)

In `api_v1.rs` `mod tests`, all under `ENV_LOCK` + `HomeGuard` with the proof-of-pin assertion:

- `sessions_round_trip_list_get_search_title_delete`: seed two sessions via `SessionStore::open(profile.sessions_db_path()).record_api_turn(...)`; `sessions_list` → `count == 2`, `total == 2`; `sessions_get` by full id and by 8-char prefix → 200; `sessions_search` with body `{"query": "\"", "limit": 5}` → **200** with `count == 0`; `sessions_set_title` → 200 and `sessions_get` reflects it; `sessions_delete` → `deleted: true`; `sessions_get` → 404.
- `agent_chat_requires_auth_when_pairing_enabled` and `resolve_approval_requires_auth_when_pairing_enabled` (if plan 220 already added them, skip — check with grep).

**Verify**: `cargo test --lib api_v1::tests::sessions_round_trip` → pass; `cargo test --lib` → all pass.

### Step 9: Format, lint, full suite

`cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --lib`.

## Test plan

Named per step above. Structural models: `store.rs:849-875` (paging), `api_v1.rs:2290-2315` (env pin), `api_v1.rs:2524-2532` (401). Run the mutation check in step 4.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib` exits 0 with the new tests from steps 1–8 present and passing
- [ ] `rtk proxy grep -n "MATCH ?1" src/sessions/store.rs` still matches (the SQL is unchanged; only the bound value is quoted)
- [ ] `rtk proxy grep -n "list_sessions(10_000)" src/` returns nothing
- [ ] `rtk proxy grep -rn "request_timeout_secs" src/gateway/mod.rs` returns exactly one match
- [ ] `rtk proxy grep -n "ORDER BY started_at DESC, id DESC" src/sessions/store.rs` returns at least one match
- [ ] `rtk proxy grep -n "split_session" src/sessions/store.rs` still matches (not deleted)
- [ ] No files outside the in-scope list modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

- The code at the cited locations does not match the excerpts.
- `agent_chat_stream` does not have access to a `Config` value that can carry `request_timeout_secs` at the layer site in `build_app` — report which struct is available rather than adding a new global.
- The `TimeoutLayer` turns out to wrap the SSE body too on the pinned `tower-http` version (check `Cargo.lock`; the audit read 0.6.8 where it does not) — report, do not raise the default further.
- A store test needs `SessionStore::open_in_memory()` and it does not exist under that name — find the in-memory constructor the existing tests use (`store.rs` `mod tests`) and use that; do not add a second one.
- A step's verification fails twice after a reasonable fix attempt.

## Maintenance notes

- `fts_literal_query` deliberately removes operator support from search. If operators are wanted later, add an explicit `mode: "raw"` body field — never re-bind raw input.
- `redact_profile_paths` runs on every 500; it is cheap (two `replace`s) but any new absolute path source (e.g. a KB documents dir) should be added there.
- Plan 225 will replace `open_session_store()` with an `AppState` handle; the `session_exists` guard from step 5 keeps working unchanged.
- Reviewer focus: the mutation check in step 4; that `chat_config_from_body` errors are returned *before* any agent is constructed in both handlers.
