//! Approval-policy checks — `command_allowlist.toml` and `approval_owners`.

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
    AllowlistDiagnosis::Healthy {
        count: entries.len(),
        has_wildcard,
    }
}

pub struct AllowlistCheck;

#[async_trait]
impl DoctorCheck for AllowlistCheck {
    fn name(&self) -> &'static str {
        "policy.allowlist"
    }
    fn category(&self) -> &'static str {
        "config"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let file = ctx
            .profile
            .root
            .join("policy")
            .join("command_allowlist.toml");
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
            AllowlistDiagnosis::Healthy {
                count: 2,
                has_wildcard: false
            }
        );
    }

    #[test]
    fn diagnose_flags_bare_wildcard() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("command_allowlist.toml");
        std::fs::write(&file, "commands = [\"*\"]\n").unwrap();
        assert_eq!(
            diagnose_allowlist(&file),
            AllowlistDiagnosis::Healthy {
                count: 1,
                has_wildcard: true
            }
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

// ────────────────────────────────────────────────────────────────────────
// Plan 450: `approval_owners` empty-list check.
//
// An empty `approval_owners` means no remote sender can ever promote
// themselves to owner — every chat is a guest, every approval is denied,
// and the operator ends up at the console to escape the loop. Surface this
// in `rantaiclaw doctor` instead of letting the warning in
// `channels::admin::warn_on_risky_approval_owners` be the only signal.
pub struct ApprovalOwnersCheck;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOwnersDiagnosis {
    NonEmpty { count: usize },
    Empty,
    Unparseable(String),
}

pub fn diagnose_approval_owners(raw: &str) -> ApprovalOwnersDiagnosis {
    // Empty or whitespace-only strings are "empty" — the warn-path treats
    // them the same as `[]`. A malformed TOML surface is informative, not
    // fatal: doctor is a tool that helps operators find issues, and a typo'd
    // `approval_owners` entry is exactly the kind of thing it should flag.
    if raw.trim().is_empty() {
        return ApprovalOwnersDiagnosis::Empty;
    }
    let parsed: toml::Table = match raw.parse() {
        Ok(v) => v,
        Err(e) => return ApprovalOwnersDiagnosis::Unparseable(e.to_string()),
    };
    let entries = parsed
        .get("approval_owners")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        return ApprovalOwnersDiagnosis::Empty;
    }
    ApprovalOwnersDiagnosis::NonEmpty {
        count: entries.len(),
    }
}

#[async_trait]
impl DoctorCheck for ApprovalOwnersCheck {
    fn name(&self) -> &'static str {
        "policy.approval_owners"
    }
    fn category(&self) -> &'static str {
        "config"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let file = ctx.profile.root.join("config.toml");
        let raw = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(_) => {
                // No config at all is a separate finding
                // (`config.schema`). Skip with `info` rather than fail here so
                // a fresh install does not get two warnings for one root
                // cause.
                return CheckResult::info(
                    self.name(),
                    "no config.toml to inspect (see config.schema check)",
                )
                .with_category(self.category());
            }
        };
        match diagnose_approval_owners(&raw) {
            ApprovalOwnersDiagnosis::Empty => CheckResult::warn(
                self.name(),
                "approval_owners is empty: no remote sender can ever approve shell commands",
            )
            .with_category(self.category())
            .with_hint("add at least one explicit sender id to [approval_owners] in config.toml"),
            ApprovalOwnersDiagnosis::NonEmpty { count } => {
                CheckResult::ok(self.name(), format!("approval_owners has {count} entries"))
                    .with_category(self.category())
            }
            ApprovalOwnersDiagnosis::Unparseable(e) => CheckResult::warn(
                self.name(),
                format!("config.toml is malformed around approval_owners: {e}"),
            )
            .with_category(self.category())
            .with_hint("fix the [approval_owners] section, see docs/reference/config.md"),
        }
    }
}

#[cfg(test)]
mod approval_owners_tests {
    use super::*;

    #[test]
    fn diagnose_empty_for_empty_string() {
        assert_eq!(diagnose_approval_owners(""), ApprovalOwnersDiagnosis::Empty);
    }

    #[test]
    fn diagnose_empty_for_whitespace_only() {
        assert_eq!(
            diagnose_approval_owners("   \n  \t\n"),
            ApprovalOwnersDiagnosis::Empty
        );
    }

    #[test]
    fn diagnose_empty_when_owners_array_missing() {
        assert_eq!(
            diagnose_approval_owners("[other]\nfoo = 1\n"),
            ApprovalOwnersDiagnosis::Empty
        );
    }

    #[test]
    fn diagnose_empty_when_owners_array_empty() {
        assert_eq!(
            diagnose_approval_owners("approval_owners = []\n"),
            ApprovalOwnersDiagnosis::Empty
        );
    }

    #[test]
    fn diagnose_non_empty_with_count() {
        assert_eq!(
            diagnose_approval_owners("approval_owners = [\"alice\", \"bob\"]\n"),
            ApprovalOwnersDiagnosis::NonEmpty { count: 2 }
        );
    }

    #[test]
    fn diagnose_unparseable_for_bad_toml() {
        assert!(matches!(
            diagnose_approval_owners("approval_owners = [unterminated string"),
            ApprovalOwnersDiagnosis::Unparseable(_)
        ));
    }
}
