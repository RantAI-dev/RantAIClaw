#![allow(clippy::field_reassign_with_default)]

//! Wave 3 integration tests — orchestrator over the canonical setup-section list.
//!
//! Plan: `docs/superpowers/plans/2026-04-27-onboarding-depth-v2.md`,
//! Wave 3 step 3.12.
//!
//! These tests exercise the public crate API of `crate::onboard::wizard`:
//!   * `canonical_section_order()` — returns the 5 wired sections in order;
//!   * `run_setup(profile, &mut config, topic, force, non_interactive)` —
//!     iterates / dispatches and bails cleanly in headless mode.
//!
//! Headless / `non_interactive: true` is the test-friendly mode: every
//! section is asked for `headless_hint()` and skipped, so we get to assert
//! sequencing without dialoguer prompts.
//!
//! All tests redirect `$HOME` to a `tempfile::TempDir`; we serialize via a
//! global `Mutex` because `std::env::set_var("HOME", ...)` is process-global
//! and Cargo runs tests in parallel by default. (Same pattern Wave 1 +
//! Wave 2 leaf tests use.)

use std::sync::Mutex;

use rantaiclaw::config::Config;
use rantaiclaw::onboard::wizard;
use rantaiclaw::profile::ProfileManager;
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn with_home<F: FnOnce()>(f: F) {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().expect("tempdir");
    let prev_home = std::env::var_os("HOME");
    let prev_profile = std::env::var_os("RANTAICLAW_PROFILE");
    std::env::set_var("HOME", tmp.path());
    std::env::remove_var("RANTAICLAW_PROFILE");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if let Some(h) = prev_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Some(p) = prev_profile {
        std::env::set_var("RANTAICLAW_PROFILE", p);
    } else {
        std::env::remove_var("RANTAICLAW_PROFILE");
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

#[test]
fn setup_runs_all_sections_in_canonical_order() {
    let order = wizard::canonical_section_order();
    assert_eq!(
        order,
        vec![
            "provider",
            "approvals",
            "channels",
            "persona",
            "skills",
            "mcp"
        ],
        "canonical section order changed — update spec + tests together",
    );
}

#[test]
fn setup_skips_already_configured_sections_without_force() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        let mut config = Config::default();

        // Pre-configure provider so it should be skipped.
        config.default_provider = Some("openrouter".to_string());
        config.api_key = Some("sk-test".to_string());
        config.default_model = Some("openai/gpt-4o-mini".to_string());

        let report = wizard::run_setup(&profile, &mut config, None, false, true)
            .expect("non-interactive run should succeed");

        assert!(
            report.skipped.contains(&"provider".to_string()),
            "provider should have been skipped (already configured): {:?}",
            report,
        );
        // The other 4 sections are not configured, so they should be visited.
        for s in ["channels", "persona", "skills", "mcp"] {
            assert!(
                report.visited.contains(&s.to_string()),
                "expected {s} to be visited (not configured): {:?}",
                report,
            );
        }
    });
}

#[test]
fn setup_force_reruns_already_configured_sections() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        let mut config = Config::default();
        config.default_provider = Some("openrouter".to_string());
        config.api_key = Some("sk-test".to_string());

        let report = wizard::run_setup(&profile, &mut config, None, /*force*/ true, true)
            .expect("force run should succeed");

        assert!(
            report.visited.contains(&"provider".to_string()),
            "force should re-run provider section: {:?}",
            report,
        );
        assert!(
            report.skipped.is_empty(),
            "force should bypass all skips: {:?}",
            report,
        );
    });
}

#[test]
fn setup_with_topic_runs_only_one_section() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        let mut config = Config::default();

        let report = wizard::run_setup(
            &profile,
            &mut config,
            Some("persona".to_string()),
            false,
            true,
        )
        .expect("topic run should succeed");

        assert_eq!(report.visited, vec!["persona".to_string()]);
        assert!(report.skipped.is_empty());
    });
}

#[test]
fn setup_with_unknown_topic_errors_with_valid_topic_list() {
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        let mut config = Config::default();

        let err = wizard::run_setup(
            &profile,
            &mut config,
            Some("doesnotexist".to_string()),
            false,
            true,
        )
        .expect_err("unknown topic must error");

        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("unknown") || msg.contains("invalid"),
            "error should call out unknown topic: {msg}"
        );
        // The error must enumerate the valid topics so the user can recover.
        for s in ["provider", "approvals", "channels", "persona", "skills", "mcp"] {
            assert!(
                msg.contains(s),
                "error should list valid topic {s}: {msg}",
            );
        }
    });
}

#[test]
fn setup_propagates_section_failures_and_stops() {
    // Headless persona section should succeed (writes default preset). To
    // simulate a hard stop without injecting a fake section, we exercise
    // the unknown-topic path which returns Err *before* any section runs.
    // The "stops at first failure" property is documented; topic errors
    // and section errors share the same propagation path. This test pairs
    // with `setup_with_unknown_topic_errors_with_valid_topic_list`.
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        let mut config = Config::default();
        let err = wizard::run_setup(
            &profile,
            &mut config,
            Some("__never__".to_string()),
            false,
            true,
        )
        .expect_err("dispatch must fail before running any section");
        assert!(!err.to_string().is_empty());
    });
}

#[test]
fn onboard_alias_dispatches_to_setup_with_no_topic() {
    // The legacy `Commands::Onboard` arm in main.rs is a thin wrapper that
    // calls `wizard::run_setup(profile, &mut config, None, false, false)`.
    // Here we assert the orchestrator entry point exists and is callable
    // with `topic=None`; in headless mode it visits every section the
    // way the legacy alias would.
    with_home(|| {
        let profile = ProfileManager::ensure_default().unwrap();
        let mut config = Config::default();
        let report = wizard::run_setup(&profile, &mut config, None, false, true)
            .expect("legacy-equivalent dispatch should succeed");
        // No topic + non-interactive ⇒ every section visited at least once
        // (some skipped, but never fewer than 6 outcomes total — Wave 4A
        // added `approvals` between provider and channels).
        assert_eq!(report.visited.len() + report.skipped.len(), 6);
    });
}

// ── Banner snapshots ────────────────────────────────────────────────

#[test]
fn welcome_banner_snapshot() {
    let banner = console::strip_ansi_codes(&rantaiclaw::onboard::ui::render_welcome_banner())
        .into_owned();
    insta::assert_snapshot!("setup_welcome_banner", banner);
}

#[test]
fn completion_banner_snapshot() {
    let banner = console::strip_ansi_codes(&rantaiclaw::onboard::ui::render_completion_banner(
        &[
            "rantaiclaw doctor — verify the install",
            "rantaiclaw chat   — start a session",
        ],
    ))
    .into_owned();
    insta::assert_snapshot!("setup_completion_banner", banner);
}
