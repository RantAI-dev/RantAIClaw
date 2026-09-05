# Plan 240: Stop `setup` erasing config and stop `save()` burning env overrides to disk

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/main.rs src/config/schema.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (env-override separation touches the load/save round trip; some container flows may rely on env landing in the file)
- **Depends on**: none
- **Category**: bug (data loss)
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Two config-persistence hazards:

1. **Setup config eraser** (C3). The section-wizard path does `Config::load_or_init().await.unwrap_or_default()` then `config.save()`. Any load error — bad TOML, decrypt failure, a `validate()` bail — turns `rantaiclaw setup` into a config eraser: provider keys, channel tokens, MCP servers, gateway `paired_tokens` all replaced with defaults. And `save()` never validates, so setup can persist a config the next load refuses (circular lockout — the recovery command dies the same way).
2. **Env overrides burned to disk** (C2). `load_or_init` overlays process env onto the in-memory `Config` (`apply_env_overrides`), and `save()` serializes that overlaid struct wholesale. So a daemon started with `PORT=8080` permanently pins `gateway.port=8080` in `config.toml` the first time the console writes; a CI-supplied `API_KEY` gets encrypted onto the operator's disk; `HTTP_PROXY` writes `proxy.enabled=true`. Because config-stored values outrank env vars, the leaked value then permanently shadows the env it came from.

## Current state

- `src/main.rs` (setup handler):
  ```rust
  let mut config = Config::load_or_init().await.unwrap_or_default();   // :1635
  ...
  config = updated_config;
  config.save().await?;                                                // :1655
  ```
  (The provisioner branch just above, `:1628-1630`, uses a properly-loaded config — only the section-wizard path has `unwrap_or_default`.)
- `src/config/schema.rs`: `load_or_init` calls `apply_env_overrides()` after parse (~`:4055`); `save()` (~`:4493`) serializes the in-memory struct wholesale. `apply_env_overrides` folds `PORT`, `API_KEY`/`RANTAICLAW_API_KEY`, `HTTP_PROXY`, etc. into the struct.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib config::schema` | pass |
| Test | `cargo test --lib` (filter as needed) | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/main.rs` (setup handler — do not `unwrap_or_default`)
- `src/config/schema.rs` (env-override separation so `save()` doesn't persist env-derived values)

**Out of scope**:
- The env PARSING correctness (F5) and proxy feedback loop (F1) — plan 253.
- `validate()` content — plan 235 adds the range checks; here you only ADD a validate call to the save path.

## Git workflow

- Branch: `fix/config-persistence-safety`
- Message e.g. `fix(config): don't erase config on setup or persist env overrides to disk`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Setup must not erase config on a load error

In `main.rs:1635`, replace `Config::load_or_init().await.unwrap_or_default()` with `Config::load_or_init().await?` so a load failure aborts setup with the real error instead of overwriting the file with defaults. (If a genuine "start fresh" path is wanted, that must be an explicit `--reset` flag — do NOT make it the default; note it as a follow-up.)

**Verify**: `cargo test --lib` (config/main filters) → pass; Test-plan `setup_aborts_on_unloadable_config` passes.

### Step 2: Validate before the setup save

After `config = updated_config;` (`main.rs:1654`), call `config.validate()?` before `config.save().await?` so setup cannot persist a config the loader would refuse.

**Verify**: Test-plan `setup_rejects_invalid_config` passes.

### Step 3: Keep env-derived values out of `save()`

Choose the lower-risk option and implement it:
- **Option A (preferred, smaller):** track which fields `apply_env_overrides` set (a `HashSet<&'static str>` field on `Config`, `#[serde(skip)]`), and in `save()` revert those specific fields to their pre-override disk values before serializing.
- **Option B:** keep the parsed-from-disk `Config` alongside the env-overlaid one and have `save()` write the disk-truth struct with only explicit deltas applied.

Implement A unless it proves awkward. The observable contract: after `PORT=8080 rantaiclaw <any console write>`, `config.toml` must NOT gain `gateway.port = 8080`.

**Verify**: Test-plan `env_override_not_persisted` passes.

## Test plan

- `config::schema`: `env_override_not_persisted` — set `PORT` (or `RANTAICLAW_API_KEY`) in a `HomeGuard`-scoped env (use `src/test_env.rs` guards so it's panic-safe), load, mutate an unrelated field, save, re-read the file, assert the env-derived field is ABSENT from disk.
- setup path: `setup_aborts_on_unloadable_config` — seed a malformed `config.toml`, run the setup section path, assert it errors and the file is unchanged (not defaults).
- `setup_rejects_invalid_config` — a wizard producing an invalid config → save returns Err, file unchanged.
- Verification: `cargo test --lib config::schema` + relevant main tests → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] `grep -n "load_or_init().await.unwrap_or_default()" src/main.rs` returns nothing
- [ ] scoped tests pass incl. `env_override_not_persisted`
- [ ] `git status` shows only `src/main.rs`, `src/config/schema.rs`
- [ ] `plans/README.md` row updated

## STOP conditions

- The env-override tracking requires touching many call sites of `apply_env_overrides` — report before a large refactor; Option A should be localized.
- Turning `unwrap_or_default()` into `?` breaks a legitimate first-run bootstrap where no config exists yet — check: `load_or_init` should CREATE a default on genuine absence, so `?` only fails on a real load error. If first-run breaks, report.
- `HomeGuard`/`EnvGuard` test helpers aren't in `src/test_env.rs` — coordinate with plan 261 which adds `EnvGuard`; use whatever exists, note the dependency.

## Maintenance notes

- Reviewer: confirm a container-style `PORT=…` no longer rewrites `config.toml`, and that setup aborts (not erases) on a bad config.
- Interacts with plan 253 (env parsing) and plan 235 (validate content) — all touch `apply_env_overrides`/`validate`; coordinate land order.
- The env-persistence fix also removes the mechanism by which C3-adjacent surprises happen (a value silently outranking the env it came from).
