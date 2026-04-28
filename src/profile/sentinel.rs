//! `.daemon_active` sentinel file — per-profile "is the daemon currently
//! running for this profile?" detector.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Profile-aware path resolution" and the Wave 4B handoff task.
//!
//! Lifecycle:
//! - The daemon writes the sentinel on bind (see `daemon::run`) with its
//!   PID and the unit name (best-effort).
//! - The daemon clears it on graceful shutdown.
//! - `profile::use_profile` reads it through `is_daemon_active` to decide
//!   whether a handoff (`systemctl restart …`) is needed.
//!
//! The sentinel is *advisory*: a stale file from a crashed daemon should
//! never block profile switching. Callers therefore tolerate parse errors
//! and treat the file's mere presence as the binary signal.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::paths;

const SENTINEL_FILE: &str = ".daemon_active";

/// Contents of `<profile>/.daemon_active`. Intentionally tiny — keep stable
/// across versions; readers tolerate unknown extra fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonSentinel {
    pub pid: u32,
    /// Service unit name (`rantaiclaw@<profile>.service`) when registered
    /// via systemd / launchd; `None` when running in-foreground.
    #[serde(default)]
    pub unit: Option<String>,
    /// ISO-8601 timestamp the daemon bound. Free-form for humans.
    #[serde(default)]
    pub started_at: Option<String>,
}

/// `<profile>/.daemon_active` path.
pub fn sentinel_path(profile: &str) -> PathBuf {
    paths::profile_dir(profile).join(SENTINEL_FILE)
}

/// Write the sentinel atomically (tempfile + rename in same dir).
pub fn write_sentinel(profile: &str, sentinel: &DaemonSentinel) -> Result<()> {
    let path = sentinel_path(profile);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create profile dir {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(sentinel).context("serialize sentinel")?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Remove the sentinel. Idempotent — missing file is not an error.
pub fn clear_sentinel(profile: &str) -> Result<()> {
    let path = sentinel_path(profile);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// Read the sentinel. Returns `Ok(None)` when the file is absent. Parse
/// errors are surfaced so callers can log + treat as "stale".
pub fn read_sentinel(profile: &str) -> Result<Option<DaemonSentinel>> {
    let path = sentinel_path(profile);
    match fs::read_to_string(&path) {
        Ok(body) => {
            let sentinel: DaemonSentinel =
                toml::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
            Ok(Some(sentinel))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Binary "is something registered for this profile?" — parse failures
/// still count as active (the file's presence alone is the signal).
pub fn is_daemon_active(profile: &str) -> bool {
    sentinel_path(profile).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
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

    #[test]
    fn write_then_read_roundtrips() {
        with_home(|| {
            super::super::ProfileManager::ensure("alpha").unwrap();
            let s = DaemonSentinel {
                pid: 4242,
                unit: Some("rantaiclaw@alpha.service".into()),
                started_at: Some("2026-04-28T00:00:00Z".into()),
            };
            write_sentinel("alpha", &s).unwrap();
            assert!(is_daemon_active("alpha"));
            let got = read_sentinel("alpha").unwrap().unwrap();
            assert_eq!(got.pid, 4242);
            assert_eq!(got.unit.as_deref(), Some("rantaiclaw@alpha.service"));
        });
    }

    #[test]
    fn clear_is_idempotent() {
        with_home(|| {
            super::super::ProfileManager::ensure("alpha").unwrap();
            // No file yet — must not error.
            clear_sentinel("alpha").unwrap();
            // Write then clear.
            write_sentinel(
                "alpha",
                &DaemonSentinel {
                    pid: 1,
                    unit: None,
                    started_at: None,
                },
            )
            .unwrap();
            assert!(is_daemon_active("alpha"));
            clear_sentinel("alpha").unwrap();
            assert!(!is_daemon_active("alpha"));
            // Second clear — still ok.
            clear_sentinel("alpha").unwrap();
        });
    }

    #[test]
    fn read_missing_returns_none() {
        with_home(|| {
            super::super::ProfileManager::ensure("alpha").unwrap();
            assert!(read_sentinel("alpha").unwrap().is_none());
            assert!(!is_daemon_active("alpha"));
        });
    }
}
