# Plan 035: Make `skills update` non-destructive (never delete before a successful fetch)

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
- **Effort**: S-M
- **Risk**: MED (data-loss path — the whole point is to stop destroying skills on a failed update)
- **Depends on**: `plans/036-skills-cli-test-harness.md` (soft — for the test harness; if 036 is not yet done, port its helper into this plan's test).
- **Category**: bug
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

`skills update` deletes the existing skill directory **before** re-fetching it. If the fetch fails (a 404, a transient network error), the skill is left **gone** with no restore path — a data-loss bug. `skills update --all` targets *every* loaded skill, including the bundled `owner-permissions` (which has no ClawHub origin and 404s), so `--all` can permanently wipe bundled skills. There is also a keying bug: `--all` builds its target list from manifest `skill.name`, but the existence check and the delete are keyed on the **slug directory** — so a ClawHub skill whose manifest name ≠ slug is falsely reported "not installed locally" and skipped, and the documented "local/git skills are skipped" guarantee holds only by accident of that mismatch. After this plan, update fetches into a temp dir and atomically swaps only on success (never remove-before-fetch), `--all` enumerates real installed slug directories and explicitly skips non-ClawHub-origin skills, and bundled skills survive a failed update.

## Current state

Files:

- `src/skills/mod.rs` — `handle_command` dispatch; the `Update` arm is at lines 1463-1523.
- `src/skills/clawhub.rs` — `install_one` (`:365-387`) installs a slug into `profile.skills_dir().join(slug)`; it **skips if the dir already exists** (`:368-372`, "Idempotent — leave existing user state alone. Callers wanting a clean re-install should `fs::remove_dir_all` first") and, on any inner failure, removes its own partial dir (`:380-386`).
- `src/profile/paths.rs:90-92` — `skills_dir(profile)` = `~/.rantaiclaw/profiles/<name>/skills` (root 1).
- `src/main.rs:1261-1266` — the `Update` help text promises: "Local-path / git skills are skipped (you can `git pull` or re-install those manually)."

The `Update` arm exactly as it exists today — `src/skills/mod.rs:1463-1523`:

```rust
        crate::SkillCommands::Update { slug, all } => {
            let profile =
                crate::profile::ProfileManager::active().context("resolve active profile")?;
            let skills = load_skills_with_config(workspace_dir, config);

            let targets: Vec<String> = if all {
                skills.iter().map(|s| s.name.clone()).collect()
            } else if let Some(s) = slug {
                vec![s]
            } else {
                anyhow::bail!("`skills update` needs either a slug or `--all`");
            };

            if targets.is_empty() {
                println!("Nothing to update — no installed skills.");
                return Ok(());
            }

            // Run the network-driven update inside an isolated tokio
            // runtime on a fresh OS thread. The outer thread is already
            // inside `#[tokio::main]`, so calling `block_on` here would
            // panic ("Cannot start a runtime from within a runtime").
            let result = std::thread::spawn(move || -> Result<(usize, usize, usize)> {
                let rt = tokio::runtime::Runtime::new().context("build tokio runtime")?;
                let (mut updated, mut skipped, mut failed) = (0usize, 0usize, 0usize);
                for slug in &targets {
                    let dir = profile.skills_dir().join(slug);
                    if !dir.exists() {
                        println!("  ⊘ {slug}: not installed locally — skipping");
                        skipped += 1;
                        continue;
                    }
                    if let Err(e) = std::fs::remove_dir_all(&dir) {
                        println!("  ✗ {slug}: failed to clear old install: {e}");
                        failed += 1;
                        continue;
                    }
                    match rt.block_on(crate::skills::clawhub::install_one(&profile, slug)) {
                        Ok(()) => {
                            println!("  {} {slug}: updated", console::style("✓").green().bold());
                            updated += 1;
                        }
                        Err(e) => {
                            println!("  ✗ {slug}: {e}");
                            failed += 1;
                        }
                    }
                }
                Ok((updated, skipped, failed))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("update thread panicked"))??;

            let (updated, skipped, failed) = result;
            println!();
            println!("Update summary: {updated} updated, {skipped} skipped, {failed} failed");
            if failed > 0 {
                anyhow::bail!("{failed} skill(s) failed to update");
            }
            Ok(())
        }
```

Two defects:

1. **Data loss (remove-before-fetch).** Line 1495 `std::fs::remove_dir_all(&dir)` runs **before** `install_one` (line 1500). `install_one` only cleans *its own* partial dir on failure (`clawhub.rs:380-386`); it does not restore what update already deleted. And `install_one` skips-if-exists (`clawhub.rs:368-372`), which is precisely why the current code deletes first. A 404/network error → the skill is gone. `--all` includes bundled `owner-permissions` (no ClawHub origin → 404) → permanent wipe.
2. **Name-vs-slug keying.** `--all` builds `targets` from `skills.iter().map(|s| s.name.clone())` (manifest name, line 1469), but the existence check + dir are keyed on `profile.skills_dir().join(slug)` (the slug **directory**, line 1489). When manifest name ≠ slug dir, the target string is the manifest name, so `dir.exists()` is false and the skill is wrongly "skipped". The "local/git skills skipped" guarantee (help at `src/main.rs:1261-1266`) is only satisfied by this accident, not by an explicit origin check.

Design vocabulary from CLAUDE.md this change must honor: Fail Fast + Explicit Errors, **Reversibility + Rollback-First** (§3.8 — the atomic-swap is the reversibility mechanism), KISS. `src/skills/**` is a **High-risk** path (§5) → include a failure-mode test proving the old dir survives a failed fetch.

## Commands you will need

| Purpose      | Command                                                        | Expected on success        |
|--------------|---------------------------------------------------------------|----------------------------|
| Format check | `cargo fmt --all -- --check`                                  | exit 0                     |
| Lint         | `cargo clippy --all-targets -- -D warnings`                   | exit 0, no warnings        |
| Unit tests   | `cargo test --lib skills::`                                   | all pass                   |
| Focused test | `cargo test --lib update_`                                    | new update tests pass      |

Notes:
- Full `cargo test` is **disk-heavy** on this box — prefer `cargo test --lib <filter>`.
- `strict-clippy-delta` + `setup_e2e` are **post-merge** gates; run scoped `cargo clippy --all-targets -- -D warnings` locally before merge.

## Suggested executor toolkit

- Invoke `rust-skills` if available for the temp-dir + atomic-rename code and error propagation.
- The ClawHub mock is the crux of the failure-mode test. Read `tests/onboard_skills_section.rs:277-373` (`spawn_mock_clawhub_full`) and `:382-399` — that mock drives `install_one` end-to-end via the `CLAWHUB_BASE_URL_ENV` override. A "fetch fails" test uses a mock that returns 404/500 for the detail endpoint.

## Scope

**In scope** (the only files you should modify):
- `src/skills/mod.rs` — the `Update` arm (`:1463-1523`) and its tests under `#[cfg(test)]`.

**Out of scope** (do NOT touch):
- `src/skills/clawhub.rs` — do NOT change `install_one`'s skip-if-exists or partial-cleanup behavior. The update arm must work *around* `install_one` (fetch into a temp slug dir that does not yet exist, then swap), not modify it. (If a clean way genuinely requires an `install_one` signature change, that is a STOP condition — report, don't refactor `install_one`.)
- The `Remove`, `Install`, `Inspect`, `InstallDeps` arms.
- `src/main.rs:1261-1266` help text — you may leave it; the behavior will now actually match it. Only touch it if the wording becomes wrong (it should not).

## Git workflow

- Branch: `advisor/035-skills-update-nondestructive`
- Commit style: conventional commits. Example from `git log`: `fix(channels): deterministic cron_add delivery to origin chat`. For this plan: `fix(skills): make \`skills update\` non-destructive (atomic swap, never delete-before-fetch)`.
- **Repo rule: do NOT add any `Co-Authored-By` trailer.**
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Enumerate `--all` targets from real installed slug dirs, and skip non-ClawHub origins

Replace the `--all` target construction (line 1468-1469). Instead of manifest names:

1. Read the directory entries of `profile.skills_dir()` and collect the **directory names** (slugs) that contain a `SKILL.md` or `SKILL.toml`. These are the only dirs `install_one` could have written and are the only ones `update` can target. (This fixes the name-vs-slug keying bug: targets are now slug dirs, matching the `profile.skills_dir().join(slug)` lookup.)
2. Explicitly skip skills that are not ClawHub-originated. Since there is no stored origin marker, use the available signals: (a) a **symlink** dir in root 1 is a local install → skip; (b) a dir matching a `crate::skills::bundled::CORE_PACK` / `STARTER_PACK` slug is bundled → skip (these have no ClawHub source and would 404). Print `  ⊘ {slug}: not a ClawHub skill — skipping` and count as skipped. This makes the "local/git skills skipped" promise (`src/main.rs:1261-1266`) an explicit rule, not an accident.
3. Single-slug mode (`slug` provided) is unchanged in target selection, but must run through the same per-slug guard in Step 2 (a user can still name a bundled/local slug explicitly — skip it with the same message rather than wiping it).

Keep it flat and explicit (KISS). Do not add an origin field to the `Skill` struct or a config key (YAGNI) — infer from dir shape + pack membership.

**Verify**: `cargo build` → compiles.

### Step 2: Fetch into a temp dir, atomically swap only on success

Replace the per-slug body (the `remove_dir_all` at line 1495 through the `match install_one` at 1500-1509). New shape per slug:

1. `let dir = profile.skills_dir().join(slug);` — if `!dir.exists()` → keep the existing `⊘ … not installed locally` skip.
2. Apply the Step-1 origin guard; if skipped, `continue`.
3. Fetch into a **sibling temp directory** that does not yet exist, e.g. `profile.skills_dir().join(format!(".{slug}.update-tmp"))` (leading dot + suffix so it is not itself picked up as a skill dir; ensure it does not exist first, clearing any stale one). Because `install_one` skips-if-exists and writes to `profile.skills_dir().join(slug)`, you cannot point it at an arbitrary temp path directly — choose the simplest correct approach and state it in the PR:
   - **Preferred:** call `install_one(&profile, slug)` into a temporary **profile whose `skills_dir()` is the temp location**, if the `Profile` type allows constructing/overriding that cheaply; OR
   - **Fallback (KISS):** since `install_one` targets `profile.skills_dir().join(slug)` and skips-if-exists, do the swap as: (a) rename the existing `dir` to a backup sibling `dir.bak`; (b) call `install_one(&profile, slug)` (now writes fresh into `dir`); (c) on `Ok`, `remove_dir_all(dir.bak)`; on `Err`, `remove_dir_all(dir)` if present and rename `dir.bak` back to `dir` (restore). The invariant that MUST hold: **on any failure the original skill directory is present and unchanged.** This fallback never leaves the skill deleted — the backup is only removed after a proven-good install.
4. Update the counters/prints (`updated` / `failed`) as before. On failure print `  ✗ {slug}: {e} (kept existing version)`.

Whichever approach you pick, the atomic-swap invariant is the contract: a failed fetch leaves the prior directory intact and identical. Prefer `rename` (atomic on the same filesystem) over copy for the swap.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0. `cargo fmt --all -- --check` → exit 0.

### Step 3: Tests (uses plan 036 harness + the ClawHub mock)

Add the failure-mode and keying tests (see Test plan). Use the plan-036 harness for the tempdir/profile/env setup and model the mock on `tests/onboard_skills_section.rs:277-399`. Note: these tests mutate `HOME`/`CLAWHUB_BASE_URL_ENV` and drive a tokio runtime — serialize env mutation via `crate::test_env::ENV_LOCK` (async: `.lock().await`; the update arm spawns its own thread+runtime, so an integration-style `tests/` file may be cleaner — if so, add to `tests/` and use that file's own `HOME_LOCK`/`CLAWHUB_BASE_URL_ENV` pattern instead of the crate-internal lock, since integration tests can't see `pub(crate)` items).

**Verify**: `cargo test --lib update_` (or `cargo test --test <file> update_` if placed in `tests/`) → all new tests pass.

### Step 4: Full local validation

**Verify**:
- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --all-targets -- -D warnings` → exit 0
- `cargo test --lib skills::` → all pass
- `git status` → only `src/skills/mod.rs` (and possibly one new `tests/*.rs`) modified, plus `plans/README.md`

## Test plan

New tests (in `src/skills/mod.rs` under `#[cfg(test)]`, or a new `tests/skills_update.rs` if a spawned-runtime integration test is cleaner):

- `update_success_swaps_new_version` — mock ClawHub serves a new SKILL.md; after `update <slug>` the on-disk SKILL.md is the new content (happy path). Model the mock on `spawn_mock_clawhub_full` (`tests/onboard_skills_section.rs:288`).
- `update_failure_preserves_old_dir` — **the regression**: existing skill on disk with known content; point `CLAWHUB_BASE_URL_ENV` at a mock that returns 404/500 for the detail endpoint; run `update <slug>`; assert it returns/counts a failure AND the original directory + its SKILL.md content are still present and unchanged.
- `update_all_skips_non_clawhub` — a bundled `owner-permissions` dir (CORE_PACK slug) + a symlinked local skill both present; `update --all` skips both (prints "not a ClawHub skill"), does not delete them, and does not 404-fail on them.
- `update_all_keys_by_slug_dir` — a skill whose on-disk dir is `weather-dir` but manifest `name: Weather Reporter`; `update --all` targets the slug dir `weather-dir` (not the manifest name) and does not falsely report it "not installed locally".

Structural patterns to copy: `tests/onboard_skills_section.rs` (`with_home` at `:26`, `spawn_mock_clawhub_full` at `:288`, `sha256_hex` at `:375`, and the full-install test at `:382`).

Verification: the update test target → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib skills::` exits 0; the new `update_*` tests exist and pass (incl. `update_failure_preserves_old_dir`)
- [ ] The `Update` arm no longer calls `remove_dir_all(&dir)` *before* a successful `install_one` (grep confirms no delete-before-fetch on the live-skill dir)
- [ ] `--all` targets are derived from installed **slug directories** under `profile.skills_dir()`, not from `skill.name`
- [ ] Bundled/symlinked skills are explicitly skipped by `--all` with a "not a ClawHub skill" message
- [ ] No files outside the in-scope list modified (`git status`), except `plans/README.md` (and an optional new `tests/skills_update.rs`)
- [ ] `plans/README.md` status row for 035 updated

## STOP conditions

Stop and report back (do not improvise) if:

- The `Update` arm at `src/skills/mod.rs:1463-1523` does not match the excerpt above (drift).
- A non-destructive swap cannot be built without changing `install_one`'s signature or its skip-if-exists / partial-cleanup semantics in `src/skills/clawhub.rs` — report; do NOT modify `clawhub.rs`.
- Any test observes the original skill directory **deleted or altered** after a failed fetch — the core invariant is violated; stop and fix before proceeding.
- The temp/backup dir you introduce is itself picked up as a skill by the loader (`load_skills_from_directory`, `src/skills/mod.rs:348`) — choose a name the loader ignores (it only reads dirs containing `SKILL.md`/`SKILL.toml`; a dot-prefixed empty/tmp dir is safe) and confirm with a `skills list` assertion if in doubt.
- A test is flaky across two consecutive runs (env-lock scope; the update arm spawns its own runtime thread — ensure the env vars it reads via `ProfileManager::active()` are set before the spawn and held for the test's duration).

## Maintenance notes

- The origin inference (symlink ⇒ local; pack-slug ⇒ bundled) is a heuristic because skills carry no persisted origin marker. If a durable origin field is added to installed skills later, replace the heuristic with it and delete this inference. Note that decision in the PR so a future maintainer knows the heuristic is deliberate, not an oversight.
- A reviewer should verify the atomic-swap invariant directly by reading the failure path: on `install_one` `Err`, the original directory must be restored/left intact before the loop continues. This is the security/data-safety-relevant part.
- Keep the `--all` slug enumeration in sync with `load_workspace_skills` root (1) — both read `profile.skills_dir()`. If skill storage moves off `profile.skills_dir()`, revisit both.
- Deferred: teaching `update` to also refresh git/local-path skills (currently skipped by design per the help text). Out of scope here.
