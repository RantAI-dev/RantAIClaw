//! Criterion benchmark for the KB retrieval pipeline.
//!
//! Measures end-to-end latency of `Retriever::retrieve` against a
//! pre-seeded `SqliteStore` for a fixed set of representative queries.
//! Uses a deterministic in-process `FakeEmbedder` so the bench measures
//! the **Rust pipeline glue** (chunk pool build + RRF fusion + SQL +
//! prompt assembly) instead of OpenRouter network latency, which is
//! identical between the Rust and TS retrievers.
//!
//! Run:
//!
//! ```bash
//! cargo bench --features kb --bench kb_retrieval
//! ```
//!
//! Output is captured in `docs/kb-bench.md`. Compare against the TS
//! retriever by running `pnpm run bench:rag` from the parent repo (out
//! of scope for this commit; see `docs/kb-bench.md` for the deferred
//! comparison plan).

#![cfg(feature = "kb")]

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::TempDir;
use tokio::runtime::Runtime;

use rantaiclaw::kb::chunk::prepare::prepare_chunk_for_embedding;
use rantaiclaw::kb::chunk::{smart_chunk_document, SmartChunkOptions};
use rantaiclaw::kb::embed::EmbeddingProvider;
use rantaiclaw::kb::retrieve::{RetrieveOptions, Retriever};
use rantaiclaw::kb::store::sqlite::SqliteStore;
use rantaiclaw::kb::store::KbStore;
use rantaiclaw::kb::{Document, DocumentId, KbConfig, KbResult};

const DIM: usize = 256;
const CORPUS_DOC_COUNT: usize = 50;
const QUERIES: &[&str] = &[
    "What is document title 7?",
    "Find the chapter about chapter 12 in the corpus",
    "How does document 23 relate to document 24?",
    "Summarize document 41 in three points",
    "Search for the term widget across all documents",
    "Compare document 5 and document 15 side by side",
    "Where is the introduction defined?",
    "Explain the structure of document 30",
    "Look up the reference for document 49",
    "Cross-reference document 1 with document 50",
];

/// Deterministic in-process embedder. Hashes the input text into a
/// stable vector so cosine similarity ordering is reproducible across
/// runs. NOT semantically meaningful — used purely to feed the
/// pipeline so we can measure pipeline cost without network noise.
struct FakeEmbedder;

#[async_trait]
impl EmbeddingProvider for FakeEmbedder {
    fn model(&self) -> &str {
        "bench/fake-embedder"
    }
    fn dim(&self) -> usize {
        DIM
    }
    async fn embed_query(&self, text: &str) -> KbResult<Vec<f32>> {
        Ok(deterministic_vec(text))
    }
    async fn embed_many(&self, texts: &[String]) -> KbResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| deterministic_vec(t)).collect())
    }
}

/// Hash-seeded f32 vector. Splits the SipHash-style 64-bit state into
/// `DIM` pseudo-random f32 values in roughly `[-1.0, 1.0]`.
fn deterministic_vec(text: &str) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    let seed = h.finish();

    let mut out = Vec::with_capacity(DIM);
    let mut state = seed;
    for _ in 0..DIM {
        // Splitmix-style step: cheap, deterministic, good enough spread.
        state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(0x1);
        let bits = (state >> 32) as u32;
        let normalized = (bits as f32 / u32::MAX as f32) * 2.0 - 1.0;
        out.push(normalized);
    }
    // L2-normalize so cosine similarity stays well-conditioned.
    let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut out {
            *x /= norm;
        }
    }
    out
}

fn bench_cfg() -> KbConfig {
    KbConfig {
        extract_primary: "smart".into(),
        extract_fallback: "unpdf".into(),
        extract_smart_fallback: "bench/fake".into(),
        embedding_model: "bench/fake-embedder".into(),
        embedding_dim: DIM,
        default_max_chunks: 8,
        rerank_enabled: false,
        rerank_provider: String::new(),
        rerank_model: "bench/fake".into(),
        rerank_initial_k: 20,
        rerank_final_k: 5,
        hybrid_bm25_enabled: true,
        contextual_retrieval_enabled: false,
        contextual_retrieval_model: "bench/fake".into(),
        query_expansion_enabled: false,
        query_expansion_model: "bench/fake".into(),
        query_expansion_paraphrases: 3,
        standalone_query_enabled: false,
        extract_vision_base_url: String::new(),
        extract_vision_api_key: String::new(),
        extract_mineru_base_url: String::new(),
        embedding_base_url: "bench://fake".into(),
        embedding_api_key: String::new(),
        embed_batch_size: 128,
        embed_concurrency: 4,
        query_embed_cache_size: 256,
        query_embed_cache_ttl_ms: 5 * 60 * 1_000,
        openrouter_chat_url: "bench://fake".into(),
        intelligence_enabled: false,
        intelligence_model: "openai/gpt-4.1-nano".into(),
        intelligence_resolution: "exact".into(),
        graph_max_nodes: 200,
        graphrag_enabled: false,
        graphrag_max_neighbors: 20,
    }
}

/// Build a synthetic corpus, ingest into a fresh SqliteStore, return the
/// Retriever ready for benchmarking. Returns the temp dir so it survives
/// the bench's lifetime.
async fn setup() -> (Retriever, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let cfg = bench_cfg();
    let store: Arc<dyn KbStore> = Arc::new(
        SqliteStore::open(tmp.path().join("kb.db"), DIM)
            .await
            .expect("open SqliteStore"),
    );
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbedder);

    for i in 0..CORPUS_DOC_COUNT {
        let title = format!("document title {i}");
        // Repeating the title 20 times gives smart_chunk_document enough
        // body to produce 1-2 chunks per doc, mirroring the parity
        // corpus shape.
        let body = format!(
            "{title}\n\n{}",
            (0..20)
                .map(|j| format!("Content paragraph {j} of {title}."))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
        let doc = Document {
            id: DocumentId(format!("bench-doc-{i:04}")),
            title: title.clone(),
            content: body.clone(),
            categories: vec!["BENCH".into()],
            subcategory: None,
            metadata: serde_json::json!({}),
            s3_key: None,
            file_type: Some("markdown".into()),
            mime_type: Some("text/markdown".into()),
            file_size: Some(body.len() as u64),
            organization_id: Some("rantaiclaw_bench_org".into()),
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
        store.create_document(&doc).await.expect("create_document");

        let chunks =
            smart_chunk_document(&body, &title, "BENCH", None, SmartChunkOptions::default());
        if chunks.is_empty() {
            continue;
        }
        let texts: Vec<String> = chunks.iter().map(prepare_chunk_for_embedding).collect();
        let embeddings = embedder.embed_many(&texts).await.expect("embed_many");
        store
            .store_chunks(&doc.id, &chunks, &embeddings, embedder.model())
            .await
            .expect("store_chunks");
    }

    let retriever = Retriever::new(cfg, store, embedder);
    (retriever, tmp)
}

fn bench_retrieve(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let (retriever, _tmp) = rt.block_on(setup());
    // Keep `_tmp` alive across the whole bench — Drop closes the SQLite
    // file handle, so leaking it via the binding extends the corpus
    // lifetime to the end of `bench_retrieve`.

    let mut group = c.benchmark_group("kb_retrieval");
    // Criterion's default sample size (100) is fine for this workload —
    // each retrieve completes in <5ms so the whole group finishes in a
    // few seconds.
    group.sample_size(50);

    for (idx, query) in QUERIES.iter().enumerate() {
        group.bench_with_input(BenchmarkId::new("retrieve_top8", idx), query, |b, &q| {
            b.to_async(&rt).iter(|| async {
                retriever
                    .retrieve(
                        q,
                        RetrieveOptions {
                            max_chunks: Some(8),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("retrieve")
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_retrieve);
criterion_main!(benches);
