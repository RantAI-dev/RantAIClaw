# Plan 045: Treat remote skills as untrusted — pin open-skills, dedup with local precedence, default remote skills to compact injection, and encrypt literal skill keys

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. This plan changes a fingerprinted default and the
> config schema; the "Decisions to confirm" section lists calls a maintainer
> must make BEFORE this merges. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 4736e2e..HEAD -- src/skills/mod.rs src/config/schema.rs src/config/migrations.rs src/tools/mod.rs src/gateway/config_api.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: MED (changes default agent behavior + config schema version)
- **Depends on**: none
- **Category**: security (defensive hardening) + migration
- **Planned at**: commit `4736e2e`, 2026-07-23

## Why this matters

Every skill's full `SKILL.md` body is injected verbatim into the system prompt
as an authoritative `<instruction>` block whenever `prompt_injection_mode` is
`Full` — which is the **default**. Skills are not all first-party: RantaiClaw
auto-`git clone`s and weekly `git pull --ff-only`s a hardcoded community
repository (`besoeasy/open-skills`) with no commit pin or signature, and ClawHub
installs land arbitrary remote `SKILL.md` files into the profile. So a remote
author (or anyone who lands a commit upstream) can inject text that the model
treats as operator instructions — a classic prompt-injection / confused-deputy
path into a runtime whose local tools include shell and file access. Worse, the
loader extends open-skills and workspace skills with **no shared dedup**, so a
remote skill can shadow or duplicate a bundled/core skill name (e.g.
`owner-permissions`). Separately, a `source = "literal"` skill API key is written
as **plaintext** into `config.toml`, bypassing the secret store every other
credential uses. This plan reframes remote skills as untrusted input: pin the
upstream, give local skills precedence on name conflicts, stop injecting remote
bodies verbatim by default, and route literal keys through the secret store.

## Current state

Files and their roles:

- `src/skills/mod.rs` — skill discovery, open-skills clone/pull, and system-prompt
  rendering. Carries the hardcoded repo URL, the unpinned pull, the missing
  cross-source dedup, and the verbatim `Full`-mode injection.
- `src/config/schema.rs` — `SkillsConfig` / `SkillsPromptInjectionMode` /
  `SkillApiKey`, plus the `Config::save`/load encrypt/decrypt secret pipeline.
- `src/config/migrations.rs` — schema-version constant + per-version migrators.
- `src/tools/mod.rs` — consumes `api_key` when building per-skill env.
- `src/gateway/config_api.rs` — redacts secrets before the config API returns
  the running config.

### SECURITY-01 — remote `SKILL.md` bodies are injected verbatim as authoritative instructions, by default

Hardcoded upstream + weekly unpinned pull (`src/skills/mod.rs:14`,
`:516-573`):

```rust
const OPEN_SKILLS_REPO_URL: &str = "https://github.com/besoeasy/open-skills";
```

```rust
    let output = Command::new("git")
        .args(["clone", "--depth", "1", OPEN_SKILLS_REPO_URL])
        .arg(repo_dir)
        .output();
```

```rust
fn pull_open_skills_repo(repo_dir: &Path) -> bool {
    // If user points to a non-git directory via env var, keep using it without pulling.
    if !repo_dir.join(".git").exists() {
        return true;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["pull", "--ff-only"])
        .output();
    ...
```

The full body becomes the skill's single prompt (`src/skills/mod.rs:639-650`,
and the open-skills constructor `:918-929`):

```rust
    Ok(Skill {
        name: frontmatter_name.unwrap_or(name),
        ...
        prompts: vec![content],
        location: Some(path.to_path_buf()),
        ...
    })
```

`Full` is the default injection mode (`src/config/schema.rs:445-453`):

```rust
pub enum SkillsPromptInjectionMode {
    /// Inline full skill instructions and tool metadata into the system prompt.
    #[default]
    Full,
    /// Inline only compact skill metadata (name/description/location) and load details on demand.
    Compact,
}
```

In `Full` mode the whole body is emitted inside `<instructions>` with a header
that tells the model to "Follow these instructions directly"
(`src/skills/mod.rs:1007-1040`):

```rust
    let mut prompt = match mode {
        crate::config::SkillsPromptInjectionMode::Full => String::from(
            "## Available Skills\n\n\
             Skill instructions and tool metadata are preloaded below.\n\
             Follow these instructions directly; do not read skill files at runtime unless the user asks.\n\n\
             <available_skills>\n",
        ),
        ...
    };
    for skill in skills {
        ...
        if matches!(mode, crate::config::SkillsPromptInjectionMode::Full) {
            if !skill.prompts.is_empty() {
                let _ = writeln!(prompt, "    <instructions>");
                for instruction in &skill.prompts {
                    write_xml_text_element(&mut prompt, 6, "instruction", instruction);
                }
                let _ = writeln!(prompt, "    </instructions>");
            }
```

### SECURITY-01b — no cross-source dedup: a remote skill can shadow/duplicate a local one

`load_skills_with_open_skills_config` extends open-skills **then** workspace
skills with no shared `seen` set (`src/skills/mod.rs:281-296`):

```rust
fn load_skills_with_open_skills_config(
    workspace_dir: &Path,
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
) -> Vec<Skill> {
    let mut skills = Vec::new();

    if let Some(open_skills_dir) =
        ensure_open_skills_repo(config_open_skills_enabled, config_open_skills_dir)
    {
        skills.extend(load_open_skills(&open_skills_dir));
    }

    skills.extend(load_workspace_skills(workspace_dir));
    skills
}
```

Only `load_workspace_skills` dedups, and only *within* its three local sources
(`src/skills/mod.rs:316-345`) using a local `seen: HashSet<String>`. So the
returned Vec can contain a remote `owner-permissions` **and** a bundled/core
`owner-permissions`, and a remote entry is emitted first (open-skills is extended
first), so it can shadow the local one in any first-match consumer.

### DX-01 — `source = "literal"` skill API key is stored plaintext in `config.toml`

`SkillApiKey.value` is a plain `Option<String>`, serialized whenever set
(`src/config/schema.rs:582-591`):

```rust
pub struct SkillApiKey {
    /// `env` (recommended) or `literal`.
    pub source: String,
    /// When `source = "env"`, the env var name to read. Ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// When `source = "literal"`, the API key value. **Avoid** — prefer env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}
```

It is consumed as plaintext at runtime (`src/tools/mod.rs:544-547`):

```rust
                "literal" => {
                    if let Some(val) = api_key.value.clone() {
                        out.insert(var_name, val);
                    }
                }
```

Unlike `config.api_key`, provider keys, `composio.api_key`, channel bot tokens,
etc., the skill literal value is **not** listed in the encrypt-on-save /
decrypt-on-load pipeline, so it survives to disk in plaintext and is re-written on
every `Config::save()`.

The encrypt/decrypt pattern to reuse — save side (`src/config/schema.rs:4342-4394`):

```rust
    pub async fn save(&self) -> Result<()> {
        let mut config_to_save = self.clone();
        let rantaiclaw_dir = self.config_path.parent()...;
        let store = crate::security::SecretStore::new(rantaiclaw_dir, self.secrets.encrypt);

        encrypt_optional_secret(&store, &mut config_to_save.api_key, "config.api_key")?;
        ...
        for key in config_to_save.provider_api_keys.values_mut() {
            let mut wrapped = Some(std::mem::take(key));
            encrypt_optional_secret(&store, &mut wrapped, "config.provider_api_keys.*")?;
            *key = wrapped.unwrap_or_default();
        }
```

Load side, symmetric (`src/config/schema.rs:3885-3903`) — decrypts back to
plaintext in memory so runtime consumers are unaffected. The helpers
`encrypt_optional_secret` / `decrypt_optional_secret` (`schema.rs:3633-3663`) are
no-ops on already-encrypted (`enc2:`/`enc:` prefixed) values and skip `None`.

The gateway config-display redactor already nulls the whole `api_key` object by
suffix (`api_key` ends with `_key`) in its JSON backstop
(`src/gateway/config_api.rs:132-174`), but the **typed** redactor enumerates a
hardcoded list and does not mention skills (`src/gateway/config_api.rs:178-195`):

```rust
/// Clear every secret field before a Config is serialized into an API response.
/// Keep in sync with the encrypt/decrypt lists in config::schema.
fn redact_config_secrets(cfg: &mut crate::config::Config) {
    cfg.api_key = None;
    ...
```

### Schema-version context

`src/config/migrations.rs:36`:

```rust
pub const CURRENT_VERSION: u32 = 15;
```

Each new schema-affecting change bumps this and adds a `from < N` arm in
`migrate()` (see the existing arms at `migrations.rs:77-89`). The `SkillsConfig`
default is fingerprinted by the schema-drift gate (CLAUDE.md §3.6), so adding a
config key (`open_skills_ref`) and changing the effective default injection for
remote skills both require the bump.

## Commands you will need

| Purpose      | Command                                                        | Expected on success |
|--------------|---------------------------------------------------------------|---------------------|
| Format check | `cargo fmt --all -- --check`                                  | exit 0, no diff     |
| Lint         | `cargo clippy --all-targets -- -D warnings`                  | exit 0              |
| Build        | `cargo build`                                                | exit 0              |
| Skills tests | `cargo test --lib skills::`                                  | all pass            |
| Config tests | `cargo test --lib config::`                                  | all pass            |
| Gateway test | `cargo test --lib gateway::config_api`                       | all pass            |

Note: `strict-clippy-delta`, the schema-drift gate, and `setup_e2e` run
POST-merge — run the scoped `clippy`/`test` above locally before merging, and
confirm the schema-drift fingerprint update is intentional (see Step 4).

## Suggested executor toolkit

- If a `.codegraph/` directory exists at the repo root, use
  `codegraph explore "load_skills_with_open_skills_config skills_to_prompt_with_mode ensure_open_skills_repo"`
  to get the full call chain for the skill-loading + prompt-build path in one
  call before editing.
- Load the `rust-skills` skill (if available) for the `Command`/`git` subprocess
  and serde-field changes.

## Scope

**In scope** (the only files you should modify):

- `src/skills/mod.rs` — pin (a), cross-source dedup (b), remote-origin tag +
  compact-for-remote injection (c).
- `src/config/schema.rs` — add `open_skills_ref` to `SkillsConfig`; add the
  skill literal `value` to the save/load encrypt/decrypt pipeline (d); update the
  `SkillApiKey.value` doc-comment.
- `src/config/migrations.rs` — bump `CURRENT_VERSION` 15 → 16 + add the migrate
  arm.
- `src/gateway/config_api.rs` — add the skill literal `value` to the typed
  redactor (d).

**Out of scope** (do NOT touch, even though they look related):

- `src/tools/mod.rs` — read for ground truth only; it consumes the **decrypted**
  in-memory value (the load path already decrypts before it runs), so no change
  is needed here. Do not add decryption in the tool.
- The `Compact` mode wording/format itself, or any change to how *local* skills
  are injected in `Full` mode — local skills keep verbatim `Full` injection.
- ClawHub install/download verification (`src/skills/clawhub.rs`) — a separate,
  larger effort. This plan hardens the open-skills path + the injection default;
  it does not add signature verification to ClawHub.
- Any change to `prompt_injection_mode`'s meaning for local skills.

## Git workflow

- Branch: `advisor/045-remote-skill-trust-boundary`
- Commit per logical unit (one per step is fine). Conventional-commit style, e.g.
  `feat(skills): pin open-skills ref and stop auto-advancing on pull`,
  `feat(skills): dedup skills across sources with local precedence`,
  `feat(skills): default remote-origin skills to compact injection`,
  `feat(config): route literal skill api_key through the secret store (schema v16)`.
- **Do NOT** add a `Co-Authored-By` trailer (repo convention).
- Do NOT push or open a PR unless the operator instructed it.
- The PR body MUST note: schema bump 15→16, the changed default (remote skills no
  longer verbatim-injected), the new `open_skills_ref` key, and a CHANGELOG
  entry — see "Decisions to confirm".

## Steps

### Step 1: Give local skills precedence and dedup across sources (b)

In `src/skills/mod.rs`, change `load_skills_with_open_skills_config`
(`:281-296`) so **workspace (local) skills load first** and open-skills entries
are only appended when their name is not already present. Target shape:

```rust
fn load_skills_with_open_skills_config(
    workspace_dir: &Path,
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
) -> Vec<Skill> {
    // Local (workspace/profile/bundled) skills win on name conflicts — a remote
    // open-skills entry must never shadow a first-party or core skill.
    let mut skills = load_workspace_skills(workspace_dir);
    let mut seen: std::collections::HashSet<String> =
        skills.iter().map(|s| s.name.clone()).collect();

    if let Some(open_skills_dir) =
        ensure_open_skills_repo(config_open_skills_enabled, config_open_skills_dir)
    {
        for s in load_open_skills(&open_skills_dir) {
            if seen.insert(s.name.clone()) {
                skills.push(s);
            }
        }
    }

    skills
}
```

**Verify**: `cargo build` → exit 0. (A dedicated test lands in the Test plan.)

### Step 2: Tag skill origin and default remote skills to compact injection (c)

**Recommended approach: compact-for-remote** (see Decisions to confirm for the
alternative delimiter approach).

1. Add a load-time origin flag to `Skill` (`src/skills/mod.rs:22`). Use a
   serde-skipped bool so it is not read from `SKILL.toml`/`SKILL.md` and defaults
   to `false`:

   ```rust
   /// True when the skill came from a remote/untrusted source (open-skills,
   /// ClawHub). Remote bodies are NOT injected verbatim in Full mode.
   #[serde(skip)]
   pub remote: bool,
   ```

   `cargo build` will now fail at every `Skill { ... }` literal. Add
   `remote: false` to each — the production constructors `load_skill_toml`
   (`:600`) and `load_skill_md` (`:639`), and every test literal — **except**
   `load_open_skill_md` (`:918-929`), which sets `remote: true`. Use the compiler
   output as the checklist (the literals live in `src/skills/mod.rs`; if the build
   flags a `Skill { ... }` literal in any other file, STOP — see STOP conditions).

2. In `skills_to_prompt_with_mode` (`src/skills/mod.rs:995-1040`), gate the
   verbatim `<instructions>` emission so a **remote** skill is rendered
   compact (name/description/location only) even when the global mode is `Full`.
   Change the `if matches!(mode, ...Full)` guard at `:1033` to also require the
   skill be local:

   ```rust
   let render_full = matches!(mode, crate::config::SkillsPromptInjectionMode::Full)
       && !skill.remote;
   ...
   let location = render_skill_location(skill, workspace_dir, !render_full);
   write_xml_text_element(&mut prompt, 4, "location", &location);

   if render_full {
       if !skill.prompts.is_empty() {
           // ... existing <instructions> block unchanged ...
       }
       if !skill.tools.is_empty() {
           // ... existing <tools> block unchanged ...
       }
   }
   ```

   (Pass `!render_full` to `render_skill_location` so a remote skill still gets
   the on-demand relative-location rendering the `Compact` path uses.)

**Verify**: `cargo build` → exit 0; `cargo test --lib skills::` → all pass
(existing prompt tests still green for local skills).

### Step 3: Pin the open-skills ref; stop auto-advancing on pull (a)

In `src/skills/mod.rs`:

1. Add a code default ref constant near `OPEN_SKILLS_REPO_URL` (`:14`). Leave the
   *value* as a Decision-to-confirm placeholder (a maintainer picks the SHA/tag —
   see Decisions):

   ```rust
   /// Default open-skills ref to pin to. `None` preserves the legacy
   /// auto-advancing `git pull --ff-only` behavior; set to a commit SHA or tag
   /// to freeze the upstream. Overridable per-install via
   /// `[skills].open_skills_ref`. SEE plan 045 "Decisions to confirm".
   const OPEN_SKILLS_DEFAULT_REF: Option<&str> = None;
   ```

2. Add `open_skills_ref: Option<String>` to `SkillsConfig`
   (`src/config/schema.rs:463-508`), `#[serde(default)]`, with a doc-comment
   explaining it overrides `OPEN_SKILLS_DEFAULT_REF`.

3. Thread the resolved ref (config value, else `OPEN_SKILLS_DEFAULT_REF`) into
   the clone/sync path. When a ref **is** resolved:
   - After a successful clone, `git -C <dir> checkout <ref>`.
   - Replace the unconditional `git pull --ff-only` (`:555-559`) with a
     `git -C <dir> fetch --depth 1 origin <ref>` followed by
     `git -C <dir> checkout <ref>` (or a hard reset to the fetched ref) — i.e.
     converge to the pin, never advance past it.
   When the ref is `None`, keep today's `clone --depth 1` + `pull --ff-only`
   behavior byte-for-byte (backward compatible).

   Keep the change localized to `clone_open_skills_repo` / `pull_open_skills_repo`
   / `ensure_open_skills_repo` and their signatures. Log the resolved ref via
   `tracing`.

**Verify**: `cargo build` → exit 0; `cargo test --lib skills::` → all pass. With
`OPEN_SKILLS_DEFAULT_REF = None` and no config override, behavior is unchanged
(existing open-skills tests stay green).

### Step 4: Route literal skill API keys through the secret store (d)

In `src/config/schema.rs`:

1. **Save side** — in `Config::save` after the channel-token block
   (`:4399-4407`), add a loop mirroring `provider_api_keys` handling:

   ```rust
   for (name, entry) in config_to_save.skills.entries.iter_mut() {
       if let Some(api_key) = entry.api_key.as_mut() {
           if api_key.source == "literal" {
               encrypt_optional_secret(
                   &store,
                   &mut api_key.value,
                   &format!("config.skills.entries.{name}.api_key.value"),
               )?;
           }
       }
   }
   ```

2. **Load side** — in the decrypt block, symmetric with `save`, after the
   Telegram decrypt (`:3895-3903`) and BEFORE `config.apply_env_overrides()`
   (`:3904`), add the matching decrypt loop so runtime consumers still get
   plaintext:

   ```rust
   for (name, entry) in config.skills.entries.iter_mut() {
       if let Some(api_key) = entry.api_key.as_mut() {
           if api_key.source == "literal" {
               decrypt_optional_secret(
                   &store,
                   &mut api_key.value,
                   &format!("config.skills.entries.{name}.api_key.value"),
               )?;
           }
       }
   }
   ```

3. Update the `SkillApiKey.value` doc-comment (`:588-589`) to state the value is
   encrypted at rest via the secret store when `secrets.encrypt = true` (the
   default), same as provider keys, and `literal` is still accepted for compat.

In `src/gateway/config_api.rs`, extend the typed `redact_config_secrets`
(`:178-195`) to clear skill literal values (keeps it "in sync" per its own
comment), e.g.:

```rust
    for entry in cfg.skills.entries.values_mut() {
        if let Some(api_key) = entry.api_key.as_mut() {
            api_key.value = None;
        }
    }
```

**Verify**: `cargo build` → exit 0; `cargo test --lib config:: gateway::config_api`
→ all pass.

### Step 5: Bump the schema version + migrate arm

In `src/config/migrations.rs`:

1. Change `pub const CURRENT_VERSION: u32 = 15;` (`:36`) to `16`.
2. Add a `if from < 16 { ... }` arm at the end of the per-version section
   (after the highest existing arm), following the pattern of the existing
   "burn a version slot" arms (`migrations.rs:81-89`): no structural transform is
   needed (the new `open_skills_ref` key is additive with a serde default, and
   the literal-key encryption happens on the next `save()`), so the arm is a
   documented placeholder that records intent. Keep the comment explicit about
   *why* v16 exists (open_skills_ref key + remote-compact default + literal-key
   encryption).

**Verify**: `cargo build` → exit 0; `cargo test --lib config::migrations` → all
pass. Confirm the schema-drift fingerprint change is intended (it will change
because `SkillsConfig` gained a field and `CURRENT_VERSION` moved).

## Test plan

Add these tests (model after existing tests in the same modules — e.g. the
skills tests around `src/skills/mod.rs:1719-1968`, the config secret tests in
`src/config/schema.rs`, and `gateway::config_api` redaction tests at
`src/gateway/config_api.rs:883+`):

- **`remote_skill_cannot_shadow_local_name`** (`src/skills/mod.rs` tests): build
  a local skills dir with a skill named `owner-permissions` and an open-skills
  dir with a same-named `owner-permissions.md`; assert the loaded set contains
  exactly one `owner-permissions` and it is the local one (assert on a
  distinguishing field, e.g. `remote == false` or a version marker).
- **`open_skills_pin_not_advanced`** (`src/skills/mod.rs` tests): with a resolved
  ref, assert `pull_open_skills_repo`/sync converges to the pinned ref and does
  not run a bare `pull --ff-only`. If exercising real `git` is impractical in the
  test env, assert instead that with `OPEN_SKILLS_DEFAULT_REF = None` and no
  override the legacy path is selected, and cover ref-resolution
  (config-over-default precedence) directly. State in the test which you did.
- **`remote_skill_defaults_to_compact_injection`** (`src/skills/mod.rs` tests):
  build one local and one remote `Skill` (`remote: true`) each with a non-empty
  `prompts`, call `skills_to_prompt_with_mode(.., Full)`, and assert the local
  skill's body appears inside `<instructions>` while the remote skill's body does
  **not** (only its name/description/location are emitted).
- **`literal_skill_api_key_not_plaintext_in_serialized_config`** (`src/config`
  tests): construct a `Config` with `secrets.encrypt = true` and a
  `[skills.entries.x.api_key]` `source = "literal"`, `value = <a test string>`;
  `save()` then read the raw `config.toml`; assert the raw string does NOT contain
  the plaintext value and DOES contain an `enc2:`/`enc:` prefix for that field;
  then load and assert the in-memory `value` decrypts back to plaintext (so
  `src/tools/mod.rs` still works). Use a neutral test value — never a real key.
- **`config_api_redacts_skill_literal_value`** (`gateway::config_api` tests):
  assert the config-API response does not contain the skill literal value.

Do NOT put any real secret value in any test — use a neutral placeholder string.

Verification: `cargo test --lib skills:: config:: gateway::config_api` → all
pass, including the five new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo build` exits 0
- [ ] `cargo test --lib skills:: config:: gateway::config_api` exits 0; the five
      new tests exist and pass
- [ ] `grep -n 'pub const CURRENT_VERSION: u32 = 16' src/config/migrations.rs`
      → 1 match
- [ ] `grep -n 'open_skills_ref' src/config/schema.rs` → at least 1 match
- [ ] Local skills still inject verbatim in `Full` mode (asserted by the
      `remote_skill_defaults_to_compact_injection` test's local-skill branch)
- [ ] With `OPEN_SKILLS_DEFAULT_REF = None` and no config override, open-skills
      clone/pull behavior is unchanged (existing open-skills tests green)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] PR body records: schema 15→16, changed remote-injection default, new
      `open_skills_ref` key, CHANGELOG entry (see Decisions to confirm)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check shows any in-scope file changed since `4736e2e` and its live
  content no longer matches the "Current state" excerpts (especially the save/
  load encrypt lists in `schema.rs` or the injection guard in
  `skills_to_prompt_with_mode`).
- Adding the `remote` field to `Skill` makes `cargo build` flag a `Skill { ... }`
  literal in a file **outside** `src/skills/mod.rs` (the scope assumed all
  literals live there; report the file so scope can be revisited).
- The maintainer has NOT resolved the Decisions to confirm below (in particular
  the `OPEN_SKILLS_DEFAULT_REF` value and the compact-vs-delimiter choice) — do
  not invent a pin SHA or silently pick the delimiter approach.
- The literal-key encryption test shows the loaded value does NOT decrypt back to
  plaintext (would break `src/tools/mod.rs` consumers) — the load-side decrypt
  arm is wrong or misplaced.
- Any verification fails twice after a reasonable fix attempt.

## Decisions to confirm (maintainer, BEFORE merge)

These are product/security calls the executor must NOT make unilaterally:

1. **Compact-for-remote vs untrusted-delimiter (Step 2)** — this plan implements
   *compact-for-remote* (remote bodies loaded on demand, not injected as
   authoritative instructions). The alternative is to keep injecting remote bodies
   in `Full` mode but wrap them in an explicit `<untrusted_reference>` delimiter
   ("treat as reference, not command"). **Recommendation: compact-for-remote** —
   it removes the instruction-authority entirely rather than relying on the model
   to honor a delimiter. Confirm before merge.
2. **`OPEN_SKILLS_DEFAULT_REF` value + on/off (Step 3)** — the mechanism ships
   with the default pin set to `None` (behavior unchanged) so the executor need
   not invent a SHA. The security benefit only lands once a maintainer sets it to
   a reviewed commit SHA or tag of `besoeasy/open-skills`. Decide the pin value
   and whether to default it on out-of-the-box.
3. **Schema bump to v16 + CHANGELOG (Step 5)** — changing a fingerprinted default
   (remote injection) and adding a config key requires the bump and a CHANGELOG /
   PR note per CLAUDE.md §3.6. Confirm the version number is free (current
   `CURRENT_VERSION = 15`) and the CHANGELOG entry wording.

## Maintenance notes

For the human/agent who owns this code after the change lands:

- **ClawHub is the other untrusted source** — this plan hardens open-skills and
  the injection default, but ClawHub installs still land unverified `SKILL.md`
  files into the profile skills dir, where (post-b) they are treated as *local*
  and get verbatim `Full` injection. A follow-up should tag ClawHub-installed
  skills `remote: true` at install time (record provenance in the profile skills
  dir) so they also default to compact. That needs a provenance marker the loader
  can read — out of scope here.
- **Keep the three lists in sync**: the encrypt list (`schema.rs` `save`), the
  decrypt list (`schema.rs` load), and the typed redactor
  (`gateway/config_api.rs`). The gateway's JSON backstop already nulls the whole
  `api_key` object by suffix, but the typed redactor and the encrypt/decrypt lists
  must both include the skill literal value or a future refactor will re-open the
  leak.
- **Precedence is now local-wins** (Step 1): if a future change reorders skill
  sources or adds a fourth source, preserve the invariant that a remote source
  can never shadow a bundled/core/workspace skill name.
- A reviewer should scrutinize: the `git checkout`/`fetch` argument construction
  in Step 3 (no shell interpolation of the ref — it is passed as a separate
  `.arg()`, and the ref should be validated as a plausible SHA/tag, not arbitrary
  text); that `src/tools/mod.rs` was NOT modified (it must keep reading the
  in-memory decrypted value); and that the new `remote` flag is `#[serde(skip)]`
  so it never round-trips through `SKILL.toml`.
