//! The session-search abstraction the `session_search` tool depends on.
//!
//! `session_search` (`src/tools/session_search_tool.rs`) reads through this
//! trait, not the concrete store, so a test can substitute a probe and assert
//! which scope the tool actually read under. In production the registry wires
//! up [`ProfileSessionSearch`], which points at the active profile's
//! `sessions.db`; tests can substitute a `ProbeStore` or the in-memory
//! [`MutexSessionStore`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;

use super::store::SessionStore;
use super::types::{Message, SearchResult};

/// Read-side abstraction the tool needs from the session store.
///
/// `search_messages` is the FTS lookup. `conversation_key = Some(k)` filters
/// by `sessions.conversation_key` so a scoped caller never reads another
/// conversation's rows; `None` is the unscoped read used under the `All` view.
/// `search_messages_any_word` runs the same FTS lookup but joins whitespace
/// tokens with `OR` instead of the implicit `AND` — the fallback path the
/// tool uses when the all-words pass returns nothing. `get_messages`
/// returns every message for `session_id` in replay order, so the tool can
/// include the message before and after each hit.
pub trait SessionSearch: Send + Sync {
    fn search_messages(
        &self,
        query: &str,
        limit: usize,
        conversation_key: Option<&str>,
    ) -> Result<Vec<SearchResult>>;

    fn search_messages_any_word(
        &self,
        query: &str,
        limit: usize,
        conversation_key: Option<&str>,
    ) -> Result<Vec<SearchResult>>;

    fn get_messages(&self, session_id: &str) -> Result<Vec<Message>>;
}

/// `Send + Sync` wrapper over [`SessionStore`] for the tool's
/// `Arc<dyn SessionSearch>` slot. The store's `Connection` carries interior
/// mutability that is `!Sync`, so the production handle keeps the lock on this
/// side rather than at the call site. This is the handle a test or a process
/// that owns the store already uses.
pub struct MutexSessionStore {
    inner: Mutex<SessionStore>,
}

impl MutexSessionStore {
    pub fn new(store: SessionStore) -> Self {
        Self {
            inner: Mutex::new(store),
        }
    }
}

impl SessionSearch for MutexSessionStore {
    fn search_messages(
        &self,
        query: &str,
        limit: usize,
        conversation_key: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let guard = self.inner.lock().expect("session store poisoned");
        guard.search_with_conversation(query, limit, conversation_key)
    }

    fn search_messages_any_word(
        &self,
        query: &str,
        limit: usize,
        conversation_key: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let guard = self.inner.lock().expect("session store poisoned");
        guard.search_any_word_with_conversation(query, limit, conversation_key)
    }

    fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let guard = self.inner.lock().expect("session store poisoned");
        SessionStore::get_messages(&guard, session_id)
    }
}

/// `SessionSearch` over a `sessions.db` path. The constructor only stores the
/// path; each call opens a fresh `SessionStore`. The point is to keep the
/// registry honest — `SessionStore::open` returns an error if the file is
/// missing, corrupt, or the parent directory cannot be created, and that
/// error reaches the tool's `ToolResult` instead of being swallowed.
pub struct ProfileSessionSearch {
    path: PathBuf,
}

impl ProfileSessionSearch {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SessionSearch for ProfileSessionSearch {
    fn search_messages(
        &self,
        query: &str,
        limit: usize,
        conversation_key: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let store = SessionStore::open(&self.path)?;
        store.search_with_conversation(query, limit, conversation_key)
    }

    fn search_messages_any_word(
        &self,
        query: &str,
        limit: usize,
        conversation_key: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let store = SessionStore::open(&self.path)?;
        store.search_any_word_with_conversation(query, limit, conversation_key)
    }

    fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let store = SessionStore::open(&self.path)?;
        store.get_messages(session_id)
    }
}
