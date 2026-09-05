# Plan 247: Fix profile handoff, sentinel validation, and daemon-state durability

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/service/mod.rs src/daemon/mod.rs src/daemon/handoff.rs src/profile/sentinel.rs src/channels/admin.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED (changes unit identity + restart decisions; existing installs must keep working)
- **Depends on**: none (coordinates with plan 244 on the unit text)
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Profile/daemon handoff is broken by several related defects:

1. **Profile identity not in the unit** (D5). The installer never writes `RANTAICLAW_PROFILE`/`RANTAICLAW_UNIT` into the unit, so a service-installed daemon always runs profile `default` regardless of which profile it was installed from; and `profile use <name>` restarts `rantaiclaw@<name>.service`, a template unit the installer never creates — handoff can never succeed.
2. **Restart without liveness check** (D10). `restart_daemon_for_profile_with` restarts unconditionally without `pid_is_alive`/`is_active`, so after an unclean daemon exit `profile use` STARTS a daemon the operator had stopped. `DaemonControl::is_active` exists but has zero production callers.
3. **`daemon_state.json` non-atomic** (D12). Written every 5s with `tokio::fs::write` (truncate-in-place); readers (`doctor`, TUI) treat a torn read as a hard error → intermittent false "daemon broken".
4. **launchd stop→start race** (D14). macOS `maybe_restart_managed_daemon_service` does async `launchctl stop`+`start` and reports `Ok(true)` on start's exit code → tells the operator "reloaded" while the daemon may be dead/old-config. Use `launchctl kickstart -k` (already used by `handoff::Launchd::restart`).
5. **PID-reuse guard** (D17). `pid_is_alive` = `kill(pid,0)` answers "some process", not "the daemon" → after SIGKILL + PID reuse the TUI permanently refuses to start channels.

## Current state

- `src/daemon/mod.rs:46-53` — the daemon records its sentinel from `RANTAICLAW_PROFILE` (default `"default"`) and `RANTAICLAW_UNIT`; a repo-wide grep finds no writer for `RANTAICLAW_UNIT`.
- `src/service/mod.rs:597-616` (`systemd_user_unit`, see plan 244) emits no `Environment=RANTAICLAW_PROFILE=…`/`RANTAICLAW_UNIT=…`; the installer only ever creates `rantaiclaw.service` (`:1232-1242`), never `rantaiclaw@<profile>.service`.
- `src/daemon/handoff.rs:172-204` — `restart_daemon_for_profile_with` reads the sentinel and calls `control.restart(&unit)` unconditionally; never `pid_is_alive`, never `control.is_active` (declared + implemented at `:26-106`, only called in a test at `:282`). Handoff with `sentinel.unit == None` restarts `rantaiclaw@<profile>.service` (`:152-154,193-200`).
- `src/daemon/mod.rs:239-251` — the state writer calls `tokio::fs::write(&path, data)` every 5s. Readers: `src/doctor/legacy.rs:700-716` (`check_daemon_state`, reports parse failure as an ERROR), `src/tui/commands/config.rs:595-614`. The atomic pattern exists at `src/profile/sentinel.rs:54-58` (temp + `fs::rename`).
- `src/channels/admin.rs:203-215` — macOS branch does `launchctl stop` (discarded) then `launchctl start`, returns `Ok(true)` on start's exit. `src/daemon/handoff.rs:78-96` already uses `kickstart -k`.
- `src/profile/sentinel.rs:95-104` — `pid_is_alive` = `kill(pid,0)`; `:37-39` — `DaemonSentinel` has a `started_at`; `active_daemon_pid` (`:110-115`) builds on `pid_is_alive`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib daemon` | pass |
| Test | `cargo test --lib service` | pass |
| Test | `cargo test --lib profile` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/service/mod.rs` (stamp `RANTAICLAW_PROFILE`/`RANTAICLAW_UNIT=rantaiclaw.service` into the unit)
- `src/daemon/handoff.rs` (liveness guard before restart)
- `src/daemon/mod.rs` (atomic `daemon_state.json`)
- `src/channels/admin.rs` (macOS `kickstart -k`)
- `src/profile/sentinel.rs` (PID-reuse guard via `started_at`/proc start time)

**Out of scope**:
- The unit TEXT quoting/XDG (plan 244) — coordinate but don't duplicate.
- Blocking-call offload (plan 248).

## Git workflow

- Branch: `fix/daemon-profile-handoff`
- Message e.g. `fix(daemon): carry profile identity into the unit and validate the sentinel before restart`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Stamp profile + unit identity into the installed unit

Add `Environment=RANTAICLAW_PROFILE=<profile>` and `Environment=RANTAICLAW_UNIT=rantaiclaw.service` to `systemd_user_unit` (and the plist `EnvironmentVariables` / OpenRC `export`). Keep the non-template `rantaiclaw.service` name; `default_unit_name` becomes a last-resort fallback only.

**Verify**: Test-plan `installed_unit_carries_profile_identity` passes.

### Step 2: Validate liveness before restarting on profile switch

In `restart_daemon_for_profile_with` (`handoff.rs:172`), skip and clear the sentinel when `!pid_is_alive(sentinel.pid)`, and gate the restart on `control.is_active(&unit)?`. Extend the existing `RecordingControl` stub tests to assert zero restarts for a dead pid / inactive unit.

**Verify**: Test-plan `dead_pid_does_not_restart` passes.

### Step 3: Write `daemon_state.json` atomically

Replace the `tokio::fs::write` (`daemon/mod.rs:239`) with temp (`daemon_state.json.tmp.<pid>`) + `tokio::fs::rename`, mirroring `sentinel::write_sentinel`. Optionally remove the file on graceful shutdown so `doctor` can distinguish "stopped" from "stale".

**Verify**: Test-plan `daemon_state_write_is_atomic` passes.

### Step 4: macOS restart via `kickstart -k`

In `channels/admin.rs:203`, replace stop+start with `launchctl kickstart -k gui/<uid>/com.rantaiclaw.daemon` (match `handoff::Launchd::restart`) and confirm the job is listed+running before returning `Ok(true)`.

**Verify**: `cargo test --lib` (channels/admin filters) → pass.

### Step 5: PID-reuse guard

In `sentinel.rs`, record/compare the process start time (Linux `/proc/<pid>/stat`) or the process name against the sentinel's `started_at` before treating the pid as the live daemon; a mismatch is stale → clear the sentinel.

**Verify**: Test-plan `stale_pid_after_reuse_is_cleared` passes (or a documented best-effort if start-time reading isn't unit-testable).

## Test plan

- `service`: `installed_unit_carries_profile_identity` — the generated unit contains `RANTAICLAW_PROFILE=<profile>` and `RANTAICLAW_UNIT=rantaiclaw.service`.
- `daemon` (handoff): `dead_pid_does_not_restart` — a `RecordingControl` + a dead pid → zero restarts; `inactive_unit_does_not_restart`.
- `daemon`: `daemon_state_write_is_atomic` — no `.tmp` residue; target parses.
- `profile`: `stale_pid_after_reuse_is_cleared` — a sentinel whose pid maps to a different start time is treated as stale.
- Verification: `cargo test --lib daemon service profile` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped tests pass with the new tests
- [ ] `control.is_active` now has a production caller (`grep -n "is_active" src/daemon/handoff.rs`)
- [ ] `grep -n "tokio::fs::write" src/daemon/mod.rs` no longer shows the state-file write (now atomic)
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Changing the unit identity would break EXISTING installs that already run `rantaiclaw.service` — keep that exact name; only ADD the Environment lines. Report if a rename seems required.
- Reading `/proc/<pid>/stat` isn't portable to the target — implement the process-name check or document the guard as Linux-only best-effort.

## Maintenance notes

- Reviewer: confirm `profile use` no longer starts a stopped daemon (liveness test) and that the installed unit carries the real profile.
- Interacts with plan 244 (unit text) — both edit `systemd_user_unit`; land order matters.
