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
//! The **TUI/CLI interactive agent** and the **gateway's `/api/v1` chat** get them
//! through `Agent::build`. **Chat channels** and **cron** get them because each
//! now splices the pool into the registry it assembles itself — that was issue
//! #283, and the reason it survived two waves of reading is that the tools were
//! simply absent, with no error and no log line.
//!
//! Ownership is per subsystem rather than per process: the channel runtime holds
//! one pool for its life, the cron scheduler holds one for its life, the gateway
//! holds one. A one-shot run spawns its own and drops them when it exits. So a
//! daemon with three configured servers can run up to nine server processes —
//! bounded by subsystem count, never by traffic, which is the property that
//! matters.
//!
//! One surface is still without them: the gateway's own **`/webhook`** path. Its
//! registry comes from a synchronous `ToolsFactory` closure and connecting a pool
//! must await, so that one is a signature change to a shared factory rather than
//! a wiring fix.

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
