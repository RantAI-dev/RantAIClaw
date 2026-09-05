# Plan 235: Validate config-API writes before persisting

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/gateway/config_api.rs src/config/schema.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH (validate() is stricter than the current write path; a partially-configured console state that used to save may start returning 400)
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

The gateway config write handlers mutate fields and persist WITHOUT running `Config::validate()`, which runs only on load. Consequences:

- `PUT /api/v1/config/autonomy {"max_actions_per_hour": 0}` persists, then the next `Config::load_or_init()` hits `validate()`'s `max_actions_per_hour != 0` bail and the daemon refuses to start — the console can brick the daemon with a valid-looking request.
- `PUT /api/v1/config/model {"temperature": 99}` persists an out-of-range temperature (the CLI clap parser rejects `0.0..=2.0`, the console does not), which then 400s every provider call far from the Config panel.
- `always_ask` / `auto_approve` entries are assigned unvalidated while the neighbouring `allowed_commands` IS validated entry-by-entry — a typo silently fails open (exact-match never fires) and is echoed back as if enforced.

After this lands: writes are validated before persist and return `400` with the offending field, so a bad value is rejected at the API boundary instead of bricking the daemon or silently failing open.

## Current state

- `src/gateway/config_api.rs`:
  - `set_model` (`:277-314`): `if let Some(t) = body.temperature { cfg.default_temperature = t; }` (`:300`) — no range check; then `persist_and_swap`.
  - `set_autonomy` (`:352-421`): assigns `always_ask`/`auto_approve` raw (`:363-368`), `max_actions_per_hour` raw (`:385`); ONLY `allowed_commands` is validated (`:369-381`, via `validate_allow_basename`, returns `err_400` on bad). Then `persist_and_swap`.
  - `persist_and_swap` (`:253-257`): `cfg.save()` then swap into state. This is the single choke point every write passes through.
- `src/config/schema.rs`: `Config::validate(&self) -> Result<()>` (~`:4094`) already checks `gateway.host`, the login pair, `autonomy.max_actions_per_hour != 0` (~`:4130`), scheduler fields, route fields, the ollama-cloud combination, and `proxy.validate()`. It does NOT currently check `default_temperature` range or `web_search.max_results` (those live only in the env-override path). See plan 253 for moving those into `validate()`; here, ADD the temperature range to `validate()` if plan 253 hasn't, or rely on it if it has.
- Tool-name registry: `always_ask`/`auto_approve` should validate against the registered tool names; find the enumerator used by the approval layer (`src/approval/`), allowing the `"*"` wildcard for `always_ask`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib gateway::config_api` | pass |
| Test | `cargo test --test config_api` | pass |
| Test | `cargo test --lib config::schema` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/gateway/config_api.rs` (`persist_and_swap` gains a validate call; `set_autonomy` validates the tool lists; `set_model` validates temperature)
- `src/config/schema.rs` (ONLY to add a `default_temperature` range check to `Config::validate` if not already present — coordinate with plan 253)

**Out of scope**:
- The env-override parsing path (plan 253).
- `forbidden_paths` handling — the floor is enforced in `policy.rs`; leave the config-API acceptance as-is.

## Git workflow

- Branch: `fix/config-api-write-validation`
- Message e.g. `fix(config): validate config-API writes before persisting`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Validate in `persist_and_swap`

In `persist_and_swap` (`config_api.rs:253`), before `cfg.save()`, call `cfg.validate().map_err(err_400)?`. This makes every write handler reject a config the loader would refuse, at the API boundary.

**Verify**: `cargo test --lib gateway::config_api` compiles + passes; the Test-plan `autonomy_zero_max_actions_is_rejected` passes.

### Step 2: Range-check temperature on write

Add `default_temperature` range validation. Preferred: add `if !(0.0..=2.0).contains(&self.default_temperature) { bail!(...) }` inside `Config::validate` (schema.rs ~`:4094`) so Step 1 covers it automatically AND the CLI/load paths share one rule. If plan 253 already added it, skip. In `set_model`, no extra code is then needed beyond Step 1.

**Verify**: `cargo test --lib config::schema` passes; Test-plan `out_of_range_temperature_is_rejected` passes.

### Step 3: Validate the autonomy tool lists

In `set_autonomy`, before assigning `always_ask`/`auto_approve` (`:363-368`), validate each entry against the registered tool names (allow `"*"` for `always_ask` only). On an unknown name return `err_400` naming it — mirror the `allowed_commands` branch immediately below. If reworking to reject is too strict for an existing config, at minimum WARN-and-echo the unknown names in the response for this release; document which you chose in the PR.

**Verify**: Test-plan `unknown_always_ask_tool_is_rejected` passes.

## Test plan

Use `tests/config_api.rs` (`spawn_test_gateway`) and the in-file `#[cfg(test)]` block:
- `autonomy_zero_max_actions_is_rejected` — `PUT autonomy {max_actions_per_hour:0}` → 400, and the persisted config is unchanged (daemon still bootable).
- `out_of_range_temperature_is_rejected` — `PUT model {temperature:99}` → 400.
- `unknown_always_ask_tool_is_rejected` — `PUT autonomy {always_ask:["not_a_tool"]}` → 400 (or warn-and-echo if you chose that; assert accordingly).
- A regression assertion that a VALID autonomy/model write still succeeds (so validation isn't over-tight).
- Verification: `cargo test --lib gateway::config_api config::schema` + `cargo test --test config_api` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped config_api + config::schema tests pass with new tests
- [ ] `PUT autonomy {max_actions_per_hour:0}` returns 400 (asserted by test)
- [ ] a valid write still succeeds (asserted by test)
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `Config::validate` no longer exists or its `max_actions_per_hour` rule changed (drift) — STOP.
- Adding the validate call makes many EXISTING valid-looking writes fail (validate is stricter than expected, e.g. it fires on a half-configured login) — STOP and report the specific rule; the fix may need to scope validation to the mutated section rather than the whole config.
- No tool-name enumerator is findable for Step 3 — implement Steps 1-2, mark Step 3 deferred, report.

## Maintenance notes

- Reviewer: confirm a VALID write still succeeds (validation not over-tight) and that the zero-max-actions brick is closed by test.
- This overlaps finding F9 (file-supplied values skipping range checks) — Step 2's move into `validate()` also fixes the file path. Coordinate with plan 253 so the temperature range lives in exactly one place.
- If `set_autonomy` chose warn-and-echo over reject, a follow-up should flip it to reject after one release.
