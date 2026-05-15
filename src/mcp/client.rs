//! Stdio JSON-RPC client for Model Context Protocol servers.
//!
//! Pairs with `handle.rs` (process lifecycle) and `supervisor.rs`
//! (crash recovery). This module owns the **protocol** — sending
//! `initialize`, `tools/list`, `tools/call` over the child's stdin
//! and matching responses by id on stdout.
//!
//! Wire format per MCP spec: newline-delimited JSON-RPC 2.0
//! messages. No Content-Length header (HTTP/SSE transport uses
//! that, stdio doesn't). Each request is exactly one line.
//!
//! Threading model: one `Mutex` over stdin (writer), one over stdout
//! (reader). Multiple concurrent `request()` callers each take the
//! stdout lock and read until they find their matching id —
//! responses for other ids are dropped. This is acceptable because
//! MCP tool-call traffic is low-frequency (a few calls per turn).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// MCP protocol version we speak. The 2024-11-05 spec is widely
/// supported by official servers (`@modelcontextprotocol/server-*`).
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// How long to wait for any single request (handshake or tool call)
/// before giving up. Most MCP tool calls return in <1s; 30s is the
/// outer envelope for slow filesystem / network operations.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One tool exposed by a connected MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// Live connection to one MCP server.
///
/// Wrapped in `Arc` across `McpTool` instances so dropping the agent
/// (and therefore all tools) terminates the child process via
/// `kill_on_drop(true)`.
pub struct McpClient {
    server_name: String,
    /// Owns the child process. Drop = SIGKILL (kill_on_drop set at spawn).
    _child: Child,
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
    request_id: AtomicU64,
}

impl McpClient {
    /// Spawn an MCP server and complete the `initialize` handshake.
    pub async fn connect(
        server_name: impl Into<String>,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        let server_name = server_name.into();
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn MCP server `{server_name}` ({command})"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP server `{server_name}` missing stdin pipe"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP server `{server_name}` missing stdout pipe"))?;

        let client = Self {
            server_name,
            _child: child,
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(BufReader::new(stdout)),
            request_id: AtomicU64::new(1),
        };

        client
            .initialize_handshake()
            .await
            .context("MCP initialize handshake failed")?;
        Ok(client)
    }

    /// Send `initialize` + `notifications/initialized`. Required
    /// before any other request per spec.
    async fn initialize_handshake(&self) -> Result<()> {
        let _server_caps = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "rantaiclaw",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    /// Query the server's tool catalogue.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let resp = self.request("tools/list", json!({})).await?;
        let tools = resp
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(tools.len());
        for entry in tools {
            match serde_json::from_value::<McpToolInfo>(entry.clone()) {
                Ok(info) if !info.name.is_empty() => out.push(info),
                Ok(_) => {
                    tracing::warn!(target: "mcp", server = %self.server_name, "tools/list entry missing name");
                }
                Err(e) => tracing::warn!(
                    target: "mcp",
                    server = %self.server_name,
                    error = %e,
                    raw = %entry,
                    "tools/list entry failed to parse"
                ),
            }
        }
        Ok(out)
    }

    /// Invoke a tool. Concatenates `text`-typed content blocks from
    /// the response into a single string suitable for `ToolResult.output`.
    pub async fn call(&self, tool: &str, arguments: Value) -> Result<String> {
        let resp = self
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await?;

        if resp
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let detail = extract_text(&resp);
            anyhow::bail!(
                "MCP `{}` tool `{tool}` returned error: {detail}",
                self.server_name
            );
        }

        Ok(extract_text(&resp))
    }

    /// Server identity used in tool names + log lines.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let payload = serde_json::to_string(&req)?;
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(payload.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
        }
        self.read_response_for(id, method).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let payload = serde_json::to_string(&req)?;
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(payload.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn read_response_for(&self, want_id: u64, method: &str) -> Result<Value> {
        let server = self.server_name.clone();
        let read = async {
            let mut stdout = self.stdout.lock().await;
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = stdout
                    .read_line(&mut buf)
                    .await
                    .with_context(|| format!("MCP `{server}` stdout read failed"))?;
                if n == 0 {
                    anyhow::bail!(
                        "MCP `{server}` server closed stdout before responding to `{method}`"
                    );
                }
                let line = buf.trim();
                if line.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            target: "mcp",
                            server = %server,
                            line = %line,
                            error = %e,
                            "skipping unparseable line"
                        );
                        continue;
                    }
                };

                // Match by id; drop notifications / unrelated responses.
                match v.get("id").and_then(|x| x.as_u64()) {
                    Some(id) if id == want_id => {
                        if let Some(err) = v.get("error") {
                            return Err(anyhow!(
                                "MCP `{server}` returned error for `{method}`: {err}"
                            ));
                        }
                        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                    }
                    _ => continue,
                }
            }
        };
        tokio::time::timeout(REQUEST_TIMEOUT, read)
            .await
            .with_context(|| {
                format!(
                    "MCP `{}` request `{method}` timeout after {:?}",
                    self.server_name, REQUEST_TIMEOUT
                )
            })?
    }
}

/// Extract concatenated text from a `tools/call` response. MCP
/// allows `content: [{type:"text", text:...}, {type:"image",...}]`
/// — we only handle text in this slice (image/audio rendering is
/// out of scope; future PR).
fn extract_text(response: &Value) -> String {
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let parts: Vec<String> = content
        .iter()
        .filter_map(|c| {
            let kind = c.get("type").and_then(Value::as_str)?;
            if kind == "text" {
                c.get("text").and_then(Value::as_str).map(String::from)
            } else {
                Some(format!("[mcp:{kind} content omitted]"))
            }
        })
        .collect();
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_concatenates_text_blocks() {
        let resp = json!({
            "content": [
                {"type": "text", "text": "Hello"},
                {"type": "text", "text": "world"},
            ]
        });
        assert_eq!(extract_text(&resp), "Hello\nworld");
    }

    #[test]
    fn extract_text_marks_non_text_blocks() {
        let resp = json!({
            "content": [
                {"type": "text", "text": "ok"},
                {"type": "image", "data": "..."},
            ]
        });
        let out = extract_text(&resp);
        assert!(out.contains("ok"));
        assert!(out.contains("[mcp:image content omitted]"));
    }

    #[test]
    fn extract_text_empty_when_no_content() {
        assert_eq!(extract_text(&json!({})), "");
        assert_eq!(extract_text(&json!({"content": []})), "");
    }

    #[test]
    fn mcp_tool_info_parses_minimal_entry() {
        let v = json!({
            "name": "read_file",
            "description": "Read a file",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}
        });
        let info: McpToolInfo = serde_json::from_value(v).unwrap();
        assert_eq!(info.name, "read_file");
        assert_eq!(info.description, "Read a file");
        assert!(info.input_schema.get("properties").is_some());
    }

    #[test]
    fn mcp_tool_info_tolerates_missing_description() {
        let v = json!({"name": "ping"});
        let info: McpToolInfo = serde_json::from_value(v).unwrap();
        assert_eq!(info.name, "ping");
        assert!(info.description.is_empty());
    }
}
