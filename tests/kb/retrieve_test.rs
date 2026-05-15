//! Tests for the KB retrieval pipeline (`src/kb/retrieve/`).
//!
//! Layered to match the implementation sub-tasks:
//! - 7.1 RRF (pure unit)
//! - 7.2 Retriever orchestrator (fake store + fake embedder)
//! - 7.3 Query expansion (wiremock)
//! - 7.4 Contextual retrieval (wiremock)
//! - 7.5 format_context_for_prompt + standalone query rewriter (wiremock)

use rantaiclaw::kb::retrieve::rrf::{reciprocal_rank_fusion, RrfOptions};

// ---- Task 7.1: RRF ---------------------------------------------------

#[test]
fn rrf_basic_two_lists() {
    let a: Vec<(String, ())> = vec![("x".into(), ()), ("y".into(), ()), ("z".into(), ())];
    let b: Vec<(String, ())> = vec![("y".into(), ()), ("x".into(), ()), ("w".into(), ())];
    let merged = reciprocal_rank_fusion(
        &[a.as_slice(), b.as_slice()],
        RrfOptions {
            k: 60,
            limit: None,
        },
    );
    assert_eq!(merged.len(), 4, "all four unique ids preserved");
    let ids: Vec<&str> = merged.iter().map(|r| r.id.as_str()).collect();
    // x and y both appear in both lists (so should rank above z and w which
    // only appear once).
    let x_pos = ids.iter().position(|&i| i == "x").expect("x present");
    let y_pos = ids.iter().position(|&i| i == "y").expect("y present");
    let z_pos = ids.iter().position(|&i| i == "z").expect("z present");
    let w_pos = ids.iter().position(|&i| i == "w").expect("w present");
    assert!(x_pos <= 1, "x ranked top-2 (got pos {x_pos})");
    assert!(y_pos <= 1, "y ranked top-2 (got pos {y_pos})");
    assert!(
        x_pos < z_pos && x_pos < w_pos && y_pos < z_pos && y_pos < w_pos,
        "shared ids beat singletons"
    );
}

#[test]
fn rrf_respects_limit() {
    let a: Vec<(String, ())> = (0..100).map(|i| (format!("id-{i}"), ())).collect();
    let merged = reciprocal_rank_fusion(
        &[a.as_slice()],
        RrfOptions {
            k: 60,
            limit: Some(5),
        },
    );
    assert_eq!(merged.len(), 5, "limit truncates output");
}

#[test]
fn rrf_score_increases_with_appearances() {
    let a: Vec<(String, ())> = vec![("triple".into(), ()), ("solo".into(), ())];
    let b: Vec<(String, ())> = vec![("triple".into(), ())];
    let c: Vec<(String, ())> = vec![("triple".into(), ())];
    let merged = reciprocal_rank_fusion(
        &[a.as_slice(), b.as_slice(), c.as_slice()],
        RrfOptions::default(),
    );
    let triple = merged.iter().find(|r| r.id == "triple").expect("triple");
    let solo = merged.iter().find(|r| r.id == "solo").expect("solo");
    assert!(
        triple.rrf_score > solo.rrf_score,
        "id appearing in 3 lists must score higher than id in 1: {} vs {}",
        triple.rrf_score,
        solo.rrf_score
    );
    assert_eq!(triple.sources, vec![0, 1, 2], "all source indices tracked");
}

#[test]
fn rrf_preserves_first_seen_payload() {
    // Same id "shared" with different payloads across lists. `first` must
    // be the payload from list 0 (vector arm), not list 1 (BM25 arm) —
    // mirrors the TS source's "vector wins metadata ties" contract.
    let a: Vec<(String, &str)> = vec![("shared".into(), "vector-meta")];
    let b: Vec<(String, &str)> = vec![("shared".into(), "bm25-meta")];
    let merged = reciprocal_rank_fusion(&[a.as_slice(), b.as_slice()], RrfOptions::default());
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].first, "vector-meta",
        "first-seen payload from list 0 preserved"
    );
    assert_eq!(merged[0].sources, vec![0, 1]);
}
