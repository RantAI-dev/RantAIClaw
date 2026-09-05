# Plan 172: Cron agent jobs carry a creator/origin record and run under the creator's capability ceiling

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/cron/types.rs src/cron/store.rs src/cron/scheduler.rs src/gateway/cron_api.rs src/agent/loop_.rs src/tools/cron_add.rs src/tui/commands/cron.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: none (coordinate with plans/173, plans/177 — see Maintenance notes)
- **Category**: security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

A scheduled **agent** cron job runs `crate::agent::run(...)` with the FULL local
tool registry and **no guest ceiling** — it executes at daemon privilege no
matter who created it. `CronJob` (`src/cron/types.rs:117-136`) records no
creator/origin, and the HTTP creation path (`create_cron`,
`src/gateway/cron_api.rs:197-266`) only proves possession of a pairing token
(`check_auth`, lines 44-67) — it never checks owner identity the way the chat
path's `build_guest_gate` does (`src/gateway/mod.rs:1537-1549`). So a paired
non-owner on the web console, or any channel guest wherever `cron_add` becomes
reachable, can persist an agent job whose later scheduled run receives the
unconstrained local toolset. This is the deferred-in-time analogue of the
`delegate` bypass documented at `src/approval/guest.rs:49-50` ("spawns a
sub-agent loop with NO guest gate ... a full bypass"). This plan records job
provenance, gates HTTP agent-job creation on owner identity, and reconstructs a
capability ceiling for the scheduled run so a non-owner-created job can no
longer run as the owner.

## Current state

Files and roles:

- `src/cron/types.rs` — `CronJob` struct (lines 117-136) and `CronJobPatch`
  (149-160). **No `created_by`/origin field today.**
- `src/cron/store.rs` — sqlite persistence. Schema + additive-migration engine
  in `with_connection` (513-573); `add_column_if_missing` helper (483-511);
  `add_shell_job` (30-65) and `add_agent_job` (68-111) INSERT sites;
  `map_cron_job_row` positional row→struct mapper (416-451); `SELECT` column
  lists in `list_jobs`/`get_job`/`due_jobs` (116-118, 134-135, 167-169).
- `src/cron/scheduler.rs` — `run_agent_job(config, security, job)` (203-263)
  calls `crate::agent::run(...)` at 240-248 with `None` as the *third* arg
  (that is `provider_override`, **not** a guest gate).
- `src/agent/loop_.rs` — `pub async fn run(config, message, provider_override,
  model_override, temperature, peripheral_overrides)` (2013-2020) has **no
  guest_gate parameter at all**; it builds the full registry via
  `all_tools_with_runtime` (2057-2071). The per-turn tool loop *does* accept a
  `guest_gate: Option<&crate::approval::GuestGate>` (see the dispatch signatures
  at lines 1224-1231 and 1464/1822), but `run()` never threads one in.
- `src/gateway/cron_api.rs` — `create_cron` (197-266): agent branch (206-242)
  does zero owner check; `check_auth` (44-67) only validates the pairing token.
- `src/gateway/mod.rs` — `build_guest_gate(state, sender)` (1537-1549): returns
  `None` for owners (`can_approve(&cc.approval_owners, sender)`), else a
  `GuestGate`. This is the owner-identity oracle the cron HTTP path lacks.
- `src/tools/cron_add.rs` — `CronAddTool` struct (9-12) holds only
  `config` + `security`; constructed at `src/tools/mod.rs:258` with no
  sender/identity. **The tool path has no creator identity available today** —
  see Step 2 note.

Key excerpts:

`src/cron/types.rs:117-136` (CronJob — no origin field):
```rust
pub struct CronJob {
    pub id: String,
    pub expression: String,
    pub schedule: Schedule,
    pub command: String,
    pub prompt: Option<String>,
    // ... name, job_type, session_target, model, enabled, delivery,
    //     delete_after_run, created_at, next_run, last_run, last_status,
    //     last_output
}
```

`src/cron/scheduler.rs:240-248` (the unconstrained scheduled run):
```rust
Box::pin(crate::agent::run(
    config.clone(),
    Some(prefixed_prompt),
    None,               // provider_override — NOT a guest gate
    model_override,
    config.default_temperature,
    vec![],
))
.await
```

`src/cron/store.rs:562-570` (the additive-migration pattern to copy):
```rust
add_column_if_missing(&conn, "schedule", "TEXT")?;
add_column_if_missing(&conn, "job_type", "TEXT NOT NULL DEFAULT 'shell'")?;
// ... one call per column added after the original schema
```

`src/gateway/mod.rs:1537-1549` (the owner-identity check to mirror):
```rust
fn build_guest_gate(state: &AppState, sender: &str) -> Option<crate::approval::GuestGate> {
    let cfg = state.config.lock();
    let cc = &cfg.channels_config;
    if crate::approval::can_approve(&cc.approval_owners, sender) {
        None
    } else {
        Some(crate::approval::GuestGate::new(
            cfg.autonomy.auto_approve.clone(),
            &cc.guest_allowed_tools,
            &cc.guest_allowed_commands,
        ))
    }
}
```

Repo conventions:
- Additive sqlite migrations use `add_column_if_missing` (never a destructive
  rewrite). New columns must be nullable or have a `DEFAULT` so legacy rows load.
- `map_cron_job_row` reads columns **by positional index**. A new column added
  to the `SELECT` lists lands at the next index; the mapper's `row.get(N)` must
  match the SELECT order exactly.
- Security posture (CLAUDE.md §3.6): exposure surfaces stay deny-by-default;
  never silently broaden who can reach an unconstrained execution primitive.

## Commands you will need

| Purpose   | Command                                              | Expected on success |
|-----------|------------------------------------------------------|---------------------|
| Format    | `cargo fmt --all -- --check`                         | exit 0, no diff     |
| Lint      | `cargo clippy --all-targets -- -D warnings`          | exit 0, no warnings |
| Tests     | `cargo test --lib cron`                              | all pass            |
| Tests     | `cargo test --lib security`                          | all pass            |
| Drift     | `git diff --stat 2aefb9f..HEAD -- src/cron src/gateway/cron_api.rs` | only your changes |

Do **not** run a bare `cargo test` (this box is disk-constrained; the full
workspace target is ~27G). Scope every test run with `--lib <filter>`.

## Scope

**In scope**:
- `src/cron/types.rs` — add `created_by` field to `CronJob`
- `src/cron/store.rs` — migration, INSERT columns, SELECT lists, row mapper,
  new `add_agent_job`/`add_shell_job` creator argument
- `src/gateway/cron_api.rs` — owner-identity gate on agent-job creation; pass
  creator into store calls
- `src/gateway/mod.rs` — reuse/extract the owner check (read-only reference)
- `src/cron/scheduler.rs` — reconstruct the ceiling for the scheduled run
  (Step 4, STOP-gated)
- `src/tools/cron_add.rs`, `src/cron/mod.rs` — pass a creator value at the
  tool/CLI creation sites
- `src/tui/commands/cron.rs` — pass `Some("tui")` at the `add_agent_job` (`:129`)
  and `add_shell_job` (`:140`) call sites (the TUI creates jobs as the local
  operator). Without this the store-signature change causes a hard `E0061` arity
  error in this file; it MUST be in scope or the "touching an out-of-scope file"
  STOP fires and the plan is non-completable.

**Out of scope**:
- Any change to `DeliveryConfig` validation — that is plan 173's job.
- Adding cron tools to `OWNER_ONLY_TOOLS` — that is plan 177's cheaper
  defense-in-depth (see Maintenance notes; this plan largely supersedes it).
- The `cron.enabled` HTTP enforcement gap — that is plan 170's job.
- Any change to the run-history redaction / DB file mode — that is plan 175.

## Git workflow

- Branch: `advisor/172-cron-agent-job-creator-scope`
- Conventional commits, one per step
  (e.g. `feat(cron): record creator on cron jobs`).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the `created_by` field and its migration

1. In `src/cron/types.rs`, add to `CronJob` (after `last_output`, keep the
   struct's derives): `pub created_by: Option<String>,`. `Option<String>` so
   legacy rows (NULL) deserialize cleanly.
2. In `src/cron/store.rs` `with_connection`, add one line alongside the other
   `add_column_if_missing` calls (after line 570):
   `add_column_if_missing(&conn, "created_by", "TEXT")?;`
   Also add `created_by TEXT` to the `CREATE TABLE IF NOT EXISTS cron_jobs`
   body (fresh installs) after `last_output TEXT` (line 542).
3. Extend every `SELECT` column list that feeds `map_cron_job_row`
   (`list_jobs` 116-118, `get_job` 134-135, `due_jobs` 167-169) by appending
   `, created_by` as the **last** selected column.
4. In `map_cron_job_row` (416-451), add `created_by: row.get(17)?,` to the
   `CronJob { ... }` literal (index 17 = the 18th column; verify it matches the
   position of `created_by` in the SELECT lists after your edit).

**Verify**: `cargo test --lib cron` → all pass (existing cron tests still map
rows correctly). `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Persist a creator at every creation site

1. Change `add_agent_job` and `add_shell_job` (`src/cron/store.rs:30`, `68`) to
   accept a new parameter `created_by: Option<&str>` and bind it into the
   `INSERT` (add `created_by` to the column list and a matching `?N` param).
2. Update **every** call site to pass a value. There is no `add_scheduled`
   *store* function — `add_scheduled` (`src/cron/mod.rs:197`) is a private mod.rs
   wrapper that itself calls the store's `add_agent_job`/`add_shell_job`. The
   store functions whose signature changes are `add_agent_job`/`add_shell_job`
   in `store.rs`; their call sites are:
   - `src/gateway/cron_api.rs` `create_cron` — pass the authenticated
     principal (see Step 3; for now pass `Some("gateway")` as a placeholder so
     it compiles, then replace in Step 3).
   - `src/cron/mod.rs` — the `add_agent_job`/`add_shell_job` call sites at lines
     `206`, `217`, `280`, `343`, `465`, `546`, `565` (`206`/`217` are inside the
     `add_scheduled` wrapper; `465`/`546`/`565` are in the `#[cfg(test)]` module).
     For the non-test/CLI sites pass `Some("cli")` (the CLI runs as the local
     operator); test sites can pass `None` or `Some("cli")` as convenient.
   - `src/cron/store.rs:27` — the `add_job` wrapper calls `add_shell_job`; thread
     the argument through (pass `None`, as `add_job` has no creator context).
   - `src/cron/scheduler.rs` — three `#[cfg(test)]` call sites at `986`, `1011`
     (`add_agent_job`) and `1078` (`add_shell_job`); pass `None`.
   - `src/tui/commands/cron.rs:129` (`add_agent_job`) and `:140`
     (`add_shell_job`) — pass `Some("tui")` (see In-scope note).
   - `src/tools/cron_add.rs` — the tool has no sender identity today
     (`CronAddTool` holds only `config`+`security`, `src/tools/cron_add.rs:9-12`).
     Pass `Some("agent-tool")` as the origin label. **Do not invent a
     per-guest identity here** — plumbing the channel sender into the tool is a
     separate change; a coarse origin label is honest and sufficient for Step 4's
     default decision. Note this limitation in the PR.

**Verify**: `cargo test --lib cron` → all pass. `cargo build` compiles with no
`created_by` type mismatch.

### Step 3: Gate HTTP agent-job creation on owner identity

The chat path distinguishes owner from guest via
`build_guest_gate` → `can_approve(&approval_owners, sender)`. The cron HTTP path
only runs `check_auth` (pairing-token possession). A pairing token does **not**
imply owner. Add an owner check to `create_cron` for the **agent** branch:

1. In `src/gateway/cron_api.rs`, before building an `Agent` job (inside the
   `JobType::Agent` arm, around line 207), resolve the caller's principal and
   require that it is an approval owner. There is no per-request sender on this
   endpoint today, so the correct conservative gate is: **agent-job creation
   over HTTP requires that the caller is authenticated *and* an owner-equivalent
   principal.** Two acceptable implementations — pick one and state which in the
   PR:
   - (a) If the pairing layer exposes the paired principal/label, check it
     against `cfg.channels_config.approval_owners` via
     `crate::approval::can_approve`.
   - (b) If no principal is available, treat the console pairing token as
     owner-equivalent **only when** `approval_owners` is empty (single-operator
     install); when `approval_owners` is non-empty, **refuse agent-job creation
     over HTTP** with 403 and a message directing the operator to create it from
     an owner channel or the CLI. This fails closed — the safe default.
2. Record the resolved principal as `created_by` in the store call from Step 2.
3. Leave the **shell** branch unchanged here (its command is already gated;
   plan 174 hardens it further).

**Verify**: add a test in `src/gateway/cron_api.rs`'s `#[cfg(test)] mod tests`
(the module exists at line 392) asserting that, with a non-empty
`approval_owners`, `POST /api/v1/cron` with an agent `prompt` is refused
(implementation (b)). `cargo test --lib` for the gateway cron tests → pass.

**Testability note**: the `create_cron` handler takes an `AppState`, and this
plan does not show how to construct one. Prefer **extracting the owner decision
into a pure helper** (e.g. `fn agent_job_creation_allowed(approval_owners:
&[String], principal: Option<&str>) -> bool`) and unit-testing that helper
directly — no `AppState` scaffolding, a crisp A/B. If you instead test the
handler end-to-end, you must first build a test `AppState`; check whether the
gateway test module already has a helper for that (grep the `#[cfg(test)]`
blocks in `src/gateway/`), and if none exists, acknowledge the extra scaffolding
cost in the PR rather than inventing a fragile one inline.

### Step 4: Reconstruct a capability ceiling for the scheduled run — STOP-and-confirm

`run_agent_job` (`src/cron/scheduler.rs:203`) calls `crate::agent::run(...)`,
which has **no guest_gate parameter** (`src/agent/loop_.rs:2013-2020`). Passing
a reconstructed ceiling into the deferred run therefore requires either:

- **Option A**: add `guest_gate: Option<crate::approval::GuestGate>` as a new
  parameter to `crate::agent::run` and thread it into the tool-dispatch calls
  inside `run()`. This touches all 4 call sites of `run()`
  (`src/cron/scheduler.rs:240`, `src/daemon/mod.rs:323`, `src/main.rs:1846`,
  `src/main.rs:2203`) — pass `None` at the three non-cron sites.
- **Option B**: give `run_agent_job` a dedicated lower-level entrypoint that
  builds the agent and runs its loop with an explicit `guest_gate`, bypassing
  the top-level `run()` wrapper.

Both are cross-cutting and change a public-ish signature. **STOP and report
which option the operator wants before implementing Step 4.** When approved:

1. In `run_agent_job`, map `job.created_by` to a ceiling: an owner/CLI origin →
   `None` (full toolset, unchanged behavior for owner-created jobs); any
   non-owner origin → a `GuestGate` built from
   `config.channels_config.{auto_approve via autonomy, guest_allowed_tools,
   guest_allowed_commands}` (mirror `build_guest_gate`'s construction).
2. Pass that ceiling into the run so the deferred agent job is constrained
   exactly like a live guest turn.
3. **Legacy rows** (`created_by IS NULL`) have no recorded origin. The default
   is a security decision — **STOP and confirm** which the operator wants:
   - treat legacy rows as owner-created (preserve current behavior; least
     disruptive), or
   - disable legacy agent jobs until re-created (fail-closed; safest).

**Verify**: a scheduler test that an agent job with a non-owner `created_by`
runs under a `GuestGate` (assert a disallowed tool is denied). `cargo test
--lib cron` → pass.

## Test plan

- `src/cron/store.rs` tests: a job created with `created_by = Some("cli")`
  round-trips through `get_job`/`list_jobs` with the field preserved; a row
  written by the *old* schema (no `created_by` column value) loads as `None`.
- `src/gateway/cron_api.rs` tests: agent-job creation over HTTP is refused when
  `approval_owners` is non-empty (Step 3 (b)); shell-job creation is unaffected.
- `src/cron/scheduler.rs` tests (Step 4, after approval): a non-owner-origin
  agent job is denied a non-allowlisted tool at run time; an owner-origin job is
  not.
- Model new store tests after the existing cron store tests; model the scheduler
  test after `run_job_command_blocks_disallowed_command`
  (`src/cron/scheduler.rs:730-742`).
- Verification: `cargo test --lib cron` and `cargo test --lib security` → all
  pass, including the new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib cron` and `cargo test --lib security` pass; new tests exist
- [ ] `CronJob` has a `created_by: Option<String>` field, persisted via the
      `add_column_if_missing` migration and read by `map_cron_job_row`
- [ ] `POST /api/v1/cron` refuses agent-job creation for non-owner principals
      (fails closed when `approval_owners` is non-empty)
- [ ] Step 4 either landed under the operator's chosen option, or is explicitly
      deferred in the PR with the STOP-and-confirm outcome recorded
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- The "Current state" excerpts do not match the live code (drift since 2aefb9f).
- Step 3 cannot resolve a caller principal AND `approval_owners` semantics are
  unclear — report rather than guessing who counts as owner.
- Step 4 requires touching `crate::agent::run`'s signature: STOP and get the
  operator to choose Option A vs B before proceeding.
- The legacy-row default (Step 4.3) is unconfirmed — do not pick one silently.
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- **Relationship to plans 173 & 177**: If this plan (esp. Step 4) lands, it
  provides a real provenance gate that constrains the deferred run — it
  **largely supersedes plan 177** (adding cron tools to `OWNER_ONLY_TOOLS`),
  which is the cheaper defense-in-depth version worth having regardless. Plan
  173 (delivery-target gating) is complementary, not superseded — land both.
- A reviewer should scrutinize: the positional index in `map_cron_job_row`
  matching the SELECT order; that the HTTP gate fails **closed**; and that the
  three non-cron `run()` call sites pass `None` (no behavior change) if Option A
  is taken.
- Deferred: plumbing a real per-guest sender identity into `CronAddTool` (today
  it only has a coarse origin label). If that lands later, Step 4's ceiling can
  become per-sender instead of owner/guest binary.
