# Plan 044: Make skills docs match runtime; remove the dead `approved` param; stop leaking host paths from skill tools

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 4736e2e..HEAD -- docs/pillars/4-skills-mcp.md docs/reference/config.md docs/reference/commands.md src/main.rs src/config/schema.rs src/tools/mod.rs src/tools/skills_install.rs src/tools/author_skill.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P3
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none (but see Maintenance notes: future plans 037/038 will further update the skill enable/disable docs)
- **Category**: docs + tech-debt (hygiene)
- **Planned at**: commit `4736e2e`, 2026-07-23

## Why this matters

Three of the skill docs describe commands and config shapes that do not exist:
the pillar doc tells users to run `rantaiclaw skill list` (the real command is
`skills`, plural) and to write a `[skills]\nenabled = [...]` block that the
config parser silently ignores, and the config reference shows an env-source
`api_key` using the wrong key (`value` instead of `id`), which produces a skill
that never receives its key. Separately, the two agent-facing skill-install
tools advertise an `approved` parameter their code never reads (approval is
enforced externally by name), and their module doc instructs the model to pass
`approved: true` — a misleading no-op that wastes a model's reasoning. Finally,
those tools echo absolute host filesystem paths back into chat output. This plan
makes the docs true, deletes the dead parameter, and stops leaking host paths —
all low-risk, no behavior change to approval or config parsing.

## Current state

Files and their roles:

- `docs/pillars/4-skills-mcp.md` — the "Skills & MCP" product pillar doc; its
  "CLI / config" section (lines 78–96) shows wrong command + config shapes.
- `docs/reference/config.md` — the `[skills]` config reference; its worked
  example (lines 156–171) uses the wrong `api_key` key.
- `docs/reference/commands.md` — the CLI command reference; its `skills`
  section (lines 200–213) omits `skills show`.
- `src/main.rs` — clap command definitions (the ground truth for command names).
- `src/config/schema.rs` — `SkillsConfig` / `SkillEntryConfig` / `SkillApiKey`
  (the ground truth for config shape).
- `src/tools/mod.rs` — builds the per-skill env from config (ground truth for
  how `api_key.source`/`id`/`value` are consumed).
- `src/tools/skills_install.rs` — the `skills_install` + `skills_install_deps`
  agent tools; carries the dead `approved` param, the misleading module doc, and
  a host-path leak in its success message.
- `src/tools/author_skill.rs` — the `author_skill` agent tool; leaks the written
  file's absolute path in its success message.

### DOCS-01 — wrong command name and fabricated config block (`docs/pillars/4-skills-mcp.md`)

Current text (`docs/pillars/4-skills-mcp.md:80-96`):

```
rantaiclaw skill list
rantaiclaw skill install <source>      # ClawHub URL or local path
rantaiclaw skill remove <name>
rantaiclaw setup skills                # multi-select picker
rantaiclaw setup mcp                   # 9-server curated picker
```

```toml
[skills]
enabled = ["web-search", "summarizer"]

[mcp.<server-name>]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "..." }
```

Ground truth — the clap command is `Skills` (plural, no singular alias),
`src/main.rs:513-517`:

```rust
    /// Manage skills (user-defined capabilities)
    Skills {
        #[command(subcommand)]
        skill_command: SkillCommands,
    },
```

Ground truth — the real per-skill config shape is `[skills.entries.<name>]`
with a per-entry `enabled` bool, `src/config/schema.rs:547-552`:

```rust
/// Per-skill configuration entry — OpenClaw `skills.entries.<name>` parity.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkillEntryConfig {
    /// Whether the skill is loaded into the agent's context. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
```

`SkillsConfig` (`src/config/schema.rs:463-465`) has **no**
`#[serde(deny_unknown_fields)]`, so the fabricated `[skills]\nenabled = [...]`
top-level array parses without error and is silently discarded — the reader gets
no signal that their skill toggles do nothing.

### DOCS-02 — env-source `api_key` documented with the wrong key (`docs/reference/config.md`)

Current text (`docs/reference/config.md:165-167`):

```toml
[skills.entries.weather.api_key]
source = "env"
value = "WEATHER_API_KEY"
```

Ground truth — for `source = "env"` the environment variable **name** is read
from `id`, not `value`; `value` is only consulted for `source = "literal"`
(`src/tools/mod.rs:531-547`):

```rust
        if let Some(api_key) = &entry.api_key {
            let var_name = api_key
                .id
                .clone()
                .unwrap_or_else(|| format!("{}_API_KEY", skill_env_prefix(name)));
            match api_key.source.as_str() {
                "env" => {
                    if let Ok(val) = std::env::var(&var_name) {
                        if !val.is_empty() {
                            out.insert(var_name, val);
                        }
                    }
                }
                "literal" => {
                    if let Some(val) = api_key.value.clone() {
                        out.insert(var_name, val);
                    }
                }
```

The schema's own doc-comment is the correct exemplar
(`src/config/schema.rs:488-490`): `source = "env"` / `id = "GEMINI_API_KEY"`.
As written, the doc's `value = "WEATHER_API_KEY"` on an `env` source is ignored
and the skill never gets its key.

### DOCS-03 — `skills show` undocumented; no signpost that enable/disable is config-only (`docs/reference/commands.md`)

`skills show <name>` exists (`src/main.rs:1246-1250`):

```rust
    /// Show metadata for a single installed skill (CLI parity for TUI `/skill <name>`)
    Show {
        /// Skill name (case-insensitive)
        name: String,
    },
```

But the docs list omits it (`docs/reference/commands.md:200-207`):

```
### `skills`

- `rantaiclaw skills list`
- `rantaiclaw skills install <source>`
- `rantaiclaw skills install-deps [<slug> | --all]`
- `rantaiclaw skills inspect <slug>`
- `rantaiclaw skills update [<slug> | --all]`
- `rantaiclaw skills remove <name>`
```

There is also no `skills enable`/`skills disable` command — toggling a skill is
config-only via `[skills.entries.<name>] enabled = false`. `docs/reference/config.md:152`
already states the mapping; `commands.md` should point readers there so they do
not hunt for a CLI toggle that does not exist.

### BUG-04 — the `approved` param is a no-op; the module doc mis-instructs the model (`src/tools/skills_install.rs`)

Both tool schemas advertise `approved` (`src/tools/skills_install.rs:62-66` and
`:149-153`):

```rust
                "approved": {
                    "type": "boolean",
                    "description": "Set to true to confirm the install in supervised mode.",
                    "default": false
                }
```

Neither `execute()` reads it — `SkillsInstallTool::execute` (`:72-107`) only
reads `slug`; `SkillsInstallDepsTool::execute` (`:159-248`) only reads `name`.
The module doc (`:1-18`) is self-contradictory: lines 7-9 tell the model it
"must pass `approved: true`", while lines 14-18 correctly explain approval is
**name-based** via `crate::approval::ApprovalManager` (external to the tool):

```rust
//! - `skills_install` → installs a ClawHub skill by slug. Wraps
//!   `clawhub::install_one`. Requires user approval (the LLM must pass
//!   `approved: true` and the supervised-mode approval manager
//!   intercepts to ask the user).
...
//! Approval is name-based via [`crate::approval::ApprovalManager`] —
//! the existing `auto_approve` / `always_ask` config keys apply.
```

An existing test asserts the param IS present and must be updated
(`src/tools/skills_install.rs:326-343`):

```rust
    #[test]
    fn install_tools_have_stable_names_and_advertise_approved_arg() {
        ...
        assert!(schema["properties"]["approved"].is_object());
        ...
        assert!(schema2["properties"]["approved"].is_object());
    }
```

### SEC-06 — skill tools echo absolute host paths into chat (`skills_install.rs`, `author_skill.rs`)

`skills_install` success message interpolates the profile skills dir
(`src/tools/skills_install.rs:90-98`):

```rust
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!(
                    "Installed `{slug_owned}` from ClawHub into {}. \
                     The agent will see it on the next turn — call \
                     `skills_list` if you need to confirm.",
                    profile.skills_dir().display()
                ),
```

`author_skill` success message interpolates the written file's absolute path
(`src/tools/author_skill.rs:288-296`):

```rust
        Ok(ToolResult {
            success: true,
            output: format!(
                "{verb} skill `{slug}` at {}. It will be available on the next turn \
                 (restart channel runtimes to pick it up immediately).",
                skill_md.display()
            ),
            error: None,
        })
```

Both put a full host path (`/home/<user>/.rantaiclaw/...`) into text the model
relays to whatever channel it is talking on. The fix is to keep the full path in
a `tracing` log (operator-visible) and give the model a profile-relative /
slug-only message.

## Commands you will need

| Purpose        | Command                                                          | Expected on success |
|----------------|-----------------------------------------------------------------|---------------------|
| Format check   | `cargo fmt --all -- --check`                                     | exit 0, no diff     |
| Lint           | `cargo clippy --all-targets -- -D warnings`                     | exit 0              |
| Build          | `cargo build`                                                   | exit 0              |
| Unit tests     | `cargo test --lib skills_install`                              | all pass            |
| Unit tests     | `cargo test --lib author_skill`                                | all pass            |
| Doc grep (neg) | `grep -rn 'rantaiclaw skill ' docs/pillars/4-skills-mcp.md`     | no matches          |
| Doc grep (neg) | `grep -n 'enabled = \[' docs/pillars/4-skills-mcp.md`          | no matches          |

Note: `strict-clippy-delta` and `setup_e2e` are POST-merge CI gates — run the
scoped `clippy`/`test` above locally before merging. Markdown lint (`markdownlint`)
runs on docs if available; if not installed, skip it and note that in the PR.

## Scope

**In scope** (the only files you should modify):

- `docs/pillars/4-skills-mcp.md`
- `docs/reference/config.md`
- `docs/reference/commands.md`
- `src/tools/skills_install.rs`
- `src/tools/author_skill.rs`

**Out of scope** (do NOT touch, even though they look related):

- `src/tools/skills_meta.rs` — the SEC-06 finding named `skill_view` here, but
  its JSON payload (`src/tools/skills_meta.rs:152-165`) does **not** emit an
  absolute path (it only reads the file at `location` to return `skill_md` body;
  there is no `location`/path field in the output). No leak to fix. See Drift
  note below.
- The approval mechanism itself (`crate::approval::ApprovalManager`, autonomy
  gating). This plan removes a *dead advertised param*; it changes nothing about
  whether/how installs are approved.
- `src/config/schema.rs`, `src/tools/mod.rs` — read for ground truth only; the
  schema doc-comment at `schema.rs:488-490` is already correct.
- `docs/pillars/4-skills-mcp.md` lines 84-86 (`setup skills`/`setup mcp`) and
  92-96 (the `[mcp.<server-name>]` block) — NOT this plan's concern; leave them
  byte-for-byte unchanged. (The `[mcp.<server-name>]` shape is separately suspect
  — see Maintenance notes — but fixing MCP docs is a different plan.)

## Git workflow

- Branch: `advisor/044-skills-docs-and-hygiene`
- Commit per logical unit (docs commit, then the two code commits are fine, or
  one commit total for this small plan). Conventional-commit style, e.g.
  `docs(skills): correct CLI command name and config shape` and
  `refactor(tools): drop dead skills_install approved param; stop path leak`.
- **Do NOT** add a `Co-Authored-By` trailer (repo convention).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Fix the pillar doc command + config block (DOCS-01)

In `docs/pillars/4-skills-mcp.md`:

1. Change the three singular `rantaiclaw skill ...` lines (81-83) to plural
   `rantaiclaw skills ...`:
   - `rantaiclaw skills list`
   - `rantaiclaw skills install <source>      # ClawHub slug, git remote, or local path`
   - `rantaiclaw skills remove <name>`
2. Replace the fabricated `[skills]\nenabled = ["web-search", "summarizer"]`
   block (lines 88-90) with the real per-entry shape:

   ```toml
   [skills.entries.web-search]
   enabled = true

   [skills.entries.summarizer]
   enabled = false          # config-only toggle; there is no CLI enable/disable
   ```

Leave the `[mcp.<server-name>]` block and the `setup` lines untouched.

**Verify**: `grep -rn 'rantaiclaw skill ' docs/pillars/4-skills-mcp.md` → no
matches; `grep -n 'enabled = \[' docs/pillars/4-skills-mcp.md` → no matches;
`grep -n 'skills.entries.web-search' docs/pillars/4-skills-mcp.md` → 1 match.

### Step 2: Fix the env-source `api_key` key in the config reference (DOCS-02)

In `docs/reference/config.md`, in the worked example (lines 165-167), change
`value = "WEATHER_API_KEY"` to `id = "WEATHER_API_KEY"` so the `env` source uses
the correct key. Do not change the `source = "env"` line.

**Verify**: `grep -n 'id = "WEATHER_API_KEY"' docs/reference/config.md` → 1
match; `grep -n 'value = "WEATHER_API_KEY"' docs/reference/config.md` → no
matches.

### Step 3: Document `skills show` and the config-only toggle (DOCS-03)

In `docs/reference/commands.md`, in the `skills` list (lines 202-207):

1. Add a bullet directly under `- \`rantaiclaw skills list\``:
   `- \`rantaiclaw skills show <name>\`` (metadata for one installed skill; CLI
   parity for the TUI `/skill <name>`).
2. Add one signpost line after the bullet list (before the `<source>`
   paragraph), e.g.:
   `There is no \`skills enable\`/\`skills disable\` command — toggle a skill on
   or off in config via \`[skills.entries.<name>] enabled = false\` (see
   \`docs/reference/config.md\`).`

**Verify**: `grep -n 'skills show <name>' docs/reference/commands.md` → 1 match;
`grep -n 'enabled = false' docs/reference/commands.md` → at least 1 match.

### Step 4: Remove the dead `approved` param and fix the module doc (BUG-04)

In `src/tools/skills_install.rs`:

1. Delete the `"approved": { ... }` property object from **both**
   `parameters_schema()` implementations (`:62-66` and `:149-153`). Leave the
   `"required"` arrays as-is (`approved` was never required). The result for
   `SkillsInstallTool` is a `properties` object with only `slug`; for
   `SkillsInstallDepsTool`, only `name`.
2. Rewrite the module-doc bullets (`:5-12`) so they no longer instruct the model
   to pass `approved: true`. Keep the accurate paragraph at `:14-18`. Target
   shape:

   ```rust
   //! - `skills_install` → installs a ClawHub skill by slug. Wraps
   //!   `clawhub::install_one`. Approval-gated in supervised mode (see
   //!   below); the model does not self-confirm.
   //! - `skills_install_deps` → runs the install recipe for an already-
   //!   installed-but-gated skill (brew/uv/npm/go/download). Wraps
   //!   `install_deps_for_with_prefs`. Same approval gate.
   ```

3. Update the test `install_tools_have_stable_names_and_advertise_approved_arg`
   (`:326-343`): rename it to `install_tools_have_stable_names_and_no_approved_arg`
   and replace the two `assert!(schema[...]["approved"].is_object())` lines with
   assertions that the param is **absent** and the real param is present, e.g.:

   ```rust
   assert!(schema["properties"]["approved"].is_null());
   assert!(schema["properties"]["slug"].is_object());
   ...
   assert!(schema2["properties"]["approved"].is_null());
   assert!(schema2["properties"]["name"].is_object());
   ```

**Verify**: `grep -n 'approved' src/tools/skills_install.rs` → no matches in the
two `parameters_schema` bodies or the module doc (only the renamed test's
`is_null()` assertions may reference the word); `cargo test --lib skills_install`
→ all pass.

### Step 5: Stop leaking host paths from the skill tools (SEC-06)

In `src/tools/skills_install.rs`, `SkillsInstallTool::execute` success arm
(`:90-98`): drop `profile.skills_dir().display()` from the `output` string and
emit it via tracing instead. Target shape:

```rust
            Ok(()) => {
                tracing::info!(
                    skill = %slug_owned,
                    dir = %profile.skills_dir().display(),
                    "installed clawhub skill"
                );
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Installed `{slug_owned}` from ClawHub into the active \
                         profile's skills directory. The agent will see it on the \
                         next turn — call `skills_list` if you need to confirm."
                    ),
                    error: None,
                })
            }
```

In `src/tools/author_skill.rs`, the success arm (`:288-296`): drop
`skill_md.display()` from `output` and log it via tracing. Target shape:

```rust
        tracing::info!(skill = %slug, path = %skill_md.display(), "authored skill");
        Ok(ToolResult {
            success: true,
            output: format!(
                "{verb} skill `{slug}`. It will be available on the next turn \
                 (restart channel runtimes to pick it up immediately)."
            ),
            error: None,
        })
```

Leave the error-path messages (`author_skill.rs:274-281`) as-is — they only fire
on a filesystem write failure and their path helps the operator diagnose (see
Maintenance notes for the optional follow-up).

**Verify**: `cargo build` → exit 0; `cargo test --lib author_skill` → all pass.

## Test plan

- **BUG-04**: `src/tools/skills_install.rs` — the renamed
  `install_tools_have_stable_names_and_no_approved_arg` now asserts `approved` is
  absent and the real params (`slug`, `name`) are present. Model after the
  existing test at `:326-343`.
- **SEC-06**: `src/tools/author_skill.rs` — add one regression test in the
  `tests` module (model after `execute_writes_a_valid_skill_file` at `:406-426`,
  which already builds a `TempDir` tool via `tool_in(&tmp)`):

  ```rust
  #[tokio::test]
  async fn execute_success_output_does_not_leak_absolute_path() {
      let tmp = TempDir::new().unwrap();
      let tool = tool_in(&tmp);
      let res = tool
          .execute(json!({ "name": "Weather Reporter", "description": "Reports the weather." }))
          .await
          .unwrap();
      assert!(res.success, "error: {:?}", res.error);
      // Host path must not appear in model-facing output; it goes to tracing.
      assert!(!res.output.contains(tmp.path().to_str().unwrap()));
  }
  ```

- Verification: `cargo test --lib skills_install author_skill` → all pass,
  including the two new/renamed tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo build` exits 0
- [ ] `cargo test --lib skills_install author_skill` exits 0; the renamed
      `no_approved_arg` test and the new `does_not_leak_absolute_path` test exist
      and pass
- [ ] `grep -rn 'rantaiclaw skill ' docs/pillars/4-skills-mcp.md` → no matches
- [ ] `grep -n 'enabled = \[' docs/pillars/4-skills-mcp.md` → no matches
- [ ] `grep -n 'value = "WEATHER_API_KEY"' docs/reference/config.md` → no matches
- [ ] `grep -n 'skills show <name>' docs/reference/commands.md` → 1 match
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The drift check shows any in-scope file changed since `4736e2e` and its live
  content no longer matches the "Current state" excerpts.
- `src/main.rs` no longer defines the command as `Skills { subcommand:
  SkillCommands }` or `SkillCommands` no longer has a `Show` variant — the docs
  you are told to write would then be wrong; report instead.
- Removing `approved` from a `parameters_schema` reveals that some caller (grep
  `"approved"` across `src/`) actually reads it from these two tools' args — the
  premise "the param is a no-op" would be false.
- A verification fails twice after a reasonable fix attempt.
- The fix appears to require touching a file outside the in-scope list.

## Maintenance notes

For the human/agent who owns this code after the change lands:

- **Future plans 037/038** (skills CLI enable/disable commands) will, if they
  land, add real `skills enable`/`skills disable` subcommands. When they do,
  update the DOCS-03 signpost in `commands.md` (and the pillar doc) to reference
  the new commands. Until then, config-only is the correct documented reality.
- **Adjacent MCP-doc bug (not fixed here)**: `docs/pillars/4-skills-mcp.md:92`
  documents `[mcp.<server-name>]`, but the working example at line 71 of the same
  file — and the config schema — use `[mcp_servers.<name>]`. This is a separate
  finding; verify the real key against `src/config/schema.rs` before writing a
  fix, and do it in its own docs plan.
- **SEC-06 error-path follow-up (deferred)**: `author_skill.rs:274-281` still
  interpolates absolute paths into *error* messages. These fire only on a
  filesystem write failure and the path aids operator diagnosis, so they are left
  as-is here. If a stricter no-host-paths-anywhere policy is adopted, route those
  through tracing too and return a generic error to the model.
- A reviewer should confirm: (a) approval behavior is unchanged (this is a
  doc/param-only change — no `ApprovalManager` edits); (b) the `tracing::info!`
  calls do not themselves log a secret (they log skill name + dir/path only, no
  key material); (c) the two `parameters_schema` `required` arrays still list the
  real required params.
