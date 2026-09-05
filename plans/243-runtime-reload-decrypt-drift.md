# Plan 243: Thread secret decryption through the runtime reload path

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/channels/routing.rs src/config/schema.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED (reload path handing secrets to the runtime; `load_or_init` has side effects the reload must not inherit)
- **Depends on**: none
- **Category**: bug / security
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

The channel runtime reload path decrypts only ONE secret and skips migration + validation — the same failure class as #565/#567 (a hot-reload path handing ciphertext to the runtime), relocated. `load_runtime_defaults_from_config_file` parses `config.toml` directly, decrypts only `config.api_key`, and never runs `migrations::migrate()` or `Config::validate()`. So any other encrypted field (`provider_api_keys`, knowledge keys, `web_search.brave_api_key`, telegram `bot_token`) stays `enc2:`-prefixed in the channel runtime's view, and an older on-disk shape is deserialized against the current schema. Separately, `gateway_agents.*.api_key` is in neither the encrypt nor the decrypt list, so it is stored plaintext at rest while every comparable credential is encrypted.

## Current state

- `src/channels/routing.rs`:
  ```rust
  pub(crate) fn decrypt_optional_secret_for_runtime_reload(...) {...}   // :332 — a 4th private decrypt copy
  pub(crate) async fn load_runtime_defaults_from_config_file(path: &Path) -> ... {   // :349
      let contents = tokio::fs::read_to_string(path).await?;
      let mut parsed: Config = toml::from_str(&contents)?;              // :355 — no migrate
      parsed.config_path = path.to_path_buf();
      if let Some(rantaiclaw_dir) = path.parent() {
          let store = SecretStore::new(rantaiclaw_dir, parsed.secrets.encrypt);
          decrypt_optional_secret_for_runtime_reload(&store, &mut parsed.api_key, "config.api_key")?;   // :361 — api_key ONLY
      }
      parsed.apply_env_overrides();                                     // :364 — no validate
      ...
  }
  ```
- `src/config/schema.rs`: `decrypt_config_secrets` (~`:3746-3824`) is documented as "the single authority on WHICH fields are encrypted"; `save()`'s encrypt side is ~`:4502-4574`. `Config.agents` (`:215`) is covered; `Config.gateway_agents` (`:220`, `GatewayAgentConfig.api_key` at `:306`) is in neither list.
- `load_or_init` (schema.rs) is the full read→migrate→parse→decrypt→validate path but has side effects (profile migrations, the write-back in plan 241). The reload needs the side-effect-FREE half.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib config::schema` | pass |
| Test | `cargo test --lib channels::routing` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/config/schema.rs` (factor a side-effect-free `Config::load_from_path(&Path)` that runs migrate→parse→decrypt_all→validate; add `gateway_agents.*.api_key` to the encrypt + decrypt lists)
- `src/channels/routing.rs` (use `load_from_path`; delete `decrypt_optional_secret_for_runtime_reload`)

**Out of scope**:
- The migration write-back atomicity (plan 241).
- Refactoring `load_or_init`'s side effects — only EXTRACT its pure core.

## Git workflow

- Branch: `fix/runtime-reload-decrypt-drift`
- Message e.g. `fix(config): decrypt all secrets and migrate on the channel runtime reload path`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Add `gateway_agents.*.api_key` to the encrypt/decrypt lists

In `decrypt_config_secrets` (`schema.rs:~3746`) and `save()`'s encrypt side (`~:4502`), add the `gateway_agents` per-entry `api_key`, mirroring how `Config.agents` is handled. Extend the existing `decrypt_config_secrets_round_trips_every_drift_prone_field` test (`schema.rs:~5357`) to cover it.

**Verify**: `cargo test --lib config::schema` → pass; the round-trip test now includes `gateway_agents`.

### Step 2: Factor a side-effect-free `Config::load_from_path`

Extract the pure core of `load_or_init` into `pub(crate) fn/async fn Config::load_from_path(path: &Path) -> Result<Config>` that: reads the file, runs `migrations::migrate` (in memory only — NO write-back here), parses to `Config`, runs the FULL `decrypt_config_secrets`, and runs `validate()`. It must NOT do profile migrations or write to disk.

**Verify**: `cargo test --lib config::schema` → pass; Test-plan `load_from_path_decrypts_all_and_migrates` passes.

### Step 3: Use `load_from_path` in the runtime reload; delete the 4th decrypt copy

In `routing.rs:349`, replace the manual parse+single-decrypt with `Config::load_from_path(path)`. Delete `decrypt_optional_secret_for_runtime_reload` (`:332`). Keep `apply_env_overrides` if the runtime reload intends env precedence (it currently calls it — preserve that behavior).

**Verify**: `cargo test --lib channels::routing` → pass; Test-plan `runtime_reload_decrypts_provider_keys` passes.

## Test plan

- `config::schema`: `load_from_path_decrypts_all_and_migrates` — seed a `config.toml` at an older `schema_version` with an encrypted `provider_api_keys` entry; assert `load_from_path` returns a migrated, fully-decrypted config (the key is plaintext in memory, `enc2:`-free).
- `channels::routing`: `runtime_reload_decrypts_provider_keys` — assert the runtime defaults built from a config with an encrypted non-`api_key` secret see the decrypted value, not `enc2:…`.
- Extended `decrypt_config_secrets_round_trips_every_drift_prone_field` covering `gateway_agents`.
- Verification: `cargo test --lib config::schema channels::routing` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] `grep -n "decrypt_optional_secret_for_runtime_reload" src/` returns nothing (the 4th copy is gone)
- [ ] scoped tests pass with the new tests
- [ ] `git status` shows only the two in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- Extracting `load_from_path` pulls in a side effect that can't be cleanly separated (e.g. profile creation is entangled) — report; implement Step 1 (the `gateway_agents` gap) independently, defer 2-3.
- Adding `validate()` to the reload path starts rejecting configs that reload today — report the rule; the reload may need to tolerate a validation warning rather than hard-fail mid-run.

## Maintenance notes

- Reviewer: confirm a non-`api_key` encrypted secret is decrypted on reload (test), and that `gateway_agents.api_key` now round-trips through encryption.
- `load_from_path` becomes the shared "read a config from disk without side effects" primitive — future readers (doctor, TUI status) should use it instead of re-parsing.
