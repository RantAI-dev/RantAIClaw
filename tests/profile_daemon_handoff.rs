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
use rantaiclaw::daemon::handoff::{restart_daemon_for_profile_with, DaemonControl};
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
    last_unit: Mutex<Option<String>>,
}

impl Recorder {
    fn new(name: &str) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            restarts: AtomicUsize::new(0),
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
        // The restart is now gated on the unit being active, so the recorder
        // reports active — these tests exercise the restart path itself.
        Ok(true)
    }
}

#[test]
fn handoff_no_op_when_no_sentinel() {
    with_home(|| {
        ProfileManager::ensure("alpha").unwrap();
        let rec = Recorder::new("stub");
        restart_daemon_for_profile_with("alpha", rec.as_ref()).unwrap();
        assert_eq!(
            rec.restarts.load(Ordering::SeqCst),
            0,
            "no sentinel ⇒ no restart attempt",
        );
    });
}

#[test]
fn handoff_restarts_when_sentinel_present() {
    with_home(|| {
        ProfileManager::ensure("work").unwrap();
        // A LIVE daemon pid — the handoff now skips (and clears) a stale
        // sentinel, so use the current process, which the liveness guard treats
        // as a live RantaiClaw process.
        write_sentinel(
            "work",
            &DaemonSentinel {
                pid: std::process::id(),
                unit: Some("rantaiclaw@work.service".into()),
                started_at: None,
            },
        )
        .unwrap();
        let rec = Recorder::new("stub");
        restart_daemon_for_profile_with("work", rec.as_ref()).unwrap();
        assert_eq!(rec.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(
            rec.last_unit.lock().unwrap().as_deref(),
            Some("rantaiclaw@work.service"),
        );
    });
}

#[test]
fn handoff_falls_back_to_default_unit_name() {
    with_home(|| {
        ProfileManager::ensure("staging").unwrap();
        // Sentinel without a unit — handoff must synthesize the default. Use a
        // live pid so the sentinel is not treated as stale.
        write_sentinel(
            "staging",
            &DaemonSentinel {
                pid: std::process::id(),
                unit: None,
                started_at: None,
            },
        )
        .unwrap();
        let rec = Recorder::new("stub");
        restart_daemon_for_profile_with("staging", rec.as_ref()).unwrap();
        assert_eq!(rec.restarts.load(Ordering::SeqCst), 1);
        assert_eq!(
            rec.last_unit.lock().unwrap().as_deref(),
            Some("rantaiclaw@staging.service"),
        );
    });
}

#[test]
fn handoff_skips_and_clears_a_stale_sentinel() {
    use rantaiclaw::profile::sentinel::is_daemon_active;
    with_home(|| {
        ProfileManager::ensure("gone").unwrap();
        // A pid that is effectively never alive — the daemon crashed / was
        // stopped. Handoff must NOT restart, and must clear the stale sentinel.
        write_sentinel(
            "gone",
            &DaemonSentinel {
                pid: 2_147_483_646,
                unit: Some("rantaiclaw.service".into()),
                started_at: None,
            },
        )
        .unwrap();
        let rec = Recorder::new("stub");
        restart_daemon_for_profile_with("gone", rec.as_ref()).unwrap();
        assert_eq!(rec.restarts.load(Ordering::SeqCst), 0, "stale ⇒ no restart");
        assert!(!is_daemon_active("gone"), "stale sentinel must be cleared");
    });
}

#[test]
fn handoff_no_init_skips_with_friendly_message() {
    // The `NoInit` impl returns Ok(()) without touching anything, even if
    // the sentinel is present — profile switching must not block on
    // missing init systems.
    use rantaiclaw::daemon::handoff::NoInit;
    with_home(|| {
        ProfileManager::ensure("ci").unwrap();
        // Live pid so we exercise the NoInit branch rather than the stale-skip.
        write_sentinel(
            "ci",
            &DaemonSentinel {
                pid: std::process::id(),
                unit: None,
                started_at: None,
            },
        )
        .unwrap();
        // Should not error.
        restart_daemon_for_profile_with("ci", &NoInit).unwrap();
    });
}
