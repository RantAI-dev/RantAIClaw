# Plan 210: Autonomy honesty — TUI picker must not lie on reload failure, and status must show the preset not the level

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/tui/app.rs src/tui/commands/config.rs src/main.rs src/approval/policy_writer.rs`

## Status

- **Priority**: P2 (honesty — the UI asserts a policy that is not in force)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

Three surfaces misreport the active autonomy:

1. **The TUI `/autonomy` picker lies on a reload failure.** When
   `apply_preset_to_config_and_reload` errors, the handler appends the failure
   message but does **not** return — it falls through and sets
   `autonomy_preset = Some(target)` and prints "⚙ Autonomy mode → {target}",
   flipping the status-bar indicator to a preset that is **not** enforced. The
   Shift+Tab path was specifically hardened against this exact "the status bar
   asserts a level not in force" lie; the picker never got the fix.
2. **CLI `status` shows the level, not the preset.** `Manual` and `Smart` are
   both `Supervised`; printing the level cannot tell them apart, so
   `rantaiclaw status` cannot answer "will this prompt before a shell command?".
   The `rantaiclaw autonomy` command already resolves the preset — `status`
   should too.
3. **TUI `/status` shows no autonomy at all.** The `/status` panel shows the
   "Approval boundary" (owner count) but omits the autonomy preset/level, so a
   TUI operator running `/status` gets no autonomy readout.

## Current state

### Picker falls through on error — `src/tui/app.rs:3347-3358`

```rust
                if let Err(e) = self.apply_preset_to_config_and_reload(target) {
                    let msg = format!("⚠ Preset written, but live reload failed: {e}");
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".into(), msg));
                    // <-- NO return here
                }
                self.context.autonomy_preset = Some(target);        // runs even on failure
                let msg = format!("⚙ Autonomy mode → {} ({}). ...", target.label(), ...);
                // ... success line printed even though the gate is unchanged
```

Contrast the Shift+Tab handler (`app.rs:662-673`), which returns early on the
same failure without touching `autonomy_preset`.

### CLI status prints the level — `src/main.rs:1901`

```rust
    cli_style::field("Autonomy", W, &format!("{:?}", config.autonomy.level));
```

`preset_for_autonomy(&config.autonomy)` (`src/approval/policy_writer.rs:290`)
resolves the real preset, and `PolicyPreset::id()` (`policy_writer.rs:57`) names
it.

### TUI /status omits autonomy — `src/tui/commands/config.rs:404-435`

The panel renders "Runtime" and "Approval boundary" sections but no autonomy
preset/level row.

## The fix

### Step 1 — picker: early-return on reload failure (mirror Shift+Tab)

In `src/tui/app.rs:3347`, add a `return` inside the `if let Err(...)` block —
after appending the failure message, before `autonomy_preset` is set and the
success line printed. The status bar must not flip when the gate did not change.

### Step 2 — CLI status: show the preset alongside the level

In `src/main.rs:1901`, render `preset_for_autonomy(&config.autonomy).id()` in
addition to (or instead of) the raw level, e.g. `Autonomy: Manual (Supervised)`.
Match the `autonomy` command's output shape.

### Step 3 — TUI /status: add an autonomy row

In `src/tui/commands/config.rs`, add an "Autonomy" section to the `/status`
panel showing the preset (via `preset_for_autonomy`) and level, next to the
existing "Approval boundary" section.

## Files

- **In scope**: `src/tui/app.rs` (Step 1), `src/main.rs` (Step 2),
  `src/tui/commands/config.rs` (Step 3). Read-only: `src/approval/policy_writer.rs`.
- **Out of scope**: the marker-vs-config drift on the TUI indicator (that is a
  MED-effort follow-up: derive the indicator from the enforced config rather
  than the on-disk marker — note it but do not fix here unless trivial), the
  propagation-to-listeners issue (plan 211), write atomicity (plan 212).

## STOP conditions

- If the picker handler has been refactored so the failure path already returns
  (drift), skip Step 1 and report.

## Done criteria

1. `cargo fmt`/`clippy` clean.
2. `cargo test -p rantaiclaw --lib` clean (the TUI is a binary crate — build the
   binary crate root, not just `--lib`, to catch the `app.rs` change:
   `cargo build -p rantaiclaw --bin rantaiclaw`).
3. Where testable, a unit test asserting `status` output contains the preset
   name for a Manual config and a Smart config (they must differ). The picker
   early-return is a control-flow fix best verified by reading + a `cargo build`;
   note in the PR that it was confirmed by inspection against the Shift+Tab
   sibling.

## Test plan

- CLI status: if the status rendering is factored into a testable function,
  assert Manual vs Smart produce different autonomy text. Otherwise verify by
  running `rantaiclaw status` under each preset (document the two outputs in the
  PR).
- Picker: inspect that the early-return matches the Shift+Tab handler; a
  reload-failure is hard to force in a unit test.

## Risk & rollback

- **Risk**: LOW — three display/control-flow fixes; no policy behavior change.
- **Rollback**: revert the three files.

## Maintenance note

Every surface that shows autonomy should render the **preset** (via
`preset_for_autonomy`), never the raw level, because Manual and Smart share a
level. The marker-vs-enforced-config drift on the TUI indicator is a related
follow-up worth a small plan if it bites.
