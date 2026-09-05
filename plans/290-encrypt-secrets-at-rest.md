# Plan 290: Encrypt the two credential paths that still write plaintext at rest

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/config/schema.rs src/mcp/setup.rs src/migration/openclaw.rs src/profile/mod.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P1 (ledger W1-3, part a)
- **Effort**: M
- **Risk**: MED — needs a migration for values already on disk in plaintext
- **Category**: security
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

`decrypt_config_secrets` is documented as the single authority on which fields are encrypted,
and every provider key, KB key, Telegram token and skill key goes through it. Two credential
paths bypass it entirely:

1. **MCP server env.** Notion, Slack and GitHub tokens configured for an MCP server live in
   `config.toml` as plaintext. The config API redacts them on the wire, which makes the gap
   easy to miss — the exposure is the file on disk.
2. **Migrated configs.** The OpenClaw/ZeroClaw importer appends the source config verbatim,
   so a plaintext `api_key` in the source stays plaintext, written with the process umask
   rather than `0600`.

There is also a write-only artefact: `write_secrets` maintains `secrets/api_keys.toml`, which
nothing reads. It suggests a separation that does not exist.

## Current state (verified at `4b8f61e`)

```rust
// src/config/schema.rs:3759 — the authority; mcp_servers is absent from it
pub(crate) fn decrypt_config_secrets(
```

```rust
// src/mcp/setup.rs:232 — writes the second, unread copy
pub fn write_secrets(profile: &Profile, env: &[(String, String)]) -> Result<()> {
// src/mcp/setup.rs:14 — module doc still advertises it
//!    `<profile>/secrets/api_keys.toml`.
```

```rust
// src/migration/openclaw.rs:240 — verbatim string append, no encryption
pub fn translate_config(source_toml: &str) -> (String, usize) {
```

The correct helper already exists in the same file: `write_config_0600`
(`src/migration/openclaw.rs:507`), used by the skills path but not by the config path.

`rg -n 'api_keys.toml' src/` shows the only other reference is the migration copying it.

## Steps

1. **Bring `mcp_servers.*.env` under the encryption authority.** Add it to the encrypt side
   of `save()` and to `decrypt_config_secrets`. Follow exactly how an existing nested secret
   (e.g. the knowledge API keys) is handled — do not invent a second mechanism.
   **Verify**: the round-trip test at `src/config/schema.rs:5587`
   (`decrypt_config_secrets_round_trips_every_drift_prone_field`) covers the new field.

2. **Migrate values already on disk.** Existing configs hold these in plaintext. Add a
   migration arm that encrypts them in place, and bump the schema version following the
   pattern PR #695 used for `trusted_authserv_id` (schema snapshots live in
   `tests/snapshots/`).
   **Verify**: `cargo test --test schema_drift` passes with the regenerated snapshots.

3. **Delete `write_secrets`, or make it the single source.** It has no reader. Deleting it is
   the smaller change and removes a misleading artefact; update the module doc at
   `src/mcp/setup.rs:14` in the same commit.
   **Verify**: `rg -n 'api_keys.toml' src/` returns only the migration's historical copy path.

4. **Make the migration write encrypted and `0600`.** Route the translated config through
   `write_config_0600` and encrypt known credential fields before writing.
   **Verify**: a test that migrates a fixture containing a plaintext `api_key` and asserts
   (a) the resulting file mode is `0600`, and (b) the value on disk is not the plaintext.

5. **Decide the encrypted-source case.** A ZeroClaw source may already hold `enc2:` values
   that a fresh profile's key cannot decrypt. Either carry `.secret_key` across (as the legacy
   path does) or fail loudly with an actionable message. Do not import values that will never
   decrypt.
   **Verify**: state the chosen behaviour in the PR body and cover it with a test.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib config`, `cargo test --lib migration`, `cargo test --test schema_drift`
  all pass.
- No plaintext credential is written by either path; both new tests fail if reverted.

## STOP conditions

- The schema bump would collide with another in-flight schema change → STOP and coordinate.
- Encrypting `mcp_servers.env` breaks MCP spawn because the value is read somewhere that does
  not decrypt → STOP; find every reader first (`rg -n 'mcp_servers' src/`).

## Test plan

Round-trip test extension for the config field; two migration tests (mode + ciphertext).
Use neutral placeholder credentials — never a real token, per CLAUDE.md §9.1.

## Maintenance note

`decrypt_config_secrets` is the authority by contract. Any new credential-shaped config field
must be added there in the same PR that introduces it.

## Rollback

Carries a schema bump, so a rollback to a pre-bump binary needs the config migrated back.
State that in the PR body per CLAUDE.md §3.8.
