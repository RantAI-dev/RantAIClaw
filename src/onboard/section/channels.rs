//! Channels setup section — wraps the existing channel-config wizard
//! in `crate::onboard::wizard::setup_channels`.
//!
//! Like `ProviderSection`, this is intentionally a thin adapter over the
//! existing 1.5k-line interview helper; lifting the body wholesale is
//! Wave 4+ scope (the spec breaks `channels` into per-platform subsections).
//!
//! `is_already_configured` returns `true` iff at least one non-CLI channel
//! is populated — the user is past the "no channels yet" state and we
//! shouldn't re-prompt by default.

use anyhow::Result;

use super::{SetupContext, SetupSection};
use crate::config::{ChannelsConfig, Config};
use crate::onboard::wizard;
use crate::profile::Profile;

pub struct ChannelsSection;

impl SetupSection for ChannelsSection {
    fn name(&self) -> &'static str {
        "channels"
    }

    fn description(&self) -> &'static str {
        "How RantaiClaw talks to you (Telegram / Discord / Slack / …)"
    }

    fn is_already_configured(&self, _profile: &Profile, config: &Config) -> bool {
        any_channel_set(&config.channels_config)
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        if !ctx.interactive {
            eprintln!("{}", self.headless_hint());
            return Ok(());
        }
        ctx.config.channels_config = wizard::setup_channels()?;
        // Unified approval model: a configured channel lets people CHAT, but
        // nobody can APPROVE a gated tool call until an owner is set. Owners get
        // the full toolset; everyone else is a "guest" under a capability
        // ceiling. If a multi-user channel was configured, offer to set both
        // now (and fall back to /claim guidance if no owner ends up set).
        if any_channel_set(&ctx.config.channels_config) {
            prompt_owners_and_guest_ceiling(ctx)?;
            if ctx.config.channels_config.approval_owners.is_empty() {
                print_owner_claim_guidance();
            }
        }
        Ok(())
    }

    fn headless_hint(&self) -> &'static str {
        "rantaiclaw channel add <platform> '<json>'  # see `rantaiclaw channel --help`\n\
         # then set an approval owner so in-chat /approve works:\n\
         #   [channels_config] approval_owners = [\"<your telegram id/username>\"]\n\
         #   (or DM the bot `/claim <code>` once it's running in pairing mode)"
    }
}

/// Interactively set channel owners and the non-owner ("guest") capability
/// ceiling, mutating `ctx.config.channels_config` in place via the shared
/// [`crate::approval::permissions`] editor (so the wizard, the CLI, and the
/// chat tool stay consistent). All prompts are optional and default to "no" —
/// declining leaves the secure defaults (no owner ⇒ owner-claim guidance is
/// printed by the caller; empty guest lists ⇒ guests get only read-only tools).
fn prompt_owners_and_guest_ceiling(ctx: &mut SetupContext) -> Result<()> {
    use crate::approval::permissions::{apply, Op, Target};
    use dialoguer::{theme::ColorfulTheme, Confirm, Input};

    let theme = ColorfulTheme::default();

    eprintln!();
    eprintln!("🔐 Owners get the FULL toolset on every channel and may approve tool calls.");
    eprintln!("   Everyone else who can chat is a \"guest\" under a capability ceiling you set.");

    // ── Owners ──────────────────────────────────────────────────
    let set_owner_now = Confirm::with_theme(&theme)
        .with_prompt("Add an approval owner now? (you can also DM the bot `/claim <code>` later)")
        .default(true)
        .interact()?;
    if set_owner_now {
        eprintln!(
            "   Enter the owner's account identity for your channel — e.g. a Telegram\n   \
             numeric user id, or a Slack/Discord/Matrix username. Blank line to finish."
        );
        loop {
            let entry: String = Input::with_theme(&theme)
                .with_prompt("  Owner identity (blank to finish)")
                .allow_empty(true)
                .interact_text()?;
            let entry = entry.trim().to_string();
            if entry.is_empty() {
                break;
            }
            let outcome = apply(ctx.config, Target::Owner, Op::Add, &entry);
            eprintln!("   {}", outcome.message);
        }
    }

    // ── Guest tool ceiling ──────────────────────────────────────
    let set_guest_now = Confirm::with_theme(&theme)
        .with_prompt("Configure what NON-owner users may do (guest ceiling)?")
        .default(false)
        .interact()?;
    if set_guest_now {
        eprintln!(
            "   Guests always get skills + read-only tools. Add extra tool names to widen\n   \
             that (comma-separated, e.g. `shell, web_search`). Blank to skip."
        );
        let tools: String = Input::with_theme(&theme)
            .with_prompt("  Guest tools to allow")
            .allow_empty(true)
            .interact_text()?;
        for t in tools.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let outcome = apply(ctx.config, Target::GuestTool, Op::Add, t);
            eprintln!("   {}", outcome.message);
        }

        if ctx
            .config
            .channels_config
            .guest_allowed_tools
            .iter()
            .any(|t| t == "shell")
        {
            eprintln!(
                "   You allowed the `shell` tool for guests. Restrict it to specific commands\n   \
                 (glob patterns, one per line, e.g. `kubectl get *`). Blank line to finish."
            );
            loop {
                let cmd: String = Input::with_theme(&theme)
                    .with_prompt("  Guest shell command glob (blank to finish)")
                    .allow_empty(true)
                    .interact_text()?;
                let cmd = cmd.trim().to_string();
                if cmd.is_empty() {
                    break;
                }
                let outcome = apply(ctx.config, Target::GuestCommand, Op::Add, &cmd);
                eprintln!("   {}", outcome.message);
            }
        }
    }

    // Make sure the owner can ask the bot to manage this from chat: install the
    // owner-permissions skill (idempotent) now that a multi-user channel exists,
    // even if the skills section was skipped.
    if let Err(e) = crate::skills::bundled::install_core_skills(ctx.profile) {
        eprintln!("   (note: could not install owner-permissions skill: {e})");
    }

    eprintln!(
        "   Tip: change any of this later with `rantaiclaw permissions ...`, the TUI\n   \
         `/permissions` command, or by asking the bot in chat (owners only)."
    );
    Ok(())
}

/// Print guidance for designating an approval owner over a channel. The
/// owner-authority gate (`channels_config.approval_owners`) is separate from a
/// channel's `allowed_users`: being able to chat does not let you approve a
/// privileged tool call. With no owner set, approval-required tools auto-deny
/// over channels (secure default). The recommended path is the `/claim <code>`
/// pairing flow — the bot records your real account id from chat.
fn print_owner_claim_guidance() {
    eprintln!();
    eprintln!("🔐 Approval owner (who can approve tool calls over a channel)");
    eprintln!(
        "   Your channel can chat now, but NO approval owner is set — any tool that\n   \
         needs approval will be auto-denied over the channel until you add one."
    );
    eprintln!("   Recommended — claim ownership from chat (captures your real id):");
    eprintln!("     1. Start the channel runtime:  rantaiclaw channels");
    eprintln!("        (with an empty allowed_users it prints a one-time pairing code)");
    eprintln!("     2. DM your bot:  /claim <code>   → registers you as an approval owner.");
    eprintln!("   Or set manually:  [channels_config] approval_owners = [\"<your id/username>\"]");
    eprintln!();
}

/// Returns `true` if any non-CLI channel has at least one configuration
/// block populated. CLI + webhook are bundled defaults and are not
/// evidence of user-driven configuration.
fn any_channel_set(c: &ChannelsConfig) -> bool {
    c.telegram.is_some()
        || c.discord.is_some()
        || c.slack.is_some()
        || c.mattermost.is_some()
        || c.imessage.is_some()
        || c.matrix.is_some()
        || c.signal.is_some()
        || c.whatsapp.is_some()
        || c.email.is_some()
        || c.irc.is_some()
        || c.lark.is_some()
        || c.dingtalk.is_some()
        || c.linq.is_some()
        || c.qq.is_some()
        || c.nextcloud_talk.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let s = ChannelsSection;
        assert_eq!(s.name(), "channels");
        assert!(!s.description().is_empty());
        assert!(s.headless_hint().contains("rantaiclaw channel"));
    }

    #[test]
    fn empty_config_is_not_configured() {
        let s = ChannelsSection;
        let dummy = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp/_rt_test"),
        };
        let cfg = Config::default();
        assert!(!s.is_already_configured(&dummy, &cfg));
    }
}
