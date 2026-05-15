//! KB embedding layer — provider trait, LRU cache, and concrete providers.
//!
//! Task 3.1 ships the cache only; the `EmbeddingProvider` trait and concrete
//! `OpenRouterEmbedding` / `TeiEmbedding` impls land in tasks 3.2 / 3.3.

pub mod cache;
