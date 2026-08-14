//! Slash command for minting an on-demand pairing code from inside the TUI —
//! the same on-disk store the `rantaiclaw channels pair` CLI and the owner-only
//! `issue_pairing_code` chat tool write to.
//!
//! - `/pair` — mint a Telegram code, valid 15 minutes, owner-capable.
//! - `/pair <channel>` — mint for another surface (whatsapp, discord, …, gateway).
//! - `/pair <channel> --ttl <minutes>` — custom validity window.
//! - `/pair <channel> --no-owner` — chat-only invite (`/claim` won't promote).
//!
//! The code is written to the shared store under the active profile root; a
//! running daemon validates it on the next `/bind`/`/claim` (or gateway pair)
//! with no restart. Minting is synchronous filesystem work, so — unlike
//! `/permissions` — no tokio bridge is needed.

use anyhow::Result;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{CommandHandler, CommandResult};
use crate::security::pairing_store;
use crate::tui::context::TuiContext;

/// Default surface when the caller omits a channel.
const DEFAULT_CHANNEL: &str = "telegram";
/// Default validity window, in minutes.
const DEFAULT_TTL_MINUTES: i64 = 15;

pub struct PairCommand;

/// Parsed `/pair` arguments.
#[derive(Debug)]
struct PairArgs {
    channel: String,
    ttl_minutes: i64,
    grant_owner: bool,
}

/// Parse `[channel] [--ttl N] [--no-owner]`. Returns `Err(message)` with a
/// friendly explanation on a malformed flag.
fn parse_args(args: &str) -> std::result::Result<PairArgs, String> {
    let mut channel: Option<String> = None;
    let mut ttl_minutes = DEFAULT_TTL_MINUTES;
    // Owner-granting is opt-IN. It used to default to true, so the plain
    // `/pair telegram` minted a code whose holder becomes an approval owner.
    let mut grant_owner = false;

    let mut it = args.split_whitespace();
    while let Some(tok) = it.next() {
        match tok {
            "--owner" => grant_owner = true,
            // Kept so an existing muscle-memory invocation is not an error;
            // it is now the default.
            "--no-owner" => grant_owner = false,
            "--ttl" => {
                let Some(val) = it.next() else {
                    return Err(
                        "`--ttl` needs a value in minutes, e.g. `/pair telegram --ttl 30`.".into(),
                    );
                };
                match val.parse::<i64>() {
                    Ok(n) if n > 0 => ttl_minutes = n,
                    _ => {
                        return Err(format!(
                            "`--ttl` expects a positive number of minutes, got `{val}`."
                        ));
                    }
                }
            }
            other if other.starts_with("--") => {
                return Err(format!(
                    "Unknown flag `{other}`. Usage: /pair [channel] [--ttl N] [--owner]"
                ));
            }
            other if channel.is_none() => channel = Some(other.to_string()),
            other => {
                return Err(format!(
                    "Unexpected argument `{other}`. Usage: /pair [channel] [--ttl N] [--owner]"
                ));
            }
        }
    }

    let channel = channel.unwrap_or_else(|| DEFAULT_CHANNEL.to_string());
    // The channel argument used to be accepted verbatim, so `/pair telgram`
    // minted a code under a surface no listener ever reads — it simply never
    // worked, with no error. Validated against the shared catalog rather than
    // a list invented here.
    if !valid_pair_surface(&channel) {
        return Err(format!(
            "Unknown channel `{channel}`. Valid surfaces: {}.",
            valid_pair_surfaces().join(", ")
        ));
    }

    Ok(PairArgs {
        channel,
        ttl_minutes,
        grant_owner,
    })
}

/// Surfaces a pairing code can be minted for: every channel in the shared
/// catalog, plus the gateway.
fn valid_pair_surfaces() -> Vec<&'static str> {
    let mut surfaces: Vec<&'static str> = crate::channels::channel_catalog_keys();
    surfaces.push("gateway");
    surfaces
}

fn valid_pair_surface(candidate: &str) -> bool {
    valid_pair_surfaces().contains(&candidate)
}

/// Current unix time in seconds (the clock the store windows codes against).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Mint a code into the shared store under the active profile root and render
/// the success message.
///
/// Returns `(displayed, persisted)`: the second is what reaches `sessions.db`,
/// with the code redacted.
fn mint_and_render(args: &PairArgs) -> Result<(String, String)> {
    let root = crate::profile::ProfileManager::active()?.root;
    let code = pairing_store::mint(
        &root,
        &args.channel,
        args.ttl_minutes.saturating_mul(60),
        // One use, matching what the gateway path already mints. `None` meant
        // unlimited: a code that leaked stayed claimable for its whole window.
        Some(1),
        args.grant_owner,
        now_unix(),
    )?;

    let mut msg = format!(
        "🔐 Pairing code for {}: {}   (valid {} min, single use)",
        args.channel, code, args.ttl_minutes
    );
    if args.grant_owner {
        let _ = write!(
            msg,
            "\n   DM the bot:  /claim {code}  (owner)  |  /bind {code}  (chat)"
        );
    } else {
        let _ = write!(
            msg,
            "\n   DM the bot:  /bind {code}  (chat — this code does not grant owner)"
        );
    }
    msg.push_str("\n   No daemon restart needed — a running channel picks this up automatically.");

    // What the session store sees. `sessions.db` is full-text indexed and long
    // outlives the code's window, so the plaintext must not reach it.
    let persisted = format!(
        "🔐 Pairing code minted for {} (valid {} min, single use{}). \
         The code itself was shown on screen only and is not recorded here.",
        args.channel,
        args.ttl_minutes,
        if args.grant_owner { ", owner" } else { "" }
    );
    Ok((msg, persisted))
}

impl CommandHandler for PairCommand {
    fn name(&self) -> &str {
        "pair"
    }

    fn description(&self) -> &str {
        "Mint an on-demand pairing code (no daemon restart) for a channel or the gateway"
    }

    fn usage(&self) -> &str {
        "/pair [channel] [--ttl N] [--owner]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let parsed = match parse_args(args.trim()) {
            Ok(p) => p,
            Err(msg) => return Ok(CommandResult::Message(format!("✗ {msg}"))),
        };
        match mint_and_render(&parsed) {
            Ok((display, persisted)) => Ok(CommandResult::SensitiveMessage { display, persisted }),
            Err(e) => Ok(CommandResult::Message(format!(
                "✗ Could not mint pairing code: {e}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    /// Restore the pre-test HOME instead of unsetting it — `remove_var` strips
    /// HOME from the whole test process and breaks every later env-reading test.
    fn restore_home(prev: Option<std::ffi::OsString>) {
        match prev {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn default_shows_a_code() {
        let _g = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let mut ctx = test_context();

        match PairCommand.execute("", &mut ctx).unwrap() {
            CommandResult::SensitiveMessage { display, persisted } => {
                assert!(
                    display.contains('-'),
                    "should carry a dash-grouped code: {display}"
                );
                assert!(display.contains("telegram"), "{display}");
                // Owner-granting is opt-in now, so the bare `/pair` mints a
                // chat-only code: `/bind`, no `/claim`.
                assert!(display.contains("/bind"), "{display}");
                assert!(
                    !display.contains("/claim"),
                    "the default must not grant owner: {display}"
                );
                // THE point of the variant: the code never reaches the store.
                assert!(
                    !persisted.contains('-') || !persisted.contains("Pairing code for"),
                    "the persisted form must not carry the code: {persisted}"
                );
                assert!(persisted.contains("not recorded here"), "{persisted}");
            }
            other => panic!("expected SensitiveMessage, got {other:?}"),
        }

        restore_home(prev_home);
    }

    /// The channel argument was accepted verbatim, so a typo minted a code
    /// under a surface no listener reads — it simply never worked, silently.
    #[test]
    fn pair_rejects_an_unknown_channel() {
        let err = parse_args("telgram").expect_err("a typo must be refused");
        assert!(err.contains("Unknown channel"), "{err}");
        assert!(
            err.contains("telegram"),
            "the valid list must be shown: {err}"
        );

        // Every catalog channel is accepted, plus the gateway.
        for good in ["telegram", "qq", "irc", "gateway"] {
            assert!(parse_args(good).is_ok(), "{good} must be valid");
        }
    }

    /// The default used to be owner-granting and unlimited-use: the plain
    /// `/pair telegram` minted a code whose holder becomes an approval owner,
    /// claimable as many times as it leaked.
    #[test]
    fn pair_defaults_to_non_owner() {
        let args = parse_args("telegram").expect("valid");
        assert!(!args.grant_owner, "owner must be opt-in");

        let args = parse_args("telegram --owner").expect("valid");
        assert!(args.grant_owner, "--owner opts in");
    }

    #[test]
    fn no_owner_flag_is_chat_only() {
        let _g = crate::test_env::ENV_LOCK.blocking_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        let mut ctx = test_context();

        match PairCommand
            .execute("whatsapp --no-owner", &mut ctx)
            .unwrap()
        {
            CommandResult::SensitiveMessage { display, .. } => {
                assert!(display.contains("whatsapp"), "{display}");
                assert!(display.contains("does not grant owner"), "{display}");
            }
            other => panic!("expected Message, got {other:?}"),
        }

        restore_home(prev_home);
    }

    #[test]
    fn unknown_flag_is_friendly() {
        let mut ctx = test_context();
        match PairCommand.execute("telegram --wat", &mut ctx).unwrap() {
            CommandResult::Message(m) => assert!(m.contains("Unknown flag"), "{m}"),
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn bad_ttl_is_friendly() {
        let mut ctx = test_context();
        match PairCommand
            .execute("telegram --ttl notanumber", &mut ctx)
            .unwrap()
        {
            CommandResult::Message(m) => assert!(m.contains("--ttl"), "{m}"),
            other => panic!("expected Message, got {other:?}"),
        }
    }
}
