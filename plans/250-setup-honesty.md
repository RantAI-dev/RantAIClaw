# Plan 250: Make setup honest about approvals, provider keys, and login state

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Each finding below names an exact `file:line` — open it and confirm the excerpt/behavior before editing. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/onboard/section/approvals.rs src/onboard/provision/approvals.rs src/onboard/provision/provider.rs src/doctor/checks/provider.rs src/onboard/wizard.rs src/onboard/section/provider.rs src/onboard/provision/login.rs src/tui/commands/config.rs`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED (approvals/login are security-adjacent; a wrong change could weaken a gate)
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Five setup-honesty defects where the tool tells the operator one thing and does another:

- **E1** `setup approvals --force` is a no-op: `write_policy_files(..., false)` hardcodes `force=false`, so an existing preset never changes, yet the summary prints the SELECTED preset ("Approval policy set: Strict") while the runtime keeps the old one. There is currently no in-product way to change the preset.
- **E2** Anthropic key validation + `provider.ping` use `Bearer`; Anthropic needs `x-api-key` + `anthropic-version`, so a valid key is reported rejected and the operator is pushed to replace a working credential.
- **E5** Setup treats an exported-but-EMPTY env var as "key detected" (`env::var(...).is_ok()`), skips the prompt, and finishes with an unusable config.
- **E7** `setup`/`doctor` disagree on "has a key": `ProviderSection` checks only the top-level `api_key`, writes only there, so a console-configured key (`provider_api_keys`) reads absent → re-prompts every run + duplicates.
- **E8** TUI login provisioner says "left disabled" on empty/mismatched password but clears nothing → the old password gate stays armed.
- **F11** `/config <key> <value>` advertises persistence it doesn't have (a 2-key session toggle).

## Current state (confirm each before editing)

- **E1**: `src/onboard/section/approvals.rs:52` — `policy_writer::write_policy_files(ctx.profile, preset, false)`; `src/onboard/provision/approvals.rs:115` — identical hardcoded `false`, then `:131-137` emits `Done { summary: format!("Approval policy set: {label}") }` with the SELECTED label. `write_policy_files` (`src/approval/policy_writer.rs:402-437`) leaves existing files untouched when `force=false` and only warns for `PolicyPreset::Off`.
- **E2**: `src/onboard/provision/provider.rs:382-385` — `probe_get(&validation_url, &[("Authorization", &format!("Bearer {api_key}"))])`; `src/doctor/checks/provider.rs:103-106` — `req.bearer_auth(key)` for all providers. The correct header selection exists at `src/onboard/wizard.rs:1579-1589` (sets `anthropic-version: 2023-06-01`, picks `x-api-key` vs `Bearer`) and `src/auth/anthropic_token.rs:31-50` (`detect_auth_kind`).
- **E5**: `src/onboard/wizard.rs:2598,2615,2621,2653` — `std::env::var("...").is_ok()`; correct idiom 160 lines later at `:2788-2790` (`.ok().is_some_and(|v| !v.trim().is_empty())`). Also `:2642,:2763` use untrimmed `key.is_empty()`.
- **E7**: `src/onboard/section/provider.rs:33-43` — `is_already_configured` checks `config.api_key` only; `:55-59` writes only `ctx.config.api_key`. Correct resolver: `config.resolve_key_for_provider(provider)` (`src/config/schema.rs:3893`), as `src/doctor/checks/config.rs:102` uses.
- **E8**: `src/onboard/provision/login.rs:115,141-147` — `leave_disabled(...)` only messages, clears nothing; the explicit "Skip/disable" branch at `:89-91` is the only one that clears `username`/`password_hash`/`idle_timeout_secs`.
- **F11**: `src/tui/commands/config.rs:72-74` (`usage`), `:86-98` (panel footer), `:120-137` (set arm handles only `model`/`debug`, never touches `Config`/`save()`).

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib onboard` | pass |
| Test | `cargo test --lib doctor::checks::provider` | pass |
| Test | `cargo test --lib tui::commands::config` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: the files listed in the drift check.
**Out of scope**: the doctor `skipped`/vacuous-check honesty (plan 251); the legacy doctor (plan 256).

## Git workflow

- Branch: `fix/setup-honesty`
- Message e.g. `fix(onboard): honor --force, use per-provider keys, and fix login/anthropic/env honesty`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (E1): Thread `force` into `write_policy_files` and report the effective preset

Pass the section/provisioner `force` flag into `write_policy_files` (both `section/approvals.rs:52` and `provision/approvals.rs:115`). When `force=false` and a marker already exists with a DIFFERENT preset, return a warning and report the EFFECTIVE (on-disk) preset in the `Done` summary, not the offered one. `force=true` overwrites — guard hand-edited allowlist files with a confirm or note the overwrite.

**Verify**: Test-plan `approvals_force_changes_preset` and `approvals_without_force_reports_effective` pass.

### Step 2 (E2): Build Anthropic probe headers via the shared helper

Extract the header selection from `wizard.rs:1579-1589` into one helper keyed off `auth::anthropic_token::detect_auth_kind`, and have `provision/provider.rs:382` and `doctor/checks/provider.rs:103` build probe headers through it (`x-api-key` + `anthropic-version` for Anthropic).

**Verify**: Test-plan `anthropic_probe_uses_x_api_key` passes.

### Step 3 (E5): Treat an empty env var as absent

Add `fn env_key_present(name: &str) -> bool { std::env::var(name).ok().is_some_and(|v| !v.trim().is_empty()) }`, apply at `wizard.rs:2598,2615,2621,2653`; trim before `is_empty()` at `:2642,:2763`.

**Verify**: Test-plan `empty_env_key_still_prompts` passes.

### Step 4 (E7): Provider section reads/writes the shared key store

`ProviderSection::is_already_configured` calls `resolve_key_for_provider(default_provider)`; new keys write into `provider_api_keys[provider]` (the store every consumer reads), not the top-level slot.

**Verify**: Test-plan `console_configured_key_not_reprompted` passes.

### Step 5 (E8): `leave_disabled` clears the login state it claims to

Move the three `config.gateway.login.* = None/0` writes into `leave_disabled` (pass `&mut Config`) so the message and state agree. Align the mismatch behavior across `wizard::setup_login`, `LoginSection`, `LoginProvisioner`.

**Verify**: Test-plan `login_mismatch_does_not_leave_gate_armed` passes.

### Step 6 (F11): Make `/config` honest about session-only scope

Change `usage()` to `/config [model|debug] [value]`, append "(session only — use /setup to persist)" to both set messages, and list the supported keys in the unknown-key branch. `debug` should reject non-bool input instead of coercing to false.

**Verify**: `cargo test --lib tui::commands::config` → pass.

## Test plan

- `approvals_force_changes_preset`, `approvals_without_force_reports_effective` — model after `run_mirrors_the_marker_on_disk_not_the_offered_preset` (`section/approvals.rs:222`).
- `anthropic_probe_uses_x_api_key` — assert the Anthropic probe headers include `x-api-key` + `anthropic-version`, not `Bearer`.
- `empty_env_key_still_prompts` — `VAR=""` → the wizard still prompts (does not report "detected").
- `console_configured_key_not_reprompted` — a key in `provider_api_keys` → `is_already_configured` true.
- `login_mismatch_does_not_leave_gate_armed` — mismatch on an enable path clears the gate (or re-prompts), never silently keeps the old password.
- Verification: `cargo test --lib onboard doctor::checks::provider tui::commands::config` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped tests pass with the new tests
- [ ] `grep -n "write_policy_files(ctx.profile, preset, false)" src/onboard/section/approvals.rs` returns nothing
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `write_policy_files`'s signature can't take `force` without cascading — report.
- `detect_auth_kind` no longer exists (drift) — STOP.
- Any excerpt doesn't match the cited line — STOP for that finding, continue the others, report which drifted.

## Maintenance notes

- Reviewer: E1 and E8 are security-adjacent — confirm the EFFECTIVE preset/login state is what's reported, not the offered one.
- E7 overlaps the legacy doctor (`legacy.rs:353` also reads only top-level `api_key`) — plan 256 removes that; note the linkage.
