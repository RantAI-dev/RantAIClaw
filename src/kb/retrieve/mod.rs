//! Retrieval pipeline — query expansion, vector + BM25 search, RRF fusion,
//! optional rerank, and prompt-context formatting.
//!
//! Port of `src/lib/rag/retriever.ts`, `hybrid-merge.ts`, `query-expansion.ts`,
//! `contextual-retrieval.ts`, and `standalone-query.ts`. Sub-modules mirror the
//! TS surface 1:1 so the port stays line-by-line auditable.

pub mod rrf;

pub use rrf::{reciprocal_rank_fusion, RrfOptions, RrfResult};
