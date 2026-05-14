//! `/mcp` slash command — lists configured MCP servers and their
//! discovered tools. Answers the user's question: "did rantaiclaw
//! pick up the MCP server I added to config.toml?"
//!
//! The view is rendered from two sources:
//! 1. The agent-side config (`config.toml` `[mcp_servers.<name>]`)
//!    — what the user *asked* for.
//! 2. The live tool registry from `TuiContext` — what the agent
//!    *actually got* after discovery. A configured-but-missing
//!    server here means handshake or `tools/list` failed at boot.

use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;

pub struct McpCommand;

impl CommandHandler for McpCommand {
    fn name(&self) -> &str {
        "mcp"
    }

    fn description(&self) -> &str {
        "Show configured MCP servers and their discovered tools"
    }

    fn usage(&self) -> &str {
        "/mcp"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let configured = &ctx.mcp_servers_configured;
        let live = &ctx.mcp_tools_by_server;

        if configured.is_empty() && live.is_empty() {
            return Ok(CommandResult::Message(
                "No MCP servers configured. Add one via `[mcp_servers.<name>]` in config.toml — \
                 see `docs/reference/mcp.md` (forthcoming) for the official server catalogue."
                    .into(),
            ));
        }

        let mut out = String::new();
        out.push_str(&format!(
            "MCP servers ({} configured, {} live):\n",
            configured.len(),
            live.len()
        ));
        out.push('\n');

        // Sort for stable rendering.
        let mut names: Vec<&String> = configured.iter().chain(live.keys()).collect();
        names.sort();
        names.dedup();

        for name in names {
            let is_configured = configured.contains(name);
            let live_tools = live.get(name).map(Vec::as_slice).unwrap_or(&[]);
            let marker = match (is_configured, !live_tools.is_empty()) {
                (true, true) => "▸",   // configured + healthy
                (true, false) => "✗",  // configured but failed to discover
                (false, true) => "?",  // live but not in config (rare; surfaces config drift)
                (false, false) => " ", // shouldn't happen
            };
            out.push_str(&format!(
                "{marker} {name} — {} tool{}\n",
                live_tools.len(),
                if live_tools.len() == 1 { "" } else { "s" }
            ));
            for tool_name in live_tools {
                out.push_str(&format!("    · {tool_name}\n"));
            }
            if is_configured && live_tools.is_empty() {
                out.push_str(
                    "    (failed to discover — check rantaiclaw logs for connect/handshake errors)\n",
                );
            }
        }

        Ok(CommandResult::Message(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TuiContext {
        let (ctx, _, _) = TuiContext::test_context();
        ctx
    }

    #[test]
    fn empty_state_returns_friendly_message() {
        let mut c = ctx();
        let result = McpCommand.execute("", &mut c).unwrap();
        match result {
            CommandResult::Message(m) => assert!(m.contains("No MCP servers configured")),
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn healthy_server_shows_tool_list() {
        let mut c = ctx();
        c.mcp_servers_configured.insert("filesystem".to_string());
        c.mcp_tools_by_server.insert(
            "filesystem".to_string(),
            vec![
                "mcp__filesystem__read_file".to_string(),
                "mcp__filesystem__write_file".to_string(),
            ],
        );

        let result = McpCommand.execute("", &mut c).unwrap();
        match result {
            CommandResult::Message(m) => {
                assert!(m.contains("filesystem"));
                assert!(m.contains("2 tools"));
                assert!(m.contains("read_file"));
                assert!(m.contains("write_file"));
                assert!(m.contains("▸"));
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn failed_server_marked_with_x() {
        let mut c = ctx();
        c.mcp_servers_configured.insert("broken".to_string());
        // No entry in mcp_tools_by_server — discovery failed.

        let result = McpCommand.execute("", &mut c).unwrap();
        match result {
            CommandResult::Message(m) => {
                assert!(m.contains("broken"));
                assert!(m.contains("✗"));
                assert!(m.contains("failed to discover"));
            }
            _ => panic!("expected Message"),
        }
    }
}
