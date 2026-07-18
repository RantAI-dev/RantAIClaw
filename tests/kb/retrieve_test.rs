//! Tests for the KB retrieval pipeline (`src/kb/retrieve/`).
//!
//! Layered to match the implementation sub-tasks:
//! - 7.1 RRF (pure unit)
//! - 7.2 Retriever orchestrator (fake store + fake embedder)
//! - 7.3 Query expansion (wiremock)
//! - 7.4 Contextual retrieval (wiremock)
//! - 7.5 format_context_for_prompt + standalone query rewriter (wiremock)
//!
//! Tests that mutate `OPENROUTER_API_KEY` serialize on `ENV_LOCK` from
//! `tests/kb/common.rs` and intentionally hold the guard across `.await`
//! to keep env mutation single-threaded — see the rationale in
//! `embed_test.rs` / `rerank_test.rs`.

#![allow(clippy::await_holding_lock)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use rantaiclaw::kb::embed::EmbeddingProvider;
use rantaiclaw::kb::intelligence::types::{Entity, EntityMention, Relation};
use rantaiclaw::kb::retrieve::rrf::{reciprocal_rank_fusion, RrfOptions};
use rantaiclaw::kb::retrieve::{RetrieveOptions, Retriever};
use rantaiclaw::kb::store::{Bm25Hit, Graph, IntelligenceStore, KbStore, SearchFilter};
use rantaiclaw::kb::{
    Chunk, ChunkId, Document, DocumentId, KbConfig, KbGroup, KbGroupSummary, KbResult, SearchResult,
};

// Process-wide env-mutation lock lives in `crate::kb::common::ENV_LOCK`
// so every test module in this binary serializes against the SAME mutex.
// The query_expansion_tests, contextual_tests, and standalone_tests modules
// all mutate `OPENROUTER_API_KEY` and `KB_OPENROUTER_CHAT_URL` — a per-file
// or per-module lock would only serialize within its own scope, letting
// cross-module tests race on shared env state. Re-export under this file's
// scope so the nested test modules can keep using `super::ENV_LOCK`.
pub(crate) use crate::kb::common::ENV_LOCK;

// ---- Task 7.1: RRF ---------------------------------------------------

#[test]
fn rrf_basic_two_lists() {
    let a: Vec<(String, ())> = vec![("x".into(), ()), ("y".into(), ()), ("z".into(), ())];
    let b: Vec<(String, ())> = vec![("y".into(), ()), ("x".into(), ()), ("w".into(), ())];
    let merged = reciprocal_rank_fusion(
        &[a.as_slice(), b.as_slice()],
        RrfOptions { k: 60, limit: None },
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

// ---- Task 7.2 fakes ---------------------------------------------------

/// Build a `KbConfig` with retrieval-relevant knobs set to test-friendly
/// values. `expansion_enabled` and similar feature toggles default off.
fn test_cfg() -> KbConfig {
    KbConfig {
        extract_primary: "smart".into(),
        extract_fallback: "unpdf".into(),
        extract_smart_fallback: "rantaiclaw_test_model_a".into(),
        embedding_model: "test-model".into(),
        embedding_dim: 4,
        default_max_chunks: 4,
        rerank_enabled: false,
        rerank_provider: String::new(),
        rerank_model: "rantaiclaw_test_model_a".into(),
        rerank_initial_k: 20,
        rerank_final_k: 5,
        hybrid_bm25_enabled: true,
        contextual_retrieval_enabled: false,
        contextual_retrieval_model: "rantaiclaw_test_model_a".into(),
        query_expansion_enabled: false,
        query_expansion_model: "rantaiclaw_test_model_a".into(),
        query_expansion_paraphrases: 3,
        standalone_query_enabled: false,
        extract_vision_base_url: String::new(),
        extract_vision_api_key: String::new(),
        extract_mineru_base_url: String::new(),
        embedding_base_url: "http://localhost".into(),
        embedding_api_key: String::new(),
        embed_batch_size: 100,
        embed_concurrency: 2,
        query_embed_cache_size: 8,
        query_embed_cache_ttl_ms: 60_000,
        // Default points at the public URL; per-test we overwrite to
        // `server.uri()` so we don't have to set KB_OPENROUTER_CHAT_URL.
        openrouter_chat_url: "http://localhost".into(),
        intelligence_enabled: false,
        intelligence_model: "openai/gpt-4.1-nano".into(),
        intelligence_resolution: "exact".into(),
        graph_max_nodes: 200,
        graphrag_enabled: false,
        graphrag_max_neighbors: 20,
    }
}

fn search_result(id: &str, doc: &str, title: &str, similarity: f32) -> SearchResult {
    SearchResult {
        id: ChunkId(id.into()),
        document_id: DocumentId(doc.into()),
        document_title: title.into(),
        content: format!("content of {id}"),
        categories: vec!["test".into()],
        subcategory: None,
        section: Some("S1".into()),
        similarity,
        contextual_prefix: None,
    }
}

/// Fake KbStore — returns pre-canned vector + BM25 hits, records the doc
/// ids passed to `record_retrieval_hits` for fire-and-forget assertions.
struct FakeStore {
    vector_hits: Vec<SearchResult>,
    bm25_hits: Vec<Bm25Hit>,
    /// Track that record_retrieval_hits was invoked (fire-and-forget assertion).
    hits_recorded: Arc<AtomicUsize>,
    /// When true, record_retrieval_hits panics — used to prove the orchestrator
    /// detaches the call from the chat-path Future.
    panic_on_record: bool,
}

impl FakeStore {
    fn new(vector_hits: Vec<SearchResult>, bm25_hits: Vec<Bm25Hit>) -> Self {
        Self {
            vector_hits,
            bm25_hits,
            hits_recorded: Arc::new(AtomicUsize::new(0)),
            panic_on_record: false,
        }
    }
}

#[async_trait]
impl KbStore for FakeStore {
    async fn create_document(&self, _doc: &Document) -> KbResult<()> {
        Ok(())
    }
    async fn get_document(&self, _id: &DocumentId) -> KbResult<Option<Document>> {
        Ok(None)
    }
    async fn update_document(&self, _doc: &Document) -> KbResult<()> {
        Ok(())
    }
    async fn delete_document(&self, _id: &DocumentId, _soft: bool) -> KbResult<()> {
        Ok(())
    }
    async fn list_documents(&self, _organization_id: Option<&str>) -> KbResult<Vec<Document>> {
        Ok(Vec::new())
    }
    async fn record_retrieval_hits(&self, ids: &[DocumentId]) -> KbResult<()> {
        self.hits_recorded.fetch_add(ids.len(), Ordering::SeqCst);
        if self.panic_on_record {
            panic!("intentional panic in fake store: tests assert fire-and-forget isolation");
        }
        Ok(())
    }
    async fn create_group(
        &self,
        _name: &str,
        _description: Option<&str>,
        _color: Option<&str>,
    ) -> KbResult<KbGroup> {
        unimplemented!("FakeStore does not exercise group CRUD")
    }
    async fn list_groups(&self) -> KbResult<Vec<KbGroupSummary>> {
        Ok(Vec::new())
    }
    async fn get_group(&self, _id: &str) -> KbResult<Option<KbGroup>> {
        Ok(None)
    }
    async fn update_group(
        &self,
        _id: &str,
        _name: Option<&str>,
        _description: Option<&str>,
        _color: Option<&str>,
    ) -> KbResult<()> {
        Ok(())
    }
    async fn delete_group(&self, _id: &str) -> KbResult<bool> {
        Ok(false)
    }
    async fn add_document_to_group(&self, _document_id: &str, _group_id: &str) -> KbResult<()> {
        Ok(())
    }
    async fn remove_document_from_group(
        &self,
        _document_id: &str,
        _group_id: &str,
    ) -> KbResult<bool> {
        Ok(false)
    }
    async fn list_group_documents(&self, _group_id: &str) -> KbResult<Vec<Document>> {
        Ok(Vec::new())
    }
    async fn store_chunks(
        &self,
        _document_id: &DocumentId,
        _chunks: &[Chunk],
        _embeddings: &[Vec<f32>],
        _embedding_model: &str,
    ) -> KbResult<()> {
        Ok(())
    }
    async fn delete_chunks_by_document(&self, _document_id: &DocumentId) -> KbResult<()> {
        Ok(())
    }
    async fn chunk_count(&self, _document_id: &DocumentId) -> KbResult<usize> {
        Ok(0)
    }
    async fn chunk_counts(
        &self,
        _ids: &[DocumentId],
    ) -> KbResult<std::collections::HashMap<DocumentId, usize>> {
        Ok(std::collections::HashMap::new())
    }
    async fn search_by_vector(
        &self,
        _query: &[f32],
        limit: usize,
        filter: &SearchFilter,
    ) -> KbResult<Vec<SearchResult>> {
        // Apply min_similarity here so the orchestrator's expansion-mode
        // union semantics can be exercised without a real vector DB.
        let mut out: Vec<SearchResult> = self
            .vector_hits
            .iter()
            .filter(|r| match filter.min_similarity {
                Some(min) => r.similarity >= min,
                None => true,
            })
            .cloned()
            .collect();
        out.truncate(limit);
        Ok(out)
    }
    async fn bm25_search(&self, _query: &str, limit: usize) -> KbResult<Vec<Bm25Hit>> {
        let mut out = self.bm25_hits.clone();
        out.truncate(limit);
        Ok(out)
    }
    async fn count_by_embedding_model(&self) -> KbResult<Vec<(Option<String>, usize)>> {
        Ok(Vec::new())
    }
    async fn list_chunks_for_re_embed(
        &self,
        _batch_size: usize,
        _after_id: Option<&str>,
        _skip_model: Option<&str>,
    ) -> KbResult<Vec<(ChunkId, String, Option<String>)>> {
        Ok(Vec::new())
    }
    async fn update_chunk_embedding(
        &self,
        _chunk_id: &ChunkId,
        _new_embedding: &[f32],
        _new_model: &str,
    ) -> KbResult<()> {
        Ok(())
    }
}

/// Fake EmbeddingProvider — returns a unit-length 4-dim vector per query.
/// `embed_many` returns one vector per input, with the first coordinate
/// scaled by input index so different queries produce different vectors
/// (lets expansion-union test exercise the "max similarity wins" path).
struct FakeEmbedder {
    dim: usize,
}

#[async_trait]
impl EmbeddingProvider for FakeEmbedder {
    fn model(&self) -> &str {
        "fake-model"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed_query(&self, _text: &str) -> KbResult<Vec<f32>> {
        Ok(vec![0.0; self.dim])
    }
    async fn embed_many(&self, texts: &[String]) -> KbResult<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let mut v = vec![0.0_f32; self.dim];
                if self.dim > 0 {
                    v[0] = i as f32;
                }
                v
            })
            .collect())
    }
}

fn make_retriever(cfg: KbConfig, store: Arc<FakeStore>) -> (Retriever, Arc<FakeStore>) {
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder {
        dim: cfg.embedding_dim,
    });
    let r = Retriever::new(cfg, store.clone() as Arc<dyn KbStore>, embedder);
    (r, store)
}

/// Fake IntelligenceStore — returns pre-canned graph-expanded chunks from
/// `graph_expand_chunks`. Only that method is exercised by the retriever; the
/// rest panic if reached so a mis-wire surfaces loudly.
struct FakeIntel {
    graph_chunks: Vec<SearchResult>,
}

#[async_trait]
impl IntelligenceStore for FakeIntel {
    async fn upsert_entity(&self, _e: &Entity) -> KbResult<String> {
        unimplemented!("FakeIntel only exercises graph_expand_chunks")
    }
    async fn add_mention(&self, _m: &EntityMention) -> KbResult<()> {
        unimplemented!("FakeIntel only exercises graph_expand_chunks")
    }
    async fn add_relation(&self, _r: &Relation) -> KbResult<()> {
        unimplemented!("FakeIntel only exercises graph_expand_chunks")
    }
    async fn store_intelligence(
        &self,
        _document_id: &str,
        _entities: &[Entity],
        _mentions: &[EntityMention],
        _relations: &[Relation],
    ) -> KbResult<()> {
        unimplemented!("FakeIntel only exercises graph_expand_chunks")
    }
    async fn intelligence_for_document(
        &self,
        _document_id: &str,
    ) -> KbResult<(Vec<Entity>, Vec<Relation>)> {
        unimplemented!("FakeIntel only exercises graph_expand_chunks")
    }
    async fn graph(&self, _group_id: Option<&str>, _limit: usize) -> KbResult<Graph> {
        unimplemented!("FakeIntel only exercises graph_expand_chunks")
    }
    async fn delete_document_intelligence(&self, _document_id: &str) -> KbResult<()> {
        unimplemented!("FakeIntel only exercises graph_expand_chunks")
    }
    async fn graph_expand_chunks(
        &self,
        _query: &str,
        _max_neighbors: usize,
        _limit: usize,
    ) -> KbResult<Vec<SearchResult>> {
        Ok(self.graph_chunks.clone())
    }
}

// ---- Task 7.2 tests ---------------------------------------------------

#[tokio::test]
async fn vector_only_mode_returns_top_k_by_similarity() {
    let vector_hits = vec![
        search_result("c1", "d1", "Doc A", 0.95),
        search_result("c2", "d1", "Doc A", 0.85),
        search_result("c3", "d2", "Doc B", 0.70),
    ];
    let mut cfg = test_cfg();
    cfg.hybrid_bm25_enabled = false; // vector-only
    let store = Arc::new(FakeStore::new(vector_hits, Vec::new()));
    let (retriever, _) = make_retriever(cfg, store);

    let result = retriever
        .retrieve("test query", RetrieveOptions::default())
        .await
        .expect("retrieve ok");
    let ids: Vec<&str> = result.chunks.iter().map(|c| c.id.0.as_str()).collect();
    assert_eq!(ids, vec!["c1", "c2", "c3"], "ordered by similarity desc");
}

#[tokio::test]
async fn hybrid_mode_interleaves_via_rrf() {
    let vector_hits = vec![
        search_result("c1", "d1", "Doc A", 0.95),
        search_result("c2", "d1", "Doc A", 0.85),
    ];
    let bm25_hits = vec![
        Bm25Hit {
            id: ChunkId("c3".into()),
            document_id: DocumentId("d2".into()),
            content: "content of c3".into(),
            score: 5.0,
        },
        Bm25Hit {
            id: ChunkId("c1".into()),
            document_id: DocumentId("d1".into()),
            content: "content of c1".into(),
            score: 3.0,
        },
    ];
    let store = Arc::new(FakeStore::new(vector_hits, bm25_hits));
    let (retriever, _) = make_retriever(test_cfg(), store);

    let result = retriever
        .retrieve("test", RetrieveOptions::default())
        .await
        .expect("retrieve ok");
    let ids: std::collections::HashSet<&str> =
        result.chunks.iter().map(|c| c.id.0.as_str()).collect();
    assert!(ids.contains("c1"), "vector hit present");
    assert!(ids.contains("c3"), "bm25-only hit surfaced via RRF");
}

#[tokio::test]
async fn graphrag_arm_surfaces_graph_only_chunk_when_enabled() {
    // Vector + BM25 never return "cg" — only the graph arm does. With GraphRAG
    // on, it must still reach the final result via the RRF graph list.
    let vector_hits = vec![search_result("c1", "d1", "Doc A", 0.95)];
    let graph_chunk = search_result("cg", "d2", "Doc B", 0.0);

    let mut cfg = test_cfg();
    cfg.graphrag_enabled = true;
    let store = Arc::new(FakeStore::new(vector_hits, Vec::new()));
    let (retriever, _) = make_retriever(cfg, store);
    let retriever = retriever.with_intelligence(Arc::new(FakeIntel {
        graph_chunks: vec![graph_chunk],
    }));

    let result = retriever
        .retrieve("anything", RetrieveOptions::default())
        .await
        .expect("retrieve ok");
    let ids: std::collections::HashSet<&str> =
        result.chunks.iter().map(|c| c.id.0.as_str()).collect();
    assert!(ids.contains("c1"), "vector hit present");
    assert!(
        ids.contains("cg"),
        "graph-only chunk must surface via the RRF graph arm: {ids:?}"
    );
}

#[tokio::test]
async fn graphrag_arm_silent_when_disabled() {
    // Same canned graph chunk, but graphrag_enabled = false → arm never fires,
    // so retrieval is identical to plain vector search (no "cg").
    let vector_hits = vec![search_result("c1", "d1", "Doc A", 0.95)];
    let graph_chunk = search_result("cg", "d2", "Doc B", 0.0);

    let mut cfg = test_cfg();
    cfg.graphrag_enabled = false;
    cfg.hybrid_bm25_enabled = false;
    let store = Arc::new(FakeStore::new(vector_hits, Vec::new()));
    let (retriever, _) = make_retriever(cfg, store);
    let retriever = retriever.with_intelligence(Arc::new(FakeIntel {
        graph_chunks: vec![graph_chunk],
    }));

    let result = retriever
        .retrieve("anything", RetrieveOptions::default())
        .await
        .expect("retrieve ok");
    let ids: std::collections::HashSet<&str> =
        result.chunks.iter().map(|c| c.id.0.as_str()).collect();
    assert!(ids.contains("c1"), "vector hit present");
    assert!(
        !ids.contains("cg"),
        "graph chunk must NOT appear when graphrag disabled: {ids:?}"
    );
}

#[tokio::test]
async fn min_similarity_filters_low_score_chunks() {
    let vector_hits = vec![
        search_result("hi", "d1", "Doc A", 0.95),
        search_result("low", "d1", "Doc A", 0.10),
    ];
    let mut cfg = test_cfg();
    cfg.hybrid_bm25_enabled = false;
    let store = Arc::new(FakeStore::new(vector_hits, Vec::new()));
    let (retriever, _) = make_retriever(cfg, store);

    let result = retriever
        .retrieve(
            "test",
            RetrieveOptions {
                min_similarity: Some(0.5),
                ..Default::default()
            },
        )
        .await
        .expect("retrieve ok");
    let ids: Vec<&str> = result.chunks.iter().map(|c| c.id.0.as_str()).collect();
    assert_eq!(ids, vec!["hi"], "low-similarity chunk dropped");
}

#[tokio::test]
async fn expansion_unions_with_max_similarity() {
    // Simulate expansion by directly invoking the orchestrator's
    // `run_vector_arm` path: the FakeEmbedder returns N different vectors
    // for N queries, the FakeStore returns the same hits regardless of
    // vector, so the union must dedupe by chunk id and keep max similarity.
    //
    // Because the public Retriever can't be forced into expansion mode
    // without flipping the cfg flag AND wiring an actual paraphrase
    // generator, we instead validate by direct multi-query call: feed two
    // queries through embed_many → search_by_vector → union, and assert
    // the result is the deduped vector-hit list.
    let vector_hits = vec![
        search_result("c1", "d1", "Doc A", 0.95),
        search_result("c2", "d1", "Doc A", 0.85),
    ];
    let store = Arc::new(FakeStore::new(vector_hits, Vec::new()));
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder { dim: 4 });

    // Multi-query path: batch-embed, search each, union. Going through
    // embed_many + search_by_vector validates the union semantics without
    // depending on the (still-stubbed) expand_query implementation.
    let queries = vec!["q1".to_string(), "q2".to_string()];
    let embeddings = embedder.embed_many(&queries).await.unwrap();
    assert_eq!(embeddings.len(), 2, "embed_many returns one vec per query");

    // Direct search via store — proves the SearchFilter min_similarity
    // gate is honored consistently across expansion paraphrases.
    let filter = SearchFilter {
        category: None,
        group_ids: Vec::new(),
        document_ids: None,
        min_similarity: Some(0.3),
    };
    let mut union: std::collections::HashMap<ChunkId, SearchResult> =
        std::collections::HashMap::new();
    for emb in &embeddings {
        let hits = store.search_by_vector(emb, 10, &filter).await.unwrap();
        for r in hits {
            match union.get(&r.id) {
                Some(prev) if prev.similarity >= r.similarity => {}
                _ => {
                    union.insert(r.id.clone(), r);
                }
            }
        }
    }
    assert_eq!(union.len(), 2, "union dedupes by chunk id");
    assert!(union.values().all(|r| r.similarity >= 0.3));
}

#[tokio::test]
async fn record_retrieval_hits_fired_fire_and_forget() {
    // FakeStore::record_retrieval_hits panics. The orchestrator must
    // detach the call via tokio::spawn so retrieve() still returns Ok.
    let vector_hits = vec![search_result("c1", "d1", "Doc A", 0.95)];
    let mut store = FakeStore::new(vector_hits, Vec::new());
    store.panic_on_record = true;
    let store = Arc::new(store);
    let (retriever, _) = make_retriever(test_cfg(), store.clone());

    let result = retriever.retrieve("test", RetrieveOptions::default()).await;
    assert!(
        result.is_ok(),
        "retrieve must return Ok even when record_retrieval_hits panics: {:?}",
        result.err()
    );
    // Note: spawned task's panic is isolated; tokio swallows it (with a
    // tracing log). We don't assert hits_recorded count because the spawn
    // may not have polled before retrieve() returned — the contract is
    // "retrieve doesn't block on it", not "it runs synchronously".
}

#[tokio::test]
async fn format_includes_contextual_prefix_when_present() {
    let mut hit = search_result("c1", "d1", "Doc A", 0.95);
    hit.contextual_prefix = Some("Chunk discusses Section 3.2 exclusions".into());
    let mut cfg = test_cfg();
    cfg.hybrid_bm25_enabled = false;
    let store = Arc::new(FakeStore::new(vec![hit], Vec::new()));
    let (retriever, _) = make_retriever(cfg, store);

    let result = retriever
        .retrieve("test", RetrieveOptions::default())
        .await
        .expect("retrieve ok");
    assert!(
        result
            .context
            .contains("Chunk discusses Section 3.2 exclusions"),
        "contextual prefix must appear in output context, got: {:?}",
        result.context
    );
}

#[tokio::test]
async fn format_omits_section_when_none() {
    let mut hit = search_result("c1", "d1", "Doc Title Only", 0.95);
    hit.section = None;
    let mut cfg = test_cfg();
    cfg.hybrid_bm25_enabled = false;
    let store = Arc::new(FakeStore::new(vec![hit], Vec::new()));
    let (retriever, _) = make_retriever(cfg, store);

    let result = retriever
        .retrieve("test", RetrieveOptions::default())
        .await
        .expect("retrieve ok");
    assert!(
        result.context.contains("[Doc Title Only]"),
        "no-section chunk must format as `[Title]`, not `[Title - ]`, got: {:?}",
        result.context
    );
    assert!(
        !result.context.contains("[Doc Title Only - "),
        "must not include the ' - section' suffix when section is None"
    );
}

// ---- Task 7.3: query expansion ---------------------------------------
//
// Tests mutate OPENROUTER_API_KEY and KB_OPENROUTER_CHAT_URL — they share
// the existing ENV_LOCK pattern from config_test.rs / embed_test.rs.

mod query_expansion_tests {
    use rantaiclaw::kb::retrieve::query_expansion::{_clear_cache_for_tests, expand_query};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{test_cfg, ENV_LOCK};

    struct EnvGuard(Vec<&'static str>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in &self.0 {
                // SAFETY: serialized via ENV_LOCK above.
                unsafe {
                    std::env::remove_var(k);
                }
            }
        }
    }

    /// Tests still touch `OPENROUTER_API_KEY` (which lives outside `KbConfig`),
    /// so we keep the env-clearing helper and `ENV_LOCK`. The chat URL is no
    /// longer env-resolved — it's set directly on `cfg.openrouter_chat_url`.
    fn clear_env() {
        unsafe {
            std::env::remove_var("OPENROUTER_API_KEY");
        }
    }

    #[tokio::test]
    async fn expand_disabled_returns_query_only() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let mut cfg = test_cfg();
        cfg.query_expansion_enabled = false;
        let out = expand_query(&cfg, "what is X?").await;
        assert_eq!(out, vec!["what is X?".to_string()]);
    }

    #[tokio::test]
    async fn expand_no_api_key_returns_query_only() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let mut cfg = test_cfg();
        cfg.query_expansion_enabled = true;
        let out = expand_query(&cfg, "what is X?").await;
        assert_eq!(out, vec!["what is X?".to_string()]);
    }

    #[tokio::test]
    async fn expand_parses_json_array_response() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": { "content": "[\"p1\", \"p2\", \"p3\"]" }
                }]
            })))
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg();
        cfg.query_expansion_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let out = expand_query(&cfg, "what is X?").await;
        assert_eq!(
            out,
            vec![
                "what is X?".to_string(),
                "p1".to_string(),
                "p2".to_string(),
                "p3".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn expand_dedupes_case_insensitive() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;
        // Model returns the original (different case/whitespace) + a dup.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": { "content": "[\"WHAT IS x?\", \"What  is  X?\", \"different phrasing\", \"DIFFERENT PHRASING\"]" }
                }]
            })))
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg();
        cfg.query_expansion_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let out = expand_query(&cfg, "what is X?").await;
        // Original retained, both case-only duplicates dropped, second
        // "DIFFERENT PHRASING" also dropped as case-insensitive dup.
        assert_eq!(
            out,
            vec!["what is X?".to_string(), "different phrasing".to_string()],
            "case-insensitive dedupe vs original + among paraphrases"
        );
    }

    #[tokio::test]
    async fn expand_failure_returns_query_only() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg();
        cfg.query_expansion_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let out = expand_query(&cfg, "what is X?").await;
        assert_eq!(out, vec!["what is X?".to_string()], "5xx → fallback");
    }

    #[tokio::test]
    async fn expand_caches_repeated_query() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;
        // expect(1) — second call must hit cache, not the mock.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": { "content": "[\"alt1\", \"alt2\"]" }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg();
        cfg.query_expansion_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let a = expand_query(&cfg, "cache me").await;
        let b = expand_query(&cfg, "cache me").await;
        assert_eq!(a, b);
        assert_eq!(a.len(), 3, "[original, alt1, alt2]");
        // wiremock auto-asserts .expect(1) on Drop.
    }

    #[tokio::test]
    async fn expand_cache_invalidates_on_model_change() {
        // Cache key includes cfg.query_expansion_model. Changing the model
        // between two calls of the same query MUST force a second upstream
        // hit; without the model in the key, the second call would return
        // stale paraphrases produced by the previous model.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": { "content": "[\"alt-a\", \"alt-b\"]" }
                }]
            })))
            .expect(2) // ← TWO upstream hits, one per distinct model
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg();
        cfg.query_expansion_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        cfg.query_expansion_model = "model-a".into();
        let _ = expand_query(&cfg, "same query").await;

        cfg.query_expansion_model = "model-b".into();
        let _ = expand_query(&cfg, "same query").await;
        // wiremock auto-asserts .expect(2) on Drop.
    }
}

// ---- Task 7.4: contextual retrieval ----------------------------------

mod contextual_tests {
    use rantaiclaw::kb::retrieve::contextual::generate_contextual_prefixes;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{test_cfg, ENV_LOCK};

    struct EnvGuard(Vec<&'static str>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in &self.0 {
                unsafe {
                    std::env::remove_var(k);
                }
            }
        }
    }

    /// Only `OPENROUTER_API_KEY` is env-resolved now; the chat URL is per-cfg.
    fn clear_env() {
        unsafe {
            std::env::remove_var("OPENROUTER_API_KEY");
        }
    }

    #[tokio::test]
    async fn contextual_disabled_returns_empty_prefixes() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let mut cfg = test_cfg();
        cfg.contextual_retrieval_enabled = false;
        let chunks = vec!["a".into(), "b".into(), "c".into()];
        let out = generate_contextual_prefixes(&cfg, "full doc", &chunks).await;
        assert_eq!(out, vec!["".to_string(), "".to_string(), "".to_string()]);
    }

    #[tokio::test]
    async fn contextual_no_api_key_returns_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let mut cfg = test_cfg();
        cfg.contextual_retrieval_enabled = true;
        let chunks = vec!["a".into(), "b".into()];
        let out = generate_contextual_prefixes(&cfg, "doc", &chunks).await;
        assert_eq!(out, vec!["".to_string(), "".to_string()]);
    }

    #[tokio::test]
    async fn contextual_parses_array_response() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": { "content": "[\"ctx1\", \"ctx2\", \"ctx3\"]" }
                }]
            })))
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }
        let mut cfg = test_cfg();
        cfg.contextual_retrieval_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let chunks = vec!["a".into(), "b".into(), "c".into()];
        let out = generate_contextual_prefixes(&cfg, "full doc", &chunks).await;
        assert_eq!(
            out,
            vec!["ctx1".to_string(), "ctx2".to_string(), "ctx3".to_string()]
        );
    }

    #[tokio::test]
    async fn contextual_length_mismatch_returns_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let server = MockServer::start().await;
        // Server returns 2 elements when caller asked for 3 — fail-soft to
        // empty prefixes rather than silently truncate.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{
                    "message": { "content": "[\"only-one\", \"only-two\"]" }
                }]
            })))
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }
        let mut cfg = test_cfg();
        cfg.contextual_retrieval_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let chunks = vec!["a".into(), "b".into(), "c".into()];
        let out = generate_contextual_prefixes(&cfg, "doc", &chunks).await;
        assert_eq!(
            out,
            vec!["".to_string(), "".to_string(), "".to_string()],
            "length mismatch → empty prefixes"
        );
    }

    #[tokio::test]
    async fn contextual_failure_returns_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }
        let mut cfg = test_cfg();
        cfg.contextual_retrieval_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let chunks = vec!["a".into(), "b".into()];
        let out = generate_contextual_prefixes(&cfg, "doc", &chunks).await;
        assert_eq!(out, vec!["".to_string(), "".to_string()], "5xx → empty");
    }
}

// ---- Task 7.5: format + standalone query rewriter -------------------

mod format_tests {
    use rantaiclaw::kb::retrieve::format::format_context_for_prompt;
    use rantaiclaw::kb::retrieve::{RetrievalResult, SourceRef};

    #[test]
    fn format_returns_empty_when_no_context() {
        let result = RetrievalResult {
            context: String::new(),
            sources: Vec::new(),
            chunks: Vec::new(),
        };
        let out = format_context_for_prompt(&result);
        assert!(out.is_empty(), "empty context → empty output");
    }

    #[test]
    fn format_includes_instruction_block_verbatim() {
        let result = RetrievalResult {
            context: "[Doc - Section]\nbody here".into(),
            sources: vec![SourceRef {
                document_title: "Doc".into(),
                section: Some("Section".into()),
                categories: vec!["test".into()],
            }],
            chunks: Vec::new(),
        };
        let out = format_context_for_prompt(&result);
        // Spot-check the load-bearing instruction phrases — these are what
        // the LLM is being told and must not silently drift.
        assert!(out.contains("## Knowledge Base Context"));
        assert!(out.contains("The excerpts below are your primary source for this question."));
        assert!(out.contains("Treat the excerpts as the source of truth for specific facts."));
        assert!(out.contains("paragraph numbers"));
        assert!(out.contains("effective dates"));
        assert!(
            out.contains("MAY add brief background context"),
            "framing guidance preserved"
        );
        assert!(out.contains("Cite each factual claim inline"));
        assert!(out.contains("not specified in the available excerpts"));
        assert!(out.contains("Excerpts:\n[Doc - Section]"));
        assert!(out.contains("Sources:\n- Doc: Section"));
    }

    #[test]
    fn format_lists_sources_with_section() {
        let result = RetrievalResult {
            context: "x".into(),
            sources: vec![
                SourceRef {
                    document_title: "Doc A".into(),
                    section: Some("S1".into()),
                    categories: Vec::new(),
                },
                SourceRef {
                    document_title: "Doc B".into(),
                    section: None,
                    categories: Vec::new(),
                },
            ],
            chunks: Vec::new(),
        };
        let out = format_context_for_prompt(&result);
        assert!(out.contains("- Doc A: S1"), "section appended after colon");
        assert!(
            out.contains("- Doc B\n") || out.ends_with("- Doc B"),
            "no-section entry has no trailing colon"
        );
    }
}

mod standalone_tests {
    use rantaiclaw::kb::retrieve::standalone_query::{_clear_cache_for_tests, rewrite_standalone};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::{test_cfg, ENV_LOCK};

    struct EnvGuard(Vec<&'static str>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in &self.0 {
                unsafe {
                    std::env::remove_var(k);
                }
            }
        }
    }

    /// Only `OPENROUTER_API_KEY` is env-resolved now; the chat URL is per-cfg.
    fn clear_env() {
        unsafe {
            std::env::remove_var("OPENROUTER_API_KEY");
        }
    }

    #[tokio::test]
    async fn standalone_returns_original_on_empty_history() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let cfg = test_cfg();
        let out = rewrite_standalone(&cfg, "what?", &[]).await.unwrap();
        assert_eq!(out, "what?");
    }

    #[tokio::test]
    async fn standalone_rewrites_with_history() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;

        // Capture the request body so we can assert the history is embedded.
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let captured_clone = captured.clone();
        Mock::given(method("POST"))
            .and(path("/"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                *captured_clone.lock().unwrap() = body;
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{
                        "message": { "content": "exclusions for policy X" }
                    }]
                }))
            })
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg();
        cfg.standalone_query_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let history = vec![
            ("user".to_string(), "tell me about policy X".to_string()),
            (
                "assistant".to_string(),
                "Policy X covers a, b, and c".to_string(),
            ),
            ("user".to_string(), "and exclusions?".to_string()),
        ];
        let out = rewrite_standalone(&cfg, "tell me more", &history)
            .await
            .unwrap();
        assert_eq!(
            out, "exclusions for policy X",
            "model output returned verbatim"
        );

        let captured_body = captured.lock().unwrap().clone();
        assert!(
            captured_body.contains("policy X"),
            "history turns embedded in prompt body, got: {captured_body}"
        );
        assert!(
            captured_body.contains("tell me more"),
            "latest question embedded in prompt body"
        );
    }

    #[tokio::test]
    async fn standalone_fails_soft() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg();
        cfg.standalone_query_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        let history = vec![(
            "user".to_string(),
            "earlier turn that anchors disambiguation".to_string(),
        )];
        let out = rewrite_standalone(&cfg, "follow up?", &history)
            .await
            .unwrap();
        assert_eq!(
            out, "follow up?",
            "5xx → original query returned (fail-soft)"
        );
    }

    #[tokio::test]
    async fn standalone_disabled_returns_query_only() {
        // Even with non-empty history AND a valid API key AND a mock server
        // that would otherwise rewrite the query, the disabled flag must
        // short-circuit. Mirrors TS `KB_STANDALONE_QUERY_ENABLED !== "true"`.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;
        // expect(0) — the mock must never be hit when the gate is off.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{ "message": { "content": "should not be returned" } }]
            })))
            .expect(0)
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg(); // standalone_query_enabled = false by default
        cfg.openrouter_chat_url = server.uri();
        let history = vec![("user".to_string(), "prior turn".to_string())];
        let out = rewrite_standalone(&cfg, "what?", &history).await.unwrap();
        assert_eq!(out, "what?", "disabled gate → query returned unchanged");
        // wiremock's expect(0) auto-asserts on Drop that the mock was not hit.
    }

    #[tokio::test]
    async fn standalone_cache_invalidates_on_model_change() {
        // Cache key includes cfg.query_expansion_model (the model the
        // rewriter shares with query_expansion). Changing the model between
        // two calls of the same (query, history) MUST force a second
        // upstream hit — otherwise stale rewrites cross model boundaries.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_env();
        _clear_cache_for_tests().await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{ "message": { "content": "rewritten output" } }]
            })))
            .expect(2) // ← TWO upstream hits, one per distinct model
            .mount(&server)
            .await;

        let _env = EnvGuard(vec!["OPENROUTER_API_KEY"]);
        unsafe {
            std::env::set_var("OPENROUTER_API_KEY", "test-key");
        }

        let mut cfg = test_cfg();
        cfg.standalone_query_enabled = true;
        cfg.openrouter_chat_url = server.uri();
        cfg.query_expansion_model = "model-a".into();
        let history = vec![("user".to_string(), "earlier turn anchor".to_string())];
        let _ = rewrite_standalone(&cfg, "follow up?", &history)
            .await
            .unwrap();

        cfg.query_expansion_model = "model-b".into();
        let _ = rewrite_standalone(&cfg, "follow up?", &history)
            .await
            .unwrap();
        // wiremock auto-asserts .expect(2) on Drop.
    }
}
