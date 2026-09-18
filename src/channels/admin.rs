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
use super::traits::Channel;
use super::{
    channel_is_configured, channel_is_usable, channel_roster_note, ChannelSupport,
    ChannelVerification, CHANNEL_CATALOG, NON_CHANNEL_CATALOG_KEYS, OPENRC_RESTART_ARGS,
    OPENRC_STATUS_ARGS, SYSTEMD_STATUS_ARGS,
};
use crate::config::Config;
use crate::doctor::checks::channels::{probe_whatsapp_web, ProbeWebResult};
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
    // `announce_daemon_reload` shells out synchronously; keep it off the worker.
    tokio::task::spawn_blocking(announce_daemon_reload)
        .await
        .map_err(|e| anyhow::anyhow!("daemon reload panicked: {e}"))?;
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
    // `announce_daemon_reload` shells out synchronously; keep it off the worker.
    tokio::task::spawn_blocking(announce_daemon_reload)
        .await
        .map_err(|e| anyhow::anyhow!("daemon reload panicked: {e}"))?;
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
///
/// **Blocking**: `maybe_restart_managed_daemon_service` shells out to
/// `launchctl`/`rc-service`/`systemctl` synchronously (a restart blocks up to
/// the unit's stop timeout). Async callers must run this via
/// `tokio::task::spawn_blocking`. CLI/TUI/headless callers live in their own
/// process and can wait for the outcome; the in-daemon gateway uses
/// `reload_managed_daemon_non_blocking` instead.
pub(crate) fn announce_daemon_reload() {
    match maybe_restart_managed_daemon_service(true) {
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
///
/// **Blocking**: shells out to `launchctl`/`rc-service`/`systemctl` synchronously
/// (a restart blocks up to the unit's stop timeout). Only safe for callers
/// running in their own process — the CLI, the TUI, the headless setup path.
/// For callers running **inside** the daemon (the gateway), the blocking call
/// would deadlock against the daemon's own SIGTERM and get SIGKILLed; use
/// [`reload_managed_daemon_non_blocking`] instead.
pub(crate) fn reload_managed_daemon() -> Result<bool> {
    maybe_restart_managed_daemon_service(true)
}

/// Same as [`reload_managed_daemon`], but never waits for the restart job to
/// finish. For callers that live in the same process as the daemon they are
/// asking to be restarted — the gateway, reached via
/// `gateway::config_api::schedule_daemon_reload`. On systemd it adds
/// `--no-block` so `systemctl` queues the job and returns at once; on launchd
/// it spawns the kickstart detached and returns without confirming the job
/// re-listed (the operator is still the one to watch the journal); on OpenRC
/// it spawns the restart detached for the same reason. The caller cannot tell
/// whether the restart actually succeeded, so its log line says *requested*,
/// not *reloaded*. Returns `Ok(true)` when the command was queued, `Ok(false)`
/// when no managed service is installed.
///
/// **Risk note (OpenRC)**: `rc-service restart` has no non-blocking form, so
/// the in-daemon variant detaches the command. The child becomes a child of
/// init when this daemon exits, which is the path the change's `STOP`
/// condition names as potentially unsafe. The project's service installer
/// produces systemd on Linux, so OpenRC is not the shipping target; the
/// blocking form stays correct for OpenRC under the CLI/TUI/headless paths.
pub(crate) fn reload_managed_daemon_non_blocking() -> Result<bool> {
    maybe_restart_managed_daemon_service(false)
}

/// Current uid for the launchd `gui/<uid>/<label>` domain target. `id -u` is
/// universally available on macOS; the `gui/<uid>` prefix lets `kickstart` reach
/// a user agent without root. Cross-platform-compilable (the block is guarded by
/// a runtime `cfg!` check, not a `#[cfg]` attribute), but only called on macOS.
fn macos_launchctl_uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0".to_string())
}

/// Build the `systemctl --user` arguments for a restart in one place so the
/// in-daemon and out-of-process variants stay in sync. `blocking = false` adds
/// `--no-block`, which queues the job and returns at once; that is what
/// prevents the daemon from deadlocking against its own SIGTERM.
pub(crate) fn systemd_restart_args(blocking: bool) -> &'static [&'static str] {
    if blocking {
        &["--user", "restart", "rantaiclaw.service"]
    } else {
        &["--user", "--no-block", "restart", "rantaiclaw.service"]
    }
}

pub(crate) fn maybe_restart_managed_daemon_service(blocking: bool) -> Result<bool> {
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

        // A discarded `launchctl stop` followed by `start` raced: `start`
        // reported success while the old instance was still tearing down (or the
        // new one had already died), so the caller was told "reloaded" when the
        // daemon could be stale or dead. `kickstart -k` atomically kills and
        // restarts the job in one call (mirrors `handoff::Launchd::restart`).
        // For the blocking form we then confirm the job is actually listed
        // before claiming success. The non-blocking form cannot do that — the
        // caller is the very job being killed, so it returns `Ok(true)` as
        // soon as `kickstart` is queued and leaves verification to the
        // operator's journal.
        let target = format!("gui/{}/com.rantaiclaw.daemon", macos_launchctl_uid());
        let mut cmd = Command::new("launchctl");
        cmd.args(["kickstart", "-k", &target]);
        if blocking {
            let kick = cmd
                .output()
                .context("Failed to kickstart launchd daemon service")?;
            if !kick.status.success() {
                let stderr = String::from_utf8_lossy(&kick.stderr);
                anyhow::bail!("launchctl kickstart -k {target} failed: {}", stderr.trim());
            }
            let after = Command::new("launchctl")
                .arg("list")
                .output()
                .context("Failed to query launchctl list after kickstart")?;
            if !String::from_utf8_lossy(&after.stdout).contains("com.rantaiclaw.daemon") {
                anyhow::bail!(
                    "launchctl kickstart reported success but com.rantaiclaw.daemon is not listed"
                );
            }
        } else {
            cmd.spawn()
                .context("Failed to spawn launchctl kickstart (detached)")?;
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
                    if blocking {
                        let restart_output = Command::new("rc-service")
                            .args(OPENRC_RESTART_ARGS)
                            .output()
                            .context("Failed to restart OpenRC daemon service")?;
                        if !restart_output.status.success() {
                            let stderr = String::from_utf8_lossy(&restart_output.stderr);
                            anyhow::bail!("rc-service restart failed: {}", stderr.trim());
                        }
                    } else {
                        // Detached: see the OpenRC risk note on
                        // `reload_managed_daemon_non_blocking`.
                        Command::new("rc-service")
                            .args(OPENRC_RESTART_ARGS)
                            .spawn()
                            .context("Failed to spawn rc-service restart (detached)")?;
                    }
                    return Ok(true);
                }
            }
        }

        // Systemd (user-level)
        // Honor XDG_CONFIG_HOME via the shared helper, so this detection matches
        // where `service install` actually wrote the unit.
        let unit_path: PathBuf =
            crate::service::systemd_user_unit_dir()?.join("rantaiclaw.service");
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
            .args(systemd_restart_args(blocking))
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

pub(crate) fn channel_roster(
    config: &Config,
) -> Vec<(
    &'static str,
    bool,
    crate::channels::ChannelSupport,
    crate::channels::ChannelVerification,
)> {
    CHANNEL_CATALOG
        .iter()
        .map(|(key, display, support, verification)| {
            (
                *display,
                channel_is_configured(key, config),
                *support,
                *verification,
            )
        })
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
            for (name, configured, support, verification) in channel_roster(config) {
                crate::cli_style::status_row(
                    configured,
                    name,
                    14,
                    &crate::channels::channel_roster_note(configured, support, verification),
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
            if let Some(refusal) = crate::security::pairing_store::whatsapp_surface_refusal(
                &channel,
                config.channels_config.running_whatsapp_surface(),
            ) {
                anyhow::bail!(refusal);
            }
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

/// WhatsApp Web's in-process client and bot task only exist once `listen` has
/// run, which happens in the daemon, never in this CLI process — so asking a
/// freshly built channel object whether it holds one always answers no, blaming
/// auth, config or network for a problem that does not exist. Judge it instead
/// by the same offline session-file probe the general doctor check already
/// uses, and word the verdict around what was actually checked: the saved
/// session, not a live connection this process never opens.
async fn whatsapp_web_doctor_state(config: &Config) -> (ChannelHealthState, String) {
    let session_path = config
        .channels_config
        .whatsapp_web
        .as_ref()
        .expect(
            "channel_doctor_state is only called with key \"whatsapp_web\" for a channel \
             build_configured_channels registered, and that only happens when \
             config.channels_config.whatsapp_web is Some",
        )
        .session_path
        .clone();

    // `probe_whatsapp_web` is a blocking filesystem call; bound it the same
    // 10s every other channel's health check is bounded to, so a stalled
    // network mount under the session path cannot hang the whole command.
    let probed = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || probe_whatsapp_web(&session_path)),
    )
    .await;

    match probed {
        Ok(Ok(ProbeWebResult::Ok)) => (ChannelHealthState::Healthy, "session present".to_string()),
        Ok(Ok(ProbeWebResult::SessionMissing)) => (
            ChannelHealthState::Unhealthy,
            "no session file yet; run `rantaiclaw setup whatsapp-web` to link".to_string(),
        ),
        Ok(Ok(ProbeWebResult::SessionPathBad(e))) => (
            ChannelHealthState::Unhealthy,
            format!("session file unreadable: {e}"),
        ),
        Ok(Err(_)) => (
            ChannelHealthState::Unhealthy,
            "session probe task panicked".to_string(),
        ),
        Err(_) => (ChannelHealthState::Timeout, "timed out (>10s)".to_string()),
    }
}

/// A channel judged by asking the object built in this process whether it
/// already holds a live connection, wrapped in the doctor's shared timeout.
/// Wrong for a channel whose connection only exists in the daemon process
/// (see `whatsapp_web_doctor_state`), right for every channel that connects
/// on demand instead of through a long-running listener.
async fn in_process_doctor_state(channel: &dyn Channel) -> (ChannelHealthState, String) {
    let result = tokio::time::timeout(Duration::from_secs(10), channel.health_check()).await;
    let state = classify_health_result(&result);
    let detail = match state {
        ChannelHealthState::Healthy => "healthy".to_string(),
        ChannelHealthState::Unhealthy => "unhealthy (auth/config/network)".to_string(),
        ChannelHealthState::Timeout => "timed out (>10s)".to_string(),
    };
    (state, detail)
}

/// One configured channel's doctor verdict, keyed by its registry key so a
/// channel whose live state only exists in the daemon process can be judged a
/// different way than one this CLI process can ask directly.
async fn channel_doctor_state(
    key: &str,
    channel: &dyn Channel,
    config: &Config,
) -> (ChannelHealthState, String) {
    if key == "whatsapp_web" {
        whatsapp_web_doctor_state(config).await
    } else {
        in_process_doctor_state(channel).await
    }
}

/// Run health checks for configured channels.
/// Every catalog channel whose table is configured but is locked, so the
/// factory deliberately did not build it. Pulled out of [`doctor_channels`]
/// so the "a locked-only config still gets a real answer" case is testable
/// without capturing stdout.
fn locked_configured_channels(
    config: &Config,
) -> Vec<(
    &'static str,
    &'static str,
    ChannelSupport,
    ChannelVerification,
)> {
    CHANNEL_CATALOG
        .iter()
        .copied()
        .filter(|(key, _, _, _)| {
            !NON_CHANNEL_CATALOG_KEYS.contains(key)
                && channel_is_configured(key, config)
                && !channel_is_usable(key)
        })
        .collect()
}

pub async fn doctor_channels(config: Config) -> Result<()> {
    factory::warn_unused_channel_config(&config);
    let channels = factory::build_configured_channels(&config);

    // Computed first so a config whose only channel is locked still gets a
    // real answer instead of falling into the "nothing configured" branch
    // beneath it.
    let locked = locked_configured_channels(&config);

    if channels.is_empty() && locked.is_empty() {
        println!("No real-time channels configured. Run `rantaiclaw onboard` first.");
        return Ok(());
    }

    println!("🩺 RantaiClaw Channel Doctor");
    println!();

    let mut healthy = 0_u32;
    let mut unhealthy = 0_u32;
    let mut timeout = 0_u32;

    for (key, name, channel) in channels {
        let (state, detail) = channel_doctor_state(key, channel.as_ref(), &config).await;

        match state {
            ChannelHealthState::Healthy => {
                healthy += 1;
                println!("  ✅ {name:<9} {detail}");
            }
            ChannelHealthState::Unhealthy => {
                unhealthy += 1;
                println!("  ❌ {name:<9} {detail}");
            }
            ChannelHealthState::Timeout => {
                timeout += 1;
                println!("  ⏱️  {name:<9} {detail}");
            }
        }
    }

    for (_, display, support, verification) in &locked {
        println!(
            "  🔒 {display:<9} {}",
            channel_roster_note(true, *support, *verification)
        );
    }

    if config.channels_config.webhook.is_some() {
        println!("  ℹ️  Webhook   check via `rantaiclaw gateway` then GET /health");
    }

    println!();
    println!("Summary: {healthy} healthy, {unhealthy} unhealthy, {timeout} timed out");
    Ok(())
}

#[cfg(test)]
mod doctor_channels_tests {
    use super::*;

    /// A config whose only channel is locked must still name it, not fall
    /// into the "nothing configured" branch `doctor_channels` uses when
    /// nothing at all is set up.
    #[test]
    fn a_locked_only_config_is_not_reported_as_unconfigured() {
        let mut config = Config::default();
        config.channels_config.irc = Some(crate::config::schema::IrcConfig {
            server: "irc.example.org".into(),
            port: 6697,
            nickname: "bot".into(),
            username: None,
            channels: vec!["#c".into()],
            allowed_users: vec![],
            server_password: None,
            nickserv_password: None,
            sasl_password: None,
            verify_tls: None,
            allow_insecure_tls_with_password: false,
        });

        let locked = locked_configured_channels(&config);
        assert_eq!(
            locked.len(),
            1,
            "expected exactly the locked irc table, got {locked:?}"
        );
        assert_eq!(locked[0].0, "irc");
        assert!(
            factory::build_configured_channels(&config).is_empty(),
            "the factory must not build a locked channel"
        );
    }

    /// A config with only usable channels has nothing locked to report.
    #[test]
    fn a_fully_usable_config_has_nothing_locked() {
        let config = config_with_session("/nonexistent/rantaiclaw-guard/whatsapp.db");
        assert!(locked_configured_channels(&config).is_empty());
    }

    fn config_with_session(session_path: &str) -> Config {
        let mut config = Config::default();
        config.channels_config.whatsapp_web = Some(crate::config::schema::WhatsAppWebConfig {
            session_path: session_path.to_string(),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["+15550000001".into()],
        });
        config
    }

    #[tokio::test]
    async fn a_readable_session_file_reads_healthy_and_says_so() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.db");
        std::fs::write(&path, b"not empty").expect("write a fixture session file");

        let (state, detail) =
            whatsapp_web_doctor_state(&config_with_session(path.to_str().expect("utf8 path")))
                .await;

        assert_eq!(state, ChannelHealthState::Healthy);
        assert_eq!(detail, "session present");
    }

    #[tokio::test]
    async fn a_missing_session_names_that_specifically_not_the_generic_wording() {
        let (state, detail) =
            whatsapp_web_doctor_state(&config_with_session("/nonexistent/path/to/session.db"))
                .await;

        assert_eq!(state, ChannelHealthState::Unhealthy);
        assert!(
            detail.contains("session"),
            "must name the session, not a generic cause: {detail}"
        );
        assert_ne!(
            detail, "unhealthy (auth/config/network)",
            "must not reuse the generic wording meant for channels that need a live connection"
        );
    }

    /// The bug this plan fixes: a fresh, CLI-built `WhatsAppWebChannel` never
    /// ran `listen`, so it never holds a live client, and `health_check` on it
    /// always answers false regardless of whether the daemon is actually
    /// connected. Proven through the exact registration `doctor_channels`
    /// itself uses (`factory::build_configured_channels`, not a hand-built
    /// channel with a hardcoded key), so a future rename of either side's key
    /// string in isolation fails this test rather than silently regressing to
    /// always-unhealthy with every test still green.
    #[cfg(feature = "whatsapp-web")]
    #[tokio::test]
    async fn the_doctor_dispatch_reads_a_present_session_healthy_though_health_check_would_not() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.db");
        std::fs::write(&path, b"not empty").expect("write a fixture session file");
        let config = config_with_session(path.to_str().expect("utf8 path"));

        let (key, _name, channel) = factory::build_configured_channels(&config)
            .into_iter()
            .find(|(key, ..)| *key == "whatsapp_web")
            .expect("a filled whatsapp_web table registers under the real doctor_channels path");
        assert!(
            !channel.health_check().await,
            "control: a freshly built channel object never ran `listen`, so it must not \
             already claim a live client"
        );

        let (state, detail) = channel_doctor_state(key, channel.as_ref(), &config).await;
        assert_eq!(
            state,
            ChannelHealthState::Healthy,
            "the doctor's own verdict must come from the session file, not the channel object: {detail}"
        );
        assert_eq!(detail, "session present");
    }

    /// A channel keyed by anything other than `whatsapp_web` must still go
    /// through the in-process `health_check` path (default `true`) — this
    /// plan changes what judges WhatsApp Web specifically, not every channel.
    #[tokio::test]
    async fn a_different_key_still_goes_through_health_check() {
        struct AlwaysHealthy;
        #[async_trait::async_trait]
        impl Channel for AlwaysHealthy {
            fn name(&self) -> &str {
                "always-healthy-test-double"
            }

            async fn send(&self, _message: &super::super::SendMessage) -> Result<()> {
                unimplemented!("not exercised by this test")
            }

            async fn listen(
                &self,
                _tx: tokio::sync::mpsc::Sender<crate::channels::traits::ChannelMessage>,
                _cancel: tokio_util::sync::CancellationToken,
            ) -> Result<()> {
                unimplemented!("not exercised by this test")
            }
        }

        let (state, detail) =
            channel_doctor_state("not-whatsapp-web", &AlwaysHealthy, &Config::default()).await;
        assert_eq!(state, ChannelHealthState::Healthy);
        assert_eq!(detail, "healthy");
    }
}
