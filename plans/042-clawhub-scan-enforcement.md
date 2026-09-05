# Plan 042: Enforce the ClawHub security scan on install (fail-closed), require a hash for SKILL.md, and reject non-https base overrides

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 4736e2e..HEAD -- src/skills/clawhub.rs`
> If the in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S-M
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

ClawHub runs a security scan on every skill version and returns a verdict in
the version manifest (`version.security.status`, per-scanner verdicts,
`hasWarnings`). Today that verdict is **display-only**: `inspect_to_stdout`
renders it for `rantaiclaw skills inspect <slug>`, but the install path
(`install_one_inner`) resolves the version, fetches files, sanitizes paths, and
verifies per-file SHA-256 — and **never reads `security`**. So a skill ClawHub
flagged as malicious installs with no warning and no gate, both via
`rantaiclaw skills install <slug>` (CLI) and the chat `skills_install` tool. A
scan the platform already paid to run should be enforced, not just printed.

Two smaller hardenings in the same file: (1) per-file SHA-256 verification is
**skipped** whenever the manifest omits the hash (`if !file.sha256.is_empty()`),
so an empty-hash entry — including the *required* `SKILL.md`, the file whose
body becomes the agent prompt — is written unverified; (2) the
`RANTAICLAW_CLAWHUB_BASE_URL` override is documented "tests only" but is read in
production with no scheme check, so a stray/hostile env var can silently
redirect installs to a plain-http attacker server.

## Current state

File: `src/skills/clawhub.rs`.

The scan render in `inspect_to_stdout` shows exactly which fields exist
(`src/skills/clawhub.rs:284-310`) — reuse this reading shape in the install
gate:

```rust
    if let Some(security) = version.get("security") {
        let status = security.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        let warnings = security.get("hasWarnings").and_then(|v| v.as_bool()).unwrap_or(false);
        println!("  security:  {status}{}", if warnings { " (with warnings)" } else { "" });
        if let Some(scanners) = security.get("scanners") {
            for (name, body) in scanners.as_object().into_iter().flatten() {
                let s = body.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                let v = body.get("verdict").and_then(|v| v.as_str()).unwrap_or("");
                // …prints per-scanner…
            }
        }
    } else {
        println!("  security:  (no scan info available)");
    }
```

`install_one_inner` — parses the version manifest at 411–422 but never inspects
`version_body.get("version").get("security")`; the per-file hash skip is at
478–481 (`src/skills/clawhub.rs:409-484`, abridged):

```rust
    let version_resp = fetch_with_retry(client, &version_url).await?;
    let version_body: serde_json::Value = version_resp.json().await
        .context("parse clawhub version manifest")?;
    let files: Vec<VersionFile> = version_body
        .get("version").and_then(|v| v.get("files"))
        .map(|f| serde_json::from_value(f.clone())).transpose()
        .context("parse version.files")?.unwrap_or_default();
    // … [NO security-verdict check here] …
    for file in &files {
        // … fetch bytes …
        let is_required = file.path.eq_ignore_ascii_case("SKILL.md");
        // …
        if !file.sha256.is_empty() {           // ← hash skipped when empty
            verify_sha256(&bytes, &file.sha256)
                .with_context(|| format!("hash check failed for {}", file.path))?;
        }
        std::fs::write(&target, &bytes)?;
    }
    if !dir.join("SKILL.md").exists() {
        anyhow::bail!("clawhub: version {version} of {slug} has no SKILL.md");
    }
```

The base-URL override (`src/skills/clawhub.rs:22-24, 70-72`):

```rust
pub const CLAWHUB_BASE_URL_ENV: &str = "RANTAICLAW_CLAWHUB_BASE_URL";
const DEFAULT_BASE_URL: &str = "https://clawhub.ai/api/v1";
// …
fn base_url() -> String {
    std::env::var(CLAWHUB_BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}
```

`install_one` is the single choke point every surface goes through — CLI
(`src/skills/mod.rs:1318`), chat tool (`src/tools/skills_install.rs:88`), TUI
(`src/tui/app.rs:913`), onboarding (`install_many` → `install_one`,
`clawhub.rs:330`). So enforcing inside `install_one`/`install_one_inner` covers
all of them with no signature change.

The `verify_sha256` helper already exists (`clawhub.rs:540-548`). The crate env
lock for tests that mutate env vars is `crate::test_env::ENV_LOCK`
(sync: `.blocking_lock()`, async: `.lock().await`; see `src/test_env.rs`).

Repo security posture (CLAUDE.md §3.5/§3.6): fail-fast with explicit errors;
never silently broaden capability. Refusing a flagged install is the
fail-closed default. **The CLI override (a `--allow-flagged` escape hatch) is a
product-policy decision and is deliberately deferred** — see Maintenance notes —
so this plan's required scope stays fail-closed and single-file.

## Commands you will need

| Purpose        | Command                                        | Expected on success |
|----------------|------------------------------------------------|---------------------|
| Build          | `cargo build`                                  | exit 0              |
| Format check   | `cargo fmt --all -- --check`                   | exit 0, no diff     |
| Lint           | `cargo clippy --all-targets -- -D warnings`    | exit 0, no warnings |
| Tests (scoped) | `cargo test --lib clawhub`                     | all pass, incl. new |

Full `cargo test` is disk-heavy — prefer `--lib` with a filter.
`strict-clippy-delta`/`setup_e2e` run POST-merge; run scoped clippy before merge.

## Scope

**In scope** (the only files you should modify):

- `src/skills/clawhub.rs` — add pure helpers (`scan_status`, `is_blocking_status`,
  `ensure_scan_allows`, `ensure_required_hash`, `ensure_base_url_safe`), wire
  them into `install_one_inner` / `install_one`, and add unit tests.

**Out of scope** (do NOT touch):

- `install_one`'s signature and its call sites (`src/skills/mod.rs`,
  `src/tools/skills_install.rs`, `src/tui/app.rs`). Enforcement goes *inside*
  the function; do not add an `allow_flagged` parameter in this plan (that is
  the deferred CLI override).
- `inspect_to_stdout` — leave the display path as-is (you may factor the
  status-reading into the shared `scan_status` helper if it's a clean reuse, but
  it is not required).
- `list_top` / `search` — read-only catalog calls; the base-url guard is added
  to the *install* entry only, per the finding.

## Git workflow

- Branch: `advisor/042-clawhub-scan-enforcement`
- Conventional commits, e.g.
  `fix(skills): enforce clawhub security verdict on install and require SKILL.md hash`
- **Do NOT add a `Co-Authored-By` trailer** (repo rule).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add pure, testable helpers

Add to `src/skills/clawhub.rs` (private fns near `verify_sha256`):

```rust
/// Read `(status, has_warnings)` out of a manifest's `version.security` block.
/// Returns `("", false)` when no scan info is present (unknown, not blocking).
fn scan_status(version: &serde_json::Value) -> (String, bool) {
    match version.get("security") {
        Some(sec) => {
            let status = sec.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let warnings = sec.get("hasWarnings").and_then(|v| v.as_bool()).unwrap_or(false);
            (status, warnings)
        }
        None => (String::new(), false),
    }
}

/// A `fail` verdict blocks the install. Unknown / `pass` / empty do not.
fn is_blocking_status(status: &str) -> bool {
    status.eq_ignore_ascii_case("fail")
}

/// Fail-closed gate: bail on a blocking scan verdict; warn (non-fatal) when the
/// scan reports warnings. `version` is the manifest's `version` object.
fn ensure_scan_allows(slug: &str, version: &serde_json::Value) -> Result<()> {
    let (status, warnings) = scan_status(version);
    if is_blocking_status(&status) {
        anyhow::bail!(
            "clawhub: refusing to install `{slug}` — security scan verdict is `{status}`. \
             Vet it with `rantaiclaw skills inspect {slug}` before installing."
        );
    }
    if warnings {
        tracing::warn!(slug, status = status.as_str(),
            "clawhub: skill has security-scan warnings; installing anyway");
    }
    Ok(())
}

/// Required files (SKILL.md) must ship a verifiable hash. Optional files may
/// omit it (best-effort, unchanged).
fn ensure_required_hash(is_required: bool, sha256: &str) -> Result<()> {
    if is_required && sha256.is_empty() {
        anyhow::bail!("clawhub: required file has no sha256 in the manifest — refusing to write it unverified");
    }
    Ok(())
}

/// Reject a `RANTAICLAW_CLAWHUB_BASE_URL` override that is not https, unless it
/// targets loopback (127.0.0.1 / [::1] / localhost) — loopback is how the mock
/// server tests point the client locally and carries no MITM exposure.
fn ensure_base_url_safe() -> Result<()> {
    let Ok(url) = std::env::var(CLAWHUB_BASE_URL_ENV) else { return Ok(()); };
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    let is_loopback = lower.starts_with("http://127.0.0.1")
        || lower.starts_with("http://[::1]")
        || lower.starts_with("http://localhost");
    if is_loopback {
        return Ok(());
    }
    anyhow::bail!(
        "refusing insecure {CLAWHUB_BASE_URL_ENV}=`{url}` — clawhub installs require https (loopback http allowed for tests)"
    );
}
```

**Verify**: `cargo build` → exit 0 (helpers may be unused until Step 2 — that's
fine, `cargo build` won't fail; `clippy` would flag dead code, so proceed to
Step 2 before running clippy).

### Step 2: Wire the guards into the install path

In `install_one` (`src/skills/clawhub.rs:365`), call `ensure_base_url_safe()?`
right after `validate_slug(slug)?` and before building the client.

In `install_one_inner`, after `version_body` is parsed (~line 422) and before
the file loop, add:

```rust
    let version_obj = version_body.get("version").unwrap_or(&version_body);
    ensure_scan_allows(slug, version_obj)?;
```

Inside the file loop, after `is_required` is computed and before the existing
`if !file.sha256.is_empty()` block, add:

```rust
        ensure_required_hash(is_required, &file.sha256)?;
```

Leave the existing `if !file.sha256.is_empty() { verify_sha256(...) }` as-is
(optional files still best-effort; the required file now can't reach it with an
empty hash because `ensure_required_hash` bails first).

**Verify**: `cargo build` → exit 0; `cargo clippy --all-targets -- -D warnings`
→ no warnings (helpers now used).

### Step 3: Unit tests (pure — no mock server needed)

Add to `src/skills/clawhub.rs` `#[cfg(test)]` (model after the existing
`verify_sha256_*` and `sanitize_relative_path_*` tests):

```rust
#[test]
fn scan_fail_verdict_blocks_install() {
    let v = serde_json::json!({ "security": { "status": "fail", "hasWarnings": true } });
    assert!(is_blocking_status(&scan_status(&v).0));
    assert!(ensure_scan_allows("evil-skill", &v).is_err());
}

#[test]
fn scan_pass_and_warnings_do_not_block() {
    let pass = serde_json::json!({ "security": { "status": "pass" } });
    assert!(ensure_scan_allows("ok", &pass).is_ok());
    let warn = serde_json::json!({ "security": { "status": "pass", "hasWarnings": true } });
    assert!(ensure_scan_allows("ok", &warn).is_ok()); // warns, does not fail
    let none = serde_json::json!({});
    assert!(ensure_scan_allows("ok", &none).is_ok()); // no scan info: not blocking
}

#[test]
fn required_file_without_hash_is_rejected() {
    assert!(ensure_required_hash(true, "").is_err());   // SKILL.md, empty hash
    assert!(ensure_required_hash(true, "abc123").is_ok());
    assert!(ensure_required_hash(false, "").is_ok());   // optional file, ok
}

#[test]
fn non_https_non_loopback_base_url_is_rejected() {
    let _guard = crate::test_env::ENV_LOCK.blocking_lock();
    let prev = std::env::var(CLAWHUB_BASE_URL_ENV).ok();
    std::env::set_var(CLAWHUB_BASE_URL_ENV, "http://evil.example/api/v1");
    assert!(ensure_base_url_safe().is_err());
    std::env::set_var(CLAWHUB_BASE_URL_ENV, "http://127.0.0.1:8080/api/v1");
    assert!(ensure_base_url_safe().is_ok());     // loopback allowed for tests
    std::env::set_var(CLAWHUB_BASE_URL_ENV, "https://clawhub.ai/api/v1");
    assert!(ensure_base_url_safe().is_ok());
    // restore
    match prev {
        Some(v) => std::env::set_var(CLAWHUB_BASE_URL_ENV, v),
        None => std::env::remove_var(CLAWHUB_BASE_URL_ENV),
    }
}
```

The env-mutating test MUST hold `crate::test_env::ENV_LOCK` — other tests read
the same var (see `list_top`'s mock tests). Restore the previous value on exit.

**Verify**: `cargo test --lib clawhub` → all pass, including the 4 new tests.

### Step 4 (recommended, optional): end-to-end mock test

If you want a full-path assertion, extend a mock server modeled on
`spawn_mock_clawhub` in `tests/onboard_skills_section.rs:147` to serve
`GET /skills/:slug`, `GET /skills/:slug/versions/:v` (with
`version.security.status = "fail"`), point `RANTAICLAW_CLAWHUB_BASE_URL` at it
(loopback), and assert `install_one` returns `Err`. This is **not required** —
the Step 3 unit tests already prove the gate — but it is the structural pattern
if a reviewer asks for it. Keep it in a `tests/` integration file, not in the
lib.

## Test plan

- `src/skills/clawhub.rs` unit tests: `scan_fail_verdict_blocks_install`,
  `scan_pass_and_warnings_do_not_block`, `required_file_without_hash_is_rejected`,
  `non_https_non_loopback_base_url_is_rejected`.
- Structural pattern: existing `verify_sha256_*` tests (pure) and, if doing the
  optional Step 4, `spawn_mock_clawhub` in `tests/onboard_skills_section.rs`.
- Verification: `cargo test --lib clawhub` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib clawhub` passes, with the 4 new tests present
- [ ] `install_one` calls `ensure_base_url_safe()?`; `install_one_inner` calls
      `ensure_scan_allows(...)` before the file loop and `ensure_required_hash(...)`
      inside it (grep: `grep -n "ensure_scan_allows\|ensure_required_hash\|ensure_base_url_safe" src/skills/clawhub.rs` → 6 hits: 3 defs + 3 call sites)
- [ ] No files outside `src/skills/clawhub.rs` are modified (`git status`) —
      unless you added the optional `tests/` mock file, which is allowed
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- Any "Current state" excerpt doesn't match the live code (drift since `4736e2e`).
- `crate::test_env::ENV_LOCK` doesn't exist or isn't reachable from
  `clawhub.rs` tests — report; do not skip the lock (env-mutating tests without
  it flake under parallel `cargo test`).
- The manifest shape differs — if `version.security.status` isn't where the
  `inspect_to_stdout` excerpt reads it, stop and report the actual shape rather
  than guessing which key means "malicious".
- Enforcing the gate breaks an *existing* passing test that installs a real/mock
  skill without a scan verdict (that would mean the default isn't
  "unknown = allowed").

## Maintenance notes

- **Deferred CLI override (product decision).** Some operators will want to
  install a flagged skill they've personally vetted. The clean shape: add
  `pub async fn install_one_forced(profile, slug)` that calls the same inner
  path with the scan gate skipped, wired to a `rantaiclaw skills install
  --allow-flagged` flag ONLY. Do **not** expose the override to the chat
  `skills_install` tool — an injected prompt must never be able to force a
  flagged install. Keep the default (`install_one`, and every non-CLI caller)
  fail-closed. This was left out of the required scope to keep the change
  single-file and to force an explicit product decision.
- Reviewer should scrutinize: fail-closed default (bail, not warn) on `fail`;
  loopback carve-out is narrow (exact `127.0.0.1`/`[::1]`/`localhost` prefixes,
  not a substring match anywhere in the URL); required-file hash check can't be
  bypassed by an empty string.
- If ClawHub changes the verdict vocabulary (e.g. adds `quarantined`), revisit
  `is_blocking_status`.
