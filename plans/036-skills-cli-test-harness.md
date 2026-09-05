# Plan 036: Add a reusable test harness + disable-filter coverage for the `skills` CLI

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 4736e2e..HEAD -- src/skills/mod.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW (tests only — no production code changes)
- **Depends on**: none. **This plan is the enabler and must be executed FIRST** — plans 034 and 035 reuse the harness it introduces.
- **Category**: tests
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

The entire `skills` CLI dispatch (`handle_command`, `src/skills/mod.rs:1169`) — `list`, `show`, `install`, `remove`, `update`, `inspect`, `install-deps` — has **zero** direct handler-level tests, and the loader's per-skill disable filter (`entries.<name>.enabled = false`) is likewise untested. These are exactly the code paths that plans 034 (`skills remove` true-uninstall) and 035 (`skills update` non-destructive) will modify. Landing those fixes without a test seam means the fixes ship unverified. This plan builds a small, reusable tempdir + fake-profile/workspace harness and adds a first regression test (the disable filter), so 034 and 035 can add behavior tests instead of scaffolding.

## Current state

Files:

- `src/skills/mod.rs` — the skills subsystem. `handle_command` (dispatch, line 1169) is wired from `src/main.rs` (the `SkillCommands` enum is declared at `src/main.rs:1242`). Loaders and the disable filter live at lines 225–279. The in-crate `#[cfg(test)] mod tests` starts at line 1618 and covers parsers/loaders only — never `handle_command`, never the disable filter. A second test submodule `mod symlink_tests` is declared at line 2127 (file `src/skills/symlink_tests.rs`).

The disable filter as it exists today — `load_skills_with_config` (`src/skills/mod.rs:225-251`):

```rust
pub fn load_skills_with_config(workspace_dir: &Path, config: &crate::config::Config) -> Vec<Skill> {
    let raw = load_skills_with_open_skills_config(
        workspace_dir,
        Some(config.skills.open_skills_enabled),
        config.skills.open_skills_dir.as_deref(),
    );
    raw.into_iter()
        .filter(|s| {
            if let Some(entry) = config.skills.entries.get(&s.name) {
                if !entry.enabled {
                    tracing::debug!(skill = %s.name, "skipped: disabled in config.toml");
                    return false;
                }
            }
            let unmet = s.requires.unmet();
            if !unmet.is_empty() {
                tracing::debug!(
                    skill = %s.name,
                    reasons = %unmet.join("; "),
                    "skipped: unmet requires"
                );
                return false;
            }
            true
        })
        .collect()
}
```

`load_skills_with_status` (`src/skills/mod.rs:256-279`) — same source set, but keeps gated skills and prepends the reason:

```rust
pub fn load_skills_with_status(
    workspace_dir: &Path,
    config: &crate::config::Config,
) -> Vec<(Skill, Vec<String>)> {
    let raw = load_skills_with_open_skills_config(
        workspace_dir,
        Some(config.skills.open_skills_enabled),
        config.skills.open_skills_dir.as_deref(),
    );
    let mut out: Vec<(Skill, Vec<String>)> = raw
        .into_iter()
        .map(|s| {
            let mut reasons = s.requires.unmet();
            if let Some(entry) = config.skills.entries.get(&s.name) {
                if !entry.enabled {
                    reasons.insert(0, "disabled in config.toml".to_string());
                }
            }
            (s, reasons)
        })
        .collect();
    out.sort_by_key(|(_, reasons)| !reasons.is_empty());
    out
}
```

**Skill identity vs. on-disk directory** (load-bearing for the harness fixtures — plans 034/035 depend on this distinction):

- A skill's `name` comes from SKILL.md frontmatter `name:` (fallback = directory name) — `load_skill_md`, `src/skills/mod.rs:636-640`:
  ```rust
  let frontmatter_name = frontmatter.get("name").cloned();
  // ...
  name: frontmatter_name.unwrap_or(name),   // `name` = dir.file_name()
  ```
  or from `SKILL.toml` `[skill].name` — `load_skill_toml`, `src/skills/mod.rs:600-601`.
  So `skill.name` may differ from the on-disk directory/slug. `list`/`show`/`entries.<name>` all key on `skill.name`.

**The three skill roots** the loader reads, in precedence order — `load_workspace_skills`, `src/skills/mod.rs:298-346`:

1. `profile.skills_dir()` = `~/.rantaiclaw/profiles/<name>/skills` (`src/profile/paths.rs:90-92`). ClawHub `install_one` and bundled `install_pack` write here.
2. `<workspace_dir>/../skills`
3. `<workspace_dir>/skills` (= `skills_dir(workspace_dir)`, `src/skills/mod.rs:1063-1065`). The local-path `skills install` symlinks here.

Dedup by `skill.name`; earliest root wins. In a default profile, roots (1) and (2) collapse to the same path; they split when `workspace_dir` is overridden to a non-profile path — which is what the harness does to isolate fixtures.

**Env-mutation gotcha (critical for a non-flaky harness).** The loader calls `crate::profile::ProfileManager::active()` (`src/skills/mod.rs:321`), which reads **process-global** env vars (`HOME`, `RANTAICLAW_PROFILE`). `cargo test --lib` runs all unit tests in one process across many threads, so any test that mutates those vars must serialize against **every other** test that does — not just tests in this module. The crate ships a single shared lock for exactly this: `crate::test_env::ENV_LOCK` (a `tokio::sync::Mutex`, `src/test_env.rs:22`; use `.blocking_lock()` in sync `#[test]`, `.lock().await` in `#[tokio::test]`). NOTE: the existing skills test module has its own **module-local** `open_skills_env_lock()` (`OnceLock<Mutex<()>>`, `src/skills/mod.rs:1625-1628`) — that lock does NOT serialize against other modules and must not be relied on for `HOME`/`RANTAICLAW_PROFILE` mutation. Route all `HOME`/`RANTAICLAW_PROFILE`/`RANTAICLAW_CONFIG_DIR` mutation through `crate::test_env::ENV_LOCK`.

**Exemplar to copy — an existing in-crate test that builds a `Config` and calls the loader against a tempdir workspace** (`src/skills/mod.rs:2103-2124`). This is the structural pattern for the disable-filter test in this plan:

```rust
let dir = tempfile::tempdir().unwrap();
let workspace_dir = dir.path().join("workspace");
fs::create_dir_all(workspace_dir.join("skills")).unwrap();
// ...
let mut config = crate::config::Config::default();
config.workspace_dir = workspace_dir.clone();
config.skills.open_skills_enabled = true;
config.skills.open_skills_dir = Some(open_skills_dir.to_string_lossy().to_string());

let skills = load_skills_with_config(&workspace_dir, &config);
assert_eq!(skills.len(), 1);
assert_eq!(skills[0].name, "http_request");
```

`Config::default()` exists and is used in-crate. `config.skills.entries` is a `HashMap<String, SkillEntryConfig>` keyed on `skill.name` (schema: `src/config/schema.rs:501`; `SkillEntryConfig` at `:549`, `enabled` field). `config.skills.open_skills_enabled` defaults `false` — leave it default so the harness never touches the network / open-skills git clone.

**Cross-repo mock exemplar** (for plans 034/035, referenced here so the harness lives alongside it): `tests/onboard_skills_section.rs` — `with_home` (lines 26-54) redirects `$HOME` to a tempdir under a `HOME_LOCK` mutex; `spawn_mock_clawhub_full` (lines 288-373) is a single-socket mock of the three ClawHub endpoints `install_one` walks; `sha256_hex` (line 375) computes the manifest hash. That file is an **integration** test (`tests/`), which is why it uses its own `HOME_LOCK` rather than the crate-internal `crate::test_env::ENV_LOCK` (integration tests cannot see `pub(crate)` items).

## Commands you will need

| Purpose      | Command                                                        | Expected on success        |
|--------------|---------------------------------------------------------------|----------------------------|
| Format check | `cargo fmt --all -- --check`                                  | exit 0                     |
| Lint (scoped)| `cargo clippy --all-targets -- -D warnings`                   | exit 0, no warnings        |
| Unit tests   | `cargo test --lib skills::`                                   | all pass incl. new tests   |
| Focused test | `cargo test --lib disable_filter`                             | new test(s) pass           |

Notes:
- Full `cargo test` (all targets) is **disk-heavy** on this box — prefer `cargo test --lib <filter>`.
- `strict-clippy-delta` and `setup_e2e` are **post-merge** CI gates. Run the scoped `cargo clippy --all-targets -- -D warnings` locally before merge so a delta-only warning doesn't surface post-merge.

## Suggested executor toolkit

- If a `rust-skills` skill is available, invoke it when writing the harness (test-organization + tempdir hygiene rules).
- Read `tests/onboard_skills_section.rs:26-54` and `:288-373` before writing the harness — reuse its shapes rather than inventing new ones.

## Scope

**In scope** (the only files you should modify):
- `src/skills/mod.rs` — add the harness helpers and the disable-filter test inside the existing `#[cfg(test)] mod tests` block (starts line 1618). Adding a small private test helper module is allowed **only under `#[cfg(test)]`**.

**Out of scope** (do NOT touch):
- Any non-test code in `src/skills/mod.rs` — this plan changes behavior of **nothing**; it only adds `#[cfg(test)]` code. If you find yourself editing a non-`#[cfg(test)]` line, STOP.
- `src/skills/clawhub.rs`, `src/skills/bundled/mod.rs` — the harness must not require changes there.
- `tests/onboard_skills_section.rs` — read it as a pattern; do not edit it.
- The `remove`/`update` handler logic — that is plans 034/035. Do not "fix while here".

## Git workflow

- Branch: `advisor/036-skills-cli-test-harness`
- Commit style: conventional commits, matching repo history. Example from `git log`: `feat(gateway): persist web "Always" grants across the conversation (session parity)`. For this plan: `test(skills): add CLI handler harness + disable-filter coverage`.
- **Repo rule: do NOT add any `Co-Authored-By` trailer** to commits or PR bodies.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the reusable harness helpers (test-only)

Inside the existing `#[cfg(test)] mod tests` block in `src/skills/mod.rs` (or a nested `#[cfg(test)]` helper module it can call), add a helper that builds an isolated fake profile + workspace. It must:

1. Acquire `crate::test_env::ENV_LOCK.blocking_lock()` and hold the guard for the whole test body (return it to the caller so the guard's lifetime spans the test).
2. Create a `tempfile::TempDir`, point `HOME` at it, and `remove_var("RANTAICLAW_PROFILE")` — restoring both on drop (model the save/restore on `tests/onboard_skills_section.rs:29-50`; you may reuse the existing `EnvVarGuard` pattern at `src/skills/mod.rs:1630-1651`).
3. Provide a way to write a skill fixture on disk: given a root dir, a **directory name** (slug), and an optional frontmatter `name:`, create `<root>/<dirname>/SKILL.md`. Keep the dir-name and manifest-name independently settable — plans 034/035 need `dirname != name` fixtures.
4. Return the tempdir, the resolved `workspace_dir` to pass into `handle_command`/loaders, and the held env guard.

Keep it KISS — a couple of small free functions, not a framework. Do not add helpers no test in this plan uses (YAGNI); plans 034/035 will extend it as needed.

**Verify**: `cargo test --lib skills::` → compiles and all existing tests still pass. `cargo fmt --all -- --check` → exit 0.

### Step 2: Add the disable-filter regression test

Add a test named for its behavior (e.g. `disable_filter_excludes_config_disabled_skill`) that:

1. Uses the harness / the exemplar at `src/skills/mod.rs:2103-2124`.
2. Writes **two** skills into the workspace root (root 3, `<workspace_dir>/skills`): `skill-a` and `skill-b` (each a dir with a `SKILL.md`). To keep the loader deterministic and off the network, leave `config.skills.open_skills_enabled = false` (the default). Point `HOME` at the tempdir so the profile root (root 1) is empty.
3. Builds `Config::default()`, sets `config.workspace_dir`, and inserts a `SkillEntryConfig { enabled: false, .. }` into `config.skills.entries` keyed on **`skill.name`** of `skill-b` (use `Default::default()` for `SkillEntryConfig` and set `enabled = false`).
4. Asserts:
   - `load_skills_with_config(&workspace_dir, &config)` returns exactly one skill, and it is `skill-a` (the disabled one is filtered out).
   - `load_skills_with_status(&workspace_dir, &config)` returns both, and the entry for `skill-b` has a reasons vector whose first element is exactly `"disabled in config.toml"` (matches `src/skills/mod.rs:271`).

**Verify**: `cargo test --lib disable_filter` → the new test passes. `cargo test --lib skills::` → all skills tests pass.

### Step 3: Full local validation

**Verify**:
- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --all-targets -- -D warnings` → exit 0, no warnings
- `cargo test --lib skills::` → all pass, including the new disable-filter test
- `git status` → only `src/skills/mod.rs` modified (plus `plans/README.md` in the final step)

## Test plan

- New tests (all in `src/skills/mod.rs`, under `#[cfg(test)] mod tests`):
  - `disable_filter_excludes_config_disabled_skill` — happy path + the regression: two skills, one disabled via `config.skills.entries`; assert `load_skills_with_config` returns only the enabled skill and `load_skills_with_status` marks the other gated with reason `"disabled in config.toml"`.
- Structural pattern to copy: the existing test at `src/skills/mod.rs:2103-2124` (builds `Config::default()`, sets `workspace_dir`, calls the loader against a tempdir workspace) and the `EnvVarGuard` pattern at `src/skills/mod.rs:1630-1651`.
- Verification: `cargo test --lib skills::` → all pass, including the 1 new test.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib skills::` exits 0; the new `disable_filter_*` test exists and passes
- [ ] A reusable harness helper (tempdir + fake-profile/workspace, env-locked via `crate::test_env::ENV_LOCK`, supports `dirname != manifest-name` fixtures) exists under `#[cfg(test)]` in `src/skills/mod.rs`
- [ ] No non-`#[cfg(test)]` lines changed in `src/skills/mod.rs` (`git diff` review)
- [ ] No files outside the in-scope list modified (`git status`), except `plans/README.md`
- [ ] `plans/README.md` status row for 036 updated to DONE

## STOP conditions

Stop and report back (do not improvise) if:

- The disable-filter code at `src/skills/mod.rs:233-238` or `:269-273` does not match the excerpts above (drift).
- `crate::test_env::ENV_LOCK` no longer exists at `src/test_env.rs:22` or is no longer `pub(crate)` — the harness needs it; do not fall back to a module-local lock silently.
- `Config::default()` no longer compiles or `config.skills.entries` / `SkillEntryConfig.enabled` changed shape (schema drift at `src/config/schema.rs:465-549`).
- Making the disable-filter test deterministic appears to require touching non-test code (e.g. the loader unconditionally clones an open-skills git repo even with `open_skills_enabled = false`). If so, report — do not modify production code.
- The new test is flaky across two consecutive `cargo test --lib skills::` runs (likely an env-lock scope bug — the guard must span the whole test body).

## Maintenance notes

- Plans 034 and 035 extend this harness (034 adds found-in-profile-dir / dir-name≠manifest-name fixtures; 035 adds a ClawHub mock, cross-referencing `tests/onboard_skills_section.rs:288-373`). Keep the fixture-writer flexible enough that `dirname != name` is expressible without a rewrite.
- A reviewer should confirm the harness holds `crate::test_env::ENV_LOCK` for the **entire** test (not just setup), and that `HOME` is restored on every exit path including panics.
- Deferred out of this plan (intentionally): behavior tests for `list`/`show`/`install`/`inspect`/`install-deps` handlers. This plan only seeds the harness + the disable filter; broadening coverage to the other subcommands is a follow-up once 034/035 land.
