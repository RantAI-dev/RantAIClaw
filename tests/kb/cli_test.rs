//! Tests for the `rantaiclaw kb` axi-cli surface.
//!
//! Split in two halves:
//!
//! - **TOON formatter unit tests** — exercise `format_toon` directly with
//!   `serde_json::json!` fixtures. No process, no DB.
//! - **End-to-end CLI smoke tests** — spawn the built `rantaiclaw` binary
//!   against an isolated `KB_DB_PATH` tempdir to validate dispatch + output
//!   shapes. Tests that would require network (live OpenRouter) are gated
//!   with `#[ignore]`.
//!
//! `KB_DB_PATH` is honored by `KbCommand::run` so each test gets its own
//! database. The binary path comes from `env!("CARGO_BIN_EXE_rantaiclaw")` —
//! `cargo test` always builds the binary before running an integration test
//! that references it, so there's no need to invoke `cargo build` here.

use serde_json::json;

use rantaiclaw::kb::axi::toon::format_toon;

// ---------------------------------------------------------------------------
// TOON formatter unit tests
// ---------------------------------------------------------------------------

#[test]
fn toon_format_empty_list() {
    let items: Vec<serde_json::Value> = Vec::new();
    let out = format_toon("items", &items, &["id", "title"]);
    // Header-only output. No rows; trailing newline closes the header line.
    assert_eq!(out, "items[0]{id,title}:\n");
}

#[test]
fn toon_format_single_item() {
    let items = vec![json!({"id": "doc-1", "title": "FAQ"})];
    let out = format_toon("items", &items, &["id", "title"]);
    assert_eq!(out, "items[1]{id,title}:\n  doc-1,FAQ\n");
}

#[test]
fn toon_format_quotes_strings_with_commas() {
    let items = vec![json!({"id": "doc-1", "title": "hello, world"})];
    let out = format_toon("items", &items, &["id", "title"]);
    // Comma inside the cell → quote the cell.
    assert!(
        out.contains("\"hello, world\""),
        "expected quoted comma cell, got: {out}"
    );
}

#[test]
fn toon_format_newlines_in_strings_replaced_with_space() {
    let items = vec![json!({"id": "doc-1", "content": "line1\nline2"})];
    let out = format_toon("items", &items, &["id", "content"]);
    assert!(
        out.contains("line1 line2"),
        "newline must collapse to space, got: {out}"
    );
    // Output must remain single-row (one row + the header line).
    assert_eq!(
        out.lines().count(),
        2,
        "embedded newline must not split into two rows: {out}"
    );
}

#[test]
fn toon_format_numeric_field_unquoted() {
    let items = vec![json!({"id": "doc-1", "score": 0.91})];
    let out = format_toon("items", &items, &["id", "score"]);
    assert_eq!(out, "items[1]{id,score}:\n  doc-1,0.91\n");
}

#[test]
fn toon_format_null_field_empty() {
    let items = vec![json!({"id": "doc-1", "subcategory": null})];
    let out = format_toon("items", &items, &["id", "subcategory"]);
    // Null → empty cell. `doc-1,\n` after the row indent.
    assert_eq!(out, "items[1]{id,subcategory}:\n  doc-1,\n");
}

#[test]
fn toon_format_missing_field_treated_as_null() {
    // Defensive: if a row simply doesn't carry a requested field, the cell
    // must render as empty (matching null semantics) rather than panicking.
    let items = vec![json!({"id": "doc-1"})];
    let out = format_toon("items", &items, &["id", "missing"]);
    assert_eq!(out, "items[1]{id,missing}:\n  doc-1,\n");
}

#[test]
fn toon_format_bool_renders_verbatim() {
    let items = vec![json!({"name": "drift", "in_sync": true})];
    let out = format_toon("status", &items, &["name", "in_sync"]);
    assert_eq!(out, "status[1]{name,in_sync}:\n  drift,true\n");
}

#[test]
fn toon_format_escapes_embedded_quotes() {
    // A cell containing both a comma and a quote must escape the quote
    // (`"` → `\"`) and wrap the whole thing in `"..."`.
    let items = vec![json!({"id": "doc-1", "title": "she said, \"hi\""})];
    let out = format_toon("items", &items, &["id", "title"]);
    assert!(
        out.contains("\"she said, \\\"hi\\\"\""),
        "expected escaped quote inside quoted cell, got: {out}"
    );
}

// ---------------------------------------------------------------------------
// End-to-end CLI tests against the built `rantaiclaw` binary.
//
// Strategy:
// 1. Each test owns its own `TempDir` and sets `KB_DB_PATH` to a fresh
//    SQLite path inside it.
// 2. Where the test needs pre-existing documents, we seed the store
//    in-process via the library API (no network, no embedder) BEFORE
//    invoking the binary. The binary then opens the SAME file.
// 3. Subcommands that don't need a live embedder (`list`, `get`, `delete`,
//    `drift`) drive the dispatcher end-to-end without `OPENROUTER_API_KEY`.
//
// Subcommands that DO need a live embedder (`search` against real chunks,
// `ingest`, `re-embed`) are gated behind `#[ignore]` so CI doesn't burn
// API credits.
// ---------------------------------------------------------------------------

use std::path::PathBuf;
use std::process::Command;

use chrono::Utc;
use tempfile::TempDir;

use rantaiclaw::kb::store::sqlite::SqliteStore;
use rantaiclaw::kb::store::KbStore;
use rantaiclaw::kb::{Document, DocumentId};

/// Path to the built `rantaiclaw` binary. Cargo populates this env var for
/// integration tests and rebuilds the binary on demand.
fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rantaiclaw"))
}

/// Build a minimal document for seeding. Embedding dim is fixed at 4 to
/// match `SqliteStore::open(&path, 4)` calls in these tests — the binary
/// reads `KB_EMBEDDING_DIM` from env, so we set it to "4" in each test that
/// invokes the binary against a seeded store.
fn sample_doc(id: &str, title: &str) -> Document {
    Document {
        id: DocumentId(id.into()),
        title: title.into(),
        content: format!("body of {title}"),
        categories: vec!["FAQ".into()],
        subcategory: None,
        metadata: serde_json::json!({}),
        s3_key: None,
        file_type: None,
        mime_type: None,
        file_size: None,
        organization_id: Some("rantaiclaw_org_a".into()),
        created_by: None,
        session_id: None,
        artifact_type: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        deleted_at: None,
        retention_days: None,
        retrieval_count: 0,
        last_retrieved_at: None,
    }
}

/// Spawn `rantaiclaw kb <args>` against a tempdir-scoped DB. Returns
/// (status_code, stdout, stderr). `KB_EMBEDDING_DIM=4` matches the
/// in-process seed dim so the store reopens cleanly.
///
/// `RANTAICLAW_LOG_STDERR=1` forces tracing logs onto stderr — without it
/// the global subscriber writes INFO lines to stdout, which would corrupt
/// the TOON/JSON output the test asserts on. Each test also sets a fresh
/// `HOME` to keep the binary's profile bootstrap out of the developer's
/// real `~/.rantaiclaw/`.
fn run_kb(db_path: &PathBuf, args: &[&str]) -> (i32, String, String) {
    // Use the same parent as the DB for a profile dir so the binary's
    // "first-run profile bootstrap" doesn't touch the developer's HOME.
    let fake_home = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    let output = Command::new(binary_path())
        .arg("kb")
        .args(args)
        .env("KB_DB_PATH", db_path)
        .env("KB_EMBEDDING_DIM", "4")
        // Belt-and-braces: clear any inherited OPENROUTER_API_KEY so a CI
        // machine with credentials in env doesn't surprise the test by
        // actually hitting the network. None of these tests need it.
        .env("OPENROUTER_API_KEY", "")
        .env("KB_EMBEDDING_API_KEY", "")
        // Send tracing logs to stderr — keeps stdout pure for parsing.
        .env("RANTAICLAW_LOG_STDERR", "1")
        // Silence the verbose-by-default INFO config-loaded line so
        // smoke tests stay fast and unambiguous. WARN+ still surfaces.
        .env("RUST_LOG", "warn")
        // Sandbox the binary's profile directory creation to the temp dir.
        .env("HOME", &fake_home)
        .output()
        .expect("failed to spawn rantaiclaw binary");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (code, stdout, stderr)
}

#[tokio::test]
async fn kb_list_empty_returns_empty_documents_toon() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");

    // Touch the file via in-process open so the binary doesn't have to
    // perform first-time schema migration with a different embedding_dim.
    let _store = SqliteStore::open(&db, 4).await.unwrap();

    let (code, stdout, stderr) = run_kb(&db, &["list"]);
    assert_eq!(code, 0, "list on empty KB must exit 0; stderr={stderr}");
    assert!(
        stdout.starts_with("documents[0]{"),
        "expected empty documents TOON header, got: {stdout}"
    );
}

#[tokio::test]
async fn kb_list_after_seed_returns_rows() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    store
        .create_document(&sample_doc("rantaiclaw_doc_a", "First Doc"))
        .await
        .unwrap();
    store
        .create_document(&sample_doc("rantaiclaw_doc_b", "Second Doc"))
        .await
        .unwrap();
    // Drop the in-process handle so the binary can open its own.
    drop(store);

    let (code, stdout, _stderr) = run_kb(&db, &["list"]);
    assert_eq!(code, 0);
    assert!(
        stdout.starts_with("documents[2]{"),
        "expected 2 documents in header, got: {stdout}"
    );
    assert!(stdout.contains("First Doc"));
    assert!(stdout.contains("Second Doc"));
}

#[tokio::test]
async fn kb_get_nonexistent_returns_error_toon_and_exit_1() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let _store = SqliteStore::open(&db, 4).await.unwrap();

    let (code, stdout, _stderr) = run_kb(&db, &["get", "rantaiclaw_missing"]);
    assert_eq!(code, 1, "missing doc must exit 1");
    assert!(
        stdout.starts_with("error[1]{code,message}"),
        "expected TOON error header, got: {stdout}"
    );
    assert!(stdout.contains("not_found"));
    assert!(stdout.contains("rantaiclaw_missing"));
}

#[tokio::test]
async fn kb_get_existing_returns_document_toon() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    store
        .create_document(&sample_doc("rantaiclaw_doc_get", "Gettable Doc"))
        .await
        .unwrap();
    drop(store);

    let (code, stdout, _stderr) = run_kb(&db, &["get", "rantaiclaw_doc_get"]);
    assert_eq!(code, 0);
    assert!(
        stdout.starts_with("document[1]{"),
        "expected single document TOON header, got: {stdout}"
    );
    assert!(stdout.contains("rantaiclaw_doc_get"));
    assert!(stdout.contains("Gettable Doc"));
}

#[tokio::test]
async fn kb_delete_soft_hides_doc_from_list() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    store
        .create_document(&sample_doc("rantaiclaw_doc_del", "Deletable"))
        .await
        .unwrap();
    drop(store);

    let (del_code, del_stdout, _) = run_kb(&db, &["delete", "rantaiclaw_doc_del"]);
    assert_eq!(del_code, 0, "delete must succeed");
    assert!(
        del_stdout.starts_with("result[1]{id,mode}"),
        "expected delete result TOON, got: {del_stdout}"
    );
    assert!(del_stdout.contains("soft"));

    let (list_code, list_stdout, _) = run_kb(&db, &["list"]);
    assert_eq!(list_code, 0);
    assert!(
        list_stdout.starts_with("documents[0]{"),
        "soft-deleted doc must be hidden, got: {list_stdout}"
    );
}

#[tokio::test]
async fn kb_drift_in_sync_when_no_chunks() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let _store = SqliteStore::open(&db, 4).await.unwrap();

    let (code, stdout, stderr) = run_kb(&db, &["drift"]);
    assert_eq!(
        code, 0,
        "drift on empty store must succeed; stderr={stderr}"
    );
    assert!(
        stdout.starts_with("drift[1]{"),
        "expected drift TOON header, got: {stdout}"
    );
    // Empty store → no rows in `count_by_embedding_model` → in_sync=true.
    assert!(
        stdout.contains("true"),
        "in_sync must be true on empty store: {stdout}"
    );
}

#[tokio::test]
async fn kb_drift_json_outputs_valid_json() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let _store = SqliteStore::open(&db, 4).await.unwrap();

    let (code, stdout, _) = run_kb(&db, &["drift", "--json"]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("drift --json must be valid JSON");
    assert!(parsed.get("current_model").is_some());
    assert!(parsed.get("in_sync").is_some());
}

#[tokio::test]
async fn kb_list_json_outputs_valid_json_array() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    store
        .create_document(&sample_doc("rantaiclaw_doc_json", "JSON Doc"))
        .await
        .unwrap();
    drop(store);

    let (code, stdout, _) = run_kb(&db, &["list", "--json"]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("list --json must be valid JSON");
    let arr = parsed.as_array().expect("list --json must return an array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "JSON Doc");
}

// --- ignored tests requiring network / live embedder ----------------------

/// Smoke test for the ingest+search round-trip. Requires `OPENROUTER_API_KEY`
/// and reaches a real embeddings endpoint, so it's `#[ignore]` by default.
#[tokio::test]
#[ignore = "requires OPENROUTER_API_KEY + network for live embedder"]
async fn kb_ingest_then_search_returns_toon() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let md = tmp.path().join("doc.md");
    std::fs::write(&md, "# Health Insurance\nCoverage is up to $100k.").unwrap();

    // Note: the ingest run uses the platform-default `KB_EMBEDDING_DIM`
    // (4096 for qwen3-embedding-8b). Unlike the offline tests above we do
    // NOT override the dim — the live embedder dictates the schema.
    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_default();
    let ingest = Command::new(binary_path())
        .arg("kb")
        .args(["ingest", md.to_str().unwrap()])
        .env("KB_DB_PATH", &db)
        .env("OPENROUTER_API_KEY", &api_key)
        .output()
        .unwrap();
    assert!(ingest.status.success(), "ingest must succeed");

    let search = Command::new(binary_path())
        .arg("kb")
        .args(["search", "what is the coverage?", "--top", "3"])
        .env("KB_DB_PATH", &db)
        .env("OPENROUTER_API_KEY", &api_key)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&search.stdout);
    assert!(
        stdout.starts_with("chunks["),
        "expected chunks TOON: {stdout}"
    );
    assert!(stdout.contains("Health Insurance"));
}
