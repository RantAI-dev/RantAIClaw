use anyhow::Result;

use crate::sessions::{Message, SessionStore};

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
    pub last_error: Option<String>,
    pub debug_mode: bool,
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
            last_error: None,
            debug_mode: false,
        })
    }

    /// Append a user message to the in-memory list and persist it.
    pub fn append_user_message(&mut self, content: &str) -> Result<()> {
        let msg = Message::user(&self.session_id, content);
        self.session_store.append_message(&msg)?;
        self.messages.push(msg);
        Ok(())
    }

    /// Append an assistant message to the in-memory list and persist it.
    pub fn append_assistant_message(&mut self, content: &str) -> Result<()> {
        let msg = Message::assistant(&self.session_id, content);
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
    pub fn clear_session(&mut self) -> Result<()> {
        self.session_store.end_session(&self.session_id)?;
        let session = self
            .session_store
            .new_session(&self.model.clone(), "tui")?;
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
        TuiContext::new(store, model, None).expect("context creation")
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

        let ctx =
            TuiContext::new(store, "test-model", Some(&session.id)).expect("context resume");

        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].content, "persisted");
        assert_eq!(ctx.session_id, session.id);
    }
}
