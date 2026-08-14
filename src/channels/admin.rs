//! The operator-facing CLI surface: `channel add/remove/list/doctor`, the
//! Telegram allowlist binder, pairing, and the managed-daemon reload.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 9). No behaviour change.
//!
//! This is also where the file's two output contracts separate: `println!` is
//! correct **here**, because these functions run in a terminal the operator is
//! looking at. On the runtime path it corrupts the TUI's alt-screen, which is
//! why the runtime modules use `tracing` instead.

use super::factory;
use super::{
    channel_is_configured, CHANNEL_CATALOG, OPENRC_RESTART_ARGS, OPENRC_STATUS_ARGS,
    SYSTEMD_RESTART_ARGS, SYSTEMD_STATUS_ARGS,
};
use crate::config::Config;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

pub(crate) fn normalize_telegram_identity(value: &str) -> String {
    value.trim().trim_start_matches('@').to_string()
}

pub(crate) async fn bind_telegram_identity(config: &Config, identity: &str) -> Result<()> {
    let normalized = normalize_telegram_identity(identity);
    if normalized.is_empty() {
        anyhow::bail!("Telegram identity cannot be empty");
    }

    let mut updated = config.clone();
    let Some(telegram) = updated.channels_config.telegram.as_mut() else {
        anyhow::bail!(
            "Telegram channel is not configured. Run `rantaiclaw onboard --channels-only` first"
        );
    };

    if telegram.allowed_users.iter().any(|u| u == "*") {
        println!(
            "⚠️ Telegram allowlist is currently wildcard (`*`) — binding is unnecessary until you remove '*'."
        );
    }

    if telegram
        .allowed_users
        .iter()
        .map(|entry| normalize_telegram_identity(entry))
        .any(|entry| entry == normalized)
    {
        println!("✅ Telegram identity already bound: {normalized}");
        return Ok(());
    }

    telegram.allowed_users.push(normalized.clone());
    updated.save().await?;
    println!("✅ Bound Telegram identity: {normalized}");
    println!("   Saved to {}", updated.config_path.display());
    announce_daemon_reload();
    Ok(())
}

pub(crate) async fn unbind_telegram_identity(config: &Config, identity: &str) -> Result<()> {
    let normalized = normalize_telegram_identity(identity);
    if normalized.is_empty() {
        anyhow::bail!("Telegram identity cannot be empty");
    }

    let mut updated = config.clone();
    let Some(telegram) = updated.channels_config.telegram.as_mut() else {
        anyhow::bail!(
            "Telegram channel is not configured. Run `rantaiclaw onboard --channels-only` first"
        );
    };

    let before = telegram.allowed_users.len();
    telegram
        .allowed_users
        .retain(|entry| normalize_telegram_identity(entry) != normalized);
    let removed = before - telegram.allowed_users.len();

    if removed == 0 {
        println!("ℹ️ Telegram identity not in allowlist: {normalized} (nothing to remove)");
        return Ok(());
    }

    let now_empty = telegram.allowed_users.is_empty();
    updated.save().await?;
    let plural = if removed == 1 { "entry" } else { "entries" };
    println!("✅ Removed Telegram identity: {normalized} ({removed} {plural} dropped)");
    println!("   Saved to {}", updated.config_path.display());
    if now_empty {
        println!(
            "⚠️ The Telegram allowlist is now empty — the bot will respond to NO ONE. \
             Add yourself with `rantaiclaw channel bind-telegram <your-username-or-id>`."
        );
    }
    announce_daemon_reload();
    Ok(())
}

/// Resolve the active profile root for the on-disk pairing-code store.
pub(crate) fn pairing_profile_root() -> Result<PathBuf> {
    Ok(crate::profile::ProfileManager::active()?.root)
}

/// Mint an on-demand pairing code for `channel` into the shared store and print
/// the code plus `/bind`/`/claim` instructions. Works whether or not the daemon
/// is running — a running daemon validates the code on the next pairing message
/// without a restart.
///
/// `ttl_minutes` is the validity window; `max_uses` bounds claims (`None` =
/// unlimited within the window); `grant_owner` permits `/claim` (owner). Returns
/// the minted plaintext code (also used by tests to assert it is non-empty).
pub(crate) fn pair_channel(
    channel: &str,
    ttl_minutes: i64,
    max_uses: Option<u32>,
    grant_owner: bool,
) -> Result<String> {
    let root = pairing_profile_root()?;
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let code = crate::security::pairing_store::mint(
        &root,
        channel,
        ttl_minutes.saturating_mul(60),
        max_uses,
        grant_owner,
        now,
    )
    .with_context(|| format!("minting pairing code for {channel}"))?;

    let uses = match max_uses {
        Some(1) => "single-use".to_string(),
        Some(n) => format!("up to {n} uses"),
        None => "multi-use".to_string(),
    };
    println!("🔐 Pairing code for {channel}: {code}   (valid {ttl_minutes} min, {uses})");
    println!("   DM the bot:  /bind {code}  (chat)  |  /claim {code}  (owner)");
    println!(
        "   No daemon restart needed — a running channel picks this up on the next pairing message."
    );
    Ok(code)
}

/// Try to reload a running managed daemon service after a config change, and
/// print a clear note about what happened either way. Shared by the channel
/// allowlist binder and the `permissions` CLI so config edits made on disk are
/// picked up without the user having to remember to bounce the service.
pub(crate) fn announce_daemon_reload() {
    match maybe_restart_managed_daemon_service() {
        Ok(true) => {
            println!("🔄 Detected running managed daemon service; reloaded automatically.");
        }
        Ok(false) => {
            println!(
                "ℹ️ No managed daemon service detected. If `rantaiclaw daemon`/`channel start` is already running, restart it to load the change."
            );
        }
        Err(e) => {
            eprintln!(
                "⚠️ Saved, but failed to reload daemon service automatically: {e}\n\
                 Restart service manually with `rantaiclaw service stop && rantaiclaw service start`."
            );
        }
    }
}

/// Reload a running managed daemon service (systemd / launchd / OpenRC) after a
/// config change, for non-CLI callers (the gateway) that must not print to
/// stdout. Returns `Ok(true)` if a managed service was restarted, `Ok(false)`
/// when none is installed. Mirrors what [`announce_daemon_reload`] does for the
/// CLI, minus the console output — callers log the outcome themselves.
pub(crate) fn reload_managed_daemon() -> Result<bool> {
    maybe_restart_managed_daemon_service()
}

pub(crate) fn maybe_restart_managed_daemon_service() -> Result<bool> {
    if cfg!(target_os = "macos") {
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let plist = home
            .join("Library")
            .join("LaunchAgents")
            .join("com.rantaiclaw.daemon.plist");
        if !plist.exists() {
            return Ok(false);
        }

        let list_output = Command::new("launchctl")
            .arg("list")
            .output()
            .context("Failed to query launchctl list")?;
        let listed = String::from_utf8_lossy(&list_output.stdout);
        if !listed.contains("com.rantaiclaw.daemon") {
            return Ok(false);
        }

        let _ = Command::new("launchctl")
            .args(["stop", "com.rantaiclaw.daemon"])
            .output();
        let start_output = Command::new("launchctl")
            .args(["start", "com.rantaiclaw.daemon"])
            .output()
            .context("Failed to start launchd daemon service")?;
        if !start_output.status.success() {
            let stderr = String::from_utf8_lossy(&start_output.stderr);
            anyhow::bail!("launchctl start failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    if cfg!(target_os = "linux") {
        // OpenRC (system-wide) takes precedence over systemd (user-level)
        let openrc_init_script = PathBuf::from("/etc/init.d/rantaiclaw");
        if openrc_init_script.exists() {
            if let Ok(status_output) = Command::new("rc-service").args(OPENRC_STATUS_ARGS).output()
            {
                // rc-service exits 0 if running, non-zero otherwise
                if status_output.status.success() {
                    let restart_output = Command::new("rc-service")
                        .args(OPENRC_RESTART_ARGS)
                        .output()
                        .context("Failed to restart OpenRC daemon service")?;
                    if !restart_output.status.success() {
                        let stderr = String::from_utf8_lossy(&restart_output.stderr);
                        anyhow::bail!("rc-service restart failed: {}", stderr.trim());
                    }
                    return Ok(true);
                }
            }
        }

        // Systemd (user-level)
        let home = directories::UserDirs::new()
            .map(|u| u.home_dir().to_path_buf())
            .context("Could not find home directory")?;
        let unit_path: PathBuf = home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("rantaiclaw.service");
        if !unit_path.exists() {
            return Ok(false);
        }

        let active_output = Command::new("systemctl")
            .args(SYSTEMD_STATUS_ARGS)
            .output()
            .context("Failed to query systemd service state")?;
        let state = String::from_utf8_lossy(&active_output.stdout);
        if !state.trim().eq_ignore_ascii_case("active") {
            return Ok(false);
        }

        let restart_output = Command::new("systemctl")
            .args(SYSTEMD_RESTART_ARGS)
            .output()
            .context("Failed to restart systemd daemon service")?;
        if !restart_output.status.success() {
            let stderr = String::from_utf8_lossy(&restart_output.stderr);
            anyhow::bail!("systemctl restart failed: {}", stderr.trim());
        }

        return Ok(true);
    }

    Ok(false)
}

/// Say out loud what an `approval_owners` value actually grants.
///
/// `"*"` makes **every sender on every channel** an owner — the full toolset and
/// the right to approve shell commands. The gateway already warns when
/// `allowed_users` contains `*`; nothing warned for this, and no doc said the
/// value was even accepted.
///
/// The bare `"user"` entry is the console's pre-`cli:local` identity. It is
/// still honoured, but it matches any remote sender who picks that name.
pub(crate) fn warn_on_risky_approval_owners(owners: &[String]) {
    if owners.iter().any(|o| o.trim() == "*") {
        tracing::warn!(
            "approval_owners contains \"*\": EVERY sender on EVERY channel is an owner \
             and may approve shell commands. Replace it with explicit sender ids."
        );
    }
    if owners
        .iter()
        .any(|o| o.trim() == crate::channels::cli::LEGACY_CLI_SENDER_ID)
    {
        tracing::warn!(
            "approval_owners contains \"{}\", the console's old unqualified identity — a \
             remote sender using that same name is also an owner. Change it to \"{}\".",
            crate::channels::cli::LEGACY_CLI_SENDER_ID,
            crate::channels::cli::CLI_SENDER_ID
        );
    }
}

pub(crate) fn channel_roster(config: &Config) -> Vec<(&'static str, bool)> {
    CHANNEL_CATALOG
        .iter()
        .map(|(key, display)| (*display, channel_is_configured(key, config)))
        .collect()
}

/// Guidance for `channel add`, which configures nothing itself.
///
/// Split out so the text is testable without capturing stdout. Both this and
/// `channel_remove_guidance` used to be `bail!`, so a script wrapping
/// `rantaiclaw channel add` saw a non-zero exit for an informational outcome.
pub(crate) fn channel_add_guidance(channel_type: &str) -> String {
    format!("Channel type '{channel_type}' — use `rantaiclaw onboard` to configure channels")
}

/// Guidance for `channel remove`. See [`channel_add_guidance`].
pub(crate) fn channel_remove_guidance(name: &str) -> String {
    format!("Remove channel '{name}' — edit ~/.rantaiclaw/config.toml directly")
}

pub(crate) async fn handle_command(command: crate::ChannelCommands, config: &Config) -> Result<()> {
    match command {
        // Dispatched in `main.rs`, which owns the async runtime these need, so
        // they never reach this match. They used to `bail!` with that routing
        // detail as the user-visible error text — an internal invariant printed
        // as if the operator had done something wrong.
        crate::ChannelCommands::Start
        | crate::ChannelCommands::Run
        | crate::ChannelCommands::Doctor => {
            unreachable!("channel start/run/doctor are dispatched in main.rs")
        }
        crate::ChannelCommands::List => {
            crate::cli_style::section("channels");
            crate::cli_style::status_row(true, "CLI", 14, "always");
            for (name, configured) in channel_roster(config) {
                crate::cli_style::status_row(
                    configured,
                    name,
                    14,
                    if configured {
                        "configured"
                    } else {
                        "not configured"
                    },
                );
            }
            if !cfg!(feature = "channel-matrix") {
                println!(
                    "  {}",
                    crate::cli_style::dim(
                        "Matrix support is disabled in this build (enable `channel-matrix`)."
                    )
                );
            }
            if !cfg!(feature = "channel-lark") {
                println!(
                    "  {}",
                    crate::cli_style::dim(
                        "Lark support is disabled in this build (enable `channel-lark`)."
                    )
                );
            }
            println!();
            println!(
                "  {}",
                crate::cli_style::dim(
                    "start: rantaiclaw channel start  ·  health: channel doctor  ·  setup: onboard"
                )
            );
            Ok(())
        }
        // Guidance, not failure. These bailed, so a script wrapping
        // `rantaiclaw channel add` saw a non-zero exit for what is an
        // informational outcome.
        crate::ChannelCommands::Add { channel_type } => {
            println!("{}", channel_add_guidance(&channel_type));
            Ok(())
        }
        crate::ChannelCommands::Remove { name } => {
            println!("{}", channel_remove_guidance(&name));
            Ok(())
        }
        crate::ChannelCommands::BindTelegram { identity } => {
            bind_telegram_identity(config, &identity).await
        }
        crate::ChannelCommands::UnbindTelegram { identity } => {
            unbind_telegram_identity(config, &identity).await
        }
        crate::ChannelCommands::Pair {
            channel,
            ttl,
            max_uses,
            no_owner,
        } => {
            pair_channel(&channel, ttl, max_uses, !no_owner)?;
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelHealthState {
    Healthy,
    Unhealthy,
    Timeout,
}

pub(crate) fn classify_health_result(
    result: &std::result::Result<bool, tokio::time::error::Elapsed>,
) -> ChannelHealthState {
    match result {
        Ok(true) => ChannelHealthState::Healthy,
        Ok(false) => ChannelHealthState::Unhealthy,
        Err(_) => ChannelHealthState::Timeout,
    }
}

/// Run health checks for configured channels.
pub async fn doctor_channels(config: Config) -> Result<()> {
    let channels = factory::build_configured_channels(&config);

    if channels.is_empty() {
        println!("No real-time channels configured. Run `rantaiclaw onboard` first.");
        return Ok(());
    }

    println!("🩺 RantaiClaw Channel Doctor");
    println!();

    let mut healthy = 0_u32;
    let mut unhealthy = 0_u32;
    let mut timeout = 0_u32;

    for (_key, name, channel) in channels {
        let result = tokio::time::timeout(Duration::from_secs(10), channel.health_check()).await;
        let state = classify_health_result(&result);

        match state {
            ChannelHealthState::Healthy => {
                healthy += 1;
                println!("  ✅ {name:<9} healthy");
            }
            ChannelHealthState::Unhealthy => {
                unhealthy += 1;
                println!("  ❌ {name:<9} unhealthy (auth/config/network)");
            }
            ChannelHealthState::Timeout => {
                timeout += 1;
                println!("  ⏱️  {name:<9} timed out (>10s)");
            }
        }
    }

    if config.channels_config.webhook.is_some() {
        println!("  ℹ️  Webhook   check via `rantaiclaw gateway` then GET /health");
    }

    println!();
    println!("Summary: {healthy} healthy, {unhealthy} unhealthy, {timeout} timed out");
    Ok(())
}
