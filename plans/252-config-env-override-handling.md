# Plan 252: Fix env-override parsing, the proxy resurrection loop, and CONFIG_DIR/WORKSPACE split-brain

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/config/schema.rs src/tools/proxy_config.rs`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (env-override semantics; a wrong change alters how deployments are configured)
- **Depends on**: coordinates with plan 240 (also touches `apply_env_overrides`/`save`)
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Three env-handling defects in the config core:

- **F1 (proxy resurrection loop)**: `apply_env_overrides` sets proxy env when enabled with NO clear-on-disable branch, then on the next load re-reads the `HTTP_PROXY` it wrote itself and flips `proxy.enabled` back to `true`. So `[proxy] enabled = false` never takes effect; combined with plan 240 it rewrites `enabled=true` to disk. Also `std::env::set_var` runs from the gateway reload task while workers call `std::env::var` — a data race on glibc `setenv`.
- **F5 (silent env-parse fallback)**: `allow_public_bind`/`web_search.enabled` are assigned unconditionally from `val=="1"||"true"`, so `WEB_SEARCH_ENABLED=yes` DISABLES web search; invalid `PORT`/temperature/timeout are silently discarded with no warning. A strict parser (`parse_proxy_enabled`) exists but is used once.
- **F6 (CONFIG_DIR/WORKSPACE split-brain)**: with both `RANTAICLAW_CONFIG_DIR` and `RANTAICLAW_WORKSPACE` set, `config_path` comes from one and `workspace_dir` from the other — skills/memory/workspace-policy resolve against a different tree than the config.

## Current state (confirm before editing)

- `src/config/schema.rs`:
  - `apply_env_overrides` proxy branch (`:4486-4488`): `self.proxy.apply_to_process_env()` when enabled, no `else` clear. `set_proxy_env_pair` (`:1747-1755`) writes the env; the SAME function reads `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` back at `:4434-4456`, and `:4458-4463` re-sets `enabled=true` when no explicit `RANTAICLAW_PROXY_ENABLED` is set.
  - Bool parsing: `:4326` `allow_public_bind = val == "1" || val.eq_ignore_ascii_case("true")`; `:4354` same for `web_search.enabled`. Invalid discards: `:4310-4312` (PORT), `:4331-4335` (temperature), `:4381-4385` (max_results), `:4392-4396` (timeout). `parse_proxy_enabled` (`:1938-1944`) accepts `1|true|yes|on`/`0|false|no|off`, used only for `RANTAICLAW_PROXY_ENABLED` (`:4424-4427`). Exemplar warns: `:4278-4281`, `:4299-4302`.
  - Split-brain: `resolve_runtime_config_dirs` (`:3676-3686`) returns early on `RANTAICLAW_CONFIG_DIR`, deriving `workspace_dir` as `<config_dir>/workspace`, never consulting `RANTAICLAW_WORKSPACE`; but `apply_env_overrides` (`:4264-4270`) unconditionally overwrites `workspace_dir` from `RANTAICLAW_WORKSPACE` afterward. `ConfigResolutionSource` already records the source.
- Exemplar clear-on-transition: `src/tools/proxy_config.rs:254-259` has the `else if previous_scope == Environment { clear_process_env() }` the config core is missing.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib config::schema` | pass |

**Disk constraint**: never bare `cargo test`. Env-mutating tests MUST use the panic-safe guards in `src/test_env.rs` (see plan 261 / `HomeGuard`).

## Scope

**In scope**: `src/config/schema.rs` (env parsing, proxy transition, split-brain).
**Out of scope**: env-override PERSISTENCE to disk (plan 240); default-value drift (plan 253).

## Git workflow

- Branch: `fix/config-env-override-handling`
- Message e.g. `fix(config): strict env-bool parsing, clear disabled proxy env, honor CONFIG_DIR precedence`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (F5): route env bools through the strict parser and warn on bad input

Rename `parse_proxy_enabled` to `parse_env_bool` and use it for `allow_public_bind` (`:4326`) and `web_search.enabled` (`:4354`). Add `tracing::warn!` on every `parse`/range rejection (`:4310`, `:4331`, `:4381`, `:4392`), matching `:4278-4281`.

**Verify**: Test-plan `web_search_enabled_yes_is_true` and `invalid_port_warns` pass.

### Step 2 (F1): clear proxy env on disable; don't re-read self-written vars

Add the clear-on-transition branch in `apply_env_overrides` mirroring `proxy_config.rs:254-259`. Record which proxy env vars THIS process authored (a static or a passed set) so the read side (`:4434-4456`) skips them and cannot resurrect a disabled proxy.

**Verify**: Test-plan `disabled_proxy_stays_disabled_across_reload` passes.

### Step 3 (F6): honor CONFIG_DIR precedence for workspace

Make the `RANTAICLAW_WORKSPACE` branch in `apply_env_overrides` (`:4264-4270`) a no-op when `resolve_runtime_config_dirs`' source was `EnvConfigDir`, so the documented precedence holds for both `config_path` and `workspace_dir`.

**Verify**: Test-plan `config_dir_wins_over_workspace_split` passes.

## Test plan

- `web_search_enabled_yes_is_true` — `WEB_SEARCH_ENABLED=yes` → enabled (not disabled).
- `invalid_port_warns` — a non-numeric `PORT` → the config port is kept AND a warning is emitted (assert via a tracing capture or that the value is unchanged + documented).
- `disabled_proxy_stays_disabled_across_reload` — set `[proxy] enabled=false`, run `apply_env_overrides` twice; assert `enabled` stays false.
- `config_dir_wins_over_workspace_split` — set both env vars; assert `workspace_dir` derives from `CONFIG_DIR`, not `WORKSPACE`.
- Use `HomeGuard`/`EnvGuard` (panic-safe) for all env mutation.
- Verification: `cargo test --lib config::schema` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped tests pass with the new tests
- [ ] `grep -n 'val == "1"' src/config/schema.rs` shows the bool parsing now routes through `parse_env_bool`
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Tracking self-authored proxy env vars requires process-global state that races the reload task — a `OnceLock<HashSet>` or a field threaded through is acceptable; report if neither is clean.
- Any cited excerpt doesn't match — STOP that finding, continue others.

## Maintenance notes

- Reviewer: confirm `WEB_SEARCH_ENABLED=yes` no longer disables it, and a disabled proxy stays disabled across a reload.
- Interacts with plan 240 (persistence) — both edit `apply_env_overrides`; land order matters.
