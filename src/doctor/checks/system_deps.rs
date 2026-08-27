//! System-dependencies check — probes external binaries.

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext, Severity};

const REQUIRED: &[&str] = &["git", "curl", "tar"];

// macOS ships `shasum`, not GNU coreutils' `sha256sum`; probing the Linux name
// there reported the hash tool "missing" on a perfectly fine Mac.
#[cfg(target_os = "macos")]
const HASH_BIN: &str = "shasum";
#[cfg(not(target_os = "macos"))]
const HASH_BIN: &str = "sha256sum";

const RECOMMENDED: &[&str] = &[HASH_BIN, "docker", "cosign"];

pub struct SystemDepsCheck;

#[async_trait]
impl DoctorCheck for SystemDepsCheck {
    fn name(&self) -> &'static str {
        "system.deps"
    }
    fn category(&self) -> &'static str {
        "system"
    }
    async fn run(&self, _ctx: &DoctorContext) -> CheckResult {
        // PATH scans are blocking filesystem work; keep them off the async
        // runtime so a doctor poll cannot stall an SSE chat worker.
        let probe = tokio::task::spawn_blocking(|| probe_binaries(REQUIRED, RECOMMENDED))
            .await
            .map_err(|e| e.to_string());
        self.result_from_probe(probe)
    }
}

impl SystemDepsCheck {
    /// Map a probe outcome to a `CheckResult`. Split out from `run` so the
    /// JoinError path is unit-testable: a JoinError previously fell through
    /// `unwrap_or_default()` to an empty report — "all binaries present" — a
    /// vacuous green. A probe that did not complete is now a warn, not a pass.
    fn result_from_probe(&self, probe: Result<DepsReport, String>) -> CheckResult {
        let report = match probe {
            Ok(report) => report,
            Err(e) => {
                return CheckResult::warn(
                    self.name(),
                    format!("dependency probe did not complete: {e}"),
                )
                .with_category(self.category())
                .with_hint("re-run `rantaiclaw doctor`");
            }
        };

        if !report.required_missing.is_empty() {
            return CheckResult::fail(
                self.name(),
                format!(
                    "required binaries missing: {}",
                    report.required_missing.join(", ")
                ),
            )
            .with_category(self.category())
            .with_hint("install missing binaries via your OS package manager");
        }

        if !report.recommended_missing.is_empty() {
            return CheckResult {
                name: self.name().to_string(),
                severity: Severity::Warn,
                message: format!(
                    "recommended binaries missing: {}",
                    report.recommended_missing.join(", ")
                ),
                hint: Some(
                    "install for full functionality (docker → runtime, cosign → signed downloads)"
                        .to_string(),
                ),
                duration_ms: 0,
                category: self.category(),
            };
        }

        CheckResult::ok(
            self.name(),
            format!(
                "{} required + {} recommended binaries present",
                REQUIRED.len(),
                RECOMMENDED.len()
            ),
        )
        .with_category(self.category())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DepsReport {
    pub required_missing: Vec<String>,
    pub recommended_missing: Vec<String>,
}

pub fn probe_binaries(required: &[&str], recommended: &[&str]) -> DepsReport {
    let mut report = DepsReport::default();
    for bin in required {
        if which::which(bin).is_err() {
            report.required_missing.push((*bin).to_string());
        }
    }
    for bin in recommended {
        if which::which(bin).is_err() {
            report.recommended_missing.push((*bin).to_string());
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_empty_when_everything_present() {
        let r = probe_binaries(&["git"], &[]);
        assert!(r.required_missing.is_empty());
    }

    #[test]
    fn probe_lists_missing_required_binaries() {
        let r = probe_binaries(&["definitely-not-a-real-binary-xyz123"], &[]);
        assert_eq!(r.required_missing.len(), 1);
    }

    #[test]
    fn probe_separates_required_from_recommended() {
        let r = probe_binaries(
            &["definitely-not-a-real-binary-xyz123"],
            &["definitely-not-a-real-binary-xyz456"],
        );
        assert_eq!(r.required_missing.len(), 1);
        assert_eq!(r.recommended_missing.len(), 1);
    }

    #[test]
    fn join_error_warns_instead_of_reporting_ok() {
        // A failed probe task must NOT read as "all present".
        let r = SystemDepsCheck.result_from_probe(Err("task panicked".to_string()));
        assert_eq!(r.severity, Severity::Warn);
        assert!(r.message.contains("did not complete"), "{}", r.message);
    }

    #[test]
    fn empty_report_is_ok() {
        let r = SystemDepsCheck.result_from_probe(Ok(DepsReport::default()));
        assert_eq!(r.severity, Severity::Ok);
    }

    #[test]
    fn missing_required_is_fail() {
        let r = SystemDepsCheck.result_from_probe(Ok(DepsReport {
            required_missing: vec!["git".to_string()],
            recommended_missing: vec![],
        }));
        assert_eq!(r.severity, Severity::Fail);
    }

    #[test]
    fn hash_binary_is_platform_appropriate() {
        // macOS uses shasum; everything else sha256sum.
        assert!(RECOMMENDED.contains(&HASH_BIN));
        #[cfg(target_os = "macos")]
        assert_eq!(HASH_BIN, "shasum");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(HASH_BIN, "sha256sum");
    }
}
