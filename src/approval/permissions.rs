//! Surface-agnostic editing + rendering of the per-role permission model.
//!
//! The model spans two config locations:
//!   * **owners** (`channels_config.approval_owners`) — senders who get the full
//!     toolset and may approve tool calls. Matched by
//!     [`crate::approval::can_approve`].
//!   * **guest capability ceiling** (`channels_config.guest_allowed_tools` /
//!     `channels_config.guest_allowed_commands`) — what everyone else (allowed
//!     to chat but not an owner) may have the agent do on their behalf. Enforced
//!     by [`crate::approval::GuestGate`].
//!   * **owner command allowlist** (`autonomy.allowed_commands`) — shell-command
//!     BASENAMES the shell tool may run for an owner without prompting for
//!     approval. Lives on the autonomy config, not `ChannelsConfig`.
//!
//! This module is the single source of truth for *mutating* and *displaying*
//! that state, so the three setup surfaces behave identically:
//!   * the CLI (`rantaiclaw permissions ...`),
//!   * the TUI slash command (`/permissions`),
//!   * the owner-gated chat self-setup tool (`manage_permissions`).
//!
//! Mutations are pure functions over a `&mut Config` — the caller owns
//! persistence (`Config::save`) and any surface-specific messaging.

use crate::config::Config;

/// Which list a mutation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `channels_config.approval_owners` — full-privilege senders.
    Owner,
    /// `channels_config.guest_allowed_tools` — tools a non-owner may use.
    GuestTool,
    /// `channels_config.guest_allowed_commands` — shell-command globs a
    /// non-owner may run.
    GuestCommand,
    /// `autonomy.allowed_commands` — shell-command BASENAMES the shell tool may
    /// run for an owner without prompting. NOTE: basenames (e.g. `kubectl`),
    /// not globs — the glob form (`kubectl *`) belongs to the GUEST command list.
    AllowCommand,
}

impl Target {
    /// Human label used in messages.
    pub fn label(self) -> &'static str {
        match self {
            Target::Owner => "owner",
            Target::GuestTool => "guest tool",
            Target::GuestCommand => "guest command",
            Target::AllowCommand => "owner command (autonomy allowlist)",
        }
    }

    /// Parse from a surface token (CLI arg / chat tool field). Case-insensitive;
    /// accepts a couple of friendly aliases.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "owner" | "owners" => Some(Target::Owner),
            "tool" | "tools" | "guest_tool" | "guest-tool" | "guesttool" => Some(Target::GuestTool),
            "command" | "commands" | "cmd" | "guest_command" | "guest-command" | "guestcommand" => {
                Some(Target::GuestCommand)
            }
            "allow-command" | "allow_command" | "allowcommand" | "allow-cmd" | "owner-command"
            | "owner_command" => Some(Target::AllowCommand),
            _ => None,
        }
    }
}

/// Add or remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add,
    Remove,
}

impl Op {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "add" | "allow" | "grant" => Some(Op::Add),
            "remove" | "rm" | "del" | "delete" | "deny" | "revoke" => Some(Op::Remove),
            _ => None,
        }
    }
}

/// Result of applying a single mutation.
#[derive(Debug, Clone)]
pub struct ChangeOutcome {
    /// Whether the config actually changed (false ⇒ nothing to persist).
    pub changed: bool,
    /// One-line, human-readable summary suitable for CLI/chat/TUI output.
    pub message: String,
}

/// Normalize a value before storing it.
///
/// Owners and tool names are stored verbatim except for trimming (a sender
/// identity / tool name has no surrounding whitespace). Command globs (guest)
/// and command basenames (owner allowlist) likewise trim outer whitespace but
/// otherwise keep their exact shape so the matchers see what the owner typed.
fn normalize(target: Target, value: &str) -> String {
    let _ = target; // same rule for every list today; kept for future divergence
    value.trim().to_string()
}

/// Apply one add/remove against `config` in place. Idempotent: adding an
/// existing entry or removing an absent one is reported as `changed: false`.
///
/// Routes each target to its backing list across both config locations:
///   * [`Target::Owner`] / [`Target::GuestTool`] / [`Target::GuestCommand`] →
///     `config.channels_config`,
///   * [`Target::AllowCommand`] → `config.autonomy.allowed_commands`.
pub fn apply(config: &mut Config, target: Target, op: Op, value: &str) -> ChangeOutcome {
    let value = normalize(target, value);
    if value.is_empty() {
        return ChangeOutcome {
            changed: false,
            message: format!("Refused: empty {} value.", target.label()),
        };
    }

    let list = match target {
        Target::Owner => &mut config.channels_config.approval_owners,
        Target::GuestTool => &mut config.channels_config.guest_allowed_tools,
        Target::GuestCommand => &mut config.channels_config.guest_allowed_commands,
        Target::AllowCommand => &mut config.autonomy.allowed_commands,
    };

    match op {
        Op::Add => {
            if list.iter().any(|e| e == &value) {
                ChangeOutcome {
                    changed: false,
                    message: format!("{} `{}` is already set.", target.label(), value),
                }
            } else {
                list.push(value.clone());
                ChangeOutcome {
                    changed: true,
                    message: format!("Added {} `{}`.", target.label(), value),
                }
            }
        }
        Op::Remove => {
            let before = list.len();
            list.retain(|e| e != &value);
            if list.len() == before {
                ChangeOutcome {
                    changed: false,
                    message: format!("{} `{}` was not set.", target.label(), value),
                }
            } else {
                ChangeOutcome {
                    changed: true,
                    message: format!("Removed {} `{}`.", target.label(), value),
                }
            }
        }
    }
}

/// Render the current per-role permission state as a multi-line summary.
///
/// `safe_tools` is the always-available read-only set (the autonomy
/// `auto_approve` list) so the reader sees the *effective* guest tool ceiling,
/// not just the additive allowlist.
pub fn render(config: &Config, safe_tools: &[String]) -> String {
    use std::fmt::Write as _;

    let cc = &config.channels_config;
    let mut out = String::new();
    out.push_str("Per-role permissions\n");
    out.push_str("────────────────────\n");

    // Owners.
    out.push_str("Owners (full toolset, may approve):\n");
    if cc.approval_owners.is_empty() {
        out.push_str("  (none — only the CLI/console operator is an owner)\n");
    } else if cc.approval_owners.iter().any(|o| o == "*") {
        out.push_str("  * (ANY sender is an owner — insecure, review this)\n");
    } else {
        for o in &cc.approval_owners {
            let _ = writeln!(out, "  • {o}");
        }
    }

    // Owner command allowlist (autonomy.allowed_commands) — basenames, not globs.
    out.push_str("\nOwner shell commands (run without approval) [basenames, not globs]:\n");
    if config.autonomy.allowed_commands.is_empty() {
        out.push_str("  (none — every shell command prompts for approval)\n");
    } else {
        for c in &config.autonomy.allowed_commands {
            let _ = writeln!(out, "  • {c}");
        }
    }

    // Guest tool ceiling.
    out.push_str("\nNon-owner (guest) tools:\n");
    out.push_str("  always: skills + read-only [");
    out.push_str(&safe_tools.join(", "));
    out.push_str("]\n");
    if cc.guest_allowed_tools.is_empty() {
        out.push_str("  extra : (none)\n");
    } else {
        let _ = writeln!(out, "  extra : {}", cc.guest_allowed_tools.join(", "));
    }

    // Guest command ceiling.
    out.push_str("\nNon-owner (guest) shell commands (globs):\n");
    if cc.guest_allowed_commands.is_empty() {
        out.push_str("  (none — guests may run no shell commands)\n");
    } else {
        for c in &cc.guest_allowed_commands {
            let _ = writeln!(out, "  • {c}");
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with empty lists so each test starts from a known-clean slate
    /// (the default autonomy allowlist ships with `git`/`cargo`/etc.).
    fn cfg() -> Config {
        let mut c = Config::default();
        c.channels_config.approval_owners.clear();
        c.channels_config.guest_allowed_tools.clear();
        c.channels_config.guest_allowed_commands.clear();
        c.autonomy.allowed_commands.clear();
        c
    }

    #[test]
    fn add_remove_owner_is_idempotent() {
        let mut c = cfg();
        let r = apply(&mut c, Target::Owner, Op::Add, " 123456 ");
        assert!(r.changed);
        assert_eq!(
            c.channels_config.approval_owners,
            vec!["123456".to_string()]
        ); // trimmed
           // duplicate add → no change
        assert!(!apply(&mut c, Target::Owner, Op::Add, "123456").changed);
        // remove
        assert!(apply(&mut c, Target::Owner, Op::Remove, "123456").changed);
        assert!(c.channels_config.approval_owners.is_empty());
        // remove again → no change
        assert!(!apply(&mut c, Target::Owner, Op::Remove, "123456").changed);
    }

    #[test]
    fn guest_tool_and_command_targets() {
        let mut c = cfg();
        assert!(apply(&mut c, Target::GuestTool, Op::Add, "shell").changed);
        assert!(apply(&mut c, Target::GuestCommand, Op::Add, "kubectl get *").changed);
        assert_eq!(
            c.channels_config.guest_allowed_tools,
            vec!["shell".to_string()]
        );
        assert_eq!(
            c.channels_config.guest_allowed_commands,
            vec!["kubectl get *".to_string()]
        );
    }

    #[test]
    fn add_remove_allow_command_is_idempotent() {
        let mut c = cfg();
        // Owner allowlist takes a BASENAME, not a glob.
        let r = apply(&mut c, Target::AllowCommand, Op::Add, " kubectl ");
        assert!(r.changed);
        assert_eq!(c.autonomy.allowed_commands, vec!["kubectl".to_string()]); // trimmed
        assert!(r.message.contains("owner command"));
        // duplicate add → no change
        assert!(!apply(&mut c, Target::AllowCommand, Op::Add, "kubectl").changed);
        // remove
        assert!(apply(&mut c, Target::AllowCommand, Op::Remove, "kubectl").changed);
        assert!(c.autonomy.allowed_commands.is_empty());
        // remove again → no change
        assert!(!apply(&mut c, Target::AllowCommand, Op::Remove, "kubectl").changed);
    }

    #[test]
    fn allow_command_does_not_touch_guest_list() {
        let mut c = cfg();
        apply(&mut c, Target::AllowCommand, Op::Add, "ls");
        assert_eq!(c.autonomy.allowed_commands, vec!["ls".to_string()]);
        // The guest (glob) list is untouched — these are distinct surfaces.
        assert!(c.channels_config.guest_allowed_commands.is_empty());
    }

    #[test]
    fn empty_value_refused() {
        let mut c = cfg();
        assert!(!apply(&mut c, Target::GuestTool, Op::Add, "   ").changed);
        assert!(!apply(&mut c, Target::AllowCommand, Op::Add, "   ").changed);
    }

    #[test]
    fn target_and_op_parse_aliases() {
        assert_eq!(Target::parse("owners"), Some(Target::Owner));
        assert_eq!(Target::parse("TOOL"), Some(Target::GuestTool));
        assert_eq!(Target::parse("cmd"), Some(Target::GuestCommand));
        assert_eq!(Target::parse("allow-command"), Some(Target::AllowCommand));
        assert_eq!(Target::parse("allow_command"), Some(Target::AllowCommand));
        assert_eq!(Target::parse("allowcommand"), Some(Target::AllowCommand));
        assert_eq!(Target::parse("allow-cmd"), Some(Target::AllowCommand));
        assert_eq!(Target::parse("owner-command"), Some(Target::AllowCommand));
        assert_eq!(Target::parse("OWNER_COMMAND"), Some(Target::AllowCommand));
        assert_eq!(Target::parse("nope"), None);
        assert_eq!(Op::parse("allow"), Some(Op::Add));
        assert_eq!(Op::parse("revoke"), Some(Op::Remove));
    }

    #[test]
    fn render_shows_all_sections() {
        let mut c = cfg();
        apply(&mut c, Target::Owner, Op::Add, "alice");
        apply(&mut c, Target::AllowCommand, Op::Add, "kubectl");
        apply(&mut c, Target::GuestTool, Op::Add, "web_search");
        apply(&mut c, Target::GuestCommand, Op::Add, "ls *");
        let s = render(&c, &["file_read".to_string()]);
        assert!(s.contains("alice"));
        assert!(s.contains("kubectl"));
        assert!(s.contains("Owner shell commands")); // autonomy allowlist section
        assert!(s.contains("web_search"));
        assert!(s.contains("ls *"));
        assert!(s.contains("file_read")); // safe set surfaced
    }
}
