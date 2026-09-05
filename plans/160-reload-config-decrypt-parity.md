# Plan 160: One decrypt pass — `reload_config` must stop drifting from `load_or_init`

> **Executor instructions**: Follow this plan step by step. One PR. Run every
> verification command. If anything in "STOP conditions" occurs, stop and
> report. When done, add/update this plan's row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat <planned-at>..HEAD -- src/tui/app.rs src/config/schema.rs`
> Line numbers below are from the PR #564 branch (`crew/158-tui-model-switch-not-applied`,
> commit `6ab9e96`). Land this AFTER #564 merges; re-anchor if the diff is
> non-empty.

## Status

- **Priority**: P2 — mid-session credential breakage for Telegram-channel and
  skill-key operators on every config reload; latent until a reload fires,
  then every affected call 401s until restart
- **Effort**: S
- **Risk**: MEDIUM (touches the secret path; mitigated by moving existing
  code, not writing new logic)
- **Depends on**: PR #564 (plan 158) merged — it added the
  `provider_api_keys` block this plan consolidates
- **Category**: bugfix / drift-killer
- **Planned at**: 2026-08-18, found during plan 158's live drive

## Why this matters

`Config::load_or_init` (schema.rs, decrypt pass ending ~`schema.rs:4023`) and
`TuiApp::reload_config` (app.rs) each maintain a hand-written list of which
secrets to decrypt. The lists have drifted **twice already**:

1. KB keys were once missing from `reload_config` (fixed earlier — see the
   "mirrors the decrypt pass" comment in `reload_config`).
2. `provider_api_keys` was missing until plan 158's live drive surfaced it as
   a 401 on every watcher reload (fixed in PR #564).

Still missing from `reload_config` today:

- `channels_config.telegram.bot_token` (`schema.rs:4002-4010`) —
  `reload_config` hands `self.config` to `restart_channels`; after a reload
  the Telegram channel polls with an `enc2:` blob as its token.
- `skills.entries.*.api_key.value` where `source == "literal"`
  (`schema.rs:4013-4023`) — skill tools read a blob after reload.

Patching the two stragglers one by one repeats the defect class. The fix is
the drift-killer: ONE function owns the list, both callers consume it.

## Step 1 — Extract `decrypt_config_secrets` in `schema.rs`

Move the entire decrypt sequence from `load_or_init` (from the first
`decrypt_optional_secret(&store, &mut config.api_key, …)` through the skills
loop ending at `schema.rs:4023`) into:

```rust
/// Decrypt every at-rest-encrypted secret in `config`, in place.
///
/// The single authority on WHICH fields are encrypted. `load_or_init` and
/// the TUI's `reload_config` both call this; before it existed each kept a
/// hand-copy of the list, and the copies drifted twice (KB keys, then
/// `provider_api_keys` — the latter 401'd every provider call after a
/// config-watcher reload until PR #564).
pub(crate) fn decrypt_config_secrets(store: &SecretStore, config: &mut Config) -> Result<()> {
    // …moved bodies, verbatim: api_key, composio, browser.computer_use,
    // web_search.brave, storage db_url, agents.*, provider_api_keys.*,
    // telegram bot_token, skills literal keys, knowledge.* …
    Ok(())
}
```

Move code verbatim — no new logic. `load_or_init` calls it where the inline
sequence used to be. Keep `apply_env_overrides`/`validate` outside (they are
not decryption).

## Step 2 — `reload_config` consumes it

Replace the whole inline decrypt sequence in `TuiApp::reload_config`
(everything between constructing `store` and `config.apply_env_overrides()`)
with:

```rust
        crate::config::schema::decrypt_config_secrets(&store, &mut config)?;
```

This deletes the drifted copy AND closes the telegram + skills gaps in one
move.

## Step 3 — Tests

In `schema.rs` tests: round-trip test — build a config with a known plaintext
telegram token + one literal skill key + one provider_api_keys entry, `save()`
it (encrypts), re-read raw and confirm `enc2:` blobs on disk, then run
`decrypt_config_secrets` and assert all three fields equal the original
plaintext. Use neutral fixtures (`test_token_value`, `rantaiclaw_user`) — no
realistic key literals (GitHub push protection rejects them).

Mutation-proof: comment the telegram arm inside `decrypt_config_secrets` —
the round-trip test must fail on the token assert. Restore.

## Step 4 — Validation

```bash
cargo fmt --all -- --check
cargo test --lib config::
cargo test --lib tui::
```

Clippy: compare error set against main (local 1.97 emits ~169 pre-existing
diagnostics; the set must not grow).

## Non-goals

- No change to `save()`'s encrypt side.
- No decrypt for fields that are not encrypted at rest today.
- Channel-runtime hot-reload paths outside the TUI keep their own loading
  (they re-run `load_or_init`, which already decrypts).

## Risk and rollback

- Risk: moving secret-handling code; mitigated by verbatim moves and the
  round-trip test. Behaviour change is strictly "two more fields decrypted
  on reload" — the load_or_init path is bit-identical.
- Rollback: revert the commit; reload_config returns to its (drifted) copy.

## STOP conditions

- The moved sequence turns out to depend on `load_or_init`-local state
  (resolution source, path fixups) — stop and report; do not thread extra
  parameters through without a look at whether the split is right.
- The round-trip test passes with the telegram arm commented out — the test
  is vacuous; fix it before proceeding.
