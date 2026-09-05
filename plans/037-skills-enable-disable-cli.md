# Plan 037: First-class `skills enable`/`skills disable` CLI, plus case-insensitive entry lookup

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 4736e2e..HEAD -- src/skills/mod.rs src/config/schema.rs src/main.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED (writes user config; must preserve the secret-encrypt round-trip and `[skills.entries.<name>]` back-compat)
- **Depends on**: plans/036-*.md (skills-CLI test harness). If 036 has not landed, this plan is still executable — build the test fixture inline per the fallback described in the Test plan.
- **Category**: bug + dx
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

The config schema already supports disabling a skill per-user via
`[skills.entries.<name>] enabled = false`, and the loader honours it — but
**there is no command that writes that key**. A user who wants to turn a skill
off has to hand-edit `config.toml`. Worse, the load-time lookup that reads the
flag is **case-sensitive** while every other skill lookup in the codebase is
case-insensitive, so `[skills.entries.Weather]` silently does nothing when the
skill's real name is `weather` — the user thinks they disabled it, but the
agent still loads it. This plan adds `skills enable <name>` / `skills disable
<name>` (writing the flag through `Config::save()`), resolves `<name>` the same
way `skills show` does (so the displayed name works), keys the `entries` map by
the canonical `skill.name`, and fixes the case-sensitivity bug. It also
introduces a single reusable resolver that plan 038 (TUI toggle) will call, so
CLI and TUI stay in parity.

## Current state

Files involved:

- `src/skills/mod.rs` — skill loading + CLI `handle_command` dispatch. Contains
  the two case-sensitive `entries.get(&s.name)` lookups (the bug) and the
  `SkillCommands` match arms.
- `src/main.rs` — `SkillCommands` clap enum (the CLI surface) and the dispatch
  line that routes to `skills::handle_command`.
- `src/config/schema.rs` — `SkillsConfig` / `SkillEntryConfig` schema and
  `Config::save()`.

### The bug: case-sensitive entry lookup (`src/skills/mod.rs`)

`load_skills_with_config` filters disabled skills (mod.rs:225-251):

```rust
pub fn load_skills_with_config(workspace_dir: &Path, config: &crate::config::Config) -> Vec<Skill> {
    let raw = load_skills_with_open_skills_config(
        workspace_dir,
        Some(config.skills.open_skills_enabled),
        config.skills.open_skills_dir.as_deref(),
    );
    raw.into_iter()
        .filter(|s| {
            if let Some(entry) = config.skills.entries.get(&s.name) {   // :233  EXACT, case-sensitive
                if !entry.enabled {
                    tracing::debug!(skill = %s.name, "skipped: disabled in config.toml");
                    return false;
                }
            }
            let unmet = s.requires.unmet();
            if !unmet.is_empty() { /* ... */ return false; }
            true
        })
        .collect()
}
```

`load_skills_with_status` has the same exact-match lookup (mod.rs:256-279):

```rust
            let mut reasons = s.requires.unmet();
            if let Some(entry) = config.skills.entries.get(&s.name) {   // :269  EXACT, case-sensitive
                if !entry.enabled {
                    reasons.insert(0, "disabled in config.toml".to_string());
                }
            }
```

By contrast, `skills show` resolves case-insensitively (mod.rs:1250-1252):

```rust
        crate::SkillCommands::Show { name } => {
            let skills = load_skills_with_config(workspace_dir, config);
            let found = skills.iter().find(|s| s.name.eq_ignore_ascii_case(&name));  // :1252
```

So a `[skills.entries.Weather]` block never matches a skill named `weather` at
:233/:269 — the disable silently no-ops.

### The CLI enum (`src/main.rs:1242-1290`)

`SkillCommands` currently has `List`, `Show`, `Install`, `Remove`, `Update`,
`Inspect`, `InstallDeps` — **no `Enable`/`Disable`**:

```rust
#[derive(Subcommand, Debug)]
enum SkillCommands {
    /// List installed skills
    List,
    /// Show metadata for a single installed skill (CLI parity for TUI `/skill <name>`)
    Show {
        /// Skill name (case-insensitive)
        name: String,
    },
    /// Install a skill from a GitHub URL or local path
    Install { source: String },
    /// Remove an installed skill
    Remove { name: String },
    // ... Update / Inspect / InstallDeps ...
}
```

Dispatch (`src/main.rs:1997`), inside an `async` match whose sibling arms
`.await` — this arm does **not**, because `handle_command` is sync:

```rust
        Some(Commands::Skills { skill_command }) => skills::handle_command(skill_command, &config),
```

The `config` passed here is loaded via `Config::load_or_init().await?`
(`src/main.rs:1718`), which **decrypts** secrets into memory.

### `handle_command` signature (`src/skills/mod.rs:1169`)

```rust
pub(crate) fn handle_command(
    command: crate::SkillCommands,
    config: &crate::config::Config,
) -> Result<()> {
    let workspace_dir = &config.workspace_dir;
    match command {
        crate::SkillCommands::List => { /* ... */ }
        crate::SkillCommands::Show { name } => { /* ... */ }
        // ...
    }
}
```

It is **sync but called from inside `#[tokio::main]`**. The established pattern
for running async work here (used by the `Install` arm, mod.rs:1315-1327) is to
spawn a fresh OS thread with its own runtime, because `Runtime::new().block_on`
would panic inside the outer runtime:

```rust
                let join = std::thread::spawn(move || -> Result<()> {
                    let rt = tokio::runtime::Runtime::new()
                        .context("build tokio runtime for clawhub install")?;
                    rt.block_on(crate::skills::clawhub::install_one(
                        &profile_for_thread,
                        &slug_for_thread,
                    ))?;
                    Ok(())
                });
                let inner_result = join
                    .join()
                    .map_err(|_| anyhow::anyhow!("ClawHub install thread panicked"))?;
```

### Schema (`src/config/schema.rs`)

`SkillsConfig.entries` (schema.rs:500-501) — `#[serde(default)]`, so absent
blocks deserialise to an empty map and are round-tripped on save:

```rust
    #[serde(default)]
    pub entries: std::collections::HashMap<String, SkillEntryConfig>,
```

`SkillEntryConfig` (schema.rs:548-566) — `enabled` defaults to true; the entry
also carries `api_key`/`env`/`config` sub-tables that must be preserved when we
flip only `enabled`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillEntryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SkillApiKey>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub config: std::collections::HashMap<String, serde_json::Value>,
}
```

`Config::save()` is **async** (schema.rs:4342) and encrypts config-level secrets
(`api_key`, `knowledge.*`, `composio.*`, `browser.*`, `web_search.*`,
`storage.*`, `agents.*`, `provider_api_keys.*`, `channels_config.telegram.
bot_token`) before serialising. Encryption is **idempotent** — it skips values
already encrypted (schema.rs:3650-3665):

```rust
fn encrypt_optional_secret(store, value, field_name) -> Result<()> {
    if let Some(raw) = value.clone() {
        if !crate::security::SecretStore::is_encrypted(&raw) {   // idempotent guard
            *value = Some(store.encrypt(&raw)...);
        }
    }
    Ok(())
}
```

**Note**: `Config::save()` does **not** encrypt `skills.entries.*.api_key`; that
sub-table is round-tripped as-is. This plan does not touch it — we only flip
`enabled` — but the save-round-trip STOP condition below still applies to the
config-level secrets that `save()` does handle.

## Commands you will need

| Purpose        | Command                                                        | Expected on success |
|----------------|---------------------------------------------------------------|---------------------|
| Format check   | `cargo fmt --all -- --check`                                  | exit 0              |
| Lint (scoped)  | `cargo clippy --all-targets -- -D warnings`                   | exit 0, no warnings |
| Unit tests     | `cargo test --lib skills::`                                   | all pass            |
| Targeted test  | `cargo test --lib skills::tests::<name>`                      | pass                |

Full `cargo test` is disk-heavy on this box — prefer `--lib`. The
strict-clippy-delta and `setup_e2e` gates run POST-merge; run the scoped clippy
above locally before merge.

## Scope

**In scope** (the only files you should modify):
- `src/skills/mod.rs` — add the shared resolver + the two new `SkillCommands`
  arms; fix the case-sensitive lookups at :233 and :269; add tests.
- `src/main.rs` — add `Enable`/`Disable` variants to the `SkillCommands` enum.
- `src/config/schema.rs` — **only if** a test helper is needed; do NOT change
  `Config::save()` or the schema structs.

**Out of scope** (do NOT touch, even though they look related):
- `Config::save()` internals and the `SkillEntryConfig`/`SkillsConfig` struct
  definitions — they are public config contract; flipping `enabled` must go
  through the existing `save()`, not a new serialiser.
- The TUI toggle — that is plan 038; this plan only exports the resolver it
  will call.
- `skills.entries.*.api_key` handling — leave the encrypt behaviour exactly as
  it is.

## Git workflow

- Branch: `advisor/037-skills-enable-disable-cli`
- Conventional commits, one per logical unit, e.g.
  `fix(skills): case-insensitive entries lookup at load time` and
  `feat(skills): skills enable/disable CLI writing entries.<name>.enabled`.
- **Repo rule: do NOT add a `Co-Authored-By` trailer.**
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Fix the case-sensitive entry lookup (the bug)

In `src/skills/mod.rs`, replace the exact `config.skills.entries.get(&s.name)`
lookups at **:233** (`load_skills_with_config`) and **:269**
(`load_skills_with_status`) with a case-insensitive match. Add one small
private helper near the top of the file so both call sites share it and future
lookups reuse it:

```rust
/// Case-insensitive lookup into `skills.entries`. Every other skill lookup
/// (`show`, install-deps, TUI `/skill`) matches case-insensitively; the two
/// load-time filters used to match exactly, so `[skills.entries.Weather]`
/// silently failed to disable a skill named `weather`. Match on the canonical
/// `skill.name` but compare case-insensitively.
fn entry_for<'a>(
    entries: &'a std::collections::HashMap<String, crate::config::schema::SkillEntryConfig>,
    skill_name: &str,
) -> Option<&'a crate::config::schema::SkillEntryConfig> {
    entries
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(skill_name))
        .map(|(_, v)| v)
}
```

Then at :233 use `if let Some(entry) = entry_for(&config.skills.entries, &s.name)`
and likewise at :269. (Confirm the exact path to `SkillEntryConfig` compiles —
it is re-exported through `crate::config::schema::SkillEntryConfig`; if the
crate uses a shorter alias elsewhere in this file, match that.)

**Verify**: `cargo test --lib skills::` → still passes (no behaviour regressions
yet). `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 2: Add a shared, reusable resolver + writer helper

Still in `src/skills/mod.rs`, add a `pub(crate)` function that (a) resolves the
user-supplied name to a loaded skill the same way `show` does, (b) returns a
**mutated clone** of the config with `entries.<canonical>.enabled` set, and (c)
returns the canonical name for messaging. Keep it **pure/no-I/O** so both the
sync CLI (this plan) and the async TUI (plan 038) can persist it with their own
idiom:

```rust
/// Resolve `name` to a loaded skill (case-insensitive, same as `skills show`),
/// then return a clone of `config` with `skills.entries.<canonical>.enabled`
/// set to `enabled`, keyed by the skill's canonical `skill.name`. The returned
/// `String` is that canonical name (for user-facing messages). Preserves any
/// existing `api_key`/`env`/`config` on the entry via `entry(..).or_default()`.
///
/// Errors if no loaded skill matches `name` (so a typo fails loudly instead of
/// writing an orphan `entries` key).
pub(crate) fn set_skill_enabled(
    config: &crate::config::Config,
    name: &str,
    enabled: bool,
) -> Result<(crate::config::Config, String)> {
    let skills = load_skills_with_config(&config.workspace_dir, config);
    let canonical = skills
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
        .map(|s| s.name.clone())
        .ok_or_else(|| {
            anyhow::anyhow!("No skill named '{name}'. Run `rantaiclaw skills list`.")
        })?;

    let mut updated = config.clone();
    // Collapse any pre-existing case-variant key onto the canonical key so we
    // don't leave both `[skills.entries.Weather]` and `[skills.entries.weather]`.
    let existing_variant_key = updated
        .skills
        .entries
        .keys()
        .find(|k| k.eq_ignore_ascii_case(&canonical) && *k != &canonical)
        .cloned();
    if let Some(old_key) = existing_variant_key {
        if let Some(entry) = updated.skills.entries.remove(&old_key) {
            updated.skills.entries.insert(canonical.clone(), entry);
        }
    }
    updated
        .skills
        .entries
        .entry(canonical.clone())
        .or_default()
        .enabled = enabled;
    Ok((updated, canonical))
}
```

Confirm `SkillEntryConfig` implements `Default` (schema.rs:568) so
`.or_default()` compiles.

**Verify**: `cargo test --lib skills::` → passes. `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 3: Add the `Enable`/`Disable` CLI variants

In `src/main.rs`, add two variants to the `SkillCommands` enum (after `Show`,
so help output groups them near `List`/`Show`):

```rust
    /// Enable a skill that was previously disabled (writes
    /// `[skills.entries.<name>] enabled = true`).
    Enable {
        /// Skill name (case-insensitive; matches the name shown by `skills list`).
        name: String,
    },
    /// Disable a skill so it is not loaded into the agent's context (writes
    /// `[skills.entries.<name>] enabled = false`).
    Disable {
        /// Skill name (case-insensitive; matches the name shown by `skills list`).
        name: String,
    },
```

**Verify**: `cargo build` (or `cargo check`) → compiles; the two arms are now
required in the `handle_command` match (non-exhaustive error until Step 4 —
that's expected).

### Step 4: Handle the new variants in `handle_command`

In `src/skills/mod.rs` `handle_command`, add arms that call `set_skill_enabled`
and persist via the **spawn-thread + fresh-runtime** pattern already used by the
`Install` arm (mod.rs:1315-1327), because `handle_command` is sync-inside-runtime:

```rust
        crate::SkillCommands::Enable { name } => {
            set_enabled_and_report(config, &name, true)
        }
        crate::SkillCommands::Disable { name } => {
            set_enabled_and_report(config, &name, false)
        }
```

with a small local helper below `handle_command`:

```rust
fn set_enabled_and_report(
    config: &crate::config::Config,
    name: &str,
    enabled: bool,
) -> Result<()> {
    let (updated, canonical) = set_skill_enabled(config, name, enabled)?;
    // `Config::save()` is async; `handle_command` is sync but called from
    // inside `#[tokio::main]`, so `Runtime::new().block_on` would panic here.
    // Persist on a fresh OS-thread runtime — mirrors the `Install` arm.
    let join = std::thread::spawn(move || -> Result<()> {
        let rt = tokio::runtime::Runtime::new()
            .context("build tokio runtime for skills enable/disable save")?;
        rt.block_on(updated.save()).context("save config")?;
        Ok(())
    });
    join
        .join()
        .map_err(|_| anyhow::anyhow!("skills enable/disable save thread panicked"))??;
    let state = if enabled { "enabled" } else { "disabled" };
    println!("✓ {canonical} {state}. Restart the agent (or reload) for it to take effect.");
    Ok(())
}
```

**Verify**: `cargo build` → compiles, match is exhaustive.
`cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 5: Manual smoke (optional but recommended)

Build the binary and drive it against a throwaway config dir so you never touch
a real user profile:

```bash
export RANTAICLAW_CONFIG_DIR="$(mktemp -d)"
mkdir -p "$RANTAICLAW_CONFIG_DIR/workspace/skills/weather"
printf '# Weather\nGives the weather.\n' > "$RANTAICLAW_CONFIG_DIR/workspace/skills/weather/SKILL.md"
cargo run --quiet -- skills disable Weather      # capital W on purpose
cargo run --quiet -- skills list                 # weather should show as gated/disabled
grep -A2 'skills.entries' "$RANTAICLAW_CONFIG_DIR"/*/config.toml 2>/dev/null || true
cargo run --quiet -- skills enable weather
rm -rf "$RANTAICLAW_CONFIG_DIR"; unset RANTAICLAW_CONFIG_DIR
```

Expected: `disable Weather` resolves to canonical `weather`, writes
`[skills.entries.weather] enabled = false`, and `skills list` marks it disabled.
(Exact config-dir layout may nest under a profile; the point is the entry is
written under the canonical lowercase key.)

## Test plan

Add tests to the existing `#[cfg(test)] mod tests` in `src/skills/mod.rs`
(starts at mod.rs:1620; models: `load_skill_from_toml` at :1660 shows the
tempdir + `skills/<name>/SKILL.toml` fixture pattern). **If plan 036 landed a
shared skills-CLI test harness, use it**; otherwise build the fixture inline as
below (a tempdir workspace + a `Config` whose `config_path` points at a temp
`config.toml`, mirroring the round-trip pattern at schema.rs:3916-3918).

Fixture sketch (inline fallback):

```rust
fn skill_fixture() -> (tempfile::TempDir, crate::config::Config) {
    let dir = tempfile::tempdir().unwrap();
    let skills_dir = dir.path().join("skills").join("weather");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join("SKILL.toml"),
        "[skill]\nname = \"weather\"\ndescription = \"w\"\nversion = \"1.0.0\"\n").unwrap();
    let mut config = crate::config::Config::default();
    config.workspace_dir = dir.path().to_path_buf();
    config.config_path = dir.path().join("config.toml");
    (dir, config)
}
```

Cases (name each by behaviour, per repo naming contract):

1. `enable_writes_entry_enabled_true` — `set_skill_enabled(&config, "weather",
   true)` returns a config whose `skills.entries["weather"].enabled == true` and
   canonical name `"weather"`.
2. `disable_writes_entry_enabled_false` — same with `false` → entry present and
   `enabled == false`.
3. `disable_then_load_excludes_skill` — apply `set_skill_enabled(.., false)`,
   then `load_skills_with_config(&updated.workspace_dir, &updated)` returns a
   list that does **not** contain `weather`.
4. `case_insensitive_entry_disables_skill` (**regression for the bug**) —
   construct a config with `entries` pre-seeded under the mixed-case key
   `"Weather"` with `enabled = false`, then assert
   `load_skills_with_config(...)` excludes `weather`. This must FAIL on the
   pre-Step-1 code and PASS after.
5. `unknown_name_errors` — `set_skill_enabled(&config, "nope", false)` returns
   `Err` and the message contains `nope`.
6. `disable_preserves_existing_entry_fields` — pre-seed
   `entries["weather"]` with a non-default `env`/`config` map, disable, and
   assert those sub-tables survive (proves `entry().or_default()` preserves).
7. (If `save()` is exercised) `save_round_trip_preserves_disable` — a
   `#[tokio::test]` that sets `config.config_path` to a temp file, runs
   `set_skill_enabled(.., false)`, `updated.save().await`, reloads with
   `toml::from_str` (or `Config::load`), and asserts the entry persisted AND a
   pre-existing config-level secret (e.g. set `config.api_key = Some("k")`
   before save) is still resolvable after reload — proving the secret
   round-trip is intact.

Verification: `cargo test --lib skills::` → all pass, including the 6-7 new
tests. Confirm case-4 fails against the unpatched Step-1 code first (checkout
the excerpt, run, see red) so you know the test actually guards the bug.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib skills::` exits 0; new tests (enable/disable/exclude/
      case-insensitive-regression/unknown/preserve) exist and pass
- [ ] `cargo run -- skills enable --help` and `... skills disable --help` print
      the new commands
- [ ] `grep -n "entries.get(&s.name)" src/skills/mod.rs` returns no matches
      (both exact lookups replaced)
- [ ] `set_skill_enabled` is `pub(crate)` and takes `(&Config, &str, bool)`
      (so plan 038 can call it)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The code at :233 / :269 / :1250-1252 / main.rs:1242-1290 does not match the
  excerpts above (drift since this plan was written).
- **`Config::save()` drops or plaintexts any secret in the round-trip** — i.e.
  test case 7 shows a previously-encrypted config-level secret came back
  plaintext, or a secret went missing. This is a security regression; STOP.
- The spawn-thread + `Runtime::new().block_on(save())` pattern panics with
  "Cannot start a runtime from within a runtime" at run time (would mean
  `handle_command` is no longer called from inside the outer runtime, or the
  save was invoked without the thread) — STOP and re-check the Install-arm
  pattern.
- `set_skill_enabled` would need to change `Config::save()` or a schema struct
  to work — it must not; STOP.
- A step's verification fails twice after a reasonable fix attempt.

## Maintenance notes

For the human/agent who owns this after it lands:

- Plan 038 (TUI skill lifecycle) reuses `set_skill_enabled` for its in-picker
  toggle — keep the function pure (no I/O) so both surfaces can persist with
  their own async idiom. If you add persistence into the resolver, you break
  038's call site.
- The `entry_for` helper is the single case-insensitive `entries` lookup — if a
  new load path reads `skills.entries`, route it through `entry_for`, not a raw
  `.get`.
- Reviewer should scrutinise: (a) the secret round-trip (case 7), (b) that the
  canonical-key collapse in Step 2 does not delete an unrelated entry, and
  (c) that `disable` errors on unknown names rather than writing an orphan key.
- Deferred out of scope: a `skills enable/disable --all` bulk form, and any
  hot-reload so a running agent picks up the flag without restart (the CLI
  message tells the user to restart). Revisit if users ask.
