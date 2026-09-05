# Plan 027: Cron HTTP API (`/api/v1/cron*`) — the web-UI bridge

> **Context**: The claw-ui console ships a Schedules panel that calls
> `GET/POST/PUT/DELETE /api/v1/cron*` + `/cron/{id}/run`, but the gateway serves
> **none** of those routes (`src/gateway/api_v1.rs`, `config_api.rs`, and the base
> router in `mod.rs` have zero cron endpoints), so every call 404s and the panel
> is non-functional. This plan adds the missing HTTP surface so the UI (plan 028)
> can work. Cron jobs live in the per-profile sqlite store, independent of
> `config.toml`, so these handlers are thin, auth-gated wrappers over the existing
> `crate::cron` store/scheduler functions.
>
> **Executor note**: Self-contained. Verification baseline —
> `cargo fmt --all -- --check` · `cargo clippy --all-targets -- -D warnings` ·
> `cargo test`. Integration tests use the existing `build_gateway_router` seam +
> `spawn_test_gateway` harness (see `tests/config_api.rs`).
>
> **Depends on**: plan 026 Task 5 (`cron::scheduler::run_job_manual` must be
> `pub`). If 026 is not yet merged, land it first or cherry-pick Task 5.
> **Branch**: `feat/cron-http-api`. **Risk**: HIGH (gateway — high blast radius).
> **Exposure decision (approved)**: the HTTP create endpoint exposes BOTH agent
> and shell jobs. Shell jobs run `sh -lc` on the host, so the create/update
> handlers **security-check the command** (`is_command_allowed`) before
> persisting — stricter than CLI `cron add`, appropriate for the HTTP surface.
> All routes are `check_auth`-gated (bearer token when pairing is required),
> exactly like the rest of `/api/v1`.
> **Schema**: NO config-schema change (routes only).

## Baseline evidence (confirmed against main, 2026-07-18)

- `api_v1::router()` (`src/gateway/api_v1.rs:33-63`) lists 20 routes; none are
  cron. `config_api::router()` (`src/gateway/config_api.rs:33-45`) — none.
  The base gateway router (`src/gateway/mod.rs:692-722`) exposes
  health/webhooks/`/tasks*` only. `/tasks` is a separate kanban system, NOT cron.
- Frontend expectations (claw-ui `src/lib/api.ts:116-128`, via the
  `/api/rc/<path>` → `<gateway>/api/v1/<path>` proxy in
  `src/app/api/rc/[...path]/route.ts`):
  - `GET  cron`            → `{ jobs: CronJob[]; count: number }`
  - `POST cron`           body `{ schedule, prompt, name?, model? }` → `CronJob`
  - `PUT  cron/{id}`      body `{ enabled?, name?, prompt?, model?, schedule? }` → `CronJob`
  - `DELETE cron/{id}`    → `{ id, deleted }`
  - `POST cron/{id}/run` → `{ id, success, output }`
- The frontend `CronSchedule` wire format (`{kind:"cron",expr,tz?}` /
  `{kind:"at",at}` / `{kind:"every",every_ms}`) is byte-identical to the backend
  `Schedule` serde enum (`src/cron/types.rs:61-75`) — so request bodies
  deserialize straight into `Schedule` with no translation.
- Store/scheduler building blocks already exist and are `pub`:
  `cron::list_jobs / get_job / add_agent_job / add_shell_job / update_job /
  remove_job / list_runs` (`src/cron/store.rs`), `CronJobPatch`
  (`src/cron/types.rs:136-147`), `cron::scheduler::run_job_manual` (plan 026).
- Handler template: `src/gateway/config_api.rs` — `router()`, a local `check_auth`,
  `type ApiError = (StatusCode, Json<serde_json::Value>)`, `err_400`/`err_500`,
  `State(state): State<AppState>` with `state.config.lock()` +
  `state.pairing.require_pairing()/is_authenticated()`.
- `AppState` (`src/gateway/mod.rs:371`) holds `config: Arc<Mutex<Config>>` and
  `pairing: Arc<PairingGuard>`. `build_gateway_router` (`mod.rs:424`) merges
  `api_v1::router()` + `config_api::router()` at `mod.rs:722-723`.
- Test harness: `tests/config_api.rs` (`spawn_test_gateway` binds `127.0.0.1:0`,
  `test_config` sets `require_pairing=true` + a `TEST_TOKEN`).

## Scope
- **In**: new `src/gateway/cron_api.rs`; `src/gateway/mod.rs` (`pub mod cron_api;`
  + `.merge(cron_api::router())`); new `tests/cron_api.rs`; doc note in
  `docs/reference/commands.md` (or `docs/reference/config.md`) that cron is now
  HTTP-controllable.
- **Out**: `src/cron/*` behavior (consumed as-is; fixes are plan 026), any config
  schema change, the frontend (plan 028).

**API contract this plan commits to (028 consumes it):**

| Method + path | Request | Success response |
|---|---|---|
| `GET /api/v1/cron` | — | `{ "jobs": CronJob[], "count": n }` |
| `POST /api/v1/cron` | `{ schedule, job_type?, prompt?, command?, name?, model?, session_target?, delivery?, delete_after_run? }` | `CronJob` (200) |
| `PUT /api/v1/cron/{id}` | partial: any of `{ enabled, name, prompt, command, model, schedule, session_target, delivery, delete_after_run }` | `CronJob` (200) |
| `DELETE /api/v1/cron/{id}` | — | `{ "id": id, "deleted": true }` |
| `POST /api/v1/cron/{id}/run?approved=<bool>` | — | `{ "id": id, "success": bool, "output": string }` |
| `GET /api/v1/cron/{id}/runs?limit=<n>` | — | `{ "runs": CronRun[], "count": n }` |

`CronJob` = the full serde serialization of `crate::cron::CronJob`
(`src/cron/types.rs:104-123`): `id`, `expression`, `schedule`, `job_type`,
`prompt`, `command`, `session_target`, `model`, `enabled`, `delivery`,
`delete_after_run`, `created_at`, `next_run` (RFC3339), `last_run`,
`last_status`, `last_output`.
`CronRun` = `crate::cron::CronRun` (`types.rs:125-134`). Not-found → 404;
validation error → 400; unexpected → 500.

---

## Task 1 — Create `cron_api.rs` with the router + auth/error scaffolding

**Files:** Create `src/gateway/cron_api.rs`; modify `src/gateway/mod.rs`.

- [ ] **Step 1 — Create the module skeleton** `src/gateway/cron_api.rs`:

```rust
//! Cron/schedule API (`/api/v1/cron*`) — lets the web console list, create,
//! edit, delete, force-run, and inspect the run history of scheduled jobs.
//!
//! Auth mirrors the rest of `/api/v1`: when the gateway requires pairing, every
//! endpoint needs `Authorization: Bearer <token>`.
//!
//! Cron jobs live in the per-profile sqlite store (`workspace_dir/cron/jobs.db`),
//! NOT in `config.toml`, so these handlers do not touch the config write lock.
//! They clone the running `Config` (for `workspace_dir` + `autonomy`) and call
//! the synchronous `crate::cron` store functions inside `spawn_blocking`
//! (rusqlite is blocking).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{delete, get, post, put},
    Router,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::cron::{self, CronJobPatch, DeliveryConfig, JobType, Schedule, SessionTarget};
use crate::security::SecurityPolicy;

/// Build the `/api/v1/cron*` router. Merged alongside `api_v1::router()` so it
/// shares the small-body limit + timeout middleware.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/cron", get(list_cron).post(create_cron))
        .route("/api/v1/cron/{id}", put(update_cron).delete(delete_cron))
        .route("/api/v1/cron/{id}/run", post(run_cron))
        .route("/api/v1/cron/{id}/runs", get(list_cron_runs))
}

type ApiError = (StatusCode, Json<serde_json::Value>);

// NOTE: `check_auth`/`err_*` duplicate `api_v1.rs` + `config_api.rs`. This is the
// established per-module pattern; the third copy now justifies a shared helper —
// a low-risk follow-up (extract `pub(super) fn check_auth` in `mod.rs`), left out
// here to keep this high-risk gateway change surgical.
fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if !state.pairing.require_pairing() {
        return Ok(());
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").or_else(|| s.strip_prefix("bearer ")))
        .unwrap_or("");
    if state.pairing.is_authenticated(token) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "unauthorized",
                "detail": "Pair via POST /pair, then send `Authorization: Bearer <token>`."
            })),
        ))
    }
}

fn err_500(msg: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal_error", "detail": msg.to_string() })),
    )
}

fn err_400(msg: impl Into<String>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "bad_request", "detail": msg.into() })),
    )
}

fn err_404(msg: impl Into<String>) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not_found", "detail": msg.into() })),
    )
}

/// Map a `crate::cron` store error to 404 when it's a missing-job error, else 400.
/// (`get_job`/`update_job`/`remove_job` return `... not found` on a bad id.)
fn map_store_error(e: anyhow::Error) -> ApiError {
    let s = e.to_string();
    if s.contains("not found") {
        err_404(s)
    } else {
        err_400(s)
    }
}

/// Clone the running config for store/scheduler calls (workspace_dir + autonomy).
fn cfg_snapshot(state: &AppState) -> crate::config::Config {
    state.config.lock().clone()
}
```

- [ ] **Step 2 — Wire it into the gateway.** In `src/gateway/mod.rs`, add the
  module declaration next to the others (`mod.rs:10-14`):

```rust
pub mod cron_api;
```

  and merge its router beside `config_api` (`mod.rs:722-723`):

```rust
        .merge(api_v1::router())
        .merge(config_api::router())
        .merge(cron_api::router())
```

- [ ] **Step 3 — Compile.** `CARGO_TARGET_DIR=<shared> cargo build --lib`
  Expected: FAILS (handlers `list_cron`/`create_cron`/… not yet defined). That's
  the next tasks — proceed.

---

## Task 2 — `GET /cron` + `GET /cron/{id}/runs` (read endpoints)

**Files:** `src/gateway/cron_api.rs`.

- [ ] **Step 1 — Add the handlers:**

```rust
// ── GET /cron ────────────────────────────────────────────────────────────────
async fn list_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    let jobs = tokio::task::spawn_blocking(move || cron::list_jobs(&cfg))
        .await
        .map_err(err_500)?
        .map_err(err_500)?;
    let count = jobs.len();
    Ok(Json(json!({ "jobs": jobs, "count": count })))
}

// ── GET /cron/{id}/runs ──────────────────────────────────────────────────────
#[derive(Deserialize)]
struct RunsQuery {
    #[serde(default = "default_runs_limit")]
    limit: usize,
}
fn default_runs_limit() -> usize {
    50
}

async fn list_cron_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<RunsQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    let runs = tokio::task::spawn_blocking(move || cron::list_runs(&cfg, &id, q.limit))
        .await
        .map_err(err_500)?
        .map_err(err_500)?;
    let count = runs.len();
    Ok(Json(json!({ "runs": runs, "count": count })))
}
```

- [ ] **Step 2 — (Deferred) test** — covered by Task 5's integration tests.
  Move to Task 3.

---

## Task 3 — `POST /cron` (create; agent + shell)

**Files:** `src/gateway/cron_api.rs`.

- [ ] **Step 1 — Add the request body + kind resolver + handler:**

```rust
// ── POST /cron ───────────────────────────────────────────────────────────────
#[derive(Deserialize)]
struct CreateCronBody {
    schedule: Schedule,
    #[serde(default)]
    job_type: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    session_target: Option<String>,
    #[serde(default)]
    delivery: Option<DeliveryConfig>,
    #[serde(default)]
    delete_after_run: Option<bool>,
}

/// Resolve the job kind: an explicit `job_type` wins; otherwise infer from which
/// of `prompt` (agent) / `command` (shell) is provided.
fn resolve_job_kind(body: &CreateCronBody) -> Result<JobType, ApiError> {
    if let Some(jt) = body.job_type.as_deref() {
        return JobType::try_from(jt).map_err(err_400);
    }
    let has_prompt = body.prompt.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    let has_command = body.command.as_deref().map(str::trim).is_some_and(|s| !s.is_empty());
    match (has_prompt, has_command) {
        (true, false) => Ok(JobType::Agent),
        (false, true) => Ok(JobType::Shell),
        (true, true) => Err(err_400("provide either 'prompt' (agent) or 'command' (shell), not both")),
        (false, false) => Err(err_400("provide 'prompt' (agent job) or 'command' (shell job)")),
    }
}

async fn create_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateCronBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let kind = resolve_job_kind(&body)?;
    let cfg = cfg_snapshot(&state);

    let job = match kind {
        JobType::Agent => {
            let prompt = body
                .prompt
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| err_400("agent job requires a non-empty 'prompt'"))?;
            let target = body
                .session_target
                .as_deref()
                .map(SessionTarget::parse)
                .unwrap_or_default();
            let delete_after = body
                .delete_after_run
                .unwrap_or(matches!(body.schedule, Schedule::At { .. }));
            let (name, model, delivery, schedule) =
                (body.name.clone(), body.model.clone(), body.delivery.clone(), body.schedule.clone());
            tokio::task::spawn_blocking(move || {
                cron::add_agent_job(&cfg, name, schedule, &prompt, target, model, delivery, delete_after)
            })
            .await
            .map_err(err_500)?
            .map_err(err_400)? // validate_schedule failures → 400
        }
        JobType::Shell => {
            let command = body
                .command
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| err_400("shell job requires a non-empty 'command'"))?;
            // Exposure hardening: a shell job created over HTTP will later run as
            // `sh -lc` on the host. Security-check the command up-front (stricter
            // than CLI `cron add`, matching the HTTP surface's blast radius).
            let security = SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir);
            if !security.is_command_allowed(&command) {
                return Err(err_400(format!("command blocked by security policy: {command}")));
            }
            let (name, schedule) = (body.name.clone(), body.schedule.clone());
            tokio::task::spawn_blocking(move || cron::add_shell_job(&cfg, name, schedule, &command))
                .await
                .map_err(err_500)?
                .map_err(err_400)?
        }
    };
    Ok(Json(serde_json::to_value(job).map_err(err_500)?))
}
```

  > Shell jobs use `add_shell_job`, which (by store design) does not carry
  > `delivery`/`session_target`/`model`/`delete_after_run` — those are agent-only.
  > This matches the store; document it in 028 (shell create form omits them).

- [ ] **Step 2 — Compile.** `CARGO_TARGET_DIR=<shared> cargo build --lib`
  (still failing until Task 4 adds update/delete/run — fine.)

---

## Task 4 — `PUT /cron/{id}`, `DELETE /cron/{id}`, `POST /cron/{id}/run`

**Files:** `src/gateway/cron_api.rs`.

- [ ] **Step 1 — Add the three handlers:**

```rust
// ── PUT /cron/{id} ───────────────────────────────────────────────────────────
#[derive(Deserialize)]
struct UpdateCronBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    schedule: Option<Schedule>,
    #[serde(default)]
    session_target: Option<String>,
    #[serde(default)]
    delivery: Option<DeliveryConfig>,
    #[serde(default)]
    delete_after_run: Option<bool>,
}

async fn update_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<UpdateCronBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    // Security-check a newly supplied shell command before persisting.
    if let Some(cmd) = body.command.as_deref().filter(|s| !s.trim().is_empty()) {
        let security = SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir);
        if !security.is_command_allowed(cmd) {
            return Err(err_400(format!("command blocked by security policy: {cmd}")));
        }
    }
    let patch = CronJobPatch {
        schedule: body.schedule,
        command: body.command,
        prompt: body.prompt,
        name: body.name,
        enabled: body.enabled,
        delivery: body.delivery,
        model: body.model,
        session_target: body.session_target.as_deref().map(SessionTarget::parse),
        delete_after_run: body.delete_after_run,
    };
    let updated = tokio::task::spawn_blocking(move || cron::update_job(&cfg, &id, patch))
        .await
        .map_err(err_500)?
        .map_err(map_store_error)?; // not-found → 404, validate → 400
    Ok(Json(serde_json::to_value(updated).map_err(err_500)?))
}

// ── DELETE /cron/{id} ────────────────────────────────────────────────────────
async fn delete_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    let id_for_store = id.clone();
    let result = tokio::task::spawn_blocking(move || cron::remove_job(&cfg, &id_for_store))
        .await
        .map_err(err_500)?;
    match result {
        Ok(()) => Ok(Json(json!({ "id": id, "deleted": true }))),
        Err(e) if e.to_string().contains("not found") => Err(err_404(e.to_string())),
        Err(e) => Err(err_500(e)),
    }
}

// ── POST /cron/{id}/run ──────────────────────────────────────────────────────
#[derive(Deserialize)]
struct RunQuery {
    #[serde(default)]
    approved: bool,
}

async fn run_cron(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<RunQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_auth(&state, &headers)?;
    let cfg = cfg_snapshot(&state);
    let cfg_for_get = cfg.clone();
    let id_for_get = id.clone();
    let job = tokio::task::spawn_blocking(move || cron::get_job(&cfg_for_get, &id_for_get))
        .await
        .map_err(err_500)?
        .map_err(map_store_error)?;

    // Security/approval gate — mirror the `cron_run` tool.
    let security = SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir);
    if !security.can_act() {
        return Err(err_400("security policy: read-only mode, cannot run a cron job"));
    }
    if matches!(job.job_type, JobType::Shell) {
        if let Err(reason) = security.validate_command_execution(&job.command, q.approved) {
            return Err(err_400(reason));
        }
    }

    let (success, output) = cron::scheduler::run_job_manual(&cfg, &job).await;
    Ok(Json(json!({ "id": job.id, "success": success, "output": output })))
}
```

  > `POST /cron/{id}/run` takes `approved` as a **query param** (`?approved=true`),
  > not a JSON body — the frontend posts an empty body, and axum's `Json`
  > extractor rejects an empty body. Supervised medium-risk shell jobs need
  > `?approved=true` (028 surfaces this).

- [ ] **Step 2 — Compile clean.**
  `CARGO_TARGET_DIR=<shared> cargo build --lib` → Expected: PASS.
  `cargo clippy --lib -- -D warnings` on the changed file → clean.

- [ ] **Step 3 — Commit (handlers).**
  `git add -A && git commit -m "feat(gateway): add /api/v1/cron* HTTP API (list/create/update/delete/run/runs)"`

---

## Task 5 — Integration tests + unit tests

**Files:** Create `tests/cron_api.rs`; unit tests inline in `cron_api.rs`.

- [ ] **Step 1 — Unit-test the pure helpers** (inline `#[cfg(test)] mod tests` in
  `cron_api.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn body(prompt: Option<&str>, command: Option<&str>, job_type: Option<&str>) -> CreateCronBody {
        CreateCronBody {
            schedule: Schedule::Every { every_ms: 60_000 },
            job_type: job_type.map(Into::into),
            prompt: prompt.map(Into::into),
            command: command.map(Into::into),
            name: None,
            model: None,
            session_target: None,
            delivery: None,
            delete_after_run: None,
        }
    }

    #[test]
    fn resolve_job_kind_infers_from_fields() {
        assert_eq!(resolve_job_kind(&body(Some("hi"), None, None)).unwrap(), JobType::Agent);
        assert_eq!(resolve_job_kind(&body(None, Some("echo hi"), None)).unwrap(), JobType::Shell);
    }

    #[test]
    fn resolve_job_kind_rejects_both_and_neither() {
        assert!(resolve_job_kind(&body(Some("hi"), Some("echo"), None)).is_err());
        assert!(resolve_job_kind(&body(None, None, None)).is_err());
    }

    #[test]
    fn resolve_job_kind_honors_explicit_job_type() {
        assert_eq!(resolve_job_kind(&body(None, Some("x"), Some("agent"))).unwrap(), JobType::Agent);
        assert!(resolve_job_kind(&body(None, None, Some("nonsense"))).is_err());
    }
}
```

- [ ] **Step 2 — Integration tests** — create `tests/cron_api.rs`, copying the
  `spawn_test_gateway` + `test_config` + `TEST_TOKEN` harness from
  `tests/config_api.rs:19-61` verbatim (same imports), then:

```rust
#[tokio::test]
async fn cron_requires_auth() {
    let ws = tempfile::tempdir().unwrap();
    let base = spawn_test_gateway(test_config(ws.path())).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/v1/cron"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cron_list_create_delete_roundtrip() {
    let ws = tempfile::tempdir().unwrap();
    let base = spawn_test_gateway(test_config(ws.path())).await;
    let client = reqwest::Client::new();

    // Empty to start.
    let list: serde_json::Value = client
        .get(format!("{base}/api/v1/cron"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["count"], 0);

    // Create an agent job.
    let created: serde_json::Value = client
        .post(format!("{base}/api/v1/cron"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "schedule": { "kind": "cron", "expr": "0 9 * * *" },
            "prompt": "Good morning"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["job_type"], "agent");

    // It shows up in the list.
    let list: serde_json::Value = client
        .get(format!("{base}/api/v1/cron"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["count"], 1);

    // Disable via PUT.
    let updated: serde_json::Value = client
        .put(format!("{base}/api/v1/cron/{id}"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["enabled"], false);

    // Delete.
    let del: serde_json::Value = client
        .delete(format!("{base}/api/v1/cron/{id}"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del["deleted"], true);
}

#[tokio::test]
async fn cron_get_missing_job_runs_returns_empty() {
    let ws = tempfile::tempdir().unwrap();
    let base = spawn_test_gateway(test_config(ws.path())).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/api/v1/cron/does-not-exist/runs"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn cron_update_missing_job_returns_404() {
    let ws = tempfile::tempdir().unwrap();
    let base = spawn_test_gateway(test_config(ws.path())).await;
    let resp = reqwest::Client::new()
        .put(format!("{base}/api/v1/cron/nope"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}
```

- [ ] **Step 3 — Run.** `CARGO_TARGET_DIR=<shared> cargo test --test cron_api`
  and `cargo test --lib gateway::cron_api`. Expected: all PASS.

- [ ] **Step 4 — Docs.** Add a short subsection to `docs/reference/commands.md`
  (or the API reference from plan 019 if merged) documenting the 6 cron endpoints
  + the exposure note (auth-gated; shell commands security-checked). One
  paragraph; do not duplicate the table above verbatim — link behavior to the
  CLI/tool docs.

- [ ] **Step 5 — Commit.**
  `git add -A && git commit -m "test(gateway): cover /api/v1/cron* auth, CRUD roundtrip, and 404s"`

---

## Done criteria (all must hold)
- [ ] All 6 endpoints served + `check_auth`-gated; unauth → 401.
- [ ] Create supports agent AND shell; shell commands are `is_command_allowed`-checked.
- [ ] `POST /cron/{id}/run` reuses `cron::scheduler::run_job_manual` (plan 026).
- [ ] Missing-job update/run/delete → 404; validation → 400.
- [ ] `cargo test --test cron_api` + `cargo test --lib gateway::cron_api` green.
- [ ] `cargo fmt` clean; `cargo clippy --all-targets -- -D warnings` clean on
  changed files. Run `cargo test --lib` before merge (2 crate roots — a new
  inline `#[cfg(test)] mod` in a `src/` file compiles under `--lib`; the
  `tests/cron_api.rs` file compiles as a separate integration crate).
- [ ] No config-schema change.

## STOP conditions
- If `build_gateway_router` is NOT present on the branch base (it is on main at
  `mod.rs:424`; plan 013 added it) — stop; the integration harness depends on it.
- If `validate_command_execution` or `is_command_allowed` signatures differ from
  what `src/tools/cron_run.rs:103` / `src/cron/mod.rs:133` use — stop and match
  the real signature (do not guess).
- If returning full `CronJob` (incl. up-to-16KB `last_output` per job) over
  `GET /cron` is judged too heavy for large job counts — note it and propose a
  trimmed list view as a follow-up; do NOT silently drop fields the frontend
  (028) relies on.

## Rollback
New file + one merge line + one test file. Revert the merge line in `mod.rs` to
fully disable the surface (routes disappear, 404 returns — the pre-plan state).
No persisted-state or schema change.
