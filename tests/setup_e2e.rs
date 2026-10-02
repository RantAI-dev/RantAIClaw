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
        .env_remove("RANTAICLAW_HOME")
        // An override inherited from the parent shell would shadow the
        // default resolution under the temp HOME.
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_WORKSPACE");
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
fn setup_non_interactive_then_doctor_brief_reports_the_written_allowlist() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    cmd(&home)
        .args(["setup", "--non-interactive"])
        .assert()
        .success();

    // After a headless setup, no provider has been configured and no
    // daemon is registered. `doctor --brief` must run cleanly (exit 0)
    // and surface those gaps with actionable hints, not panic on missing
    // config keys. The approvals section's headless path applies the
    // Smart preset through the real writer, so the doctor check reads
    // `[command_allowlist].patterns` from the file the writer wrote and
    // reports the written bundle as healthy. A `setup approvals` hint
    // would only fire if the file were missing or malformed, never on a
    // successful headless setup.
    let assert = cmd(&home).args(["doctor", "--brief"]).assert().success();

    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        combined.contains("RantaiClaw Doctor"),
        "doctor --brief must emit its banner; got:\n{combined}"
    );
    // The Smart bundle the writer lays down on a fresh headless install
    // has 63 pre-approved patterns; the allowlist check must report that
    // exact count from the file on disk, not fall back to the old
    // top-level `commands`-array parse that misreported every install as
    // empty.
    assert!(
        combined
            .contains("command_allowlist.toml reports 63 pre-approved patterns shown to the model"),
        "doctor --brief should report the written Smart bundle's 63 patterns; got:\n{combined}"
    );
    assert!(
        !combined.contains("strict-like autonomy mode with empty allowlist"),
        "doctor --brief must not replay the pre-fix empty-allowlist warning on a healthy headless install; got:\n{combined}"
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

/// Materialise `config.toml` so a later run can be compared against it.
fn baseline_config(home: &TempDir) -> (std::path::PathBuf, Vec<u8>) {
    // Persona is the one headless topic that genuinely configures something,
    // so it both materialises `config.toml` and proves the success path still
    // saves.
    cmd(home)
        .args(["setup", "--non-interactive", "persona"])
        .assert()
        .success();
    let path = home.path().join(".rantaiclaw/profiles/default/config.toml");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "config.toml should exist at {} after a successful headless setup: {e}",
            path.display()
        )
    });
    (path, bytes)
}

/// `setup <topic> --non-interactive` is what installers and CI run. A
/// provisioner that aborted for a missing field used to print "nothing saved"
/// and then exit zero **with the config saved anyway** — an installer could not
/// tell success from failure by exit code.
#[test]
fn headless_setup_that_configures_nothing_fails_and_writes_nothing() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");
    let (config_path, before) = baseline_config(&home);

    // No bot token is reachable in an unattended run, so this cannot succeed.
    cmd(&home)
        .args(["setup", "--non-interactive", "telegram"])
        .assert()
        .failure();

    let after = std::fs::read(&config_path).expect("config.toml still readable");
    assert_eq!(
        before, after,
        "a headless run that configured nothing must leave config.toml byte-identical"
    );
}

/// The headless driver answered every choice with option 0. For MCP that was
/// "install all zero-auth servers", so a documented no-op instead registered
/// subprocess-spawning servers into the operator's config.
#[test]
fn headless_mcp_setup_registers_no_servers() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");
    let (config_path, _) = baseline_config(&home);

    // Exit code is not asserted here — this test is about what reaches disk.
    let _ = cmd(&home)
        .args(["setup", "--non-interactive", "mcp"])
        .assert();

    let body = std::fs::read_to_string(&config_path).expect("config.toml readable");
    assert!(
        !body.contains("[mcp_servers."),
        "an unattended run must not register MCP servers; config.toml:\n{body}"
    );
}

/// Plan 296 made a headless run exit non-zero when a provisioner ABORTS. The
/// provider section slipped through: it treats a selection with no key as a
/// completed selection, so the run reported success, saved the config, and
/// exited zero — for an install that cannot send a single message.
#[test]
fn headless_provider_setup_without_a_key_anywhere_fails_and_writes_nothing() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");
    let (config_path, before) = baseline_config(&home);

    let assert = cmd(&home)
        // Strip every credential the resolver would otherwise find on the
        // developer's own machine — the point is "no key anywhere".
        .env_remove("RANTAICLAW_API_KEY")
        .env_remove("API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .args(["setup", "--non-interactive", "provider"])
        .assert()
        .failure();

    let output = assert.get_output();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("no API key"),
        "the refusal must say what is missing; got:\n{combined}"
    );
    assert!(
        combined.contains("RANTAICLAW_API_KEY"),
        "the refusal must name a variable that would satisfy it; got:\n{combined}"
    );

    let after = std::fs::read(&config_path).expect("config.toml still readable");
    assert_eq!(
        before, after,
        "a provider run that configured nothing usable must leave config.toml byte-identical"
    );
}

/// The other half, and the one that stops this becoming a false positive: a key
/// supplied through the environment is a credential the agent WILL use at send
/// time, so the same run must succeed.
#[test]
fn headless_provider_setup_succeeds_when_the_key_comes_from_the_environment() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");
    let (config_path, _) = baseline_config(&home);

    cmd(&home)
        .env("RANTAICLAW_API_KEY", "neutral-env-supplied-key")
        .args(["setup", "--non-interactive", "provider"])
        .assert()
        .success();

    let body = std::fs::read_to_string(&config_path).expect("config.toml readable");
    assert!(
        body.contains("default_provider"),
        "a usable provider run must persist its selection:\n{body}"
    );
    assert!(
        !body.contains("neutral-env-supplied-key"),
        "an env-supplied credential must not be baked into config.toml:\n{body}"
    );
}

/// A locked channel's provisioner exists and compiles, so it must be refused
/// by name rather than falling through to "unknown topic" or, worse, running
/// and saving a channel section the catalog does not commit to.
#[test]
fn setup_a_locked_channel_fails_and_writes_nothing() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");
    let (config_path, before) = baseline_config(&home);

    let assert = cmd(&home)
        .args(["setup", "--non-interactive", "irc"])
        .assert()
        .failure();
    let output = assert.get_output();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("under development"),
        "must name why it was refused: {stderr}"
    );

    let after = std::fs::read(&config_path).expect("config.toml still readable");
    assert_eq!(
        before, after,
        "a refused locked-channel setup must leave config.toml byte-identical"
    );
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

/// `rantaiclaw migrate` was removed. The CLI no longer accepts the subcommand,
/// so clap exits non-zero and surfaces an "unrecognized subcommand" error. This
/// pins the removal so a future PR cannot quietly reintroduce the command.
#[test]
fn migrate_subcommand_is_removed_and_fails_as_unknown() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    let assert = cmd(&home).arg("migrate").assert().failure().stderr(
        predicate::str::contains("migrate").and(
            predicate::str::contains("unrecognized subcommand")
                .or(predicate::str::contains("unknown subcommand"))
                .or(predicate::str::contains("unexpected argument")),
        ),
    );

    // The exit status is non-zero, captured above by `.failure()`. Confirm
    // the status code clap chooses for an unknown subcommand — historically 2
    // (usage error), as opposed to a runtime failure that would be 1. This
    // catches a future change that wires `migrate` back as a real command
    // but routes it through a runtime panic (exit 101).
    let output = assert.get_output();
    let exit_code = output.status.code();
    assert_eq!(
        exit_code,
        Some(2),
        "clap usage errors should exit with status 2; got {exit_code:?}"
    );
}

/// `rantaiclaw hardware` was removed in plan 506. The CLI no longer accepts
/// the subcommand, so clap exits non-zero and surfaces an "unrecognized
/// subcommand" error. This pins the removal so a future PR cannot quietly
/// reintroduce the command.
#[test]
fn hardware_subcommand_is_removed_and_fails_as_unknown() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    let assert = cmd(&home).arg("hardware").assert().failure().stderr(
        predicate::str::contains("hardware").and(
            predicate::str::contains("unrecognized subcommand")
                .or(predicate::str::contains("unknown subcommand"))
                .or(predicate::str::contains("unexpected argument")),
        ),
    );

    let output = assert.get_output();
    let exit_code = output.status.code();
    assert_eq!(
        exit_code,
        Some(2),
        "clap usage errors should exit with status 2; got {exit_code:?}"
    );
}

/// `rantaiclaw peripheral` was removed in plan 506. The CLI no longer accepts
/// the subcommand, so clap exits non-zero and surfaces an "unrecognized
/// subcommand" error. This pins the removal so a future PR cannot quietly
/// reintroduce the command.
#[test]
fn peripheral_subcommand_is_removed_and_fails_as_unknown() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    let assert = cmd(&home).arg("peripheral").assert().failure().stderr(
        predicate::str::contains("peripheral").and(
            predicate::str::contains("unrecognized subcommand")
                .or(predicate::str::contains("unknown subcommand"))
                .or(predicate::str::contains("unexpected argument")),
        ),
    );

    let output = assert.get_output();
    let exit_code = output.status.code();
    assert_eq!(
        exit_code,
        Some(2),
        "clap usage errors should exit with status 2; got {exit_code:?}"
    );
}

#[test]
fn version_reports_package_version() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    // Assert against the crate version rather than a hardcoded string so the
    // test tracks every release bump (it was pinned to 0.5.0 and had been red
    // since the 0.6.x line).
    cmd(&home)
        .args(["--version"])
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

/// A debug or test build must never ask the operator's service manager whether
/// a daemon is registered. Two fake `systemctl` programs disagree on the
/// answer ("active" vs. "inactive") but the rendered `daemon.registration`
/// line in `doctor --brief` is identical for both — the dev-build guard short-
/// circuits `detect_registration` before either fake is consulted. Without the
/// guard, the "active" fake would fold into `Registered` and the line would
/// differ. The fake scripts only ever answer what their first positional
/// argument asks; nothing the binary can spawn reaches the host's real
/// `systemctl` because PATH points at a tempdir.
#[test]
fn doctor_brief_dev_build_skips_the_service_manager_regardless_of_fake_state() {
    let _guard = CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = TempDir::new().expect("tempdir");

    // One fake says the unit is active; the other says it is not. The real
    // systemctl is never reached because PATH is overridden on each call.
    let fake_active = write_fake_systemctl(b"#!/bin/sh\necho active\nexit 0\n");
    let fake_inactive = write_fake_systemctl(b"#!/bin/sh\necho inactive\nexit 3\n");

    // A headless setup is required so the doctor's other checks don't fail on
    // missing config; the test does not depend on what setup writes.
    cmd(&home)
        .args(["setup", "--non-interactive"])
        .assert()
        .success();

    let run_with_fake = |fake_dir: &std::path::Path| {
        let prepended = prepend_path(fake_dir);
        let assert = cmd(&home)
            .env("PATH", &prepended)
            .args(["doctor", "--brief"])
            .assert()
            .success();
        let out = assert.get_output();
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        combined
    };

    let out_active = run_with_fake(fake_active.path());
    let out_inactive = run_with_fake(fake_inactive.path());

    let daemon_line = |combined: &str| -> String {
        combined
            .lines()
            .find(|l| l.contains("daemon.registration"))
            .unwrap_or_else(|| {
                panic!("doctor --brief must print a daemon.registration line; got:\n{combined}")
            })
            .to_string()
    };

    let line_active = daemon_line(&out_active);
    let line_inactive = daemon_line(&out_inactive);
    assert_eq!(
        line_active, line_inactive,
        "doctor --brief must print the same daemon.registration line regardless of \
         what a fake systemctl reports. With the dev-build guard both runs are \
         identical; without it the 'active' fake would fold into 'Registered' \
         and the line would differ. active={line_active:?}\ninactive={line_inactive:?}"
    );
    assert!(
        line_active.contains("development build does not talk to the service manager"),
        "the dev-build daemon.registration line must name the build mode; got: {line_active}"
    );
}

/// Write a fake `systemctl` shell script into a fresh tempdir and chmod it
/// executable. The script is pathologically simple on purpose — the binary
/// under test never reaches it under the dev-build guard, so it just needs to
/// exit 0 if it ever is invoked (so a regression that removes the guard
/// produces a diagnosable result rather than a hung subprocess).
fn write_fake_systemctl(body: &[u8]) -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::Builder::new()
        .prefix("rantaiclaw-fake-svcmgr-e2e-")
        .tempdir()
        .expect("fake svcmgr tempdir");
    let path = dir.path().join("systemctl");
    std::fs::write(&path, body).expect("write fake systemctl");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake systemctl");
    dir
}

/// Build a `PATH` value with `dir` placed first, preserving the rest of the
/// existing PATH so cargo-built binaries and shared libraries still resolve.
fn prepend_path(dir: &std::path::Path) -> String {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut parts: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
    for p in std::env::split_paths(&existing) {
        parts.push(p);
    }
    std::env::join_paths(parts)
        .expect("join PATH")
        .into_string()
        .expect("PATH is UTF-8")
}
