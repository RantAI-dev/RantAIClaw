# Plan 242: Produce valid, encrypted TOML from the OpenClaw migration

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/migration/openclaw.rs src/migration/mod.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (changes migration output formatting; covered by an idempotency guard + a new round-trip test)
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

`rantaiclaw migrate --from openclaw` hand-rolls TOML by string concatenation with no escaping, from a foreign `openclaw.json`. Three defects:

1. **TOML injection / invalid output.** A skill slug containing `.`, a space, or `]`, or a value containing `"` / newline, produces wrong nesting or invalid TOML. A hostile `openclaw.json` can inject arbitrary keys — including security-relevant ones (`[gateway] require_pairing`, `allow_public_bind`, `allowed_users = ["*"]`) — by writing a value that closes its quote and emits a newline.
2. **Plaintext credential.** A `literal` skill API key is written to `config.toml` in plaintext (`value = "…"`), bypassing `Config::save`'s encryption.
3. **Reports success anyway.** The port failure is swallowed to a `tracing::warn!`, so the user sees "Migrated N skills…" and a `config.toml` that cannot load, or a leaked secret.

## Current state

`src/migration/openclaw.rs` (`:349-414`) — string-concatenated TOML:
```rust
toml_out.push_str(&format!("[skills.entries.{slug}]\n"));      // :353 — raw slug in a table header
...
for (k, v) in env { if let Some(s)=v.as_str() {
    toml_out.push_str(&format!("{k} = {}\n", toml::Value::String(s.into()))); } }   // :367 — value OK, key raw
...
toml_out.push_str(&format!("source = \"{s}\"\n"));   // :378 — raw, unescaped
toml_out.push_str(&format!("id = \"{s}\"\n"));       // :381
toml_out.push_str(&format!("value = \"{s}\"\n"));    // :384 — plaintext credential, unescaped
...
other => format!("\"{}\"", other.to_string().replace('"', "\\\"")),   // :398 — doesn't handle backslash
...
fs::write(dest_config_toml, existing)                // :414 — no encryption, umask mode
```
The `env` map two lines up (`:367`) does it correctly via `toml::Value::String` — the unsafe path is the outlier. The port error is swallowed at `~:201-206` (`tracing::warn!`).

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib migration` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/migration/openclaw.rs` (build a typed `toml::Table`/`toml::Value` and serialize; route persistence through `Config::save`; surface port failures)
- `src/migration/mod.rs` — only if the summary needs a failure field

**Out of scope**:
- Other migration sources (zeroclaw) beyond what shares this code path.
- The `count_top_level_blocks` heuristic (a known cosmetic issue, not in scope).

## Git workflow

- Branch: `fix/openclaw-migration-toml-safety`
- Message e.g. `fix(migrate): serialize OpenClaw import via the toml crate and encrypt migrated secrets`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Build the skills block as a typed `toml::Value`, not a string

Replace the `format!`-concatenation (`:349-406`) with construction of a `toml::Table` for `skills.entries`: use `toml::Value::Table`/`String`/`Boolean` for every field, and validate each slug against `^[A-Za-z0-9_-]+$` (skip or sanitize a slug that fails, and record it). Serialize with `toml::to_string`. This makes escaping and nesting correct by construction.

**Verify**: Test-plan `migration_handles_hostile_slug_and_value` passes.

### Step 2: Merge and persist through `Config::save` so encryption + 0600 apply

Instead of `fs::write(dest_config_toml, existing)` (`:414`), parse the destination config, merge the migrated `skills.entries` into it as typed values, and persist via `Config::save` (or replicate its atomic 0600 write) so a `literal` skill `api_key.value` is encrypted at rest like every other credential. After writing, parse the merged file once to prove it round-trips.

**Verify**: Test-plan `migrated_literal_key_is_encrypted` passes.

### Step 3: Surface a port/parse failure instead of swallowing it

Change the `tracing::warn!` swallow (~`:201-206`) so a `port_skills_entries` failure is reflected in the `MigrationSummary` (a visible line / non-zero failure count), not just a log. The command must not report "Migrated N skills" when the write produced an unloadable config.

**Verify**: Test-plan `migration_failure_is_reported` passes.

## Test plan

- `migration`: `migration_handles_hostile_slug_and_value` — an `openclaw.json` with a slug containing `.`/`]`/space and a value containing `"` and a newline → the output parses as valid TOML and does NOT gain an injected top-level key (assert no `[gateway]`/`allowed_users` appears from a value payload). Use neutral marker payloads, no real secrets.
- `migrated_literal_key_is_encrypted` — a `literal` api_key in the source → the persisted config's value is `enc2:`-prefixed (encrypted), not plaintext (assert the plaintext marker is absent from the file).
- `migration_failure_is_reported` — force a write/parse failure → the summary reflects it (not a bare success).
- Verification: `cargo test --lib migration` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] `grep -n 'push_str(&format!("\[skills.entries' src/migration/openclaw.rs` returns nothing (no string-concatenated TOML tables)
- [ ] scoped migration tests pass with the 3 new tests
- [ ] `git status` shows only in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `Config::save` can't be reused for the merge because the migration runs before a full `Config` exists — replicate the atomic 0600 write + the secret-encryption call explicitly, note it.
- The merge would need to touch `src/config/schema.rs` encryption internals — report before widening scope.

## Maintenance notes

- Reviewer: confirm a hostile value cannot inject a top-level key (the injection test) and that a migrated literal key lands encrypted.
- Rotation: any credential that passed through a migration on an affected build was written plaintext — recommend rotating it.
