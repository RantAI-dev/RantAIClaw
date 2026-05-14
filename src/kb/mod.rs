//! Knowledge Base subsystem — pure document storage and retrieval.
//!
//! Strictly isolated from [`crate::memory`]: KB is for org-level documents
//! (PDFs, markdown, office files). Agent short-term memory lives in `memory/`.
//! Do not cross-import between these two modules.

pub mod axi;
pub mod chunk;
pub mod config;
pub mod embed;
pub mod error;
pub mod extract;
pub mod file;
pub mod maintenance;
pub mod rerank;
pub mod retrieve;
pub mod store;
pub mod types;

pub use config::KbConfig;
pub use error::{KbError, KbResult};
