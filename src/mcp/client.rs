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
//! Threading model: one background task owns stdout and routes each
//! reply to the caller waiting on its id; callers only serialise on
//! stdin, which they hold just long enough to write one line. Readers
//! used to be whoever asked first — that caller read until it saw *its*
//! id and dropped everything else, so a second concurrent call had its
//! reply binned and waited out the full timeout. A second task drains
//! stderr: it is piped, and a server that logs past the pipe buffer
//! (~64 KiB) blocks on its own `write` and stops answering. Neither
//! shape had time to appear while a client lasted one chat request.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

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
    /// Callers waiting on a reply, keyed by request id. The reader task
    /// fulfils an entry and removes it; a caller that gives up removes its
    /// own. Dropping the map (reader exit) fails every waiter at once.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    request_id: AtomicU64,
    /// Aborted on drop so a client that goes away does not leave two tasks
    /// reading pipes whose child is being SIGKILLed underneath them.
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for McpClient {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
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
        let mut cmd = Command::new(command);
        cmd.args(args);
        crate::mcp::apply_hardened_env(&mut cmd, env);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
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
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("MCP server `{server_name}` missing stderr pipe"))?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let tasks = vec![
            tokio::spawn(pump_stdout(
                server_name.clone(),
                stdout,
                Arc::clone(&pending),
            )),
            tokio::spawn(drain_stderr(server_name.clone(), stderr)),
        ];

        let client = Self {
            server_name,
            _child: child,
            stdin: Mutex::new(stdin),
            pending,
            request_id: AtomicU64::new(1),
            tasks,
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

        // Register before writing: the reply can arrive while we still hold
        // the stdin lock, and a reply with nobody waiting is dropped.
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let sent = async {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(payload.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(e) = sent {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        let outcome = tokio::time::timeout(REQUEST_TIMEOUT, rx).await;
        // Whatever happened, this id is no longer wanted. A timed-out entry
        // left behind would pin a sender for the life of the client.
        self.pending.lock().await.remove(&id);

        let server = &self.server_name;
        match outcome {
            Ok(Ok(message)) => {
                if let Some(err) = message.get("error") {
                    anyhow::bail!("MCP `{server}` returned error for `{method}`: {err}");
                }
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            }
            // The reader dropped our sender: stdout closed, so the server is
            // gone. Fail now rather than waiting out the timeout on a pipe
            // nobody will ever write to.
            Ok(Err(_)) => {
                anyhow::bail!("MCP `{server}` server closed stdout before responding to `{method}`")
            }
            Err(_) => {
                anyhow::bail!("MCP `{server}` request `{method}` timeout after {REQUEST_TIMEOUT:?}")
            }
        }
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
}

/// Own stdout for the life of the connection and hand each reply to the caller
/// waiting on its id. Notifications and replies nobody is waiting for are
/// dropped here — which is correct, because the only reader is this task.
///
/// On EOF every remaining waiter is failed by dropping its sender, so a server
/// that dies mid-request fails its caller immediately instead of at the
/// 30-second timeout.
async fn pump_stdout(
    server: String,
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(target: "mcp", server = %server, error = %e, "stdout read failed");
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(line) {
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
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            // A notification, or a reply with no id — nothing to route.
            continue;
        };
        if let Some(waiter) = pending.lock().await.remove(&id) {
            // Send failing means the caller gave up (timeout); nothing to do.
            let _ = waiter.send(message);
        } else {
            tracing::debug!(target: "mcp", server = %server, id, "reply with no waiter");
        }
    }
    // Dropping the senders wakes every waiter with a receive error.
    pending.lock().await.clear();
}

/// Read stderr so the server never blocks writing to it.
///
/// The pipe holds ~64 KiB; a server that logs more than that with nobody
/// reading blocks on `write` and stops answering requests, while looking alive
/// to everything else. `npx` printing install progress is enough.
///
/// Logged at DEBUG, one line at a time and length-capped: server logs are
/// diagnostics worth having when a server misbehaves, but they are also
/// arbitrary output that may quote arguments, so they stay off by default.
async fn drain_stderr(server: String, stderr: ChildStderr) {
    /// Enough to identify a message; short enough that a runaway server cannot
    /// fill the log with one line.
    const MAX_LOGGED: usize = 512;
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let shown: String = line.chars().take(MAX_LOGGED).collect();
        tracing::debug!(target: "mcp", server = %server, "{shown}");
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
