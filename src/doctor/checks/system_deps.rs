//! System-dependencies check — probes external binaries.

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext, Severity};

const REQUIRED: &[&str] = &["git", "curl", "tar"];
const RECOMMENDED: &[&str] = &["sha256sum", "docker", "cosign"];

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
        let report = probe_binaries(REQUIRED, RECOMMENDED);

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
}
