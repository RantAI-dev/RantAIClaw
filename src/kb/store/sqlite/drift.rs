//! Embedding-model aggregation for drift detection.
//!
//! Feeds the maintenance pipeline (`bulk_re_embed` in Phase 9): when the
//! configured `KB_EMBEDDING_MODEL` no longer matches the dominant historical
//! model, the operator can trigger a re-embed run.

use super::SqliteStore;
use crate::kb::{KbError, KbResult};

impl SqliteStore {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) async fn count_by_embedding_model_impl(
        &self,
    ) -> KbResult<Vec<(Option<String>, usize)>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> KbResult<Vec<(Option<String>, usize)>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT embedding_model, COUNT(*) AS n
                 FROM chunk
                 GROUP BY embedding_model",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    let model: Option<String> = row.get(0)?;
                    let count: i64 = row.get(1)?;
                    Ok((model, count.max(0) as usize))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }
}
