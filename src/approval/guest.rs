//! Per-role capability ceiling for **normal users** (non-owners) on multi-user
//! channels.
//!
//! Owners (senders in `channels_config.approval_owners`) get the full toolset.
//! Everyone else who is allowed to chat is a *guest*, and their turns run under
//! a [`GuestGate`]: a tool the agent calls on a guest's behalf must be
//! permitted, and if it's `shell`, the command must match one of the guest
//! command globs. Anything else is denied outright (a hard ceiling — never
//! escalated to an owner).
//!
//! Built per turn at the channel/gateway entry from config; `None` means "no
//! restriction" (owner, CLI, or console-authenticated user).

use std::collections::HashSet;

/// The capability ceiling applied to a single non-owner ("guest") turn.
#[derive(Debug, Clone)]
pub struct GuestGate {
    /// Tools a guest may use: the always-safe set (config `auto_approve`) unioned
    /// with `channels_config.guest_allowed_tools`.
    permitted_tools: HashSet<String>,
    /// Shell-command glob patterns a guest may run (`channels_config.guest_allowed_commands`).
    allowed_commands: Vec<String>,
}

impl GuestGate {
    /// Build a gate from the safe (auto-approved) tool set plus the configured
    /// guest allowances.
    pub fn new<S>(auto_approve: S, guest_tools: &[String], guest_commands: &[String]) -> Self
    where
        S: IntoIterator<Item = String>,
    {
        let mut permitted_tools: HashSet<String> = auto_approve.into_iter().collect();
        permitted_tools.extend(guest_tools.iter().cloned());
        Self {
            permitted_tools,
            allowed_commands: guest_commands.to_vec(),
        }
    }

    /// Tools that are **owner-only**, no matter what `guest_allowed_tools`
    /// says. These mutate authority itself (who owns the bot / what guests may
    /// do), so allowing a guest to call one — even by an owner's misconfiguration
    /// — would be a privilege-escalation hole. Checked before the allowlist.
    pub const OWNER_ONLY_TOOLS: &'static [&'static str] = &["manage_permissions"];

    /// Whether a guest may invoke `tool` at all. Owner-only tools are always
    /// denied; otherwise the tool must be in the permitted set.
    pub fn tool_permitted(&self, tool: &str) -> bool {
        if Self::OWNER_ONLY_TOOLS.contains(&tool) {
            return false;
        }
        self.permitted_tools.contains(tool)
    }

    /// Whether a guest may run shell `command`. Conservative: the command must
    /// be a single simple command (no chaining/pipe/redirect/subshell — those
    /// could smuggle a non-allowlisted command past the glob) AND match one of
    /// the configured globs.
    pub fn command_permitted(&self, command: &str) -> bool {
        let cmd = command.trim();
        if cmd.is_empty() {
            return false;
        }
        // Reject any shell metacharacter that could chain, redirect, or inject a
        // second command — a guest only runs one plain command.
        const FORBIDDEN: &[&str] = &[
            "`", "$(", "${", "<(", ">(", "&&", "||", ";", "|", ">", "<", "&", "\n", "\r",
        ];
        if FORBIDDEN.iter().any(|m| cmd.contains(m)) {
            return false;
        }
        self.allowed_commands.iter().any(|p| glob_match(p, cmd))
    }

    /// Decision for a single tool call. `arguments` is the parsed call args
    /// (used to extract the shell command). Returns `None` if permitted, or
    /// `Some(reason)` to deny with that message.
    pub fn deny_reason(&self, tool: &str, arguments: &serde_json::Value) -> Option<String> {
        if Self::OWNER_ONLY_TOOLS.contains(&tool) {
            return Some(format!(
                "The `{tool}` tool is owner-only and cannot be used by non-owner users \
                 (it changes who owns the bot). An owner must do this."
            ));
        }
        if !self.tool_permitted(tool) {
            return Some(format!(
                "The `{tool}` tool isn't available to non-owner users on this channel. \
                 Ask an owner to run it, or to add it to the guest allowlist."
            ));
        }
        // Shell is permitted as a tool — now gate the specific command.
        if is_shell_tool(tool) {
            let command = arguments
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            if !self.command_permitted(&command) {
                return Some(format!(
                    "As a non-owner you can only run commands an owner has allowlisted for guests \
                     (and only simple, single commands). `{}` isn't permitted.",
                    command.trim()
                ));
            }
        }
        None
    }
}

/// Tools whose calls carry a shell `command` argument to gate.
fn is_shell_tool(tool: &str) -> bool {
    matches!(tool, "shell" | "bash" | "run_command")
}

/// Anchored glob match supporting `*` (matches any run of characters, incl.
/// empty). Case-sensitive. `"kubectl get *"` matches `"kubectl get pods -n x"`.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn helper(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(&b'*') => helper(&p[1..], t) || (!t.is_empty() && helper(p, &t[1..])),
            Some(&c) => !t.is_empty() && t[0] == c && helper(&p[1..], &t[1..]),
        }
    }
    helper(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gate() -> GuestGate {
        GuestGate::new(
            ["file_read".to_string(), "memory_recall".to_string()],
            &["shell".to_string(), "web_search".to_string()],
            &[
                "kubectl get *".to_string(),
                "kubectl describe *".to_string(),
                "ls".to_string(),
            ],
        )
    }

    #[test]
    fn glob_matches_anchored_with_star() {
        assert!(glob_match("kubectl get *", "kubectl get pods"));
        assert!(glob_match(
            "kubectl get *",
            "kubectl get pods -n kube-system"
        ));
        assert!(glob_match("ls", "ls"));
        assert!(!glob_match("kubectl get *", "kubectl delete pods"));
        assert!(!glob_match("ls", "ls -la")); // anchored — no trailing wildcard
        assert!(!glob_match("kubectl get *", "xkubectl get pods")); // anchored start
    }

    #[test]
    fn safe_and_allowed_tools_permitted_others_denied() {
        let g = gate();
        assert!(g.tool_permitted("file_read")); // auto-approve safe set
        assert!(g.tool_permitted("web_search")); // explicit guest tool
        assert!(g.tool_permitted("shell")); // explicit guest tool
        assert!(!g.tool_permitted("file_write")); // not allowed
        assert!(!g.tool_permitted("ssh"));
    }

    #[test]
    fn shell_command_ceiling() {
        let g = gate();
        assert!(g.command_permitted("kubectl get pods"));
        assert!(g.command_permitted("kubectl describe pod x"));
        assert!(g.command_permitted("ls"));
        // off-list verb
        assert!(!g.command_permitted("kubectl delete pod x"));
        // injection / chaining blocked even though prefix matches
        assert!(!g.command_permitted("kubectl get pods; rm -rf /"));
        assert!(!g.command_permitted("kubectl get pods && rm -rf /"));
        assert!(!g.command_permitted("kubectl get pods | tee /etc/x"));
        assert!(!g.command_permitted("kubectl get pods > /etc/x"));
        assert!(!g.command_permitted("kubectl get $(whoami)"));
        assert!(!g.command_permitted(""));
    }

    #[test]
    fn owner_only_tools_never_permitted_for_guests() {
        // Even if an owner mistakenly adds `manage_permissions` to the guest
        // allowlist, the hard owner-only denylist still blocks it.
        let g = GuestGate::new(
            ["file_read".to_string()],
            &["manage_permissions".to_string()],
            &[],
        );
        assert!(!g.tool_permitted("manage_permissions"));
        let reason = g.deny_reason("manage_permissions", &json!({})).unwrap();
        assert!(reason.contains("owner-only"));
    }

    #[test]
    fn deny_reason_paths() {
        let g = gate();
        // disallowed tool
        assert!(g.deny_reason("file_write", &json!({})).is_some());
        // allowed safe tool
        assert!(g.deny_reason("file_read", &json!({})).is_none());
        // shell allowed + command allowed
        assert!(g
            .deny_reason("shell", &json!({"command": "kubectl get pods"}))
            .is_none());
        // shell allowed + command denied
        assert!(g
            .deny_reason("shell", &json!({"command": "rm -rf /"}))
            .is_some());
    }
}
