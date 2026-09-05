# Plan 244: Generate valid service units on every platform

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/service/mod.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW–MED (unit-file text generation; changes are validated by pure-function tests)
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Several service-unit generation defects produce a unit that fails AFTER `service install` reports success:

1. **launchd plist is invalid XML** (D1). The plist is a raw string (`r#"..."#`) whose XML declaration/DOCTYPE use backslash-escaped quotes (`version=\"1.0\"`) that land verbatim, so `launchctl load` rejects it — the entire macOS service path never works.
2. **systemd unit values unquoted/unescaped** (D13). `ExecStart={exe} daemon` and `WorkingDirectory={}` interpolate unquoted → a path with a space breaks the command line; `%` in `PATH`/cwd is a systemd specifier and must be `%%`.
3. **`XDG_CONFIG_HOME` ignored** (D15). `linux_service_file` hard-codes `~/.config/systemd/user/...` → install writes where systemd can't see it on XDG-overridden hosts; "Unit not found" after "install succeeded".
4. **`run_capture` ignores exit status** (D16). Windows `is_service_installed` returns true whenever `schtasks.exe` exists (latent today; non-Linux early-returns).

## Current state

- `src/service/mod.rs`:
  - launchd plist (`:539-566`): `let plist = format!(r#"<?xml version=\"1.0\" ...`— raw string, backslashes land literally. `exe`/`stdout`/`stderr` go through `xml_escape` (fine); the hardcoded declaration lines are the bug.
  - `systemd_user_unit` (`:597-616`): `ExecStart={} daemon` with `exe.display()` unquoted (`:611/614`); `WorkingDirectory={}` unquoted (`:601`); PATH is double-quoted (`:607`) but no `%` escaping anywhere.
  - `linux_service_file` (`:1232-1242`): hard-codes `<home>/.config/systemd/user/rantaiclaw.service` from `directories::UserDirs`, discards the `config` arg. `maybe_restart_managed_daemon_service` in `src/channels/admin.rs:240-250` reconstructs the same path independently.
  - `run_capture` (`:1253-1259`): returns `Ok(text)` regardless of `output.status`; `is_service_installed` Windows branch (`:143`) is `run_capture(...).is_ok()`.
  - Existing tests cover `xml_escape`, `systemd_user_unit`, `decide_*_action` (pure fns) — but none assert the plist body or the install/uninstall paths.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib service` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/service/mod.rs` (plist string, `systemd_user_unit`, a shared `systemd_user_unit_dir()` honoring XDG, `run_capture`/a new `command_succeeded` helper, new tests)
- `src/channels/admin.rs` — only to call the shared `systemd_user_unit_dir()` instead of the duplicated path

**Out of scope**:
- Install orchestration semantics beyond the unit text + path + status check.
- The blocking-call-on-async issue (plan 248).

## Git workflow

- Branch: `fix/service-unit-generation`
- Message e.g. `fix(service): emit valid launchd/systemd units and honor XDG_CONFIG_HOME`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Fix the launchd plist XML

In the plist `format!` (`:539`), remove the `\` before every `"` inside the raw string (raw strings do not process escapes, so `\"` is wrong — use plain `"`). Extract the body into a pure `fn macos_plist(label, exe, stdout, stderr) -> String` and add a test asserting the first line is exactly `<?xml version="1.0" encoding="UTF-8"?>` (no backslash).

**Verify**: Test-plan `macos_plist_is_well_formed` passes.

### Step 2: Quote and `%`-escape systemd values

Add `fn systemd_escape_value(s: &str) -> String` that doubles `%` and wraps a value containing whitespace in double quotes. Route `ExecStart`'s exe, `WorkingDirectory`, and the PATH value through it. Update the existing `systemd_user_unit` tests' expected strings.

**Verify**: Test-plan `systemd_unit_quotes_paths_with_spaces` passes.

### Step 3: Honor `XDG_CONFIG_HOME` for the unit path

Add `fn systemd_user_unit_dir() -> PathBuf` that reads `XDG_CONFIG_HOME` with the `~/.config` fallback, then `/systemd/user`. Use it from `linux_service_file` and from `admin.rs`'s duplicate path. Tighten the existing path test to assert the full path under a controlled env.

**Verify**: Test-plan `unit_dir_honors_xdg` passes.

### Step 4: Check exit status in presence probes

Add `fn command_succeeded(cmd) -> bool` that checks `status.success()`, and use it for `is_service_installed`'s presence checks (all "does X exist / is X running" probes). Keep `run_capture` for the callers that genuinely want stdout regardless of status.

**Verify**: `cargo test --lib service` → pass.

## Test plan

- `macos_plist_is_well_formed` — asserts the declaration/DOCTYPE lines byte-for-byte (no backslash) and that the body parses as XML if a cheap check is available, else string assertions.
- `systemd_unit_quotes_paths_with_spaces` — exe/cwd with a space and a `%` → quoted + `%%`.
- `unit_dir_honors_xdg` — with `XDG_CONFIG_HOME` set (via a panic-safe env guard), the unit dir is under it; without, under `~/.config`.
- Verification: `cargo test --lib service` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] `grep -n 'version=\\\\"1.0' src/service/mod.rs` returns nothing (no backslash-escaped quotes in the plist)
- [ ] scoped service tests pass with the new tests
- [ ] `git status` shows only `src/service/mod.rs`, `src/channels/admin.rs`
- [ ] `plans/README.md` row updated

## STOP conditions

- The plist/systemd generation moved out of `service/mod.rs` (drift) — STOP.
- Changing `run_capture` callers cascades broadly — add `command_succeeded` for the presence probes only, leave `run_capture` where stdout is needed; report if a caller is ambiguous.

## Maintenance notes

- Reviewer: confirm the plist test asserts NO backslash, and that a spaced path yields a quoted unit.
- The launchd fix (Step 1) is what makes macOS `service install` work at all — prioritize it in review.
- `%%` escaping only matters when a literal `%` appears in PATH/cwd; the quote fix is the common case.
