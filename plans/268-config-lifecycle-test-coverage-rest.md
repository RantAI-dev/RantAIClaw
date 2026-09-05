# Plan 268: Config-lifecycle test coverage — the rest of 258

> **Follow-up to plan 258 / PR #668.** Plan 258 shipped H1 + H16 (mockito lock). The remaining seven vacuous/absent-coverage findings (H5, H8, H10, H13, H14, H15, H17) were split off. This file was reconstructed from 258's finding descriptions.

## Status

- **Priority**: P2
- **Effort**: M (seven independent findings)
- **Risk**: LOW (tests only)
- **Depends on**: none (H8 coordinates with `test_env.rs`; H5 coordinates with plan 250)
- **Category**: tests

## Findings (each independent; cited sites verified against current code)

- **H13** — `SetupCommand::execute` routing was untested: the tests asserted the `category_from_arg`/`provisioner_for` resolvers but never called `execute`. **DONE (this PR)**: `execute("full") → OpenFirstRunWizard`, `execute("runtime") → OpenSetupOverlay{provisioner}` (provisioner-first), `execute("channels") → OpenSetupCategory{category}` (category fallback).
- **H15** — the TUI `DoctorCommand` (`config.rs`), which had a documented past false-green (`!model.is_empty()` reported "✓ Model configured" on a keyless install), was untested. **DONE (this PR)**: `doctor_model_row_reflects_provider_key_state` asserts the three `provider_key_ok` states map to Ok / Warn / Ok on the "Model configured" row.

### Deferred (not in this PR — each is its own small follow-up)

- **H5** — `src/onboard/section/login.rs` `run` (console-password gate) has no seam; the `p1==p2` confirm loop and the decline-clears-idle-timeout branch are untested. Needs extracting an `apply_login(config, answers)` pure fn first. **Coordinate with plan 250** (setup-honesty) before refactoring login.
- **H8** — ~30 HOME/env-mutating tests restore via trailing statements (leak on panic). Add a generic `EnvGuard` to `src/test_env.rs` (Drop-based, like the existing `HomeGuard`) and convert the `schema.rs` + `onboard/section/persona.rs` sites. Mechanical but large.
- **H10** — `tests/setup_orchestration.rs` `setup_propagates_section_failures_and_stops` asserts only `!err.is_empty()` and duplicates a sibling. Rewrite to inject a failing stub section and assert the Err propagates AND later sections were not visited.
- **H14** — `src/config/watcher.rs` Access-event filter + debounce drain are unfalsifiable (the sibling test discriminates on filename, not event kind). Add one test that reads `config.toml` (Access events) and asserts NO tick within the debounce, and one that issues three rapid writes and asserts exactly one tick. **Timing-sensitive** — needs generous bounds to stay deterministic (§3.7); deferred to give it that care.
- **H17** — `src/tui/first_run_wizard.rs` forward state machine (`start_provisioners`, `advance_to_next_in_queue_or_picker`, `picker_submit`) is untested (only back/scroll are). Assert the visited sequence equals the selection order.
- **Vacuous `!description().is_empty()` tests** — scattered across `src/tools/*`, `src/persona/` etc. (17 sites); deleting them wholesale is low-value and risks removing a command's only test. Left as-is pending a per-site pass.

## Verification (this PR)

- `cargo test --lib tui::commands` → 107 pass (incl. the 4 new tests).

## Done criteria

- [x] `SetupCommand::execute` routing pinned (H13) — PR #675
- [x] `DoctorCommand` model-row status pinned across the three key states (H15) — PR #675
- [x] **H10** setup stop-on-failure — private `run_section_sweep` seam + stub-section unit test; misleading integration test removed — PR #676
- [x] **H17** first-run wizard forward machine (walk + picker index-order) — PR #676
- [x] `cargo test --lib tui::commands` + `first_run_wizard` pass

### Also done (second pass, after the owner asked to do everything doable)

- [x] **H14** — extracted `is_actionable_event_kind` (pure predicate) + `debounce_loop` (async fn); tested the filter directly and the debounce with `#[tokio::test(start_paused = true)]` — **deterministic, not flaky** (the flakiness was an artifact of testing through real fs events) — #677
- [x] **H8** — added a Drop-based `EnvGuard` to `test_env.rs`; converted 41 tests / 67 env-var ops in `schema.rs` + `persona.rs` (proxy multi-var tests correctly excluded) — #678
- [x] **H5** — extracted `password_pair_is_valid` + `clear_login` from `login.rs::run` (behaviour-identical; hashing/gate/precedence untouched, §7.5) and tested both — #679

### Still NOT done (net-negative, deliberately left)

- **Vacuous `!description().is_empty()` tests** (~17 scattered sites across `tools/`, `persona/`, …) — deleting adds NO coverage and risks removing a command's only test. Net neutral-to-negative; left as-is.

## Maintenance notes

- The new tests are non-vacuous: H13 fails on any routing change; H15 fails if the keyless-install false-green regresses.
- H8's `EnvGuard` touches `test_env.rs`; sequence it against any other `test_env.rs` work.
