# Plan 251: Make doctor report skipped/failed checks honestly and validate routing

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/doctor/mod.rs src/doctor/report.rs src/doctor/checks/system_deps.rs src/doctor/checks/channels.rs src/doctor/checks/config.rs src/main.rs src/tui/first_run_wizard.rs src/onboard/provision/runtime_surfaces/model_routes.rs`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW–MED (doctor output shape changes; a CI recipe may parse it)
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Doctor and first-run report false confidence:

- **E3** CLI `doctor` calls `run_all`, which discards the `skipped` list that #624 added, so `--brief` prints "7/7 ok" on a run that never probed provider/channels/mcp — exactly the "all-green while nothing was probed" failure #624 set out to eliminate, still on the surface most operators/CI use.
- **E4** `system.deps` uses `spawn_blocking(...).unwrap_or_default()` → on a JoinError it reports all binaries present (vacuous green). Also `sha256sum` is `shasum` on macOS.
- **E6** Online `channels.auth` returns "no channels configured" for an incomplete WhatsApp block that offline `inspect_channels` correctly fails — the two doctor modes contradict.
- **E10** First-run wizard's `is_channel_name` uses a hardcoded 16-entry array while the picker is registry-driven → the next channel added without editing the array loops the user back to the picker with no way forward.
- **E11** Nothing validates `model_routes[].provider` (provisioner, `validate()`, or doctor) → a typo'd provider is accepted everywhere and fails at routing time.

## Current state (confirm before editing)

- **E3**: `src/doctor/mod.rs:158-184` — `run_all_detailed` returns `DoctorRun { results, skipped }`; `src/gateway/api_v1.rs:399,418` consumes it. `src/main.rs:2103` calls `run_all` (the wrapper that discards `skipped`, `doctor/mod.rs:148-150`). `render_text`/`render_json`/`render_brief` (`doctor/report.rs:22-89`) have no notion of skipped.
- **E4**: `src/doctor/checks/system_deps.rs:23-25` — `spawn_blocking(...).await.unwrap_or_default()` → `DepsReport::default()` falls through to `CheckResult::ok` at `:56-64`. Siblings handle JoinError honestly (`daemon.rs:23-25` `unwrap_or(Unsupported)`; `config.rs:192-194` maps to `Err`). `sha256sum` in `RECOMMENDED` (`:8`).
- **E6**: `src/doctor/checks/channels.rs:175-207` — WhatsApp branch pushes to ok/bad/warn only if the cloud pair or `session_path` is set; otherwise falls through, and if it's the only channel `n_total==0` → `Severity::Info` "no channels configured". Offline `inspect_channels` (`:98-115`) fails correctly, pinned by `whatsapp_with_no_credentials_returns_fail` (`:398-413`).
- **E10**: `src/tui/first_run_wizard.rs:86-103` — `CHANNEL_PROVISIONER_NAMES` hardcoded array backs `is_channel_name` (`:1168-1170`); `channel_options()` (`:1197-1210`) is registry-derived from `provision::available()`. Phase transition `:203-217` routes a non-`is_channel_name` provisioner back to `PickChannels`.
- **E11**: `src/onboard/provision/runtime_surfaces/model_routes.rs:124-127,145-150` accepts any non-empty provider; `src/config/schema.rs:4144-4155` (`validate`) only rejects empty; `src/doctor/checks/config.rs:23-47` validates `default_provider`/`fallback_providers` but not `model_routes`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib doctor` | pass |
| Test | `cargo test --lib tui::first_run_wizard` | pass |
| Test | `cargo test --lib onboard::provision` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: the files in the drift check.
**Out of scope**: the legacy doctor removal (plan 256); the setup-honesty items (plan 250).

## Git workflow

- Branch: `fix/doctor-check-honesty`
- Message e.g. `fix(doctor): surface skipped/failed checks and validate model routes`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (E3): CLI doctor surfaces the skipped list

Switch `main.rs:2103` to `run_all_detailed`; add a `skipped: &[String]` parameter to `report::render_text`/`render_json`/`render_brief`; print a `[skipped]` block in text/brief and a `"skipped"` array in JSON.

**Verify**: Test-plan `brief_reports_skipped_checks` passes.

### Step 2 (E4): `system.deps` fails honestly on a probe error; fix macOS binary name

Return `CheckResult::warn("probe task failed")` on JoinError instead of a defaulted report. Make `sha256sum` platform-conditional (`shasum` on macOS).

**Verify**: `cargo test --lib doctor::checks::system_deps` → pass.

### Step 3 (E6): online WhatsApp check matches offline

Add an `else` that pushes `"whatsapp: incomplete credentials"` into `bad` when neither the cloud pair nor `session_path` is usable; extend the existing test to run through `probe_channels` for that case.

**Verify**: Test-plan `online_whatsapp_incomplete_fails` passes.

### Step 4 (E10): derive `is_channel_name` from the registry

Delete `CHANNEL_PROVISIONER_NAMES`; define `is_channel_name(n)` as `provisioner_for(n).is_some_and(|p| p.category() == ProvisionerCategory::Channel)`. Add a test asserting `channel_options()` and `is_channel_name` agree over the whole registry.

**Verify**: Test-plan `channel_name_matches_registry` passes.

### Step 5 (E11): validate `model_routes`/`embedding_routes` providers

Reuse `provider_validation_error` over `model_routes`/`embedding_routes` inside `ConfigSchemaCheck` (`doctor/checks/config.rs`), and validate the typed provider against `providers::list_providers()` in the model_routes provisioner before accepting it. Add a duplicate-`hint` check.

**Verify**: Test-plan `typo_provider_in_route_is_flagged` passes.

## Test plan

- `brief_reports_skipped_checks` — a run with skipped checks shows them in brief/JSON.
- `system_deps_join_error_warns` — a forced JoinError → warn, not ok.
- `online_whatsapp_incomplete_fails` — an incomplete whatsapp block → Fail online (matching offline).
- `channel_name_matches_registry` — the two channel-name sources agree over the registry.
- `typo_provider_in_route_is_flagged` — a `model_routes` entry with an unknown provider → doctor Fail / provisioner reject.
- Verification: `cargo test --lib doctor tui::first_run_wizard onboard::provision` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped tests pass with the new tests
- [ ] `grep -n "run_all(" src/main.rs` shows the CLI doctor now uses `run_all_detailed`
- [ ] `grep -n "CHANNEL_PROVISIONER_NAMES" src/tui/first_run_wizard.rs` returns nothing
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- A CI recipe parses `doctor` stdout/JSON in a fixed shape — the skipped block is additive; if it would break a parser, add it behind `--format json` only and note it.
- Any cited excerpt doesn't match — STOP that finding, continue the rest, report.

## Maintenance notes

- Reviewer: E3 is the crux — confirm `--brief` no longer prints "N/N ok" when checks were skipped.
- E10 is latent (the arrays currently match) but guaranteed to fire on the next channel addition — the registry-derived version is the durable fix.
