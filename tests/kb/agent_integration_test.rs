//! Unit tests for the AXI ambient-context helper consumed by the
//! agent loop's system-prompt assembly path.
//!
//! These tests cover `kb_ambient_context()` in isolation. Booting the
//! full agent runtime just to assert that a string was concatenated is
//! gross overkill — the wiring in `src/agent/loop_.rs` is a 4-line
//! `if let Some(line) = …` block, gated `#[cfg(feature = "kb")]`, and
//! is grep-verifiable.
//!
//! Env-state caveat: `kb_ambient_context()` reads `KB_DB_PATH` via
//! `std::env::var`, so we serialize both tests behind the shared
//! `ENV_LOCK` mutex defined in `kb/common.rs`.

use std::fs::File;

use tempfile::TempDir;

use rantaiclaw::kb::axi::kb_ambient_context;

use super::common::ENV_LOCK;

/// When a KB database file exists at the path `KB_DB_PATH` resolves to,
/// the helper returns the ambient one-liner with the canonical command
/// surface so the agent can shell out.
#[test]
fn ambient_context_returned_when_db_exists() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("kb.db");
    File::create(&db_path).expect("create kb.db sentinel file");

    // Env mutation is serialized by ENV_LOCK above.
    std::env::set_var("KB_DB_PATH", &db_path);

    let result = kb_ambient_context();

    // Tear down before assertions so a panic doesn't leak env state.
    std::env::remove_var("KB_DB_PATH");

    let text = result.expect("ambient context should be Some when db file exists");
    assert!(
        text.contains("Knowledge base"),
        "missing capability tag, got: {text}"
    );
    assert!(
        text.contains("rantaiclaw kb search"),
        "missing canonical search command, got: {text}"
    );
}

/// When `KB_DB_PATH` points to a nonexistent file, the helper must
/// return `None` so the agent never learns about a KB it cannot reach.
#[test]
fn ambient_context_none_when_db_missing() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist.db");
    assert!(!missing.exists(), "precondition: path must be missing");

    // Env mutation is serialized by ENV_LOCK above.
    std::env::set_var("KB_DB_PATH", &missing);

    let result = kb_ambient_context();

    // Tear down before assertions.
    std::env::remove_var("KB_DB_PATH");

    assert!(
        result.is_none(),
        "ambient context must be None when db file is missing, got: {result:?}"
    );
}
