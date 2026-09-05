# Plan 177: Add cron mutation tools to the guest OWNER_ONLY_TOOLS ceiling

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 2aefb9f..HEAD -- src/approval/guest.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none (defense-in-depth counterpart to plans/172)
- **Category**: security
- **Planned at**: commit `2aefb9f`, 2026-08-19

## Why this matters

`OWNER_ONLY_TOOLS` (`src/approval/guest.rs:60-69`) is the hard, unconditional
capability ceiling for non-owner ("guest") channel users — it is checked
**first** in `tool_permitted` (73-78), so a tool listed there is denied even if
an owner mistakenly adds it to `guest_allowed_tools`. The documented inclusion
criterion (`src/approval/guest.rs:47-52`) is a tool that "executes code outside
this gate's reach, so the guest ceiling can't constrain it" — with `delegate`
given as the canonical example ("spawns a sub-agent loop with NO guest gate ...
a full bypass"). A cron **agent** job is exactly that primitive **deferred in
time**: its scheduled run calls `crate::agent::run(...)`
(`src/cron/scheduler.rs:240`) with the full toolset and no guest gate; `cron_run`
triggers such a run immediately. Yet `cron_add`, `cron_update`, and `cron_run`
are **not** on the list. This is safe *today* only because
`guest_allowed_tools` defaults empty — but it is a latent footgun: an owner who
adds `cron_add` to the guest allowlist ("let people set reminders") would
silently hand guests the unconstrained-sub-loop primitive. This plan closes that
footgun cheaply.

## Current state

`src/approval/guest.rs:60-69` — the current list:
```rust
pub const OWNER_ONLY_TOOLS: &'static [&'static str] = &[
    "manage_permissions",
    "issue_pairing_code",
    "delegate",
    "ssh",
    "pty",
    "author_skill",
    "skills_install",
    "skills_install_deps",
];
```

`src/approval/guest.rs:73-78` — the ceiling is checked before the allowlist:
```rust
pub fn tool_permitted(&self, tool: &str) -> bool {
    if Self::OWNER_ONLY_TOOLS.contains(&tool) {
        return false;
    }
    self.permitted_tools.contains(tool)
}
```

`src/approval/guest.rs:47-52` — the inclusion criterion (the reasoning to cite
in the new comment):
```rust
///   * it executes code outside this gate's reach, so the guest ceiling
///     can't constrain it:
///       - `delegate` spawns a sub-agent loop with NO guest gate, so any tool
///         the sub-agent is allowed runs unconstrained — a full bypass;
```

Existing tests (the structural pattern to extend):
- `owner_only_tools_never_permitted_for_guests` (`src/approval/guest.rs:221-247`)
- `guest_denied_skill_write_tools_even_when_allowlisted` (249-268)

Tool names (factory registration keys, confirmed): `cron_add`, `cron_update`,
`cron_run` (mutation/trigger); `cron_list`, `cron_runs` (read-only — keep OFF
the list). See `src/tools/mod.rs` (`CronAddTool` at 258, `CronRunTool` at 262).

Repo conventions:
- Registration keys are stable, lowercase, user-facing (CLAUDE.md §6.3). Use the
  exact tool names above.
- The list carries a doc comment explaining *why* each entry is owner-only; add
  the cron reasoning in the same style.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format  | `cargo fmt --all -- --check` | exit 0 |
| Lint    | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests   | `cargo test --lib approval` | all pass |
| Drift   | `git diff --stat 2aefb9f..HEAD -- src/approval/guest.rs` | only your changes |

Do **not** run a bare `cargo test` (disk-constrained). Scope with `--lib`.

## Scope

**In scope**:
- `src/approval/guest.rs` — add three entries to `OWNER_ONLY_TOOLS`, a comment,
  and test coverage

**Out of scope**:
- `cron_list`, `cron_runs` (read-only) — must stay usable by guests; do NOT add
  them.
- Any change to the cron subsystem, scheduler, or store — this is a guest-gate
  change only. The runtime provenance gate is plan 172.
- Any change to `guest_allowed_tools` defaults or config schema.

## Git workflow

- Branch: `advisor/177-cron-tools-owner-only`
- Conventional commit
  (e.g. `fix(approval): make cron mutation tools owner-only for guests`).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the three cron mutation tools to the ceiling

Append to `OWNER_ONLY_TOOLS` (`src/approval/guest.rs:60-69`):
```rust
    "cron_add",
    "cron_update",
    "cron_run",
```
Extend the doc comment above the constant (in the "executes code outside this
gate's reach" bullet at 47-52) to explain the cron case, e.g.:

> `cron_add` / `cron_update` persist an **agent** job whose later scheduled run
> executes `crate::agent::run(...)` with the full toolset and **no guest
> gate** — the same deferred-sub-loop bypass as `delegate`, just fired later;
> `cron_run` triggers that run immediately. Read-only `cron_list` / `cron_runs`
> stay allowed.

**Verify**: `cargo test --lib approval` → pass (existing tests unaffected).

### Step 2: Extend the tests

In `src/approval/guest.rs` `#[cfg(test)] mod tests`, add a test modeled on
`owner_only_tools_never_permitted_for_guests` (221-247) that constructs a gate
with `cron_add`, `cron_update`, `cron_run` in `guest_allowed_tools` and asserts:
- `tool_permitted("cron_add")` / `cron_update` / `cron_run` are all `false`;
- `deny_reason(...)` for each contains `"owner-only"`;
- and, as a guard against over-restriction, `tool_permitted("cron_list")` and
  `tool_permitted("cron_runs")` are `true` when placed in `guest_allowed_tools`.

**Verify**: `cargo test --lib approval` → the new test passes.

## Test plan

- New test (above): the three cron mutation tools are denied even when
  allowlisted; the two read-only cron tools remain permitted when allowlisted.
- Model after `owner_only_tools_never_permitted_for_guests` and
  `guest_denied_skill_write_tools_even_when_allowlisted`.
- Verification: `cargo test --lib approval` → all pass, including the new test.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib approval` passes; the new test exists and passes
- [ ] `OWNER_ONLY_TOOLS` contains `cron_add`, `cron_update`, `cron_run` and NOT
      `cron_list`/`cron_runs`
- [ ] The doc comment explains the deferred-sub-loop reasoning for cron
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report if:

- The "Current state" excerpt of `OWNER_ONLY_TOOLS` does not match live code
  (drift since 2aefb9f) — e.g. the cron tools were already added.
- The cron tool registration keys differ from `cron_add`/`cron_update`/`cron_run`
  (verify against `src/tools/mod.rs`) — use the actual keys.
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- This is the **cheap defense-in-depth** version. It is largely superseded by
  plan 172 (a per-job provenance gate that constrains the deferred run properly)
  — but it is worth having regardless, because it protects against operator
  misconfiguration even after 172 lands. If 172 lands first, this remains a
  correct belt-and-suspenders and should still merge.
- A reviewer should scrutinize: that only mutation/trigger tools were added, and
  that no read-only cron tool was swept in (guests must still be able to *view*
  schedules).
