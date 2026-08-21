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

    /// `ssh exec` runs an arbitrary remote command with no allowlist, so — like
    /// `pty` — the only gate is a human decision. Full autonomy trusts the
    /// agent; otherwise ask the approval backend. Returns `Some(refusal)` if the
    /// command must not run, or `None` to proceed.
    async fn require_command_approval(&self, label: &str, command: &str) -> Option<ToolResult> {
        if self.security.effective_autonomy() == crate::security::AutonomyLevel::Full {
            return None;
        }
        let Some(approvals) = self.security.pending() else {
            return Some(fail(format!(
                "Action blocked: `{label}` runs an arbitrary command and needs approval, but no \
                 interactive approver is available. Use an interactive session, or \
                 `rantaiclaw autonomy full` in a trusted environment."
            )));
        };
        let (channel, reply_target) = crate::security::current_turn_scope();
        match approvals
            .request_decision_in(
                uuid::Uuid::new_v4(),
                label.to_string(),
                command.to_string(),
                channel,
                reply_target,
            )
            .await
        {
            crate::security::Decision::Deny => {
                Some(fail(format!("`{label}` denied by the operator")))
            }
            _ => None,
        }
    }

    /// Confine a local file-transfer path to the workspace + forbidden-path
    /// policy, exactly as the file tools do — so `ssh push local_path=~/.ssh/id_rsa`
    /// can't exfiltrate a host file and `pull` can't overwrite one outside the
    /// workspace. Returns `Some(refusal)` if the path is out of bounds.
    fn require_local_path_allowed(&self, local: &str) -> Option<ToolResult> {
        if !self.security.is_path_allowed(local) {
            return Some(fail(format!(
                "Action blocked: local path `{local}` is outside the workspace or is a forbidden path"
            )));
        }
        // Post-canonicalization containment (symlink escape). `pull` may target a
        // not-yet-existing file (canonicalize fails) — then the lexical check
        // above is the guard.
        if let Ok(canon) = std::path::Path::new(local).canonicalize() {
            if !self.security.is_resolved_path_allowed(&canon) {
                return Some(fail(format!(
                    "Action blocked: local path `{local}` resolves outside the workspace"
                )));
            }
        }
        None
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
        // `exec` runs an arbitrary remote command — require a human decision.
        if action == "exec" {
            if let Some(command) = str_field(&args, "command") {
                if let Some(refusal) = self.require_command_approval("ssh exec", command).await {
                    return Ok(refusal);
                }
            }
        }
        // `push`/`pull` read/write a local path — confine it to the workspace.
        if matches!(action, "push" | "pull") {
            if let Some(local) = str_field(&args, "local_path") {
                if let Some(refusal) = self.require_local_path_allowed(local) {
                    return Ok(refusal);
                }
            }
        }
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
        SshTool::new(Arc::new(SecurityPolicy::default().with_autonomy(level)))
    }

    #[tokio::test]
    async fn exec_without_an_approver_is_refused() {
        // Supervised + no approval backend attached → an arbitrary remote
        // command must be refused, not run unattended.
        let t = tool(AutonomyLevel::Supervised);
        let res = t
            .execute(json!({"action": "exec", "session": "nope", "command": "id"}))
            .await
            .unwrap();
        assert!(!res.success, "ssh exec must not run without an approver");
        assert!(res.error.unwrap_or_default().contains("approver"));
    }

    #[tokio::test]
    async fn push_of_a_forbidden_local_path_is_refused() {
        // A host file outside the workspace must not be exfiltrated via push.
        let t = tool(AutonomyLevel::Supervised);
        let res = t
            .execute(json!({
                "action": "push",
                "session": "nope",
                "local_path": "/etc/hosts",
                "remote_path": "/tmp/x"
            }))
            .await
            .unwrap();
        assert!(!res.success, "ssh push of a forbidden path must be refused");
        assert!(res
            .error
            .unwrap_or_default()
            .to_lowercase()
            .contains("path"));
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
