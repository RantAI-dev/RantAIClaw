//! Interactive approval workflow for supervised mode.
//!
//! Provides a pre-execution hook that prompts the user before tool calls,
//! with session-scoped "Always" allowlists and audit logging.

pub mod guest;
pub mod permissions;
pub mod policy_writer;

pub use guest::GuestGate;

use crate::config::AutonomyConfig;
use crate::security::AutonomyLevel;
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::Arc;

// ── Types ────────────────────────────────────────────────────────

/// A request to approve a tool call before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// The user's response to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalResponse {
    /// Execute this one call.
    Yes,
    /// Deny this call.
    No,
    /// Execute and add tool to session-scoped allowlist.
    Always,
}

/// A single audit log entry for an approval decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalLogEntry {
    pub timestamp: String,
    pub tool_name: String,
    pub arguments_summary: String,
    pub decision: ApprovalResponse,
    pub channel: String,
}

// ── ApprovalManager ──────────────────────────────────────────────

/// Manages the interactive approval workflow.
///
/// - Checks config-level `auto_approve` / `always_ask` lists
/// - Maintains a session-scoped "always" allowlist
/// - Records an audit trail of all decisions
pub struct ApprovalManager {
    /// Tools that never need approval (from config).
    auto_approve: HashSet<String>,
    /// Tools that always need approval, ignoring session allowlist.
    always_ask: HashSet<String>,
    /// Autonomy level from config.
    autonomy_level: AutonomyLevel,
    /// Session-scoped allowlist built from "Always" responses.
    session_allowlist: Mutex<HashSet<String>>,
    /// Audit trail of approval decisions.
    audit_log: Mutex<Vec<ApprovalLogEntry>>,
    /// Live autonomy source. When set, `needs_approval` reads through this
    /// instead of the boot-time `autonomy_level`, so a config hot-reload that
    /// tightens autonomy also tightens the approval gate. `None` keeps the
    /// snapshot behaviour for callers that rebuild per turn anyway.
    policy: Option<Arc<crate::security::SecurityPolicy>>,
}

impl ApprovalManager {
    /// Create from autonomy config.
    pub fn from_config(config: &AutonomyConfig) -> Self {
        Self {
            auto_approve: config.auto_approve.iter().cloned().collect(),
            always_ask: config.always_ask.iter().cloned().collect(),
            autonomy_level: config.level,
            session_allowlist: Mutex::new(HashSet::new()),
            audit_log: Mutex::new(Vec::new()),
            policy: None,
        }
    }

    /// Attach a live policy so the autonomy level is read at decision time.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<crate::security::SecurityPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Autonomy level to gate on: the live policy when one is attached,
    /// otherwise the level captured at construction.
    fn effective_autonomy(&self) -> AutonomyLevel {
        match &self.policy {
            Some(p) => p.effective_autonomy(),
            None => self.autonomy_level,
        }
    }

    /// Does `tool` always prompt? Reads the live policy when one is attached.
    ///
    /// Manual and Smart are both `Supervised` and differ *only* by this list,
    /// so a cached copy made switching between them a no-op on any surface
    /// that builds its manager once — which the channels path does.
    fn forces_prompt(&self, tool_name: &str) -> bool {
        match &self.policy {
            Some(p) => p.fields().always_ask.iter().any(|t| t == tool_name),
            None => self.always_ask.contains(tool_name),
        }
    }

    /// Does `tool` skip the prompt outright? Live counterpart of
    /// [`Self::forces_prompt`].
    fn pre_approved(&self, tool_name: &str) -> bool {
        match &self.policy {
            Some(p) => p.fields().auto_approve.iter().any(|t| t == tool_name),
            None => self.auto_approve.contains(tool_name),
        }
    }

    /// Check whether a tool call requires interactive approval.
    ///
    /// Returns `true` if the call needs a prompt, `false` if it can proceed.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        let autonomy = self.effective_autonomy();

        // Full autonomy never prompts.
        if autonomy == AutonomyLevel::Full {
            return false;
        }

        // ReadOnly blocks everything — handled elsewhere; no prompt needed.
        if autonomy == AutonomyLevel::ReadOnly {
            return false;
        }

        // always_ask overrides everything.
        if self.forces_prompt(tool_name) {
            return true;
        }

        // auto_approve skips the prompt.
        if self.pre_approved(tool_name) {
            return false;
        }

        // Session allowlist (from prior "Always" responses).
        let allowlist = self.session_allowlist.lock();
        if allowlist.contains(tool_name) {
            return false;
        }

        // Default: supervised mode requires approval.
        true
    }

    /// Record an approval decision and update session state.
    pub fn record_decision(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        decision: ApprovalResponse,
        channel: &str,
    ) {
        // If "Always", add to session allowlist.
        if decision == ApprovalResponse::Always {
            let mut allowlist = self.session_allowlist.lock();
            allowlist.insert(tool_name.to_string());
        }

        // Append to audit log.
        let summary = summarize_args(args);
        let entry = ApprovalLogEntry {
            timestamp: Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            arguments_summary: summary,
            decision,
            channel: channel.to_string(),
        };
        let mut log = self.audit_log.lock();
        log.push(entry);
    }

    /// Get a snapshot of the audit log.
    pub fn audit_log(&self) -> Vec<ApprovalLogEntry> {
        self.audit_log.lock().clone()
    }

    /// Get the current session allowlist.
    pub fn session_allowlist(&self) -> HashSet<String> {
        self.session_allowlist.lock().clone()
    }

    /// Pre-seed the session allowlist with tools already granted "Always"
    /// earlier in the same conversation. The web console rebuilds a fresh
    /// `ApprovalManager` per SSE turn, so seeding from a per-session store lets
    /// an "Always" grant persist across messages (parity with the TUI's
    /// session-scoped grant) instead of resetting every turn.
    pub fn seed_session_allowlist(&self, tools: impl IntoIterator<Item = String>) {
        self.session_allowlist.lock().extend(tools);
    }

    /// Drop all prior "Always" session grants. Call this when the live policy is
    /// tightened/reloaded so a blanket grant made under a looser preset does not
    /// survive the change (a tightening must actually re-prompt).
    pub fn clear_session_allowlist(&self) {
        self.session_allowlist.lock().clear();
    }

    /// Prompt the user on the CLI and return their decision.
    ///
    /// For non-CLI channels, returns `Yes` automatically (interactive
    /// approval is only supported on CLI for now).
    pub fn prompt_cli(&self, request: &ApprovalRequest) -> ApprovalResponse {
        prompt_cli_interactive(request)
    }
}

// ── Owner authority gate ─────────────────────────────────────────

/// Whether `sender` is authorized to **approve** a tool call on a channel.
///
/// The owner list (`[channels_config] approval_owners`) is a separate,
/// deliberately smaller allowlist than each channel's `allowed_users` (who may
/// chat): the person who *requested* an action is not automatically allowed to
/// *approve* it. This is the command/owner gate, kept distinct from the sender
/// gate so unifying capability across surfaces is an upgrade, not a security
/// regression. Shared by both in-chat approval paths — the gateway turn-based
/// flow (`crate::gateway::channel_approval`) and the polling-channel shell
/// allowlist relay (`crate::channels::approval_relay`).
///
/// - Empty list ⇒ `false` for everyone (secure default: nobody can approve, so
///   approval-required tools stay auto-denied on channels).
/// - `"*"` ⇒ `true` for any sender (insecure; opt-in only).
/// - Otherwise ⇒ sender match, normalized exactly like the per-channel
///   `allowed_users` gate: a leading `@` is stripped on both sides (so a
///   hand-written `approval_owners = ["@rantaiclaw_owner"]` authorizes sender
///   `rantaiclaw_owner`), but matching is otherwise exact and **case-sensitive** —
///   identical to `allowed_users`, so the two gates never disagree.
pub fn can_approve(owners: &[String], sender: &str) -> bool {
    can_approve_any(owners, std::iter::once(sender))
}

/// Like [`can_approve`], but authorizes if **any** of the sender's identity
/// forms matches an owner.
///
/// A channel can resolve one sender to more than one identity (e.g. a Telegram
/// numeric id AND a username). The per-channel chat allowlist already checks
/// every form, so the owner gate must too — otherwise an owner added by one
/// form (say a numeric id, as the CLI examples suggest) is silently treated as
/// a guest whenever the runtime resolves that same sender to the other form
/// (the username). Semantics otherwise match [`can_approve`]: `"*"` authorizes
/// anyone, an empty owner list denies everyone, and matching is case-sensitive
/// with a leading `@` stripped on both sides.
pub fn can_approve_any<'a>(
    owners: &[String],
    identities: impl IntoIterator<Item = &'a str>,
) -> bool {
    fn normalize(s: &str) -> &str {
        s.trim().trim_start_matches('@')
    }
    if owners.iter().any(|o| o == "*") {
        return true;
    }
    let normalized_owners: Vec<&str> = owners.iter().map(|o| normalize(o.as_str())).collect();
    identities
        .into_iter()
        .any(|id| normalized_owners.contains(&normalize(id)))
}

// ── Approval backends (surface-pluggable decision) ───────────────

/// How an approval decision is obtained for a given surface.
///
/// Extracted so the agent loop no longer hardcodes `channel_name == "cli"`:
/// the loop asks a backend for the decision, and each surface supplies the
/// one that fits it (interactive terminal, in-chat owner relay, web modal, or
/// auto-deny). `decide` is async because the in-chat relay must post a message
/// and await an owner's reply (a separate inbound message); the terminal and
/// auto-deny backends resolve synchronously and just return.
#[async_trait::async_trait]
pub trait ApprovalBackend: Send + Sync {
    async fn decide(&self, mgr: &ApprovalManager, request: &ApprovalRequest) -> ApprovalResponse;
}

/// Interactive terminal prompt (TUI / CLI surface).
pub struct CliApprovalBackend;

#[async_trait::async_trait]
impl ApprovalBackend for CliApprovalBackend {
    async fn decide(&self, mgr: &ApprovalManager, request: &ApprovalRequest) -> ApprovalResponse {
        mgr.prompt_cli(request)
    }
}

/// Non-interactive surface with no approver present in-band: auto-deny.
/// The surface may still offer its own out-of-band, owner-gated approval
/// relay; this only governs the inline decision the loop makes.
pub struct AutoDenyBackend;

#[async_trait::async_trait]
impl ApprovalBackend for AutoDenyBackend {
    async fn decide(&self, _mgr: &ApprovalManager, _request: &ApprovalRequest) -> ApprovalResponse {
        ApprovalResponse::No
    }
}

/// Pick the inline approval backend for a surface by its channel name.
///
/// Behavior-preserving: only the `"cli"` surface prompts interactively;
/// every other surface auto-denies inline (and relies on its own owner-gated
/// relay). This replaces the former hardcoded `if channel_name == "cli"`
/// branch in the agent loop with a single, surface-agnostic seam.
pub fn default_backend_for(channel_name: &str) -> Box<dyn ApprovalBackend> {
    if channel_name == "cli" {
        Box::new(CliApprovalBackend)
    } else {
        Box::new(AutoDenyBackend)
    }
}

// ── CLI prompt ───────────────────────────────────────────────────

/// Display the approval prompt and read user input from stdin.
fn prompt_cli_interactive(request: &ApprovalRequest) -> ApprovalResponse {
    // No interactive approver present (systemd stdin=null, piped input, or a
    // scheduled/headless run that reached this path by mistake): never block on
    // a read that cannot be answered — auto-deny, matching AutoDenyBackend.
    if !io::stdin().is_terminal() {
        return ApprovalResponse::No;
    }
    let summary = summarize_args(&request.arguments);
    eprintln!();
    eprintln!("🔧 Agent wants to execute: {}", request.tool_name);
    eprintln!("   {summary}");
    eprint!("   [Y]es / [N]o / [A]lways for {}: ", request.tool_name);
    let _ = io::stderr().flush();

    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return ApprovalResponse::No;
    }

    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalResponse::Yes,
        "a" | "always" => ApprovalResponse::Always,
        _ => ApprovalResponse::No,
    }
}

/// Produce a short human-readable summary of tool arguments.
pub(crate) fn summarize_args(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => truncate_for_summary(s, 80),
                        other => {
                            let s = other.to_string();
                            truncate_for_summary(&s, 80)
                        }
                    };
                    format!("{k}: {val}")
                })
                .collect();
            parts.join(", ")
        }
        other => {
            let s = other.to_string();
            truncate_for_summary(&s, 120)
        }
    }
}

fn truncate_for_summary(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        input.to_string()
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `approval_owners = ["*"]` makes every sender on every channel an owner.
    /// It is honoured — removing it silently would lock out anyone relying on it
    /// — but nothing warned, and no doc said the value was even accepted.
    #[test]
    fn wildcard_owner_authorizes_anyone() {
        assert!(can_approve_any(
            &["*".to_string()],
            ["some_stranger"].into_iter()
        ));
        assert!(can_approve_any(&["*".to_string()], ["anyone_at_all"]));
    }

    /// The console used the bare identity `user`, compared with no channel
    /// qualification. An operator who added it also made any remote sender who
    /// picked that name an owner.
    #[test]
    fn qualified_console_owner_does_not_match_a_remote_sender() {
        let owners = vec![crate::channels::cli::CLI_SENDER_ID.to_string()];
        assert!(
            can_approve_any(&owners, [crate::channels::cli::CLI_SENDER_ID]),
            "the console itself is still an owner"
        );
        assert!(
            !can_approve_any(&owners, ["user"]),
            "a remote sender named `user` must not inherit the console's authority"
        );
    }

    /// An empty owner list authorises nobody — including the console. The
    /// permissions output used to claim the opposite.
    #[test]
    fn empty_owner_list_authorizes_nobody() {
        assert!(!can_approve_any(&[], ["user"]));
        assert!(!can_approve_any(&[], [crate::channels::cli::CLI_SENDER_ID]));
    }
    use crate::config::AutonomyConfig;

    /// Manual and Smart are both `Supervised`; the *only* thing separating
    /// them is `always_ask`. With that list cached at construction, switching
    /// between the two changed nothing on the channels path — a tightening
    /// that silently did not apply.
    #[test]
    fn needs_approval_follows_a_live_switch_between_smart_and_manual() {
        // Smart: nothing forced to prompt, reads pre-approved.
        let smart = crate::config::AutonomyConfig {
            level: AutonomyLevel::Supervised,
            always_ask: vec![],
            auto_approve: vec!["file_read".to_string()],
            ..crate::config::AutonomyConfig::default()
        };
        let policy = Arc::new(crate::security::SecurityPolicy::default());
        policy.apply_config(&smart);
        let mgr = ApprovalManager::from_config(&smart).with_policy(Arc::clone(&policy));

        assert!(!mgr.needs_approval("file_read"), "Smart pre-approves reads");

        // Manual: the same level, but every built-in is forced to prompt.
        policy.apply_config(&crate::config::AutonomyConfig {
            level: AutonomyLevel::Supervised,
            always_ask: vec!["file_read".to_string()],
            auto_approve: vec!["file_read".to_string()],
            ..crate::config::AutonomyConfig::default()
        });
        assert!(
            mgr.needs_approval("file_read"),
            "switching Smart -> Manual must start prompting even though the \
             autonomy level did not change"
        );
    }

    /// The mirror: dropping a tool from `auto_approve` must start prompting.
    #[test]
    fn needs_approval_follows_a_live_auto_approve_change() {
        let cfg = crate::config::AutonomyConfig {
            level: AutonomyLevel::Supervised,
            always_ask: vec![],
            auto_approve: vec!["memory_recall".to_string()],
            ..crate::config::AutonomyConfig::default()
        };
        let policy = Arc::new(crate::security::SecurityPolicy::default());
        policy.apply_config(&cfg);
        let mgr = ApprovalManager::from_config(&cfg).with_policy(Arc::clone(&policy));

        assert!(!mgr.needs_approval("memory_recall"));

        policy.apply_config(&crate::config::AutonomyConfig {
            auto_approve: vec![],
            ..cfg.clone()
        });
        assert!(
            mgr.needs_approval("memory_recall"),
            "removing a tool from auto_approve must take effect without a restart"
        );
    }

    /// The manager is built once when channels start. Before this, it cached
    /// the boot autonomy level, so tightening autonomy at runtime left the
    /// channel approval gate open — fail-open on the exact path that matters.
    #[test]
    fn needs_approval_follows_a_live_autonomy_tightening() {
        let policy =
            Arc::new(crate::security::SecurityPolicy::default().with_autonomy(AutonomyLevel::Full));
        let mgr = ApprovalManager::from_config(&full_config()).with_policy(Arc::clone(&policy));

        assert!(
            !mgr.needs_approval("shell"),
            "full autonomy should not prompt"
        );

        policy.apply_config(&supervised_config());
        assert!(
            mgr.needs_approval("shell"),
            "tightening autonomy at runtime must close the approval gate"
        );
    }

    #[test]
    fn needs_approval_follows_a_live_autonomy_loosening() {
        let policy = Arc::new(
            crate::security::SecurityPolicy::default().with_autonomy(AutonomyLevel::Supervised),
        );
        let mgr =
            ApprovalManager::from_config(&supervised_config()).with_policy(Arc::clone(&policy));

        assert!(
            mgr.needs_approval("shell"),
            "supervised should prompt for shell"
        );

        policy.apply_config(&full_config());
        assert!(
            !mgr.needs_approval("shell"),
            "loosening autonomy at runtime must open the gate"
        );
    }

    /// Callers that rebuild per turn are unaffected: with no policy attached
    /// the manager keeps reading its construction-time snapshot.
    #[test]
    fn needs_approval_without_a_policy_uses_the_snapshot() {
        assert!(!ApprovalManager::from_config(&full_config()).needs_approval("shell"));
        assert!(ApprovalManager::from_config(&supervised_config()).needs_approval("shell"));

        let read_only = AutonomyConfig {
            level: AutonomyLevel::ReadOnly,
            ..AutonomyConfig::default()
        };
        assert!(!ApprovalManager::from_config(&read_only).needs_approval("shell"));
    }

    fn supervised_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Supervised,
            auto_approve: vec!["file_read".into(), "memory_recall".into()],
            always_ask: vec!["shell".into()],
            ..AutonomyConfig::default()
        }
    }

    fn full_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Full,
            ..AutonomyConfig::default()
        }
    }

    // ── owner authority gate ─────────────────────────────────

    #[test]
    fn owner_gate_denies_by_default_and_matches_owners() {
        // Empty owner list ⇒ nobody can approve (secure default).
        assert!(!can_approve(&[], "alice"));
        // Exact match authorizes; non-owners are rejected even if they can chat.
        let owners = vec!["alice".to_string(), "123456".to_string()];
        assert!(can_approve(&owners, "alice"));
        assert!(can_approve(&owners, "123456"));
        assert!(!can_approve(&owners, "bob"));
        // Wildcard authorizes anyone (insecure, opt-in).
        assert!(can_approve(&["*".to_string()], "anyone"));

        // Normalization matches the allowed_users gate exactly: a leading `@`
        // is stripped on both sides (so a hand-edited config doesn't silently
        // fail), but matching stays case-sensitive — the two gates never disagree.
        let owners = vec!["@rantaiclaw_owner".to_string()];
        assert!(can_approve(&owners, "rantaiclaw_owner"));
        assert!(can_approve(&owners, "@rantaiclaw_owner"));
        assert!(!can_approve(&owners, "RantaiClaw_Owner")); // case-sensitive, like allowed_users
        assert!(!can_approve(&owners, "someone_else"));
    }

    #[test]
    fn owner_gate_matches_any_of_a_senders_identity_forms() {
        // Owner stored as a Telegram numeric id. The runtime resolves the
        // sender to their username, but the numeric id is available as an alias
        // — the owner must be recognized, matching the chat allowlist which
        // already checks both forms.
        let owners = vec!["100000001".to_string()];
        assert!(can_approve_any(&owners, ["rantaiclaw_user", "100000001"]));
        // The single-form check still misses it — this is the asymmetry the
        // alias-aware check fixes.
        assert!(!can_approve(&owners, "rantaiclaw_user"));
        // Secure default and wildcard behave like `can_approve`.
        assert!(!can_approve_any(&[], ["a", "b"]));
        assert!(can_approve_any(&["*".to_string()], ["anyone"]));
        // A leading `@` is tolerated on either side, as in `can_approve`.
        assert!(can_approve_any(
            &["@rantaiclaw_owner".to_string()],
            ["@rantaiclaw_owner"]
        ));
    }

    // ── needs_approval ───────────────────────────────────────

    #[test]
    fn auto_approve_tools_skip_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(!mgr.needs_approval("file_read"));
        assert!(!mgr.needs_approval("memory_recall"));
    }

    #[test]
    fn always_ask_tools_always_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn unknown_tool_needs_approval_in_supervised() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));
        assert!(mgr.needs_approval("http_request"));
    }

    #[test]
    fn seeded_session_allowlist_skips_the_prompt() {
        // A tool granted "Always" earlier in the conversation is re-seeded onto a
        // fresh per-turn manager and must no longer prompt (web session-grant
        // parity with the TUI).
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("http_request"), "prompts before seeding");
        mgr.seed_session_allowlist(["http_request".to_string()]);
        assert!(
            !mgr.needs_approval("http_request"),
            "a seeded tool must not prompt again"
        );
        assert!(
            mgr.needs_approval("file_write"),
            "unrelated tools still prompt"
        );
    }

    #[test]
    fn full_autonomy_never_prompts() {
        let mgr = ApprovalManager::from_config(&full_config());
        assert!(!mgr.needs_approval("shell"));
        assert!(!mgr.needs_approval("file_write"));
        assert!(!mgr.needs_approval("anything"));
    }

    #[test]
    fn readonly_never_prompts() {
        let config = AutonomyConfig {
            level: AutonomyLevel::ReadOnly,
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&config);
        assert!(!mgr.needs_approval("shell"));
    }

    // ── session allowlist ────────────────────────────────────

    #[test]
    fn always_response_adds_to_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));

        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "test.txt"}),
            ApprovalResponse::Always,
            "cli",
        );

        // Now file_write should be in session allowlist.
        assert!(!mgr.needs_approval("file_write"));
    }

    #[test]
    fn clearing_the_session_allowlist_revokes_a_prior_always_grant() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "x"}),
            ApprovalResponse::Always,
            "cli",
        );
        assert!(!mgr.needs_approval("file_write"), "granted this session");
        // A live tightening/reload must revoke the blanket grant.
        mgr.clear_session_allowlist();
        assert!(
            mgr.needs_approval("file_write"),
            "clearing the session allowlist must re-prompt"
        );
    }

    #[test]
    fn always_ask_overrides_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        // Even after "Always" for shell, it should still prompt.
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Always,
            "cli",
        );

        // shell is in always_ask, so it still needs approval.
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn yes_response_does_not_add_to_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "file_write",
            &serde_json::json!({}),
            ApprovalResponse::Yes,
            "cli",
        );
        assert!(mgr.needs_approval("file_write"));
    }

    // ── audit log ────────────────────────────────────────────

    #[test]
    fn audit_log_records_decisions() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "rm -rf ./build/"}),
            ApprovalResponse::No,
            "cli",
        );
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "out.txt", "content": "hello"}),
            ApprovalResponse::Yes,
            "cli",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].tool_name, "shell");
        assert_eq!(log[0].decision, ApprovalResponse::No);
        assert_eq!(log[1].tool_name, "file_write");
        assert_eq!(log[1].decision, ApprovalResponse::Yes);
    }

    #[test]
    fn audit_log_contains_timestamp_and_channel() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Yes,
            "telegram",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert!(!log[0].timestamp.is_empty());
        assert_eq!(log[0].channel, "telegram");
    }

    // ── summarize_args ───────────────────────────────────────

    #[test]
    fn summarize_args_object() {
        let args = serde_json::json!({"command": "ls -la", "cwd": "/tmp"});
        let summary = summarize_args(&args);
        assert!(summary.contains("command: ls -la"));
        assert!(summary.contains("cwd: /tmp"));
    }

    #[test]
    fn summarize_args_truncates_long_values() {
        let long_val = "x".repeat(200);
        let args = serde_json::json!({"content": long_val});
        let summary = summarize_args(&args);
        assert!(summary.contains('…'));
        assert!(summary.len() < 200);
    }

    #[test]
    fn summarize_args_unicode_safe_truncation() {
        let long_val = "🦀".repeat(120);
        let args = serde_json::json!({"content": long_val});
        let summary = summarize_args(&args);
        assert!(summary.contains("content:"));
        assert!(summary.contains('…'));
    }

    #[test]
    fn summarize_args_non_object() {
        let args = serde_json::json!("just a string");
        let summary = summarize_args(&args);
        assert!(summary.contains("just a string"));
    }

    // ── ApprovalResponse serde ───────────────────────────────

    #[test]
    fn approval_response_serde_roundtrip() {
        let json = serde_json::to_string(&ApprovalResponse::Always).unwrap();
        assert_eq!(json, "\"always\"");
        let parsed: ApprovalResponse = serde_json::from_str("\"no\"").unwrap();
        assert_eq!(parsed, ApprovalResponse::No);
    }

    // ── ApprovalRequest ──────────────────────────────────────

    #[test]
    fn approval_request_serde() {
        let req = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ApprovalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_name, "shell");
    }

    /// The headless surface (`"scheduler"`) selects the auto-deny backend, which
    /// decides `No` without touching stdin — so a scheduled agent job that hits
    /// an approval-required tool is denied, never blocked.
    #[tokio::test]
    async fn scheduler_surface_auto_denies_without_hanging() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({ "command": "echo hi" }),
        };
        let backend = default_backend_for("scheduler");
        assert_eq!(backend.decide(&mgr, &request).await, ApprovalResponse::No);
    }

    /// Defense-in-depth: even the interactive backend refuses to block on a read
    /// that cannot be answered when stdin is not a terminal (systemd, piped, or a
    /// headless run routed here by mistake).
    #[test]
    fn cli_prompt_denies_when_stdin_not_a_terminal() {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            return; // interactive run: the guard only fires for non-TTY stdin
        }
        let request = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({ "command": "echo hi" }),
        };
        assert_eq!(prompt_cli_interactive(&request), ApprovalResponse::No);
    }
}
