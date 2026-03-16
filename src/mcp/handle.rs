//! Individual MCP server process handle — manages lifecycle of a single stdio-based MCP server.

use std::collections::HashMap;
use std::process::Stdio;
use anyhow::{Context, Result};
use serde::{Serialize, Deserialize};
use tokio::process::{Child, Command};
use tracing::{info, error};

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
    pub async fn spawn(
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        let process = Command::new(&command)
            .args(&args)
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {} {}", command, args.join(" ")))?;

        info!("MCP server spawned: {} {} (pid: {:?})", command, args.join(" "), process.id());

        Ok(Self {
            command,
            args,
            env,
            process,
            status: McpStatus::Running,
            consecutive_failures: 0,
        })
    }

    pub async fn respawn(&mut self) -> Result<()> {
        let process = Command::new(&self.command)
            .args(&self.args)
            .envs(&self.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to respawn MCP server: {}", self.command))?;

        self.process = process;
        self.status = McpStatus::Running;
        self.consecutive_failures = 0;
        info!("MCP server respawned: {} (pid: {:?})", self.command, self.process.id());
        Ok(())
    }

    pub async fn kill(&mut self) -> Result<()> {
        self.process.kill().await.context("Failed to kill MCP server process")?;
        self.status = McpStatus::Stopped;
        Ok(())
    }

    pub fn is_running(&mut self) -> bool {
        match self.process.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }

    pub fn is_failed(&self) -> bool {
        self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
    }

    pub fn record_failure(&mut self) -> bool {
        self.consecutive_failures += 1;
        if self.is_failed() {
            self.status = McpStatus::Error(format!(
                "Exceeded {} consecutive failures", MAX_CONSECUTIVE_FAILURES
            ));
            error!("MCP server {} failed {} times, giving up", self.command, self.consecutive_failures);
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
