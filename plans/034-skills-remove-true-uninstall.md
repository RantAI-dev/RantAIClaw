# Plan 034: Make `skills remove <name>` a true uninstall across all three skill roots

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
- **Risk**: MED (`remove_dir_all` on a resolved path — traversal/containment safety is load-bearing)
- **Depends on**: `plans/036-skills-cli-test-harness.md` (soft — for the test harness; if 036 is not yet done, either do it first or port its harness helper into this plan's test).
- **Category**: bug
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

`skills remove <name>` only ever looks in **one** of the three roots the loader reads — `<workspace_dir>/skills` (root 3). But the primary install paths (ClawHub `install_one`, bundled starter/core packs) write to `profile.skills_dir()` (root 1). So `skills remove <a-clawhub-or-bundled-skill>` fails with "Skill not found" even though `skills list` shows it — remove is **not a true uninstall for the main install path**. Worse, the handler joins the raw user argument as a *directory* name, while `list`/`show`/`entries.<name>` key on the manifest `skill.name`; when a skill's dir-name differs from its manifest-name, the identifier the user sees in `skills list` literally cannot be removed. After this plan, `remove` resolves a skill the same way the loader does (by loaded identity → its real on-disk `location`) and deletes from whichever root actually holds it, while preserving the existing traversal and containment guards.

## Current state

Files:

- `src/skills/mod.rs` — `handle_command` dispatch; the `Remove` arm is at lines 1421-1462.
- Skill identity: `skill.name` = SKILL.md frontmatter `name:` (fallback dir name; `src/skills/mod.rs:636-640`) or `SKILL.toml [skill].name` (`:600-601`). May differ from the on-disk directory/slug. `list`/`show` match on `skill.name` case-insensitively (`src/skills/mod.rs:1252`).
- Each loaded `Skill` carries `location: Option<PathBuf>` (`src/skills/mod.rs:34-35`), set to the **manifest file path** (`.../SKILL.md` or `.../SKILL.toml`) — see `load_skill_toml` (`:608`) and `load_skill_md` (`:647`). The skill's on-disk **directory** is that path's parent.
- The three roots (loader precedence, `load_workspace_skills`, `src/skills/mod.rs:298-346`): (1) `profile.skills_dir()` = `~/.rantaiclaw/profiles/<name>/skills` (`src/profile/paths.rs:90-92`) — ClawHub + bundled write here; (2) `<workspace_dir>/../skills`; (3) `<workspace_dir>/skills` = `skills_dir(workspace_dir)` (`src/skills/mod.rs:1063-1065`) — local-path `skills install` symlinks here.

The `Remove` arm exactly as it exists today — `src/skills/mod.rs:1421-1462`:

```rust
        crate::SkillCommands::Remove { name } => {
            // Reject path traversal attempts
            if name.contains("..") || name.contains('/') || name.contains('\\') {
                anyhow::bail!("Invalid skill name: {name}");
            }

            let skills_root = skills_dir(workspace_dir);
            let skill_path = skills_root.join(&name);

            // Verify the path *itself* (not the symlink target) lives directly
            // under <skills_root>. Pre-fix code canonicalized the symlink target
            // and rejected legit installs whose source was outside the workspace
            // (the common `skills install /tmp/foo` flow).
            let canonical_skills = skills_root
                .canonicalize()
                .unwrap_or_else(|_| skills_root.clone());
            // Use `parent().canonicalize()` to verify containment without
            // resolving a symlink target.
            if let Some(parent) = skill_path.parent() {
                if let Ok(canonical_parent) = parent.canonicalize() {
                    if canonical_parent != canonical_skills {
                        anyhow::bail!("Skill path escapes skills directory: {name}");
                    }
                }
            }

            // Use symlink_metadata so we don't fail on dangling symlinks.
            let meta = std::fs::symlink_metadata(&skill_path)
                .map_err(|_| anyhow::anyhow!("Skill not found: {name}"))?;

            if meta.file_type().is_symlink() {
                std::fs::remove_file(&skill_path)?;
            } else {
                std::fs::remove_dir_all(&skill_path)?;
            }
            println!(
                "  {} Skill '{}' removed.",
                console::style("✓").green().bold(),
                name
            );
            Ok(())
        }
```

Two defects in that arm:

1. **Wrong root.** `skills_root = skills_dir(workspace_dir)` is root (3) only. A ClawHub/bundled skill lives in root (1) `profile.skills_dir()`, so `symlink_metadata` at line 1448 fails → "Skill not found". (Bundled examples that live in root 1: the starter pack and the core `owner-permissions` skill — see `src/skills/bundled/mod.rs:80-98` `install_pack`, and `install_core_skills` `:113-115`.)
2. **Identifier mismatch.** Line 1428 joins `name` as a **directory** name (`skills_root.join(&name)`). But `list`/`show` show the manifest `skill.name`. When dir-name ≠ manifest-name, the name the user sees can't be removed.

Guards to **preserve** (do not weaken):
- Traversal reject: `name.contains("..") || name.contains('/') || name.contains('\\')` → bail (`:1423`).
- Per-candidate containment: the resolved directory's canonical **parent** must equal the canonical root it was found under (adapted from `:1439-1445`). The existing code compares `parent().canonicalize()` to the canonical root **without** resolving a symlink target — keep that property so a legit `skills install /tmp/foo` symlink (whose target is outside the workspace) is still removable.
- `symlink_metadata` (not `metadata`) so dangling symlinks are removable; symlinks are `remove_file`, real dirs are `remove_dir_all` (`:1447-1455`).

Design vocabulary from CLAUDE.md this change must honor: KISS (straightforward control flow, explicit match branches), Fail Fast + Explicit Errors (bail on unresolvable / out-of-root), Reversibility (small scope). `src/skills/**` is a **High-risk** path per §5 — include a boundary/failure-mode test.

## Commands you will need

| Purpose      | Command                                                        | Expected on success        |
|--------------|---------------------------------------------------------------|----------------------------|
| Format check | `cargo fmt --all -- --check`                                  | exit 0                     |
| Lint         | `cargo clippy --all-targets -- -D warnings`                   | exit 0, no warnings        |
| Unit tests   | `cargo test --lib skills::`                                   | all pass                   |
| Focused test | `cargo test --lib remove_`                                    | new remove tests pass      |

Notes:
- Full `cargo test` is **disk-heavy** on this box — prefer `cargo test --lib <filter>`.
- `strict-clippy-delta` + `setup_e2e` are **post-merge** gates; run scoped `cargo clippy --all-targets -- -D warnings` locally before merge.

## Suggested executor toolkit

- Invoke `rust-skills` if available when writing the resolution + fs code (path handling, error propagation).
- Read `src/skills/symlink_tests.rs` (the `mod symlink_tests` submodule) for the repo's symlink test idioms before writing the symlink-vs-dir test.

## Scope

**In scope** (the only files you should modify):
- `src/skills/mod.rs` — the `Remove` arm (`:1421-1462`) and its tests under `#[cfg(test)]`.

**Out of scope** (do NOT touch):
- The `Install`, `Update`, `Inspect`, `InstallDeps` arms — unrelated; `Update` is plan 035.
- `src/skills/clawhub.rs`, `src/skills/bundled/mod.rs` — do not change install behavior.
- `src/onboard/section/channels.rs` — the re-seed of `owner-permissions` on setup (`:150`) is expected behavior; document it, do not change it.
- The traversal-reject and containment guards' **security intent** — you may relocate/adapt them per candidate root, but must not remove or loosen them.

## Git workflow

- Branch: `advisor/034-skills-remove-true-uninstall`
- Commit style: conventional commits. Example from `git log`: `fix(tools): retire redundant \`schedule\` tool in favor of delivery-capable \`cron_add\``. For this plan: `fix(skills): make \`skills remove\` a true uninstall across all roots`.
- **Repo rule: do NOT add any `Co-Authored-By` trailer.**
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Resolve the skill by loaded identity, not by raw dir-join

Rewrite the body of the `Remove` arm so it:

1. Keeps the traversal reject verbatim at the top (`name.contains("..") || name.contains('/') || name.contains('\\')` → bail).
2. Loads the skill set the same way `list`/`show` do: `let skills = load_skills_with_config(workspace_dir, config);` then finds the target by identity: `skills.iter().find(|s| s.name.eq_ignore_ascii_case(&name))` (match `show`'s case-insensitive rule at `:1252`).
3. If not found → `anyhow::bail!("Skill not found: {name}. Run \`rantaiclaw skills list\`.")` (Fail Fast).
4. Derives the on-disk **directory** from the found skill's `location` (the manifest path): `let manifest = skill.location.as_ref()...; let skill_dir = manifest.parent()...`. If `location` is `None` (should not happen for disk-loaded skills) → bail with an explicit error.

Rationale: `location` is set to the real manifest path in whichever root the loader picked (root 1/2/3), so this fixes both the wrong-root bug and the dir-name≠manifest-name bug at once, without the handler re-deriving root precedence.

**Verify**: `cargo build` (or `cargo check`) → compiles. (No behavior test yet; Step 3 adds them.)

### Step 2: Re-apply the containment guard against the known roots, then delete

Before deleting, prove `skill_dir` is contained in one of the three known roots — do **not** delete an arbitrary resolved path:

1. Build the candidate root list exactly as the loader does (mirror `load_workspace_skills`, `src/skills/mod.rs:321-343`): root (1) `ProfileManager::active().ok().map(|p| p.skills_dir())`, root (2) `workspace_dir.parent().map(|p| p.join("skills"))`, root (3) `skills_dir(workspace_dir)`.
2. Compute `skill_dir.parent()` and require that its `canonicalize()` equals the `canonicalize()` of **at least one** candidate root (reuse the "canonicalize the parent, not the symlink target" technique from the current `:1439-1445` so an outside-workspace symlink target stays removable). If it matches none → `anyhow::bail!("Skill path escapes known skills roots: {name}")`. **This is the primary safety gate — see STOP conditions.**
3. `symlink_metadata(&skill_dir)`; if symlink → `remove_file`, else → `remove_dir_all`. Keep the success `println!`.
4. If the removed skill is a bundled/core skill (its dir name matches an entry in `crate::skills::bundled::CORE_PACK` or `STARTER_PACK` — compare `skill_dir.file_name()` to the pack `slug`s), print a non-blocking warning that it may be re-seeded on next `setup`/channel-config (`owner-permissions` is re-installed by `install_core_skills`, `src/onboard/section/channels.rs:150`). Warn only — do NOT block the removal.

Keep control flow flat and branches explicit (KISS). Do not introduce a shared "root list" helper unless a third caller already needs it (YAGNI / rule-of-three — the loader having its own copy is fine).

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0. `cargo fmt --all -- --check` → exit 0.

### Step 3: Tests (uses the plan 036 harness)

Add handler-level tests (see Test plan). Use the harness from plan 036 (tempdir + fake profile/workspace, `crate::test_env::ENV_LOCK`, `dirname != manifest-name` fixture support). If 036 is not yet merged, port its harness helper into this plan's test module rather than blocking.

**Verify**: `cargo test --lib remove_` → all new tests pass. `cargo test --lib skills::` → all pass.

### Step 4: Full local validation

**Verify**:
- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --all-targets -- -D warnings` → exit 0
- `cargo test --lib skills::` → all pass
- `git status` → only `src/skills/mod.rs` modified (plus `plans/README.md`)

## Test plan

New tests in `src/skills/mod.rs` under `#[cfg(test)]`, each set up via the plan-036 harness:

- `remove_found_in_profile_dir` — skill installed in root (1) `profile.skills_dir()`; `remove <name>` deletes it (the core bug: previously "Skill not found").
- `remove_found_in_workspace_dir` — skill in root (3) `<workspace_dir>/skills`; `remove` deletes it (parity with old behavior).
- `remove_resolves_by_manifest_name_when_dir_differs` — skill whose dir is `pkg-dir` but SKILL.md `name: cool-skill`; `remove cool-skill` (the listed identity) succeeds and deletes `pkg-dir`.
- `remove_not_found_reports_error` — `remove nonexistent` → `Err` containing "Skill not found".
- `remove_rejects_traversal` — `remove "../evil"`, `remove "a/b"`, `remove "a\\b"` each → `Err("Invalid skill name...")` and delete nothing.
- `remove_symlinked_skill_uses_remove_file` — local-path install symlinked into root (3) with a target **outside** the workspace; `remove` unlinks the symlink (via `remove_file`) and leaves the target directory intact.
- `remove_out_of_root_path_is_rejected` — (defensive) if resolution somehow yields a dir whose parent is none of the three roots, the containment gate bails and nothing is deleted.

Structural patterns to copy: `src/skills/symlink_tests.rs` (symlink creation/asserts) and the `Config`/loader exemplar at `src/skills/mod.rs:2103-2124`.

Verification: `cargo test --lib skills::` → all pass, including the new remove tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib skills::` exits 0; the new `remove_*` tests exist and pass
- [ ] `skills remove` resolves via `load_skills_with_config` + `skill.location` (grep confirms the arm no longer does a bare `skills_dir(workspace_dir).join(&name)` as its only lookup)
- [ ] The traversal reject and a per-candidate-root canonical-parent containment check are both present in the new arm
- [ ] Removing a bundled/core skill prints a re-seed warning but still succeeds
- [ ] No files outside the in-scope list modified (`git status`), except `plans/README.md`
- [ ] `plans/README.md` status row for 034 updated

## STOP conditions

Stop and report back (do not improvise) if:

- The `Remove` arm at `src/skills/mod.rs:1421-1462` does not match the excerpt above (drift).
- **The resolved `skill_dir` canonicalizes to a parent that matches none of the three known roots — STOP, do not `remove_dir_all` it.** An out-of-root delete is the worst-case failure of this change; a miss here must abort, never guess.
- `Skill.location` is no longer set to the manifest path (check `load_skill_toml:608` / `load_skill_md:647`) — the resolution strategy depends on it.
- The containment check cannot be expressed without resolving a symlink target (which would regress the legit `skills install /tmp/foo` flow) — report rather than weaken it.
- A remove test deletes a fixture outside its tempdir, or any test is flaky across two consecutive runs (env-lock scope bug — see plan 036).
- The fix appears to need changes in `clawhub.rs`, `bundled/mod.rs`, or `onboard/section/channels.rs` (all out of scope).

## Maintenance notes

- The re-seed warning is intentionally advisory: `install_core_skills` (`src/onboard/section/channels.rs:150`) will re-create `owner-permissions` on the next channel-config/setup run by design. If a future "permanently disable a core skill" feature is added, prefer the config disable flag (`[skills.entries.<name>] enabled = false`) over deleting the dir — deletion is not durable for core skills.
- A reviewer should scrutinize the containment gate most closely: confirm it canonicalizes the **parent** (not the symlink target), covers all three roots, and bails on a miss. This is the security-relevant line of the change.
- If skill root precedence in `load_workspace_skills` (`:298-346`) changes later, the candidate-root list built in Step 2 must be kept in sync (they are intentionally duplicated per rule-of-three; a shared helper is only warranted once a third caller appears).
- Deferred: unifying the loader's root list and this handler's root list into one helper — not done here (only two callers; YAGNI).
