//! sqlite-vec + FTS5 backend for [`super::KbStore`].
//!
//! Each submodule keeps a single responsibility: schema, document CRUD,
//! chunk insert/search, BM25, drift.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::kb::{KbError, KbResult};

pub mod bm25;
pub mod chunks;
pub mod documents;
pub mod schema;
mod trait_impl;

/// SQLite-backed KB store. Uses a single connection guarded by a `Mutex` —
/// SQLite is internally serialized anyway, and the synchronous rusqlite API
/// doesn't pair cleanly with multi-conn pooling. Per-thread connections via a
/// pool can be added later if the agent makes many concurrent KB calls.
///
/// Path-traversal validation is intentionally left to the HTTP boundary
/// (Phase 11). Callers in the in-process API are trusted today; if `path`
/// ever flows from a user-controlled string, sanitize at the entry point.
pub struct SqliteStore {
    pub(crate) path: PathBuf,
    pub(crate) embedding_dim: usize,
    pub(crate) conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    pub async fn open(path: impl AsRef<Path>, embedding_dim: usize) -> KbResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            // Skip empty parent (relative filename) — `create_dir_all("")`
            // errors on some platforms.
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        let open_path = path.clone();
        let conn = tokio::task::spawn_blocking(move || -> KbResult<Connection> {
            // Register sqlite-vec as a process-wide auto-extension BEFORE
            // opening the connection — sqlite3_auto_extension only fires for
            // newly-opened connections. Idempotent across calls via Once.
            schema::ensure_vec_extension_registered();
            let conn = Connection::open(&open_path)?;
            schema::migrate(&conn, embedding_dim)?;
            Ok(conn)
        })
        .await
        .map_err(|e| KbError::Other(format!("join error: {e}")))??;

        Ok(Self {
            path,
            embedding_dim,
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}
