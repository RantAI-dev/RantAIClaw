#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::assigning_clones,
    clippy::bool_to_int_with_if,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::cast_possible_wrap,
    clippy::doc_markdown,
    clippy::field_reassign_with_default,
    clippy::float_cmp,
    clippy::implicit_clone,
    clippy::items_after_statements,
    clippy::map_unwrap_or,
    clippy::manual_let_else,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::new_without_default,
    clippy::needless_pass_by_value,
    clippy::needless_raw_string_hashes,
    clippy::redundant_closure_for_method_calls,
    clippy::return_self_not_must_use,
    clippy::similar_names,
    clippy::single_match_else,
    clippy::struct_field_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_literal_bound,
    clippy::unnecessary_map_or,
    clippy::unused_self,
    clippy::cast_precision_loss,
    clippy::unnecessary_wraps,
    dead_code
)]

use clap::Subcommand;
use serde::{Deserialize, Serialize};

pub mod agent;
pub mod approval;
pub mod channels;
#[doc(hidden)]
pub mod cli_style;
pub mod config;
#[doc(hidden)]
pub mod cost;
pub mod cron;
pub mod daemon;
pub mod doctor;
pub mod gateway;
pub(crate) mod health;
pub(crate) mod heartbeat;
pub(crate) mod identity;
pub mod integrations;
#[cfg(feature = "kb")]
pub mod kb;
pub mod lifecycle;
pub mod logging;
pub mod mcp;
pub mod memory;
pub mod migration;
pub(crate) mod multimodal;
pub mod observability;
pub mod onboard;
pub mod persona;
pub mod profile;
pub mod providers;
#[cfg(feature = "remote-install")]
pub mod remote;
pub mod runtime;
pub mod security;
pub mod service;
pub(crate) mod services;
pub mod sessions;
pub mod skills;
pub(crate) mod tasks;
#[cfg(test)]
mod test_env;
pub mod tools;
pub mod tui;
pub(crate) mod tunnel;
pub(crate) mod util;

pub use config::Config;

/// What the removed `auth` command prints, and the code it exits with.
///
/// The command survives as a hidden stub, so `rantaiclaw auth login` explains
/// what happened instead of printing "unrecognized subcommand". It exits
/// non-zero: the operator asked for a login, and no login happened.
pub const AUTH_COMMAND_REMOVED_MESSAGE: &str = "The `auth` command was removed: RantaiClaw no longer stores provider subscription logins. Use an API key instead — set ANTHROPIC_API_KEY, store the provider's key in config.toml, or run `rantaiclaw setup provider`.";

/// The exit code the removed `auth` command exits with.
pub const AUTH_COMMAND_REMOVED_EXIT_CODE: i32 = 1;

/// Service management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServiceCommands {
    /// Install daemon service unit for auto-start and restart
    Install,
    /// Start daemon service
    Start,
    /// Stop daemon service
    Stop,
    /// Restart daemon service to apply latest config
    Restart,
    /// Check daemon service status
    Status,
    /// Uninstall daemon service unit
    Uninstall,
}

/// Channel management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChannelCommands {
    /// List configured channels
    List,
    /// Start all configured channels — foreground
    Start,
    /// Run all configured channels in the foreground until Ctrl-C.
    ///
    /// Alias of `start` with a louder, friendlier startup banner so users
    /// arriving from the TUI's "configure channels" flow see clear stop
    /// instructions. Background it from your shell: `nohup rantaiclaw
    /// channels run > out.log 2>&1 &` or via systemd-run / tmux / screen.
    Run,
    /// Run health checks for configured channels
    Doctor,
    /// Add a new channel (currently redirects to `onboard` — see long help)
    #[command(long_about = "\
Add a channel configuration.

NOTE: CLI `channel add` is not yet implemented. It prints the requested type \
and directs you to the interactive setup. Configure channels with:
  rantaiclaw onboard          # guided channel setup (recommended)
or edit the active profile's `config.toml` directly.

Channel types: telegram, discord, slack, mattermost, webhook, \
imessage, matrix, signal, whatsapp, whatsapp_web, linq, nextcloud_talk, \
email, irc, lark, dingtalk, qq.")]
    Add {
        /// Channel type. Channel types: telegram, discord, slack,
        /// mattermost, webhook, imessage, matrix, signal, whatsapp,
        /// whatsapp_web, linq, nextcloud_talk, email, irc, lark, dingtalk, qq.
        channel_type: String,
    },
    /// Remove a channel (currently redirects to config edit — see long help)
    #[command(long_about = "\
Remove a channel configuration.

NOTE: CLI `channel remove` is not yet implemented. It directs you to edit \
the active profile's `config.toml` directly (delete the channel's \
[channels_config.<name>] section) or re-run `rantaiclaw onboard`.")]
    Remove {
        /// Channel name to remove
        name: String,
    },
    /// Bind a Telegram identity (username or numeric user ID) into allowlist
    BindTelegram {
        /// Telegram identity to allow (username without '@' or numeric user ID)
        identity: String,
    },
    /// Remove a Telegram identity (or the `*` wildcard) from the allowlist
    UnbindTelegram {
        /// Telegram identity to remove (username without '@', numeric user ID, or `*`)
        identity: String,
    },
    /// Issue an on-demand pairing code (no daemon restart needed).
    ///
    /// Mints a time-windowed, multi-claim code into the shared store. A running
    /// channel validates it on the next `/bind`/`/claim` message — no restart.
    Pair {
        /// Channel surface the code is scoped to (telegram, discord, …).
        #[arg(long, default_value = "telegram")]
        channel: String,
        /// Validity window in minutes.
        #[arg(long, default_value_t = 15)]
        ttl: i64,
        /// Maximum number of claims within the window (default: unlimited).
        #[arg(long)]
        max_uses: Option<u32>,
        /// Disallow `/claim` (owner) with this code — chat-only `/bind`.
        #[arg(long)]
        no_owner: bool,
    },
}

/// Skills management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillCommands {
    /// List installed skills
    List,
    /// Show metadata for a single installed skill (CLI parity for TUI `/skill <name>`)
    Show {
        /// Skill name (case-insensitive)
        name: String,
    },
    /// Enable a skill that was previously disabled (writes
    /// `[skills.entries.<name>] enabled = true`).
    Enable {
        /// Skill name (case-insensitive; matches the name shown by `skills list`).
        name: String,
    },
    /// Disable a skill so it is not loaded into the agent's context (writes
    /// `[skills.entries.<name>] enabled = false`).
    Disable {
        /// Skill name (case-insensitive; matches the name shown by `skills list`).
        name: String,
    },
    /// Install a skill from a GitHub URL or local path
    Install {
        /// GitHub URL or local path
        source: String,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name
        name: String,
    },
    /// Re-pull installed ClawHub skill(s) at their latest version.
    /// With `--all`, updates every installed skill. With a slug, updates
    /// just that one. Local-path / git skills are skipped (you can `git pull`
    /// or re-install those manually).
    Update {
        /// Specific skill slug to update; omit with `--all` to update every skill.
        slug: Option<String>,
        /// Update every installed skill.
        #[arg(long)]
        all: bool,
    },
    /// Show ClawHub metadata + security scan for a skill *before* installing.
    /// Hits ClawHub's public API; no install side-effect.
    Inspect {
        /// ClawHub skill slug.
        slug: String,
    },
    /// Install missing binary dependencies declared by a skill's
    /// `metadata.clawdbot.install[]` recipes (brew/npm/uv/go/download).
    /// Picks the preferred recipe based on what's available on this host.
    /// Closes the OpenClaw-parity gap where rantaiclaw could detect
    /// missing bins but couldn't fix them.
    InstallDeps {
        /// Skill name. Omit with --all to fix every gated skill that has recipes.
        slug: Option<String>,
        /// Run install-deps for every skill whose requires aren't met.
        #[arg(long)]
        all: bool,
    },
}

/// Cron subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CronCommands {
    /// List all scheduled tasks
    List,
    /// Add a new scheduled task
    Add {
        /// Cron expression
        expression: String,
        /// Optional IANA timezone (e.g. America/Los_Angeles)
        #[arg(long)]
        tz: Option<String>,
        /// Command to run
        command: String,
        /// Create an AGENT job (the positional is the prompt, not a shell command).
        #[arg(long)]
        agent: bool,
        /// Model override for an agent job (ignored for shell jobs).
        #[arg(long)]
        model: Option<String>,
    },
    /// Add a one-shot scheduled task at an RFC3339 timestamp
    AddAt {
        /// One-shot timestamp in RFC3339 format
        at: String,
        /// Command to run
        command: String,
        /// Create an AGENT job (the positional is the prompt, not a shell command).
        #[arg(long)]
        agent: bool,
        /// Model override for an agent job (ignored for shell jobs).
        #[arg(long)]
        model: Option<String>,
    },
    /// Add a fixed-interval scheduled task
    AddEvery {
        /// Interval in milliseconds
        every_ms: u64,
        /// Command to run
        command: String,
        /// Create an AGENT job (the positional is the prompt, not a shell command).
        #[arg(long)]
        agent: bool,
        /// Model override for an agent job (ignored for shell jobs).
        #[arg(long)]
        model: Option<String>,
    },
    /// Add a one-shot delayed task (e.g. "30m", "2h", "1d")
    Once {
        /// Delay duration
        delay: String,
        /// Command to run
        command: String,
        /// Create an AGENT job (the positional is the prompt, not a shell command).
        #[arg(long)]
        agent: bool,
        /// Model override for an agent job (ignored for shell jobs).
        #[arg(long)]
        model: Option<String>,
    },
    /// Remove a scheduled task
    Remove {
        /// Task ID
        id: String,
    },
    /// Update a scheduled task
    Update {
        /// Task ID
        id: String,
        /// New cron expression
        #[arg(long)]
        expression: Option<String>,
        /// New IANA timezone
        #[arg(long)]
        tz: Option<String>,
        /// New command to run
        #[arg(long)]
        command: Option<String>,
        /// New job name
        #[arg(long)]
        name: Option<String>,
    },
    /// Pause a scheduled task
    Pause {
        /// Task ID
        id: String,
    },
    /// Resume a paused task
    Resume {
        /// Task ID
        id: String,
    },
    /// Force-run a scheduled task now and record the run.
    Run {
        /// Job id
        id: String,
    },
    /// Show recent run history for a scheduled task.
    Runs {
        /// Job id
        id: String,
        /// Max rows to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

/// Memory management subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MemoryCommands {
    /// List memory entries with optional filters
    List {
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value = "50")]
        limit: usize,
        #[arg(long, default_value = "0")]
        offset: usize,
    },
    /// Get a specific memory entry by key
    Get { key: String },
    /// Store a memory entry
    Add {
        key: String,
        content: String,
        #[arg(long, default_value = "core")]
        category: String,
    },
    /// Search memory entries by keyword
    Recall {
        query: String,
        #[arg(long, default_value = "5")]
        limit: usize,
    },
    /// Re-embed memories the current embedding model cannot use
    Reindex,
    /// Show memory backend statistics and health
    Stats,
    /// Clear memories by category, by key, or clear all
    Clear {
        /// Delete a single entry by key (supports prefix match)
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        category: Option<String>,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

/// Integration subcommands
#[derive(Subcommand, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IntegrationCommands {
    /// List all integrations grouped by category (default)
    List,
    /// Show details about a specific integration
    Info {
        /// Integration name
        name: String,
    },
}
