//! `ssh` tool — secure remote transport (russh) for the installer agent.
//!
//! Action-dispatched: connect / exec / push / pull / disconnect. Unlike
//! `http_request`, this tool intentionally allows private/loopback hosts —
//! install targets are usually LAN addresses.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::traits::{Tool, ToolResult};
use crate::remote::session::{self, Auth};
use crate::security::SecurityPolicy;

const MAX_STREAM_CHARS: usize = 30_000;

/// Remote SSH transport tool.
pub struct SshTool {
    security: Arc<SecurityPolicy>,
}

impl SshTool {
    #[must_use]
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

fn fail(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(msg.into()),
    }
}

fn ok(output: String) -> ToolResult {
    ToolResult {
        success: true,
        output,
        error: None,
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_STREAM_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_STREAM_CHARS).collect();
    out.push_str("\n…[truncated]");
    out
}

fn str_field<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str)
}

fn parse_auth(args: &serde_json::Value) -> Result<Auth, String> {
    let auth = args
        .get("auth")
        .ok_or_else(|| "connect requires an `auth` object".to_string())?;
    let method = auth
        .get("method")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "auth.method is required (password|key|agent)".to_string())?;
    match method {
        "password" => {
            let p = auth
                .get("password")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "password auth requires auth.password".to_string())?;
            Ok(Auth::Password(p.to_string()))
        }
        "key" => {
            let path = auth.get("key_path").and_then(serde_json::Value::as_str);
            let pem = auth.get("key_pem").and_then(serde_json::Value::as_str);
            if path.is_none() && pem.is_none() {
                return Err("key auth requires auth.key_path or auth.key_pem".to_string());
            }
            Ok(Auth::Key {
                path: path.map(String::from),
                pem: pem.map(String::from),
                passphrase: auth
                    .get("passphrase")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from),
            })
        }
        "agent" => Ok(Auth::Agent),
        other => Err(format!("unknown auth.method `{other}`")),
    }
}

impl SshTool {
    async fn do_connect(args: &serde_json::Value) -> ToolResult {
        let Some(host) = str_field(args, "host") else {
            return fail("connect requires `host`");
        };
        let Some(user) = str_field(args, "user") else {
            return fail("connect requires `user`");
        };
        let port = u16::try_from(
            args.get("port")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(22),
        )
        .unwrap_or(22);
        let auth = match parse_auth(args) {
            Ok(a) => a,
            Err(e) => return fail(e),
        };
        match session::connect(host, port, user, auth).await {
            Ok(id) => ok(id),
            Err(e) => fail(format!("{e}")),
        }
    }

    async fn do_exec(args: &serde_json::Value) -> ToolResult {
        let (Some(id), Some(command)) = (str_field(args, "session"), str_field(args, "command"))
        else {
            return fail("exec requires `session` and `command`");
        };
        let timeout = args
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(120);
        match session::exec(id, command, timeout).await {
            Ok(out) => {
                let body = json!({
                    "rc": out.code,
                    "stdout": truncate(&out.stdout),
                    "stderr": truncate(&out.stderr),
                });
                ToolResult {
                    success: out.code == 0,
                    output: body.to_string(),
                    error: (out.code != 0).then(|| format!("remote exit code {}", out.code)),
                }
            }
            Err(e) => fail(format!("{e}")),
        }
    }

    async fn do_transfer(args: &serde_json::Value, push: bool) -> ToolResult {
        let (Some(id), Some(local), Some(remote)) = (
            str_field(args, "session"),
            str_field(args, "local_path"),
            str_field(args, "remote_path"),
        ) else {
            return fail("push/pull require `session`, `local_path`, `remote_path`");
        };
        let res = if push {
            session::push(id, local, remote).await
        } else {
            session::pull(id, remote, local).await
        };
        match res {
            Ok(()) => ok(format!(
                "{} ok: {} {} {}",
                if push { "push" } else { "pull" },
                local,
                if push { "->" } else { "<-" },
                remote
            )),
            Err(e) => fail(format!("{e}")),
        }
    }

    async fn do_disconnect(args: &serde_json::Value) -> ToolResult {
        let Some(id) = str_field(args, "session") else {
            return fail("disconnect requires `session`");
        };
        if session::disconnect(id).await {
            ok(format!("disconnected {id}"))
        } else {
            fail(format!("no such session {id}"))
        }
    }
}

#[async_trait]
impl Tool for SshTool {
    fn name(&self) -> &str {
        "ssh"
    }

    fn description(&self) -> &str {
        "Secure SSH transport to a remote host. Actions: connect (password|key|agent auth, \
         returns a session id), exec (run a command), push/pull (SFTP file transfer), disconnect. \
         Private/LAN hosts are allowed. Use this to reach install targets."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["connect", "exec", "push", "pull", "disconnect"],
                    "description": "Operation to perform"
                },
                "host": { "type": "string", "description": "Target host/IP (connect)" },
                "port": { "type": "integer", "description": "SSH port (connect, default 22)" },
                "user": { "type": "string", "description": "SSH username (connect)" },
                "auth": {
                    "type": "object",
                    "description": "Credentials (connect)",
                    "properties": {
                        "method": { "type": "string", "enum": ["password", "key", "agent"] },
                        "password": { "type": "string" },
                        "key_path": { "type": "string", "description": "Path to a private key file" },
                        "key_pem": { "type": "string", "description": "Inline private key PEM" },
                        "passphrase": { "type": "string", "description": "Key passphrase, if any" }
                    }
                },
                "session": { "type": "string", "description": "Session id from connect (exec/push/pull/disconnect)" },
                "command": { "type": "string", "description": "Command to run (exec)" },
                "timeout_secs": { "type": "integer", "description": "Exec timeout seconds (default 120)" },
                "local_path": { "type": "string", "description": "Local file path (push/pull)" },
                "remote_path": { "type": "string", "description": "Remote file path (push/pull)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(fail("Action blocked: autonomy is read-only"));
        }
        if !self.security.record_action() {
            return Ok(fail("Action blocked: rate limit exceeded"));
        }
        let Some(action) = str_field(&args, "action") else {
            return Ok(fail("missing `action`"));
        };
        let result = match action {
            "connect" => Self::do_connect(&args).await,
            "exec" => Self::do_exec(&args).await,
            "push" => Self::do_transfer(&args, true).await,
            "pull" => Self::do_transfer(&args, false).await,
            "disconnect" => Self::do_disconnect(&args).await,
            other => fail(format!("unknown action `{other}`")),
        };
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    fn tool(level: AutonomyLevel) -> SshTool {
        SshTool::new(Arc::new(SecurityPolicy {
            autonomy: level,
            ..SecurityPolicy::default()
        }))
    }

    #[test]
    fn schema_has_action_enum() {
        let t = tool(AutonomyLevel::Supervised);
        let s = t.parameters_schema();
        assert_eq!(s["properties"]["action"]["enum"][0], "connect");
        assert_eq!(s["required"][0], "action");
    }

    #[tokio::test]
    async fn readonly_blocks() {
        let t = tool(AutonomyLevel::ReadOnly);
        let r = t
            .execute(json!({"action": "exec", "session": "x", "command": "id"}))
            .await
            .unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn missing_action_fails() {
        let t = tool(AutonomyLevel::Full);
        let r = t.execute(json!({})).await.unwrap();
        assert!(!r.success);
    }

    #[tokio::test]
    async fn unknown_action_fails() {
        let t = tool(AutonomyLevel::Full);
        let r = t.execute(json!({"action": "frob"})).await.unwrap();
        assert!(!r.success);
        assert!(r.error.unwrap().contains("unknown action"));
    }

    #[test]
    fn parse_auth_variants() {
        assert!(matches!(
            parse_auth(&json!({"auth": {"method": "password", "password": "x"}})),
            Ok(Auth::Password(_))
        ));
        assert!(matches!(
            parse_auth(&json!({"auth": {"method": "key", "key_path": "/k"}})),
            Ok(Auth::Key { .. })
        ));
        assert!(parse_auth(&json!({"auth": {"method": "key"}})).is_err());
        assert!(parse_auth(&json!({"auth": {"method": "password"}})).is_err());
    }
}
