# Plan 212: Make autonomy writes atomic + rollback-safe, and announce the daemon reload from the CLI

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/approval/policy_writer.rs src/main.rs src/tui/app.rs src/tui/commands/autonomy.rs`

## Status

- **Priority**: P2 (correctness — split state between marker and enforced config)
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

Setting an autonomy preset writes **two** things: the policy-dir files
(including the `autonomy.toml` marker that the TUI indicator and CLI drift
warning read) and `config.toml` (which the gate actually enforces). Every
autonomy surface writes them in an order with no rollback, so a failure between
the two leaves a **split state**: the marker says one preset while the enforced
config is another.

- CLI `autonomy <preset>` (the known CLI-18): writes policy files
  (`main.rs:1804`) then config (`main.rs:1810-1813`); a config-save failure
  leaves the marker on the new preset and `config.toml` on the old.
- TUI `/autonomy <preset>` and Shift+Tab have the same two-write ordering.

Additionally, the policy files are written with plain `fs::write` (not the
temp-file + atomic-rename that `Config::save` uses), so a crash mid-write can
truncate `autonomy.toml`/`command_allowlist.toml` — the very files the gate
reads next boot.

Finally, CLI `autonomy <preset>` does not call `announce_daemon_reload()` (which
`permissions` and `bind-telegram` do), so a running managed daemon is neither
reloaded nor does the operator get a "restart to apply" note.

## Current state

### CLI order — `src/main.rs:1802-1815`

```rust
    if let Some(warning) = policy_writer::write_policy_files(&profile, target, true)? {
        eprintln!("{warning}");
    }
    // marker is now `target`; config.toml is still the old level:
    let mut config = Config::load_or_init().await?;
    policy_writer::apply_preset_to_config(&mut config, target);
    config.save().await?;                    // failure here => split state, no rollback
```

### Non-atomic policy writes — `src/approval/policy_writer.rs:325, 549, 582`

`write_active_preset`, `write_autonomy`, `write_patterns` all use plain
`fs::write`. `Config::save` (`src/config/schema.rs:4594-4620`) writes a temp
file and atomically renames.

## The fix

### Step 1 — write config first, then the marker

Reorder every autonomy write path so the **enforced** `config.toml` is saved
first (it is already atomic), and only on success is the policy-dir marker
written. On a marker-write failure, the enforced state is still correct and the
marker can be reconciled (the CLI already has a drift warning). This inverts the
current "marker first" order across:

- CLI `autonomy <preset>` (`src/main.rs:1802`),
- TUI `/autonomy <preset>` (`src/tui/commands/autonomy.rs:90-104`),
- TUI Shift+Tab (`src/tui/app.rs:629-662`).

Alternatively, wrap both writes in a single helper that rolls back the marker on
a config-save failure. Prefer the reorder (simpler, no rollback path to get
wrong).

### Step 2 — make policy-file writes atomic

Change `write_active_preset` / `write_autonomy` / `write_patterns` in
`policy_writer.rs` to temp-file + rename (reuse the pattern from
`Config::save`, or a shared `atomic_write(path, bytes)` helper). This prevents a
crash from truncating the policy the gate reads next boot.

### Step 3 — CLI announces the daemon reload

At the end of the CLI `autonomy <preset>` branch, call `announce_daemon_reload()`
(as `permissions` at `src/main.rs:2554` and `bind-telegram` do), so a managed
daemon is reloaded or the operator is told to restart.

## Files

- **In scope**: `src/approval/policy_writer.rs` (atomic writes), `src/main.rs`
  (CLI order + announce), `src/tui/commands/autonomy.rs` and `src/tui/app.rs`
  (TUI order).
- **Out of scope**: the forced-reload/listener propagation (plan 211), the
  session-allowlist invalidation (plan 207).

## STOP conditions

- If a shared atomic-write helper already exists (search `atomic`/`tempfile`/
  `persist` in `policy_writer.rs`/`config`), reuse it; do not add a second.
- If reordering to "config first" breaks a path that reads the marker
  immediately after writing it (before config), audit that reader and adjust —
  do not leave a window where the marker is expected but not yet written.

## Done criteria

1. `cargo fmt`/`clippy` clean; `cargo build -p rantaiclaw --bin rantaiclaw`.
2. `cargo test -p rantaiclaw --lib approval::policy_writer` clean, plus:

```rust
#[test]
fn policy_files_are_written_atomically() {
    // Write a preset into a temp policy_dir, then assert no temp/partial file
    // remains and the final file parses. (Mirror how write_policy_files tests
    // set up a temp dir.)
}
```

3. A test (or documented manual check) that a simulated config-save failure
   after the marker write does NOT leave the marker ahead of the enforced level
   — i.e. with the reorder, the marker is only written after config succeeds.

## Test plan

Mirror the existing `write_policy_files` tests (they use a temp `policy_dir`).
Add the atomic-write assertion and, if the code is structured to allow injecting
a save failure, the split-state assertion; otherwise document the reorder as the
guarantee and note it was verified by inspection.

## Risk & rollback

- **Risk**: MED — reordering write order changes crash semantics; the reorder is
  the safer order (enforced state leads). Atomic writes are strictly safer.
- **Rollback**: revert the touched files; no schema/migration change.

## Maintenance note

The marker is a convenience/reporting artifact; `config.toml` is the enforced
truth. Always write the enforced truth first. Any new policy-dir file must use
the atomic-write helper — plain `fs::write` on a security-relevant file is the
defect this plan removes.
