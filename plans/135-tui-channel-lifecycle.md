# Plan 135: TUI — configuring a channel starts it; one roster; honest status

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/tui/app.rs src/tui/commands/config.rs src/channels/auto_start_state.rs`
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged first.
> That is expected and is not a stop condition. Relocate by symbol name and continue.
> STOP only if the *code itself* no longer matches the "Current state" excerpt
> semantically.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/120 (provides the single channel factory this plan consumes)
- **Category**: bug
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

The TUI's primary channel-setup flow — `/setup channels` → picker → provisioner — is
documented as the path users take, and it **never starts the channel it just
configured**.

The cause is a fix for a different bug. The save handler swaps in the new config
immediately, with a comment explaining that it does so "so the next provisioner's
clone sees this provisioner's writes". The change detector then computes the previous
channel count from that already-swapped config, so previous always equals new,
`channels_changed` is always false, and the restart never fires. Config is written,
"✓ configured" is printed, and the live runtime is untouched for the rest of the
session.

Two more make it worse. QQ appears **nowhere** in `src/tui/app.rs` — `grep -c 'qq'`
returns 0 — so it is missing from both private roster copies: configuring it leaves
the count at zero, the restart returns early, and `/channels` lists it in neither the
configured nor the not-configured section. And detection by *count* means rotating a
leaked bot token is count-neutral, so the listener keeps polling with the old token
with no status line printed at all.

Then `/channels` reports one global runtime status replicated onto every row, and
keeps saying "polling" on the panic and teardown paths — in the module written
because "the status table kept lying".

## Current state

`src/tui/app.rs:2012-2016` — the swap, with the comment that explains it:

```rust
        if let Some(Some(cfg)) = &saved_config {
            // Save succeeded — swap in the new config NOW so the next
            // provisioner's clone sees this provisioner's writes.
            self.config = cfg.clone();
        }
```

`:2281`, `:2295`, `:2320-2322` — the detector reading the already-swapped value:

```rust
        let prev_channels_count = count_configured_channels(&self.config);
        …
        let channels_changed = new_channels_count != prev_channels_count;
        …
        if channels_changed {
            self.restart_channels();
        }
```

`grep -c 'qq' src/tui/app.rs` → **0**, while `src/config/schema.rs:2711` has the
field, `src/onboard/provision/registry.rs:170-173` offers the provisioner
unconditionally, and `src/channels/mod.rs:2950` and `:3576` start it.

`src/tui/app.rs:7334-7389` (`count_configured_channels`, 15 entries, `.is_some()`
presence) and `:7429-7523` (`channel_status_summary`, 15 entries, credential
presence) — two private copies with two different predicates, against
`src/channels/mod.rs:2622-2647`'s `channel_roster`, which is documented as the single
source of truth and has 16.

Matrix and Lark: `registry.rs:146-149` and `:159-162` offer those provisioners with
**no** `#[cfg]`, while both TUI copies gate them on the build feature.

`src/tui/commands/config.rs:406-428` — `polling_label` and `kind` are computed once
from the process-wide snapshot and applied to every row. `:364`, `:406`, `:415` —
three separate `snapshot()` calls in one panel build, which can disagree.

`src/tui/app.rs:2142-2165` — `mark_failed` is reachable only from the `Err` arm, so a
**panic** inside the spawned task reaches neither arm and the state stays `Starting`
forever; `src/channels/auto_start_state.rs:78-84` renders a `Starting` older than 5 s
as "running". `:2109-2119` and `:2126-2137` both cancel the supervisor and `return`
without calling `mark_terminated()`.

`src/channels/auto_start_state.rs:80` — `looks_running()` exists for this check and is
called nowhere; the 5-second heuristic is re-inlined four times in `config.rs`.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| TUI tests | `cargo test --lib tui::` | all pass |
| Integration | `cargo test --test tui_integration` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/tui/app.rs`, `src/tui/commands/config.rs`,
`src/channels/auto_start_state.rs`.

**Out of scope**: approval and autonomy handling in `app.rs` — plan 136, which depends
on this one; `src/channels/mod.rs` (the factory comes from plan 120, consume it, do
not edit it); the provisioners' whole-struct writes — plan 133.

## Git workflow

- Branch: `fix/tui-channel-lifecycle`
- Conventional commits, e.g. `fix(tui): restart the channel runtime after a setup save`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Detect the change before the swap, and by content

Capture a comparable value from `channels_config` **before** `self.config` is
replaced, and drive `restart_channels()` from that.

Compare by **content**, not by count — a token rotation and a one-for-one channel swap
are both count-neutral, and a stale token is the security-relevant case. Serialize the
`channels_config` to a canonical string, or derive `PartialEq`.

Fire the restart from the save-complete branch as well as from `reload_config`, so the
operator does not have to press Esc for it to happen.

Gate on an actual diff: `restart_channels` cancels and re-binds every listener, and
firing it on a save that changed nothing costs a Telegram 409 window.

**Verify**: `cargo test --lib tui::` → all pass.

### Step 2: Delete both roster copies and use the factory

Replace `count_configured_channels` and `channel_status_summary` with the single
factory/roster from plan 120. Keep the **credential-presence** predicate — it is the
better of the two and is what the status summary needs — and make it part of the
shared roster rather than a TUI-local variant.

That fixes QQ, and it fixes the Matrix/Lark offer-versus-display mismatch as a side
effect: one list cannot disagree with itself.

**Verify**: `cargo test --lib tui::` → all pass.

### Step 3: Make the status display honest

- Take **one** `snapshot()` per panel build, so a panel cannot contradict itself.
- Call `mark_terminated()` on both early-return paths in `restart_channels`.
- Wrap the spawned channels task so a panic maps to `mark_failed` — today a panic
  leaves the state at `Starting`, which renders as "running" forever.
- Use `looks_running()` instead of re-inlining the 5-second heuristic four times, or
  delete it if the per-channel work below replaces it.

Then decide the per-channel question and say which in the PR:

- **Preferred**: widen `AutoStartState` to carry a per-channel map so one dead channel
  among five does not read as Ok. This changes a contract shared with the CLI.
- **Acceptable for now**: keep the global state but label the row as a runtime-wide
  status rather than a per-channel one, so the display stops implying something it
  does not know.

**Verify**: `cargo test --lib tui::` and `cargo test --test tui_integration` → all pass.

### Step 4: Drive it

Static checks cannot show that a channel actually started. Using the tmux TUI
procedure this repo already uses for interactive testing, verify in order:

1. `/setup telegram` with a valid token — confirm the listener starts **without**
   leaving and re-entering the TUI.
2. Re-run `/setup telegram` with a different token — confirm a restart happens
   (count-neutral case).
3. `/setup` a QQ channel — confirm it appears in `/channels` and starts.
4. Kill the channels task and confirm `/channels` stops saying "polling".

Record what you observed for each. **A green test run is not sufficient evidence for
this plan** — the whole finding is that config-level success did not reach the
runtime.

## Test plan

1. `setup_save_triggers_a_restart` — **the plan's primary test**; assert a restart was
   requested after a save-complete signal without an Esc.
2. `token_rotation_triggers_a_restart` — the count-neutral case.
3. `no_change_does_not_trigger_a_restart`.
4. `roster_includes_qq` — and matches the shared factory's key set.
5. `roster_matches_the_factory_under_each_feature_set` — default and
   `--features channel-lark`.
6. `panicking_channels_task_marks_failed`.
7. `early_return_paths_mark_terminated` — both of them.
8. `panel_uses_one_snapshot`.

**Mutation check (required).** For test 1, restore the count comparison against the
already-swapped config and confirm it **fails**. For test 4, remove QQ from the shared
roster and confirm it **fails**. Restore both.

**Verify**: `cargo test --lib tui::` and `cargo test --test tui_integration` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] Both scoped test commands pass, including the eight new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -c 'fn count_configured_channels\|fn channel_status_summary' src/tui/app.rs`
      returns 0
- [ ] `grep -c 'qq' src/tui/app.rs` is no longer 0, or QQ is covered via the shared roster
- [ ] The four drive observations from step 4 are recorded in the PR body
- [ ] The step-3 per-channel decision is stated in the PR
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 135 updated

## STOP conditions

Stop and report back if:

- Plan 120 has not landed — step 2 consumes its factory, and reproducing it here would
  create a fourth copy.
- Firing the restart from the save-complete branch causes a visible flap during the
  first-run wizard, which saves several channels in sequence. If so, debounce to the
  end of the wizard rather than reverting to the count comparison.
- Widening `AutoStartState` (step 3, preferred) breaks the CLI's use of it. Take the
  labelling option instead and say so.
- The step-4 drive shows the channel starting **without** your change. That would mean
  the finding is not reproducible on this build and the premise needs rechecking.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 136 covers approval and autonomy in the same file
  and depends on this one; plan 120 owns the factory. Plan 133 fixes the provisioners'
  whole-struct writes, which is the other half of why re-running `/setup` is lossy.
- **What a reviewer should scrutinise**: that step 1 captures the pre-swap value rather
  than moving the swap — the swap exists for a reason and moving it reintroduces the
  bug it was added to fix.
- **Deliberately deferred**: the TUI's own `/allow` scope confusion and the truncated
  approval pane — plan 136.
