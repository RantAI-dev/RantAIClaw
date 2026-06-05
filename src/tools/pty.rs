//! `pty` tool — drive an interactive TUI installer over tmux.
//!
//! The installer runs inside a detached tmux session on the target (local or an
//! open `ssh` session), so it gets a real PTY and behaves interactively while we
//! drive it with `tmux send-keys` / `capture-pane`. No terminal emulator needed.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;

use super::traits::{Tool, ToolResult};
use crate::remote::keys::{parse_key_tokens, strip_ansi, tmux_send_batches};
use crate::remote::{keys::frames_stable, session};
use crate::security::SecurityPolicy;

/// Where a tmux session runs.
#[derive(Clone)]
enum Target {
    Local,
    Ssh(String),
}

fn targets() -> &'static Mutex<HashMap<String, Target>> {
    static M: OnceLock<Mutex<HashMap<String, Target>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_target(session: &str, target: Target) {
    targets()
        .lock()
        .expect("tmux target lock")
        .insert(session.to_string(), target);
}

fn lookup_target(session: &str) -> Option<Target> {
    targets().lock().expect("tmux target lock").get(session).cloned()
}

fn forget_target(session: &str) {
    targets().lock().expect("tmux target lock").remove(session);
}

// --- pure command builders (unit-tested) ---

fn new_session_argv(session: &str, cols: u32, rows: u32, command: &str) -> Vec<String> {
    vec![
        "new-session".into(),
        "-d".into(),
        "-s".into(),
        session.into(),
        "-x".into(),
        cols.to_string(),
        "-y".into(),
        rows.to_string(),
        command.into(),
    ]
}

fn capture_argv(session: &str) -> Vec<String> {
    vec!["capture-pane".into(), "-t".into(), session.into(), "-p".into()]
}

fn kill_argv(session: &str) -> Vec<String> {
    vec!["kill-session".into(), "-t".into(), session.into()]
}

/// One `send-keys` argv per key batch.
fn send_argvs(session: &str, batches: &[Vec<String>]) -> Vec<Vec<String>> {
    batches
        .iter()
        .map(|b| {
            let mut v = vec!["send-keys".to_string(), "-t".to_string(), session.to_string()];
            v.extend(b.iter().cloned());
            v
        })
        .collect()
}

/// POSIX single-quote an argument for embedding in a remote shell command.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

// --- tmux runner (local process or remote ssh exec) ---

async fn run_tmux(target: &Target, argv: &[String], timeout_secs: u64) -> anyhow::Result<String> {
    match target {
        Target::Local => {
            let out = tokio::process::Command::new("tmux")
                .args(argv)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("spawn tmux failed (is tmux installed?): {e}"))?;
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            } else {
                Err(anyhow::anyhow!(
                    "tmux {:?} failed: {}",
                    argv,
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        }
        Target::Ssh(id) => {
            let cmd = std::iter::once("tmux".to_string())
                .chain(argv.iter().map(|a| shell_quote(a)))
                .collect::<Vec<_>>()
                .join(" ");
            let out = session::exec(id, &cmd, timeout_secs).await?;
            if out.code == 0 {
                Ok(out.stdout)
            } else {
                Err(anyhow::anyhow!(
                    "remote tmux failed (rc {}): {}",
                    out.code,
                    out.stderr.trim()
                ))
            }
        }
    }
}

/// tmux interactive-driver tool.
pub struct PtyTool {
    security: Arc<SecurityPolicy>,
}

use std::sync::Arc;

impl PtyTool {
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

fn str_field<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str)
}

impl PtyTool {
    async fn do_start(args: &serde_json::Value, sess: &str) -> ToolResult {
        let Some(command) = str_field(args, "command") else {
            return fail("start requires `command`");
        };
        let target = match str_field(args, "target") {
            Some("local") | None => Target::Local,
            Some(id) => Target::Ssh(id.to_string()),
        };
        let cols = u32::try_from(args.get("cols").and_then(serde_json::Value::as_u64).unwrap_or(200))
            .unwrap_or(200);
        let rows = u32::try_from(args.get("rows").and_then(serde_json::Value::as_u64).unwrap_or(50))
            .unwrap_or(50);
        // best-effort: kill any stale session of the same name first
        let _ = run_tmux(&target, &kill_argv(sess), 15).await;
        match run_tmux(&target, &new_session_argv(sess, cols, rows, command), 30).await {
            Ok(_) => {
                remember_target(sess, target);
                ok(format!("started tmux session `{sess}`"))
            }
            Err(e) => fail(format!("{e}")),
        }
    }

    async fn do_screen(sess: &str) -> ToolResult {
        let Some(target) = lookup_target(sess) else {
            return fail(format!("no tmux session `{sess}` (start it first)"));
        };
        match run_tmux(&target, &capture_argv(sess), 30).await {
            Ok(raw) => ok(strip_ansi(&raw)),
            Err(e) => fail(format!("{e}")),
        }
    }

    async fn do_send(args: &serde_json::Value, sess: &str) -> ToolResult {
        let Some(target) = lookup_target(sess) else {
            return fail(format!("no tmux session `{sess}` (start it first)"));
        };
        let Some(keys) = args.get("keys").and_then(serde_json::Value::as_array) else {
            return fail("send requires a `keys` array");
        };
        let tokens = match parse_key_tokens(keys) {
            Ok(t) => t,
            Err(e) => return fail(format!("{e}")),
        };
        for argv in send_argvs(sess, &tmux_send_batches(&tokens)) {
            if let Err(e) = run_tmux(&target, &argv, 15).await {
                return fail(format!("{e}"));
            }
        }
        ok(format!("sent {} key group(s)", keys.len()))
    }

    async fn do_wait(args: &serde_json::Value, sess: &str) -> ToolResult {
        let Some(target) = lookup_target(sess) else {
            return fail(format!("no tmux session `{sess}` (start it first)"));
        };
        let until = match str_field(args, "until").map(regex::Regex::new) {
            Some(Ok(re)) => Some(re),
            Some(Err(e)) => return fail(format!("invalid `until` regex: {e}")),
            None => None,
        };
        let stable = args.get("stable").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(15_000);
        if until.is_none() && !stable {
            return fail("wait requires `until` (regex) and/or `stable: true`");
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut last: Option<String> = None;
        loop {
            let screen = match run_tmux(&target, &capture_argv(sess), 30).await {
                Ok(raw) => strip_ansi(&raw),
                Err(e) => return fail(format!("{e}")),
            };
            if let Some(re) = &until {
                if re.is_match(&screen) {
                    return ok(screen);
                }
            }
            if stable {
                if let Some(prev) = &last {
                    if frames_stable(prev, &screen) {
                        return ok(screen);
                    }
                }
            }
            if Instant::now() >= deadline {
                // return the last screen so the agent can decide; mark as timeout
                return ToolResult {
                    success: false,
                    output: screen,
                    error: Some("wait timed out".into()),
                };
            }
            last = Some(screen);
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    async fn do_stop(sess: &str) -> ToolResult {
        let Some(target) = lookup_target(sess) else {
            return ok(format!("no tmux session `{sess}` to stop"));
        };
        let _ = run_tmux(&target, &kill_argv(sess), 15).await;
        forget_target(sess);
        ok(format!("stopped tmux session `{sess}`"))
    }
}

#[async_trait]
impl Tool for PtyTool {
    fn name(&self) -> &str {
        "pty"
    }

    fn description(&self) -> &str {
        "Drive an interactive terminal program over tmux (local or via an open ssh session). \
         Actions: start (launch a command in a detached tmux session), screen (capture the \
         rendered screen text), send (named keys like Up/Down/Enter/Tab/C-c or {\"text\":\"...\"}), \
         wait (poll until an `until` regex matches or the screen is `stable`), stop. Always wait \
         for the screen you expect before sending keys."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["start", "screen", "send", "wait", "stop"] },
                "session": { "type": "string", "description": "tmux session name (default `nqr`)" },
                "target": { "type": "string", "description": "start only: `local` or an ssh session id" },
                "command": { "type": "string", "description": "start only: command to run in tmux" },
                "cols": { "type": "integer", "description": "start only: width (default 200)" },
                "rows": { "type": "integer", "description": "start only: height (default 50)" },
                "keys": {
                    "type": "array",
                    "description": "send only: list of named keys (strings) and/or {\"text\":\"...\"} objects",
                    "items": {}
                },
                "until": { "type": "string", "description": "wait only: regex to match on the screen" },
                "stable": { "type": "boolean", "description": "wait only: return once the screen stops changing" },
                "timeout_ms": { "type": "integer", "description": "wait only: max wait (default 15000)" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let Some(action) = str_field(&args, "action") else {
            return Ok(fail("missing `action`"));
        };
        // Only mutating actions are gated/rate-limited; screen/wait/stop poll freely.
        if matches!(action, "start" | "send") {
            if !self.security.can_act() {
                return Ok(fail("Action blocked: autonomy is read-only"));
            }
            if !self.security.record_action() {
                return Ok(fail("Action blocked: rate limit exceeded"));
            }
        }
        let sess = str_field(&args, "session").unwrap_or("nqr").to_string();
        let result = match action {
            "start" => Self::do_start(&args, &sess).await,
            "screen" => Self::do_screen(&sess).await,
            "send" => Self::do_send(&args, &sess).await,
            "wait" => Self::do_wait(&args, &sess).await,
            "stop" => Self::do_stop(&sess).await,
            other => fail(format!("unknown action `{other}`")),
        };
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_builder() {
        assert_eq!(
            new_session_argv("nqr", 200, 50, "sudo /tmp/nqr-installer install"),
            vec![
                "new-session", "-d", "-s", "nqr", "-x", "200", "-y", "50",
                "sudo /tmp/nqr-installer install"
            ]
        );
    }

    #[test]
    fn capture_and_kill_builders() {
        assert_eq!(capture_argv("nqr"), vec!["capture-pane", "-t", "nqr", "-p"]);
        assert_eq!(kill_argv("nqr"), vec!["kill-session", "-t", "nqr"]);
    }

    #[test]
    fn send_argvs_per_batch() {
        let batches = vec![
            vec!["Down".to_string(), "Enter".to_string()],
            vec!["-l".to_string(), "prod".to_string()],
        ];
        assert_eq!(
            send_argvs("nqr", &batches),
            vec![
                vec!["send-keys", "-t", "nqr", "Down", "Enter"],
                vec!["send-keys", "-t", "nqr", "-l", "prod"],
            ]
        );
    }

    #[test]
    fn shell_quote_escapes() {
        assert_eq!(shell_quote("prod"), "'prod'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
