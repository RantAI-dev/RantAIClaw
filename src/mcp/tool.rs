//! Bridges an MCP server's tool catalogue into the agent's `Tool`
//! registry. Each `McpTool` proxies `Tool::execute` to a `tools/call`
//! request on its underlying `McpClient`.
//!
//! Tool names are namespaced `mcp__<server>__<original>` so an MCP
//! server's `read_file` doesn't collide with a built-in `file_read`
//! (or with another MCP server's identically-named tool). The
//! double-underscore convention matches Claude Code's MCP tool
//! naming so users coming from there have one less surprise.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use super::client::{McpClient, McpToolInfo};
use crate::tools::traits::{Tool, ToolResult};

/// Build the qualified, agent-visible name for an MCP tool.
///
/// Public so the `/mcp` slash command can produce the same name the
/// agent sees without re-deriving it.
pub fn qualified_name(server: &str, tool: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("mcp__{}__{}", sanitize(server), sanitize(tool))
}

pub struct McpTool {
    client: Arc<McpClient>,
    qualified: String,
    description: String,
    parameters: Value,
    upstream_name: String,
}

impl McpTool {
    pub fn new(client: Arc<McpClient>, info: McpToolInfo) -> Self {
        let server = client.server_name().to_string();
        let qualified = qualified_name(&server, &info.name);
        let description = if info.description.is_empty() {
            format!("[mcp:{server}] (no description provided by server)")
        } else {
            format!("[mcp:{server}] {}", info.description)
        };
        // Empty schema is uncomfortable for native tool-calling
        // providers — they want at least `{type:"object"}`. Default
        // to the most permissive shape.
        let parameters = if info.input_schema.is_null()
            || info.input_schema.as_object().is_none_or(|o| o.is_empty())
        {
            serde_json::json!({"type": "object", "properties": {}})
        } else {
            info.input_schema
        };
        Self {
            client,
            qualified,
            description,
            parameters,
            upstream_name: info.name,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.parameters.clone()
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // MCP expects an object as `arguments`. Some LLMs may emit
        // `null` for parameterless tools — coerce.
        let normalised = match args {
            Value::Object(_) => args,
            Value::Null => serde_json::json!({}),
            other => serde_json::json!({"value": other}),
        };
        match self.client.call(&self.upstream_name, normalised).await {
            Ok(text) => Ok(ToolResult {
                success: true,
                output: text,
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_name_namespaces_server_and_tool() {
        assert_eq!(
            qualified_name("filesystem", "read_file"),
            "mcp__filesystem__read_file"
        );
    }

    #[test]
    fn qualified_name_sanitises_special_chars() {
        assert_eq!(
            qualified_name("my-server", "do.thing"),
            "mcp__my_server__do_thing"
        );
    }
}
