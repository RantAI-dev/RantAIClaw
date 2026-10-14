//! MCP startup check — starts each configured server and confirms it answers
//! the MCP handshake and lists its tools.
//!
//! Online, each server is started with its configured command, args and env,
//! sent `initialize` and `tools/list`, and then dropped (which ends the
//! process). All of that runs under one deadline per server. A server passes
//! only if all three steps complete in time; otherwise the FAIL names the
//! server and the step that failed.
//!
//! The command is first looked up on `$PATH`, which is cheap and needs no
//! spawn, so a missing binary is reported without starting anything.
//!
//! `--offline` stops after the PATH lookup and never starts a server; its OK
//! says "on PATH", never "launchable", because a name lookup cannot prove the
//! server starts.

use std::time::Duration;

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};
use crate::mcp::client::McpClient;

pub struct McpStartupCheck;

/// Budget for one server's start, handshake and tool listing together.
/// Shorter under test so the silent-server test does not wait the full 10s.
fn startup_budget() -> Duration {
    if cfg!(test) {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(10)
    }
}

/// What an operator configuring an MCP server is not otherwise told.
///
/// Issue #283 closed: chat channels and cron now build their registries with the
/// MCP splice, alongside the TUI/CLI agent and the gateway's `/api/v1` chat. One
/// surface is left — the gateway's own `/webhook` path, whose registry comes from
/// a synchronous `ToolsFactory` closure that cannot await a pool connect.
///
/// The hint stays because the remaining gap is still silent: a configured server
/// is simply absent there, with no error and no log line, and silence is the harm.
const MCP_REACH_HINT: &str = "MCP tools reach the TUI/CLI agent, chat channels, cron \
     and the gateway's /api/v1 chat — not the gateway's own /webhook path";

/// Start one server and list its tools within `budget`. On error returns the
/// step that failed. Never includes env values in the reason.
async fn probe(
    name: &str,
    srv: &crate::config::schema::McpServerConfig,
    budget: Duration,
) -> Result<usize, &'static str> {
    let deadline = tokio::time::Instant::now() + budget;
    let client = match tokio::time::timeout_at(
        deadline,
        McpClient::connect(name, &srv.command, &srv.args, &srv.env),
    )
    .await
    {
        Err(_) => return Err("timed out starting or initializing"),
        Ok(Err(e)) => return Err(classify_connect_error(&e)),
        Ok(Ok(client)) => client,
    };
    let listed = tokio::time::timeout_at(deadline, client.list_tools()).await;
    // Dropping the client ends the process (`kill_on_drop`), whatever the outcome.
    drop(client);
    match listed {
        Err(_) => Err("timed out listing tools"),
        Ok(Err(e)) if is_exit(&e) => Err("exited before answering tools/list"),
        Ok(Err(_)) => Err("protocol error listing tools"),
        Ok(Ok(tools)) => Ok(tools.len()),
    }
}

fn classify_connect_error(e: &anyhow::Error) -> &'static str {
    let spawning = e
        .chain()
        .any(|c| c.to_string().starts_with("spawn MCP server"));
    if spawning {
        if e.chain().any(|c| {
            c.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        }) {
            "not found when spawning"
        } else {
            "could not be spawned"
        }
    } else if is_exit(e) {
        "exited before answering initialize"
    } else {
        "protocol error during initialize"
    }
}

/// The server went away before answering: either the client saw it close
/// stdout, or a write to its stdin hit a closed pipe.
fn is_exit(e: &anyhow::Error) -> bool {
    let closed_pipe = e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
            )
        })
    });
    closed_pipe || format!("{e:#}").contains("closed stdout before responding")
}

fn plural_tools(n: usize) -> &'static str {
    if n == 1 {
        "tool"
    } else {
        "tools"
    }
}

#[async_trait]
impl DoctorCheck for McpStartupCheck {
    fn name(&self) -> &'static str {
        "mcp.startup"
    }
    fn category(&self) -> &'static str {
        "live"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let servers = &ctx.config.mcp_servers;
        if servers.is_empty() {
            return CheckResult::info(self.name(), "no MCP servers configured")
                .with_category(self.category());
        }

        let mut names: Vec<&String> = servers.keys().collect();
        names.sort();

        let mut missing: Vec<String> = Vec::new();
        for name in &names {
            let cmd = &servers[*name].command;
            if which::which(cmd).is_err() {
                missing.push(format!("{name} ({cmd})"));
            }
        }
        if !missing.is_empty() {
            return CheckResult::fail(
                self.name(),
                format!("commands not on PATH: {}", missing.join(", ")),
            )
            .with_category(self.category())
            .with_hint("install the missing binaries or run: rantaiclaw setup mcp");
        }

        if ctx.offline {
            return CheckResult::ok(
                self.name(),
                format!("{} MCP server command(s) found on PATH", names.len()),
            )
            .with_category(self.category())
            .with_hint(MCP_REACH_HINT);
        }

        let budget = startup_budget();
        let probes = names.iter().map(|name| {
            let srv = &servers[*name];
            async move { (*name, probe(name, srv, budget).await) }
        });
        let results = futures::future::join_all(probes).await;

        let mut ok_parts: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for (name, result) in results {
            match result {
                Ok(n) => ok_parts.push(format!("{name}: {n} {}", plural_tools(n))),
                Err(step) => failed.push(format!("{name} ({}): {step}", servers[name].command)),
            }
        }

        if failed.is_empty() {
            CheckResult::ok(
                self.name(),
                format!("MCP servers started; {}", ok_parts.join(", ")),
            )
            .with_category(self.category())
            .with_hint(MCP_REACH_HINT)
        } else {
            CheckResult::fail(
                self.name(),
                format!("MCP servers failed to start: {}", failed.join("; ")),
            )
            .with_category(self.category())
            .with_hint("fix the command, args or env of the failing server(s), or run: rantaiclaw setup mcp")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::profile::Profile;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn ctx(cfg: Config) -> (DoctorContext, TempDir) {
        let tmp = TempDir::new().unwrap();
        let p = Profile {
            name: "test".into(),
            root: tmp.path().to_path_buf(),
        };
        (
            DoctorContext {
                profile: p,
                config: cfg,
                offline: false,
            },
            tmp,
        )
    }

    #[tokio::test]
    async fn no_mcp_servers_returns_info() {
        let cfg = Config::default();
        let (c, _t) = ctx(cfg);
        let r = McpStartupCheck.run(&c).await;
        assert_eq!(r.severity, crate::doctor::Severity::Info);
    }

    #[tokio::test]
    async fn missing_command_returns_fail() {
        let mut cfg = Config::default();
        let mut servers = HashMap::new();
        servers.insert(
            "fake".to_string(),
            crate::config::schema::McpServerConfig {
                command: "definitely-not-a-real-binary-xyz123".into(),
                args: vec![],
                env: HashMap::new(),
            },
        );
        cfg.mcp_servers = servers;
        let (c, _t) = ctx(cfg);
        let r = McpStartupCheck.run(&c).await;
        assert_eq!(r.severity, crate::doctor::Severity::Fail);
        assert!(r.message.contains("definitely-not-a-real-binary-xyz123"));
    }

    /// A well-behaved stdio server: answers `initialize` and `tools/list`
    /// with two tools, and ignores everything else.
    const GOOD_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *initialize*) printf '{"jsonrpc":"2.0","id":%s,"result":{"capabilities":{}}}\n' "$id" ;;
    *tools/list*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"a"},{"name":"b"}]}}\n' "$id" ;;
  esac
done
"#;

    fn server(command: &str, args: &[&str]) -> crate::config::schema::McpServerConfig {
        crate::config::schema::McpServerConfig {
            command: command.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: HashMap::new(),
        }
    }

    fn with_servers(entries: Vec<(&str, crate::config::schema::McpServerConfig)>) -> Config {
        let mut cfg = Config::default();
        cfg.mcp_servers = entries
            .into_iter()
            .map(|(n, s)| (n.to_string(), s))
            .collect();
        cfg
    }

    fn online(cfg: Config) -> (DoctorContext, TempDir) {
        ctx(cfg)
    }

    #[tokio::test]
    async fn server_that_answers_initialize_and_tools_list_is_ok_with_tool_count() {
        let cfg = with_servers(vec![("good", server("sh", &["-c", GOOD_SERVER]))]);
        let (c, _t) = online(cfg);
        let r = McpStartupCheck.run(&c).await;
        assert_eq!(
            r.severity,
            crate::doctor::Severity::Ok,
            "msg: {}",
            r.message
        );
        assert!(r.message.contains("good"), "msg: {}", r.message);
        assert!(r.message.contains("good: 2 tools"), "msg: {}", r.message);
        assert!(r.hint.is_some(), "MCP_REACH_HINT stays on the OK result");
    }

    #[tokio::test]
    async fn server_that_exits_at_once_fails_naming_server_and_step() {
        let cfg = with_servers(vec![("exits", server("sh", &["-c", "exit 3"]))]);
        let (c, _t) = online(cfg);
        let r = McpStartupCheck.run(&c).await;
        assert_eq!(
            r.severity,
            crate::doctor::Severity::Fail,
            "msg: {}",
            r.message
        );
        assert!(r.message.contains("exits"), "msg: {}", r.message);
        assert!(r.message.contains("exited"), "msg: {}", r.message);
    }

    #[tokio::test]
    async fn server_that_never_answers_fails_as_timed_out_naming_server() {
        let cfg = with_servers(vec![("silent", server("sh", &["-c", "sleep 30"]))]);
        let (c, _t) = online(cfg);
        // Outer guard: a regression that waits out the server must fail the
        // test, not hang CI.
        let r = tokio::time::timeout(std::time::Duration::from_secs(15), McpStartupCheck.run(&c))
            .await
            .expect("check must not hang on a silent server");
        assert_eq!(
            r.severity,
            crate::doctor::Severity::Fail,
            "msg: {}",
            r.message
        );
        assert!(r.message.contains("silent"), "msg: {}", r.message);
        assert!(r.message.contains("timed out"), "msg: {}", r.message);
    }

    /// The ok message must not claim more than a PATH lookup proves. Under
    /// `--offline` nothing is started, so the verdict is the PATH scan and
    /// says "on PATH", never "launchable". The server here would fail if it
    /// were started, so the OK result also proves it was not spawned.
    #[tokio::test]
    async fn offline_scan_says_on_path_and_never_starts_the_server() {
        let cfg = with_servers(vec![("exits", server("sh", &["-c", "exit 3"]))]);
        let (mut c, _t) = ctx(cfg);
        c.offline = true;
        let r = McpStartupCheck.run(&c).await;
        assert_eq!(
            r.severity,
            crate::doctor::Severity::Ok,
            "msg: {}",
            r.message
        );
        assert!(r.message.contains("on PATH"), "msg: {}", r.message);
        assert!(
            !r.message.contains("launchable"),
            "must not overclaim: {}",
            r.message
        );
    }
}
