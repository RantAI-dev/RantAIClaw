//! Bridges an MCP server's tool catalogue into the agent's `Tool`
//! registry. Each `McpTool` proxies `Tool::execute` to a `tools/call`
//! request on its underlying `McpClient`.
//!
//! Tool names are namespaced `mcp__<server>__<original>` so an MCP
//! server's `read_file` doesn't collide with a built-in `file_read`
//! (or with another MCP server's identically-named tool). The
//! double-underscore convention matches Claude Code's MCP tool
//! naming so users coming from there have one less surprise.

use std::fmt::Write;
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

/// Largest MCP tool output that reaches the agent, in bytes. Same value as
/// `MAX_OUTPUT_BYTES` in `src/tools/shell.rs` (private to that module, so it
/// is repeated here); keep the two in step.
pub(crate) const MAX_MCP_OUTPUT_BYTES: usize = 1_048_576;

/// Keep at most `MAX_MCP_OUTPUT_BYTES` of a successful tool's text, cut on a
/// char boundary, and say how much was dropped.
fn cap_output(mut text: String) -> String {
    if text.len() > MAX_MCP_OUTPUT_BYTES {
        let keep = text.floor_char_boundary(MAX_MCP_OUTPUT_BYTES);
        let dropped = text.len() - keep;
        text.truncate(keep);
        let _ = write!(text, "\n[mcp output truncated: {dropped} bytes dropped]");
    }
    text
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
                output: cap_output(text),
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

    /// Fake stdio server (POSIX sh, as in `tests/mcp_client_transport.rs`):
    /// answers `initialize`/`tools/list` like a normal server, and every
    /// `tools/call` with one text block of `$TEXT_BYTES` bytes of `a`.
    fn text_server(text_bytes: usize) -> String {
        format!(
            r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *initialize*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"capabilities":{{}}}}}}\n' "$id" ;;
    *tools/list*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[]}}}}\n' "$id" ;;
    *) big=$(head -c {text_bytes} /dev/zero | tr '\0' a)
       printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"%s"}}]}}}}\n' "$id" "$big" ;;
  esac
done
"#
        )
    }

    async fn tool_over(script: &str) -> McpTool {
        let client = crate::mcp::client::McpClient::connect(
            "fixture",
            "sh",
            &["-c".to_string(), script.to_string()],
            &std::collections::HashMap::new(),
        )
        .await
        .expect("fixture server connects");
        McpTool::new(
            Arc::new(client),
            McpToolInfo {
                name: "echo".into(),
                description: String::new(),
                input_schema: Value::Null,
            },
        )
    }

    #[tokio::test]
    async fn output_over_one_mib_is_cut_with_a_marker() {
        let tool = tool_over(&text_server(2 * 1024 * 1024)).await;
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        let marker = "[mcp output truncated: ";
        let at = result.output.find(marker).expect("cut marker present");
        assert!(
            result.output.len() <= MAX_MCP_OUTPUT_BYTES + 200,
            "kept {} bytes",
            result.output.len()
        );
        assert!(
            result.output[at..].contains("bytes dropped"),
            "{}",
            &result.output[at..]
        );
        assert!(result.output.starts_with("aaaa"));
    }

    #[tokio::test]
    async fn output_under_the_cap_is_unchanged() {
        let tool = tool_over(&text_server(10)).await;
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert_eq!(result.output, "a".repeat(10));
        assert!(result.success);
    }
}
