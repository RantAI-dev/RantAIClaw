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
/// Plan 107: the KB data subcommands gate on `[knowledge].enabled`, and a
/// fresh sandbox HOME yields a default (disabled) config. Each spawn writes
/// an activated config into the sandbox so the e2e tests exercise the data
/// paths, mirroring the api_test harness setting `enabled = true`.
fn write_enabled_config(fake_home: &std::path::Path) {
    let dir = fake_home.join(".rantaiclaw");
    std::fs::create_dir_all(&dir).expect("create sandbox config dir");
    // The three root fields are the schema's only required ones (same
    // minimal shape as tests/fixtures/legacy_layout).
    std::fs::write(
        dir.join("config.toml"),
        "schema_version = 18\n\
         default_provider = \"openrouter\"\n\
         default_model = \"rantaiclaw_test_model\"\n\
         default_temperature = 0.7\n\
         [knowledge]\nenabled = true\n",
    )
    .expect("write sandbox config");
}

fn run_kb(db_path: &PathBuf, args: &[&str]) -> (i32, String, String) {
    // Use the same parent as the DB for a profile dir so the binary's
    // "first-run profile bootstrap" doesn't touch the developer's HOME.
    let fake_home = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    write_enabled_config(&fake_home);

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
        // An override inherited from the parent shell would shadow the
        // default resolution under the temp HOME.
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_WORKSPACE")
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
async fn kb_delete_unknown_id_returns_error_toon_and_exit_1() {
    // Regression: `kb delete <unknown>` printed a success row and exited 0.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let _store = SqliteStore::open(&db, 4).await.unwrap();

    let (code, stdout, stderr) = run_kb(&db, &["delete", "rantaiclaw_missing"]);
    assert_eq!(code, 1, "unknown id must exit 1; stderr={stderr}");
    assert!(
        stdout.starts_with("error[1]{code,message}"),
        "expected TOON error header, got: {stdout}"
    );
    assert!(stdout.contains("not_found"));
    assert!(stdout.contains("rantaiclaw_missing"));
    assert!(
        !stdout.contains("result["),
        "no success row may be printed: {stdout}"
    );
}

#[tokio::test]
async fn kb_delete_already_soft_deleted_id_returns_error_toon_and_exit_1() {
    // A soft-deleted document is "not found" for GET, so the same rule applies
    // to a second delete: it must not report a fresh success.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    store
        .create_document(&sample_doc("rantaiclaw_doc_twice", "Twice"))
        .await
        .unwrap();
    store
        .delete_document(
            &rantaiclaw::kb::DocumentId("rantaiclaw_doc_twice".into()),
            true,
        )
        .await
        .unwrap();
    drop(store);

    let (code, stdout, _stderr) = run_kb(&db, &["delete", "rantaiclaw_doc_twice"]);
    assert_eq!(code, 1, "second delete must exit 1");
    assert!(
        stdout.starts_with("error[1]{code,message}"),
        "got: {stdout}"
    );
    assert!(stdout.contains("not_found"));
}

#[tokio::test]
async fn kb_delete_hard_removes_a_soft_deleted_document() {
    // `--hard` must reach a document that was soft-deleted first: the store
    // had filtered it out, so the CLI answered not_found and kept the row.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    store
        .create_document(&sample_doc("rantaiclaw_doc_hardgone", "HardGone"))
        .await
        .unwrap();
    store
        .delete_document(
            &rantaiclaw::kb::DocumentId("rantaiclaw_doc_hardgone".into()),
            true,
        )
        .await
        .unwrap();
    drop(store);

    let (soft_code, soft_stdout, _) = run_kb(&db, &["delete", "rantaiclaw_doc_hardgone"]);
    assert_eq!(soft_code, 1, "soft delete of a soft-deleted id must exit 1");
    assert!(soft_stdout.contains("not_found"));

    let (code, stdout, stderr) = run_kb(&db, &["delete", "--hard", "rantaiclaw_doc_hardgone"]);
    assert_eq!(code, 0, "hard delete must succeed; stderr={stderr}");
    assert!(
        stdout.starts_with("result[1]{id,mode}"),
        "expected delete result TOON, got: {stdout}"
    );
    assert!(stdout.contains("hard"), "got: {stdout}");
}

#[tokio::test]
async fn kb_intelligence_unknown_id_returns_error_toon_and_exit_1() {
    // Regression: `kb intelligence <unknown>` printed empty entities/relations
    // and exited 0, where HTTP answers 404.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let _store = SqliteStore::open(&db, 4).await.unwrap();

    let (code, stdout, stderr) = run_kb(&db, &["intelligence", "rantaiclaw_missing"]);
    assert_eq!(code, 1, "unknown id must exit 1; stderr={stderr}");
    assert!(
        stdout.starts_with("error[1]{code,message}"),
        "expected TOON error header, got: {stdout}"
    );
    assert!(stdout.contains("not_found"));
    assert!(stdout.contains("rantaiclaw_missing"));
    assert!(
        !stdout.contains("entities["),
        "no empty entity table may be printed: {stdout}"
    );
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
    // Since plan 088 the context block precedes the TOON table; the table is
    // still present and still parseable.
    assert!(stdout.contains("chunks["), "expected chunks TOON: {stdout}");
    assert!(stdout.contains("Health Insurance"));
}

/// Like [`run_kb`] but with extra env vars — used by the offline-embedder
/// tests below to point `KB_EMBEDDING_BASE_URL` at a local wiremock server.
fn run_kb_with_env(
    db_path: &PathBuf,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> (i32, String, String) {
    let fake_home = db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    write_enabled_config(&fake_home);
    let mut cmd = Command::new(binary_path());
    cmd.arg("kb")
        .args(args)
        .env("KB_DB_PATH", db_path)
        .env("KB_EMBEDDING_DIM", "4")
        .env("OPENROUTER_API_KEY", "")
        .env("KB_EMBEDDING_API_KEY", "")
        .env("RANTAICLAW_LOG_STDERR", "1")
        .env("RUST_LOG", "warn")
        .env("HOME", &fake_home)
        // An override inherited from the parent shell would shadow the
        // default resolution under the temp HOME.
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_WORKSPACE");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to spawn rantaiclaw binary");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (code, stdout, stderr)
}

/// Plan 088: the non-JSON search output must carry the retrieval `context`
/// (full chunk text under `[Title]` headers + the document inventory), not
/// only the 600-char TOON preview table. Before the fix the agent's ONLY KB
/// path returned 120-char previews and discarded `context` entirely.
///
/// Offline: a wiremock server plays the embedding endpoint (TEI-shaped —
/// the base URL contains no "openrouter.ai", so the binary omits auth).
#[tokio::test]
async fn kb_search_prints_context_block_before_toon_table() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // One vector per input item, dim 4. Ingest sends a 1-chunk batch and the
    // search sends a single query — both get one identical unit vector, so
    // the stored chunk always matches the query with similarity 1.0.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [1.0, 0.0, 0.0, 0.0] } ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let md = tmp.path().join("doc.md");
    std::fs::write(
        &md,
        "# Health Insurance\nCoverage is up to one hundred thousand dollars per year.",
    )
    .unwrap();
    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];

    let (code, stdout, stderr) = run_kb_with_env(&db, &["ingest", md.to_str().unwrap()], &mock_env);
    assert_eq!(
        code, 0,
        "ingest must succeed: stdout={stdout} stderr={stderr}"
    );

    let (code, stdout, _stderr) = run_kb_with_env(
        &db,
        &["search", "what is the coverage?", "--top", "3"],
        &mock_env,
    );
    assert_eq!(code, 0, "search must succeed: {stdout}");
    // Plan 089: the context arrives wrapped in the RAG instruction block
    // (cite inline, refuse cleanly) with the source list appended.
    assert!(
        stdout.contains("## Knowledge Base Context"),
        "RAG instruction block missing from search output: {stdout}"
    );
    assert!(
        stdout.contains("Sources:"),
        "source list missing from search output: {stdout}"
    );
    // Context block: inventory + [title] header + FULL chunk text.
    assert!(
        stdout.contains("## Documents in this knowledge base"),
        "document inventory missing from search output: {stdout}"
    );
    assert!(
        stdout.contains("Coverage is up to one hundred thousand dollars"),
        "full chunk text (not just a preview) must reach the agent: {stdout}"
    );
    // The TOON table still follows, and after the context block — the table
    // is the index, the context is the content.
    let ctx_pos = stdout.find("## Documents").unwrap();
    let toon_pos = stdout.find("chunks[").expect("TOON table present");
    assert!(
        ctx_pos < toon_pos,
        "context must precede the TOON table: {stdout}"
    );
}

/// Plan 088: a search over an empty scope must say so instead of printing a
/// bare empty table the agent reads as "the KB is empty" ambiguity.
#[tokio::test]
async fn kb_search_empty_scope_says_so() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [1.0, 0.0, 0.0, 0.0] } ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];

    let (code, stdout, _stderr) =
        run_kb_with_env(&db, &["search", "anything at all", "--top", "3"], &mock_env);
    assert_eq!(code, 0, "empty-scope search must still exit 0: {stdout}");
    assert!(
        stdout.contains("No documents found in this knowledge-base scope."),
        "empty scope must be stated explicitly: {stdout}"
    );
}

/// Plan 090: ingest must embed the metadata-PREFIXED text, not the raw chunk
/// body. The category below is a distinctive token that appears nowhere in
/// the document body — it can only reach the embedding request through
/// prepare_chunk_for_embedding. Inspects the wiremock request bodies.
#[tokio::test]
async fn kb_ingest_embeds_metadata_prefixed_text() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [1.0, 0.0, 0.0, 0.0] } ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let md = tmp.path().join("doc.md");
    std::fs::write(&md, "# Plain Title\nBody text with nothing special.").unwrap();
    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];

    let (code, stdout, stderr) = run_kb_with_env(
        &db,
        &[
            "ingest",
            md.to_str().unwrap(),
            "--category",
            "XyzzyDistinctiveCategory",
        ],
        &mock_env,
    );
    assert_eq!(
        code, 0,
        "ingest must succeed: stdout={stdout} stderr={stderr}"
    );

    let requests = server.received_requests().await.unwrap();
    assert!(!requests.is_empty(), "embedder must have been called");
    let bodies: Vec<String> = requests
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    assert!(
        bodies
            .iter()
            .any(|b| b.contains("Category: XyzzyDistinctiveCategory")),
        "embedding request must carry the metadata prefix, got bodies: {bodies:?}"
    );

    // And the stored model tag carries the recipe marker so drift can see a
    // pre-metadata corpus (plan 090 step 3 verify).
    let conn = rusqlite::Connection::open(&db).unwrap();
    let model: String = conn
        .query_row("SELECT DISTINCT embedding_model FROM chunk", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(
        model.ends_with("+meta1"),
        "stored embedding_model must carry the recipe tag, got: {model}"
    );
}

/// Plan 091: with contextual retrieval enabled, the LLM-generated prefix
/// must reach the embedding input. The prefix token appears nowhere in the
/// document — only the stubbed chat endpoint emits it, so finding it in the
/// embedding request proves the whole chain: chat call -> metadata ->
/// prepare_chunk_for_embedding -> embedder.
#[tokio::test]
async fn kb_ingest_contextual_prefix_reaches_the_embedding() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // Chat endpoint: one situating sentence for the single chunk.
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [ { "message": { "content": "[\"ZorbluqContext situates this chunk\"]" } } ]
        })))
        .mount(&server)
        .await;
    // Embedding endpoint on a distinct path.
    Mock::given(method("POST"))
        .and(path("/embed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [1.0, 0.0, 0.0, 0.0] } ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let md = tmp.path().join("doc.md");
    std::fs::write(&md, "# Plain Title\nBody text with nothing special.").unwrap();
    let uri = server.uri();
    let embed_url = format!("{uri}/embed");
    let chat_url = format!("{uri}/chat");
    let mock_env = [
        ("KB_EMBEDDING_BASE_URL", embed_url.as_str()),
        ("KB_OPENROUTER_CHAT_URL", chat_url.as_str()),
        ("KB_CONTEXTUAL_RETRIEVAL_ENABLED", "true"),
        // contextual.rs reads the key from env directly (plan 108 caveat).
        ("OPENROUTER_API_KEY", "rantaiclaw_test_key"),
    ];

    let (code, stdout, stderr) = run_kb_with_env(&db, &["ingest", md.to_str().unwrap()], &mock_env);
    assert_eq!(
        code, 0,
        "ingest must succeed: stdout={stdout} stderr={stderr}"
    );

    let requests = server.received_requests().await.unwrap();
    let embed_bodies: Vec<String> = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/embed"))
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    assert!(!embed_bodies.is_empty(), "embedder must have been called");
    assert!(
        embed_bodies
            .iter()
            .any(|b| b.contains("Context: ZorbluqContext situates this chunk")),
        "the contextual prefix must reach the embedding input, got: {embed_bodies:?}"
    );

    // And it must be persisted on the chunk row (indexing-time enrichment).
    let conn = rusqlite::Connection::open(&db).unwrap();
    let stored: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunk WHERE metadata_json LIKE '%ZorbluqContext%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        stored >= 1,
        "contextual prefix must be stored in chunk metadata"
    );
}

/// Plan 107: with the KB off (default fresh config), data subcommands must
/// answer a parseable kb_disabled TOON error with exit 1 — not an empty
/// result the agent misreads as "no documents".
#[test]
fn kb_data_subcommands_gate_on_disabled() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    // Deliberately NO write_enabled_config: the sandbox default is off.
    let fake_home = tmp.path().to_path_buf();
    let output = Command::new(binary_path())
        .arg("kb")
        .args(["list"])
        .env("KB_DB_PATH", &db)
        .env("KB_EMBEDDING_DIM", "4")
        .env("OPENROUTER_API_KEY", "")
        .env("KB_EMBEDDING_API_KEY", "")
        .env("RANTAICLAW_LOG_STDERR", "1")
        .env("RUST_LOG", "warn")
        .env("HOME", &fake_home)
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_WORKSPACE")
        .output()
        .expect("spawn binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "disabled KB must exit 1");
    assert!(
        stdout.contains("kb_disabled"),
        "must answer the parseable kb_disabled error, got: {stdout}"
    );
    assert!(
        stdout.contains("kb enable"),
        "must name the way out, got: {stdout}"
    );
}

/// Plan 107: `kb status` works while disabled and reports the state.
#[test]
fn kb_status_reports_disabled_state() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let fake_home = tmp.path().to_path_buf();
    let output = Command::new(binary_path())
        .arg("kb")
        .args(["status"])
        .env("KB_DB_PATH", &db)
        .env("KB_EMBEDDING_DIM", "4")
        .env("OPENROUTER_API_KEY", "")
        .env("KB_EMBEDDING_API_KEY", "")
        .env("RANTAICLAW_LOG_STDERR", "1")
        .env("RUST_LOG", "warn")
        .env("HOME", &fake_home)
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_WORKSPACE")
        .output()
        .expect("spawn binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "status must work while off: {stdout}"
    );
    assert!(stdout.starts_with("status["), "TOON status block: {stdout}");
    assert!(stdout.contains("false"), "reports disabled: {stdout}");
}

/// Spawn `rantaiclaw kb <args>` in a sandbox HOME that holds NO config, so the
/// knowledge base starts off. Unlike [`run_kb`], nothing activates it and the
/// two credential variables start empty; `extra_env` sets them per call.
fn run_kb_in_fresh_home(
    home: &std::path::Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> (i32, String, String) {
    let mut cmd = Command::new(binary_path());
    cmd.arg("kb")
        .args(args)
        .env("KB_DB_PATH", home.join("kb.db"))
        .env("KB_EMBEDDING_DIM", "4")
        .env("OPENROUTER_API_KEY", "")
        .env("KB_EMBEDDING_API_KEY", "")
        .env("RANTAICLAW_LOG_STDERR", "1")
        .env("RUST_LOG", "warn")
        .env("HOME", home)
        .env_remove("RANTAICLAW_CONFIG_DIR")
        .env_remove("RANTAICLAW_WORKSPACE");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to spawn rantaiclaw binary");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// What a LATER process sees: `kb status --json` with no credential in its
/// environment, so only the persisted config can say the knowledge base is on.
fn persisted_enabled(home: &std::path::Path) -> bool {
    let (code, stdout, stderr) = run_kb_in_fresh_home(home, &["status", "--json"], &[]);
    assert_eq!(code, 0, "status must work: {stdout} {stderr}");
    let status: serde_json::Value = serde_json::from_str(&stdout).expect("status json");
    status["enabled"].as_bool().expect("enabled is a bool")
}

/// A fake embedding endpoint that answers every POST with `dim`-sized vectors.
pub(crate) async fn embedding_endpoint(dim: usize) -> wiremock::MockServer {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": vec![0.5_f64; dim] } ]
        })))
        .mount(&server)
        .await;
    server
}

/// With the key only in the environment, `KB_EMBEDDING_API_KEY` already turns
/// the knowledge base on at load, and the config saver drops values equal to
/// what the environment produced. `kb enable` used to print "activated" while
/// `config.toml` kept `enabled = false`. A later process must see it on.
#[tokio::test]
async fn kb_enable_with_env_only_key_persists_enabled_for_the_next_process() {
    let server = embedding_endpoint(4).await;
    let tmp = TempDir::new().unwrap();
    let uri = server.uri();

    let (code, stdout, stderr) = run_kb_in_fresh_home(
        tmp.path(),
        &["enable"],
        &[
            ("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key"),
            ("KB_EMBEDDING_BASE_URL", uri.as_str()),
        ],
    );
    assert_eq!(code, 0, "enable must succeed: {stdout} {stderr}");
    assert!(stdout.contains("activated"), "got: {stdout}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "enable probes the embedding endpoint once"
    );

    assert!(
        persisted_enabled(tmp.path()),
        "a command that said \"activated\" must have left the knowledge base on"
    );
}

/// An endpoint that returns the wrong vector size must stop `enable`: the
/// first ingest would otherwise fail with a dimension error.
#[tokio::test]
async fn kb_enable_refuses_when_the_endpoint_returns_the_wrong_vector_size() {
    let server = embedding_endpoint(3).await;
    let tmp = TempDir::new().unwrap();
    let uri = server.uri();

    let (code, stdout, _stderr) = run_kb_in_fresh_home(
        tmp.path(),
        &["enable"],
        &[
            ("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key"),
            ("KB_EMBEDDING_BASE_URL", uri.as_str()),
        ],
    );
    assert_eq!(code, 1, "a wrong vector size must exit 1: {stdout}");
    assert!(
        stdout.starts_with("error[1]{code,message}"),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("3-dimension") && stdout.contains("KB_EMBEDDING_DIM is 4"),
        "the message names both sizes, got: {stdout}"
    );
    assert!(
        !stdout.contains("rantaiclaw_test_key"),
        "the key must not appear in the output: {stdout}"
    );
    assert!(
        !persisted_enabled(tmp.path()),
        "a refused enable must leave the knowledge base off"
    );
}

/// An endpoint that answers 200 with something that is not an embedding list
/// must stop `enable`, and the refusal must not repeat what it sent.
#[tokio::test]
async fn kb_enable_refusal_does_not_echo_a_malformed_endpoint_answer() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "leak": "secret-body-marker" })),
        )
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let uri = server.uri();

    let (code, stdout, stderr) = run_kb_in_fresh_home(
        tmp.path(),
        &["enable"],
        &[
            ("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key"),
            ("KB_EMBEDDING_BASE_URL", uri.as_str()),
        ],
    );
    assert_eq!(code, 1, "a malformed answer must exit 1: {stdout} {stderr}");
    assert!(
        !stdout.contains("secret-body-marker"),
        "the refusal must not echo the endpoint's answer: {stdout}"
    );
    assert!(
        stdout.contains("not an embedding list"),
        "the refusal says what was wrong, got: {stdout}"
    );
    assert!(!persisted_enabled(tmp.path()));
}

/// A 200 whose body is not JSON fails in the HTTP client's decoder. That is
/// not an unreachable endpoint and must not be reported as one.
#[tokio::test]
async fn kb_enable_does_not_call_an_undecodable_answer_unreachable() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("secret-body-marker <html>"))
        .mount(&server)
        .await;
    let tmp = TempDir::new().unwrap();
    let uri = server.uri();

    let (code, stdout, _stderr) = run_kb_in_fresh_home(
        tmp.path(),
        &["enable"],
        &[
            ("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key"),
            ("KB_EMBEDDING_BASE_URL", uri.as_str()),
        ],
    );
    assert_eq!(code, 1, "an undecodable answer must exit 1: {stdout}");
    assert!(
        stdout.contains("not an embedding list"),
        "the refusal says what was wrong, got: {stdout}"
    );
    assert!(
        !stdout.contains("could not reach"),
        "a decode failure is not an unreachable endpoint: {stdout}"
    );
    assert!(!stdout.contains("secret-body-marker"), "{stdout}");
}

/// A dead endpoint must stop `enable` too, and say it could not be reached.
#[tokio::test]
async fn kb_enable_refuses_when_the_endpoint_is_unreachable() {
    // Bind, note the port, release it: nothing listens there afterwards.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/v1/embeddings");
    let tmp = TempDir::new().unwrap();

    let (code, stdout, _stderr) = run_kb_in_fresh_home(
        tmp.path(),
        &["enable"],
        &[
            ("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key"),
            ("KB_EMBEDDING_BASE_URL", url.as_str()),
        ],
    );
    assert_eq!(code, 1, "an unreachable endpoint must exit 1: {stdout}");
    assert!(
        stdout.contains("could not reach the embedding endpoint"),
        "got: {stdout}"
    );
    assert!(
        !persisted_enabled(tmp.path()),
        "a refused enable must leave the knowledge base off"
    );
}

/// `kb disable` writes `enabled = false`, but a key still in the environment
/// turns the knowledge base back on at every load. The command must say so
/// instead of claiming a plain "deactivated", and the file must hold `false`.
#[tokio::test]
async fn kb_disable_reports_when_an_env_key_keeps_the_knowledge_base_on() {
    let server = embedding_endpoint(4).await;
    let tmp = TempDir::new().unwrap();
    let uri = server.uri();
    let env = [
        ("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key"),
        ("KB_EMBEDDING_BASE_URL", uri.as_str()),
    ];

    let (code, stdout, stderr) = run_kb_in_fresh_home(tmp.path(), &["enable"], &env);
    assert_eq!(code, 0, "enable must succeed: {stdout} {stderr}");
    assert!(persisted_enabled(tmp.path()));

    let (code, stdout, stderr) = run_kb_in_fresh_home(tmp.path(), &["disable"], &env);
    assert_eq!(code, 0, "disable must succeed: {stdout} {stderr}");
    assert!(
        stdout.contains("KB_EMBEDDING_API_KEY"),
        "the note must name the variable that keeps it on, got: {stdout}"
    );
    assert!(
        !persisted_enabled(tmp.path()),
        "config.toml must hold the disabled state"
    );
}

/// A dry run must not call the paid embedding endpoint, and its counts are
/// labelled as "would" counts with `re_embedded` left at 0.
#[tokio::test]
async fn kb_re_embed_dry_run_makes_no_embedding_call_and_reports_would_counts() {
    let server = embedding_endpoint(4).await;
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    let doc = sample_doc("rantaiclaw_doc_dry", "Dry Run Doc");
    store.create_document(&doc).await.unwrap();
    let chunks: Vec<rantaiclaw::kb::Chunk> = (0..3)
        .map(|i| rantaiclaw::kb::Chunk {
            content: format!("chunk content {i}"),
            metadata: rantaiclaw::kb::ChunkMetadata {
                document_title: doc.title.clone(),
                category: "FAQ".into(),
                subcategory: None,
                section: None,
                chunk_index: i,
                contextual_prefix: None,
            },
        })
        .collect();
    let embeds = vec![vec![1.0_f32; 4]; chunks.len()];
    store
        .store_chunks(&doc.id, &chunks, &embeds, "rantaiclaw_model_old")
        .await
        .unwrap();
    drop(store);

    let uri = server.uri();
    let (code, stdout, stderr) = run_kb_with_env(
        &db,
        &["re-embed", "--dry-run", "--json"],
        &[("KB_EMBEDDING_BASE_URL", uri.as_str())],
    );
    assert_eq!(code, 0, "dry run must succeed: {stdout} {stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("re-embed json");
    assert_eq!(report["dry_run"], true, "{report}");
    assert_eq!(report["examined"], 3, "{report}");
    assert_eq!(report["would_re_embed"], 3, "{report}");
    assert_eq!(report["re_embedded"], 0, "{report}");
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a dry run must not call the embedding endpoint"
    );
}

/// `kb ingest --group <name>` must attach the new document to the named group.
/// Before the fix the name was written only into `metadata["groups"]`, so the
/// group listed nothing.
#[tokio::test]
async fn kb_ingest_group_attaches_document_to_the_named_group() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [1.0, 0.0, 0.0, 0.0] } ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    let group = store.create_group("Onboarding", None, None).await.unwrap();
    drop(store);

    let md = tmp.path().join("doc.md");
    std::fs::write(&md, "# Onboarding\nWelcome to the team.").unwrap();
    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];

    let (code, stdout, stderr) = run_kb_with_env(
        &db,
        &["ingest", md.to_str().unwrap(), "--group", "Onboarding"],
        &mock_env,
    );
    assert_eq!(
        code, 0,
        "ingest must succeed: stdout={stdout} stderr={stderr}"
    );

    let store = SqliteStore::open(&db, 4).await.unwrap();
    let members = store.list_group_documents(&group.id).await.unwrap();
    assert_eq!(
        members.len(),
        1,
        "the ingested document must be a member of the named group"
    );
    assert_eq!(members[0].title, "doc");
}

/// `kb ingest --group <unknown>` must fail before any embedding call and store
/// no document. The embedder is a wiremock server, so its request count is
/// the number of embedding calls.
#[tokio::test]
async fn kb_ingest_unknown_group_errors_before_embedding_and_stores_nothing() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [1.0, 0.0, 0.0, 0.0] } ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    drop(store);

    let md = tmp.path().join("doc.md");
    std::fs::write(&md, "# Onboarding\nWelcome to the team.").unwrap();
    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];

    let (code, stdout, _stderr) = run_kb_with_env(
        &db,
        &["ingest", md.to_str().unwrap(), "--group", "NoSuchGroup"],
        &mock_env,
    );
    assert_eq!(code, 1, "unknown group must exit 1: {stdout}");
    assert!(
        stdout.starts_with("error[1]{code,message}"),
        "got: {stdout}"
    );
    assert!(stdout.contains("not_found"));
    assert!(stdout.contains("NoSuchGroup"));

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "an unknown group must fail before any embedding call; got {} calls",
        requests.len()
    );

    let store = SqliteStore::open(&db, 4).await.unwrap();
    let docs = store.list_documents(None).await.unwrap();
    assert!(docs.is_empty(), "no document may be stored: {docs:?}");
}

/// `kb search --group <unknown id>` must refuse the group instead of silently
/// answering an empty result — the same `not_found` TOON block the ingest door
/// prints, with exit 1. The embedder is a wiremock server, so its request
/// count proves the refusal happens before any embedding call.
#[tokio::test]
async fn kb_search_unknown_group_errors_toon_and_exits_1() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [1.0, 0.0, 0.0, 0.0] } ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let _store = SqliteStore::open(&db, 4).await.unwrap();

    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];
    let (code, stdout, _stderr) = run_kb_with_env(
        &db,
        &[
            "search",
            "anything at all",
            "--group",
            "rantaiclaw_nosuchgroup",
        ],
        &mock_env,
    );
    assert_eq!(code, 1, "unknown group must exit 1: {stdout}");
    assert!(
        stdout.starts_with("error[1]{code,message}"),
        "got: {stdout}"
    );
    assert!(stdout.contains("not_found"));
    assert!(stdout.contains("rantaiclaw_nosuchgroup"));

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests.is_empty(),
        "an unknown group must be refused before any embedding call; got {} calls",
        requests.len()
    );
}

/// `kb graph --group <unknown id>` must refuse the group the same way instead
/// of printing an empty graph and exiting 0.
#[tokio::test]
async fn kb_graph_unknown_group_errors_toon_and_exits_1() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let _store = SqliteStore::open(&db, 4).await.unwrap();

    let (code, stdout, _stderr) = run_kb(&db, &["graph", "--group", "rantaiclaw_nosuchgroup"]);
    assert_eq!(code, 1, "unknown group must exit 1: {stdout}");
    assert!(
        stdout.starts_with("error[1]{code,message}"),
        "got: {stdout}"
    );
    assert!(stdout.contains("not_found"));
    assert!(stdout.contains("rantaiclaw_nosuchgroup"));
}

/// Control for the two refusals above: a group that EXISTS but holds no
/// documents is not refused — the search answers its empty scope normally with
/// exit 0.
#[tokio::test]
async fn kb_search_with_a_known_empty_group_exits_0_with_an_empty_result() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [ { "embedding": [1.0, 0.0, 0.0, 0.0] } ]
        })))
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let store = SqliteStore::open(&db, 4).await.unwrap();
    let group = store.create_group("Empty Group", None, None).await.unwrap();
    drop(store);

    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];
    let (code, stdout, stderr) = run_kb_with_env(
        &db,
        &["search", "anything at all", "--group", &group.id],
        &mock_env,
    );
    assert_eq!(
        code, 0,
        "a known empty group must still answer exit 0: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("No documents found in this knowledge-base scope."),
        "the empty scope must be stated explicitly: {stdout}"
    );
}

/// A corrupt file is the caller's own input: `kb ingest` prints
/// `unprocessable_document` (as the gateway answers 422) and exits 1.
#[tokio::test]
async fn kb_ingest_of_a_corrupt_file_prints_unprocessable_document_and_exits_1() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let pdf = tmp.path().join("corrupt.pdf");
    std::fs::write(&pdf, b"not a pdf at all").unwrap();

    let (code, stdout, _stderr) = run_kb_with_env(
        &db,
        &["ingest", pdf.to_str().unwrap()],
        &[
            ("KB_EXTRACT_PRIMARY", "unpdf"),
            ("KB_EXTRACT_FALLBACK", "unpdf"),
        ],
    );
    assert_eq!(code, 1, "a corrupt file must exit 1: {stdout}");
    assert!(stdout.contains("unprocessable_document"), "got: {stdout}");
    assert!(!stdout.contains("internal"), "got: {stdout}");
}

/// The vision service is down: `kb ingest` prints `extractor_unavailable`
/// (as the gateway answers 502), exits 1, and leaves out the endpoint URL.
#[tokio::test]
async fn kb_ingest_with_the_vision_service_down_prints_extractor_unavailable_and_exits_1() {
    // Bind, note the port, release it: nothing listens there afterwards.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let png = tmp.path().join("scan.png");
    std::fs::write(&png, b"\x89PNG\r\n\x1a\n rantaiclaw image").unwrap();
    let vision_url = format!("http://127.0.0.1:{port}/rantaiclaw_vision_probe");

    let (code, stdout, _stderr) = run_kb_with_env(
        &db,
        &["ingest", png.to_str().unwrap()],
        &[
            ("KB_EXTRACT_VISION_BASE_URL", vision_url.as_str()),
            ("KB_EXTRACT_VISION_API_KEY", "rantaiclaw_test_vision_key"),
        ],
    );
    assert_eq!(code, 1, "a dead extractor must exit 1: {stdout}");
    assert!(stdout.contains("extractor_unavailable"), "got: {stdout}");
    assert!(
        stdout.contains("the vision service upstream is unreachable"),
        "the answer names the service: {stdout}"
    );
    assert!(!stdout.contains("internal"), "got: {stdout}");
    assert!(
        !stdout.contains("VisionImageExtractor") && !stdout.contains(" extractor upstream"),
        "a Rust type name must not reach the answer: {stdout}"
    );
    assert!(
        !stdout.contains("http") && !stdout.contains("127.0.0.1"),
        "the answer must not carry the endpoint URL: {stdout}"
    );
}

/// MinerU is down for both the primary and the fallback slot: `kb ingest`
/// prints `extractor_unavailable` naming the service alone — not the
/// internal `Fallback(...)` chain label — and exits 1.
#[tokio::test]
async fn kb_ingest_with_the_mineru_primary_and_fallback_down_names_the_service_and_exits_1() {
    // Bind, note the port, release it: nothing listens there afterwards.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let pdf = tmp.path().join("scan.pdf");
    std::fs::write(&pdf, b"%PDF-1.4 whatever").unwrap();
    let mineru_url = format!("http://127.0.0.1:{port}");

    let (code, stdout, _stderr) = run_kb_with_env(
        &db,
        &["ingest", pdf.to_str().unwrap()],
        &[
            ("KB_EXTRACT_PRIMARY", "mineru"),
            ("KB_EXTRACT_FALLBACK", "mineru"),
            ("KB_EXTRACT_MINERU_BASE_URL", mineru_url.as_str()),
        ],
    );
    assert_eq!(code, 1, "a dead extractor must exit 1: {stdout}");
    assert!(stdout.contains("extractor_unavailable"), "got: {stdout}");
    assert!(
        stdout.contains("the MinerU service upstream is unreachable"),
        "the answer names the MinerU service alone: {stdout}"
    );
    assert!(
        !stdout.contains("Fallback(") && !stdout.contains("SmartRouter("),
        "an internal composite label must not reach the answer: {stdout}"
    );
    assert!(!stdout.contains("internal"), "got: {stdout}");
}

/// An embedding endpoint that refused the key (HTTP 401) is the operator's
/// to fix: `kb ingest` prints `provider_auth_failed` with the fixed sentence
/// the HTTP ingest answers, exits 1, and leaves out the upstream body and
/// URL — the same body shape the gateway's 502 carries.
#[tokio::test]
async fn kb_ingest_with_a_rejected_embedding_key_prints_provider_auth_failed_and_exits_1() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string("the upstream echoes the credential"),
        )
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let md = tmp.path().join("doc.md");
    std::fs::write(
        &md,
        "# Note\nOne short paragraph about lanterns, long enough to embed.",
    )
    .unwrap();
    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];

    let (code, stdout, _stderr) =
        run_kb_with_env(&db, &["ingest", md.to_str().unwrap()], &mock_env);
    assert_eq!(code, 1, "a rejected key must exit 1: {stdout}");
    assert!(stdout.contains("provider_auth_failed"), "got: {stdout}");
    assert!(
        stdout.contains("the embedding provider rejected the API key"),
        "the fixed sentence must be printed: {stdout}"
    );
    assert!(!stdout.contains("internal"), "got: {stdout}");
    assert!(
        !stdout.contains("echoes the credential"),
        "the upstream body must never surface: {stdout}"
    );
    assert!(
        !stdout.contains("http") && !stdout.contains("127.0.0.1"),
        "the answer must not carry the endpoint URL: {stdout}"
    );
}

/// An upstream error status that is not a rejected key (HTTP 500, after the
/// client's own transient retries) is the upstream's failure: `kb ingest`
/// names the service and the status, exits 1, and carries neither the
/// upstream body — which can echo the credential back — nor the endpoint
/// URL, the same words the HTTP ingest answers with.
#[tokio::test]
async fn kb_ingest_with_a_500_embedding_endpoint_prints_the_status_without_the_body() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("the upstream wrote a failure page"),
        )
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let md = tmp.path().join("doc.md");
    std::fs::write(
        &md,
        "# Note\nOne short paragraph about lanterns, long enough to embed.",
    )
    .unwrap();
    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];

    let (code, stdout, _stderr) =
        run_kb_with_env(&db, &["ingest", md.to_str().unwrap()], &mock_env);
    assert_eq!(code, 1, "an upstream error must exit 1: {stdout}");
    assert!(stdout.contains("embedding_upstream"), "got: {stdout}");
    assert!(
        stdout.contains("returned status 500"),
        "the status is named: {stdout}"
    );
    assert!(!stdout.contains("internal"), "got: {stdout}");
    assert!(
        !stdout.contains("the upstream wrote a failure page"),
        "the upstream body must never surface: {stdout}"
    );
    assert!(
        !stdout.contains("http") && !stdout.contains("127.0.0.1"),
        "the answer must not carry the endpoint URL: {stdout}"
    );
}

/// A 2xx answer whose JSON is the wrong shape — compat layers serve
/// `{"error": …}` with 200 — is the upstream's failure: `kb ingest` prints
/// the fixed diagnostic, exits non-zero, and carries neither the upstream
/// payload — which can echo the credential back — nor the endpoint URL.
#[tokio::test]
async fn kb_ingest_with_a_malformed_200_embedding_answer_prints_a_fixed_diagnostic() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "error": "the upstream echoed the credential" }),
            ),
        )
        .mount(&server)
        .await;

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("kb.db");
    let md = tmp.path().join("doc.md");
    std::fs::write(
        &md,
        "# Note\nOne short paragraph about lanterns, long enough to embed.",
    )
    .unwrap();
    let uri = server.uri();
    let mock_env = [("KB_EMBEDDING_BASE_URL", uri.as_str())];

    let (code, stdout, _stderr) =
        run_kb_with_env(&db, &["ingest", md.to_str().unwrap()], &mock_env);
    assert_ne!(code, 0, "a malformed answer must exit non-zero: {stdout}");
    assert!(
        stdout.contains("Invalid embedding response structure"),
        "the fixed diagnostic is printed: {stdout}"
    );
    assert!(
        !stdout.contains("the upstream echoed the credential"),
        "the upstream payload must never surface: {stdout}"
    );
    assert!(
        !stdout.contains("http") && !stdout.contains("127.0.0.1"),
        "the answer must not carry the endpoint URL: {stdout}"
    );
}
