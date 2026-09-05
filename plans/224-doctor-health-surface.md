# Plan 224: Make the doctor/health surface honest and non-blocking — no blocking work on the async runtime, no hidden skipped checks, no unauthenticated pid/error leak

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 8503328..HEAD -- src/doctor/ src/gateway/api_v1.rs src/gateway/mod.rs src/health/mod.rs docs/reference/api-v1.md`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW — the check semantics are unchanged; the work is moved off the async runtime, a `skipped` list is added, and `/health` drops two fields.
- **Depends on**: none
- **Category**: bug (perf + data-minimization)
- **Planned at**: commit `8503328`, 2026-08-24
- **Branch**: `fix/doctor-health-honesty`
- **One PR**: commit per step.

## Why this matters

`GET /api/v1/doctor` — polled by the console's Status panel and re-run on every Refresh click — does blocking, side-effecting work directly on a tokio worker: it spawns `systemctl --user status rantaiclaw`, runs seven `which` PATH scans, and **creates/writes/deletes a probe file in the workspace** on every request, all sequentially. A GET that is documented as "safe to poll … without side effects" spawns a subprocess and mutates the filesystem, and it blocks a worker thread that other SSE chat streams share. It also silently drops the three "live" checks in brief mode with no marker, so the panel reports all-green on a gateway whose provider key is rejected. Separately, `/health` and `/readyz` are unauthenticated yet return the process `pid` and the verbatim `last_error` string of every supervised component.

## Current state

### Blocking work — `src/doctor/checks/daemon.rs:82-88`, `src/doctor/checks/config.rs:202-215`, `src/doctor/mod.rs:150-158`

```rust
        if which::which("systemctl").is_ok() {
            let out = std::process::Command::new("systemctl")
                .args(["--user", "status", "rantaiclaw"]).output();
```
```rust
fn writable_probe(dir: &Path) -> std::io::Result<()> {
    let probe = dir.join(format!(".doctor_probe_{}_{}", std::process::id(), nanos));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
    f.write_all(b"probe")?; drop(f);
    let _ = std::fs::remove_file(&probe);
    Ok(())
}
```
```rust
    for check in &registry {
        if brief && check.category() == "live" { continue; }
        results.push(run_one(check.as_ref(), &ctx).await);   // sequential
    }
```

The registry (`mod.rs:139-149`) has ten checks; the three `live` ones are `provider.ping`, `channels.auth`, `mcp.startup`.

### Handler — `src/gateway/api_v1.rs:349-376`

Builds the summary from `run_all(ctx, true)`; the response has only `{ "results": [...] }` — no marker that anything was skipped. `check_auth` gates it.

### `/health` and `/readyz` — `src/gateway/mod.rs:949-978` and `src/health/mod.rs:17-22, 104-112`

```rust
async fn handle_health(State(state): State<AppState>) -> impl IntoResponse {
    let body = serde_json::json!({ "status": "ok", "paired": state.pairing.is_paired(), "runtime": crate::health::snapshot_json() });
    Json(body)
}
```
```rust
pub struct HealthSnapshot { pub pid: u32, pub updated_at: String, pub uptime_seconds: u64, pub components: BTreeMap<String, ComponentHealth> }
```

`ComponentHealth.last_error` (`:14`) is `error.to_string()` verbatim (`mark_component_error`, `:89-94`). Both handlers' doc comments claim "Public, no secrets leaked". The gateway binds loopback by default and refuses a public bind without a tunnel or `allow_public_bind` (`mod.rs:862-871`), so this is an exposure risk only for tunnel/`allow_public_bind`/container deployments — but the "no secrets" claim is unenforced regardless.

### Docs — `docs/reference/api-v1.md:167-169`

"This endpoint always runs in **offline/brief mode** … no live network probes, so it is safe to poll from a console without side effects." — contradicted by the probe file and the subprocess.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Doctor tests | `cargo test --test doctor_checks` | pass |
| Handler tests | `cargo test --lib api_v1::tests` | pass |
| Full lib | `cargo test --lib` | pass |
| Never | bare `cargo test` | — disk-constrained |

## Scope

**In scope**:
- `src/doctor/checks/daemon.rs`, `src/doctor/checks/config.rs` (wrap blocking work)
- `src/doctor/mod.rs` (parallel run + return skipped names)
- `src/gateway/api_v1.rs` (`doctor` handler: `skipped` array; `spawn_blocking` if any blocking remains at the handler)
- `src/gateway/mod.rs` (`/health`, `/readyz` payload split)
- `src/health/mod.rs` (a public/minimal snapshot method)
- `docs/reference/api-v1.md` (doctor side-effects + skipped)

**Out of scope**:
- The check logic itself (what each check verifies) — unchanged.
- Live-checks-on-demand (`?live=1`) — a follow-up; this plan only *labels* what brief mode skips, it does not add a way to run them from the browser.
- `/metrics`, `/pair`, `/login` — untouched.
- `src/doctor/legacy.rs` and `report.rs` — CLI-only, not reached by the API.

## Git workflow

- Branch: `fix/doctor-health-honesty`.
- Commits: `fix(doctor): run blocking checks off the async runtime`, `fix(doctor): run checks concurrently and report which were skipped`, `fix(gateway): stop /health leaking pid and raw component errors to unauthenticated callers`, `docs(api): state doctor's real side effects and skipped checks`.
- No `Co-Authored-By: Claude`. Do not push/PR unless instructed.

## Steps

### Step 1: Move blocking work off the async runtime

In `src/doctor/checks/daemon.rs` `DaemonRegistrationCheck::run`, wrap the `which::which` + `std::process::Command` block in `tokio::task::spawn_blocking(move || { … }).await?`. Same for `src/doctor/checks/system_deps.rs` if it does synchronous `which` scans (`:73-78`) and for `writable_probe` in `config.rs` (`PathsCheck::run` at `:179-198`) — wrap the `writable_probe(ws)` call in `spawn_blocking`. Keep the probe file logic itself; only its thread changes.

**Verify**: `cargo test --test doctor_checks` → existing tests pass (they don't assert threading, but must still pass); `cargo clippy --all-targets -- -D warnings` → 0.

### Step 2: Run checks concurrently and report skipped

In `src/doctor/mod.rs::run_all`, replace the sequential loop with a parallel run of the non-skipped checks and collect the skipped names:

```rust
pub struct DoctorRun { pub results: Vec<CheckResult>, pub skipped: Vec<String> }

pub async fn run_all(ctx: DoctorContext, brief: bool) -> Vec<CheckResult> { run_all_detailed(ctx, brief).await.results }

pub async fn run_all_detailed(ctx: DoctorContext, brief: bool) -> DoctorRun {
    let registry = /* the existing Vec<Box<dyn DoctorCheck>> */;
    let ctx = std::sync::Arc::new(ctx);
    let mut skipped = Vec::new();
    let mut futs = Vec::new();
    for check in registry { // owned iteration
        if brief && check.category() == "live" { skipped.push(check.name().to_string()); continue; }
        let ctx = ctx.clone();
        futs.push(async move { run_one(check.as_ref(), &ctx).await });
    }
    let results = futures::future::join_all(futs).await;
    DoctorRun { results, skipped }
}
```

(Keep `run_all` as the back-compat wrapper so CLI callers are unchanged. `DoctorContext` must be shareable — if it is not `Sync`, box it in `Arc` as above; if a field prevents it, STOP and report.)

**Verify**: `cargo test --test doctor_checks` → pass; add `run_all_detailed_reports_skipped_live_checks` (brief=true → `skipped` contains the three live names; brief=false → empty).

### Step 3: Handler returns `skipped`

`src/gateway/api_v1.rs::doctor`: call `run_all_detailed(ctx, true)` and add `"skipped": run.skipped` to the JSON alongside `"results"`. Since the handler no longer does blocking work directly (step 1 moved it into the checks), no `spawn_blocking` is needed at the handler; if any synchronous cost remains (e.g. `ProfileManager::active()`), leave it — it is cheap.

**Verify**: add `doctor_response_lists_skipped_live_checks` (call the handler with a `test_state()`, assert the JSON has a `skipped` array containing `provider.ping`). `cargo test --lib api_v1::tests::doctor_` → pass. Also add `doctor_requires_auth_when_pairing_enabled` if plan 221 has not already.

### Step 4: Minimal public `/health` and `/readyz`

In `src/health/mod.rs` add:

```rust
/// A public-safe view: readiness verdict + unhealthy component NAMES only.
/// No pid, no uptime, no raw error strings — those ride on the bearer-gated
/// `/api/v1/status.runtime`.
pub fn public_status_json(&self) -> serde_json::Value {
    serde_json::json!({
        "ready": self.unhealthy_components().is_empty(),
        "unhealthy_components": self.unhealthy_components(),
    })
}
```

`src/gateway/mod.rs`:
- `handle_health`: `Json(json!({ "status": "ok", "paired": state.pairing.is_paired() }))` — drop `runtime`.
- `handle_readyz`: `let snap = crate::health::snapshot(); let ready = snap.unhealthy_components().is_empty(); (code, Json(snap.public_status_json()))` — drop the full `runtime` serialization.
- Update both doc comments to say what is now returned.

The `pid`/`uptime`/`last_error` detail still reaches an authenticated operator via `/api/v1/status`'s `runtime` field (unchanged).

Optionally (same commit) redact in `mark_component_error` (`src/health/mod.rs:89-94`): run the stored error through `crate::providers::sanitize_api_error` before storing, so even the authenticated view never keeps a raw token. Cheap and defensive.

**Verify**: `health_public_status_has_no_pid` (build a snapshot, assert `public_status_json()` has no `pid`/`components` keys). `cargo test --lib health` → pass.

### Step 5: Fix the docs

`docs/reference/api-v1.md:167-169`: replace "no live network probes, so it is safe to poll from a console without side effects" with: "runs the seven non-live checks (config, provider-key presence, api-url, paths, allowlist, daemon registration, system deps). It does **not** make network calls, but it spawns `systemctl` and writes a short-lived probe file in the workspace to verify writability. The three live checks (`provider.ping`, `channels.auth`, `mcp.startup`) are listed in the response's `skipped` array." Add a `skipped` field to the documented response shape.

**Verify**: manual read; `rtk proxy grep -n "without side effects" docs/reference/api-v1.md` → nothing.

### Step 6: Format, lint, full suite

`cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --lib`, `cargo test --test doctor_checks`.

## Test plan

Named per step. Model `doctor_checks` additions on the existing `tests/doctor_checks.rs` fixtures (`:26+`). Handler test in `api_v1.rs` `mod tests` (no store access needed → no `ENV_LOCK`). Health test in `src/health/mod.rs` `mod tests` (`:132+`).

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib` and `cargo test --test doctor_checks` exit 0 with the new tests
- [ ] `rtk proxy grep -n "spawn_blocking" src/doctor/checks/daemon.rs` returns a match
- [ ] `rtk proxy grep -n "join_all" src/doctor/mod.rs` returns a match
- [ ] `rtk proxy grep -n "skipped" src/gateway/api_v1.rs` returns a match in the doctor handler
- [ ] `rtk proxy grep -n "snapshot_json" src/gateway/mod.rs` returns nothing in `handle_health`/`handle_readyz` (they use the minimal view)
- [ ] `rtk proxy grep -n "without side effects" docs/reference/api-v1.md` returns nothing
- [ ] No files outside the in-scope list modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

- Cited excerpts do not match live code.
- `DoctorContext` cannot be shared across the concurrent futures (not `Sync`, or holds a `!Send` handle) — report which field; fall back to keeping the sequential loop but still wrapping each check's blocking calls in `spawn_blocking` (step 1) and adding the `skipped` list (steps 2–3 minus the parallelism).
- `futures` is not already a dependency (`rtk proxy grep -n "^futures" Cargo.toml`) — it is used elsewhere in the gateway; if not, use `tokio::join!`-style or leave sequential per the fallback above rather than adding a dependency.
- Removing `runtime` from `/health` breaks an orchestrator contract documented elsewhere (`rtk proxy grep -rn "health" docs/operations/`) — if `/health.runtime` is a documented interface, keep the field but move `pid`/`last_error` out of it, and report.
- A step's verification fails twice after a reasonable fix.

## Maintenance notes

- A future `?live=1` opt-in on `/doctor` should run the three skipped checks behind a *separate* rate limiter (they fan out to every provider) — do not fold them into the default path.
- If `sanitize_api_error` is added to `mark_component_error`, any component that formats a credentialed URL into its error is covered centrally; new channels get it for free.
- Reviewer focus: that no check still does synchronous subprocess/FS work on the async path; that `/health` no longer serializes the full snapshot.
