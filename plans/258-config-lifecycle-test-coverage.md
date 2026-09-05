# Plan 258: Cover the untested config/lifecycle critical paths and fix vacuous/flaky tests

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done. This plan is test-only — it must not change production behavior; if a test reveals a real bug, STOP and report it (do not fix production code here).
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- tests/ src/`
> If a cited test/site changed, confirm before editing.

## Status

- **Priority**: P2
- **Effort**: L (many small tests; can be split if needed)
- **Risk**: LOW (test-only)
- **Depends on**: several feature plans add their own tests (232-255); this plan covers the STANDALONE gaps not owned by another plan
- **Category**: tests
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Multiple critical paths are untested or tested vacuously, so a dangerous change lands green. This plan adds real coverage and replaces vacuous/flaky tests. (Findings whose fix-plan already adds a test — H2→239, H3→254, H4→249, H6→232, H7→244 — are NOT repeated here.)

## In-scope findings and current state (confirm before editing)

- **H1** `tests/config_api.rs:8` claims "check_auth on every route" but only 2 of 11 routes have a 401 test. `src/gateway/config_api.rs` `check_auth` call sites: `:107,:282,:357,:440,:467,:632,:732,:862,:874,:966,:994`. `set_secrets` (`:874`) and `add_mcp_server` (`:440`) are the highest-consequence unguarded ones. `spawn_test_gateway` exists in `tests/config_api.rs`.
- **H5** `src/onboard/section/login.rs:41-115` (`run`) — the console-password gate — has no seam; the `p1==p2` confirm loop and the decline-clears-idle-timeout branch are untested. (If plan 250 refactors login, coordinate.)
- **H8** ~30 HOME/env-mutating tests restore via trailing statements (leak on panic): `src/config/schema.rs:6740-6767,6777,6805,6843,6877,6945` and `src/onboard/section/persona.rs:87-107`. `HomeGuard` (`src/test_env.rs:26-51`) exists; `with_home` + `catch_unwind` done right at `approvals.rs:117`, `tests/setup_orchestration.rs:31`.
- **H9** `src/daemon/handoff.rs:286` `recording_stub_counts_calls` tests the mock, never calls `restart_daemon_for_profile_with` (`:172`); `fail_with` (`:260`) never used. (If plan 247 adds these, skip H9 here.)
- **H10** `tests/setup_orchestration.rs:188` `setup_propagates_section_failures_and_stops` asserts only `!err.is_empty()`; duplicates `:153`.
- **H11** `src/auth/profiles.rs:688` `atomic_write_replaces_file` writes once, proves neither atomicity nor replacement. (If plan 238 adds a mode/atomicity test, coordinate.)
- **H12** `src/config/schema.rs:3998` — `load_or_init` migration + credential-strip write-back untested. (If plan 240/241 covers it, skip.)
- **H13** `src/tui/commands/setup.rs:216` — asserts two resolvers return Some, never calls `SetupCommand::execute`; `TuiContext::test_context()` exists (`config.rs:550`).
- **H14** `src/config/watcher.rs:52-66` Access-event filter + `:84-90` debounce drain both unfalsifiable (sibling test discriminates on filename, not event kind).
- **H15** 7 vacuous `!description().is_empty()` section tests; TUI `DoctorCommand` (`config.rs:146`, documented past false-green) untested.
- **H16** `tests/doctor_checks.rs:24` `MOCKITO_LOCK.lock().unwrap()` poisons cascade (fix: `unwrap_or_else(|e| e.into_inner())`); `tests/onboard_mcp_section.rs:139` real 5s timer; `tests/onboard_mcp_section.rs:36` `assert_eq!` where a `>=` floor is intended; `tests/config_persistence.rs:17-97` nine default-value asserts triple-covered.
- **H17** `src/tui/first_run_wizard.rs:1226` — only back()/scroll tested; the forward state machine (`start_provisioners:193`, `advance_to_next_in_queue_or_picker:197`, `picker_submit:332`) untested.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib <module>` / `cargo test --test config_api` | pass |

**Disk constraint**: never bare `cargo test`. Env-mutating tests MUST use panic-safe guards.

## Scope

**In scope**: test files + `#[cfg(test)]` blocks for the findings above, plus a test seam where one is needed (e.g. `SetupCommand::execute` via `TuiContext::test_context`). A seam may add a `#[cfg(test)]`-visible variant of a function, but MUST NOT change production behavior.
**Out of scope**: findings whose fix-plan owns the test (H2,H3,H4,H6,H7, and H9/H11/H12 if 247/238/240 cover them — verify and skip duplicates).

## Git workflow

- Branch: `test/config-lifecycle-coverage`
- Message e.g. `test(config): cover config-API auth, watcher, setup, and fix vacuous/flaky tests`
- Do NOT push/PR unless instructed.

## Steps

Work finding-by-finding; each is independent. For each: confirm the cited site, write the test(s), verify. Suggested order (highest value first): H1, H16, H14, H13, H15, H10, H17, H8, then H5 (if not owned by 250).

- **H1**: table-drive one 401 case per (method, path) over the router; add one `check_auth` unit test for the `require_pairing==false` bypass. **Verify**: `cargo test --test config_api` → all pass.
- **H16**: `MOCKITO_LOCK` → `unwrap_or_else(|e| e.into_inner())`; make the MCP timeout injectable so the test uses ~100ms not 5s; change the curated-list `assert_eq!` to `>=`; delete the `config_persistence.rs` default-value block (covered by the schema-drift snapshot). **Verify**: `cargo test --test doctor_checks onboard_mcp_section` → pass, faster.
- **H14**: one test that opens+reads `config.toml` (Access events) and asserts NO tick within the debounce; one that issues three rapid writes and asserts exactly one tick (generous timing bounds). **Verify**: `cargo test --lib config::watcher` → pass.
- **H13**: assert `execute("runtime", &mut ctx)` → provisioner-first result; `execute("channels")` → category; `execute("full")` → wizard. **Verify**: `cargo test --lib tui::commands::setup` → pass.
- **H15**: delete the 7 metadata tests; add `DoctorCommand` tests asserting the three `provider_key_ok` states map to Ok/Warn/Ok. **Verify**: `cargo test --lib tui::commands::config` → pass.
- **H10**: rewrite to inject a failing stub section and assert the Err propagates AND later sections were not visited. **Verify**: `cargo test --test setup_orchestration` → pass.
- **H17**: multi-select options, `picker_submit`, `start_provisioners`, step `advance_to_next_in_queue_or_picker`, assert the visited sequence equals the selection order. **Verify**: `cargo test --lib tui::first_run_wizard` → pass.
- **H8**: add a generic `EnvGuard` to `src/test_env.rs` (Drop-based, like `HomeGuard`); convert the `schema.rs` + `persona.rs` sites. **Verify**: `cargo test --lib config::schema onboard::section::persona` → pass. (Coordinates with plan 261, which also touches `test_env.rs`.)
- **H5** (only if plan 250 didn't): extract `apply_login(config, answers)` pure fn and test the three branches. **Verify**: `cargo test --lib onboard::section::login` → pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] each in-scope finding has at least one real (non-vacuous) test; deleting the guarded line makes it fail (spot-check 2-3)
- [ ] no production behavior changed (`git diff` shows only tests + `#[cfg(test)]` seams)
- [ ] `git status` files are tests / test-only seams
- [ ] `plans/README.md` row updated

## STOP conditions

- A new test reveals a REAL production bug (e.g. a 401 test fails because a route is genuinely unguarded) — STOP and report; the fix belongs in the relevant feature plan (e.g. that route's auth is plan 232/234/235 territory), not here.
- A finding is already covered by its owner plan (H2/H3/H4/H6/H7/H9/H11/H12) — skip it, note the overlap.

## Maintenance notes

- Reviewer: spot-check that a couple of the new tests actually fail when the guarded line is mutated (not vacuous).
- H8's `EnvGuard` and plan 261 both touch `src/test_env.rs`; coordinate.
