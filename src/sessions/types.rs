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
    /// The chat this session records for. `None` on every session written
    /// before v2 or by a path that does not key on a chat (TUI, gateway). Only
    /// rows the channel recording path writes carry a value, and only they are
    /// pruned by [`crate::sessions::SessionStore::prune_channel_sessions`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_key: Option<String>,
}

/// Minimal session info for listing
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub model: String,
    pub started_at: i64,
    pub message_count: i64,
    /// The session's `source` column. Set on every listing path now, since
    /// the source filter the channel recording pass uses reads it; the
    /// pre-existing lists at the CLI/TUI/API surfaces read it too even
    /// though they never display it.
    #[allow(dead_code)]
    pub source: String,
}

/// Session row with the conversation-channel metadata needed by the
/// console's `source=channel` listing.
///
/// Carries everything `SessionMeta` does plus the conversation key, the
/// surface/place/thread tuple, and the last-activity time. SessionMeta
/// stays small: the TUI/CLI listings do not want these fields and the row
/// sits next to a non-channel row in the same `serde_json::Value` response.
#[derive(Debug, Clone)]
pub struct ConversationSessionRow {
    pub id: String,
    pub title: Option<String>,
    pub model: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub message_count: i64,
    pub source: String,
    pub conversation_key: String,
    /// Epoch seconds of the most recent message in this session, or
    /// `started_at` when the session is brand new.
    pub last_activity_at: i64,
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
