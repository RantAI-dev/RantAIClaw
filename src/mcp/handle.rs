//! Individual MCP server process handle — manages lifecycle of a single stdio-based MCP server.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", content = "error")]
pub enum McpStatus {
    Running,
    Stopped,
    Error(String),
}

pub struct McpHandle {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub process: Child,
    pub status: McpStatus,
    pub consecutive_failures: u32,
}

pub const MAX_CONSECUTIVE_FAILURES: u32 = 5;

impl McpHandle {
    #[allow(clippy::unused_async)]
    pub async fn spawn(
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new(&command);
        cmd.args(&args);
        crate::mcp::apply_hardened_env(&mut cmd, &env);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let process = cmd.spawn().with_context(|| {
            format!("Failed to spawn MCP server: {} {}", command, args.join(" "))
        })?;

        info!(
            "MCP server spawned: {} {} (pid: {:?})",
            command,
            args.join(" "),
            process.id()
        );

        Ok(Self {
            command,
            args,
            env,
            process,
            status: McpStatus::Running,
            consecutive_failures: 0,
        })
    }

    #[allow(clippy::unused_async)]
    pub async fn respawn(&mut self) -> Result<()> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args);
        crate::mcp::apply_hardened_env(&mut cmd, &self.env);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let process = cmd
            .spawn()
            .with_context(|| format!("Failed to respawn MCP server: {}", self.command))?;

        self.process = process;
        self.status = McpStatus::Running;
        self.consecutive_failures = 0;
        info!(
            "MCP server respawned: {} (pid: {:?})",
            self.command,
            self.process.id()
        );
        Ok(())
    }

    pub async fn kill(&mut self) -> Result<()> {
        self.process
            .kill()
            .await
            .context("Failed to kill MCP server process")?;
        self.status = McpStatus::Stopped;
        Ok(())
    }

    pub fn is_running(&mut self) -> bool {
        match self.process.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => false,
        }
    }

    pub fn is_failed(&self) -> bool {
        self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
    }

    pub fn record_failure(&mut self) -> bool {
        self.consecutive_failures += 1;
        if self.is_failed() {
            self.status = McpStatus::Error(format!(
                "Exceeded {} consecutive failures",
                MAX_CONSECUTIVE_FAILURES
            ));
            error!(
                "MCP server {} failed {} times, giving up",
                self.command, self.consecutive_failures
            );
            false
        } else {
            true
        }
    }

    pub fn reset_failures(&mut self) {
        self.consecutive_failures = 0;
        self.status = McpStatus::Running;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    /// RAII guard that restores an environment variable to its original state
    /// on drop, ensuring cleanup even if the test panics. Mirrors the
    /// equivalent guard in `src/tools/shell.rs`'s test module.
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

    #[tokio::test]
    async fn spawn_does_not_leak_daemon_secrets_into_mcp_child() {
        // Mutates process-global env — serialize against other env-mutating
        // tests across the crate (see `src/test_env.rs`).
        let _lock = crate::test_env::ENV_LOCK.lock().await;
        let _secret = EnvGuard::set("RANTAICLAW_TEST_SECRET", "leak-me-not");

        let mut env = HashMap::new();
        env.insert(
            "MCP_CONFIGURED_MARKER".to_string(),
            "configured-value".to_string(),
        );

        let mut handle = McpHandle::spawn(
            "sh".to_string(),
            vec!["-c".to_string(), "env".to_string()],
            env,
        )
        .await
        .expect("spawn should succeed");

        let mut stdout = handle
            .process
            .stdout
            .take()
            .expect("stdout should be piped");
        let mut output = String::new();
        stdout
            .read_to_string(&mut output)
            .await
            .expect("reading child stdout should succeed");
        let _ = handle.process.wait().await;

        assert!(
            !output.contains("leak-me-not"),
            "daemon secret leaked into MCP child env:\n{output}"
        );
        assert!(
            output.contains("MCP_CONFIGURED_MARKER=configured-value"),
            "configured env entry missing from MCP child env:\n{output}"
        );
        assert!(
            output.contains("PATH="),
            "allowlisted PATH missing from MCP child env:\n{output}"
        );
    }
}
