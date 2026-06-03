//! Bulk re-embed migration.
//!
//! Port of `src/lib/rag/bulk-re-embed.ts`. Walks chunks in fixed-size pages
//! via [`KbStore::list_chunks_for_re_embed`], re-embeds each page through
//! [`EmbeddingProvider::embed_many`], and replaces the stored vector + model
//! tag via [`KbStore::update_chunk_embedding`].
//!
//! Design notes:
//! - **Streaming.** Chunks are paginated, never loaded all-at-once, so a
//!   re-embed of a ten-million-chunk store still has a bounded memory
//!   footprint (one batch at a time).
//! - **Per-batch error isolation.** An embed failure on batch N is recorded
//!   in [`BulkReEmbedReport::errors`] and the driver advances to batch N+1;
//!   the run does not abort. Store errors (DB-level), by contrast, are
//!   propagated immediately — they signal something is structurally wrong.
//! - **Dry-run.** With `dry_run = true`, no `update_chunk_embedding` calls
//!   are issued. The report reflects what *would* have happened.
//! - **Progress.** One `tracing::info!` per batch — operators can `tail -f`
//!   the agent log to watch large migrations.

use std::sync::Arc;
use std::time::Instant;

use crate::kb::embed::EmbeddingProvider;
use crate::kb::store::KbStore;
use crate::kb::{KbConfig, KbResult};

/// Caller-facing options. Defaults mirror the TS reference's sensible
/// production values: page size 100, skip-already-current, no dry-run.
#[derive(Debug, Clone)]
pub struct BulkReEmbedOptions {
    /// Number of chunks to process per page. Bounded by the embedding
    /// provider's batch-size sweet spot; the OpenRouter and TEI providers
    /// both prefer 64–256.
    pub batch_size: usize,
    /// When `true`, re-embed every chunk regardless of current model tag.
    /// When `false` (default), skip chunks already tagged with
    /// [`KbConfig::embedding_model`].
    pub include_already_current: bool,
    /// When `true`, report what would happen without writing.
    pub dry_run: bool,
}

impl Default for BulkReEmbedOptions {
    fn default() -> Self {
        Self {
            batch_size: 100,
            include_already_current: false,
            dry_run: false,
        }
    }
}

/// Aggregated report from a bulk re-embed run. Per-batch error messages
/// are returned verbatim so the operator can see provider-specific signals
/// (HTTP status, rate-limit hints) without grepping logs.
#[derive(Debug, Clone, Default)]
pub struct BulkReEmbedReport {
    /// Chunks the driver looked at (sum of all page sizes).
    pub total_chunks_examined: usize,
    /// Chunks re-embedded and written (or, with `dry_run`, would have been).
    pub chunks_re_embedded: usize,
    /// Chunks the driver skipped — currently always 0 here because the SQL
    /// pre-filters via `skip_model`; the field exists for parity with the
    /// TS report and to leave room for client-side skip logic later.
    pub chunks_skipped: usize,
    /// One entry per failed batch. Format: `"batch N (size M): <reason>"`.
    pub errors: Vec<String>,
    /// Wall-clock duration of the run.
    pub elapsed_ms: u64,
}

/// Run a bulk re-embed migration. See module-level docs for semantics.
///
/// `cfg.embedding_model` is used as both the filter (skip rows already on
/// this model unless `include_already_current`) AND the new tag value
/// written into `chunk.embedding_model` for each re-embedded row.
pub async fn run_bulk_re_embed(
    cfg: &KbConfig,
    store: &Arc<dyn KbStore>,
    embedder: &Arc<dyn EmbeddingProvider>,
    opts: BulkReEmbedOptions,
) -> KbResult<BulkReEmbedReport> {
    let started = Instant::now();
    // Guard against degenerate input — a zero batch_size would loop forever.
    let batch_size = opts.batch_size.max(1);
    let target_model = cfg.embedding_model.clone();
    let skip_model: Option<&str> = if opts.include_already_current {
        None
    } else {
        Some(target_model.as_str())
    };

    let mut report = BulkReEmbedReport::default();
    let mut after_id: Option<String> = None;
    let mut batch_idx: usize = 0;

    loop {
        let page = store
            .list_chunks_for_re_embed(batch_size, after_id.as_deref(), skip_model)
            .await?;
        if page.is_empty() {
            break;
        }
        batch_idx += 1;
        let page_size = page.len();
        report.total_chunks_examined += page_size;

        // Last id seen on this page advances the cursor for the next loop.
        // Capture it BEFORE we decompose `page` into (id, text) vecs.
        after_id = page.last().map(|(id, _, _)| id.0.clone());

        let (ids, texts): (Vec<_>, Vec<_>) = page
            .into_iter()
            .map(|(id, content, _)| (id, content))
            .unzip();

        // Embed the page. Provider errors are per-batch — record and move on.
        let embeddings = match embedder.embed_many(&texts).await {
            Ok(v) => v,
            Err(err) => {
                let msg = format!("batch {batch_idx} (size {page_size}): embed failed: {err}");
                tracing::warn!(target: "kb::maintenance", "{msg}");
                report.errors.push(msg);
                continue;
            }
        };
        if embeddings.len() != ids.len() {
            let msg = format!(
                "batch {batch_idx} (size {page_size}): embed count mismatch {} vs {}",
                embeddings.len(),
                ids.len()
            );
            tracing::warn!(target: "kb::maintenance", "{msg}");
            report.errors.push(msg);
            continue;
        }

        if opts.dry_run {
            // Report shape stays consistent so the operator sees the same
            // numbers they would have seen with a real run. No DB writes.
            report.chunks_re_embedded += ids.len();
            tracing::info!(
                target: "kb::maintenance",
                "bulk re-embed [dry-run] batch {batch_idx} size {page_size} (total examined {})",
                report.total_chunks_examined
            );
            continue;
        }

        // Write each chunk's new embedding. A row-level DB error here aborts
        // the run — drift between metadata table and vec0 table is the kind
        // of corruption that needs operator attention, not a silent skip.
        let mut written: usize = 0;
        for (id, emb) in ids.iter().zip(embeddings.iter()) {
            store
                .update_chunk_embedding(id, emb, &target_model)
                .await?;
            written += 1;
        }
        report.chunks_re_embedded += written;

        tracing::info!(
            target: "kb::maintenance",
            "bulk re-embed batch {batch_idx} size {page_size} (total written {})",
            report.chunks_re_embedded
        );

        // A short page means the driver has reached the tail. Bail to avoid
        // a final empty round-trip.
        if page_size < batch_size {
            break;
        }
    }

    // u128 ms → u64 ms: a Duration that overflows u64 ms is ~584 million
    // years; the cast is safe in any realistic bulk-re-embed run.
    #[allow(clippy::cast_possible_truncation)]
    {
        report.elapsed_ms = started.elapsed().as_millis() as u64;
    }
    Ok(report)
}
