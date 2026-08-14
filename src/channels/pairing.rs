//! Shared `/bind` / `/claim` self-onboarding for multi-user channels.
//!
//! Every multi-user channel routes inbound text through [`try_handle_pairing`]
//! *before* agent dispatch. The helper parses a pairing command, validates the
//! code against the shared [`crate::security::pairing_store`], and — on a hit —
//! appends the sender's native identity to that channel's allowlist field in
//! `config.toml` (and, for an owner `/claim` against an owner-capable code, to
//! `channels_config.approval_owners`). Pairing messages are consumed: the
//! caller must not forward a handled message to the agent.
//!
//! Identity forms differ per channel, so a code is *surface-scoped* in the
//! store and the allowlist field is selected via [`AllowlistField`]. The actual
//! channel→config mapping lives in [`apply_pairing`].

use crate::config::ChannelsConfig;
use crate::security::pairing_store;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Which per-channel allowlist field a successful pairing appends to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowlistField {
    /// `allowed_users` — telegram/discord/slack/mattermost/matrix/irc/lark/
    /// dingtalk/qq/nextcloud_talk.
    AllowedUsers,
    /// `allowed_numbers` — whatsapp (cloud + web).
    AllowedNumbers,
    /// `allowed_from` — signal.
    AllowedFrom,
    /// `allowed_senders` — linq.
    AllowedSenders,
    /// `allowed_contacts` — imessage.
    AllowedContacts,
}

/// A parsed pairing command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCommand {
    /// `true` for `/claim` (request owner), `false` for `/bind` (chat only).
    pub owner: bool,
    /// The plaintext code as typed (normalization happens in the store).
    pub code: String,
}

/// Current unix time in seconds (runtime clock; tests pass `now` to the store
/// directly).
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether a code looks plausible: non-empty and only Crockford-base32-ish
/// characters once group separators are stripped. Deliberately permissive on
/// case and dashes (the store normalizes); rejects obvious non-codes so a
/// normal message starting with `/bind`/`/claim` plus garbage doesn't mint a
/// spurious store lookup with control characters.
fn looks_like_code(code: &str) -> bool {
    let stripped: String = code.chars().filter(|c| *c != '-').collect();
    let stripped = stripped.trim();
    !stripped.is_empty()
        && stripped.len() <= 64
        && stripped.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Parse `/claim <code>` (owner) or `/bind <code>` (chat-only). Returns `None`
/// for anything else so normal messages flow through to the agent.
pub fn parse_pairing_command(text: &str) -> Option<PairingCommand> {
    let trimmed = text.trim();
    let (owner, rest) = if let Some(rest) = trimmed.strip_prefix("/claim") {
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix("/bind") {
        (false, rest)
    } else {
        return None;
    };
    // Require whitespace between the verb and the code so `/claimfoo` (a single
    // token) isn't mistaken for a command. An exact `/claim` / `/bind` with no
    // argument leaves an empty `rest`, which fails the code check below.
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let code = rest.split_whitespace().next().unwrap_or("");
    if !looks_like_code(code) {
        return None;
    }
    Some(PairingCommand {
        owner,
        code: code.to_string(),
    })
}

/// Append `identity` to `list` if not already present (case-sensitive dedupe).
fn push_unique(list: &mut Vec<String>, identity: &str) {
    let identity = identity.trim();
    if identity.is_empty() {
        return;
    }
    if !list.iter().any(|x| x == identity) {
        list.push(identity.to_string());
    }
}

/// Apply a successful pairing to `cc`: append each identity (deduped) to the
/// matched channel's allowlist field; if `make_owner`, also append (deduped) to
/// `cc.approval_owners`. No-op if the channel's config section is `None`.
/// The active profile root, for the shared pairing-code store.
///
/// Was copy-pasted into fifteen channel files; the iMessage copy had silently
/// dropped its error log, so a profile-resolution failure there looked like
/// "no pairing code matched".
pub fn profile_root(channel: &str) -> Option<std::path::PathBuf> {
    match crate::profile::ProfileManager::active() {
        Ok(p) => Some(p.root),
        Err(e) => {
            tracing::warn!("{channel} pairing: couldn't resolve profile root: {e:#}");
            None
        }
    }
}

/// The allowlist field a channel's identities belong in.
///
/// `apply_pairing` used to take an `AllowlistField` and consume it with
/// a discard binding, so passing the wrong one for a channel was undetectable by
/// construction — the parameter gave the appearance of a check that did not
/// exist. It is now resolved from the channel here, and `apply_pairing`
/// verifies the caller's argument against it.
pub fn expected_field_for(channel: &str) -> Option<AllowlistField> {
    Some(match channel {
        "telegram" | "discord" | "slack" | "mattermost" | "matrix" | "irc" | "lark"
        | "dingtalk" | "qq" | "nextcloud_talk" => AllowlistField::AllowedUsers,
        "signal" => AllowlistField::AllowedFrom,
        "whatsapp" => AllowlistField::AllowedNumbers,
        "linq" => AllowlistField::AllowedSenders,
        "imessage" => AllowlistField::AllowedContacts,
        _ => return None,
    })
}

pub fn apply_pairing(
    cc: &mut ChannelsConfig,
    channel: &str,
    field: AllowlistField,
    identities: &[String],
    make_owner: bool,
) {
    // The field is now checked rather than ignored. A mismatch means the caller
    // and this function disagree about which list a channel's identities go in,
    // which is a programming error worth surfacing loudly — but not worth
    // writing the wrong list over, so it returns.
    match expected_field_for(channel) {
        Some(expected) if expected == field => {}
        Some(expected) => {
            tracing::error!(
                "pairing: {channel} expects {expected:?} but the caller passed {field:?}; \
                 refusing to write the wrong allowlist"
            );
            return;
        }
        None => {
            tracing::warn!("pairing: unknown channel {channel}; nothing to append to");
            return;
        }
    }

    // Resolve the target allowlist Vec. `channel` selects the config section.
    let target: Option<&mut Vec<String>> = match channel {
        "telegram" => cc.telegram.as_mut().map(|c| &mut c.allowed_users),
        "discord" => cc.discord.as_mut().map(|c| &mut c.allowed_users),
        "slack" => cc.slack.as_mut().map(|c| &mut c.allowed_users),
        "mattermost" => cc.mattermost.as_mut().map(|c| &mut c.allowed_users),
        "matrix" => cc.matrix.as_mut().map(|c| &mut c.allowed_users),
        "irc" => cc.irc.as_mut().map(|c| &mut c.allowed_users),
        "lark" => cc.lark.as_mut().map(|c| &mut c.allowed_users),
        "dingtalk" => cc.dingtalk.as_mut().map(|c| &mut c.allowed_users),
        "qq" => cc.qq.as_mut().map(|c| &mut c.allowed_users),
        "nextcloud_talk" => cc.nextcloud_talk.as_mut().map(|c| &mut c.allowed_users),
        "signal" => cc.signal.as_mut().map(|c| &mut c.allowed_from),
        "whatsapp" => cc.whatsapp.as_mut().map(|c| &mut c.allowed_numbers),
        "linq" => cc.linq.as_mut().map(|c| &mut c.allowed_senders),
        "imessage" => cc.imessage.as_mut().map(|c| &mut c.allowed_contacts),
        _ => None,
    };

    let Some(list) = target else {
        // Channel section not configured — nothing to append to.
        return;
    };

    for id in identities {
        push_unique(list, id);
    }

    if make_owner {
        for id in identities {
            push_unique(&mut cc.approval_owners, id);
        }
    }
}

/// Full self-onboarding flow for one inbound message.
///
/// Returns `Some(reply)` when `text` WAS a pairing command (so the caller must
/// NOT forward it to the agent), `None` otherwise. On a valid code, mutates and
/// persists `config.toml` via [`apply_pairing`] + `Config::save`.
pub async fn try_handle_pairing(
    text: &str,
    surface: &str,
    field: AllowlistField,
    identities: &[String],
    profile_root: &Path,
) -> Option<String> {
    let cmd = parse_pairing_command(text)?;

    let outcome = match pairing_store::try_consume(profile_root, surface, &cmd.code, now_unix()) {
        Ok(Some(outcome)) => outcome,
        Ok(None) => {
            return Some(
                "❌ Invalid or expired pairing code. Ask the operator for a fresh one (`rantaiclaw channels pair`).".to_string(),
            );
        }
        Err(e) => {
            tracing::warn!("pairing store read failed for {surface}: {e:#}");
            return Some(
                "❌ Couldn't verify the pairing code right now. Please try again shortly."
                    .to_string(),
            );
        }
    };

    let make_owner = cmd.owner && outcome.grant_owner;

    let mut config = match crate::config::Config::load_or_init().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("pairing config load failed for {surface}: {e:#}");
            return Some(
                "❌ Pairing succeeded but I couldn't update the allowlist. Tell the operator to check the config.".to_string(),
            );
        }
    };

    apply_pairing(
        &mut config.channels_config,
        surface,
        field,
        identities,
        make_owner,
    );

    if let Err(e) = config.save().await {
        tracing::warn!("pairing config save failed for {surface}: {e:#}");
        return Some(
            "❌ Pairing succeeded but saving the allowlist failed. Tell the operator to check the config.".to_string(),
        );
    }

    if make_owner {
        Some(
            "✅ You're now an owner — you can chat with me and approve privileged actions."
                .to_string(),
        )
    } else if cmd.owner {
        // Asked for owner but the code wasn't owner-capable; still bound for chat.
        Some(
            "✅ You're paired and can chat with me. (This code didn't grant owner rights.)"
                .to_string(),
        )
    } else {
        Some("✅ You're paired — you can now chat with me.".to_string())
    }
}

#[cfg(test)]
mod tests {
    /// The iMessage copy of this helper had silently dropped its error log, so
    /// a profile-resolution failure there read as "no pairing code matched".
    #[test]
    fn profile_root_names_the_channel_in_its_warning() {
        // The warning text is what makes a failure attributable; assert the
        // format string carries the channel, since a `tracing` sink is not
        // available here.
        let src = include_str!("pairing.rs");
        let production = src.split("#[cfg(test)]").next().expect("source");
        assert!(
            production.contains("{channel} pairing: couldn't resolve profile root"),
            "the shared helper must name the channel it failed for"
        );
        // And there is exactly one implementation left to name it.
        assert_eq!(production.matches("fn profile_root(").count(), 1);
    }

    use super::*;
    use crate::config::{ChannelsConfig, StreamMode, TelegramConfig};

    // ── parse_pairing_command ────────────────────────────────

    #[test]
    fn parse_claim_sets_owner_true() {
        let cmd = parse_pairing_command("/claim ABCD-EFGH").unwrap();
        assert!(cmd.owner);
        assert_eq!(cmd.code, "ABCD-EFGH");
    }

    #[test]
    fn parse_bind_sets_owner_false() {
        let cmd = parse_pairing_command("/bind ABCD-EFGH").unwrap();
        assert!(!cmd.owner);
        assert_eq!(cmd.code, "ABCD-EFGH");
    }

    #[test]
    fn parse_trims_surrounding_whitespace() {
        let cmd = parse_pairing_command("   /bind   WXYZ-1234   ").unwrap();
        assert!(!cmd.owner);
        assert_eq!(cmd.code, "WXYZ-1234");
    }

    #[test]
    fn parse_takes_first_token_as_code() {
        let cmd = parse_pairing_command("/claim ABCD-EFGH please").unwrap();
        assert_eq!(cmd.code, "ABCD-EFGH");
    }

    #[test]
    fn parse_non_command_is_none() {
        assert!(parse_pairing_command("hello there").is_none());
        assert!(parse_pairing_command("/help").is_none());
        assert!(parse_pairing_command("just /bind in the middle").is_none());
    }

    #[test]
    fn parse_command_without_code_is_none() {
        assert!(parse_pairing_command("/bind").is_none());
        assert!(parse_pairing_command("/claim   ").is_none());
    }

    #[test]
    fn parse_rejects_garbage_code() {
        // Non-alphanumeric payload is not a plausible code.
        assert!(parse_pairing_command("/bind @#$%^&*").is_none());
    }

    #[test]
    fn parse_requires_whitespace_after_verb() {
        // `/claimABCD` is a single token, not `/claim ABCD`.
        assert!(parse_pairing_command("/claimABCD").is_none());
        assert!(parse_pairing_command("/bindABCD").is_none());
    }

    // ── apply_pairing ────────────────────────────────────────

    fn telegram_config() -> TelegramConfig {
        TelegramConfig {
            bot_token: "x".into(),
            allowed_users: vec![],
            stream_mode: StreamMode::Off,
            draft_update_interval_ms: 500,
            interrupt_on_new_message: false,
            mention_only: false,
        }
    }

    fn channels_with_telegram() -> ChannelsConfig {
        ChannelsConfig {
            telegram: Some(telegram_config()),
            ..Default::default()
        }
    }

    /// All fourteen arms, and the field each one expects. Every existing
    /// `apply_pairing` test used `"telegram"`, so thirteen arms had no coverage
    /// at all and a typo in any of them was invisible.
    #[test]
    fn every_channel_arm_resolves_its_own_allowlist() {
        use AllowlistField::{
            AllowedContacts, AllowedFrom, AllowedNumbers, AllowedSenders, AllowedUsers,
        };

        let table: [(&str, AllowlistField); 14] = [
            ("telegram", AllowedUsers),
            ("discord", AllowedUsers),
            ("slack", AllowedUsers),
            ("mattermost", AllowedUsers),
            ("matrix", AllowedUsers),
            ("irc", AllowedUsers),
            ("lark", AllowedUsers),
            ("dingtalk", AllowedUsers),
            ("qq", AllowedUsers),
            ("nextcloud_talk", AllowedUsers),
            ("signal", AllowedFrom),
            ("whatsapp", AllowedNumbers),
            ("linq", AllowedSenders),
            ("imessage", AllowedContacts),
        ];

        for (channel, field) in table {
            assert_eq!(
                expected_field_for(channel),
                Some(field),
                "{channel} must resolve to {field:?}"
            );
        }

        // An unknown channel resolves to nothing, rather than defaulting into
        // somebody else's list.
        assert_eq!(expected_field_for("telgram"), None);
        assert_eq!(expected_field_for(""), None);
    }

    /// The wrong field for a channel used to be accepted silently — the
    /// parameter was discarded unread. It must not write.
    #[test]
    fn a_mismatched_field_writes_nothing() {
        let mut cc = channels_with_telegram();
        apply_pairing(
            &mut cc,
            "telegram",
            // Telegram's list is `allowed_users`; this is Signal's field.
            AllowlistField::AllowedFrom,
            &["rantaiclaw_user".to_string()],
            false,
        );
        assert!(
            cc.telegram
                .as_ref()
                .expect("telegram section")
                .allowed_users
                .is_empty(),
            "a mismatched field must not append to the resolved list"
        );

        // The matching field does write — otherwise this test would pass for a
        // channel that appends nothing at all.
        apply_pairing(
            &mut cc,
            "telegram",
            AllowlistField::AllowedUsers,
            &["rantaiclaw_user".to_string()],
            false,
        );
        assert_eq!(
            cc.telegram.as_ref().expect("telegram").allowed_users,
            vec!["rantaiclaw_user".to_string()],
            "control: the matching field appends"
        );
    }

    #[test]
    fn apply_pairing_appends_users_and_owner() {
        let mut cc = channels_with_telegram();
        apply_pairing(
            &mut cc,
            "telegram",
            AllowlistField::AllowedUsers,
            &["123".to_string(), "alice".to_string()],
            true,
        );
        let users = &cc.telegram.as_ref().unwrap().allowed_users;
        assert!(users.contains(&"123".to_string()));
        assert!(users.contains(&"alice".to_string()));
        assert!(cc.approval_owners.contains(&"123".to_string()));
        assert!(cc.approval_owners.contains(&"alice".to_string()));
    }

    #[test]
    fn apply_pairing_bind_does_not_add_owner() {
        let mut cc = channels_with_telegram();
        apply_pairing(
            &mut cc,
            "telegram",
            AllowlistField::AllowedUsers,
            &["123".to_string()],
            false,
        );
        assert!(cc
            .telegram
            .as_ref()
            .unwrap()
            .allowed_users
            .contains(&"123".to_string()));
        assert!(cc.approval_owners.is_empty());
    }

    #[test]
    fn apply_pairing_dedupes() {
        let mut cc = channels_with_telegram();
        cc.telegram.as_mut().unwrap().allowed_users = vec!["123".to_string()];
        cc.approval_owners = vec!["123".to_string()];
        apply_pairing(
            &mut cc,
            "telegram",
            AllowlistField::AllowedUsers,
            &["123".to_string(), "123".to_string()],
            true,
        );
        assert_eq!(cc.telegram.as_ref().unwrap().allowed_users.len(), 1);
        assert_eq!(cc.approval_owners.len(), 1);
    }

    #[test]
    fn apply_pairing_noop_when_channel_config_absent() {
        let mut cc = ChannelsConfig::default(); // telegram is None
        apply_pairing(
            &mut cc,
            "telegram",
            AllowlistField::AllowedUsers,
            &["123".to_string()],
            true,
        );
        assert!(cc.telegram.is_none());
        // No owner appended either — the section didn't exist.
        assert!(cc.approval_owners.is_empty());
    }

    #[test]
    fn apply_pairing_unknown_channel_is_noop() {
        let mut cc = channels_with_telegram();
        apply_pairing(
            &mut cc,
            "nosuchchannel",
            AllowlistField::AllowedUsers,
            &["123".to_string()],
            true,
        );
        assert!(cc.telegram.as_ref().unwrap().allowed_users.is_empty());
        assert!(cc.approval_owners.is_empty());
    }

    // ── try_handle_pairing (happy path) ──────────────────────

    #[tokio::test]
    async fn try_handle_pairing_non_command_returns_none() {
        // No store, no config needed — a non-command short-circuits.
        let dir = tempfile::tempdir().unwrap();
        let reply = try_handle_pairing(
            "hello",
            "telegram",
            AllowlistField::AllowedUsers,
            &["123".to_string()],
            dir.path(),
        )
        .await;
        assert!(reply.is_none());
    }

    #[tokio::test]
    async fn try_handle_pairing_invalid_code_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let reply = try_handle_pairing(
            "/bind ABCD-EFGH",
            "telegram",
            AllowlistField::AllowedUsers,
            &["123".to_string()],
            dir.path(),
        )
        .await;
        assert!(reply.unwrap().contains("Invalid or expired"));
    }

    #[tokio::test]
    async fn try_handle_pairing_happy_path_owner() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Point Config::load_or_init at our tempdir, then materialize a full
        // default config and inject a telegram section (so we don't have to
        // hand-write every required field).
        std::env::set_var("RANTAICLAW_CONFIG_DIR", root);
        std::env::remove_var("RANTAICLAW_WORKSPACE");
        {
            let mut seed = crate::config::Config::load_or_init().await.unwrap();
            seed.channels_config.telegram = Some(telegram_config());
            seed.save().await.unwrap();
        }

        // Mint an owner-capable code into the same profile root.
        let code = pairing_store::mint(root, "telegram", 900, None, true, now_unix()).unwrap();

        let reply = try_handle_pairing(
            &format!("/claim {code}"),
            "telegram",
            AllowlistField::AllowedUsers,
            &["999".to_string(), "carol".to_string()],
            root,
        )
        .await
        .expect("pairing command should be handled");

        assert!(reply.contains("owner"), "reply was: {reply}");

        // Re-load and confirm persistence.
        let config = crate::config::Config::load_or_init().await.unwrap();
        let users = &config
            .channels_config
            .telegram
            .as_ref()
            .unwrap()
            .allowed_users;
        assert!(users.contains(&"999".to_string()));
        assert!(users.contains(&"carol".to_string()));
        assert!(config
            .channels_config
            .approval_owners
            .contains(&"999".to_string()));

        std::env::remove_var("RANTAICLAW_CONFIG_DIR");
    }
}
