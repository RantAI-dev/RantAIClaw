//! Daemon registration check — detects systemd/launchd registration.

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};

pub struct DaemonRegistrationCheck;

#[async_trait]
impl DoctorCheck for DaemonRegistrationCheck {
    fn name(&self) -> &'static str {
        "daemon.registration"
    }
    fn category(&self) -> &'static str {
        "system"
    }
    async fn run(&self, _ctx: &DoctorContext) -> CheckResult {
        let cat = self.category();
        match detect_registration() {
            DaemonState::Registered { backend } => {
                CheckResult::ok(self.name(), format!("daemon registered via {backend}"))
                    .with_category(cat)
            }
            DaemonState::Inactive { backend } => CheckResult::info(
                self.name(),
                format!("daemon registered with {backend} but not running"),
            )
            .with_category(cat)
            .with_hint("run: rantaiclaw service start"),
            DaemonState::NotRegistered { backend } => CheckResult::info(
                self.name(),
                format!("daemon not registered with {backend} (optional)"),
            )
            .with_category(cat)
            .with_hint("run: rantaiclaw service install"),
            DaemonState::Unsupported => {
                CheckResult::info(self.name(), "init system not detected (skipped)")
                    .with_category(cat)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonState {
    Registered {
        backend: &'static str,
    },
    /// Installed but not running. `systemctl status` returns exit 3 for this,
    /// which the check used to fold into `NotRegistered` — so a stopped-but-
    /// installed service was told to re-install, a no-op.
    Inactive {
        backend: &'static str,
    },
    NotRegistered {
        backend: &'static str,
    },
    Unsupported,
}

/// Map a `systemctl --user status` result to a [`DaemonState`].
///
/// Exit codes (verified empirically on systemd): `0` = unit active, `3` = unit
/// exists but is inactive/dead, `4` = no such unit. Folding 3 and 4 together —
/// as the old `Ok(_) => NotRegistered` arm did — told a stopped-but-installed
/// service to re-install itself, which does nothing. Any other non-zero code
/// (e.g. `1`/`2` for a failed unit that still exists) is treated as inactive
/// rather than absent: the unit is registered, just not healthy.
#[cfg(target_os = "linux")]
fn classify_systemctl(success: bool, code: Option<i32>) -> DaemonState {
    let backend = "systemd (user)";
    if success {
        DaemonState::Registered { backend }
    } else if code == Some(4) {
        DaemonState::NotRegistered { backend }
    } else {
        DaemonState::Inactive { backend }
    }
}

pub fn detect_registration() -> DaemonState {
    #[cfg(target_os = "linux")]
    {
        if which::which("systemctl").is_ok() {
            let out = std::process::Command::new("systemctl")
                .args(["--user", "status", "rantaiclaw"])
                .output();
            return match out {
                Ok(o) => classify_systemctl(o.status.success(), o.status.code()),
                Err(_) => DaemonState::Unsupported,
            };
        }
    }
    #[cfg(target_os = "macos")]
    {
        if which::which("launchctl").is_ok() {
            let out = std::process::Command::new("launchctl").arg("list").output();
            return match out {
                Ok(o) if o.status.success() => {
                    let s = String::from_utf8_lossy(&o.stdout);
                    if s.contains("rantaiclaw") {
                        DaemonState::Registered { backend: "launchd" }
                    } else {
                        DaemonState::NotRegistered { backend: "launchd" }
                    }
                }
                _ => DaemonState::Unsupported,
            };
        }
    }
    DaemonState::Unsupported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_registration_does_not_panic() {
        let _ = detect_registration();
    }

    /// Exit 3 (installed but stopped) and exit 4 (not installed) are distinct
    /// states — verified against real synthetic units — and must not both map
    /// to NotRegistered, which hints at a re-install that is a no-op.
    #[cfg(target_os = "linux")]
    #[test]
    fn systemctl_exit_codes_map_to_distinct_states() {
        let sd = "systemd (user)";
        assert_eq!(
            classify_systemctl(true, Some(0)),
            DaemonState::Registered { backend: sd },
            "active"
        );
        assert_eq!(
            classify_systemctl(false, Some(3)),
            DaemonState::Inactive { backend: sd },
            "installed but stopped"
        );
        assert_eq!(
            classify_systemctl(false, Some(4)),
            DaemonState::NotRegistered { backend: sd },
            "no such unit"
        );
        // A failed-but-present unit (1/2) is registered, not absent.
        assert_eq!(
            classify_systemctl(false, Some(1)),
            DaemonState::Inactive { backend: sd },
            "failed but present"
        );
        // A signal-killed probe with no code must not read as 'not installed'.
        assert_eq!(
            classify_systemctl(false, None),
            DaemonState::Inactive { backend: sd },
            "no exit code"
        );
    }

    #[tokio::test]
    async fn check_returns_a_category_of_system() {
        use crate::config::Config;
        use crate::profile::Profile;
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let ctx = DoctorContext {
            profile: Profile {
                name: "test".into(),
                root: tmp.path().to_path_buf(),
            },
            config: Config::default(),
            offline: false,
        };
        let r = DaemonRegistrationCheck.run(&ctx).await;
        assert_eq!(r.category, "system");
    }
}
