//! Tests for the `SessionSearch` abstraction.
//!
//! These tests live next to the trait so the contract is exercised where it is
//! defined. `session_search` (`src/tools/session_search_tool.rs`) goes through
//! `Arc<dyn SessionSearch>`, so the in-memory `MutexSessionStore` plus the
//! path-backed `ProfileSessionSearch` are the two impls a test or the registry
//! will substitute.

use std::collections::HashMap;

use tempfile::TempDir;

use crate::sessions::search::{ProfileSessionSearch, SessionSearch};
use crate::sessions::{Message, SearchResult, SessionStore};

/// The probe session reads from a file-backed store, so `open` is what we are
/// really exercising; the seed has to come from the same `open` path so the
/// rows are visible to the probe.
fn seed(path: &std::path::Path, conv_key: &str, user_msg: &str) -> String {
    let mut store = SessionStore::open(path).unwrap();
    store
        .record_channel_turn("m", conv_key, user_msg, "r", None)
        .unwrap()
}

#[test]
fn profile_session_search_uses_the_given_path() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("sessions.db");
    seed(&path, "chat-x", "hello there");

    let probe = ProfileSessionSearch::new(path);
    let hits = probe.search_messages("hello", 10, None).unwrap();
    assert_eq!(hits.len(), 1, "FTS search must find the seeded row");
    let session_id = hits[0].session_id.clone();

    let rows = probe.get_messages(&session_id).unwrap();
    // record_channel_turn stores both the user row and the assistant row, so
    // the session has two messages — the seed plus the canned assistant text.
    assert_eq!(rows.len(), 2, "two-message seed must round-trip: {rows:?}");
    assert!(
        rows.iter().any(|m| m.content == "hello there"),
        "user row must be visible: {rows:?}"
    );
}

#[test]
fn profile_session_search_opens_fresh_per_call() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("sessions.db");
    seed(&path, "chat-y", "first second");
    // Second record uses a different conversation_key, so the probe can
    // disambiguate the session ids when reading back.
    seed(&path, "chat-z", "third fourth");

    let probe = ProfileSessionSearch::new(path);
    let first = probe
        .search_messages("first", 10, None)
        .unwrap()
        .into_iter()
        .next()
        .expect("first search must produce a hit")
        .session_id;
    let second = probe
        .search_messages("third", 10, None)
        .unwrap()
        .into_iter()
        .next()
        .expect("second search must produce a hit")
        .session_id;

    let first_rows = probe.get_messages(&first).unwrap();
    let second_rows = probe.get_messages(&second).unwrap();
    let first_session_ids: std::collections::HashSet<&str> =
        first_rows.iter().map(|m| m.session_id.as_str()).collect();
    let second_session_ids: std::collections::HashSet<&str> =
        second_rows.iter().map(|m| m.session_id.as_str()).collect();
    assert_ne!(
        first_session_ids, second_session_ids,
        "two sessions have distinct rows"
    );
    assert!(first_rows.iter().any(|m| m.content == "first second"));
    assert!(second_rows.iter().any(|m| m.content == "third fourth"));
}

#[test]
fn profile_session_search_propagates_open_failure() {
    // A path that cannot be created (parent is a file, not a directory) must
    // surface as an explicit error, not as an empty search result.
    let tmp = TempDir::new().unwrap();
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let bad = blocker.join("sessions.db");

    let probe = ProfileSessionSearch::new(bad);
    let err = probe
        .search_messages("anything", 10, None)
        .expect_err("open must fail when the parent is a file");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("sessions.db") || msg.to_lowercase().contains("directory"),
        "open failure must mention the failing path or reason: {msg}"
    );
}

#[test]
fn profile_session_search_filters_by_conversation_key_when_some() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("sessions.db");
    let other = seed(&path, "telegram:chat-1", "apple banana");
    let mine = seed(&path, "telegram:chat-2", "apple strawberry");

    let probe = ProfileSessionSearch::new(path);
    let hits = probe
        .search_messages("apple", 10, Some("telegram:chat-2"))
        .unwrap();
    assert_eq!(hits.len(), 1, "scoped call must skip chat-1: {hits:?}");
    assert_eq!(
        hits[0].session_id, mine,
        "scoped hit must come from chat-2 only"
    );
    assert_ne!(hits[0].session_id, other);
}

#[test]
fn mutex_session_store_delegates_get_messages() {
    let mut store = SessionStore::in_memory().unwrap();
    let session = store
        .record_channel_turn("m", "chat-a", "hi", "hello back", None)
        .unwrap();

    let probe = crate::sessions::MutexSessionStore::new(store);
    let rows = probe.get_messages(&session).unwrap();
    assert!(
        rows.iter().any(|m| m.content == "hi"),
        "recorded user row must be visible: got {rows:?}"
    );
}

// Silence unused-import warnings if the surrounding `HashMap` ends up unused
// after future edits to the tests.
#[allow(dead_code)]
fn _unused_imports_anchor(_h: HashMap<String, Vec<Message>>, _r: Vec<SearchResult>) {}
