# Plan 040: Contain skill `download` recipes to a fixed base dir and verify raw downloads against a declared SHA-256

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 4736e2e..HEAD -- src/skills/install_deps.rs src/skills/mod.rs`
> If either in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

A skill recipe (`metadata.clawdbot.install[]` in a `SKILL.md`) is **untrusted
data** — it can come from ClawHub, `open-skills`, a git clone, or any file a
user drops into their skills dir. Today a `download` recipe controls exactly
where its payload lands: `run_download` takes `recipe.target_dir` **verbatim**
and, for the `raw`/no-archive branch, writes `target_dir.join(<last URL
segment>)` and `chmod 0o755`. Nothing anchors `target_dir` to a safe base and
nothing verifies the bytes. So a hostile recipe can set
`target_dir: "/home/<user>/.local/bin"` and `url: ".../git"` to drop an
executable named `git` earlier on `$PATH` than the real one — a PATH hijack.
The runtime itself shells out to `git` (and brew/npm/etc.), so this is direct
code execution on the operator's machine with no integrity check. This plan
constrains the write location to `<data>/tools/<slug>/` and lets a recipe
declare a `sha256` that raw downloads are verified against (fail-closed on
mismatch). Archive branches already reject traversal entries
(`validate_archive_entries`) but still extract into the attacker-chosen base —
this plan closes that too by anchoring the base.

## Current state

Files:

- `src/skills/install_deps.rs` — the recipe runner. `run_download` (lines
  319–380) is the vulnerable path. `validate_archive_entries` (468–481) already
  guards archive *entries* but not the base dir.
- `src/skills/mod.rs` — defines `SkillInstallRecipe` (65–104, has **no**
  checksum field) and parses `install[]` recipes from frontmatter
  (`parse_skill_metadata`, 674–782; the field-by-field copy is 707–753).

`run_download` today — note `target_dir` is used verbatim and the `raw` branch
writes + chmods with no hash check (`src/skills/install_deps.rs:319-372`):

```rust
fn run_download(recipe: &SkillInstallRecipe, slug: &str) -> Result<()> {
    let url = recipe
        .url
        .as_ref()
        .ok_or_else(|| anyhow!("download recipe missing `url`"))?;
    let target_dir = recipe
        .target_dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            directories::ProjectDirs::from("", "", "rantaiclaw")
                .map(|d| d.data_dir().join("tools").join(slug))
                .unwrap_or_else(|| PathBuf::from(format!(".rantaiclaw/tools/{slug}")))
        });
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("create target dir {}", target_dir.display()))?;

    println!("  · downloading {url}");
    let bytes = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_mins(2))
        .build()
        .context("build reqwest client")?
        .get(url)
        .send()
        .context("download GET")?
        .error_for_status()
        .context("download HTTP status")?
        .bytes()
        .context("download body")?;

    match recipe.archive.as_deref() {
        Some("tar.gz" | "tgz") => {
            extract_targz(&bytes, &target_dir, recipe.strip_components.unwrap_or(0))?;
        }
        Some("zip") => extract_zip(&bytes, &target_dir, recipe.strip_components.unwrap_or(0))?,
        Some("tar.bz2") => {
            bail!("tar.bz2 archives not yet supported")
        }
        Some("raw") | None => {
            // Plain binary — write to target_dir as the last URL segment
            // and `chmod +x`.
            let name = url.rsplit('/').next().unwrap_or("downloaded-bin");
            let dest = target_dir.join(name);
            std::fs::write(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&dest)?.permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&dest, perms)?;
            }
        }
        Some(other) => bail!("unsupported archive type `{other}`"),
    }
    // …prints target_dir + $PATH reminder…
}
```

`SkillInstallRecipe` today — **no** `sha256` field
(`src/skills/mod.rs:65-104`):

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillInstallRecipe {
    #[serde(default)] pub id: String,
    #[serde(default)] pub kind: String,
    #[serde(default)] pub bins: Vec<String>,
    #[serde(default)] pub label: String,
    #[serde(default)] pub os: Vec<String>,
    #[serde(default)] pub formula: Option<String>,
    #[serde(default)] pub pkg: Option<String>,
    #[serde(default)] pub module: Option<String>,
    #[serde(default)] pub url: Option<String>,
    #[serde(default)] pub archive: Option<String>,
    #[serde(default)] pub strip_components: Option<usize>,
    /// Target directory for download recipes; defaults to
    /// `~/.rantaiclaw/tools/<skill-slug>/`.
    #[serde(default)] pub target_dir: Option<String>,
}
```

The **exemplar** for the containment check is `sanitize_relative_path` in the
sibling module `src/skills/clawhub.rs:524-537` — reject absolute paths and any
component that isn't `Normal`/`CurDir`:

```rust
fn sanitize_relative_path(raw: &str) -> Result<std::path::PathBuf> {
    let path = std::path::Path::new(raw);
    if path.is_absolute() {
        anyhow::bail!("absolute path");
    }
    for comp in path.components() {
        match comp {
            std::path::Component::Normal(_) => {}
            std::path::Component::CurDir => {}
            _ => anyhow::bail!("forbidden component {comp:?}"),
        }
    }
    Ok(path.to_path_buf())
}
```

The **exemplar** for hash verification is `verify_sha256` in
`src/skills/clawhub.rs:540-548` (copy this shape into `install_deps.rs` as a
small local helper — a duplicated 8-line pure function is fine here per
CLAUDE.md §3.3, and keeps `install_deps` from depending on `clawhub` internals):

```rust
fn verify_sha256(body: &[u8], expected_hex: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(body);
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_hex) {
        anyhow::bail!("sha256 mismatch: got {actual}, expected {expected_hex}");
    }
    Ok(())
}
```

`sha2` and `hex` are already crate dependencies (used by `clawhub.rs`), so no
`Cargo.toml` change is required. Confirm with the drift check / a `cargo build`.

Repo security posture (CLAUDE.md §3.5/§3.6): fail-fast with explicit errors,
never silently broaden capability. Tightening `target_dir` is a *local
capability* hardening, not an exposure-boundary change, so no schema-version
bump is required — but the new `sha256` field IS a new config-contract key on a
user-facing struct; document it in the field doc-comment (the schema-drift gate
fingerprints defaults, and `Option<String>` defaults to `None`, so an
additive optional field is safe).

## Commands you will need

| Purpose        | Command                                                   | Expected on success   |
|----------------|----------------------------------------------------------|-----------------------|
| Build          | `cargo build`                                            | exit 0                |
| Format check   | `cargo fmt --all -- --check`                             | exit 0, no diff       |
| Lint (scoped)  | `cargo clippy --all-targets -- -D warnings`              | exit 0, no warnings   |
| Tests (scoped) | `cargo test --lib install_deps`                          | all pass, incl. new   |

Full `cargo test` is disk-heavy on this box — prefer `--lib` with a filter.
`strict-clippy-delta` and `setup_e2e` run POST-merge; run the scoped clippy
above locally before merge so a latent warning doesn't fail main.

## Scope

**In scope** (the only files you should modify):

- `src/skills/install_deps.rs` — add `resolve_target_dir` + local
  `verify_sha256`; call both in `run_download`; add unit tests.
- `src/skills/mod.rs` — add the optional `sha256` field to
  `SkillInstallRecipe` (65–104) and parse it in `parse_skill_metadata`
  (707–753, next to `strip_components`/`target_dir`).

**Out of scope** (do NOT touch):

- `src/skills/clawhub.rs` — its `verify_sha256`/`sanitize_relative_path` are the
  *pattern* to copy, not to re-export or edit. Don't add a cross-module
  dependency.
- `extract_targz` / `extract_zip` / `validate_archive_entries` internals — they
  already guard entries; you only change the *base dir* they receive (via
  `resolve_target_dir`) and add the pre-dispatch hash check.
- The recipe-selection logic (`pick_preferred*`), other recipe kinds
  (`run_brew`/`run_npm`/…). This plan is `download`-only.

## Git workflow

- Branch: `advisor/040-install-deps-download-hardening`
- Conventional commits, e.g.
  `fix(skills): contain download recipe target_dir and verify raw download sha256`
- **Do NOT add a `Co-Authored-By` trailer** (repo rule).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add the optional `sha256` field to `SkillInstallRecipe`

In `src/skills/mod.rs`, add to the struct (after `target_dir`, ~line 103):

```rust
    /// Optional hex-encoded SHA-256 for `kind = download`. When present,
    /// the downloaded bytes are verified against it before anything is
    /// written to disk; a mismatch fails the recipe. `None` = no
    /// integrity check (legacy behaviour, matches how recipes ship today).
    #[serde(default)]
    pub sha256: Option<String>,
```

In `parse_skill_metadata` (the `install[]` loop, next to the `target_dir`
assignment around line 750), add:

```rust
                            recipe.sha256 = entry
                                .get("sha256")
                                .and_then(|v| v.as_str())
                                .map(String::from);
```

**Verify**: `cargo build` → exit 0.

### Step 2: Add a pure `resolve_target_dir` helper and a local `verify_sha256`

In `src/skills/install_deps.rs`, add a pure, testable function that computes
the safe base and validates any recipe-supplied `target_dir` as a *relative*
subpath of it:

```rust
/// Resolve the directory a `download` recipe may write into.
///
/// The base is always `<data>/tools/<slug>/` (the historical default). A
/// recipe MAY narrow to a subdirectory by supplying a **relative**
/// `target_dir` with no parent (`..`) components; an absolute path or any
/// `..` is rejected so an untrusted recipe cannot escape the base (e.g.
/// dropping an executable into `~/.local/bin` to hijack `$PATH`).
fn resolve_target_dir(target_dir: Option<&str>, slug: &str) -> Result<PathBuf> {
    let base = directories::ProjectDirs::from("", "", "rantaiclaw")
        .map(|d| d.data_dir().join("tools").join(slug))
        .unwrap_or_else(|| PathBuf::from(format!(".rantaiclaw/tools/{slug}")));

    let Some(raw) = target_dir else {
        return Ok(base);
    };
    let rel = Path::new(raw);
    if rel.is_absolute() {
        bail!("download recipe target_dir must be relative, got absolute `{raw}`");
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("download recipe target_dir `{raw}` has a forbidden path component"),
        }
    }
    Ok(base.join(rel))
}
```

Add the local `verify_sha256` helper (copy the shape from
`clawhub.rs:540-548`, shown in "Current state"). `Component` and `Path` are
already imported at the top of the file (`use std::path::{Component, Path,
PathBuf};` at line 17).

**Verify**: `cargo build` → exit 0.

### Step 3: Wire both into `run_download`

In `run_download` (`src/skills/install_deps.rs:319-372`), replace the inline
`target_dir` computation with the helper, and add the hash check immediately
after the bytes are fetched and before the `match recipe.archive` dispatch:

```rust
    let target_dir = resolve_target_dir(recipe.target_dir.as_deref(), slug)?;
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("create target dir {}", target_dir.display()))?;

    println!("  · downloading {url}");
    let bytes = /* …unchanged reqwest fetch… */;

    if let Some(expected) = recipe.sha256.as_deref() {
        if !expected.is_empty() {
            verify_sha256(&bytes, expected)
                .with_context(|| format!("integrity check failed for {url}"))?;
        }
    }

    match recipe.archive.as_deref() { /* …unchanged… */ }
```

Verifying the fetched `bytes` **before** the archive/raw dispatch covers every
download kind uniformly (raw and archives) with a single check — simplest and
fail-closed.

**Verify**: `cargo build` → exit 0; `cargo fmt --all -- --check` → no diff.

### Step 4: Unit tests

Add to the `#[cfg(test)] mod tests` block at the bottom of
`src/skills/install_deps.rs` (model after the existing
`archive_entry_validation_rejects_escape_paths` test at lines 688–694):

```rust
#[test]
fn target_dir_absolute_is_rejected() {
    assert!(resolve_target_dir(Some("/home/x/.local/bin"), "demo").is_err());
    assert!(resolve_target_dir(Some("/etc/cron.d"), "demo").is_err());
}

#[test]
fn target_dir_parent_escape_is_rejected() {
    assert!(resolve_target_dir(Some("../../bin"), "demo").is_err());
    assert!(resolve_target_dir(Some("sub/../../escape"), "demo").is_err());
}

#[test]
fn target_dir_default_and_relative_subdir_are_allowed() {
    let base = resolve_target_dir(None, "demo").unwrap();
    assert!(base.ends_with("tools/demo") || base.ends_with("demo"));
    let sub = resolve_target_dir(Some("bin"), "demo").unwrap();
    assert!(sub.ends_with("demo/bin"));
}

#[test]
fn raw_download_sha256_mismatch_fails() {
    // Wrong hash for "abc" must be rejected.
    let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
    assert!(verify_sha256(b"abc", wrong).is_err());
}

#[test]
fn raw_download_sha256_match_succeeds() {
    // SHA-256 of "abc".
    let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    verify_sha256(b"abc", abc).unwrap();
}
```

These are pure and need no network — `run_download`'s network I/O stays
untested here by design (the security logic lives in the two extracted
helpers).

**Verify**: `cargo test --lib install_deps` → all pass, including the 5 new
tests.

## Test plan

- New tests (all in `src/skills/install_deps.rs`): `target_dir_absolute_is_rejected`,
  `target_dir_parent_escape_is_rejected`, `target_dir_default_and_relative_subdir_are_allowed`,
  `raw_download_sha256_mismatch_fails`, `raw_download_sha256_match_succeeds`.
- Structural pattern: the existing `archive_entry_validation_rejects_escape_paths`
  and `verify_sha256_*` tests (the latter live in `clawhub.rs` — same shape).
- Verification: `cargo test --lib install_deps` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib install_deps` passes, with the 5 new tests present
- [ ] `SkillInstallRecipe` has an `Option<String> sha256` field parsed in
      `parse_skill_metadata`
- [ ] `run_download` no longer uses `recipe.target_dir` verbatim — it goes
      through `resolve_target_dir`, which rejects absolute/`..` paths
- [ ] No files outside `src/skills/install_deps.rs` and `src/skills/mod.rs`
      are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- The `run_download` or `SkillInstallRecipe` excerpts in "Current state" don't
  match the live code (drift since `4736e2e`).
- `sha2`/`hex` are NOT already available (a `cargo build` after adding
  `verify_sha256` fails to resolve them) — do not add new dependencies without
  reporting.
- Adding the `resolve_target_dir` base changes the *default* install location
  for existing recipes (a recipe with no `target_dir` must still land in
  `<data>/tools/<slug>/` — the default path must be byte-identical to today's).
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- If a future recipe kind needs to write outside `<data>/tools/<slug>/`, do
  NOT loosen `resolve_target_dir` — add an explicit, separately-reviewed path.
- Reviewer should scrutinize: that the default (`target_dir = None`) path is
  unchanged; that the hash check is fail-closed (bail, not warn) on mismatch;
  and that `verify_sha256` is a local copy, not a new `install_deps → clawhub`
  dependency.
- Deferred out of scope: surfacing the concrete resolved `url` + target +
  package name in the interactive **approval prompt** shown before the recipe
  runs. Today `skills_install_deps` receives only `{name}` as tool args
  (`src/tools/skills_install.rs:159-181`), so the approval payload can't name
  the URL without restructuring how the recipe is resolved vs. approved. The
  runner already `println!`s the URL + target at run time
  (`install_deps.rs:336,374-378`); enriching the *pre-approval* payload is a
  larger UX change tracked separately.
