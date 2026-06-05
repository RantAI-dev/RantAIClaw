//! Chunking strategies for the KB ingest pipeline.
//!
//! - [`recursive`]: priority-separator recursive splitter (port of
//!   `src/lib/rag/chunker.ts`).
//! - [`smart`]: structure-aware chunker preserving code blocks, tables,
//!   and heading hierarchy (port of `src/lib/rag/smart-chunker.ts`).
//! - [`prepare`]: `prepare_chunk_for_embedding` helper that prepends
//!   metadata context for embedding.

pub mod prepare;
pub mod recursive;
pub mod smart;

pub use smart::{smart_chunk_document, SmartChunkOptions};
