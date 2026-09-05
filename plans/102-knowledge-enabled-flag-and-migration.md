# Plan 102: Add `[knowledge] enabled` (schema 17 to 18) with a migration that preserves existing users

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/config/schema.rs src/config/migrations.rs tests/snapshots/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: feature
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

The Knowledge Base has no on/off switch. Whether it is "configured" is inferred
from whether an embedding key happens to be non-empty
(`gateway/config_api.rs:840`), so the only way to turn it off is to **delete
the credential** — the console's Clear button
(`claw-ui knowledge-settings-card.tsx:55`).

The operator asked for the opposite model: an explicit toggle, where turning
the KB off keeps the key so turning it back on is one click. That needs a real
field; three states cannot be derived from one.

| State | Today | Wanted |
|---|---|---|
| never configured | key empty | key empty, `enabled = false` |
| configured, off | **impossible** | key stored, `enabled = false` |
| configured, on | key present | key present, `enabled = true` |

This plan adds only the config field and its migration. The surfaces that read
it are plans 103-107, so each stays independently revertable.

## Naming

Do **not** call this login/logout. `login` is already a provisioner name —
"Console login — username + password gate for the web console & TUI"
(`src/onboard/provision/login.rs:24-25`). Use `enabled` in config and
"Activate"/"Deactivate" in user-facing text.

## Current state (verified at 2ca7e59)

`src/config/schema.rs:1113-1118`:

```rust
pub struct KnowledgeConfig {
    #[serde(default)]
    pub embedding_api_key: Option<String>,
    #[serde(default)]
    pub vision_api_key: Option<String>,
}
```

Current schema version is **17**: `pub const CURRENT_VERSION: u32 = 17`
(`src/config/migrations.rs:36`), and the newest drift snapshot is
`tests/snapshots/schema_drift__config_schema@v17.snap`.

The migration runner takes the **raw TOML `Value`**, not a deserialized
`Config` — `pub fn migrate(raw: &mut Value) -> Result<bool>`
(`migrations.rs:48`). It walks the version chain and stamps
`CURRENT_VERSION` at the end (`:258`). Most past bumps were additive no-ops
with only a comment (e.g. v9→v10 at `:175`); this one needs real logic, so
follow the shape of `migrate_v17` (`:262-268`), which mutates the value.

Precedent for the release class: three of the last four schema bumps shipped in
a minor `.0` release, and the closest analogue — the bump that *added*
`[knowledge]` (commit `3b18c5d`, schema v9→v10) — went out as `v0.7.0-alpha`.

## The migration hazard — the point of this plan

`enabled` must default to `false` for a fresh install. If the migration simply
adds the field with that default, **every existing operator with a working KB
loses it silently** on upgrade.

The migration must derive intent from what is already there: a config that
already carries `embedding_api_key` was configured on purpose, so it upgrades to
`enabled = true`.

## Scope

**In scope**: the field, its default, the v17→v18 migration, the snapshot, and
tests for both upgrade directions.

**Out of scope**: every consumer — gateway API (103), route gating (104),
ambient (105), console (106), TUI/CLI (107). This plan changes no behaviour.

## Git workflow

```bash
git switch -c feat/knowledge-enabled-flag
```

## Steps

### Step 1: Add the field

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct KnowledgeConfig {
    /// Whether the Knowledge Base is active. `false` (the default) means the
    /// agent is not told the KB exists, the `/api/v1/kb/*` routes report
    /// `kb_disabled`, and the console shows an activation screen. Turning it
    /// off does NOT clear the credentials below — reactivation is one click.
    /// Deleting a key is a separate, explicit action.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub embedding_api_key: Option<String>,
    #[serde(default)]
    pub vision_api_key: Option<String>,
}
```

`bool` defaults to `false`, so `Default` still derives correctly.

**Verify**: `cargo build`; `cargo test config` shows the drift-snapshot test
failing — that is expected and Step 3 handles it.

### Step 2: Migration v17 → v18

Three edits, following `migrate_v17` (`migrations.rs:262-268`) for the house
style:

1. Bump `CURRENT_VERSION` to `18` (`migrations.rs:36`).
2. Add the chain step beside the v16→v17 one (`:248-253`):

```rust
    // v17 → v18: `[knowledge] enabled` gates the Knowledge Base explicitly.
    // Existing installs that already carry an embedding key were configured
    // deliberately, so they upgrade ON. A config with no key upgrades OFF,
    // which matches what it already did.
    if from < 18 {
        migrate_v18(raw);
    }
```

3. Write `migrate_v18(raw: &mut Value)`. It operates on the **raw TOML**, so
   read `raw["knowledge"]["embedding_api_key"]` directly; do not try to
   deserialize a `Config`.

The rule is: `enabled = the raw [knowledge].embedding_api_key is a non-empty string`.

**The env case matters.** `KB_EMBEDDING_API_KEY` folds onto `config.knowledge`
at *load* (`schema.rs:4093-4099`), but migration runs on the file, before that
merge. An operator who supplies the key only via env has no key in the file and
would migrate to `enabled = false`, silently losing the KB. Treat a non-empty
`KB_EMBEDDING_API_KEY` in the process environment as evidence too, and say so
in the comment.

Note the key may be stored encrypted (`enc2:` prefix) — presence is what
matters, so do not decrypt; just check for a non-empty string.

**Verify**: unit tests in Step 4.

### Step 3: Accept the v18 snapshot

```bash
cargo test --test schema_drift
# review the diff, then accept:
INSTA_UPDATE=always cargo test --test schema_drift
git status --short tests/snapshots/    # expect a new @v18.snap
```

Read the diff before accepting. A schema-drift snapshot is a public-contract
record; accepting it blind defeats the gate.

### Step 4: Tests for both directions

In `migrations.rs` tests:

1. `schema_version = 17` with an embedding key → `enabled = true`
2. `schema_version = 17` with no key → `enabled = false`
3. a v18 config with `enabled = false` and a key → unchanged (idempotent)

Test 1 is the one that protects existing users; make it fail if the rule is
inverted.

### Step 5: Document it

`docs/reference/config.md` — the field, its default, that deactivating keeps
credentials, and the upgrade rule.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb config
cargo test --features kb --test schema_drift
cargo test --features kb
```

Real upgrade check:

```bash
cp ~/.rantaiclaw/config.toml /tmp/cfg-backup.toml
grep -n 'schema_version\|\[knowledge\]' -A3 ~/.rantaiclaw/config.toml
cargo run --features kb -- config show | grep -A4 knowledge
# expect enabled = true when a key was already present
```

## Done criteria

- `enabled` exists, defaults false, and round-trips.
- An existing configured install upgrades ON.
- v18 snapshot accepted after review.

## STOP conditions

- The current schema version is not 17 — re-derive the migration number.
- Accepting the snapshot changes fields outside `[knowledge]` — something else
  drifted; stop and report rather than baking it in.
