//! Phase-12 parity gate.
//!
//! Ingests a synthetic corpus built from the union of `expectedDocs`
//! titles in `tests/fixtures/rag-golden-seed.json` and
//! `tests/fixtures/rag-golden.json`, then runs every `lookup`-kind query
//! through the Rust retriever and asserts at least one of the entry's
//! `expectedDocs` titles ends up in the top-K source list.
//!
//! Gated `#[ignore]` because:
//!   1. it requires `OPENROUTER_API_KEY`,
//!   2. ingest + per-query retrieval takes ~30-60s end-to-end,
//!   3. it talks to OpenRouter and is therefore not deterministic enough
//!      for the default CI pass — surface as an explicit acceptance gate
//!      instead.
//!
//! Run manually:
//!
//! ```bash
//! OPENROUTER_API_KEY=... \
//!   cargo test --features kb --release --test kb \
//!   -- --ignored rag_golden_parity --nocapture
//! ```

use std::sync::Arc;

use rantaiclaw::kb::embed::make_provider;
use rantaiclaw::kb::retrieve::{RetrieveOptions, Retriever};
use rantaiclaw::kb::store::sqlite::SqliteStore;
use rantaiclaw::kb::store::KbStore;
use rantaiclaw::kb::KbConfig;
use tempfile::TempDir;

use super::parity_helpers::{build_corpus_from_fixtures, fixture_path, ingest_corpus};

const RECALL_THRESHOLD: f64 = 0.85;
const TOP_K: usize = 8;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires OPENROUTER_API_KEY + parity corpus ingest takes ~30-60s"]
async fn rag_golden_parity() {
    // Skip cleanly when no key — keeps `cargo test -- --ignored` runnable
    // in dev without surprising on CI machines that lack credentials.
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        eprintln!("skipping rag_golden_parity: OPENROUTER_API_KEY not set");
        return;
    }

    let cfg = KbConfig::from_env().expect("KbConfig::from_env");

    let tmp = TempDir::new().expect("tempdir");
    let store: Arc<dyn KbStore> = Arc::new(
        SqliteStore::open(tmp.path().join("kb.db"), cfg.embedding_dim)
            .await
            .expect("open SqliteStore"),
    );
    let embedder = make_provider(&cfg).expect("make_provider");

    let seed_path = fixture_path("rag-golden-seed.json");
    let golden_path = fixture_path("rag-golden.json");
    eprintln!(
        "fixture paths: seed={} golden={}",
        seed_path.display(),
        golden_path.display(),
    );
    assert!(
        seed_path.exists(),
        "rag-golden-seed.json not found at {} — adjust fixture_path()",
        seed_path.display(),
    );
    assert!(
        golden_path.exists(),
        "rag-golden.json not found at {} — adjust fixture_path()",
        golden_path.display(),
    );

    eprintln!("building corpus from fixtures...");
    let corpus =
        build_corpus_from_fixtures(&seed_path, &golden_path).expect("build_corpus_from_fixtures");
    eprintln!("synthesized {} unique documents", corpus.len());

    eprintln!("ingesting corpus (this calls OpenRouter)...");
    let chunk_total = ingest_corpus(&store, &embedder, &corpus)
        .await
        .expect("ingest_corpus");
    eprintln!("ingested {chunk_total} chunks");

    let retriever = Retriever::new(cfg.clone(), store.clone(), embedder.clone());

    let golden_raw = std::fs::read_to_string(&golden_path).expect("read rag-golden.json");
    let golden: serde_json::Value =
        serde_json::from_str(&golden_raw).expect("parse rag-golden.json");
    let entries = golden["entries"]
        .as_array()
        .expect("rag-golden.json: entries[]");

    let mut pass = 0usize;
    let mut total = 0usize;
    let mut misses: Vec<String> = Vec::new();

    for entry in entries {
        // Only `lookup` entries carry a directly-assertable expectedDocs
        // contract. `followup` depends on standalone-query rewrite which
        // is a separate test surface, `oos` expects refusal not retrieval,
        // and `enumerate` checks group membership coverage that the
        // current corpus doesn't model.
        let kind = entry["kind"].as_str().unwrap_or("");
        if kind != "lookup" {
            continue;
        }
        let q = match entry["query"].as_str() {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let expected: Vec<&str> = entry["expectedDocs"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if expected.is_empty() {
            continue;
        }
        total += 1;

        let result = retriever
            .retrieve(
                q,
                RetrieveOptions {
                    max_chunks: Some(TOP_K),
                    ..Default::default()
                },
            )
            .await
            .expect("retrieve");
        let retrieved_titles: std::collections::HashSet<&str> = result
            .sources
            .iter()
            .map(|s| s.document_title.as_str())
            .collect();

        let hit = expected.iter().any(|e| retrieved_titles.contains(e));
        if hit {
            pass += 1;
        } else {
            misses.push(format!(
                "MISS: query={q:?} expected={expected:?} got={retrieved_titles:?}",
            ));
        }
    }

    assert!(
        total > 0,
        "no lookup-kind entries found in rag-golden.json — fixture changed?"
    );

    let recall = pass as f64 / total as f64;
    eprintln!("parity hit@{TOP_K} recall = {recall:.3} ({pass}/{total})");
    for m in &misses {
        eprintln!("{m}");
    }
    assert!(
        recall >= RECALL_THRESHOLD,
        "recall {recall:.3} below {RECALL_THRESHOLD:.2} threshold",
    );
}
