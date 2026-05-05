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
    Registered { backend: &'static str },
    NotRegistered { backend: &'static str },
    Unsupported,
}

pub fn detect_registration() -> DaemonState {
    #[cfg(target_os = "linux")]
    {
        if which::which("systemctl").is_ok() {
            let out = std::process::Command::new("systemctl")
                .args(["--user", "status", "rantaiclaw"])
                .output();
            return match out {
                Ok(o) if o.status.success() => DaemonState::Registered {
                    backend: "systemd (user)",
                },
                Ok(_) => DaemonState::NotRegistered {
                    backend: "systemd (user)",
                },
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
