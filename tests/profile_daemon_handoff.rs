//! Wave 4B integration test — `restart_daemon_for_profile` flow with a
//! recording stub init system. Exercises the public seam
//! `daemon::handoff::restart_daemon_for_profile_with` so we don't depend on
//! the host actually having systemd / launchd installed.
//!
//! HOME is redirected to a tempdir per test; the same `Mutex` pattern Wave 1 +
//! Wave 2 + Wave 3 tests use serializes against `set_var` racing.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rantaiclaw::daemon::handoff::{restart_daemon_for_profile_with, DaemonControl, HandoffOutcome};
use rantaiclaw::profile::sentinel::{write_sentinel, DaemonSentinel};
use rantaiclaw::profile::ProfileManager;
use tempfile::TempDir;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn with_home<F: FnOnce()>(f: F) {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", tmp.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    if let Some(h) = prev {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

struct Recorder {
    name: String,
    restarts: AtomicUsize,
    is_active_calls: AtomicUsize,
    last_unit: Mutex<Option<String>>,
}

impl Recorder {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            restarts: AtomicUsize::new(0),
            is_active_calls: AtomicUsize::new(0),
            last_unit: Mutex::new(None),
        })
    }
}

impl DaemonControl for Recorder {
    fn name(&self) -> &str {
        &self.name
    }
    fn restart(&self, unit: &str) -> Result<()> {
        self.restarts.fetch_add(1, Ordering::SeqCst);
        *self.last_unit.lock().unwrap() = Some(unit.to_string());
        Ok(())
    }
    fn is_active(&self, _unit: &str) -> Result<bool> {
        self.is_active_calls.fetch_add(1, Ordering::SeqCst);
        // The restart is gated on the unit being active, so the recorder
        // reports active — these tests exercise the restart path itself.
        Ok(true)
    }
}

fn write_sentinel_for(profile: &str, pid: u32, unit: Option<&str>, pinned: bool) {
    ProfileManager::ensure(profile).unwrap();
    write_sentinel(
        profile,
        &DaemonSentinel {
            pid,
            unit: unit.map(str::to_string),
            started_at: None,
            profile_pinned: pinned,
        },
    )
    .unwrap();
}

#[test]
fn handoff_no_op_when_no_sentinel() {
    with_home(|| {
        ProfileManager::ensure("alpha").unwrap();
        let rec = Recorder::new("stub");
        let outcome = restart_daemon_for_profile_with("alpha", rec.as_ref()).unwrap();
        assert_eq!(outcome, HandoffOutcome::NotRunning);
        assert_eq!(
            rec.restarts.load(Ordering::SeqCst),
            0,
            "no sentinel ⇒ no restart attempt",
        );
    });
}

#[test]
fn handoff_restarts_the_recorded_unit_of_a_following_daemon() {
    with_home(|| {
        // A LIVE daemon pid — the handoff skips (and clears) a stale
        // sentinel, so use the current process, which the liveness guard
        // treats as a live RantaiClaw process. `profile_pinned = false`: the
        // daemon follows the active-profile marker, which is the only kind
        // of daemon a `profile use` may restart.
        write_sentinel_for(
            "work",
            std::process::id(),
            Some("rantaiclaw.service"),
            false,
        );
        let rec = Recorder::new("stub");
        let outcome = restart_daemon_for_profile_with("work", rec.as_ref()).unwrap();
        assert_eq!(
            outcome,
            HandoffOutcome::Restarted {
                unit: "rantaiclaw.service".to_string()
            }
        );
        assert_eq!(rec.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(
            rec.last_unit.lock().unwrap().as_deref(),
            Some("rantaiclaw.service"),
        );
    });
}

#[test]
fn handoff_leaves_a_pinned_daemon_alone() {
    with_home(|| {
        // A daemon whose launcher pinned its profile: a restart cannot move
        // it, so the handoff reports `Pinned` and touches nothing.
        write_sentinel_for("work", std::process::id(), Some("rantaiclaw.service"), true);
        let rec = Recorder::new("stub");
        let outcome = restart_daemon_for_profile_with("work", rec.as_ref()).unwrap();
        assert_eq!(
            outcome,
            HandoffOutcome::Pinned {
                profile: "work".to_string(),
                unit: Some("rantaiclaw.service".to_string())
            }
        );
        assert_eq!(
            rec.restarts.load(Ordering::SeqCst),
            0,
            "a pinned daemon must not be restarted"
        );
        assert_eq!(
            rec.is_active_calls.load(Ordering::SeqCst),
            0,
            "a pinned daemon must not reach the init system at all"
        );
    });
}

#[test]
fn handoff_does_not_restart_a_daemon_without_a_recorded_unit() {
    with_home(|| {
        // A sentinel without a unit is a hand-started daemon. The old
        // `rantaiclaw@staging.service` fallback cycled a template unit the
        // installer never creates; the handoff must report `HandStarted`
        // instead and contact no init system.
        write_sentinel_for("staging", std::process::id(), None, false);
        let rec = Recorder::new("stub");
        let outcome = restart_daemon_for_profile_with("staging", rec.as_ref()).unwrap();
        assert_eq!(
            outcome,
            HandoffOutcome::HandStarted {
                profile: "staging".to_string()
            }
        );
        assert_eq!(rec.restarts.load(Ordering::SeqCst), 0);
        assert_eq!(rec.is_active_calls.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn handoff_skips_and_clears_a_stale_sentinel() {
    use rantaiclaw::profile::sentinel::is_daemon_active;
    with_home(|| {
        // A pid that is effectively never alive — the daemon crashed / was
        // stopped. Handoff must NOT restart, and must clear the stale sentinel.
        write_sentinel_for("gone", 2_147_483_646, Some("rantaiclaw.service"), false);
        let rec = Recorder::new("stub");
        let outcome = restart_daemon_for_profile_with("gone", rec.as_ref()).unwrap();
        assert_eq!(outcome, HandoffOutcome::NotRunning);
        assert_eq!(rec.restarts.load(Ordering::SeqCst), 0, "stale ⇒ no restart");
        assert!(!is_daemon_active("gone"), "stale sentinel must be cleared");
    });
}

#[test]
fn handoff_without_an_init_system_reports_noinit_and_does_not_error() {
    // The `NoInit` impl returns Ok(()) without touching anything, even if
    // the sentinel is present — profile switching must not block on
    // missing init systems.
    use rantaiclaw::daemon::handoff::NoInit;
    with_home(|| {
        ProfileManager::ensure("ci").unwrap();
        // Live pid and a recorded, following unit so we exercise the NoInit
        // branch rather than the stale-skip or the hand-started one.
        write_sentinel_for("ci", std::process::id(), Some("rantaiclaw.service"), false);
        let outcome = restart_daemon_for_profile_with("ci", &NoInit).unwrap();
        assert_eq!(
            outcome,
            HandoffOutcome::NoInit {
                profile: "ci".to_string(),
                pid: std::process::id(),
            }
        );
    });
}
