//! Spawn every configured MCP server and collect its tools into a
//! `Vec<Box<dyn Tool>>` suitable for splicing into the agent's tool
//! registry.
//!
//! Two lifetimes live here. [`discover_mcp_tools`] pins its servers to one
//! agent — right for the TUI and CLI, where the agent *is* the session. The
//! gateway builds an agent per chat request, so that shape made every console
//! turn pay spawn + handshake + `tools/list` for every server and then SIGKILL
//! them; [`McpPool`] is the same discovery with the ownership moved to
//! whatever owns the gateway's lifetime.
//!
//! A pool is invalidated by its own inputs: [`McpPoolHandle::current`] compares
//! the `mcp_servers` it is handed against the map the pool was built from and
//! reconnects when they differ, so a hot-reloaded config takes effect on the
//! next turn. Callers hold an `Arc<McpPool>` for the duration of their turn, so
//! a rebuild never pulls a client out from under an in-flight tool call: the
//! old pool (and its processes) drops once the last turn holding it finishes.
//!
//! Failure to connect or list tools is **non-fatal**: the offending
//! server is logged and skipped, the agent keeps booting. This is
//! deliberate — a misconfigured MCP server should never block the
//! user from chatting with the agent. The failure shows up later in
//! `/mcp` (the slash command renders status per server).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::config::schema::McpServerConfig;
use crate::tools::traits::Tool;

use super::client::{McpClient, McpToolInfo};
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

/// A set of MCP servers connected once and reused across turns.
///
/// Holds the live clients plus the `tools/list` answers they gave, so each turn
/// can be handed a fresh `Vec<Box<dyn Tool>>` (tools are boxed trait objects
/// and cannot be cloned) without touching the wire. Dropping the pool drops the
/// clients, which SIGKILLs the child processes via `kill_on_drop`.
#[derive(Default)]
pub struct McpPool {
    /// The config this pool was built from — the thing that invalidates it.
    servers: HashMap<String, McpServerConfig>,
    clients: HashMap<String, Arc<McpClient>>,
    /// `(server, tool)` in discovery order, so `tools()` rebuilds a registry
    /// slice identical to the one `discover_mcp_tools` would have produced.
    tool_infos: Vec<(String, McpToolInfo)>,
    health: Vec<McpServerHealth>,
}

impl McpPool {
    /// Connect every server in `servers`. Failures are non-fatal and land in
    /// [`health`](Self::health), exactly as in [`discover_mcp_tools`].
    pub async fn connect(servers: &HashMap<String, McpServerConfig>) -> Self {
        let mut pool = Self {
            servers: servers.clone(),
            ..Self::default()
        };
        for (name, cfg) in servers {
            match McpClient::connect(name.clone(), &cfg.command, &cfg.args, &cfg.env).await {
                Ok(client) => {
                    let client = Arc::new(client);
                    match client.list_tools().await {
                        Ok(infos) => {
                            let tool_count = infos.len();
                            for info in infos {
                                pool.tool_infos.push((name.clone(), info));
                            }
                            tracing::info!(
                                target: "mcp",
                                server = %name,
                                tool_count,
                                "pooled MCP server"
                            );
                            pool.health.push(McpServerHealth {
                                name: name.clone(),
                                status: McpHealthStatus::Healthy,
                                tool_count,
                            });
                            pool.clients.insert(name.clone(), client);
                        }
                        Err(e) => {
                            tracing::warn!(target: "mcp", server = %name, error = %e, "tools/list failed");
                            pool.health.push(McpServerHealth {
                                name: name.clone(),
                                status: McpHealthStatus::Failed(format!("tools/list: {e}")),
                                tool_count: 0,
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "mcp", server = %name, error = %e, "connect failed");
                    pool.health.push(McpServerHealth {
                        name: name.clone(),
                        status: McpHealthStatus::Failed(format!("connect: {e}")),
                        tool_count: 0,
                    });
                }
            }
        }
        pool
    }

    /// A registry slice for one turn. Cheap: each tool is a new `McpTool` over
    /// an `Arc` of the already-connected client.
    #[must_use]
    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.tool_infos
            .iter()
            .filter_map(|(server, info)| {
                let client = self.clients.get(server)?;
                Some(Box::new(McpTool::new(Arc::clone(client), info.clone())) as Box<dyn Tool>)
            })
            .collect()
    }

    /// Per-server outcome, for `/mcp` and the API's server list.
    #[must_use]
    pub fn health(&self) -> &[McpServerHealth] {
        &self.health
    }

    /// Qualified tool names keyed by server, mirroring `health`.
    #[must_use]
    pub fn tools_by_server(&self) -> HashMap<String, Vec<String>> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for (server, info) in &self.tool_infos {
            out.entry(server.clone())
                .or_default()
                .push(format!("mcp__{server}__{}", info.name));
        }
        out
    }

    /// Was this pool built from exactly these servers?
    #[must_use]
    pub fn matches(&self, servers: &HashMap<String, McpServerConfig>) -> bool {
        &self.servers == servers
    }
}

/// The gateway's handle on the pool: hands out the current one and rebuilds it
/// when the configured servers change.
///
/// Lazy on purpose. Connecting at gateway start would make a slow or broken
/// server delay the port opening, and the pool would still need this check
/// afterwards for hot-reload — so there is one code path, taken on the first
/// turn that needs MCP at all.
#[derive(Default)]
pub struct McpPoolHandle {
    inner: RwLock<Arc<McpPool>>,
}

impl McpPoolHandle {
    /// The pool for `servers`, reconnecting first if the previous one was built
    /// from a different config.
    pub async fn current(&self, servers: &HashMap<String, McpServerConfig>) -> Arc<McpPool> {
        {
            let current = self.inner.read().await;
            if current.matches(servers) {
                return Arc::clone(&current);
            }
        }
        let mut slot = self.inner.write().await;
        // Re-check under the write lock: another turn may have rebuilt it while
        // this one waited, and a second rebuild would spawn a second set of
        // processes for nothing.
        if slot.matches(servers) {
            return Arc::clone(&slot);
        }
        let rebuilt = Arc::new(McpPool::connect(servers).await);
        *slot = Arc::clone(&rebuilt);
        rebuilt
    }
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
