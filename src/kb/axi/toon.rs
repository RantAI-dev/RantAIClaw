//! TOON (Token-Optimized Object Notation) formatter for axi-cli output.
//!
//! Per `axi.md`, the agent surface emits a compact tabular form that costs
//! fewer tokens than JSON while staying machine-parsable. Shape:
//!
//! ```text
//! items[3]{id,title,score}:
//!   doc-1,Insurance Policy,0.91
//!   doc-2,Coverage Rules,0.87
//!   doc-3,FAQ Entry,0.75
//! ```
//!
//! The header declares the row count and the projected columns. Each row is
//! indented two spaces and contains the projected field values joined by `,`.
//!
//! Quoting rules (per axi.md):
//! - Strings containing a comma get double-quoted with `"` escapes.
//! - Newlines inside strings collapse to a single space — TOON rows MUST be
//!   line-oriented.
//! - Numbers, booleans, null render verbatim (null → empty cell).
//!
//! This formatter is intentionally narrow: it converts any
//! `serde::Serialize` item to `serde_json::Value`, then projects a fixed
//! field set. Anything richer (nested objects, arrays in cells) is reduced
//! to a JSON string representation — the caller is responsible for picking
//! flat fields. Keeps the AXI surface predictable.
//!
//! See `tests/kb/cli_test.rs` for the behavior matrix.
//!
//! Defensive note: TOON consumers typically parse line-by-line. Embedding a
//! literal newline inside a cell would corrupt downstream tooling, so the
//! formatter strips `\r` and replaces `\n` with a space rather than relying
//! on quoting semantics.

use serde::Serialize;

/// Project `items` into TOON output with header `name[count]{fields}:` and
/// one row per item.
///
/// Items are serialized via `serde_json::to_value` and the listed fields are
/// pulled by key. Missing fields render as empty cells (matching `null`).
///
/// Returns the header alone (no rows) when `items` is empty. Output never
/// contains a trailing newline beyond the final row separator.
pub fn format_toon<T: Serialize>(name: &str, items: &[T], fields: &[&str]) -> String {
    let mut out = String::new();
    out.push_str(name);
    out.push('[');
    out.push_str(&items.len().to_string());
    out.push(']');
    out.push('{');
    out.push_str(&fields.join(","));
    out.push('}');
    out.push(':');
    out.push('\n');

    for item in items {
        // Serialization to `Value` is infallible for any well-formed
        // `Serialize` impl. Treat a failure as an empty row so the header
        // count stays honest — operators shouldn't see TOON output silently
        // truncated when one item happens to serialize badly.
        let value = serde_json::to_value(item).unwrap_or(serde_json::Value::Null);
        let cells: Vec<String> = fields
            .iter()
            .map(|f| {
                let v = value.get(*f).unwrap_or(&serde_json::Value::Null);
                serialize_value(v)
            })
            .collect();
        out.push_str("  ");
        out.push_str(&cells.join(","));
        out.push('\n');
    }

    out
}

/// Render one cell value to its TOON-compatible string form.
///
/// - `Null` → empty string (renders as an empty cell).
/// - `Bool` / `Number` → unquoted lexical form.
/// - `String` → as-is unless it contains a comma or quote (quoted with `"`
///   escapes). Newlines/carriage returns are collapsed to a single space
///   before the quote-decision so multi-line content can never break TOON's
///   line-oriented parsing.
/// - `Array` / `Object` → JSON serialization, then quoted if it contains a
///   comma (which it always will for non-trivial nested values). Cheaper
///   than implementing a full nested-TOON encoder, and unlikely to be hit
///   for the flat fields TOON is built for.
pub fn serialize_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format_string_cell(s),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            let raw = serde_json::to_string(v).unwrap_or_default();
            format_string_cell(&raw)
        }
    }
}

fn format_string_cell(s: &str) -> String {
    // Collapse line endings first — TOON consumers split on `\n`, so embedded
    // newlines must never reach the output buffer.
    let single_line: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\r' => ' ',
            other => other,
        })
        .collect();

    if single_line.contains(',') || single_line.contains('"') {
        let escaped = single_line.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        single_line
    }
}
