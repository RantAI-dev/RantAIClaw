//! MCP client and tool discovery for stdio-based MCP servers.
//!
//! **There is no crash recovery here, and no code claims there is.** An
//! `McpRegistry` / `McpHandle` / `spawn_supervisor` stack used to live in this
//! module describing respawn-with-backoff; it had no caller anywhere, its own log
//! promised five restart attempts while the counter gave up after four, and it
//! carried a second process-spawn implementation alongside [`client`]. Plan 312
//! deleted it. Git history holds it.
//!
//! The lifetime that IS real is the gateway's pool
//! ([`discover::McpPoolHandle`], plan 287): servers are connected once and reused
//! across chat requests, and reconnected when `mcp_servers` config changes. If
//! supervision is ever wanted, it is built on that pool.
//!
//! # Where MCP tools actually reach
//!
//! Not everywhere, and the gap is wider than it looks: MCP tools are spliced into
//! the registry by `Agent::build`, so they reach the **TUI/CLI interactive agent**
//! and the **gateway's `/api/v1` chat path** (which uses the pool). They do NOT
//! reach chat channels, cron, or the gateway's own webhook path — each of those
//! assembles its tool list from `tools::all_tools_with_runtime` with no MCP
//! splice. Tracked in issue #283; making it reach everywhere needs a shared pool
//! with one owner, which is a feature decision rather than a cleanup.

pub mod client;
pub mod curated;
pub mod discover;
pub mod oauth;
pub mod setup;
pub mod tool;

use std::collections::HashMap;

/// Strip the inherited daemon environment and re-add only a non-secret
/// allowlist plus the explicitly-configured `env` map, mirroring the shell
/// tool's hardening (`src/tools/shell.rs`). Without this, every MCP
/// subprocess — frequently a third-party npm/uv package the operator did not
/// write — inherits the daemon's entire process environment (provider API
/// keys, proxy credentials) on top of its own declared `env`. A compromised
/// or malicious MCP server could otherwise read and exfiltrate every daemon
/// secret with no extra access.
pub(crate) fn apply_hardened_env(cmd: &mut tokio::process::Command, env: &HashMap<String, String>) {
    cmd.env_clear();
    for var in crate::tools::shell::SAFE_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    cmd.envs(env); // configured env overrides the allowlist — intentional.
}
