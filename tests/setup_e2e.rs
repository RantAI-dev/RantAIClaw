//! Wave 5 — end-to-end integration smoke tests for `rantaiclaw setup`.
//!
//! These tests drive the compiled `rantaiclaw` binary through `assert_cmd`
//! with a freshly-minted `$HOME` so we exercise the same code path a real
//! user hits the very first time they install the binary. They double as
//! the release-readiness gate: if either headless smoke fails, v0.5.0 is
//! not shippable.
//!
//! Headless mode (`--non-interactive`) is the only branch we test from
//! the CLI surface; the interactive prompts are covered by lower-level
//! section unit tests (`tests/onboard_*_section.rs`) and by the
//! orchestrator dispatch tests in `tests/setup_orchestration.rs`.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//!       §"End-to-end smoke" + §"Acceptance criteria".

use std::sync::Mutex;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// `assert_cmd::Command` mutates `HOME` per-call (not process-global), but
/// we still serialize because some sections walk `$HOME/.rantaiclaw` and
/// concurrent runs against the same temp dir would race. Each test owns
/// its own `TempDir`; the lock just guards the binary build cache.
static CMD_LOCK: Mutex<()> = Mutex::new(());

fn cmd(home: &TempDir) -> Command {
    let mut c = Command::cargo_bin("rantaiclaw").expect("cargo build rantaiclaw");
    c.env("HOME", home.path())
        // Force a clean profile resolve — the wizard promotes `default`
        // automatically when `RANTAICLAW_PROFILE` is unset.
        .env_remove("RANTAICLAW_PROFILE")
        // Avoid pulling whatever the developer configured for their own
        // shell into the test binary.
        .env_remove("RANTAICLAW_HOME");
    c
}

#[test]
fn setup_non_interactive_visits_all_sections_and_exits_zero() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    let assert = cmd(&home)
        .args(["setup", "--non-interactive"])
        .assert()
        .success();

    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");

    // The summary line is the canonical machine-readable signal that
    // every section was dispatched. `visited` ∪ `skipped` must equal the
    // canonical six-section list (provider, approvals, channels,
    // persona, skills, mcp).
    assert!(
        combined.contains("setup: visited=") && combined.contains("skipped="),
        "expected single-line summary; got:\n{combined}"
    );
    for section in [
        "provider",
        "approvals",
        "channels",
        "persona",
        "skills",
        "mcp",
    ] {
        assert!(
            combined.contains(section),
            "summary should reference section {section}; got:\n{combined}"
        );
    }

    // Each headless section emits a hint pointing the user at the
    // interactive entry point. We assert on a representative substring
    // from each, which gives us a load-bearing "every section ran"
    // signal without locking the test to exact wording.
    let hints = [
        // provider
        ("provider", "rantaiclaw onboard"),
        // approvals
        ("approvals", "rantaiclaw setup approvals"),
        // channels
        ("channels", "rantaiclaw channel"),
        // skills (starter pack auto-installs in headless)
        ("skills", "starter pack"),
        // mcp
        ("mcp", "rantaiclaw mcp add"),
    ];
    for (label, needle) in hints {
        assert!(
            combined.contains(needle),
            "expected {label} headless hint substring `{needle}`; got:\n{combined}"
        );
    }
}

#[test]
fn setup_non_interactive_then_doctor_brief_runs_clean() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    cmd(&home)
        .args(["setup", "--non-interactive"])
        .assert()
        .success();

    // After a headless setup, no provider has been configured, no
    // allowlist has been chosen, and no daemon is registered. `doctor
    // --brief` must run cleanly (exit 0) and surface those gaps with
    // actionable hints, not panic on missing config keys.
    let assert = cmd(&home).args(["doctor", "--brief"]).assert().success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        combined.contains("RantaiClaw Doctor"),
        "doctor --brief must emit its banner; got:\n{combined}"
    );
    // The L1-L4 approval policy was never picked, so the allowlist
    // check should fire (warn or info) and point at `setup approvals`.
    assert!(
        combined.contains("rantaiclaw setup approvals"),
        "doctor --brief should hint at `setup approvals` after a fresh \
         non-interactive setup; got:\n{combined}"
    );
    assert!(
        combined.contains("Summary:"),
        "doctor --brief should print a summary line; got:\n{combined}"
    );
}

#[test]
fn setup_force_topic_persona_writes_persona_toml() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    cmd(&home)
        .args(["setup", "--non-interactive", "persona"])
        .assert()
        .success();

    // Headless persona section must materialise the default preset
    // template under the active profile so chat sessions have a
    // SYSTEM.md to render from.
    let persona_toml = home
        .path()
        .join(".rantaiclaw/profiles/default/persona/persona.toml");
    assert!(
        persona_toml.exists(),
        "persona.toml should exist at {}; tree: {:?}",
        persona_toml.display(),
        std::fs::read_dir(home.path().join(".rantaiclaw/profiles/default"))
            .ok()
            .map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>()),
    );
    let body = std::fs::read_to_string(&persona_toml).expect("persona.toml readable");
    assert!(!body.trim().is_empty(), "persona.toml must not be empty");
}

#[test]
fn setup_unknown_topic_errors_and_lists_valid_topics() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    let assert = cmd(&home)
        .args(["setup", "--non-interactive", "doesnotexist"])
        .assert()
        .failure();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");
    for needle in ["provider", "approvals", "mcp"] {
        assert!(
            combined.contains(needle),
            "error should list valid topic `{needle}`; got:\n{combined}"
        );
    }
}

#[test]
fn migrate_help_shows_from_flag_with_openclaw_zeroclaw_auto() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    cmd(&home)
        .args(["migrate", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--from")
                .and(predicate::str::contains("openclaw"))
                .and(predicate::str::contains("zeroclaw"))
                .and(predicate::str::contains("auto")),
        );
}

#[test]
fn version_reports_v0_5_0() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    cmd(&home)
        .args(["--version"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0.5.0"));
}
