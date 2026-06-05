//! Retrieval pipeline — query expansion, vector + BM25 search, RRF fusion,
//! optional rerank, and prompt-context formatting.
//!
//! Port of `src/lib/rag/retriever.ts`, `hybrid-merge.ts`, `query-expansion.ts`,
//! `contextual-retrieval.ts`, and `standalone-query.ts`. Sub-modules mirror the
//! TS surface 1:1 so the port stays line-by-line auditable.

pub mod contextual;
pub mod format;
pub mod query_expansion;
pub mod rrf;
pub mod standalone_query;

pub use query_expansion::expand_query;
pub use rrf::{reciprocal_rank_fusion, RrfOptions};

use std::collections::HashMap;
use std::sync::Arc;

use crate::kb::embed::EmbeddingProvider;
use crate::kb::rerank::{Candidate, Reranker};
use crate::kb::store::{KbStore, SearchFilter};
use crate::kb::{ChunkId, DocumentId, KbConfig, KbResult, SearchResult};

/// Per-call retrieval overrides. Fields are `Option` so the orchestrator can
/// fall back to `cfg.default_max_chunks` etc.
#[derive(Debug, Clone, Default)]
pub struct RetrieveOptions {
    pub min_similarity: Option<f32>,
    pub max_chunks: Option<usize>,
    pub category_filter: Option<String>,
    pub group_ids: Vec<String>,
}

/// One unique source surfaced in the final result, keyed by `(title, section)`.
#[derive(Debug, Clone)]
pub struct SourceRef {
    pub document_title: String,
    pub section: Option<String>,
    pub categories: Vec<String>,
}

/// Final output of [`Retriever::retrieve`].
#[derive(Debug, Clone)]
pub struct RetrievalResult {
    /// LLM-ready context block — `[Title - Section]\n{prefix}\n\n{content}`
    /// sections joined by `\n\n---\n\n`.
    pub context: String,
    pub sources: Vec<SourceRef>,
    pub chunks: Vec<SearchResult>,
}

/// Default min_similarity matches the TS source (`retriever.ts:59`) — chunks
/// below 0.30 cosine are dropped before they ever reach the LLM.
const DEFAULT_MIN_SIMILARITY: f32 = 0.30;

/// Orchestrator that ties together: query expansion → vector + BM25 search in
/// parallel → RRF → optional rerank → prompt formatting. Mirrors
/// `retrieveContext` in `src/lib/rag/retriever.ts`.
pub struct Retriever {
    pub cfg: KbConfig,
    pub store: Arc<dyn KbStore>,
    pub embedder: Arc<dyn EmbeddingProvider>,
    /// Optional reranker. `None` skips the rerank stage entirely; `Some(_)`
    /// activates the "top-fetch_limit → top-max_chunks" reorder when the
    /// fused set is larger than `max_chunks`.
    pub reranker: Option<Arc<dyn Reranker>>,
}

impl Retriever {
    pub fn new(
        cfg: KbConfig,
        store: Arc<dyn KbStore>,
        embedder: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            cfg,
            store,
            embedder,
            reranker: None,
        }
    }

    /// Attach a reranker. Builder-style so callers can construct then opt-in
    /// without juggling intermediate `Option`s.
    #[must_use]
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = Some(reranker);
        self
    }

    /// End-to-end retrieval. Returns an empty [`RetrievalResult`] when no
    /// chunk crosses the similarity threshold — never throws on empty input.
    pub async fn retrieve(&self, query: &str, opts: RetrieveOptions) -> KbResult<RetrievalResult> {
        let min_similarity = opts.min_similarity.unwrap_or(DEFAULT_MIN_SIMILARITY);
        let max_chunks = opts.max_chunks.unwrap_or(self.cfg.default_max_chunks);

        // Rerank pulls a wider pool (rerank_initial_k or max_chunks, whichever
        // is larger) so the reranker has enough candidates to shuffle.
        let fetch_limit = if self.reranker.is_some() {
            self.cfg.rerank_initial_k.max(max_chunks)
        } else {
            max_chunks
        };

        // Query expansion — opt-in, fail-soft. Returns `[query]` when disabled.
        let expanded = expand_query(&self.cfg, query).await;

        let filter = SearchFilter {
            category: opts.category_filter.clone(),
            group_ids: opts.group_ids.clone(),
            document_ids: None,
            min_similarity: Some(min_similarity),
        };

        // Vector arm and BM25 arm run concurrently — max, not sum, of the two
        // latencies. tokio::join! polls both to completion before returning.
        let vector_fut = self.run_vector_arm(&expanded, fetch_limit, min_similarity, &filter);
        let bm25_fut = self.run_bm25_arm(query, fetch_limit);

        let (vector_chunks, bm25_chunks) = tokio::join!(vector_fut, bm25_fut);
        let vector_chunks = vector_chunks?;
        // BM25 is fail-soft per the TS source — store error never fails the
        // whole retrieval.
        let bm25_chunks = bm25_chunks.unwrap_or_default();

        // Build chunk pool: vector wins metadata ties. BM25-only hits get
        // synthesized SearchResult records with empty title/categories so
        // they're addressable but visually distinguishable in the output.
        let mut pool: HashMap<ChunkId, SearchResult> = HashMap::new();
        for v in &vector_chunks {
            pool.insert(v.id.clone(), v.clone());
        }
        for b in &bm25_chunks {
            pool.entry(b.id.clone()).or_insert_with(|| SearchResult {
                id: b.id.clone(),
                document_id: b.document_id.clone(),
                document_title: String::new(),
                content: b.content.clone(),
                categories: Vec::new(),
                subcategory: None,
                section: None,
                similarity: 0.0,
                contextual_prefix: None,
            });
        }

        // RRF only when BM25 actually produced hits — otherwise just rank by
        // vector similarity directly.
        let fused_ids: Vec<ChunkId> = if self.cfg.hybrid_bm25_enabled && !bm25_chunks.is_empty() {
            let v_list: Vec<(String, ())> =
                vector_chunks.iter().map(|c| (c.id.0.clone(), ())).collect();
            let b_list: Vec<(String, ())> =
                bm25_chunks.iter().map(|c| (c.id.0.clone(), ())).collect();
            let fused = reciprocal_rank_fusion(
                &[v_list.as_slice(), b_list.as_slice()],
                RrfOptions {
                    k: 60,
                    limit: Some(fetch_limit),
                },
            );
            fused.into_iter().map(|r| ChunkId(r.id)).collect()
        } else {
            vector_chunks.iter().map(|c| c.id.clone()).collect()
        };

        let mut chunks: Vec<SearchResult> = fused_ids
            .iter()
            .filter_map(|id| pool.get(id).cloned())
            .collect();

        // Optional rerank stage — only fires when reranker is configured AND
        // the fused set is larger than max_chunks (no point reordering a set
        // that already fits).
        chunks = self.apply_rerank(query, chunks, max_chunks).await;
        chunks.truncate(max_chunks);

        if chunks.is_empty() {
            return Ok(RetrievalResult {
                context: String::new(),
                sources: Vec::new(),
                chunks: Vec::new(),
            });
        }

        // Coverage analytics: fire-and-forget. tokio::spawn detaches the
        // store call so retrieve() returns even if the store hangs or panics.
        // Mirrors the TS source's `void import(...).then(...).catch(()=>{})`.
        let doc_ids: Vec<DocumentId> = chunks.iter().map(|c| c.document_id.clone()).collect();
        let store = self.store.clone();
        tokio::spawn(async move {
            // Errors deliberately swallowed — analytics must never affect the
            // chat path. Tracing surfaces it for ops without blocking.
            if let Err(e) = store.record_retrieval_hits(&doc_ids).await {
                tracing::debug!(target: "kb::retrieve", error = %e, "record_retrieval_hits failed (fire-and-forget)");
            }
        });

        // Build prompt context. Each chunk gets `[Title - Section]\n{prefix?}\n\n{content}`,
        // joined by `\n\n---\n\n`.
        let mut parts: Vec<String> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let source = match &chunk.section {
                Some(s) => format!("[{} - {}]", chunk.document_title, s),
                None => format!("[{}]", chunk.document_title),
            };
            let prefix = match &chunk.contextual_prefix {
                Some(p) if !p.is_empty() => format!("{p}\n\n"),
                _ => String::new(),
            };
            parts.push(format!("{source}\n{prefix}{}", chunk.content));
        }
        let context = parts.join("\n\n---\n\n");

        // Unique sources keyed by (title, section). Preserves insertion order
        // so the source list mirrors the order chunks appear in `context`.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut sources: Vec<SourceRef> = Vec::new();
        for chunk in &chunks {
            let key = format!(
                "{}-{}",
                chunk.document_title,
                chunk.section.as_deref().unwrap_or("")
            );
            if seen.insert(key) {
                sources.push(SourceRef {
                    document_title: chunk.document_title.clone(),
                    section: chunk.section.clone(),
                    categories: chunk.categories.clone(),
                });
            }
        }

        Ok(RetrievalResult {
            context,
            sources,
            chunks,
        })
    }

    /// Currently an alias for [`retrieve`]. Reserved for future intent
    /// classification + query rewriting (TS source has both; deferred per
    /// plan §"Out of scope").
    pub async fn smart_retrieve(
        &self,
        query: &str,
        opts: RetrieveOptions,
    ) -> KbResult<RetrievalResult> {
        self.retrieve(query, opts).await
    }

    /// Vector search arm. Single-query path uses `embed_query` + a single
    /// `search_by_vector`; multi-query (expansion) path batch-embeds and
    /// unions results keeping the max similarity per chunk id.
    async fn run_vector_arm(
        &self,
        queries: &[String],
        fetch_limit: usize,
        min_similarity: f32,
        filter: &SearchFilter,
    ) -> KbResult<Vec<SearchResult>> {
        if queries.is_empty() {
            return Ok(Vec::new());
        }
        if queries.len() == 1 {
            let vec = self.embedder.embed_query(&queries[0]).await?;
            return self.store.search_by_vector(&vec, fetch_limit, filter).await;
        }
        // Batch-embed every paraphrase, search each, then union by id keeping
        // the highest similarity. Mirrors `searchSimilarBatch` semantics.
        let embeddings = self.embedder.embed_many(queries).await?;
        let mut union: HashMap<ChunkId, SearchResult> = HashMap::new();
        for emb in &embeddings {
            let results = self
                .store
                .search_by_vector(emb, fetch_limit, filter)
                .await?;
            for r in results {
                if r.similarity < min_similarity {
                    continue;
                }
                match union.get(&r.id) {
                    Some(prev) if prev.similarity >= r.similarity => {}
                    _ => {
                        union.insert(r.id.clone(), r);
                    }
                }
            }
        }
        let mut merged: Vec<SearchResult> = union.into_values().collect();
        // Sort by similarity desc — deterministic ordering across runs.
        merged.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged.truncate(fetch_limit);
        Ok(merged)
    }

    /// BM25 lexical arm. Fail-soft: on store error, returns empty list and
    /// logs at debug level — caller treats absence of BM25 hits as "vector
    /// only" mode.
    async fn run_bm25_arm(
        &self,
        query: &str,
        fetch_limit: usize,
    ) -> KbResult<Vec<crate::kb::store::Bm25Hit>> {
        if !self.cfg.hybrid_bm25_enabled {
            return Ok(Vec::new());
        }
        match self.store.bm25_search(query, fetch_limit).await {
            Ok(hits) => Ok(hits),
            Err(e) => {
                tracing::debug!(target: "kb::retrieve", error = %e, "bm25 arm failed, falling back to vector-only");
                Ok(Vec::new())
            }
        }
    }

    /// Apply the reranker when configured AND the fused set exceeds
    /// `max_chunks`. On reranker error, falls back to the upstream fused
    /// order (sliced to `max_chunks` by the caller).
    async fn apply_rerank(
        &self,
        query: &str,
        chunks: Vec<SearchResult>,
        max_chunks: usize,
    ) -> Vec<SearchResult> {
        let Some(reranker) = self.reranker.as_ref() else {
            return chunks;
        };
        if chunks.len() <= max_chunks {
            return chunks;
        }
        let candidates: Vec<Candidate> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| Candidate {
                id: c.id.0.clone(),
                text: c.content.clone(),
                original_rank: i,
                original_score: c.similarity,
            })
            .collect();
        match reranker.rerank(query, &candidates, max_chunks).await {
            Ok(ranked) => {
                let by_id: HashMap<String, SearchResult> =
                    chunks.into_iter().map(|c| (c.id.0.clone(), c)).collect();
                ranked
                    .into_iter()
                    .filter_map(|r| by_id.get(&r.id).cloned())
                    .collect()
            }
            Err(e) => {
                tracing::warn!(
                    target: "kb::retrieve",
                    reranker = reranker.name(),
                    error = %e,
                    "rerank failed, falling back to fused order",
                );
                chunks
            }
        }
    }
}
