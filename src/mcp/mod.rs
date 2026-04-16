//! MCP server registry — manages lifecycle of multiple stdio-based MCP server processes.
//! Enforces a maximum of 10 concurrent servers per container.

pub mod handle;
pub mod supervisor;

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::config::schema::McpServerConfig;
pub use handle::{McpHandle, McpStatus};

const MAX_MCP_SERVERS: usize = 10;

pub struct McpRegistry {
    servers: HashMap<String, McpHandle>,
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    pub async fn add_server(&mut self, id: String, config: McpServerConfig) -> Result<()> {
        if self.servers.len() >= MAX_MCP_SERVERS {
            return Err(anyhow!(
                "MCP server limit reached (max {})",
                MAX_MCP_SERVERS
            ));
        }
        if self.servers.contains_key(&id) {
            return Err(anyhow!("MCP server '{}' already exists", id));
        }

        let handle = McpHandle::spawn(config.command, config.args, config.env).await?;
        self.servers.insert(id.clone(), handle);
        info!("MCP server '{}' added and running", id);
        Ok(())
    }

    pub async fn remove_server(&mut self, id: &str) -> Result<()> {
        let mut handle = self
            .servers
            .remove(id)
            .ok_or_else(|| anyhow!("MCP server '{}' not found", id))?;
        handle.kill().await?;
        info!("MCP server '{}' removed", id);
        Ok(())
    }

    pub async fn update_server(&mut self, id: String, config: McpServerConfig) -> Result<()> {
        let _ = self.remove_server(&id).await;
        self.add_server(id, config).await
    }

    pub fn list_servers(&self) -> HashMap<String, McpStatus> {
        self.servers
            .iter()
            .map(|(id, handle)| (id.clone(), handle.status.clone()))
            .collect()
    }

    pub fn server_ids(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    pub fn get_server_mut(&mut self, id: &str) -> Option<&mut McpHandle> {
        self.servers.get_mut(id)
    }

    pub async fn shutdown_all(&mut self) {
        let ids: Vec<String> = self.servers.keys().cloned().collect();
        for id in ids {
            if let Err(e) = self.remove_server(&id).await {
                warn!("Error shutting down MCP server '{}': {}", id, e);
            }
        }
    }

    pub fn count(&self) -> usize {
        self.servers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_config(cmd: &str) -> McpServerConfig {
        McpServerConfig {
            command: cmd.to_string(),
            args: vec!["--version".to_string()],
            env: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn test_add_duplicate_server() {
        let mut registry = McpRegistry::new();
        // Use "echo" which exists on Linux and exits immediately
        let _ = registry
            .add_server("test".to_string(), test_config("echo"))
            .await;
        let result = registry
            .add_server("test".to_string(), test_config("echo"))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_remove_nonexistent_server() {
        let mut registry = McpRegistry::new();
        let result = registry.remove_server("nonexistent").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_server_count() {
        let mut registry = McpRegistry::new();
        assert_eq!(registry.count(), 0);
        let _ = registry
            .add_server("s1".to_string(), test_config("echo"))
            .await;
        assert_eq!(registry.count(), 1);
    }

    #[tokio::test]
    async fn test_list_servers() {
        let mut registry = McpRegistry::new();
        let _ = registry
            .add_server("s1".to_string(), test_config("echo"))
            .await;
        let servers = registry.list_servers();
        assert!(servers.contains_key("s1"));
    }
}
