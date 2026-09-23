//! Audit logging for security events

use crate::config::AuditConfig;
use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Audit event types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventType {
    CommandExecution,
    FileAccess,
    ConfigChange,
    AuthSuccess,
    AuthFailure,
    PolicyViolation,
    SecurityEvent,
}

/// Whether a human approved a call, or whether nobody was ever asked.
///
/// A single `approved: bool` wrote "the owner answered yes" and "policy let it
/// run without asking" as the same byte, so the trail could not prove that
/// anyone had approved anything. Three states is the fewest that can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// A human was asked and answered yes.
    Granted,
    /// Nobody was asked: the active policy let the call run unprompted.
    NotRequired,
    /// The call was refused, so the tool did not run.
    Denied,
}

/// Actor information (who performed the action)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub channel: String,
    pub user_id: Option<String>,
    pub username: Option<String>,
    /// Whether the chat sender is an owner or a guest. `None` on non-chat
    /// surfaces (CLI / scheduler / webhook / delegate). `#[serde(default)]` so
    /// audit records written before this field existed still parse, with the
    /// role reading as `None`.
    #[serde(default)]
    pub role: Option<String>,
}

/// Action information (what was done)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub command: Option<String>,
    pub risk_level: Option<String>,
    /// What let this call run — a human, the policy, or nobody.
    pub approval: ApprovalOutcome,
    pub allowed: bool,
}

/// Execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
}

/// Security context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityContext {
    pub policy_violation: bool,
    pub rate_limit_remaining: Option<u32>,
    pub sandbox_backend: Option<String>,
}

/// Complete audit event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub timestamp: DateTime<Utc>,
    pub event_id: String,
    pub event_type: AuditEventType,
    pub actor: Option<Actor>,
    pub action: Option<Action>,
    pub result: Option<ExecutionResult>,
    pub security: SecurityContext,
}

impl AuditEvent {
    /// Create a new audit event
    pub fn new(event_type: AuditEventType) -> Self {
        Self {
            timestamp: Utc::now(),
            event_id: Uuid::new_v4().to_string(),
            event_type,
            actor: None,
            action: None,
            result: None,
            security: SecurityContext {
                policy_violation: false,
                rate_limit_remaining: None,
                sandbox_backend: None,
            },
        }
    }

    /// Set the actor
    pub fn with_actor(
        mut self,
        channel: String,
        user_id: Option<String>,
        username: Option<String>,
    ) -> Self {
        self.actor = Some(Actor {
            channel,
            user_id,
            username,
            role: None,
        });
        self
    }

    /// Set the chat sender's role (owner / guest) on the actor. Kept as a
    /// separate setter so the existing 3-arg `with_actor` signature stays
    /// untouched; non-chat callers simply do not invoke this.
    pub fn with_role(mut self, role: Option<String>) -> Self {
        if let Some(actor) = self.actor.as_mut() {
            actor.role = role;
        } else {
            // No actor yet — record the role on a barebones one so the
            // channel can be inferred from the caller's context.
            self.actor = Some(Actor {
                channel: String::new(),
                user_id: None,
                username: None,
                role,
            });
        }
        self
    }

    /// Set the action
    pub fn with_action(
        mut self,
        command: String,
        risk_level: String,
        approval: ApprovalOutcome,
        allowed: bool,
    ) -> Self {
        self.action = Some(Action {
            command: Some(command),
            risk_level: Some(risk_level),
            approval,
            allowed,
        });
        self
    }

    /// Set the result
    pub fn with_result(
        mut self,
        success: bool,
        exit_code: Option<i32>,
        duration_ms: u64,
        error: Option<String>,
    ) -> Self {
        self.result = Some(ExecutionResult {
            success,
            exit_code,
            duration_ms: Some(duration_ms),
            error,
        });
        self
    }

    /// Set security context
    pub fn with_security(mut self, sandbox_backend: Option<String>) -> Self {
        self.security.sandbox_backend = sandbox_backend;
        self
    }
}

/// Audit logger
pub struct AuditLogger {
    log_path: PathBuf,
    config: AuditConfig,
    buffer: Mutex<Vec<AuditEvent>>,
}

/// Structured command execution details for audit logging.
#[derive(Debug, Clone)]
pub struct CommandExecutionLog<'a> {
    pub channel: &'a str,
    pub command: &'a str,
    pub risk_level: &'a str,
    pub approval: ApprovalOutcome,
    pub allowed: bool,
    pub success: bool,
    pub duration_ms: u64,
    /// Chat sender id. `None` for non-chat surfaces (CLI / scheduler /
    /// webhook / delegate), where the audit's `user_id` slot stays empty.
    pub sender: Option<&'a str>,
    /// `"owner"` or `"guest"` for chat; `None` for non-chat surfaces.
    pub role: Option<&'a str>,
}

/// Owned form of [`CommandExecutionLog`], so one record can cross a
/// `spawn_blocking` boundary.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub channel: String,
    /// Chat sender id; `None` for non-chat surfaces.
    pub sender: Option<String>,
    /// `"owner"` or `"guest"` for chat; `None` for non-chat surfaces.
    pub role: Option<String>,
    pub tool: String,
    pub risk_level: String,
    pub approval: ApprovalOutcome,
    pub allowed: bool,
    pub success: bool,
    pub duration_ms: u64,
}

/// Identity of who asked for a tool call, threaded from each entry point to
/// the audit log. Chat sends a sender id + role (owner or guest); non-chat
/// surfaces (CLI / scheduler / webhook / delegate) record their surface name
/// on the actor's `channel` field, so the trail still says who triggered the
/// call without filling `sender` or `role`.
#[derive(Debug, Clone, Default)]
pub struct AuditActor {
    pub sender: Option<String>,
    pub role: Option<String>,
}

impl AuditActor {
    /// Chat caller. `role` is `"owner"` or `"guest"`; the audit record carries
    /// both fields.
    pub fn chat(sender: String, role: &str) -> Self {
        Self {
            sender: Some(sender),
            role: Some(role.to_string()),
        }
    }

    /// Non-chat surface: returns the default `AuditActor` with `sender` and
    /// `role` left empty. The surface name is recorded separately on
    /// `channel` at every call site, so this constructor carries no argument
    /// and adds nothing to the actor.
    pub fn surface() -> Self {
        Self::default()
    }
}

/// Append one tool-call record to the active profile's audit log.
///
/// **Best-effort by construction.** The agent must not stop because a log write
/// failed, and must not pay a synchronous disk write per tool call, so the
/// append runs on a blocking worker and every failure is a `warn!` rather than
/// an error returned to the caller. Outside a Tokio runtime (unit tests that
/// call the tool path directly) it is a no-op.
///
/// Only the tool NAME is recorded, never its arguments: those carry file paths,
/// prompts and — for `shell` — whatever the model composed. The audit trail
/// answers "what ran, was it approved, did it succeed", which is what the
/// operator-facing docs have always claimed it answers.
///
/// **Test builds cannot reach the operator profile by accident.** Under
/// `cfg(test)` the directory is read from the `RANTAICLAW_AUDIT_DIR_OVERRIDE`
/// env var; if it is unset (or empty) the record is dropped with a `debug`
/// log line. A test that wants to capture audit records must point that var
/// at a temp directory while holding `crate::test_env::ENV_LOCK`, e.g. via
/// `crate::test_env::EnvGuard::set("RANTAICLAW_AUDIT_DIR_OVERRIDE", tmp.path())`.
/// Under `cfg(not(test))` the path is resolved from the active profile name,
/// unchanged from before.
pub fn record_tool_call(record: ToolCallRecord) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let Some(dir) = audit_dir_for_record() else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        let Ok(logger) = AuditLogger::new(crate::config::AuditConfig::default(), dir) else {
            return;
        };
        if let Err(e) = logger.log_command_event(CommandExecutionLog {
            channel: &record.channel,
            command: &record.tool,
            risk_level: &record.risk_level,
            approval: record.approval,
            allowed: record.allowed,
            success: record.success,
            duration_ms: record.duration_ms,
            sender: record.sender.as_deref(),
            role: record.role.as_deref(),
        }) {
            tracing::warn!(target: "security", error = %e, "failed to write tool-call audit record");
        }
    });
}

/// Resolve the directory `record_tool_call` should write into. Under
/// `cfg(test)` the operator's home is never reachable unless an env-var
/// override is set by the caller (typically a test holding `ENV_LOCK`); under
/// `cfg(not(test))` the active profile directory is used as before.
fn audit_dir_for_record() -> Option<PathBuf> {
    #[cfg(test)]
    {
        // Test build: refuse the real operator profile. A test that wants to
        // capture audit records must point this env var at a temp directory
        // (crate::test_env::EnvGuard::set("RANTAICLAW_AUDIT_DIR_OVERRIDE", …)
        // while holding ENV_LOCK); otherwise the record is dropped with a
        // debug log line and no file is written.
        match std::env::var_os("RANTAICLAW_AUDIT_DIR_OVERRIDE") {
            Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
            _ => {
                tracing::debug!(
                    "record_tool_call: RANTAICLAW_AUDIT_DIR_OVERRIDE is unset in a \
                     test build; dropping the audit record to keep the operator's \
                     real audit log untouched"
                );
                None
            }
        }
    }
    #[cfg(not(test))]
    {
        Some(crate::profile::paths::profile_dir(
            &crate::profile::ProfileManager::resolve_active_name(),
        ))
    }
}

/// Whether the log file at `path` ends in content whose last byte is not a
/// newline — the signature of a record torn by a crash.
///
/// Read through its own handle: the append handle is write-only, and reading
/// from it fails with `Bad file descriptor`.
fn ends_without_newline(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    if len == 0 {
        return false;
    }
    if file.seek(SeekFrom::Start(len - 1)).is_err() {
        return false;
    }
    let mut last = [0u8; 1];
    if file.read_exact(&mut last).is_err() {
        return false;
    }
    last[0] != b'\n'
}

impl AuditLogger {
    /// Create a new audit logger
    pub fn new(config: AuditConfig, rantaiclaw_dir: PathBuf) -> Result<Self> {
        let log_path = rantaiclaw_dir.join(&config.log_path);
        Ok(Self {
            log_path,
            config,
            buffer: Mutex::new(Vec::new()),
        })
    }

    /// Log an event
    pub fn log(&self, event: &AuditEvent) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        // Check log size and rotate if needed
        self.rotate_if_needed()?;

        // Serialize and write
        let line = serde_json::to_string(event)?;

        // Heal a torn tail before appending. A crash mid-write leaves a partial
        // record with no trailing newline; appending straight onto it glues the
        // next record to the broken one and loses BOTH — the damage spreads
        // instead of stopping at the record that was being written. One extra
        // newline confines it to the line that was already lost.
        let torn_tail = ends_without_newline(&self.log_path);

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;

        // ONE write per record, newline included. `writeln!` on a `File` is
        // unbuffered and issues a separate syscall for the body and for the
        // newline, so two concurrent writers interleave as
        // `bodyA bodyB \n \n` — two records glued into one unparseable line.
        // Every tool call now writes here, and a batch runs its audits
        // concurrently, so that race is the common case rather than a corner.
        let mut buf = String::with_capacity(line.len() + 2);
        if torn_tail {
            buf.push('\n');
        }
        buf.push_str(&line);
        buf.push('\n');
        file.write_all(buf.as_bytes())?;
        file.sync_all()?;

        Ok(())
    }

    /// Log a command execution event.
    pub fn log_command_event(&self, entry: CommandExecutionLog<'_>) -> Result<()> {
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            // The chat sender fits the existing `user_id` slot — the
            // `with_actor` signature stays 3-arg and the role rides on a
            // separate setter so non-chat callers don't have to thread a
            // value they have no use for.
            .with_actor(
                entry.channel.to_string(),
                entry.sender.map(str::to_string),
                None,
            )
            .with_role(entry.role.map(str::to_string))
            .with_action(
                entry.command.to_string(),
                entry.risk_level.to_string(),
                entry.approval,
                entry.allowed,
            )
            .with_result(entry.success, None, entry.duration_ms, None);

        self.log(&event)
    }

    /// Backward-compatible helper to log a command execution event.
    #[allow(clippy::too_many_arguments)]
    pub fn log_command(
        &self,
        channel: &str,
        command: &str,
        risk_level: &str,
        approval: ApprovalOutcome,
        allowed: bool,
        success: bool,
        duration_ms: u64,
    ) -> Result<()> {
        self.log_command_event(CommandExecutionLog {
            channel,
            command,
            risk_level,
            approval,
            allowed,
            success,
            duration_ms,
            sender: None,
            role: None,
        })
    }

    /// Rotate log if it exceeds max size
    fn rotate_if_needed(&self) -> Result<()> {
        if let Ok(metadata) = std::fs::metadata(&self.log_path) {
            let current_size_mb = metadata.len() / (1024 * 1024);
            if current_size_mb >= u64::from(self.config.max_size_mb) {
                self.rotate()?;
            }
        }
        Ok(())
    }

    /// Rotate the log file
    fn rotate(&self) -> Result<()> {
        for i in (1..10).rev() {
            let old_name = format!("{}.{}.log", self.log_path.display(), i);
            let new_name = format!("{}.{}.log", self.log_path.display(), i + 1);
            let _ = std::fs::rename(&old_name, &new_name);
        }

        let rotated = format!("{}.1.log", self.log_path.display());
        std::fs::rename(&self.log_path, &rotated)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn audit_event_new_creates_unique_id() {
        let event1 = AuditEvent::new(AuditEventType::CommandExecution);
        let event2 = AuditEvent::new(AuditEventType::CommandExecution);
        assert_ne!(event1.event_id, event2.event_id);
    }

    #[test]
    fn audit_event_with_actor() {
        let event = AuditEvent::new(AuditEventType::CommandExecution).with_actor(
            "telegram".to_string(),
            Some("123".to_string()),
            Some("@alice".to_string()),
        );

        assert!(event.actor.is_some());
        let actor = event.actor.as_ref().unwrap();
        assert_eq!(actor.channel, "telegram");
        assert_eq!(actor.user_id, Some("123".to_string()));
        assert_eq!(actor.username, Some("@alice".to_string()));
    }

    /// The audit record for a tool call must say WHO asked (chat sender) and
    /// WHETHER they were an owner or guest. Before this change the actor
    /// carried `channel` only, so a denial on a multi-user channel was
    /// unattributable. This pins the in-memory and on-disk shape: `user_id`
    /// is the chat sender and `role` is one of `"owner"` / `"guest"` for
    /// chat, absent otherwise.
    #[test]
    fn tool_call_record_carries_sender_and_role() -> Result<()> {
        let tmp = TempDir::new()?;
        let logger = enabled_logger(tmp.path())?;
        logger.log_command_event(CommandExecutionLog {
            channel: "telegram",
            command: "shell",
            risk_level: "executed",
            approval: ApprovalOutcome::Granted,
            allowed: true,
            success: true,
            duration_ms: 5,
            sender: Some("u_42"),
            role: Some("guest"),
        })?;

        let log_path = tmp.path().join("audit.log");
        let content = std::fs::read_to_string(&log_path)?;
        let parsed: AuditEvent = serde_json::from_str(content.trim())?;
        let actor = parsed.actor.as_ref().expect("actor set");

        assert_eq!(actor.channel, "telegram");
        assert_eq!(actor.user_id.as_deref(), Some("u_42"));
        assert_eq!(actor.role.as_deref(), Some("guest"));

        // Backward compat: an old record written before the `role` field
        // existed must still parse, with `role` defaulting to None. The
        // `#[serde(default)]` on `Actor.role` is what makes this safe; if
        // someone removes that attribute this test fails to parse.
        let old_json = r#"{"timestamp":"2026-09-18T00:00:00Z","event_id":"abc","event_type":"command_execution","actor":{"channel":"cli","user_id":null,"username":null},"action":{"command":"shell","risk_level":"low","approval":"not_required","allowed":true},"result":{"success":true,"exit_code":null,"duration_ms":5,"error":null},"security":{"policy_violation":false,"rate_limit_remaining":null,"sandbox_backend":null}}"#;
        let old: AuditEvent =
            serde_json::from_str(old_json).expect("old record without role key must still parse");
        assert_eq!(old.actor.as_ref().unwrap().role, None);
        Ok(())
    }

    #[test]
    fn audit_event_with_action() {
        let event = AuditEvent::new(AuditEventType::CommandExecution).with_action(
            "ls -la".to_string(),
            "low".to_string(),
            ApprovalOutcome::Granted,
            true,
        );

        assert!(event.action.is_some());
        let action = event.action.as_ref().unwrap();
        assert_eq!(action.command, Some("ls -la".to_string()));
        assert_eq!(action.risk_level, Some("low".to_string()));
        assert_eq!(action.approval, ApprovalOutcome::Granted);
    }

    /// The three names are the on-disk contract: anything parsing `audit.log`
    /// reads these strings, and the whole point of the field is that a reader
    /// can tell a human's yes from a policy that never asked.
    #[test]
    fn the_three_approval_outcomes_serialize_under_distinct_names() {
        let names: Vec<String> = [
            ApprovalOutcome::Granted,
            ApprovalOutcome::NotRequired,
            ApprovalOutcome::Denied,
        ]
        .iter()
        .map(|o| serde_json::to_string(o).expect("serialize"))
        .collect();
        assert_eq!(names, vec!["\"granted\"", "\"not_required\"", "\"denied\""]);
    }

    #[test]
    fn audit_event_serializes_to_json() {
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor("telegram".to_string(), None, None)
            .with_action(
                "ls".to_string(),
                "low".to_string(),
                ApprovalOutcome::Denied,
                true,
            )
            .with_result(true, Some(0), 15, None);

        let json = serde_json::to_string(&event);
        assert!(json.is_ok());
        let json = json.expect("serialize");
        let parsed: AuditEvent = serde_json::from_str(json.as_str()).expect("parse");
        assert!(parsed.actor.is_some());
        assert!(parsed.action.is_some());
        assert!(parsed.result.is_some());
    }

    #[test]
    fn audit_logger_disabled_does_not_create_file() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            enabled: false,
            ..Default::default()
        };
        let logger = AuditLogger::new(config, tmp.path().to_path_buf())?;
        let event = AuditEvent::new(AuditEventType::CommandExecution);

        logger.log(&event)?;

        // File should not exist since logging is disabled
        assert!(!tmp.path().join("audit.log").exists());
        Ok(())
    }

    // ── The claims pillar 3 already made (plan 305 step 4) ──────────────────
    //
    // `docs/pillars/3-tools-approvals.md` listed the audit log as Stable and
    // cited "v0.6 Resilience test verifies it survives restart + corruption".
    // No such test existed. These are it; the doc claim is restored in the same
    // PR, not before.

    fn enabled_logger(dir: &std::path::Path) -> Result<AuditLogger> {
        AuditLogger::new(
            AuditConfig {
                enabled: true,
                max_size_mb: 10,
                ..Default::default()
            },
            dir.to_path_buf(),
        )
    }

    fn record(command: &str) -> CommandExecutionLog<'_> {
        CommandExecutionLog {
            channel: "cli",
            command,
            risk_level: "executed",
            approval: ApprovalOutcome::Granted,
            allowed: true,
            success: true,
            duration_ms: 1,
            sender: None,
            role: None,
        }
    }

    #[tokio::test]
    async fn audit_log_survives_a_restart() -> Result<()> {
        let tmp = TempDir::new()?;
        let path = tmp.path().join("audit.log");

        // First "process": write, then drop the logger entirely.
        {
            let logger = enabled_logger(tmp.path())?;
            logger.log_command_event(record("before_restart"))?;
        }

        // Second "process": a fresh logger on the same directory must append,
        // not truncate — the earlier record is what an operator comes back for.
        {
            let logger = enabled_logger(tmp.path())?;
            logger.log_command_event(record("after_restart"))?;
        }

        let content = tokio::fs::read_to_string(&path).await?;
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "both records present: {content}");
        assert!(
            content.contains("before_restart"),
            "pre-restart record kept"
        );
        assert!(
            content.contains("after_restart"),
            "post-restart record added"
        );
        for line in lines {
            serde_json::from_str::<AuditEvent>(line)
                .unwrap_or_else(|e| panic!("line is not a whole event ({e}): {line}"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_truncated_audit_log_does_not_stop_new_records() -> Result<()> {
        let tmp = TempDir::new()?;
        let path = tmp.path().join("audit.log");

        {
            let logger = enabled_logger(tmp.path())?;
            logger.log_command_event(record("first"))?;
            logger.log_command_event(record("second"))?;
        }

        // Corrupt it the way a crash does: cut the file mid-record, leaving a
        // partial JSON line with no trailing newline.
        let whole = tokio::fs::read_to_string(&path).await?;
        let cut = whole.len() - 20;
        tokio::fs::write(&path, &whole[..cut]).await?;
        let damaged = tokio::fs::read_to_string(&path).await?;
        assert!(
            serde_json::from_str::<AuditEvent>(damaged.lines().last().unwrap_or_default()).is_err(),
            "the fixture must actually leave a broken last line"
        );

        // A later run must still append, and its own record must be whole and
        // parseable even though the line above it is not.
        {
            let logger = enabled_logger(tmp.path())?;
            logger.log_command_event(record("after_corruption"))?;
        }

        let content = tokio::fs::read_to_string(&path).await?;
        assert!(content.contains("after_corruption"), "new record written");
        let last = content.lines().last().expect("a last line");
        let parsed: AuditEvent = serde_json::from_str(last)
            .unwrap_or_else(|e| panic!("record after corruption is not whole ({e}): {last}"));
        assert_eq!(
            parsed.action.as_ref().and_then(|a| a.command.as_deref()),
            Some("after_corruption")
        );
        Ok(())
    }

    #[test]
    fn record_tool_call_outside_a_runtime_is_a_no_op() {
        // The tool path is called from sync unit tests too; a `spawn_blocking`
        // with no runtime would panic and take the caller down with it.
        record_tool_call(ToolCallRecord {
            channel: "cli".into(),
            sender: None,
            role: None,
            tool: "shell".into(),
            risk_level: "executed".into(),
            approval: ApprovalOutcome::NotRequired,
            allowed: true,
            success: true,
            duration_ms: 1,
        });
    }

    /// Pin the contract that a `#[tokio::test]` cannot reach the operator's
    /// real audit log without an explicit override. A channel funnel test
    /// that exercised this path used to append `mock_price` /
    /// `test-channel` entries to the operator's real
    /// `~/.../profiles/default/audit.log`, and the same happened whenever
    /// the live daemon happened to write there while this test ran, which is
    /// what made a before/after length comparison against the real file
    /// flaky. `HOME` is redirected to a temp directory so the assertion
    /// checks a profile dir nothing else on the machine can write to.
    #[tokio::test]
    async fn record_tool_call_in_a_test_without_override_does_not_touch_the_real_profile() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp_home = TempDir::new().expect("tempdir");
        let _home = crate::test_env::HomeGuard::set(tmp_home.path());
        // Explicitly UNSET the override so a leak from a sibling test cannot
        // redirect the write somewhere it would also miss the assertion.
        let _audit_off = crate::test_env::EnvGuard::unset("RANTAICLAW_AUDIT_DIR_OVERRIDE");

        // Resolve the profile dir the production way, now rooted under the
        // isolated temp `HOME` rather than the operator's real one. Create it
        // up front so a write, if one happened, would not fail merely
        // because the directory tree is missing — the only thing this test
        // wants to prove is that the override guard stops it.
        let profile_dir = crate::profile::paths::profile_dir(
            &crate::profile::ProfileManager::resolve_active_name(),
        );
        std::fs::create_dir_all(&profile_dir).expect("create isolated profile dir");
        let audit_log = profile_dir.join("audit.log");
        assert!(
            !audit_log.exists(),
            "the isolated temp profile dir must start clean, got: {audit_log:?}"
        );

        record_tool_call(ToolCallRecord {
            channel: "test-channel".into(),
            sender: None,
            role: None,
            tool: "noop".into(),
            risk_level: "executed".into(),
            approval: ApprovalOutcome::NotRequired,
            allowed: true,
            success: true,
            duration_ms: 1,
        });

        // Give the `spawn_blocking` a chance to run before checking.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            !audit_log.exists(),
            "record_tool_call under cfg(test) without the override must not write \
             any audit.log, got: {audit_log:?}"
        );
    }

    /// Pin the other half of the contract: when a test DOES opt in to
    /// capturing audit records by pointing the override at a temp dir, the
    /// write lands there, not in the operator profile.
    #[tokio::test]
    async fn record_tool_call_in_a_test_with_override_writes_to_the_override_dir() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let tmp = TempDir::new().expect("tempdir");
        let _audit = crate::test_env::EnvGuard::set("RANTAICLAW_AUDIT_DIR_OVERRIDE", tmp.path());

        record_tool_call(ToolCallRecord {
            channel: "test-channel".into(),
            sender: None,
            role: None,
            tool: "noop".into(),
            risk_level: "executed".into(),
            approval: ApprovalOutcome::NotRequired,
            allowed: true,
            success: true,
            duration_ms: 1,
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let log = tmp.path().join("audit.log");
        assert!(
            log.exists(),
            "with the override set, record_tool_call must write to <override>/audit.log"
        );
    }

    // ── §8.1 Log rotation tests ─────────────────────────────

    #[tokio::test]
    async fn audit_logger_writes_event_when_enabled() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            enabled: true,
            max_size_mb: 10,
            ..Default::default()
        };
        let logger = AuditLogger::new(config, tmp.path().to_path_buf())?;
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor("cli".to_string(), None, None)
            .with_action(
                "ls".to_string(),
                "low".to_string(),
                ApprovalOutcome::Denied,
                true,
            );

        logger.log(&event)?;

        let log_path = tmp.path().join("audit.log");
        assert!(log_path.exists(), "audit log file must be created");

        let content = tokio::fs::read_to_string(&log_path).await?;
        assert!(!content.is_empty(), "audit log must not be empty");

        let parsed: AuditEvent = serde_json::from_str(content.trim())?;
        assert!(parsed.action.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn audit_log_command_event_writes_structured_entry() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            enabled: true,
            max_size_mb: 10,
            ..Default::default()
        };
        let logger = AuditLogger::new(config, tmp.path().to_path_buf())?;

        logger.log_command_event(CommandExecutionLog {
            channel: "telegram",
            command: "echo test",
            risk_level: "low",
            approval: ApprovalOutcome::NotRequired,
            allowed: true,
            success: true,
            duration_ms: 42,
            sender: None,
            role: None,
        })?;

        let log_path = tmp.path().join("audit.log");
        let content = tokio::fs::read_to_string(&log_path).await?;
        let parsed: AuditEvent = serde_json::from_str(content.trim())?;

        let action = parsed.action.unwrap();
        assert_eq!(action.command, Some("echo test".to_string()));
        assert_eq!(action.risk_level, Some("low".to_string()));
        assert_eq!(action.approval, ApprovalOutcome::NotRequired);
        assert!(action.allowed);

        let result = parsed.result.unwrap();
        assert!(result.success);
        assert_eq!(result.duration_ms, Some(42));
        Ok(())
    }

    #[test]
    fn audit_rotation_creates_numbered_backup() -> Result<()> {
        let tmp = TempDir::new()?;
        let config = AuditConfig {
            enabled: true,
            max_size_mb: 0, // Force rotation on first write
            ..Default::default()
        };
        let logger = AuditLogger::new(config, tmp.path().to_path_buf())?;

        // Write initial content that triggers rotation
        let log_path = tmp.path().join("audit.log");
        std::fs::write(&log_path, "initial content\n")?;

        let event = AuditEvent::new(AuditEventType::CommandExecution);
        logger.log(&event)?;

        let rotated = format!("{}.1.log", log_path.display());
        assert!(
            std::path::Path::new(&rotated).exists(),
            "rotation must create .1.log backup"
        );
        Ok(())
    }

    // ── The audit comments match the code ─────────────────────────
    //
    // Three comments in this module, `src/agent/loop_.rs` and
    // `src/channels/slack.rs` described behaviour that did not exist.
    // Pin the actual contract here so a future reader cannot quietly
    // re-introduce the half-truths.

    /// `AuditActor::surface` takes no argument and returns the default. The
    /// surface name is already recorded on `channel` at every call site, so
    /// the constructor deliberately leaves `sender` and `role` empty. A
    /// version that took the name and stuffed it onto `sender` (or
    /// `user_id`) would change what existing audit readers parse — keep it
    /// that way.
    ///
    /// Written against the post-fix API (`surface()` with zero args). It will
    /// fail to compile against the pre-fix `surface(_name: &str)` signature;
    /// that compile failure is the expected red, and the implementation step
    /// drops the parameter to turn it green.
    #[test]
    fn audit_actor_surface_returns_default_and_takes_no_argument() {
        let actor = AuditActor::surface();
        assert!(
            actor.sender.is_none(),
            "surface() must leave sender empty, got {:?}",
            actor.sender
        );
        assert!(
            actor.role.is_none(),
            "surface() must leave role empty, got {:?}",
            actor.role
        );
        let default = AuditActor::default();
        assert_eq!(actor.sender, default.sender);
        assert_eq!(actor.role, default.role);
    }

    /// Every production call site must have been migrated to the new
    /// zero-argument form. Slicing the `#[cfg(test)]` module off first so
    /// this assertion cannot match itself.
    ///
    /// Mutation: restore one call site to `AuditActor::surface("cli")` —
    /// the substring assertion finds it and the test falls.
    #[test]
    fn no_production_call_site_passes_an_argument_to_audit_actor_surface() {
        const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod ";
        fn production_half(src: &str) -> &str {
            let cut = [TEST_MODULE_MARKER]
                .iter()
                .flat_map(|marker| src.match_indices(marker))
                .map(|(at, _)| at)
                .min()
                .unwrap_or(src.len());
            &src[..cut]
        }
        const SOURCE_FILES: &[(&str, &str)] = &[
            ("agent/loop_.rs", include_str!("../agent/loop_.rs")),
            ("agent/agent.rs", include_str!("../agent/agent.rs")),
            ("gateway/mod.rs", include_str!("../gateway/mod.rs")),
            ("tools/delegate.rs", include_str!("../tools/delegate.rs")),
        ];
        for (name, src) in SOURCE_FILES {
            let production = production_half(src);
            assert!(
                !production.contains("AuditActor::surface(\""),
                "production of {name} still calls AuditActor::surface(\"...\"); \
                 drop the argument — the surface name is already on `channel`"
            );
        }
    }

    /// The 4-line comment above the `audit_actor` parameter in
    /// `loop_.rs::execute_structured_tool_calls` once claimed the executor
    /// derived `"guest"` from `guest_gate` when the caller left the role
    /// empty. No such derivation exists — `role` is whatever the caller
    /// passed (chat sets it at `channels/dispatch.rs:761`). Pin that no
    /// `audit_actor` doc block in `loop_.rs` makes the derivation claim
    /// or mentions `guest_gate`.
    ///
    /// Mutation: restore the derivation claim in any of the four
    /// `// Identity of who asked for the call.` doc blocks — the substring
    /// assertion finds it and the test falls.
    #[test]
    fn loop_rs_no_longer_claims_executor_derives_guest_role_from_gate() {
        const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod ";
        fn production_half(src: &str) -> &str {
            let cut = [TEST_MODULE_MARKER]
                .iter()
                .flat_map(|marker| src.match_indices(marker))
                .map(|(at, _)| at)
                .min()
                .unwrap_or(src.len());
            &src[..cut]
        }
        let production = production_half(include_str!("../agent/loop_.rs"));

        // The loop has several `audit_actor` parameter doc blocks that all
        // start with `// Identity of who asked for the call.` Walk every one
        // and assert none of them claims derivation or names the gate.
        let start_marker = "// Identity of who asked for the call.";
        let mut search_from = 0usize;
        let mut blocks_checked = 0usize;
        while let Some(rel) = production[search_from..].find(start_marker) {
            let block_start = search_from + rel;
            // End of the doc block: the next `audit_actor:` parameter line.
            let after = &production[block_start..];
            let end_rel = after
                .find("audit_actor: &crate::security::AuditActor")
                .unwrap_or(after.len());
            let doc_block = &after[..end_rel];
            blocks_checked += 1;
            assert!(
                !doc_block.contains("derives"),
                "an audit_actor doc block still claims the executor derives a \
                 role; delete the derivation claim — no such code exists. \
                 Block: {doc_block:?}"
            );
            assert!(
                !doc_block.contains("guest_gate"),
                "an audit_actor doc block still mentions guest_gate; the \
                 parameter is independent of the gate. Block: {doc_block:?}"
            );
            search_from = block_start + start_marker.len();
        }
        assert!(
            blocks_checked > 0,
            "expected at least one `// Identity of who asked for the call.` \
             doc block in loop_.rs; the test would silently pass on a delete"
        );
    }
}
