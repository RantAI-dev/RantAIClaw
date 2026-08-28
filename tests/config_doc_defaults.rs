//! Guard: every SCALAR default documented in `docs/reference/config.md` must
//! match the real `Config::default()`. Catches the J7-class drift — config.md
//! claimed the channel message-timeout default was `300s` while the real default
//! is `600s` — at CI time instead of by a reader tripping over it later.
//!
//! Best-effort and honest about its limits: a documented key is checked only
//! when it maps UNAMBIGUOUSLY to exactly one scalar leaf of the serialized
//! default. Section-relative keys that collide across sections (`enabled`,
//! `model`, `api_key`, …), non-scalar defaults, and `unset` / `_required_` / `—`
//! rows are reported as UNVERIFIED, never failed — the same grep-limit honesty
//! as the config-reader gate. A documented scalar that resolves to exactly one
//! schema leaf and disagrees with it is a hard failure.

use std::collections::HashMap;

use rantaiclaw::config::Config;

/// Recurse the serialized default, collecting `leaf_key -> [scalar values]`.
/// A key that appears under more than one section lands in a >1 vec and is
/// treated as ambiguous (unverifiable by name alone).
fn collect_scalar_leaves(
    value: &serde_json::Value,
    out: &mut HashMap<String, Vec<serde_json::Value>>,
) {
    if let serde_json::Value::Object(map) = value {
        for (key, child) in map {
            match child {
                serde_json::Value::Object(_) => collect_scalar_leaves(child, out),
                serde_json::Value::Array(_) | serde_json::Value::Null => {}
                scalar => out.entry(key.clone()).or_default().push(scalar.clone()),
            }
        }
    }
}

/// The documented default cell, if it is a single backtick-wrapped scalar.
/// `unset`, `_required_`, `—`, list cells, and prose are rejected (return None).
fn parse_documented_default(cell: &str) -> Option<String> {
    let cell = cell.trim();
    if !(cell.starts_with('`') && cell.ends_with('`') && cell.len() >= 2) {
        return None;
    }
    let inner = &cell[1..cell.len() - 1];
    // A single scalar has no embedded backtick (which would mean a multi-value
    // or annotated cell).
    if inner.is_empty() || inner.contains('`') {
        return None;
    }
    Some(inner.to_string())
}

/// Whether a documented scalar string matches a JSON scalar from the schema.
fn documented_matches(doc: &str, actual: &serde_json::Value) -> bool {
    match actual {
        serde_json::Value::Bool(b) => doc.eq_ignore_ascii_case(&b.to_string()),
        serde_json::Value::Number(n) => doc
            .parse::<f64>()
            .ok()
            .zip(n.as_f64())
            .is_some_and(|(a, b)| (a - b).abs() < f64::EPSILON),
        serde_json::Value::String(s) => doc == s,
        _ => false,
    }
}

#[test]
fn documented_config_defaults_match_schema() {
    let config_md = include_str!("../docs/reference/config.md");

    let default_json =
        serde_json::to_value(Config::default()).expect("Config::default serializes to JSON");
    let mut leaves: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
    collect_scalar_leaves(&default_json, &mut leaves);

    // First pass: collect every documented (key -> [default]) row. A key
    // documented more than once (e.g. `transport` appears under several
    // peripheral boards with different example defaults) is ambiguous on the DOC
    // side and cannot be attributed to one schema leaf — collect all so we can
    // skip it.
    let mut documented: HashMap<String, Vec<String>> = HashMap::new();
    for line in config_md.lines() {
        if !line.trim_start().starts_with("| `") {
            continue;
        }
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 3 {
            continue;
        }
        let Some(key) = cols[1]
            .strip_prefix('`')
            .and_then(|k| k.strip_suffix('`'))
            .filter(|k| !k.is_empty() && !k.contains('`'))
        else {
            continue;
        };
        let Some(doc_default) = parse_documented_default(cols[2]) else {
            continue;
        };
        documented
            .entry(key.to_string())
            .or_default()
            .push(doc_default);
    }

    let mut checked = 0usize;
    let mut unverified = 0usize;
    let mut mismatches: Vec<String> = Vec::new();

    for (key, defaults) in &documented {
        // Verifiable only when the key is documented ONCE and resolves to exactly
        // ONE scalar schema leaf. Multi-row docs, skip-when-empty fields, and
        // names shared across sections are reported, not failed.
        match (defaults.as_slice(), leaves.get(key).map(Vec::as_slice)) {
            ([doc_default], Some([actual])) => {
                checked += 1;
                if !documented_matches(doc_default, actual) {
                    mismatches.push(format!(
                        "`{key}`: config.md documents default `{doc_default}` but Config::default() is `{actual}`"
                    ));
                }
            }
            _ => unverified += 1,
        }
    }
    mismatches.sort();

    // Sanity: the parser must actually resolve a meaningful number of defaults,
    // else a table-format change silently turned this into a no-op.
    assert!(
        checked >= 15,
        "only {checked} documented defaults were verifiable — the config.md table \
         format likely changed and this guard has gone blind; fix the parser"
    );

    assert!(
        mismatches.is_empty(),
        "documented config defaults drifted from Config::default() \
         ({checked} checked, {unverified} unverified by name):\n  - {}",
        mismatches.join("\n  - ")
    );

    eprintln!("config.md default check: {checked} verified, {unverified} unverified by name");
}
