//! Approval-policy checks — `command_allowlist.toml` and `approval_owners`.

use async_trait::async_trait;
use std::path::Path;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowlistDiagnosis {
    Healthy { count: usize },
    Empty,
    Missing,
    Malformed(String),
}

/// Read the `<profile>/policy/command_allowlist.toml` file and classify it.
///
/// The file is written by `crate::approval::policy_writer::write_policy_files`
/// with `[command_allowlist].patterns = [...]` — that shape, not a top-level
/// `commands` array, is what the agent prompt and the on-disk reality use.
/// Reading the wrong key is the doctor-bug this whole module exists to
/// keep fixed; tests build every fixture with the real writer.
pub fn diagnose_allowlist(file: &Path) -> AllowlistDiagnosis {
    if !file.exists() {
        return AllowlistDiagnosis::Missing;
    }
    let raw = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => return AllowlistDiagnosis::Malformed(e.to_string()),
    };
    let table: toml::Table = match raw.parse() {
        Ok(v) => v,
        Err(e) => return AllowlistDiagnosis::Malformed(e.to_string()),
    };
    let entries = table
        .get("command_allowlist")
        .and_then(toml::Value::as_table)
        .and_then(|t| t.get("patterns"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        return AllowlistDiagnosis::Empty;
    }
    AllowlistDiagnosis::Healthy {
        count: entries.len(),
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
        let file_diag = diagnose_allowlist(&file);
        let level = ctx.config.autonomy.level;
        let allowed_commands = &ctx.config.autonomy.allowed_commands;

        // File malformed wins everything: the model prompt renders nothing
        // (agent.rs falls back to `unwrap_or_default()`), so the user must
        // see and fix it.
        if let AllowlistDiagnosis::Malformed(ref err) = file_diag {
            return CheckResult::fail(
                self.name(),
                format!("command_allowlist.toml is malformed: {err}"),
            )
            .with_category(self.category())
            .with_hint("delete or fix the file then re-run doctor");
        }

        // Gate warning — the source the shell `is_command_allowed` actually
        // reads is `config.autonomy.allowed_commands`. The bare-`*` warning
        // the old check emitted was about a key the gate does NOT treat as
        // allow-all (`a == base_cmd` is an exact match), so dropping the
        // warning also keeps the operator from chasing a fake fix. Under
        // `ReadOnly` the gate denies every shell command regardless, and
        // under `Full` it allows them — so neither level needs a heads-up.
        if level == crate::security::AutonomyLevel::Supervised && allowed_commands.is_empty() {
            return CheckResult::warn(
                self.name(),
                "[autonomy].allowed_commands is empty under Supervised — every shell command outside the runtime allowlist will prompt for approval",
            )
            .with_category(self.category())
            .with_hint(
                "run: rantaiclaw setup approvals (or set [autonomy].allowed_commands in config.toml)",
            );
        }

        // No gate warning — report the on-disk file as the model sees it.
        match file_diag {
            AllowlistDiagnosis::Missing => CheckResult::info(
                self.name(),
                "no command_allowlist.toml yet (written when a preset is applied: `rantaiclaw setup approvals`)",
            )
            .with_category(self.category()),
            AllowlistDiagnosis::Empty => CheckResult::ok(
                self.name(),
                "command_allowlist.toml is empty (no pre-approved commands shown to the model)",
            )
            .with_category(self.category()),
            AllowlistDiagnosis::Healthy { count } => CheckResult::ok(
                self.name(),
                format!("command_allowlist.toml reports {count} pre-approved patterns shown to the model"),
            )
            .with_category(self.category()),
            AllowlistDiagnosis::Malformed(_) => unreachable!("handled above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::policy_writer::{self, PolicyPreset};
    use crate::config::Config;
    use crate::doctor::Severity;
    use crate::profile::Profile;
    use tempfile::TempDir;

    /// Count the patterns in the Smart bundle. Used as the test's expected
    /// value so the assertion tracks the writer rather than a literal count.
    fn smart_command_allowlist_count() -> usize {
        let bundle = include_str!("../../approval/presets/policy_smart.toml");
        let parsed: toml::Table = bundle.parse().expect("smart bundle parses");
        parsed
            .get("command_allowlist")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("patterns"))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .expect("smart bundle has command_allowlist.patterns")
    }

    fn profile_with(tmp: &TempDir) -> Profile {
        Profile {
            name: "test".into(),
            root: tmp.path().to_path_buf(),
        }
    }

    #[test]
    fn diagnose_returns_missing_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("command_allowlist.toml");
        assert_eq!(diagnose_allowlist(&file), AllowlistDiagnosis::Missing);
    }

    #[test]
    fn diagnose_returns_empty_when_writer_wrote_an_empty_patterns_list() {
        let tmp = TempDir::new().unwrap();
        let profile = profile_with(&tmp);
        // Manual preset ships with `patterns = []` — the writer puts that on
        // disk under `<profile>/policy/command_allowlist.toml`.
        policy_writer::write_policy_files(&profile, PolicyPreset::Manual, true)
            .expect("writer succeeds for Manual");
        let allowlist = profile.policy_dir().join("command_allowlist.toml");
        assert_eq!(diagnose_allowlist(&allowlist), AllowlistDiagnosis::Empty);
    }

    #[test]
    fn diagnose_returns_healthy_with_count_when_writer_wrote_patterns() {
        let tmp = TempDir::new().unwrap();
        let profile = profile_with(&tmp);
        let expected = smart_command_allowlist_count();
        assert!(expected > 0, "Smart bundle must declare patterns");
        policy_writer::write_policy_files(&profile, PolicyPreset::Smart, true)
            .expect("writer succeeds for Smart");
        let allowlist = profile.policy_dir().join("command_allowlist.toml");
        assert_eq!(
            diagnose_allowlist(&allowlist),
            AllowlistDiagnosis::Healthy { count: expected }
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

    // ── AllowlistCheck::run — gate wiring ──────────────────────────────

    fn stage_allowlist_file(root: &std::path::Path, diag: &AllowlistDiagnosis) {
        use std::fmt::Write as _;
        let dir = root.join("policy");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("command_allowlist.toml");
        match diag {
            AllowlistDiagnosis::Missing => {}
            AllowlistDiagnosis::Empty => {
                std::fs::write(&file, "[command_allowlist]\npatterns = []\n").unwrap();
            }
            AllowlistDiagnosis::Healthy { count } => {
                let mut body = String::from("[command_allowlist]\npatterns = [\n");
                for i in 0..*count {
                    writeln!(body, "  \"p{i}\",").unwrap();
                }
                body.push_str("]\n");
                std::fs::write(&file, body).unwrap();
            }
            AllowlistDiagnosis::Malformed(_) => {
                std::fs::write(&file, "this is { not toml = ").unwrap();
            }
        }
    }

    fn run_check(
        level: crate::security::AutonomyLevel,
        allowed_commands: Vec<String>,
        file_diag: AllowlistDiagnosis,
    ) -> CheckResult {
        let tmp = TempDir::new().unwrap();
        let mut config = Config::default();
        config.autonomy.level = level;
        config.autonomy.allowed_commands = allowed_commands;
        stage_allowlist_file(tmp.path(), &file_diag);
        let ctx = DoctorContext {
            profile: profile_with(&tmp),
            config,
            offline: true,
        };
        futures::executor::block_on(AllowlistCheck.run(&ctx))
    }

    #[test]
    fn run_warns_when_allowed_commands_empty_under_supervised() {
        let r = run_check(
            crate::security::AutonomyLevel::Supervised,
            vec![],
            AllowlistDiagnosis::Missing,
        );
        assert_eq!(r.severity, Severity::Warn, "{}", r.message);
        assert!(
            r.message.contains("allowed_commands"),
            "warn should name the gate key: {}",
            r.message
        );
        let hint = r.hint.expect("warn should carry a hint");
        assert!(
            hint.contains("setup approvals"),
            "hint should name `rantaiclaw setup approvals`: {hint}"
        );
        assert!(
            hint.contains("allowed_commands"),
            "hint should name `[autonomy].allowed_commands`: {hint}"
        );
    }

    #[test]
    fn run_does_not_warn_when_allowed_commands_non_empty_under_supervised() {
        let r = run_check(
            crate::security::AutonomyLevel::Supervised,
            vec!["git".to_string(), "ls".to_string()],
            AllowlistDiagnosis::Healthy { count: 63 },
        );
        assert_ne!(r.severity, Severity::Warn, "{}", r.message);
        assert_eq!(r.severity, Severity::Ok, "{}", r.message);
        assert!(
            r.message.contains("63"),
            "ok should report the file's pattern count: {}",
            r.message
        );
    }

    #[test]
    fn run_does_not_warn_about_approvals_under_read_only() {
        let r = run_check(
            crate::security::AutonomyLevel::ReadOnly,
            vec![],
            AllowlistDiagnosis::Healthy { count: 63 },
        );
        assert_ne!(
            r.severity,
            Severity::Warn,
            "ReadOnly must never warn about approvals: {}",
            r.message
        );
    }

    #[test]
    fn run_does_not_warn_about_approvals_under_full() {
        // Full mode allows every shell command by default, so an empty
        // allowed_commands is the expected state — never a warning.
        let r = run_check(
            crate::security::AutonomyLevel::Full,
            vec![],
            AllowlistDiagnosis::Healthy { count: 63 },
        );
        assert_ne!(r.severity, Severity::Warn, "{}", r.message);
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
