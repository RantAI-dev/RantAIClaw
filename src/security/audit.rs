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

/// Actor information (who performed the action)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub channel: String,
    pub user_id: Option<String>,
    pub username: Option<String>,
}

/// Action information (what was done)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub command: Option<String>,
    pub risk_level: Option<String>,
    pub approved: bool,
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
        });
        self
    }

    /// Set the action
    pub fn with_action(
        mut self,
        command: String,
        risk_level: String,
        approved: bool,
        allowed: bool,
    ) -> Self {
        self.action = Some(Action {
            command: Some(command),
            risk_level: Some(risk_level),
            approved,
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
    pub approved: bool,
    pub allowed: bool,
    pub success: bool,
    pub duration_ms: u64,
}

/// Owned form of [`CommandExecutionLog`], so one record can cross a
/// `spawn_blocking` boundary.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub channel: String,
    pub tool: String,
    pub risk_level: String,
    pub approved: bool,
    pub allowed: bool,
    pub success: bool,
    pub duration_ms: u64,
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
pub fn record_tool_call(record: ToolCallRecord) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let dir =
        crate::profile::paths::profile_dir(&crate::profile::ProfileManager::resolve_active_name());
    tokio::task::spawn_blocking(move || {
        // `SecurityConfig` is still not a field of `Config`, so there is no
        // reachable per-deployment `[security.audit]` to read; defaults
        // (enabled, `audit.log`, 100 MB rotation) are what the config-change
        // trail in `gateway/config_api.rs` already uses. Threading the operator's
        // block through is part of the sandbox decision that gates it.
        let Ok(logger) = AuditLogger::new(crate::config::AuditConfig::default(), dir) else {
            return;
        };
        if let Err(e) = logger.log_command_event(CommandExecutionLog {
            channel: &record.channel,
            command: &record.tool,
            risk_level: &record.risk_level,
            approved: record.approved,
            allowed: record.allowed,
            success: record.success,
            duration_ms: record.duration_ms,
        }) {
            tracing::warn!(target: "security", error = %e, "failed to write tool-call audit record");
        }
    });
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
            .with_actor(entry.channel.to_string(), None, None)
            .with_action(
                entry.command.to_string(),
                entry.risk_level.to_string(),
                entry.approved,
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
        approved: bool,
        allowed: bool,
        success: bool,
        duration_ms: u64,
    ) -> Result<()> {
        self.log_command_event(CommandExecutionLog {
            channel,
            command,
            risk_level,
            approved,
            allowed,
            success,
            duration_ms,
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

    #[test]
    fn audit_event_with_action() {
        let event = AuditEvent::new(AuditEventType::CommandExecution).with_action(
            "ls -la".to_string(),
            "low".to_string(),
            false,
            true,
        );

        assert!(event.action.is_some());
        let action = event.action.as_ref().unwrap();
        assert_eq!(action.command, Some("ls -la".to_string()));
        assert_eq!(action.risk_level, Some("low".to_string()));
    }

    #[test]
    fn audit_event_serializes_to_json() {
        let event = AuditEvent::new(AuditEventType::CommandExecution)
            .with_actor("telegram".to_string(), None, None)
            .with_action("ls".to_string(), "low".to_string(), false, true)
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
            approved: true,
            allowed: true,
            success: true,
            duration_ms: 1,
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
            tool: "shell".into(),
            risk_level: "executed".into(),
            approved: true,
            allowed: true,
            success: true,
            duration_ms: 1,
        });
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
            .with_action("ls".to_string(), "low".to_string(), false, true);

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
            approved: false,
            allowed: true,
            success: true,
            duration_ms: 42,
        })?;

        let log_path = tmp.path().join("audit.log");
        let content = tokio::fs::read_to_string(&log_path).await?;
        let parsed: AuditEvent = serde_json::from_str(content.trim())?;

        let action = parsed.action.unwrap();
        assert_eq!(action.command, Some("echo test".to_string()));
        assert_eq!(action.risk_level, Some("low".to_string()));
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
}
