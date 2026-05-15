//! Embedding-model drift report.
//!
//! Port of `src/lib/rag/embedding-drift.ts`. Cheap read-only aggregation
//! over [`KbStore::count_by_embedding_model`] that tells an operator how
//! many chunks were embedded with a different model than the one currently
//! configured in [`KbConfig`]. Bulk re-embed (see [`super::bulk_re_embed`])
//! is the corrective action when `stale_chunk_count > 0`.

use std::sync::Arc;

use crate::kb::store::KbStore;
use crate::kb::{KbConfig, KbResult};

/// Result of a drift scan. Mirrors the TS `DriftReport` shape so admin
/// surfaces (CLI, HTTP) stay line-by-line auditable against the reference.
#[derive(Debug, Clone)]
pub struct DriftReport {
    /// Currently-configured embedding model (env or default).
    pub current_model: String,
    /// `(model_id, chunk_count)` per historical model present in the store.
    /// `model_id == None` denotes pre-tracking rows (no `embedding_model`
    /// column value at insert time).
    pub by_model: Vec<(Option<String>, usize)>,
    /// Number of chunks NOT matching `current_model` (sum of all non-current
    /// rows, including `None` rows).
    pub stale_chunk_count: usize,
    /// True when ALL chunks match `current_model`.
    pub in_sync: bool,
}

/// Detect chunks embedded with a model other than `cfg.embedding_model`.
///
/// Read-only. Safe to call from an admin endpoint or a periodic cron — the
/// underlying aggregation is a single `GROUP BY` query in the SQLite backend.
pub async fn check_drift(cfg: &KbConfig, store: &Arc<dyn KbStore>) -> KbResult<DriftReport> {
    let by_model = store.count_by_embedding_model().await?;
    let stale_chunk_count: usize = by_model
        .iter()
        .filter(|(model, _)| model.as_deref() != Some(cfg.embedding_model.as_str()))
        .map(|(_, n)| *n)
        .sum();
    let in_sync = stale_chunk_count == 0;
    Ok(DriftReport {
        current_model: cfg.embedding_model.clone(),
        by_model,
        stale_chunk_count,
        in_sync,
    })
}
