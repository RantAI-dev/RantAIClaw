//! Spawn every configured MCP server and collect its tools into a
//! `Vec<Box<dyn Tool>>` suitable for splicing into the agent's tool
//! registry.
//!
//! Failure to connect or list tools is **non-fatal**: the offending
//! server is logged and skipped, the agent keeps booting. This is
//! deliberate — a misconfigured MCP server should never block the
//! user from chatting with the agent. The failure shows up later in
//! `/mcp` (the slash command renders status per server).

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::schema::McpServerConfig;
use crate::tools::traits::Tool;

use super::client::McpClient;
use super::tool::McpTool;

/// One-line outcome per server, returned alongside the tools so the
/// `/mcp` slash command can render server health without having to
/// re-probe. Carried as a sibling to the tool registry on the agent.
#[derive(Debug, Clone)]
pub struct McpServerHealth {
    pub name: String,
    pub status: McpHealthStatus,
    pub tool_count: usize,
}

#[derive(Debug, Clone)]
pub enum McpHealthStatus {
    /// Connected, handshake completed, tools discovered.
    Healthy,
    /// Spawn / handshake / tools/list failed. Carries the error.
    Failed(String),
}

#[derive(Default)]
pub struct McpDiscovery {
    pub tools: Vec<Box<dyn Tool>>,
    pub health: Vec<McpServerHealth>,
    /// Live client handles. Held so the underlying child processes
    /// stay alive for as long as the agent does. Each `McpTool` also
    /// holds an `Arc<McpClient>`, but pinning them here too keeps a
    /// stable lookup for `/mcp` and future hot-disconnect support.
    pub clients: HashMap<String, Arc<McpClient>>,
}

/// Spawn every server in `servers`, list its tools, build the
/// agent-side registry slice. Skips silently when the map is empty.
pub async fn discover_mcp_tools(servers: &HashMap<String, McpServerConfig>) -> McpDiscovery {
    let mut out = McpDiscovery::default();
    if servers.is_empty() {
        return out;
    }
    for (name, cfg) in servers {
        match McpClient::connect(name.clone(), &cfg.command, &cfg.args, &cfg.env).await {
            Ok(client) => {
                let client = Arc::new(client);
                match client.list_tools().await {
                    Ok(infos) => {
                        let tool_count = infos.len();
                        for info in infos {
                            out.tools.push(Box::new(McpTool::new(client.clone(), info)));
                        }
                        tracing::info!(
                            target: "mcp",
                            server = %name,
                            tool_count,
                            "registered MCP tools"
                        );
                        out.health.push(McpServerHealth {
                            name: name.clone(),
                            status: McpHealthStatus::Healthy,
                            tool_count,
                        });
                        out.clients.insert(name.clone(), client);
                    }
                    Err(e) => {
                        tracing::warn!(target: "mcp", server = %name, error = %e, "tools/list failed");
                        out.health.push(McpServerHealth {
                            name: name.clone(),
                            status: McpHealthStatus::Failed(format!("tools/list: {e}")),
                            tool_count: 0,
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!(target: "mcp", server = %name, error = %e, "connect failed");
                out.health.push(McpServerHealth {
                    name: name.clone(),
                    status: McpHealthStatus::Failed(format!("connect: {e}")),
                    tool_count: 0,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_config_returns_empty_discovery() {
        let servers: HashMap<String, McpServerConfig> = HashMap::new();
        let out = discover_mcp_tools(&servers).await;
        assert!(out.tools.is_empty());
        assert!(out.health.is_empty());
        assert!(out.clients.is_empty());
    }

    #[tokio::test]
    async fn nonexistent_command_records_failure_without_panicking() {
        let mut servers = HashMap::new();
        servers.insert(
            "broken".to_string(),
            McpServerConfig {
                command: "/this/does/not/exist".into(),
                args: vec![],
                env: HashMap::new(),
            },
        );
        let out = discover_mcp_tools(&servers).await;
        assert!(out.tools.is_empty());
        assert_eq!(out.health.len(), 1);
        assert_eq!(out.health[0].name, "broken");
        match &out.health[0].status {
            McpHealthStatus::Failed(msg) => assert!(msg.contains("connect")),
            _ => panic!("expected Failed status"),
        }
    }
}
