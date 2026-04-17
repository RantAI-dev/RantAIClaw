#![cfg(feature = "tui")]

use rantaiclaw::sessions::SessionStore;
use rantaiclaw::tui::TuiConfig;
use tempfile::tempdir;

#[test]
fn tui_config_has_sensible_defaults() {
    let config = TuiConfig::default();
    assert!(!config.model.is_empty());
    assert!(config.resume_session.is_none());
}

#[test]
fn session_store_persists_across_opens() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sessions.db");

    let session_id = {
        let store = SessionStore::open(&db_path).unwrap();
        let session = store.new_session("test-model", "tui").unwrap();
        session.id
    };

    {
        let store = SessionStore::open(&db_path).unwrap();
        let session = store.get_session(&session_id).unwrap();
        assert!(session.is_some());
        assert_eq!(session.unwrap().model, "test-model");
    }
}

#[test]
fn messages_persist_and_are_searchable() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sessions.db");

    let store = SessionStore::open(&db_path).unwrap();
    let session = store.new_session("test-model", "tui").unwrap();

    use rantaiclaw::sessions::Message;
    let msg1 = Message::user(&session.id, "Tell me about Rust programming");
    let msg2 = Message::assistant(&session.id, "Rust is a systems programming language.");
    store.append_message(&msg1).unwrap();
    store.append_message(&msg2).unwrap();

    let messages = store.get_messages(&session.id).unwrap();
    assert_eq!(messages.len(), 2);

    let results = store.search("Rust", 10).unwrap();
    assert!(!results.is_empty());
}

#[test]
fn session_splitting_preserves_parent_link() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sessions.db");

    let store = SessionStore::open(&db_path).unwrap();
    let session = store.new_session("test-model", "tui").unwrap();

    let new_session = store
        .split_session(&session.id, "Summary text", "test-model")
        .unwrap();

    assert_eq!(new_session.parent_session_id, Some(session.id.clone()));

    let old_session = store.get_session(&session.id).unwrap().unwrap();
    assert!(old_session.ended_at.is_some());
}
