//! Approval-policy check — validates `command_allowlist.toml`.

use async_trait::async_trait;
use std::path::Path;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowlistDiagnosis {
    Healthy { count: usize, has_wildcard: bool },
    Empty,
    Missing,
    Malformed(String),
}

pub fn diagnose_allowlist(file: &Path) -> AllowlistDiagnosis {
    if !file.exists() {
        return AllowlistDiagnosis::Missing;
    }
    let raw = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => return AllowlistDiagnosis::Malformed(e.to_string()),
    };
    let parsed: toml::Table = match raw.parse() {
        Ok(v) => v,
        Err(e) => return AllowlistDiagnosis::Malformed(e.to_string()),
    };
    let entries = parsed
        .get("commands")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        return AllowlistDiagnosis::Empty;
    }
    let has_wildcard = entries.iter().any(|v| v.as_str() == Some("*"));
    AllowlistDiagnosis::Healthy { count: entries.len(), has_wildcard }
}

pub struct AllowlistCheck;

#[async_trait]
impl DoctorCheck for AllowlistCheck {
    fn name(&self) -> &'static str { "policy.allowlist" }
    fn category(&self) -> &'static str { "config" }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let file = ctx.profile.root.join("policy").join("command_allowlist.toml");
        let diag = diagnose_allowlist(&file);
        let strict_like = matches!(
            ctx.config.autonomy.level,
            crate::security::AutonomyLevel::ReadOnly | crate::security::AutonomyLevel::Supervised
        );

        match diag {
            AllowlistDiagnosis::Healthy { count, has_wildcard } => {
                if has_wildcard {
                    CheckResult::warn(
                        self.name(),
                        format!("allowlist has {count} entries but includes a bare \"*\""),
                    )
                    .with_category(self.category())
                    .with_hint("replace \"*\" with explicit command globs")
                } else {
                    CheckResult::ok(self.name(), format!("allowlist healthy ({count} entries)"))
                        .with_category(self.category())
                }
            }
            AllowlistDiagnosis::Empty if strict_like => CheckResult::warn(
                self.name(),
                "strict-like autonomy mode with empty allowlist — every tool call will require approval",
            )
            .with_category(self.category())
            .with_hint("run: rantaiclaw setup approvals"),
            AllowlistDiagnosis::Empty => CheckResult::info(
                self.name(),
                "allowlist is empty (autonomy mode is permissive)",
            )
            .with_category(self.category()),
            AllowlistDiagnosis::Missing => CheckResult::info(
                self.name(),
                "no command_allowlist.toml yet (will be created on first approval)",
            )
            .with_category(self.category()),
            AllowlistDiagnosis::Malformed(e) => CheckResult::fail(
                self.name(),
                format!("command_allowlist.toml is malformed: {e}"),
            )
            .with_category(self.category())
            .with_hint("delete or fix the file then re-run doctor"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn diagnose_returns_missing_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("command_allowlist.toml");
        assert_eq!(diagnose_allowlist(&file), AllowlistDiagnosis::Missing);
    }

    #[test]
    fn diagnose_returns_empty_for_zero_entries() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("command_allowlist.toml");
        std::fs::write(&file, "commands = []\n").unwrap();
        assert_eq!(diagnose_allowlist(&file), AllowlistDiagnosis::Empty);
    }

    #[test]
    fn diagnose_returns_healthy_with_count() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("command_allowlist.toml");
        std::fs::write(&file, "commands = [\"git status\", \"ls -la\"]\n").unwrap();
        assert_eq!(
            diagnose_allowlist(&file),
            AllowlistDiagnosis::Healthy { count: 2, has_wildcard: false }
        );
    }

    #[test]
    fn diagnose_flags_bare_wildcard() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("command_allowlist.toml");
        std::fs::write(&file, "commands = [\"*\"]\n").unwrap();
        assert_eq!(
            diagnose_allowlist(&file),
            AllowlistDiagnosis::Healthy { count: 1, has_wildcard: true }
        );
    }

    #[test]
    fn diagnose_returns_malformed_for_bad_toml() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("command_allowlist.toml");
        std::fs::write(&file, "this is { not toml = ").unwrap();
        match diagnose_allowlist(&file) {
            AllowlistDiagnosis::Malformed(_) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
