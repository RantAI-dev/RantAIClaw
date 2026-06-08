use serde::{Deserialize, Serialize};

/// A conversation session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub parent_session_id: Option<String>,
    pub model: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub message_count: i64,
    pub token_count: i64,
    pub source: String,
}

/// Minimal session info for listing
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub model: String,
    pub started_at: i64,
    pub message_count: i64,
}

/// A message within a session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tool_calls: Option<String>,
    pub timestamp: i64,
}

impl Message {
    pub fn user(session_id: &str, content: &str) -> Self {
        Self {
            id: 0,
            session_id: session_id.to_string(),
            role: "user".to_string(),
            content: content.to_string(),
            tool_calls: None,
            timestamp: chrono::Utc::now().timestamp(),
        }
    }

    pub fn assistant(session_id: &str, content: &str) -> Self {
        Self {
            id: 0,
            session_id: session_id.to_string(),
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_calls: None,
            timestamp: chrono::Utc::now().timestamp(),
        }
    }
}

/// Stored session messages as `(role, content)` turns for seeding agent
/// history on resume / continuation. Empty content is skipped; tool-call
/// metadata is not replayed (the stored assistant text already reflects
/// the outcome). Primitive tuples keep this usable across the lib/bin
/// boundary (no `ConversationMessage` type identity mismatch).
pub fn messages_to_turns(messages: &[Message]) -> Vec<(String, String)> {
    messages
        .iter()
        .filter(|m| !m.content.trim().is_empty())
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect()
}

/// Search result from FTS5
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub session_id: String,
    pub session_title: Option<String>,
    pub message_id: i64,
    pub role: String,
    pub content: String,
    pub timestamp: i64,
    pub rank: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_user_creates_user_message() {
        let msg = Message::user("sess-1", "hello");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.session_id, "sess-1");
    }

    #[test]
    fn message_assistant_creates_assistant_message() {
        let msg = Message::assistant("sess-1", "hi there");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "hi there");
    }
}
