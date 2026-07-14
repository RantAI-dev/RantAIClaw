use super::traits::{Tool, ToolResult};
use crate::runtime::RuntimeAdapter;
use crate::security::{Decision, SecurityPolicy};
use crate::tools::RATE_LIMIT_REMEDIATION;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Maximum shell command execution time before the process group is killed.
/// Generous enough for real installs (apt/dpkg, `docker pull`, image builds);
/// a genuinely hung command still dies at this bound.
const SHELL_TIMEOUT_SECS: u64 = 600;
/// Maximum output size in bytes (1MB).
const MAX_OUTPUT_BYTES: usize = 1_048_576;
/// Environment variables safe to pass to shell commands.
/// Only functional variables are included — never API keys or secrets.
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

/// Appended to hard-blocked shell errors so the operator discovers a concrete
/// next step instead of dead-ending. Real sessions showed users grinding
/// through manual config edits because the bare "Command not allowed" message
/// named no remediation. `rantaiclaw autonomy full` removes all gating
/// (Full autonomy bypasses the command allowlist), while the allowlist edit is
/// the narrower, safer option for a single recurring command.
const BLOCKED_COMMAND_REMEDIATION: &str = "\nBlocked by the active security policy. \
An operator can allow the base command via [autonomy].allowed_commands in config.toml, \
or remove approval prompts entirely with `rantaiclaw autonomy full` \
(no prompts — use only in trusted/sandboxed environments).";

/// Read an async pipe to EOF, keeping at most `cap` bytes in memory. Bytes past
/// the cap are still drained (so the child never blocks on a full pipe) but
/// discarded — a runaway command can't OOM the agent, unlike `Command::output`
/// which buffers everything before truncation.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<&mut R>,
    cap: usize,
) -> Vec<u8> {
    let mut buf = Vec::new();
    let Some(reader) = reader else {
        return buf;
    };
    // Heap buffer (not a stack array) so the read future stays small.
    let mut chunk = vec![0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = n.min(cap - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    buf
}

/// RAII guard that reaps the shell's whole process group on drop — i.e. when the
/// tool future is dropped by an agent cancel (`tokio::select!`) or by the shell
/// timeout. SIGTERM immediately, then SIGKILL after a short grace if any group
/// member is still alive. Disarmed on normal completion so a since-reused pgid is
/// never signalled. No-op on non-Unix (`pgid` is always `None` there).
struct ProcGroupKill {
    pgid: Option<i32>,
}

impl ProcGroupKill {
    fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for ProcGroupKill {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid {
                // SAFETY: `pgid > 1` (checked at construction) and names the
                // shell's own new group (created via setsid), never rantaiclaw's.
                unsafe {
                    libc::kill(-pgid, libc::SIGTERM);
                }
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        // Poll for the group to exit rather than sleeping the full
                        // grace and checking once at the end. As soon as the group
                        // is gone (ESRCH) we stop — so we escalate to SIGKILL only
                        // for a group that is STILL alive after the grace (one
                        // genuinely ignoring SIGTERM, i.e. our own). This shrinks
                        // the window in which the pgid could have been reused by an
                        // unrelated group and then wrongly force-killed from the
                        // full 2s down to a single poll interval. (A race-free fix
                        // needs pidfd; this is the portable mitigation.)
                        const GRACE: Duration = Duration::from_secs(2);
                        const POLL: Duration = Duration::from_millis(50);
                        let mut waited = Duration::ZERO;
                        while waited < GRACE {
                            // ESRCH => the whole group has exited; nothing to kill.
                            if unsafe { libc::kill(-pgid, 0) } != 0 {
                                return;
                            }
                            tokio::time::sleep(POLL).await;
                            waited += POLL;
                        }
                        // Still alive after the grace — force-kill.
                        unsafe {
                            libc::kill(-pgid, libc::SIGKILL);
                        }
                    });
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.pgid;
        }
    }
}

/// Shell command execution tool with sandboxing
pub struct ShellTool {
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    /// Per-skill env overlay merged onto every shell exec on top of
    /// `SAFE_ENV_VARS`. Built from `[skills.entries.<n>].env`,
    /// `.api_key`, and `.config.*` of every *enabled* skill at tool
    /// construction time. See `compose_skill_env` in `src/tools/mod.rs`.
    /// `OpenClaw`-parity behavior: a skill that declares
    /// `api_key.source = "env"` and the user has set the matching
    /// outer env var gets that value re-exported into the child
    /// process; `config.*` values become `RANTAICLAW_SKILL_<NAME>_<KEY>`.
    skill_env: std::collections::HashMap<String, String>,
}

impl ShellTool {
    pub fn new(security: Arc<SecurityPolicy>, runtime: Arc<dyn RuntimeAdapter>) -> Self {
        Self {
            security,
            runtime,
            skill_env: std::collections::HashMap::new(),
        }
    }

    /// Construct with a precomputed skill-env overlay. Used by
    /// `all_tools_with_runtime` after consulting `[skills.entries]`.
    pub fn with_skill_env(
        security: Arc<SecurityPolicy>,
        runtime: Arc<dyn RuntimeAdapter>,
        skill_env: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            security,
            runtime,
            skill_env,
        }
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the workspace directory"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "approved": {
                    "type": "boolean",
                    "description": "Set true to explicitly approve medium/high-risk commands in supervised mode",
                    "default": false
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Rate limit exceeded: too many actions in the last hour.{RATE_LIMIT_REMEDIATION}"
                )),
            });
        }

        // Cascading approval loop: a single shell command may chain
        // multiple basenames via `&&` (e.g. `cd … && python3 …`). The
        // gate rejects on the FIRST unallowed basename; approving it
        // doesn't help if the next segment is also blocked. Walk the
        // chain, prompting for each new blocker, until either the
        // command validates or the user denies. Cap at 6 prompts per
        // call so an adversarial command can't spin forever.
        const MAX_CASCADING_APPROVALS: usize = 6;
        let mut last_reason: Option<String> = None;
        let mut iters = 0;
        loop {
            match self.security.validate_command_execution(command, approved) {
                Ok(_) => break,
                Err(reason) => {
                    iters += 1;
                    if iters > MAX_CASCADING_APPROVALS {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!(
                                "Cascading approval limit reached ({MAX_CASCADING_APPROVALS}) — last error: {reason}"
                            )),
                        });
                    }
                    let (Some(approvals), Some(basename)) = (
                        self.security.pending(),
                        self.security.first_unallowed_basename(command),
                    ) else {
                        // Hard block (high-risk, redirect, subshell
                        // expansion, etc.) — no basename to approve;
                        // return the error and let the LLM/UI decide
                        // what to do. The TUI's per-turn block counter
                        // surfaces a "switch to /autonomy off" toast
                        // when these pile up.
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(format!("{reason}{BLOCKED_COMMAND_REMEDIATION}")),
                        });
                    };
                    let decision = approvals
                        .request_decision(basename.clone(), command.to_string(), "")
                        .await;
                    match decision {
                        Decision::Once | Decision::Session => {
                            if let Err(e) = self.security.add_runtime_command(&basename, false) {
                                tracing::warn!(target: "shell", error = %e, "add_runtime_command failed");
                            }
                            last_reason = Some(reason);
                            continue;
                        }
                        Decision::Persist => {
                            if let Err(e) = self.security.add_runtime_command(&basename, true) {
                                tracing::warn!(
                                    target: "shell",
                                    error = %e,
                                    "add_runtime_command(persist) failed; falling back to session-only"
                                );
                                let _ = self.security.add_runtime_command(&basename, false);
                            }
                            last_reason = Some(reason);
                            continue;
                        }
                        Decision::Deny => {
                            // Explicit user deny: keep the LAST error
                            // (the one that triggered this prompt) so
                            // the message accurately names the blocker
                            // the user just rejected — not the very
                            // first one from before any approvals.
                            let final_reason = last_reason.unwrap_or(reason);
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(final_reason),
                            });
                        }
                    }
                }
            }
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Rate limit exceeded: action budget exhausted.{RATE_LIMIT_REMEDIATION}"
                )),
            });
        }

        // Defensive: re-create the workspace directory if it vanished
        // (e.g. user deleted it between Config::load_or_init and now).
        // Without this, `current_dir(workspace_dir)` makes Command::spawn
        // return a confusing "No such file or directory" with 0ms
        // elapsed — same shape as a missing `sh` binary.
        if !self.security.workspace_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&self.security.workspace_dir) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Workspace directory {} did not exist and could not be re-created: {e}",
                        self.security.workspace_dir.display()
                    )),
                });
            }
        }

        // Execute with timeout to prevent hanging commands.
        // Clear the environment to prevent leaking API keys and other secrets
        // (CWE-200), then re-add only safe, functional variables.
        let mut cmd = match self
            .runtime
            .build_shell_command(command, &self.security.workspace_dir)
        {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to build runtime command: {e}")),
                });
            }
        };
        cmd.env_clear();

        for var in SAFE_ENV_VARS {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }

        // Per-skill env overlay (`[skills.entries.<n>]` from config). User
        // explicitly opts in by writing values into config; this is *not*
        // an automatic leak of process env. SAFE_ENV_VARS comes first so
        // skills can override PATH (intentional, e.g. add brew to PATH).
        for (k, v) in &self.skill_env {
            cmd.env(k, v);
        }

        // Spawn with piped stdio so we can (a) bound-read the output to cap memory
        // and (b) reap the whole process group on cancel/timeout. `Command::output`
        // instead buffers to EOF (OOM risk) and stops only the direct child.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                // ENOENT usually means `sh` isn't on PATH (env_clear stripped it
                // and SAFE_ENV_VARS didn't restore it) or workspace_dir vanished.
                let mut hint = format!("Failed to execute command: {e}");
                if e.kind() == std::io::ErrorKind::NotFound {
                    if !self.security.workspace_dir.exists() {
                        hint = format!(
                            "Shell spawn failed because workspace directory does not exist: {}\n\
                             Re-create with: mkdir -p {}",
                            self.security.workspace_dir.display(),
                            self.security.workspace_dir.display(),
                        );
                    } else if std::env::var_os("PATH").is_none() {
                        hint = "Shell spawn failed: PATH is empty in the rantaiclaw \
                                 process environment. Set PATH before launching."
                            .to_string();
                    }
                }
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(hint),
                });
            }
        };

        // Arm the process-group killer (native/Unix only). On agent cancel OR
        // timeout the tool future is dropped, dropping this guard, which reaps the
        // shell's whole tree — apt/dpkg/ssh/pipeline children that `kill_on_drop`
        // (direct child only) would otherwise leave running.
        let pgid = if self.runtime.spawns_process_group() {
            child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .filter(|p| *p > 1)
        } else {
            None
        };
        let mut group_kill = ProcGroupKill { pgid };

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let collect = async {
            let (out_bytes, err_bytes) = tokio::join!(
                read_capped(stdout_pipe.as_mut(), MAX_OUTPUT_BYTES),
                read_capped(stderr_pipe.as_mut(), MAX_OUTPUT_BYTES),
            );
            let status = child.wait().await;
            (status, out_bytes, err_bytes)
        };

        match tokio::time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), collect).await {
            Ok((Ok(status), out_bytes, err_bytes)) => {
                // Completed on its own — disarm so we never signal a reused pgid.
                group_kill.disarm();
                let mut stdout = String::from_utf8_lossy(&out_bytes).to_string();
                let mut stderr = String::from_utf8_lossy(&err_bytes).to_string();
                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(stdout.floor_char_boundary(MAX_OUTPUT_BYTES));
                    stdout.push_str("\n... [output truncated at 1MB]");
                }
                if stderr.len() > MAX_OUTPUT_BYTES {
                    stderr.truncate(stderr.floor_char_boundary(MAX_OUTPUT_BYTES));
                    stderr.push_str("\n... [stderr truncated at 1MB]");
                }
                if status.success() {
                    // Fold stderr into output — apt/docker/git write progress and
                    // warnings to stderr even on success, and the agent loop
                    // discards `error` on success, so this is the only surviving
                    // channel for it.
                    let output = if stderr.is_empty() {
                        stdout
                    } else {
                        format!("{stdout}\n[stderr]\n{stderr}")
                    };
                    Ok(ToolResult {
                        success: true,
                        output,
                        error: None,
                    })
                } else {
                    // Surface the exit code so a silent non-zero exit isn't a bare
                    // "Error:" with no signal for the model.
                    let status_desc = match status.code() {
                        Some(code) => format!("Command exited with status {code}"),
                        None => "Command terminated by signal".to_string(),
                    };
                    let error = if stderr.is_empty() {
                        status_desc
                    } else {
                        format!("{status_desc}: {stderr}")
                    };
                    Ok(ToolResult {
                        success: false,
                        output: stdout,
                        error: Some(error),
                    })
                }
            }
            Ok((Err(e), _, _)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to wait on command: {e}")),
            }),
            // Timeout: `group_kill` (still armed) drops after this match and reaps
            // the whole group, so the message is now accurate.
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Command timed out after {SHELL_TIMEOUT_SECS}s; the process group was terminated"
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{NativeRuntime, RuntimeAdapter};
    use crate::security::{AutonomyLevel, PendingApprovals, SecurityPolicy};

    fn test_security(autonomy: AutonomyLevel) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_runtime() -> Arc<dyn RuntimeAdapter> {
        Arc::new(NativeRuntime::new())
    }

    #[test]
    fn shell_tool_name() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        assert_eq!(tool.name(), "shell");
    }

    #[test]
    fn shell_tool_description() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn shell_tool_schema_has_command() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["command"].is_object());
        assert!(schema["required"]
            .as_array()
            .expect("schema required field should be an array")
            .contains(&json!("command")));
        assert!(schema["properties"]["approved"].is_object());
    }

    #[tokio::test]
    async fn shell_executes_allowed_command() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(json!({"command": "echo hello"}))
            .await
            .expect("echo command execution should succeed");
        assert!(result.success);
        assert!(result.output.trim().contains("hello"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn shell_blocks_disallowed_command() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(json!({"command": "rm -rf /"}))
            .await
            .expect("disallowed command execution should return a result");
        assert!(!result.success);
        let error = result.error.as_deref().unwrap_or("");
        assert!(error.contains("not allowed") || error.contains("high-risk"));
    }

    #[tokio::test]
    async fn shell_blocks_readonly() {
        let tool = ShellTool::new(test_security(AutonomyLevel::ReadOnly), test_runtime());
        let result = tool
            .execute(json!({"command": "ls"}))
            .await
            .expect("readonly command execution should return a result");
        assert!(!result.success);
        assert!(result
            .error
            .as_ref()
            .expect("error field should be present for blocked command")
            .contains("not allowed"));
    }

    #[tokio::test]
    async fn shell_missing_command_param() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("command"));
    }

    #[tokio::test]
    async fn shell_wrong_type_param() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool.execute(json!({"command": 123})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn shell_captures_exit_code() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(json!({"command": "ls /nonexistent_dir_xyz"}))
            .await
            .expect("command with nonexistent path should return a result");
        assert!(!result.success);
    }

    fn test_security_with_env_cmd() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["env".into(), "echo".into()],
            ..SecurityPolicy::default()
        })
    }

    /// RAII guard that restores an environment variable to its original state on drop,
    /// ensuring cleanup even if the test panics.
    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(val) => std::env::set_var(self.key, val),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shell_does_not_leak_api_key() {
        let _g1 = EnvGuard::set("API_KEY", "sk-test-secret-12345");
        let _g2 = EnvGuard::set("RANTAICLAW_API_KEY", "sk-test-secret-67890");

        let tool = ShellTool::new(test_security_with_env_cmd(), test_runtime());
        let result = tool
            .execute(json!({"command": "env"}))
            .await
            .expect("env command execution should succeed");
        assert!(result.success);
        assert!(
            !result.output.contains("sk-test-secret-12345"),
            "API_KEY leaked to shell command output"
        );
        assert!(
            !result.output.contains("sk-test-secret-67890"),
            "RANTAICLAW_API_KEY leaked to shell command output"
        );
    }

    #[tokio::test]
    async fn shell_preserves_path_and_home() {
        let tool = ShellTool::new(test_security_with_env_cmd(), test_runtime());

        let result = tool
            .execute(json!({"command": "echo $HOME"}))
            .await
            .expect("echo HOME command should succeed");
        assert!(result.success);
        assert!(
            !result.output.trim().is_empty(),
            "HOME should be available in shell"
        );

        let result = tool
            .execute(json!({"command": "echo $PATH"}))
            .await
            .expect("echo PATH command should succeed");
        assert!(result.success);
        assert!(
            !result.output.trim().is_empty(),
            "PATH should be available in shell"
        );
    }

    #[tokio::test]
    async fn shell_requires_approval_for_medium_risk_command() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            allowed_commands: vec!["touch".into()],
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });

        let tool = ShellTool::new(security.clone(), test_runtime());
        let denied = tool
            .execute(json!({"command": "touch rantaiclaw_shell_approval_test"}))
            .await
            .expect("unapproved command should return a result");
        assert!(!denied.success);
        assert!(denied
            .error
            .as_deref()
            .unwrap_or("")
            .contains("explicit approval"));

        let allowed = tool
            .execute(json!({
                "command": "touch rantaiclaw_shell_approval_test",
                "approved": true
            }))
            .await
            .expect("approved command execution should succeed");
        assert!(allowed.success);

        let _ = tokio::fs::remove_file(std::env::temp_dir().join("rantaiclaw_shell_approval_test"))
            .await;
    }

    // ── §5.2 Shell timeout enforcement tests ─────────────────

    #[test]
    fn shell_timeout_constant_is_reasonable() {
        assert_eq!(
            SHELL_TIMEOUT_SECS, 600,
            "shell timeout must be 600s — long enough for real installs, still bounded"
        );
    }

    #[test]
    fn shell_output_limit_is_1mb() {
        assert_eq!(
            MAX_OUTPUT_BYTES, 1_048_576,
            "max output must be 1 MB to prevent OOM"
        );
    }

    // ── §5.3 Non-UTF8 binary output tests ────────────────────

    #[test]
    fn shell_safe_env_vars_excludes_secrets() {
        for var in SAFE_ENV_VARS {
            let lower = var.to_lowercase();
            assert!(
                !lower.contains("key") && !lower.contains("secret") && !lower.contains("token"),
                "SAFE_ENV_VARS must not include sensitive variable: {var}"
            );
        }
    }

    #[test]
    fn shell_safe_env_vars_includes_essentials() {
        assert!(
            SAFE_ENV_VARS.contains(&"PATH"),
            "PATH must be in safe env vars"
        );
        assert!(
            SAFE_ENV_VARS.contains(&"HOME"),
            "HOME must be in safe env vars"
        );
        assert!(
            SAFE_ENV_VARS.contains(&"TERM"),
            "TERM must be in safe env vars"
        );
    }

    #[tokio::test]
    async fn shell_blocks_rate_limited() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            max_actions_per_hour: 0,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime());
        let result = tool
            .execute(json!({"command": "echo test"}))
            .await
            .expect("rate-limited command should return a result");
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Rate limit"));
    }

    // ── approval-driven runtime allowlist ───────────────────

    fn supervised_security_only_echo() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["echo".into()],
            ..SecurityPolicy::default()
        })
    }

    #[tokio::test]
    async fn shell_without_approvals_still_blocks_unknown_basename() {
        let tool = ShellTool::new(supervised_security_only_echo(), test_runtime());
        let result = tool.execute(json!({"command": "true"})).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("not allowed"),
            "without approvals registry, behavior must match pre-PR-2"
        );
    }

    #[tokio::test]
    async fn shell_with_approvals_session_decision_unblocks() {
        let security = supervised_security_only_echo();
        let approvals = Arc::new(PendingApprovals::new(Some(Duration::from_secs(5))));
        security.set_pending(approvals.clone());
        let tool = ShellTool::new(security.clone(), test_runtime());

        let mut rx = approvals.subscribe();
        let approvals_resolver = approvals.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("notification");
            assert_eq!(req.basename, "true");
            approvals_resolver.resolve(req.id, Decision::Session);
        });

        let result = tool.execute(json!({"command": "true"})).await.unwrap();
        assert!(
            result.success,
            "session approval should unblock the command"
        );
        assert!(
            security
                .runtime_allowlist_snapshot()
                .contains(&"true".to_string()),
            "session approval must add basename to runtime allowlist"
        );
    }

    #[tokio::test]
    async fn shell_with_approvals_deny_keeps_original_error() {
        let security = supervised_security_only_echo();
        let approvals = Arc::new(PendingApprovals::new(Some(Duration::from_secs(5))));
        security.set_pending(approvals.clone());
        let tool = ShellTool::new(security.clone(), test_runtime());

        let mut rx = approvals.subscribe();
        let approvals_resolver = approvals.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("notification");
            approvals_resolver.resolve(req.id, Decision::Deny);
        });

        let result = tool.execute(json!({"command": "true"})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("not allowed"));
        assert!(
            !security
                .runtime_allowlist_snapshot()
                .contains(&"true".to_string()),
            "deny must not mutate the runtime allowlist"
        );
    }

    #[tokio::test]
    async fn shell_with_approvals_timeout_denies() {
        let security = supervised_security_only_echo();
        let approvals = Arc::new(PendingApprovals::new(Some(Duration::from_millis(50))));
        security.set_pending(approvals.clone());
        let tool = ShellTool::new(security.clone(), test_runtime());

        let result = tool.execute(json!({"command": "true"})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("not allowed"));
    }

    #[tokio::test]
    async fn shell_with_approvals_skipped_for_structural_rejection() {
        // Structural failures (subshells, redirects, …) should NOT
        // surface an approval prompt — approving "echo" doesn't fix
        // `echo $(rm -rf /)`.
        let security = supervised_security_only_echo();
        let approvals = Arc::new(PendingApprovals::new(Some(Duration::from_millis(50))));
        security.set_pending(approvals.clone());
        let tool = ShellTool::new(security.clone(), test_runtime());

        // If the prompt path were entered, we'd time out at 50ms; the
        // structural skip means we bail synchronously.
        let start = std::time::Instant::now();
        let result = tool
            .execute(json!({"command": "echo $(true)"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            start.elapsed() < Duration::from_millis(40),
            "structural rejection must skip the approval timeout"
        );
    }

    /// A group leader that ignores SIGTERM must still be reaped by the SIGKILL
    /// escalation after the grace — the polling refactor must not weaken that.
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_cancel_escalates_to_sigkill_for_a_sigterm_ignoring_group() {
        let pidfile =
            std::env::temp_dir().join(format!("rantaiclaw_sigkill_{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let tool = ShellTool::new(test_security(AutonomyLevel::Full), test_runtime());
        // Trap SIGTERM and loop forever: SIGTERM alone can't stop it, so only the
        // SIGKILL escalation can.
        let cmd = format!(
            "trap '' TERM; echo $$ > {}; while :; do sleep 0.2; done",
            pidfile.display()
        );
        let handle = tokio::spawn(async move { tool.execute(json!({ "command": cmd })).await });

        let mut pid = None;
        for _ in 0..200 {
            if let Ok(s) = std::fs::read_to_string(&pidfile) {
                if let Ok(p) = s.trim().parse::<i32>() {
                    pid = Some(p);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid = pid.expect("leader should have recorded its pid");

        // Cancel: SIGTERM (ignored) → then SIGKILL after the ~2s grace.
        handle.abort();
        tokio::time::sleep(Duration::from_millis(2800)).await;

        let alive = unsafe { libc::kill(pid, 0) } == 0;
        let _ = std::fs::remove_file(&pidfile);
        if alive {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        assert!(
            !alive,
            "SIGTERM-ignoring leader pid {pid} must be SIGKILLed by the escalation"
        );
    }
}
