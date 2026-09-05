# Plan 174: Warn (do NOT block) when a cron shell job is created with a command the fire-time gate will refuse

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **RE-SCOPED (2026-08-19).** The original plan made HTTP/CLI creation *gate* a
> shell job on the full `validate_command_execution` (allowlist + risk). That was
> implemented and then **reverted** — CI proved it breaks a deliberately-pinned
> contract: `tests/cron_api.rs::cron_run_honours_an_operator_supplied_approval`
> requires that a medium/high-risk **allowlisted** shell job be *creatable* over
> HTTP so it can be force-run later with `POST /cron/{id}/run?approved=true`.
> Gating at create makes that job impossible to create, killing the
> operator-approval force-run path. This plan therefore does the honest fix
> **without** the gate: an **advisory warning** at create time. Firing behavior
> and what-can-be-created are unchanged.
>
> **Drift check (run first)**:
> `git diff --stat 434141c..HEAD -- src/gateway/cron_api.rs src/cron/mod.rs src/cron/scheduler.rs`
> If any in-scope file changed since this plan was written, re-locate the named
> functions and compare against live code; on a mismatch, treat it as a STOP.

## Status

- **Priority**: P2 (was P1; downgraded — this is now an advisory-only UX fix, not a gate)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: usability / honesty
- **Planned at**: commit `434141c`, 2026-08-19 (re-scoped to advisory)

## Why this matters

A cron **shell** job created over HTTP or CLI is checked only against the
allowlist (`is_command_allowed`) at create time, while the scheduler applies the
full gate (`validate_command_execution(cmd, false)` — allowlist **plus** risk
classification) at fire time. This is **not** an execution bypass (the scheduler
fails closed), and it is **not** something to tighten at create (see the pinned
force-run contract above). The only real defect is **honesty**: an operator can
create an allowlisted-but-medium/high-risk shell job from the console/CLI, get a
200/success, and the job then silently errors on its first *scheduled* fire — with
no signal at create time that it will only ever run via an explicit approved
force-run.

The fix is a one-line-ish **advisory**: at create, if the command would be
refused by the fire-time gate, still create the job (unchanged), but tell the
caller it will not run on its schedule without allowlist+low-risk or an approved
force-run. No gate. No behavior change to what is created or fired.

## Current state

- `src/gateway/cron_api.rs` — `create_cron` shell branch: keeps its existing
  `SecurityPolicy::from_config(...)` + `is_command_allowed(&command)` allowlist
  check (returns 400 on failure). Persists via `spawn_blocking(add_shell_job(...))`
  and returns `Json(json!({...the job...}))`. (Re-locate; the handler shape has
  shifted since earlier drift.)
- `src/cron/mod.rs` — the CLI `Update` handler shell branch keeps its
  `is_command_allowed` + `bail!` allowlist check, then prints an `Updated cron
  job` confirmation.
- `src/cron/scheduler.rs` — `run_job_command_with_timeout` applies
  `validate_command_execution(&job.command, false)` at fire time; on refusal
  returns `(false, "blocked by security policy: ...")`. This is the gate whose
  outcome the advisory predicts.
- `SecurityPolicy::validate_command_execution(cmd, approved) -> Result<CommandRiskLevel, String>`
  — `Err(String)` is the human reason (allowlist / risk / approval-required).

`is_command_allowed` at create stays — it is a cheap allowlist sanity check and
the pinned `touch` test depends on it passing. The advisory is layered on top; it
predicts the *risk* half that the allowlist check does not cover.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format  | `cargo fmt --all -- --check` | exit 0 |
| Lint    | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Cron    | `cargo test --lib cron` | all pass |
| Gateway | `cargo test --test cron_api` | all pass (esp. `cron_run_honours_an_operator_supplied_approval`) |

Do NOT run a bare `cargo test`.

## Scope

**In scope**:
- `src/gateway/cron_api.rs` — `create_cron` shell branch: add a `warning` field to
  the success response when the fire-time gate would refuse the command.
- `src/cron/mod.rs` — CLI shell create/update: print an advisory line in the same case.
- One small shared predicate (e.g. `pub(crate) fn command_will_be_refused_at_fire(config, cmd) -> Option<String>` returning the reason) so the two surfaces agree.

**Out of scope** (do NOT touch):
- The `is_command_allowed` allowlist checks — keep them; do NOT replace with the
  risk gate (that is the reverted change that broke the pinned test).
- `run_cron` (`?approved=true` force-run) and the fire-time gate — unchanged.
- `add_shell_job` / persistence — unchanged (the job is still created exactly as before).
- The pinned `tests/cron_api.rs::cron_run_honours_an_operator_supplied_approval`
  — it MUST still pass unchanged (create succeeds, force-run with approval runs).

## Steps

### Step 1: Add the shared predicate

Add a small helper (e.g. in `src/cron/mod.rs`, `pub(crate)`):

```rust
/// If a shell command would be refused by the scheduler's fire-time gate
/// (allowlist + risk classification, no approval), return the human reason.
/// `None` means it will run on schedule. Advisory only — callers must NOT block
/// creation on this; the operator can still create it and force-run it with an
/// explicit approval.
pub(crate) fn command_will_be_refused_at_fire(config: &Config, command: &str) -> Option<String> {
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
    security.validate_command_execution(command, false).err()
}
```

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: HTTP `create_cron` — attach an advisory `warning`

In the shell branch of `create_cron`, after the existing allowlist check and after
the job is built, compute the advisory and include it in the JSON response body
(job unchanged):

```rust
    let warning = crate::cron::command_will_be_refused_at_fire(&cfg, &command);
    // ... existing spawn_blocking(add_shell_job(...)) → job ...
    let mut body = serde_json::to_value(&job).map_err(err_500)?;
    if let (Some(obj), Some(reason)) = (body.as_object_mut(), warning) {
        obj.insert("warning".into(), json!(format!(
            "created, but will not run on its schedule ({reason}); force-run with an approval, or allowlist a low-risk command"
        )));
    }
    Ok(Json(body))
```

Keep the response shape otherwise identical (the job fields stay top-level; the
`warning` key is additive and optional). Do NOT change the status code — it stays
success; the job really was created.

**Verify**: `cargo test --test cron_api` → all pass, including the pinned approval test.

### Step 3: CLI shell create/update — print the advisory

In `src/cron/mod.rs`, the shell create (`cron add`) and `cron update` handlers,
after the existing confirmation `println!`, add:

```rust
    if let Some(reason) = command_will_be_refused_at_fire(config, &command) {
        println!("  \u{26a0} Note: this command will not run on its schedule ({reason}). Force-run it with an approval, or use an allowlisted low-risk command.");
    }
```

(Adapt `command` to whatever the handler holds; only emit when a shell command is
present.)

**Verify**: `cargo test --lib cron` → pass; `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Tests

- `src/cron/mod.rs::tests` (or `cron_api` integration): a unit test for
  `command_will_be_refused_at_fire` — an allowlisted-but-high-risk command (e.g.
  allowlist `curl`, Supervised) returns `Some(reason)`; an allowlisted low-risk
  command (e.g. `echo`) returns `None`. **Mutation check**: swap `.err()` for
  `.ok().map(|_| String::new())` (or invert) and confirm the test fails; restore.
- Confirm `cron_run_honours_an_operator_supplied_approval` still passes (create
  succeeds, force-run with approval runs) — the advisory must not change it.

**Verify**: `cargo test --lib cron` and `cargo test --test cron_api` → pass.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` and `cargo test --test cron_api` pass, incl. the
      pinned `cron_run_honours_an_operator_supplied_approval` (UNCHANGED)
- [ ] `create_cron` still returns success and still creates the job for a
      medium/high-risk allowlisted command; only an additive `warning` is attached
- [ ] `is_command_allowed` create-time checks are still present (no risk gate at create)
- [ ] `command_will_be_refused_at_fire` has a mutation-checked test
- [ ] Only in-scope files modified; `plans/README.md` status row updated

## STOP conditions

Stop and report if:
- Making the advisory work appears to require *gating* creation (returning an
  error / 4xx) — it must not; that is the reverted change. Advisory only.
- The pinned `cron_run_honours_an_operator_supplied_approval` test fails — you
  have changed create/force-run behavior; back out.
- The named handlers no longer match (drift since 434141c).

## Maintenance notes

- This is intentionally advisory, not a gate: the create-then-approved-force-run
  workflow is a pinned product contract. If a future change wants to *refuse*
  risky shell jobs at create, it must first remove/renegotiate that contract
  (the pinned test) — a product decision, not a refactor.
- Alternative if the team decides the advisory is not worth the surface area:
  **drop this plan entirely.** The scheduler already fails closed and records the
  refusal in run history (per plan 185's per-attempt rows), so nothing unsafe
  happens without it — it only improves the create-time signal.
