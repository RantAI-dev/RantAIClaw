//! Database files written by the release before the rusqlite 0.40 move
//! (rusqlite 0.37, its bundled SQLite) must open and read unchanged with the
//! current build. The fixtures under `tests/fixtures/sqlite/` hold placeholder
//! data only; see the README next to them.

use std::path::{Path, PathBuf};

use rantaiclaw::memory::sqlite::SqliteMemory;
use rantaiclaw::memory::traits::{Memory, MemoryCategory};
use rantaiclaw::memory::SessionScope;
use rantaiclaw::sessions::SessionStore;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sqlite")
        .join(name)
}

#[test]
fn sessions_db_from_previous_release_reads_back_seeded_rows() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("sessions.db");
    std::fs::copy(fixture("sessions.sqlite"), &db).unwrap();

    let store = SessionStore::open(&db).unwrap();

    let metas = store.list_sessions(100).unwrap();
    assert_eq!(metas.len(), 3);
    assert_eq!(store.count_sessions().unwrap(), 3);

    let by_title = |title: &str| {
        metas
            .iter()
            .find(|m| m.title.as_deref() == Some(title))
            .unwrap_or_else(|| panic!("session titled {title} missing"))
    };

    let a = by_title("rantaiclaw_session_a");
    assert_eq!(a.model, "rantaiclaw_model_a");
    assert_eq!(a.source, "tui");
    assert_eq!(a.message_count, 2);
    let a_msgs = store.get_messages(&a.id).unwrap();
    let a_turns: Vec<(&str, &str)> = a_msgs
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_str()))
        .collect();
    assert_eq!(
        a_turns,
        vec![
            ("user", "user_a asks about quokka habitats"),
            (
                "assistant",
                "rantaiclaw_reply_alpha lists three quokka habitats"
            ),
        ]
    );

    let b = by_title("rantaiclaw_session_b");
    assert_eq!(b.source, "gateway");
    assert_eq!(b.message_count, 1);
    let b_msgs = store.get_messages(&b.id).unwrap();
    assert_eq!(b_msgs.len(), 1);
    assert_eq!(
        b_msgs[0].content,
        "user_a asks about lighthouse maintenance"
    );

    // The fork keeps its parent link and its single system note.
    let fork = metas
        .iter()
        .find(|m| m.title.is_none())
        .expect("untitled fork session missing");
    let fork_row = store.get_session(&fork.id).unwrap().unwrap();
    assert_eq!(fork_row.parent_session_id.as_deref(), Some(a.id.as_str()));
    let fork_msgs = store.get_messages(&fork.id).unwrap();
    assert_eq!(fork_msgs.len(), 1);
    assert_eq!(fork_msgs[0].role, "system");
    assert_eq!(fork_msgs[0].content, "forked from rantaiclaw_session_a");

    // The FTS index written by the old build answers queries.
    let hits = store.search("quokka", 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.session_id == a.id));
}

#[tokio::test]
async fn brain_db_from_previous_release_reads_back_seeded_entries() {
    let tmp = tempfile::TempDir::new().unwrap();
    let memory_dir = tmp.path().join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::copy(fixture("brain.sqlite"), memory_dir.join("brain.db")).unwrap();

    let mem = SqliteMemory::new(tmp.path()).unwrap();

    assert_eq!(mem.count(SessionScope::Any).await.unwrap(), 3);

    let theme = mem.get("rantaiclaw_pref_theme").await.unwrap().unwrap();
    assert_eq!(theme.content, "user_a prefers the dark theme");
    assert_eq!(theme.category, MemoryCategory::Core);
    assert_eq!(theme.session_id, None);

    let deploy = mem.get("rantaiclaw_fact_deploy").await.unwrap().unwrap();
    assert_eq!(deploy.content, "the rantaiclaw_service deploys on fridays");
    assert_eq!(deploy.category, MemoryCategory::Daily);

    let scoped = mem.get("rantaiclaw_note_scoped").await.unwrap().unwrap();
    assert_eq!(scoped.content, "scoped note about quokka feeding");
    assert_eq!(scoped.category, MemoryCategory::Conversation);
    assert_eq!(scoped.session_id.as_deref(), Some("rantaiclaw_session_a"));

    // The FTS table written by the old build answers keyword queries.
    let hits = mem
        .recall("dark theme", 5, SessionScope::Any)
        .await
        .unwrap();
    let keys: Vec<&str> = hits.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys, vec!["rantaiclaw_pref_theme"]);
}

#[cfg(feature = "kb")]
#[tokio::test]
async fn kb_db_from_previous_release_reads_back_seeded_chunks() {
    use rantaiclaw::kb::store::sqlite::SqliteStore;
    use rantaiclaw::kb::store::{KbStore, SearchFilter};
    use rantaiclaw::kb::DocumentId;

    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    std::fs::copy(fixture("kb.sqlite"), &db).unwrap();

    let store = SqliteStore::open(&db, 4).await.unwrap();

    let docs = store
        .list_documents(Some("rantaiclaw_org_a"))
        .await
        .unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].id.0, "rantaiclaw_doc_alpha");
    assert_eq!(docs[0].title, "Alpha Doc");
    assert_eq!(
        store
            .chunk_count(&DocumentId("rantaiclaw_doc_alpha".into()))
            .await
            .unwrap(),
        2
    );

    // The sqlite-vec table written by the old build answers vector queries.
    let by_vector = store
        .search_by_vector(&[1.0; 4], 5, &SearchFilter::default())
        .await
        .unwrap();
    assert_eq!(by_vector.len(), 2);
    assert_eq!(by_vector[0].content, "alpha content");

    // The FTS5 table written by the old build answers keyword queries.
    let by_keyword = store.bm25_search("alpha", 5).await.unwrap();
    let contents: Vec<&str> = by_keyword.iter().map(|h| h.content.as_str()).collect();
    assert_eq!(contents, vec!["alpha content"]);
}
