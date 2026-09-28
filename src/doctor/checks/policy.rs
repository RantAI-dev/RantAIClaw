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
// `approval_owners` empty-list check.
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
}

/// Classify an already-loaded `[channels_config].approval_owners` list.
/// Entries that are empty or whitespace-only count as no owner at all — the
/// warn-path treats them the same as `[]`.
pub fn diagnose_approval_owners(owners: &[String]) -> ApprovalOwnersDiagnosis {
    if owners.iter().all(|o| o.trim().is_empty()) {
        ApprovalOwnersDiagnosis::Empty
    } else {
        ApprovalOwnersDiagnosis::NonEmpty {
            count: owners.len(),
        }
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
        // Nothing can reach the agent as a remote sender yet, so there is
        // nothing an owner list would gate — matches the notion of
        // "configured" the channel catalog and daemon startup already use.
        if !crate::channels::any_catalog_channel_configured(&ctx.config) {
            return CheckResult::info(
                self.name(),
                "no channel is configured yet (see the channels check)",
            )
            .with_category(self.category());
        }
        match diagnose_approval_owners(&ctx.config.channels_config.approval_owners) {
            ApprovalOwnersDiagnosis::Empty => {
                CheckResult::warn(self.name(), crate::approval::APPROVAL_OWNERS_EMPTY_MESSAGE)
                    .with_category(self.category())
                    .with_hint(crate::approval::APPROVAL_OWNERS_EMPTY_HINT)
            }
            ApprovalOwnersDiagnosis::NonEmpty { count } => {
                CheckResult::ok(self.name(), format!("approval_owners has {count} entries"))
                    .with_category(self.category())
            }
        }
    }
}

#[cfg(test)]
mod approval_owners_tests {
    use super::*;
    use crate::config::Config;
    use crate::doctor::Severity;

    fn config_with_telegram(owners: Vec<String>) -> Config {
        let mut config = Config::default();
        config.channels_config.telegram = Some(crate::config::TelegramConfig {
            bot_token: "abc:123".into(),
            allowed_users: vec![],
            stream_mode: crate::config::StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
        });
        config.channels_config.approval_owners = owners;
        config
    }

    fn run_check(config: Config) -> CheckResult {
        let ctx = DoctorContext {
            profile: crate::profile::Profile {
                name: "test".to_string(),
                root: std::path::PathBuf::from("/tmp"),
            },
            config,
            offline: true,
        };
        futures::executor::block_on(ApprovalOwnersCheck.run(&ctx))
    }

    #[test]
    fn diagnose_empty_for_no_owners() {
        assert_eq!(
            diagnose_approval_owners(&[]),
            ApprovalOwnersDiagnosis::Empty
        );
    }

    #[test]
    fn diagnose_empty_for_whitespace_only_owners() {
        assert_eq!(
            diagnose_approval_owners(&["   ".to_string(), "\t\n".to_string()]),
            ApprovalOwnersDiagnosis::Empty
        );
    }

    #[test]
    fn diagnose_non_empty_with_count() {
        assert_eq!(
            diagnose_approval_owners(&["alice".to_string(), "bob".to_string()]),
            ApprovalOwnersDiagnosis::NonEmpty { count: 2 }
        );
    }

    /// Pins the bug this check used to have: it parsed a top-level
    /// `approval_owners` out of raw TOML instead of the real
    /// `[channels_config].approval_owners`, so a configured owner list was
    /// never seen and `rantaiclaw doctor` warned even when owners were set.
    #[test]
    fn run_reports_ok_when_channels_config_approval_owners_is_set() {
        let result = run_check(config_with_telegram(vec!["alice".to_string()]));
        assert_eq!(result.severity, Severity::Ok, "{}", result.message);
        assert!(result.message.contains("1 entries"), "{}", result.message);
    }

    #[test]
    fn run_warns_with_the_shared_message_when_a_channel_is_configured_and_owners_is_empty() {
        let result = run_check(config_with_telegram(vec![]));
        assert_eq!(result.severity, Severity::Warn, "{}", result.message);
        assert_eq!(
            result.message,
            crate::approval::APPROVAL_OWNERS_EMPTY_MESSAGE
        );
        let hint = result.hint.expect("warn should carry a hint");
        assert_eq!(hint, crate::approval::APPROVAL_OWNERS_EMPTY_HINT);
    }

    #[test]
    fn run_skips_when_no_channel_is_configured() {
        let result = run_check(Config::default());
        assert_eq!(result.severity, Severity::Info, "{}", result.message);
    }
}
