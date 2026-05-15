use anyhow::Result;
use tokio::sync::mpsc;

use crate::agent::events::AgentEvent;
use crate::sessions::{derive_session_title, Message, SessionStore};
use crate::tui::async_bridge::TurnRequest;

/// Accumulated token usage for the current TUI session.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Holds the runtime state of an active TUI conversation.
pub struct TuiContext {
    pub session_id: String,
    pub session_store: SessionStore,
    pub messages: Vec<Message>,
    pub model: String,
    pub input_buffer: String,
    pub scroll_offset: usize,
    pub token_usage: TokenUsage,
    /// Total context window of the active model, in tokens. Used by the
    /// status bar to display a `used/window  pct%` meter. `None` when the
    /// provider didn't surface a window size.
    pub context_window: Option<u64>,
    /// When the TUI session started — used by the status bar to display a
    /// compact `1h2m` / `34m` / `12s` age label.
    pub started_at: std::time::Instant,
    pub last_error: Option<String>,
    pub debug_mode: bool,
    /// Outbound channel to the `TuiAgentActor` for submitting turn requests.
    pub req_tx: mpsc::Sender<TurnRequest>,
    /// Inbound channel from the `TuiAgentActor` for draining agent events.
    pub events_rx: mpsc::Receiver<AgentEvent>,
    /// Number of turn requests currently queued at the actor (submitted but
    /// not yet completed). Incremented on submit, decremented on `Done`.
    pub queued_turns: usize,
    /// Canonical names of providers detected as enabled (have an API key
    /// configured) when the TUI launched. Used by `/model` to populate
    /// the model picker. First entry is the primary/default provider.
    pub available_providers: Vec<String>,
    /// Skills loaded from the workspace at TUI startup. Used by
    /// direct slash-skill invocation and agent prompt construction.
    pub available_skills: Vec<crate::skills::Skill>,
    /// All loaded skills, including gated/disabled rows, paired with
    /// unmet gating reasons. Used by `/skills` so install-deps can be
    /// reached from rows that are not active yet.
    pub available_skills_with_status: Vec<(crate::skills::Skill, Vec<String>)>,
    /// Snapshot of `(command_name, description)` pairs taken at TUI
    /// startup. Used by `/help` to populate the help picker without
    /// reaching back into the command registry from inside handlers.
    pub available_commands: Vec<(String, String)>,
    /// Submitted prompts in chronological order (oldest first). Used
    /// by Up/Down to recall past prompts when the input is empty or
    /// when already in history-navigation mode.
    /// How many configured channels were dispatched to `start_channels`
    /// when the TUI launched. Surfaced by `/channels` and `/platforms`
    /// so the user can see whether their Telegram / Discord / etc. is
    /// actually being polled by this process. `0` means TUI-only mode.
    pub channels_autostart_count: usize,
    /// Snapshot of `(name, configured)` rows at TUI startup. Used by
    /// `/channels` and `/platforms` to render the table without needing
    /// live access to the on-disk config. Refreshed by `reload_config`.
    pub channels_summary: Vec<(String, bool)>,
    pub input_history: Vec<String>,
    /// Current position in history navigation, indexed from the end:
    /// `Some(0)` = most recent submission, `Some(1)` = next-most, etc.
    /// `None` = not navigating history.
    pub input_history_pos: Option<usize>,
    /// Buffer contents at the moment the user started navigating
    /// history (so Down past the newest entry restores what they were
    /// typing). `None` when history navigation is inactive.
    pub input_history_stash: Option<String>,
    /// Shared security policy handle. `Some` when the TUI was launched
    /// against a real agent (`Agent::from_config`); `None` for unit
    /// tests / `test_context()`. Used by `/allow`, `/deny`, and
    /// `/allowlist` slash commands to mutate the runtime allowlist
    /// live, and to resolve pending approvals surfaced by the shell
    /// tool.
    pub security: Option<std::sync::Arc<crate::security::SecurityPolicy>>,
    /// Snapshot of the most recently finished turn's tool calls. Used
    /// by the `/calls` slash command so the user can see what the
    /// agent did, especially after a soft-cap hit. Refreshed on every
    /// `finalize_turn`; empty until at least one turn completes.
    pub last_turn_tool_calls: Vec<crate::tui::render::PersistedToolCall>,
    /// MCP server names from `config.toml` `[mcp_servers.*]`. Used by
    /// `/mcp` so the user can see which servers were *asked for*,
    /// independent of whether discovery succeeded.
    pub mcp_servers_configured: std::collections::HashSet<String>,
    /// Live MCP tool registry, keyed by server name. Each value is
    /// the list of fully-qualified tool names the agent actually
    /// has access to. Empty entries mean the server is configured
    /// but discovery failed.
    pub mcp_tools_by_server: std::collections::HashMap<String, Vec<String>>,
}

impl TuiContext {
    /// Create a new context, opening (or creating) a session in the store.
    ///
    /// If `resume_session` is `Some`, the existing session is loaded;
    /// otherwise a fresh session is created.
    pub fn new(
        session_store: SessionStore,
        model: &str,
        resume_session: Option<&str>,
        req_tx: mpsc::Sender<TurnRequest>,
        events_rx: mpsc::Receiver<AgentEvent>,
    ) -> Result<Self> {
        let (session_id, messages) = match resume_session {
            Some(id) => {
                let msgs = session_store.get_messages(id)?;
                (id.to_string(), msgs)
            }
            None => {
                let session = session_store.new_session(model, "tui")?;
                (session.id, Vec::new())
            }
        };

        Ok(Self {
            session_id,
            session_store,
            messages,
            model: model.to_string(),
            input_buffer: String::new(),
            scroll_offset: 0,
            token_usage: TokenUsage::default(),
            context_window: None,
            started_at: std::time::Instant::now(),
            last_error: None,
            debug_mode: false,
            req_tx,
            events_rx,
            queued_turns: 0,
            available_providers: Vec::new(),
            available_skills: Vec::new(),
            available_skills_with_status: Vec::new(),
            available_commands: Vec::new(),
            channels_autostart_count: 0,
            channels_summary: Vec::new(),
            input_history: Vec::new(),
            input_history_pos: None,
            input_history_stash: None,
            security: None,
            last_turn_tool_calls: Vec::new(),
            mcp_servers_configured: std::collections::HashSet::new(),
            mcp_tools_by_server: std::collections::HashMap::new(),
        })
    }

    /// Append a submission to the input-history ring. Skips empty
    /// strings and adjacent duplicates. Caps the history at 200 entries
    /// to bound memory.
    pub fn push_input_history(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self
            .input_history
            .last()
            .is_some_and(|prev| prev == trimmed)
        {
            return;
        }
        const MAX_HISTORY: usize = 200;
        if self.input_history.len() >= MAX_HISTORY {
            self.input_history.remove(0);
        }
        self.input_history.push(trimmed.to_string());
    }

    /// Advance one step deeper into history (older entry). On the first
    /// call it stashes the current buffer so Down can restore it.
    /// Returns the resulting buffer contents to display, or `None` when
    /// no further history is available.
    pub fn history_recall_older(&mut self) -> Option<String> {
        if self.input_history.is_empty() {
            return None;
        }
        let next_pos = match self.input_history_pos {
            None => {
                // Entering history mode — stash the live buffer.
                self.input_history_stash = Some(self.input_buffer.clone());
                0
            }
            Some(p) => p + 1,
        };
        if next_pos >= self.input_history.len() {
            return None; // already at the oldest entry
        }
        self.input_history_pos = Some(next_pos);
        let idx = self.input_history.len() - 1 - next_pos;
        Some(self.input_history[idx].clone())
    }

    /// Step one toward the present (newer entry). Returns the buffer
    /// contents to display, or `None` if not currently in history mode.
    /// When stepping past the newest entry, the stashed live buffer
    /// is restored and history mode exits.
    pub fn history_recall_newer(&mut self) -> Option<String> {
        let pos = self.input_history_pos?;
        if pos == 0 {
            // Past the newest → exit history mode, restore stash.
            self.input_history_pos = None;
            return self.input_history_stash.take();
        }
        let next = pos - 1;
        self.input_history_pos = Some(next);
        let idx = self.input_history.len() - 1 - next;
        Some(self.input_history[idx].clone())
    }

    /// Drop history-navigation state without altering the buffer. Call
    /// whenever the user edits the input (typing or backspace) so the
    /// edits aren't clobbered by Up/Down.
    pub fn exit_history_navigation(&mut self) {
        self.input_history_pos = None;
        self.input_history_stash = None;
    }

    /// Build a `TuiContext` suitable for unit tests, returning the peer ends
    /// of the bridge channels so tests can assert on what the TUI sends and
    /// feed simulated agent events back in.
    #[cfg(test)]
    pub fn test_context() -> (
        TuiContext,
        mpsc::Receiver<TurnRequest>,
        mpsc::Sender<AgentEvent>,
    ) {
        let store = SessionStore::in_memory().expect("in-memory session store");
        let (req_tx, req_rx) = mpsc::channel(4);
        let (events_tx, events_rx) = mpsc::channel(32);
        let ctx = TuiContext::new(store, "mock-model", None, req_tx, events_rx)
            .expect("test context creation");
        (ctx, req_rx, events_tx)
    }

    /// Append a user message to the in-memory list and persist it.
    ///
    /// If this is the first message in the session and the session has no
    /// title yet, derive a title from the message preview and persist it.
    /// Title-write errors are swallowed — never block the user turn on a
    /// best-effort UI affordance.
    pub fn append_user_message(&mut self, content: &str) -> Result<()> {
        let is_first_message = self.messages.is_empty();
        let msg = Message::user(&self.session_id, content);
        self.session_store.append_message(&msg)?;
        self.messages.push(msg);

        if is_first_message {
            let title = derive_session_title(content);
            if !title.is_empty() {
                let _ = self.session_store.set_title(&self.session_id, &title);
            }
        }
        Ok(())
    }

    /// Append a system-role message (used for inline command output like
    /// `/usage`, `/sessions`, etc.). Persisted to the session store so
    /// scrollback + resume still show the line.
    pub fn append_system_message(&mut self, content: &str) -> Result<()> {
        let msg = Message {
            id: 0,
            session_id: self.session_id.clone(),
            role: "system".to_string(),
            content: content.to_string(),
            tool_calls: None,
            timestamp: chrono::Utc::now().timestamp(),
        };
        self.session_store.append_message(&msg)?;
        self.messages.push(msg);
        Ok(())
    }

    /// Append an assistant message to the in-memory list and persist it.
    pub fn append_assistant_message(&mut self, content: &str) -> Result<()> {
        self.append_assistant_message_with_tools(content, None)
    }

    /// Append an assistant message with optional tool-call snapshot
    /// (JSON-serialized PersistedToolCall list). Used by the bridge
    /// finalize path so chat history can re-render tool blocks after
    /// the streaming session ends.
    pub fn append_assistant_message_with_tools(
        &mut self,
        content: &str,
        tool_calls_json: Option<String>,
    ) -> Result<()> {
        let mut msg = Message::assistant(&self.session_id, content);
        msg.tool_calls = tool_calls_json;
        self.session_store.append_message(&msg)?;
        self.messages.push(msg);
        Ok(())
    }

    /// Reload all messages for the current session from the store.
    pub fn load_session_messages(&mut self) -> Result<()> {
        self.messages = self.session_store.get_messages(&self.session_id)?;
        Ok(())
    }

    /// End the current session and start a fresh one, clearing in-memory state.
    /// The active model is intentionally **preserved** — Hermes / Claude-Code
    /// convention: model is a runtime preference that survives `/new`.
    pub fn clear_session(&mut self) -> Result<()> {
        self.session_store.end_session(&self.session_id)?;
        let session = self.session_store.new_session(&self.model.clone(), "tui")?;
        self.session_id = session.id;
        self.messages.clear();
        self.input_buffer.clear();
        self.scroll_offset = 0;
        self.token_usage = TokenUsage::default();
        self.last_error = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_context(model: &str) -> TuiContext {
        let store = SessionStore::in_memory().expect("in-memory store");
        let (req_tx, _req_rx) = mpsc::channel(4);
        let (_events_tx, events_rx) = mpsc::channel(32);
        TuiContext::new(store, model, None, req_tx, events_rx).expect("context creation")
    }

    #[test]
    fn first_user_message_auto_titles_session() {
        let mut ctx = in_memory_context("test-model");
        let sid = ctx.session_id.clone();

        ctx.append_user_message("design the new picker overlay")
            .unwrap();

        let after = ctx.session_store.get_session(&sid).unwrap().unwrap();
        assert_eq!(
            after.title.as_deref(),
            Some("design the new picker overlay")
        );
    }

    #[test]
    fn second_user_message_does_not_overwrite_title() {
        let mut ctx = in_memory_context("test-model");
        let sid = ctx.session_id.clone();
        ctx.append_user_message("first prompt").unwrap();
        ctx.session_store.set_title(&sid, "manually set").unwrap();
        ctx.append_user_message("follow-up that should not retitle")
            .unwrap();
        let after = ctx.session_store.get_session(&sid).unwrap().unwrap();
        assert_eq!(after.title.as_deref(), Some("manually set"));
    }

    #[test]
    fn clear_session_preserves_active_model() {
        let mut ctx = in_memory_context("minimax:MiniMax-M2.5");
        // User switches model mid-session via /model.
        ctx.model = "openrouter:anthropic/claude-sonnet-4.6".to_string();
        ctx.clear_session().unwrap();
        // Model preference survives /new (Hermes/CC convention).
        assert_eq!(ctx.model, "openrouter:anthropic/claude-sonnet-4.6");
    }

    #[test]
    fn input_history_skips_empty_and_duplicate_submissions() {
        let mut ctx = in_memory_context("test-model");
        ctx.push_input_history("first prompt");
        ctx.push_input_history("first prompt"); // duplicate adjacent
        ctx.push_input_history("");
        ctx.push_input_history("   ");
        ctx.push_input_history("second prompt");
        assert_eq!(ctx.input_history, vec!["first prompt", "second prompt"]);
    }

    #[test]
    fn history_recall_walks_oldest_to_newest() {
        let mut ctx = in_memory_context("test-model");
        ctx.push_input_history("alpha");
        ctx.push_input_history("beta");
        ctx.push_input_history("gamma");

        // Up from empty buffer → newest first.
        assert_eq!(ctx.history_recall_older().as_deref(), Some("gamma"));
        assert_eq!(ctx.history_recall_older().as_deref(), Some("beta"));
        assert_eq!(ctx.history_recall_older().as_deref(), Some("alpha"));
        // Past the oldest → None, position stays.
        assert_eq!(ctx.history_recall_older(), None);
    }

    #[test]
    fn history_down_restores_stash_at_newest() {
        let mut ctx = in_memory_context("test-model");
        ctx.push_input_history("alpha");
        ctx.push_input_history("beta");
        ctx.input_buffer = "draft I was typing".to_string();

        // Up → enter history at newest, stash the draft.
        assert_eq!(ctx.history_recall_older().as_deref(), Some("beta"));
        assert_eq!(ctx.history_recall_older().as_deref(), Some("alpha"));
        // Down twice → back through newer, then stash.
        assert_eq!(ctx.history_recall_newer().as_deref(), Some("beta"));
        assert_eq!(
            ctx.history_recall_newer().as_deref(),
            Some("draft I was typing")
        );
        // Past the newest → None, history_pos cleared.
        assert_eq!(ctx.history_recall_newer(), None);
        assert!(ctx.input_history_pos.is_none());
        assert!(ctx.input_history_stash.is_none());
    }

    #[test]
    fn exit_history_navigation_drops_pos_and_stash() {
        let mut ctx = in_memory_context("test-model");
        ctx.push_input_history("alpha");
        let _ = ctx.history_recall_older();
        assert!(ctx.input_history_pos.is_some());
        ctx.exit_history_navigation();
        assert!(ctx.input_history_pos.is_none());
        assert!(ctx.input_history_stash.is_none());
    }

    #[test]
    fn context_appends_messages() {
        let mut ctx = in_memory_context("test-model");

        ctx.append_user_message("hello").unwrap();
        ctx.append_assistant_message("world").unwrap();

        assert_eq!(ctx.messages.len(), 2);
        assert_eq!(ctx.messages[0].role, "user");
        assert_eq!(ctx.messages[0].content, "hello");
        assert_eq!(ctx.messages[1].role, "assistant");
        assert_eq!(ctx.messages[1].content, "world");
    }

    #[test]
    fn context_loads_existing_messages() {
        let store = SessionStore::in_memory().expect("in-memory store");
        let session = store.new_session("test-model", "tui").unwrap();

        store
            .append_message(&Message::user(&session.id, "persisted"))
            .unwrap();

        let (req_tx, _req_rx) = mpsc::channel(4);
        let (_events_tx, events_rx) = mpsc::channel(32);
        let ctx = TuiContext::new(store, "test-model", Some(&session.id), req_tx, events_rx)
            .expect("context resume");

        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].content, "persisted");
        assert_eq!(ctx.session_id, session.id);
    }

    #[test]
    fn test_context_helper_wires_peer_channel_ends() {
        let (ctx, mut req_rx, events_tx) = TuiContext::test_context();
        assert_eq!(ctx.queued_turns, 0);
        assert_eq!(ctx.model, "mock-model");
        // Peer ends are live: sending through the ctx reaches the test receiver,
        // and sending via the test sender reaches the ctx's events receiver.
        ctx.req_tx
            .try_send(TurnRequest::Submit("ping".into()))
            .expect("req channel open");
        match req_rx.try_recv().expect("req received") {
            TurnRequest::Submit(s) => assert_eq!(s, "ping"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
            TurnRequest::Reload(_) => panic!("expected Submit, got Reload"),
        }
        events_tx
            .try_send(AgentEvent::Chunk("ok".into()))
            .expect("events channel open");
    }
}
