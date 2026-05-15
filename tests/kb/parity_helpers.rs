//! Shared helpers for the Phase-12 parity tests.
//!
//! `rag-golden-seed.json` and `rag-golden.json` are query sets (each entry
//! is `{id, query, kind, expectedDocs, ...}`); they do NOT carry the raw
//! corpus content. The original TS evaluator pulls bodies from a running
//! DB, which we cannot do in a hermetic Rust integration test.
//!
//! To keep the parity test self-contained we synthesize a corpus from the
//! union of `expectedDocs` titles found in both fixtures: every unique
//! title becomes one document whose body restates the title plus any
//! `expectedAnswerSubstrings` the curator listed. This is enough signal
//! for embedding-based retrieval to rank the right document for a given
//! query without depending on external state.
//!
//! The helper here is plain Rust (no test attribute) so it can be reused
//! by both the parity test and any future bench setup that wants a
//! seeded store. `#![allow(dead_code)]` because some helpers (e.g.
//! `ingest_corpus`) are only consumed by `#[ignore]`-gated tests and
//! would otherwise flag warnings on a `cargo test --no-run` of just the
//! baseline suite.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use rantaiclaw::kb::chunk::{prepare_chunk_for_embedding, smart_chunk_document, SmartChunkOptions};
use rantaiclaw::kb::embed::EmbeddingProvider;
use rantaiclaw::kb::store::KbStore;
use rantaiclaw::kb::{Document, DocumentId};

/// One synthesized document built from a unique `expectedDocs` title.
#[derive(Debug, Clone)]
pub struct SeedDoc {
    pub title: String,
    pub content: String,
}

/// Read both golden fixtures and collapse their `expectedDocs` arrays into
/// a deduplicated list of `(title, synthesized_content)` pairs. The seed
/// fixture provides ~300 entries and the curated fixture ~40; together
/// they give us a corpus of unique document titles to ingest.
///
/// `expectedAnswerSubstrings` (when present) is appended to the body so
/// chunks that the curator considered "expected answers" become embedded
/// content for that document.
pub fn build_corpus_from_fixtures(
    seed_path: &Path,
    golden_path: &Path,
) -> Result<Vec<SeedDoc>, Box<dyn std::error::Error>> {
    // BTreeMap preserves deterministic title order across runs (helps when
    // diffing CI logs across machines).
    let mut by_title: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for path in [seed_path, golden_path] {
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(path)?;
        let v: serde_json::Value = serde_json::from_str(&raw)?;
        let Some(entries) = v["entries"].as_array() else {
            continue;
        };
        for entry in entries {
            let titles = entry["expectedDocs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let substrings = entry["expectedAnswerSubstrings"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for t in titles {
                let bag = by_title.entry(t).or_default();
                for s in &substrings {
                    if !bag.contains(s) {
                        bag.push(s.clone());
                    }
                }
            }
        }
    }

    let docs = by_title
        .into_iter()
        .map(|(title, substrings)| {
            // Body: title twice (boost surface form) + each curated answer
            // substring on its own line. If no substrings are present, the
            // title alone still embeds — short, but unambiguous.
            let mut body = String::with_capacity(title.len() * 2 + 64);
            body.push_str(&title);
            body.push_str("\n\n");
            body.push_str(&title);
            for s in &substrings {
                body.push_str("\n\n");
                body.push_str(s);
            }
            SeedDoc {
                title,
                content: body,
            }
        })
        .collect();

    Ok(docs)
}

/// Ingest a list of seed documents into the store: create the Document
/// row, smart-chunk the body, embed every chunk, and persist via
/// `store.store_chunks`. Returns the total chunk count actually written.
///
/// Errors propagate so the caller can surface ingest failures before
/// starting the retrieval portion of the parity test.
pub async fn ingest_corpus(
    store: &Arc<dyn KbStore>,
    embedder: &Arc<dyn EmbeddingProvider>,
    docs: &[SeedDoc],
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut total = 0usize;
    for (i, sd) in docs.iter().enumerate() {
        let doc = Document {
            id: DocumentId(format!("parity-doc-{i:04}")),
            title: sd.title.clone(),
            content: sd.content.clone(),
            categories: vec!["PARITY".into()],
            subcategory: None,
            metadata: serde_json::json!({}),
            s3_key: None,
            file_type: Some("markdown".into()),
            mime_type: Some("text/markdown".into()),
            file_size: Some(sd.content.len() as u64),
            organization_id: Some("rantaiclaw_parity_org".into()),
            created_by: None,
            session_id: None,
            artifact_type: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deleted_at: None,
            retention_days: None,
            retrieval_count: 0,
            last_retrieved_at: None,
        };
        store.create_document(&doc).await?;

        let chunks = smart_chunk_document(
            &sd.content,
            &sd.title,
            "PARITY",
            None,
            SmartChunkOptions::default(),
        );
        if chunks.is_empty() {
            continue;
        }
        let texts: Vec<String> = chunks.iter().map(prepare_chunk_for_embedding).collect();
        let embeddings = embedder.embed_many(&texts).await?;
        store
            .store_chunks(&doc.id, &chunks, &embeddings, embedder.model())
            .await?;
        total += chunks.len();
    }
    Ok(total)
}

/// Resolve the absolute path of a fixture under the parent repo's
/// `tests/fixtures/` directory.
///
/// The KB worktree lives at `packages/rantaiclaw/.worktrees/<branch>` and
/// the fixtures are kept at the repo root. We anchor the lookup against
/// `CARGO_MANIFEST_DIR` (set by cargo to the rantaiclaw crate root) and
/// walk up the four-level chain; this keeps the helper working both
/// when run from a worktree and when run from a regular checkout of the
/// rantaiclaw submodule.
pub fn fixture_path(name: &str) -> std::path::PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let base = std::path::PathBuf::from(manifest_dir);
    // Worktree case: <repo>/packages/rantaiclaw/.worktrees/<branch>
    let worktree_candidate = base.join("../../../tests/fixtures").join(name);
    if worktree_candidate.exists() {
        return worktree_candidate;
    }
    // Submodule case: <repo>/packages/rantaiclaw
    let submodule_candidate = base.join("../../tests/fixtures").join(name);
    if submodule_candidate.exists() {
        return submodule_candidate;
    }
    // Fallback: relative to current working directory.
    std::path::PathBuf::from("tests/fixtures").join(name)
}
