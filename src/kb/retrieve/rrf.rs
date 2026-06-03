//! Reciprocal Rank Fusion — port of `src/lib/rag/hybrid-merge.ts`.
//!
//! Pure function: merges N ranked lists of items keyed by string id. Score for
//! item i across lists L_1..L_n is the sum over lists of `1 / (k + rank + 1)`
//! (so rank 0 contributes `1/(k+1)`, not `1/k`). Order-stable for ties: the
//! first-seen item wins, mirroring the TS source's `insertIndex` tiebreak.

use indexmap::IndexMap;

/// RRF tuning knobs.
#[derive(Debug, Clone)]
pub struct RrfOptions {
    /// RRF constant — larger values flatten rank differences. Default 60 per
    /// the original paper.
    pub k: usize,
    /// Max results returned after fusion. `None` = unlimited.
    pub limit: Option<usize>,
}

impl Default for RrfOptions {
    fn default() -> Self {
        Self { k: 60, limit: None }
    }
}

/// One entry in the fused output.
#[derive(Debug, Clone)]
pub struct RrfResult<T: Clone> {
    pub id: String,
    pub rrf_score: f64,
    /// Indices of the input lists that contributed this item. Useful for
    /// telemetry / debugging which arm of a hybrid pipeline surfaced a hit.
    pub sources: Vec<usize>,
    /// The first-seen source payload, preserved verbatim so callers keep
    /// domain fields (e.g. similarity, full chunk metadata).
    pub first: T,
}

/// Internal accumulator. Holds the running RRF score plus enough metadata to
/// recover insertion order for tie-breaking.
struct Acc<T: Clone> {
    score: f64,
    sources: Vec<usize>,
    first: T,
    insert_index: usize,
}

/// Merge N ranked lists via Reciprocal Rank Fusion.
///
/// Each input list is `&[(id, payload)]`. The payload is preserved as `first`
/// in the output for the first list that introduced the id — vector search
/// metadata wins over BM25 metadata when both arms surface the same chunk.
///
/// Order-stable for ties: the first-seen id wins.
pub fn reciprocal_rank_fusion<T: Clone>(
    lists: &[&[(String, T)]],
    opts: RrfOptions,
) -> Vec<RrfResult<T>> {
    let k = opts.k;
    // IndexMap preserves insertion order for stable tie-breaking. Mirrors the
    // TS source's `insertIndex` counter + final sort.
    let mut scores: IndexMap<String, Acc<T>> = IndexMap::new();
    let mut next_insert: usize = 0;

    for (list_idx, list) in lists.iter().enumerate() {
        for (rank, (id, payload)) in list.iter().enumerate() {
            // +1 so rank 0 → 1/(k+1), not 1/k.
            let contribution = 1.0_f64 / ((k + rank + 1) as f64);
            if let Some(existing) = scores.get_mut(id) {
                existing.score += contribution;
                existing.sources.push(list_idx);
            } else {
                scores.insert(
                    id.clone(),
                    Acc {
                        score: contribution,
                        sources: vec![list_idx],
                        first: payload.clone(),
                        insert_index: next_insert,
                    },
                );
                next_insert += 1;
            }
        }
    }

    let mut merged: Vec<(String, Acc<T>)> = scores.into_iter().collect();
    // Sort: rrf_score desc, then insert_index asc for ties.
    merged.sort_by(|(_, a), (_, b)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.insert_index.cmp(&b.insert_index))
    });

    let iter = merged.into_iter().map(|(id, acc)| RrfResult {
        id,
        rrf_score: acc.score,
        sources: acc.sources,
        first: acc.first,
    });

    match opts.limit {
        Some(n) => iter.take(n).collect(),
        None => iter.collect(),
    }
}
