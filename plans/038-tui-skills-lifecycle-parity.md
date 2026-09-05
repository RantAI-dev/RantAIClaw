# Plan 038: TUI skill-lifecycle parity — toggle, live reload, unified name matching, gated-row UX, dead-code cleanup

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 4736e2e..HEAD -- src/tui/app.rs src/tui/commands/mod.rs src/tui/commands/skills.rs src/onboard/provision/traits.rs src/onboard/provision/skills.rs src/onboard/provision/smoke.rs src/tui/widgets/setup_overlay.rs src/main.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: M-L
- **Risk**: MED (touches the interactive TUI picker dispatch and removes a
  cross-module dead event; low blast radius but wide file spread)
- **Depends on**: plans/037-*.md (provides `skills::set_skill_enabled`, the
  shared config-writer). Sub-steps 6 (the optional `/skill remove` / `/skill
  update` stretch) additionally depend on plans/034-*.md and plans/035-*.md; if
  those are not present, **skip Step 6 entirely** — the rest of the plan stands
  alone.
- **Category**: dx + tech-debt + bug
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

The TUI can browse and install skills but cannot **toggle** one on/off — a user
who wants to disable a skill has to drop to the CLI (plan 037) or hand-edit
`config.toml`. Three separate, subtly different name-matching rules for
`/skill`, `/skills` preselect, and the `/<skill>` direct-invoke fallback mean a
name that works in one entrypoint silently fails in another. Pressing Enter on a
**disabled/gated** row prefills a bogus `Use the X skill:` prompt for a skill
the agent can't actually load, so the user sends a request that goes nowhere.
`reload_config` refreshes model/providers/channels but **not skills**, so a
config-driven enable/disable (from the CLI or an external edit) doesn't show
until relaunch. And there's a fully **dead wizard-install code path**
(`OpenSkillInstallPicker`) that is defined and matched in four places but
**never emitted** — dead weight that misleads readers (the provisioner's module
doc still claims a ClawHub multi-select it doesn't do). This plan closes all of
these so TUI skill lifecycle reaches parity with the CLI.

## Current state

### (a) The `/skills` picker Enter dispatch — no toggle, prefills for gated rows

`src/tui/app.rs` `dispatch_list_picker_selection` (app.rs:2748) matches on the
picker kind; the `Skill` arm unconditionally prefills an invoke prompt
(app.rs:2822-2828):

```rust
            ListPickerKind::Skill => {
                // Pre-fill an invocation prompt into the input buffer.
                // The user can edit, append context, and Enter to send.
                self.context.input_buffer = format!("Use the {key} skill: ");
                self.context.cursor_to_end();
                self.refresh_autocomplete();
            }
```

`key` here is the skill name (the picker item's `key`, set to `s.name` in
`build_skill_items`, skills.rs:70-74 and `skill_picker_items`, app.rs:307-321).
The gated/disabled status lives in `self.context.available_skills_with_status`
(a `Vec<(Skill, Vec<String>)>` where a non-empty reasons vec = gated). The
picker already reserves `Ctrl+I` / `Tab` for "install deps" on gated rows
(app.rs:926-943), so a toggle needs a different key.

### (b) `reload_config` refreshes everything except skills

`src/tui/app.rs` `reload_config` (app.rs:2047-2212) reloads config, refreshes
the model label (2130-2141), providers (2146-2155), channels (2156-2192), sets
`self.config = config.clone()` (2189), then pushes the new config to the agent
actor (2197-2202) and clears `last_error` (2210). It never calls
`refresh_available_skills()`. That method exists (app.rs:300-305):

```rust
    fn refresh_available_skills(&mut self) {
        self.context.available_skills =
            crate::skills::load_skills_with_config(&self.config.workspace_dir, &self.config);
        self.context.available_skills_with_status =
            crate::skills::load_skills_with_status(&self.config.workspace_dir, &self.config);
    }
```

It is currently only called on install / install-deps / watcher / startup
(app.rs:3255, 3464, 3522, 7104) — never on a plain config reload.

### (c) Three different skill name-matching rules

1. Direct-invoke fallback in `src/tui/commands/mod.rs` `dispatch` (mod.rs:207-211)
   normalises **case AND dash/underscore**:

   ```rust
        let normalised = |s: &str| s.to_lowercase().replace('-', "_");
        if let Some(skill) = ctx
            .available_skills
            .iter()
            .find(|s| normalised(&s.name) == normalised(&cmd_name))
   ```

2. `/skill <name>` in `src/tui/commands/skills.rs` `SkillCommand::execute`
   (skills.rs:186-189) matches **case-only**, and only over
   `available_skills` (active skills — can't inspect a gated one):

   ```rust
        let found = ctx
            .available_skills
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name));
   ```

3. `/skills <name>` preselect in `SkillsCommand::execute` (skills.rs:117) passes
   the **raw** arg as the preselect key; the picker later matches it against
   `ListPickerItem.key` (which is the exact `s.name`) — so preselect is
   effectively **exact-match**:

   ```rust
        let preselect = (!trimmed.is_empty()).then_some(trimmed);
   ```

### (d) Gated-row Enter prefills a phantom prompt; `/skill <name>` can't inspect gated

See (a): Enter on a `✗ disabled/gated` row prefills `Use the X skill:` even
though `load_skills_with_config` excluded X from the agent's context, so the
request cannot use it. And `/skill <name>` (skills.rs:186-189) only searches
`available_skills`, so a gated skill returns "No skill named 'X'" instead of
showing why it's gated.

### (e) Dead wizard-install path — `OpenSkillInstallPicker` never emitted

`grep -rn "OpenSkillInstallPicker" src/` shows the variant is **defined once**
and **matched four times, but constructed nowhere**:

- Defined: `src/onboard/provision/traits.rs:49-51`
  ```rust
      OpenSkillInstallPicker {
          label: String,
      },
  ```
- Matched (never sent): `src/main.rs:2875`, `src/onboard/provision/smoke.rs:84`,
  `src/tui/app.rs:1830`, `src/tui/widgets/setup_overlay.rs:118`.

The App carries two fields solely for this dead path
(`src/tui/app.rs:199,203`):

```rust
    pub wizard_install_in_progress: bool,
    pub wizard_installed_slugs: Vec<String>,
```

initialised in ~6 constructors (app.rs:425-426 and test builders at 7597,
7701, 8067, 8252, 8422) and referenced only inside branches gated on
`wizard_install_in_progress` (app.rs:1828-1845 intercept, 3465-3467,
3573-3588) — all reachable only if `OpenSkillInstallPicker` were emitted, which
it never is.

The provisioner's module doc still advertises a ClawHub multi-select it does
not implement (`src/onboard/provision/skills.rs:1-8`):

```rust
//! Skills provisioner — implements [`TuiProvisioner`] for in-TUI skills setup.
//!
//! Mirrors the legacy flow in [`crate::onboard::section::skills`]:
//!   1. Confirm starter pack install
//!   2. Optionally browse ClawHub top-20 and multi-select
//!   3. Install selected skills
//!
//! Config writes: none (skills live in `<profile>/skills/`)
```

The actual `run` (skills.rs:45-147) installs the starter pack then just prints
"Run `/skills install` after setup" (skills.rs:116-133) — no multi-select.

### Test harness available

`src/tui/commands/skills.rs` tests (skills.rs:365-529) use
`TuiContext::test_context()`:

```rust
    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }
```

and push fixture skills onto `ctx.available_skills` (skills.rs:447-458 shows the
full `Skill { .. }` literal). `TuiApp` has state-level test builders around
app.rs:7590+ (the ones initialising `wizard_install_*`).

## Commands you will need

| Purpose        | Command                                                        | Expected on success |
|----------------|---------------------------------------------------------------|---------------------|
| Format check   | `cargo fmt --all -- --check`                                  | exit 0              |
| Lint           | `cargo clippy --all-targets -- -D warnings`                   | exit 0, no warnings |
| TUI cmd tests  | `cargo test --lib tui::commands::`                            | all pass            |
| TUI app tests  | `cargo test --lib tui::app::`                                 | all pass            |
| Dead-code grep | `grep -rn "OpenSkillInstallPicker\|wizard_install" src/`      | (see Done criteria) |

Full `cargo test` is disk-heavy — prefer `--lib`. strict-clippy-delta +
`setup_e2e` run POST-merge; run the scoped clippy above before merge. For a
live TUI smoke, the headless pattern is tmux `send-keys` / `paste-buffer` /
`capture-pane` against a binary launched with a fresh `RANTAICLAW_CONFIG_DIR`
(see the repo's TUI test notes) — optional here since the changes are covered
by unit tests.

## Scope

**In scope** (the only files you should modify):
- `src/tui/app.rs` — toggle dispatch in the Skill picker arm; add
  `refresh_available_skills()` to `reload_config`; remove the dead
  `wizard_install_*` fields + the intercept + the two gated branches; the
  shared `normalise` helper if it lives here.
- `src/tui/commands/mod.rs` — route the direct-invoke fallback through the
  shared `normalise`.
- `src/tui/commands/skills.rs` — route `/skill` and `/skills` preselect through
  the shared `normalise`; gated-row inspect fallback to
  `available_skills_with_status`; tests.
- `src/onboard/provision/traits.rs` — remove the `OpenSkillInstallPicker`
  variant.
- `src/onboard/provision/skills.rs` — fix the misleading module doc (:1-8).
- `src/onboard/provision/smoke.rs`, `src/tui/widgets/setup_overlay.rs`,
  `src/main.rs` — remove the now-orphaned match arms for the deleted variant.

**Out of scope** (do NOT touch):
- `skills::set_skill_enabled` and the config schema — the writer is delivered by
  plan 037; call it, don't reimplement or modify it.
- The ClawHub install picker flow itself (`/skills install`) — only the dead
  *wizard* path is removed, not the working command.
- `ProvisionResponse::InstalledSkills` — leave it unless clippy proves it dead
  after the variant removal (see Step 5 note).

## Git workflow

- Branch: `advisor/038-tui-skills-lifecycle-parity`
- Conventional commits, one per step, e.g.
  `feat(tui): toggle skill enabled/disabled from the /skills picker`,
  `fix(tui): refresh skills on config reload`,
  `refactor(tui): unify skill name matching behind normalise()`,
  `chore(onboard): remove dead OpenSkillInstallPicker wizard path`.
- **Repo rule: do NOT add a `Co-Authored-By` trailer.**
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a shared `normalise()` helper and route all three name matchers through it

Add one helper (case + dash/underscore fold). Put it where all three callers can
reach it — `src/tui/commands/mod.rs` already hosts the closure at mod.rs:207, so
promote it to a `pub(crate) fn` there (or a small `tui::commands` util) and
import it in `skills.rs`:

```rust
/// Canonical skill-name key for command matching: lowercase, and treat `-`
/// and `_` as equivalent. Used by `/skill <name>`, `/skills <name>` preselect,
/// and the `/<skill>` direct-invoke fallback so all three accept the same
/// spellings.
pub(crate) fn normalise_skill_name(s: &str) -> String {
    s.to_lowercase().replace('-', "_")
}
```

Then:
- mod.rs:207-211 — replace the local `normalised` closure with
  `normalise_skill_name`.
- skills.rs:186-189 (`/skill <name>`) — match with
  `normalise_skill_name(&s.name) == normalise_skill_name(name)` instead of
  `eq_ignore_ascii_case`.
- skills.rs:117 (`/skills <name>` preselect) — the picker matches preselect
  against `ListPickerItem.key` (exact `s.name`). To make preselect honour the
  same normalisation, resolve the arg to a concrete `s.name` before passing it:
  find a skill whose `normalise_skill_name(&s.name)` equals
  `normalise_skill_name(trimmed)` and pass **that skill's exact name** as the
  preselect key; fall back to the raw `trimmed` when nothing matches (preserves
  today's "matches nothing → preselect nothing" behaviour). Use
  `active_skill_status_from_context(ctx)` (skills.rs:20) as the source list so
  gated skills are also preselectable.

**Verify**: `cargo test --lib tui::commands::` → passes.
`cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Gated-row Enter shows status instead of a phantom prompt; `/skill <name>` inspects gated skills

In `src/tui/app.rs` `dispatch_list_picker_selection`, `ListPickerKind::Skill`
arm (app.rs:2822-2828), look up the selected `key` in
`self.context.available_skills_with_status`; if its reasons vec is **non-empty**
(gated/disabled), do NOT prefill — show a status/remediation system message
instead:

```rust
            ListPickerKind::Skill => {
                let gated = self
                    .context
                    .available_skills_with_status
                    .iter()
                    .find(|(s, _)| s.name == key)
                    .map(|(_, reasons)| reasons.clone())
                    .filter(|r| !r.is_empty());
                if let Some(reasons) = gated {
                    let msg = format!(
                        "'{key}' is not active — {}. Enable it with `rantaiclaw skills enable {key}` \
                         (or Ctrl+I to install missing deps), then reload.",
                        reasons.join("; ")
                    );
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".to_string(), msg));
                } else {
                    self.context.input_buffer = format!("Use the {key} skill: ");
                    self.context.cursor_to_end();
                    self.refresh_autocomplete();
                }
            }
```

In `src/tui/commands/skills.rs` `SkillCommand::execute` (skills.rs:185-217),
when `available_skills` has no match, fall back to
`available_skills_with_status` so a gated skill can still be inspected (render
its detail panel, and add a line noting it's gated + why) instead of returning
"No skill named 'X'". Only return the not-found `Message` when it appears in
neither list.

**Verify**: `cargo test --lib tui::` → passes. Add the tests from the Test plan
now if convenient.

### Step 3: Add a skill enable/disable toggle key in the `/skills` picker

Pick an unused key in the Skill-picker key handler (near app.rs:926-943 where
`Ctrl+I`/`Tab` are handled) — e.g. `Ctrl+E` (mnemonic: enable/disable) — that,
for `ListPickerKind::Skill`, reads the currently-highlighted item, calls
`crate::skills::set_skill_enabled(&self.config, &key, !currently_enabled)` (from
plan 037), then:

1. `self.config = updated;` (adopt the returned mutated config),
2. persist: `updated.save().await` — the TUI runs inside tokio, so await
   directly, or reuse the existing `tokio::spawn` reload idiom (app.rs:2198),
3. `self.refresh_available_skills();` so the picker rows re-render with the new
   status,
4. rebuild the picker items in place (mirror the item-rebuild at app.rs:3476-3483)
   so the `✗` glyph flips live,
5. push the agent-reload request (same `TurnRequest::Reload` used at
   app.rs:2199-2201) so the running agent's skill set updates without relaunch.

Determine `currently_enabled` from `available_skills_with_status` (reasons
containing `"disabled in config.toml"` ⇒ currently disabled). Keep the handler
small; if the branch grows past ~30 lines, extract a
`fn toggle_selected_skill(&mut self, key: &str) -> anyhow::Result<()>`.

Surface a one-line system message on success (`✓ {name} enabled/disabled`) and
on error set `self.context.last_error`.

**Verify**: `cargo build` → compiles. `cargo clippy --all-targets -- -D warnings`
→ exit 0. (Behaviour is covered by Step 6 tests + optional tmux smoke.)

### Step 4: Refresh skills on config reload

In `src/tui/app.rs` `reload_config`, after `self.config = config.clone();`
(app.rs:2189) and before the agent-reload spawn (app.rs:2197), add:

```rust
        // Config reload can flip `skills.entries.<name>.enabled` (CLI
        // `skills enable/disable`, an external edit, or the in-picker toggle).
        // Refresh the cached skill lists so `/skills`, autocomplete, and the
        // next agent turn reflect the new set — model/providers/channels are
        // already refreshed above; skills were the missing surface.
        self.refresh_available_skills();
```

(`refresh_available_skills` reads `self.config`, which was just set at 2189, so
placement after that line is required.)

**Verify**: `cargo test --lib tui::app::` → passes. Add the
`reload_config_refreshes_skills` test from the Test plan.

### Step 5: Remove the dead `OpenSkillInstallPicker` wizard path

This is the widest-spread but lowest-risk change (removing code that never
runs). Do it as its own commit so it can be reverted independently.

1. `src/onboard/provision/traits.rs` — delete the `OpenSkillInstallPicker`
   variant (traits.rs:44-51, including its doc comment).
2. Remove its match arms (now non-exhaustive-error until removed):
   - `src/main.rs:2875-2880`
   - `src/onboard/provision/smoke.rs:84-90`
   - `src/tui/widgets/setup_overlay.rs:118-122`
   - `src/tui/app.rs:1828-1845` — the whole `match ev { OpenSkillInstallPicker
     => ..., other => forward }` collapses to just forwarding every event to the
     overlay; simplify the two-phase drain back to a plain
     `while let Ok(ev) = rx.try_recv() { if let Some(o) = &mut self.setup_overlay
     { o.handle_event(ev); } }` (verify the surrounding block at app.rs:1819-1855
     to keep the exact forwarding semantics).
3. Remove the now-unreachable fields and their uses:
   - `src/tui/app.rs:199,203` field declarations,
   - every initialiser (`grep -rn "wizard_install_in_progress\|wizard_installed_slugs" src/`
     — app.rs:425-426 plus test builders 7597-7598, 7701-7702, 8067-8068,
     8252-8253, 8422-8423),
   - the two gated branches at app.rs:3465-3467 and app.rs:3573-3588 (the
     `if self.wizard_install_in_progress { ... }` blocks). At 3465-3467 keep the
     surrounding `refresh_available_skills()` + picker rebuild; only the inner
     `if self.wizard_install_in_progress { ... }` push is removed.
4. `ProvisionResponse::InstalledSkills` may become unused after this. Run
   `grep -rn "InstalledSkills" src/` — if it is now referenced **only** by its
   own definition, remove that variant and any remaining arms too; if anything
   still constructs it, leave it. State which you did.
5. Fix the misleading module doc in `src/onboard/provision/skills.rs:1-8` — drop
   the "Optionally browse ClawHub top-20 and multi-select / Install selected
   skills" bullets and describe what `run` actually does: install starter pack,
   then point the user to `/skills install`.

**Verify**: `cargo build` → compiles (all matches exhaustive again).
`grep -rn "OpenSkillInstallPicker" src/` → **no matches**.
`grep -rn "wizard_install" src/` → **no matches**.
`cargo clippy --all-targets -- -D warnings` → exit 0 (no dead-code warnings).

### Step 6 (STRETCH — only if plans 034 and 035 have landed): `/skill remove` / `/skill update`

If `plans/034-*.md` (skill remove logic) and `plans/035-*.md` (skill update
logic) exist and are implemented, add `/skill remove <name>` and `/skill update
<name>` subcommands to `SkillCommand::execute` (skills.rs:146) that reuse their
logic (call the same functions those plans exported — do not duplicate). Mirror
the existing `/skill install` sub-routing at skills.rs:155-163. If either plan
is absent, **skip this step** and note it in the status row.

**Verify**: `cargo test --lib tui::commands::` → passes.

## Test plan

Add tests to `src/tui/commands/skills.rs` `mod tests` (skills.rs:365; model on
`skill_command_with_known_name_shows_details` at :444 for the `Skill { .. }`
fixture literal) and to `src/tui/app.rs` tests (use the state builders near
app.rs:7590):

1. `normalise_matches_across_entrypoints` — assert `normalise_skill_name` folds
   both case and dash/underscore: `image-lab`, `Image_Lab`, `IMAGE-LAB` all map
   to the same key; then assert `/skill Image_Lab` resolves a fixture skill
   named `image-lab`, and the direct-invoke fallback (`dispatch("/image_lab")`)
   resolves the same skill.
2. `skills_preselect_normalises` — `/skills Image-Lab` (SkillsCommand::execute)
   yields a picker whose preselect key equals the fixture's exact name
   `image-lab` (not the raw `Image-Lab`).
3. `gated_skill_enter_shows_status_not_prefill` — push a fixture into
   `available_skills_with_status` with a non-empty reasons vec, drive the Skill
   picker Enter dispatch, and assert `input_buffer` is empty (no `Use the …`
   prefill) and a system message mentioning the gating reason was appended.
4. `skill_command_inspects_gated_skill` — a skill present only in
   `available_skills_with_status` (gated) returns an InfoPanel (not the
   "No skill named" Message).
5. `reload_config_refreshes_skills` — build a `TuiApp` state, point its config
   at a temp profile whose `config.toml` disables a fixture skill, call
   `reload_config`, and assert `available_skills` no longer contains it. (If a
   full `reload_config` needs on-disk config the test can't easily stage, cover
   the narrower invariant instead: after `refresh_available_skills()` runs with
   a config that disables the skill, `available_skills` excludes it — and add a
   comment that Step 4 wires this into `reload_config`.)

Verification: `cargo test --lib tui::` → all pass, including the new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib tui::` exits 0; new tests (normalise/preselect/
      gated-enter/gated-inspect/reload-refresh) exist and pass
- [ ] `grep -rn "OpenSkillInstallPicker" src/` returns no matches
- [ ] `grep -rn "wizard_install" src/` returns no matches
- [ ] `reload_config` calls `refresh_available_skills()`
      (`grep -n "refresh_available_skills" src/tui/app.rs` shows a call between
      the `self.config = config.clone()` and the agent-reload spawn)
- [ ] The `/skills` picker toggle calls `crate::skills::set_skill_enabled`
      (`grep -n "set_skill_enabled" src/tui/app.rs` returns ≥1)
- [ ] All three name matchers call `normalise_skill_name`
      (`grep -rn "normalise_skill_name" src/tui/` returns ≥3)
- [ ] `src/onboard/provision/skills.rs` module doc no longer claims a ClawHub
      multi-select
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `skills::set_skill_enabled` does not exist or its signature is not
  `(&Config, &str, bool) -> Result<(Config, String)>` — plan 037 has not landed
  or drifted; STOP (do not reimplement the writer here).
- `grep -rn "OpenSkillInstallPicker" src/` shows a **construction site** (an
  `OpenSkillInstallPicker { .. }` that is *sent*, not matched) — then the path
  is NOT dead; do not remove it, STOP and report (the "wire it up" alternative
  becomes the correct choice and needs a design decision).
- Removing `wizard_install_*` leaves a compile error you cannot resolve without
  touching a file outside the in-scope list.
- The excerpts at app.rs:2822-2828 / 2047-2212 / 926-943, mod.rs:207-211,
  skills.rs:117/186-189, or the four `OpenSkillInstallPicker` arms do not match
  live code (drift).
- A step's verification fails twice after a reasonable fix attempt.

## Maintenance notes

For the human/agent who owns this after it lands:

- The toggle and the CLI (plan 037) share `skills::set_skill_enabled` — a change
  to entries-keying or the resolver must keep both surfaces working; add a test
  on both sides.
- `normalise_skill_name` is now the single source of truth for skill-name
  matching in the TUI. If a new entrypoint matches skill names, route it through
  this helper rather than adding a fourth rule.
- Reviewer should scrutinise: (a) the gated-row branch actually distinguishes
  gated from active (reasons vec non-empty), (b) the dead-code removal did not
  drop a live forwarding path in the `drain_events` two-phase loop
  (app.rs:1819-1855), and (c) the picker item-rebuild after a toggle keeps the
  cursor on the toggled row.
- Deferred: hot config-file-watch that auto-toggles without a manual reload; a
  bulk enable/disable-all in the picker. Revisit if requested. Step 6
  (`/skill remove`/`update`) is deferred whenever plans 034/035 are absent.
