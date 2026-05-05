//! MCP provisioner — implements [`TuiProvisioner`] for in-TUI MCP server setup.
//!
//! Mirrors the legacy flow in [`crate::onboard::section::mcp`]:
//!   1. Multi-select from curated MCP servers
//!   2. Per-auth-required server: prompt for token
//!   3. Option to install all zero-auth servers
//!   4. Custom MCP server command (optional)
//!
//! Config writes: `config.mcp_servers`

use super::traits::{ProvisionEvent, ProvisionIo, ProvisionResponse, Severity, TuiProvisioner};
use crate::config::Config;
use crate::mcp::curated::{self, CuratedMcpServer, AUTHED, NO_AUTH};
use crate::mcp::setup;
use crate::profile::Profile;
use anyhow::Result;
use async_trait::async_trait;

pub const MCP_NAME: &str = "mcp";
pub const MCP_DESC: &str = "Add curated MCP servers (Notion, Slack, GitHub, …)";

#[derive(Debug, Clone)]
pub struct McpProvisioner;

impl McpProvisioner {
    pub fn new() -> Self {
        Self
    }
}

impl Default for McpProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TuiProvisioner for McpProvisioner {
    fn name(&self) -> &'static str {
        MCP_NAME
    }

    fn description(&self) -> &'static str {
        MCP_DESC
    }

    async fn run(&self, config: &mut Config, profile: &Profile, io: ProvisionIo) -> Result<()> {
        let ProvisionIo {
            events,
            mut responses,
        } = io;

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: "Let's add MCP servers to extend your agent's capabilities.".into(),
            },
        )
        .await?;

        // Build the full option list: zero-auth first, then authed
        let mut all_options: Vec<String> = Vec::new();
        // Map global index -> server
        let mut server_by_idx: Vec<(&CuratedMcpServer, bool)> = Vec::new(); // (server, is_authed)

        for s in NO_AUTH.iter() {
            all_options.push(format!("{} (no auth)", s.display_name));
            server_by_idx.push((s, false));
        }
        for s in AUTHED.iter() {
            all_options.push(format!("{} (auth required)", s.display_name));
            server_by_idx.push((s, true));
        }

        send(
            &events,
            ProvisionEvent::Message {
                severity: Severity::Info,
                text: format!(
                    "Available: {} curated servers ({} require auth)",
                    server_by_idx.len(),
                    AUTHED.len()
                ),
            },
        )
        .await?;

        send(
            &events,
            ProvisionEvent::Choose {
                id: "servers".into(),
                label: "Select MCP servers to install (multi-select)".into(),
                options: all_options,
                multi: true,
            },
        )
        .await?;

        let selections = recv_selection_multi(&mut responses).await?;

        // Prompt for auth tokens for selected authed servers
        let mut env_pairs: Vec<(String, String)> = Vec::new();

        for (global_idx, server) in server_by_idx.iter().enumerate() {
            if !server.1 {
                continue; // skip zero-auth
            }
            if !selections.contains(&global_idx) {
                continue;
            }
            if let crate::mcp::curated::AuthMethod::Token { secret_key, hint } = server.0.auth {
                send(
                    &events,
                    ProvisionEvent::Prompt {
                        id: format!("token_{}", server.0.slug),
                        label: format!("{} — {}", server.0.display_name, hint),
                        default: None,
                        secret: true,
                    },
                )
                .await?;
                let token = recv_text(&mut responses).await?;
                if !token.trim().is_empty() {
                    env_pairs.push((secret_key.to_string(), token));
                }
            }
        }

        // Ask about zero-auth servers
        send(
            &events,
            ProvisionEvent::Choose {
                id: "install_zero_auth".into(),
                label: "Install all zero-auth servers?".into(),
                options: vec![
                    "Yes — install all zero-auth servers".to_string(),
                    "No — only selected".to_string(),
                ],
                multi: false,
            },
        )
        .await?;

        let install_all_zero_auth = {
            let sel = recv_selection(&mut responses).await?;
            sel.first().copied() == Some(0)
        };

        // Custom MCP server
        send(
            &events,
            ProvisionEvent::Prompt {
                id: "custom_command".into(),
                label: "Custom MCP server command (Enter to skip)".into(),
                default: None,
                secret: false,
            },
        )
        .await?;

        let custom_cmd = recv_text(&mut responses).await?;

        // Register selected authed servers
        let mut authed_added = 0;
        for (global_idx, server) in server_by_idx.iter().enumerate() {
            if !server.1 {
                continue;
            }
            if selections.contains(&global_idx) {
                let env: Vec<(String, String)> = env_pairs
                    .iter()
                    .filter(|(k, _)| server.0.env_vars.contains(&k.as_str()))
                    .cloned()
                    .collect();
                let _ = setup::register_mcp(config, server.0, &env);
                authed_added += 1;
            }
        }

        // Register zero-auth servers
        let mut zero_auth_added = 0;
        let all_zero_auth_indices: Vec<usize> =
            NO_AUTH.iter().enumerate().map(|(i, _)| i).collect();

        for (zero_idx, server) in NO_AUTH.iter().enumerate() {
            let global_idx = zero_idx;
            if selections.contains(&global_idx) || install_all_zero_auth {
                let _ = setup::register_mcp(config, server, &[]);
                zero_auth_added += 1;
            }
        }

        // Custom server
        let mut custom_added = false;
        if !custom_cmd.trim().is_empty() {
            let (cmd, args) = split_custom_command(custom_cmd.trim());
            let cfg = crate::config::schema::McpServerConfig {
                command: cmd,
                args,
                env: std::collections::HashMap::new(),
            };
            let key = format!("custom_{}", env_pairs.len());
            config.mcp_servers.insert(key, cfg);
            custom_added = true;
        }

        let total = authed_added + zero_auth_added + if custom_added { 1 } else { 0 };
        send(
            &events,
            ProvisionEvent::Done {
                summary: format!(
                    "MCP servers registered: {} total ({} authed, {} zero-auth{}{})",
                    total,
                    authed_added,
                    zero_auth_added,
                    if custom_added { ", 1 custom" } else { "" },
                    if install_all_zero_auth && !selections.is_empty() {
                        " (all zero-auth)"
                    } else {
                        ""
                    }
                ),
            },
        )
        .await?;

        Ok(())
    }
}

fn split_custom_command(cmd: &str) -> (String, Vec<String>) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts.split_first() {
        Some((head, tail)) => (
            head.to_string(),
            tail.iter().map(|s| s.to_string()).collect(),
        ),
        None => (String::new(), Vec::new()),
    }
}

async fn send(
    events: &tokio::sync::mpsc::Sender<ProvisionEvent>,
    ev: ProvisionEvent,
) -> Result<()> {
    events
        .send(ev)
        .await
        .map_err(|e| anyhow::anyhow!("send failed: {e}"))
}

async fn recv_selection(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    match responses.recv().await {
        Some(ProvisionResponse::Selection(indices)) => Ok(indices),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

async fn recv_selection_multi(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<Vec<usize>> {
    recv_selection(responses).await
}

async fn recv_text(
    responses: &mut tokio::sync::mpsc::Receiver<ProvisionResponse>,
) -> Result<String> {
    match responses.recv().await {
        Some(ProvisionResponse::Text(t)) => Ok(t),
        Some(ProvisionResponse::Cancelled) => anyhow::bail!("cancelled"),
        Some(_) => anyhow::bail!("unexpected response"),
        None => anyhow::bail!("channel closed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioner_name_is_mcp() {
        let p = McpProvisioner::new();
        assert_eq!(p.name(), "mcp");
    }

    #[test]
    fn provisioner_description_is_non_empty() {
        let p = McpProvisioner::new();
        assert!(!p.description().is_empty());
    }
}
