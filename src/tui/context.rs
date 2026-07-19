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

/// A large paste held out of the composer behind a placeholder marker.
///
/// Substitution is by EXACT string match on `marker` — not by parsing a
/// bracket grammar — because we generate the marker ourselves. That sidesteps
/// the fragility that repeatedly bit `multimodal::parse_image_markers`
/// (nested `[::1]`, unbalanced `[`): there is no grammar here to break, only a
/// literal string we put in and take back out.
#[derive(Debug, Clone)]
pub struct PendingPaste {
    pub marker: String,
    pub content: String,
}

/// A paste that would occupy more than this many *wrapped* terminal rows is
/// collapsed to a placeholder; a shorter paste lands inline. Wrapped rows, not
/// raw line count: a few long paragraphs (lots of text, few `\n`s — e.g. three
/// prose paragraphs separated by blank lines) are only ~5 raw lines yet flood
/// the composer, and a line-count-only threshold let them through. The wrapped-
/// row estimate (see `estimated_paste_rows`) catches both tall and wide pastes.
pub const PASTE_PLACEHOLDER_MIN_ROWS: usize = 5;

/// Holds the runtime state of an active TUI conversation.
pub struct TuiContext {
    /// `None` until the first message is persisted. Deferring creation keeps
    /// launch-and-close (with no input) from leaving empty "Untitled" sessions.
    pub session_id: Option<String>,
    pub session_store: SessionStore,
    pub messages: Vec<Message>,
    pub model: String,
    pub input_buffer: String,
    /// Large pastes collapsed to a `[Pasted text #N +M lines]` placeholder in
    /// the buffer, with the real content held here until submit. Keeps a
    /// 500-line log paste from burying the composer while still sending the
    /// full text. Expanded and cleared by `expand_pending_pastes` on submit;
    /// cleared with the buffer on `/new`.
    pub pending_pastes: Vec<PendingPaste>,
    /// Cursor position inside `input_buffer`, measured in characters (not
    /// bytes). Always `<= input_buffer.chars().count()`. Drives the
    /// terminal cursor placement in `render_input_pane` and the insert /
    /// delete points for `KeyCode::Char`, `Backspace`, `Delete`, `Left`,
    /// `Right`, `Home`, `End`. Must be reset (usually to the end of the
    /// new buffer) whenever `input_buffer` is replaced wholesale.
    pub cursor_pos: usize,
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
    /// Whether the active provider has a usable API key (or is a local provider
    /// that needs none). Precomputed where `Config` is available — same pattern
    /// as `channels_summary` — so `/doctor` can report the truth without the
    /// panel carrying `Config`. `None` until the first config load; the panel
    /// is unreachable before then. Refreshed by `reload_config`.
    pub provider_key_ok: Option<bool>,
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
    /// Shared memory backend handle. `Some` when the TUI was launched
    /// against a real agent (`Agent::from_config`); `None` for unit
    /// tests / `test_context()`. Used by `/memory` and `/forget`
    /// slash commands to drive the user-facing memory surface
    /// without going through a turn round-trip.
    pub memory: Option<std::sync::Arc<dyn crate::memory::Memory>>,
    /// Currently-active approval preset for this profile (`Manual` /
    /// `Smart` / `Strict` / `Off`). Snapshotted at TUI startup and
    /// refreshed whenever Shift+Tab / `/autonomy` writes a new preset.
    /// `None` means the policy dir hasn't been provisioned yet (pre-
    /// onboarding) or the `[autonomy].preset` field couldn't be read —
    /// the status bar hides the segment in either case.
    pub autonomy_preset: Option<crate::approval::policy_writer::PolicyPreset>,
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
    /// Create a new context for a TUI conversation.
    ///
    /// If `resume_session` is `Some`, that existing session is loaded and bound
    /// immediately. Otherwise the context starts unbound (`session_id = None`)
    /// and defers creating its session until the first message is persisted.
    pub fn new(
        session_store: SessionStore,
        model: &str,
        resume_session: Option<&str>,
        req_tx: mpsc::Sender<TurnRequest>,
        events_rx: mpsc::Receiver<AgentEvent>,
    ) -> Result<Self> {
        // Resuming binds to the existing session immediately; a fresh launch
        // stays unbound (`None`) and creates its session lazily on first message.
        let (session_id, messages) = match resume_session {
            Some(id) => {
                let msgs = session_store.get_messages(id)?;
                (Some(id.to_string()), msgs)
            }
            None => (None, Vec::new()),
        };

        Ok(Self {
            session_id,
            session_store,
            messages,
            model: model.to_string(),
            input_buffer: String::new(),
            pending_pastes: Vec::new(),
            cursor_pos: 0,
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
            provider_key_ok: None,
            input_history: Vec::new(),
            input_history_pos: None,
            input_history_stash: None,
            security: None,
            memory: None,
            autonomy_preset: None,
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

    /// Character count of the input buffer. Used as the upper bound for
    /// `cursor_pos` and as the "End" target.
    pub fn input_char_count(&self) -> usize {
        self.input_buffer.chars().count()
    }

    /// Convert a `cursor_pos` char index into the matching byte index
    /// inside `input_buffer`. Returns `input_buffer.len()` when the
    /// cursor sits at end-of-buffer.
    fn cursor_byte_index(&self) -> usize {
        self.input_buffer
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(b, _)| b)
            .unwrap_or(self.input_buffer.len())
    }

    /// Place the cursor at end-of-buffer. Call this any time
    /// `input_buffer` is replaced wholesale (history recall, slash
    /// completion, external editor return, list-picker prefill).
    pub fn cursor_to_end(&mut self) {
        self.cursor_pos = self.input_char_count();
    }

    /// Insert a single char at the cursor and advance past it.
    pub fn insert_char_at_cursor(&mut self, c: char) {
        let byte_idx = self.cursor_byte_index();
        self.input_buffer.insert(byte_idx, c);
        self.cursor_pos += 1;
    }

    /// Insert a chunk of text at the cursor and advance past it. Used by
    /// the bracketed-paste handler so multi-line pastes land as a single
    /// buffer mutation rather than a stream of per-char inserts (which
    /// would also have to split on the embedded `\n` becoming an
    /// `Enter` event and auto-submitting the prompt).
    pub fn paste_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let byte_idx = self.cursor_byte_index();
        self.input_buffer.insert_str(byte_idx, text);
        self.cursor_pos += text.chars().count();
    }

    /// Register a large paste and return the placeholder marker to insert in
    /// its place. The marker is numbered per pending list so multiple pastes in
    /// one message read `#1`, `#2`, … .
    pub fn register_paste(&mut self, content: String) -> String {
        let id = self.pending_pastes.len() + 1;
        // `lines()` not `split('\n')`: a trailing newline should not inflate
        // the count a user would get by eye (a 40-line paste reads "+40").
        let lines = content.lines().count();
        let marker = format!("[Pasted text #{id} +{lines} lines]");
        self.pending_pastes.push(PendingPaste {
            marker: marker.clone(),
            content,
        });
        marker
    }

    /// Replace each pending placeholder marker in `text` with its real content
    /// and clear the pending list. Exact-string substitution: a marker the user
    /// edited or deleted no longer matches, so its content is simply dropped
    /// (they removed it) and the leftover text goes through literally. Called
    /// once at submit.
    pub fn expand_pending_pastes(&mut self, text: String) -> String {
        if self.pending_pastes.is_empty() {
            return text;
        }
        let mut out = text;
        for p in self.pending_pastes.drain(..) {
            out = out.replace(&p.marker, &p.content);
        }
        out
    }

    /// Delete the char immediately before the cursor (Backspace).
    /// No-op when the cursor is already at the start.
    ///
    /// Special case: if the text ending at the cursor is a whole pasted-text
    /// placeholder (`[Pasted text #N +M lines]`), delete the ENTIRE marker and
    /// drop its held content in one keystroke — matching Claude Code, where
    /// backspacing a paste chip removes it whole rather than one char at a time.
    pub fn backspace_at_cursor(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let cursor_byte = self.cursor_byte_index();
        if let Some(idx) = self
            .pending_pastes
            .iter()
            .position(|p| self.input_buffer[..cursor_byte].ends_with(&p.marker))
        {
            let marker_bytes = self.pending_pastes[idx].marker.len();
            let marker_chars = self.pending_pastes[idx].marker.chars().count();
            let start_byte = cursor_byte - marker_bytes;
            self.input_buffer.replace_range(start_byte..cursor_byte, "");
            self.cursor_pos -= marker_chars;
            self.pending_pastes.remove(idx);
            return;
        }
        self.cursor_pos -= 1;
        let byte_idx = self.cursor_byte_index();
        self.input_buffer.remove(byte_idx);
    }

    /// Delete the char at the cursor (Delete key). No-op when the cursor
    /// is at end-of-buffer.
    pub fn delete_at_cursor(&mut self) {
        if self.cursor_pos >= self.input_char_count() {
            return;
        }
        let byte_idx = self.cursor_byte_index();
        self.input_buffer.remove(byte_idx);
    }

    /// Move the cursor one char left, saturating at 0.
    pub fn cursor_left(&mut self) {
        self.cursor_pos = self.cursor_pos.saturating_sub(1);
    }

    /// Move the cursor one char right, clamped at end-of-buffer.
    pub fn cursor_right(&mut self) {
        let max = self.input_char_count();
        if self.cursor_pos < max {
            self.cursor_pos += 1;
        }
    }

    /// Move the cursor up one logical line (split on `\n`), keeping the same
    /// column where possible. Returns `false` when the cursor is already on the
    /// first line, so the caller can fall back to history recall. This gives
    /// editor-style vertical movement in a multi-line composer while a
    /// single-line input still maps Up straight to history.
    pub fn cursor_move_up(&mut self) -> bool {
        let chars: Vec<char> = self.input_buffer.chars().collect();
        // Start of the line the cursor currently sits on.
        let mut line_start = 0;
        for (i, &c) in chars.iter().enumerate().take(self.cursor_pos) {
            if c == '\n' {
                line_start = i + 1;
            }
        }
        if line_start == 0 {
            return false; // already on the first line
        }
        let col = self.cursor_pos - line_start;
        // The `\n` ending the previous line sits at `line_start - 1`.
        let mut prev_start = 0;
        for (i, &c) in chars.iter().enumerate().take(line_start - 1) {
            if c == '\n' {
                prev_start = i + 1;
            }
        }
        let prev_len = (line_start - 1) - prev_start;
        self.cursor_pos = prev_start + col.min(prev_len);
        true
    }

    /// Move the cursor down one logical line, keeping the same column where
    /// possible. Returns `false` when the cursor is already on the last line,
    /// so the caller can fall back to history recall.
    pub fn cursor_move_down(&mut self) -> bool {
        let chars: Vec<char> = self.input_buffer.chars().collect();
        let n = chars.len();
        let mut line_start = 0;
        for (i, &c) in chars.iter().enumerate().take(self.cursor_pos) {
            if c == '\n' {
                line_start = i + 1;
            }
        }
        let col = self.cursor_pos - line_start;
        // End of the current line = the next `\n` at or after `line_start`.
        let Some(cur_end) = chars[line_start..]
            .iter()
            .position(|&c| c == '\n')
            .map(|off| line_start + off)
        else {
            return false; // no newline after → already on the last line
        };
        let next_start = cur_end + 1;
        let next_end = chars[next_start..]
            .iter()
            .position(|&c| c == '\n')
            .map(|off| next_start + off)
            .unwrap_or(n);
        let next_len = next_end - next_start;
        self.cursor_pos = next_start + col.min(next_len);
        true
    }

    /// Jump the cursor to the beginning of the buffer (Home).
    pub fn cursor_home(&mut self) {
        self.cursor_pos = 0;
    }

    /// Jump the cursor to the end of the buffer (End). Alias for
    /// `cursor_to_end` named for keyboard symmetry with `cursor_home`.
    pub fn cursor_end(&mut self) {
        self.cursor_to_end();
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

    /// Return the active session id, creating the session lazily on first use.
    ///
    /// The TUI defers creation until there is something to persist, so opening
    /// and closing without typing never leaves an empty, untitled session.
    fn ensure_session_id(&mut self) -> Result<String> {
        if let Some(id) = &self.session_id {
            return Ok(id.clone());
        }
        let session = self.session_store.new_session(&self.model.clone(), "tui")?;
        self.session_id = Some(session.id.clone());
        Ok(session.id)
    }

    /// Short session id for status displays; `"new"` while the session is
    /// still unbound (no message sent yet).
    pub fn session_id_short(&self) -> &str {
        match &self.session_id {
            Some(id) => &id[..8.min(id.len())],
            None => "new",
        }
    }

    /// Append a user message to the in-memory list and persist it.
    ///
    /// If this is the first message in the session and the session has no
    /// title yet, derive a title from the message preview and persist it.
    /// Title-write errors are swallowed — never block the user turn on a
    /// best-effort UI affordance.
    pub fn append_user_message(&mut self, content: &str) -> Result<()> {
        let is_first_message = self.messages.is_empty();
        let sid = self.ensure_session_id()?;
        let msg = Message::user(&sid, content);
        self.session_store.append_message(&msg)?;
        self.messages.push(msg);

        if is_first_message {
            let title = derive_session_title(content);
            if !title.is_empty() {
                let _ = self.session_store.set_title(&sid, &title);
            }
        }
        Ok(())
    }

    /// Append a system-role message (used for inline command output like
    /// `/usage`, `/sessions`, etc.). Persisted to the session store so
    /// scrollback + resume still show the line.
    pub fn append_system_message(&mut self, content: &str) -> Result<()> {
        let sid = self.ensure_session_id()?;
        let msg = Message {
            id: 0,
            session_id: sid,
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
        let sid = self.ensure_session_id()?;
        let mut msg = Message::assistant(&sid, content);
        msg.tool_calls = tool_calls_json;
        self.session_store.append_message(&msg)?;
        self.messages.push(msg);
        Ok(())
    }

    /// Reload all messages for the current session from the store.
    pub fn load_session_messages(&mut self) -> Result<()> {
        match &self.session_id {
            Some(sid) => self.messages = self.session_store.get_messages(sid)?,
            None => self.messages.clear(),
        }
        Ok(())
    }

    /// End the current session and start a fresh one, clearing in-memory state.
    /// The active model is intentionally **preserved** — Hermes / Claude-Code
    /// convention: model is a runtime preference that survives `/new`.
    pub fn clear_session(&mut self) -> Result<()> {
        // End the current session if one exists; leave `session_id` unbound so
        // the next message lazily creates a fresh one (no empty session on /new).
        if let Some(sid) = self.session_id.take() {
            self.session_store.end_session(&sid)?;
        }
        self.messages.clear();
        self.input_buffer.clear();
        self.pending_pastes.clear();
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

    fn tall(n: usize) -> String {
        (0..n)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole point: the placeholder shown in the composer must expand back
    /// to the exact pasted content on submit — nothing lost, nothing altered.
    #[test]
    fn a_registered_paste_round_trips_through_the_placeholder() {
        let mut ctx = in_memory_context("m");
        let content = tall(40);
        let marker = ctx.register_paste(content.clone());
        assert!(marker.starts_with("[Pasted text #1 +"));
        // The buffer would hold the marker plus whatever the user types around it.
        let buffer = format!("please review {marker} and explain");
        let expanded = ctx.expand_pending_pastes(buffer);
        assert_eq!(expanded, format!("please review {content} and explain"));
        assert!(ctx.pending_pastes.is_empty(), "consumed on expand");
    }

    #[test]
    fn multiple_pastes_number_and_expand_independently() {
        let mut ctx = in_memory_context("m");
        let a = ctx.register_paste("AAA\nAAA".into());
        let b = ctx.register_paste("BBB\nBBB".into());
        assert!(a.contains("#1"));
        assert!(b.contains("#2"));
        let expanded = ctx.expand_pending_pastes(format!("{a} then {b}"));
        assert_eq!(expanded, "AAA\nAAA then BBB\nBBB");
    }

    /// If the user edits or deletes the marker, it no longer matches, so its
    /// content is dropped (they removed it) and the rest passes through — no
    /// panic, no stray content.
    #[test]
    fn an_edited_marker_is_not_expanded() {
        let mut ctx = in_memory_context("m");
        let _marker = ctx.register_paste(tall(40));
        let expanded = ctx.expand_pending_pastes("I deleted the paste".into());
        assert_eq!(expanded, "I deleted the paste");
        assert!(ctx.pending_pastes.is_empty());
    }

    #[test]
    fn expand_is_a_noop_without_pending_pastes() {
        let mut ctx = in_memory_context("m");
        assert_eq!(ctx.expand_pending_pastes("hello".into()), "hello");
    }

    /// A paste containing a `]` or a bracketed IPv6 host must round-trip
    /// intact — the exact-string substitution has no grammar to trip on,
    /// unlike the image-marker parser.
    #[test]
    fn content_with_brackets_round_trips() {
        let mut ctx = in_memory_context("m");
        let content =
            "curl http://[::1]:8080/x\ngrep '[' file\nmore\nlines\nhere\nand more".to_string();
        let marker = ctx.register_paste(content.clone());
        assert_eq!(ctx.expand_pending_pastes(marker), content);
    }

    /// `/new` clears held pastes so they cannot leak into the next message.
    #[test]
    fn new_session_clears_pending_pastes() {
        let mut ctx = in_memory_context("m");
        ctx.register_paste(tall(40));
        assert!(!ctx.pending_pastes.is_empty());
        ctx.clear_session().unwrap();
        assert!(ctx.pending_pastes.is_empty());
    }

    #[test]
    fn backspace_deletes_a_whole_pasted_placeholder_at_once() {
        let mut ctx = in_memory_context("m");
        let marker = ctx.register_paste(tall(40));
        ctx.paste_at_cursor(&marker);
        assert_eq!(ctx.input_buffer, marker);
        assert_eq!(ctx.cursor_pos, marker.chars().count());
        assert_eq!(ctx.pending_pastes.len(), 1);
        // One Backspace removes the whole marker AND drops its held content.
        ctx.backspace_at_cursor();
        assert_eq!(ctx.input_buffer, "", "marker deleted whole, not per-char");
        assert_eq!(ctx.cursor_pos, 0);
        assert!(ctx.pending_pastes.is_empty(), "held content dropped too");
    }

    #[test]
    fn backspace_not_after_a_placeholder_removes_one_char() {
        let mut ctx = in_memory_context("m");
        ctx.paste_at_cursor("hi ");
        let marker = ctx.register_paste(tall(40));
        ctx.paste_at_cursor(&marker);
        ctx.paste_at_cursor(" bye"); // cursor now sits after "bye", not the marker
        ctx.backspace_at_cursor();
        assert_eq!(ctx.input_buffer, format!("hi {marker} by"));
        assert_eq!(ctx.pending_pastes.len(), 1, "marker left intact");
    }

    #[test]
    fn first_user_message_auto_titles_session() {
        let mut ctx = in_memory_context("test-model");

        ctx.append_user_message("design the new picker overlay")
            .unwrap();

        let sid = ctx
            .session_id
            .clone()
            .expect("session created on first message");
        let after = ctx.session_store.get_session(&sid).unwrap().unwrap();
        assert_eq!(
            after.title.as_deref(),
            Some("design the new picker overlay")
        );
    }

    #[test]
    fn new_context_defers_session_creation_until_first_message() {
        let mut ctx = in_memory_context("test-model");

        // Opening the TUI without typing anything must NOT create a session —
        // otherwise every launch-and-close leaves an empty "Untitled" row.
        assert_eq!(
            ctx.session_store.list_sessions(10).unwrap().len(),
            0,
            "no session should exist before the first message"
        );

        ctx.append_user_message("hello there agent").unwrap();

        // The first message lazily creates exactly one, titled from that message.
        let sessions = ctx.session_store.list_sessions(10).unwrap();
        assert_eq!(sessions.len(), 1, "first message creates the session");
        assert_eq!(sessions[0].title.as_deref(), Some("hello there agent"));
    }

    #[test]
    fn second_user_message_does_not_overwrite_title() {
        let mut ctx = in_memory_context("test-model");
        ctx.append_user_message("first prompt").unwrap();
        let sid = ctx
            .session_id
            .clone()
            .expect("session created on first message");
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
    fn cursor_moves_between_logical_lines_keeping_column() {
        let mut ctx = in_memory_context("m");
        ctx.input_buffer = "abc\ndefgh\nij".to_string(); // 3 lines: len 3, 5, 2
                                                         // Cursor at column 4 on the middle line ("defg|h").
        ctx.cursor_pos = 4 + 4;
        // Up → first line, column clamped to its length (3) = end of "abc".
        assert!(ctx.cursor_move_up());
        assert_eq!(ctx.cursor_pos, 3);
        // Down → middle line at column min(3, 5) = 3 ("def|gh").
        assert!(ctx.cursor_move_down());
        assert_eq!(ctx.cursor_pos, 4 + 3);
        // Down → last line "ij", column clamped to its length (2).
        assert!(ctx.cursor_move_down());
        assert_eq!(ctx.cursor_pos, 4 + 6 + 2);
        // Down on the last line → false so the caller recalls newer history.
        assert!(!ctx.cursor_move_down());
    }

    #[test]
    fn single_line_input_defers_vertical_move_to_history() {
        let mut ctx = in_memory_context("m");
        ctx.input_buffer = "just one line".to_string();
        ctx.cursor_to_end();
        // No line above or below → both return false so Up/Down hit history.
        assert!(!ctx.cursor_move_up());
        assert!(!ctx.cursor_move_down());
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
        assert_eq!(ctx.session_id.as_deref(), Some(session.id.as_str()));
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
            TurnRequest::Compact { .. } => panic!("expected Submit, got Compact"),
        }
        events_tx
            .try_send(AgentEvent::Chunk("ok".into()))
            .expect("events channel open");
    }
}
