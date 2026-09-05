# Plan 253: Align config defaults across serde, `impl Default`, and the drift gate

> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/config/schema.rs src/config/migrations.rs tests/schema_drift.rs docs/reference/config.md`
> Mismatch on any cited excerpt = STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED (aligning serde defaults up to `impl Default` widens behavior for configs that omit those keys — that is the point, but it needs a migration arm + schema version bump)
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Config defaults have three different answers depending on how you arrive, and the governance gate that is supposed to catch this doesn't:

- **F2 (3-way drift)**: `HttpRequestConfig` (all 4 fields), `BrowserConfig.enabled`, `WebSearchConfig.enabled`, `block_high_risk_commands`, `CostConfig.prices` disagree between the serde `#[serde(default = ...)]` value, the `impl Default` value, and `docs/reference/config.md`. A fresh install gets `allowed_domains=["*"]`; an operator hand-adding `[http_request]\nenabled=true` per the docs gets `[]`, which rejects EVERY HTTP request — the tool silently does nothing. `block_high_risk_commands` is inverse (file path enforces `true`, fresh install gets `false`), so two machines on the same version have different shell policy.
- **F3 (missing serde defaults)**: `[autonomy]` (`level`, `workspace_only`, `allowed_commands`, `forbidden_paths`, `max_actions_per_hour`, `max_cost_per_day_cents`) plus `default_temperature`, `channels.cli`, `memory.backend/auto_save`, `heartbeat.enabled/interval_minutes`, `observability.backend` have no serde default → a partial section (`[autonomy]\nlevel="full"`, exactly what the docs teach) fails the whole load with "missing field".
- **F4 (gate blind to Default)**: the schema-drift gate fingerprints `schema_for!(Config)` (serde side) only, not `Config::default()`, so CLAUDE.md's "defaults are fingerprinted" is false for the values a fresh install actually gets — every F2 drift shipped green.

## Current state (confirm before editing)

- `src/config/schema.rs` divergences: `:1262-1274` (serde `HttpRequestConfig`) vs `:1284-1292` (`impl Default`); `:1203-1206` vs `:1237-1240` (`BrowserConfig.enabled`); `:1299-1301` vs `:1334-1338` (`WebSearchConfig.enabled`); `:2207-2208` (`#[serde(default="default_true")]`) vs `:2270-2271` (`block_high_risk_commands: false`); `:723-725` vs `:758-759` (`CostConfig.prices`). Missing serde defaults: `AutonomyConfig` `:2184-2200`; `default_temperature` `:86`; `channels.cli` `:2746`; `memory.backend/auto_save` `:2023-2025`; `heartbeat` `:2595-2597`; `observability.backend` `:2156`.
- `CLAUDE.md:110` documents the `impl Default` side as intended; `src/config/migrations.rs:162-173` records the v8→v9 change to those values; `docs/reference/config.md:275,305-308,399` still document the serde side.
- Exemplar of the fix pattern: `GatewayConfig` (`:1054-1068`) delegates `impl Default` fields to the same `default_*()` fns the `#[serde(default = ...)]` attribute names.
- Gate: `tests/schema_drift.rs:31-44` fingerprints `schema_for!(Config)`. Snapshot `tests/snapshots/schema_drift__config_schema@v23.snap` records the serde side. `CURRENT_VERSION` in `src/config/migrations.rs:36`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib config::schema` | pass |
| Test | `cargo test --lib config::migrations` | pass |
| Snapshot | `cargo test --test schema_drift` (then `cargo insta accept` if intended) | pass after accept |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**: `src/config/schema.rs`, `src/config/migrations.rs` (a new `migrate_vN` arm + `CURRENT_VERSION` bump), `tests/schema_drift.rs` (add the Default snapshot), `docs/reference/config.md` (correct the documented defaults).
**Out of scope**: env-override parsing (plan 252); dead keys (plan 257).

## Git workflow

- Branch: `fix/config-default-alignment`
- Message e.g. `fix(config): unify serde/Default/doc defaults and fingerprint Config::default()`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Pick one source of truth per field and delegate both sides to it

For each F2 field, make the `impl Default` field delegate to the SAME `default_*()` fn the `#[serde(default = ...)]` attribute names (mirror `GatewayConfig:1054-1068`). Use the `impl Default` VALUES as the intended contract (per CLAUDE.md:110): serde defaults rise to match (`allowed_domains=["*"]`, `enabled=true`, `block_high_risk_commands` per the documented intent, `CostConfig.prices=get_default_pricing()`).

**Verify**: `cargo test --lib config::schema` compiles; Test-plan `serde_and_default_agree` passes.

### Step 2: Add serde defaults to the required-field sections

Add `#[serde(default = "...")]` per F3 field (or `#[serde(default)]` at struct level if `impl Default` is the intended set) so a partial `[autonomy]` section parses.

**Verify**: Test-plan `partial_autonomy_section_loads` passes.

### Step 3: Fingerprint `Config::default()`

Add a test `config_defaults_do_not_drift_unannounced` next to the existing drift test that `insta::assert_snapshot!`s `toml::to_string_pretty(&Config::default())` (config_path/workspace_dir are `#[serde(skip)]`, already excluded), suffixed by `CURRENT_VERSION`.

**Verify**: `cargo test --test schema_drift` — the new snapshot's first diff IS the F2 inventory; review it, then `cargo insta accept` intentionally.

### Step 4: Migration arm + version bump + docs

Add a `migrate_vN` arm for the widened defaults (bump `CURRENT_VERSION` in `migrations.rs:36`), and correct `docs/reference/config.md:275,305-308,399` to match the new defaults.

**Verify**: `cargo test --lib config::migrations` → pass; `grep` the doc for the old default values returns nothing.

## Test plan

- `serde_and_default_agree` — for each F2 field, `serde_json::from_str("{}")`-style default equals `Config::default()`'s value.
- `partial_autonomy_section_loads` — `[autonomy]\nlevel="full"` (only) parses without a missing-field error.
- `config_defaults_do_not_drift_unannounced` — the new Default snapshot.
- Verification: `cargo test --lib config::schema config::migrations` + `cargo test --test schema_drift` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped tests pass; the Default snapshot exists and is intentional
- [ ] `CURRENT_VERSION` bumped with a matching `migrate_vN` arm
- [ ] `docs/reference/config.md` no longer states the old serde-side defaults
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Widening a default would change an EXPOSURE boundary (e.g. `allow_public_bind`) — those must NOT be widened; keep exposure defaults deny-by-default and note any that looked drifted.
- The migration arm can't preserve an existing operator's explicit value — a migration must only fill ABSENT keys, never overwrite a set one; report if that's not achievable.

## Maintenance notes

- Reviewer: confirm no exposure boundary was widened, and that the new Default snapshot is reviewed (not blindly accepted).
- This makes finding F4's gate real — future `impl Default` edits now fail the snapshot until acknowledged.
- Overlaps plan 235 (temperature range in `validate()`) — coordinate so `default_temperature`'s default + range live consistently.
