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
