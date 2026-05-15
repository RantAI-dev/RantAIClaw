//! Slash commands for managing the Supervised-mode runtime allowlist.
//!
//! - `/allow <basename> [--persist]` — add a basename to the live
//!   allowlist (session only by default; with `--persist`, also write
//!   to `<policy_dir>/runtime_allowlist.toml`). Resolves a matching
//!   pending approval request if one is queued.
//! - `/deny <basename>` — explicitly deny a pending approval request
//!   for this basename. Does not mutate the allowlist.
//! - `/allowlist` — show the current allowlist (boot + runtime) and
//!   any pending approval requests.
//!
//! All three rely on `TuiContext.security` being `Some`, which is the
//! case when the TUI was launched against a real `Agent::from_config`.
//! Test contexts have `security == None` and surface a friendly
//! "security policy not available" message.

use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::security::Decision;
use crate::tui::context::TuiContext;

pub struct AllowCommand;

impl CommandHandler for AllowCommand {
    fn name(&self) -> &str {
        "allow"
    }

    fn description(&self) -> &str {
        "Add a shell command to the runtime allowlist (Supervised mode)"
    }

    fn usage(&self) -> &str {
        "/allow <basename> [--persist]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let Some(security) = ctx.security.as_ref() else {
            return Ok(CommandResult::Message(
                "Security policy not available in this context.".into(),
            ));
        };

        let mut basename: Option<&str> = None;
        let mut persist = false;
        for tok in args.split_whitespace() {
            match tok {
                "--persist" | "--save" | "-p" => persist = true,
                _ if basename.is_none() => basename = Some(tok),
                _ => {
                    return Ok(CommandResult::Message(format!(
                        "Unexpected extra argument: `{tok}`. Usage: /allow <basename> [--persist]"
                    )));
                }
            }
        }

        let Some(basename) = basename else {
            return Ok(CommandResult::Message(
                "Usage: /allow <basename> [--persist] — e.g. `/allow brew --persist`".into(),
            ));
        };

        if let Err(e) = security.add_runtime_command(basename, persist) {
            return Ok(CommandResult::Message(format!(
                "Failed to allow `{basename}`: {e}"
            )));
        }

        // If there's a pending approval request for this basename,
        // resolve it so the suspended shell call can resume.
        let mut resolved = false;
        if let Some(pending) = security.pending() {
            let decision = if persist {
                Decision::Persist
            } else {
                Decision::Session
            };
            if pending.resolve_by_basename(basename, decision).is_some() {
                resolved = true;
            }
        }

        let scope = if persist { "persistent" } else { "session" };
        let suffix = if resolved {
            " — pending approval resolved, the agent's tool call will resume"
        } else {
            ""
        };
        Ok(CommandResult::Message(format!(
            "Added `{basename}` to the {scope} allowlist{suffix}."
        )))
    }
}

pub struct DenyCommand;

impl CommandHandler for DenyCommand {
    fn name(&self) -> &str {
        "deny"
    }

    fn description(&self) -> &str {
        "Reject a pending shell-command approval request"
    }

    fn usage(&self) -> &str {
        "/deny <basename>"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let Some(security) = ctx.security.as_ref() else {
            return Ok(CommandResult::Message(
                "Security policy not available in this context.".into(),
            ));
        };

        let basename = args.split_whitespace().next();
        let Some(basename) = basename else {
            return Ok(CommandResult::Message(
                "Usage: /deny <basename> — rejects a pending approval prompt".into(),
            ));
        };

        let Some(pending) = security.pending() else {
            return Ok(CommandResult::Message(
                "No async-approval registry is active on this policy.".into(),
            ));
        };

        match pending.resolve_by_basename(basename, Decision::Deny) {
            Some(_) => Ok(CommandResult::Message(format!(
                "Denied pending approval for `{basename}`. The agent's tool call will fail."
            ))),
            None => Ok(CommandResult::Message(format!(
                "No pending approval for `{basename}` (or more than one queued — wait for resolution)."
            ))),
        }
    }
}

pub struct AllowlistCommand;

impl CommandHandler for AllowlistCommand {
    fn name(&self) -> &str {
        "allowlist"
    }

    fn description(&self) -> &str {
        "Show the current shell-command allowlist and pending approval requests"
    }

    fn usage(&self) -> &str {
        "/allowlist"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let Some(security) = ctx.security.as_ref() else {
            return Ok(CommandResult::Message(
                "Security policy not available in this context.".into(),
            ));
        };

        let boot = &security.allowed_commands;
        let runtime = security.runtime_allowlist_snapshot();
        let pending: Vec<crate::security::PendingRequest> =
            security.pending().map(|p| p.list()).unwrap_or_default();

        let mut out = String::new();
        out.push_str(&format!(
            "Boot allowlist ({}): {}\n",
            boot.len(),
            if boot.is_empty() {
                "(none)".to_string()
            } else {
                boot.join(", ")
            }
        ));
        out.push_str(&format!(
            "Runtime allowlist ({}): {}\n",
            runtime.len(),
            if runtime.is_empty() {
                "(none)".to_string()
            } else {
                runtime.join(", ")
            }
        ));
        if pending.is_empty() {
            out.push_str("Pending approvals: (none)");
        } else {
            out.push_str("Pending approvals:\n");
            for req in pending {
                out.push_str(&format!(
                    "  - {} (full: `{}`) — /allow {} | /allow {} --persist | /deny {}\n",
                    req.basename, req.full_command, req.basename, req.basename, req.basename,
                ));
            }
        }
        Ok(CommandResult::Message(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityPolicy;
    use std::sync::Arc;

    fn test_context() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    fn ctx_with_security() -> TuiContext {
        let mut ctx = test_context();
        ctx.security = Some(Arc::new(SecurityPolicy::default()));
        ctx
    }

    #[test]
    fn allow_without_security_returns_message() {
        let mut ctx = test_context();
        let cmd = AllowCommand;
        let result = cmd.execute("brew", &mut ctx).unwrap();
        match result {
            CommandResult::Message(m) => assert!(m.contains("not available")),
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn allow_session_adds_to_runtime() {
        let mut ctx = ctx_with_security();
        let cmd = AllowCommand;
        cmd.execute("brew", &mut ctx).unwrap();
        let snap = ctx.security.as_ref().unwrap().runtime_allowlist_snapshot();
        assert!(snap.contains(&"brew".to_string()));
    }

    #[test]
    fn allow_requires_basename() {
        let mut ctx = ctx_with_security();
        let cmd = AllowCommand;
        match cmd.execute("", &mut ctx).unwrap() {
            CommandResult::Message(m) => assert!(m.contains("Usage:")),
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn allow_rejects_multi_token_argument() {
        let mut ctx = ctx_with_security();
        let cmd = AllowCommand;
        let result = cmd.execute("brew install", &mut ctx).unwrap();
        match result {
            CommandResult::Message(m) => {
                assert!(m.contains("Unexpected") || m.contains("Failed"));
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn allowlist_shows_boot_and_runtime() {
        let mut ctx = ctx_with_security();
        ctx.security
            .as_ref()
            .unwrap()
            .add_runtime_command("rg", false)
            .unwrap();

        let result = AllowlistCommand.execute("", &mut ctx).unwrap();
        match result {
            CommandResult::Message(m) => {
                assert!(m.contains("Boot allowlist"));
                assert!(m.contains("Runtime allowlist"));
                assert!(m.contains("rg"));
                assert!(m.contains("Pending approvals"));
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn deny_without_pending_request_is_friendly() {
        let mut ctx = ctx_with_security();
        // Set up a pending registry so the deny path reaches `resolve_by_basename`.
        let pending = Arc::new(crate::security::PendingApprovals::default());
        ctx.security.as_ref().unwrap().set_pending(pending);

        let result = DenyCommand.execute("brew", &mut ctx).unwrap();
        match result {
            CommandResult::Message(m) => assert!(m.contains("No pending approval")),
            _ => panic!("expected Message"),
        }
    }
}
