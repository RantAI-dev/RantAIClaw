# 059 — One score contract across all five backends

- **Findings:** #3, #28 (memory deepscan, wave 2)
- **Written against:** main after 058
- **Risk tier:** medium (`src/memory/**`)
- **Effort:** M
- **Depends on:** 058 (touches the same `recall` path; must not race it)
- **Blocks:** 060 (recall hardening), 061 (single context builder)

## Problem

`MemoryEntry.score` has no defined meaning. Five backends produce five incompatible
scales, and one configuration value — `min_relevance_score`, default `0.4` — is compared
against all of them.

| Producer | Range | Site |
|---|---|---|
| sqlite, keyword-only | raw BM25 magnitude, unbounded | `sqlite.rs` recall, `vector_results.is_empty()` branch |
| sqlite, hybrid | `vector_weight·cos + keyword_weight·norm_bm25` | `vector.rs::hybrid_merge` |
| sqlite, LIKE fallback | hardcoded `1.0` | `sqlite.rs` recall, fallback branch |
| markdown | `matched_keywords / total_keywords` | `markdown.rs:176` |
| postgres | `0.0`, `1.0`, `2.0` or `3.0` | `postgres.rs:229-231` |
| lucid | `1.0 - rank·0.05`, floor `0.1` | `lucid.rs:228` |

Three consequences, each independently wrong:

1. **An exact keyword match is silently dropped once embeddings are on.** In the hybrid
   branch a document found only by keyword scores at most `keyword_weight` — `0.3` by
   default — which is below the `0.4` threshold. Always. No matter how well it matches.

2. **The weakest retrieval method scores highest.** The LIKE fallback is an unranked
   substring scan and hands back `1.0`, outscoring every BM25 and vector hit.

3. **The threshold is meaningless on postgres and on the keyword-only path.** Postgres
   returns `≥1.0` for any match, so `0.4` never filters. Raw BM25 is unbounded, so
   whether `0.4` filters anything at all depends on corpus size.

## The contract

Written onto the `Memory` trait as documentation, and satisfied by every backend:

> `score` is relevance normalised to `[0, 1]` **within the returned result set**.
> The best hit for a query scores `1.0`; weaker hits score proportionally lower.
> Scores are not comparable across queries or across calls.

Relative-to-best is the honest reading of what max-normalisation actually yields, and it
is the only semantic all five producers can satisfy without inventing an absolute scale
none of them has. It also gives `min_relevance_score` a meaning an operator can reason
about: *keep hits at least this fraction as good as the best one*.

## Change

### Files in scope

- `src/memory/traits.rs` — document the contract on `score` and on `recall`
- `src/memory/vector.rs` — normalise final scores in `hybrid_merge`
- `src/memory/sqlite.rs` — keyword-only branch, LIKE fallback branch
- `src/memory/markdown.rs` — normalise by best
- `src/memory/postgres.rs` — normalise by best
- `src/memory/lucid.rs` — confirm rank 0 yields exactly `1.0`

### Files explicitly out of scope

- `min_relevance_score`'s default value — the contract is what was broken, not the number
- The three context builders that consume the score — 061 unifies those
- Session filtering, FTS escaping, embedding-failure degradation — all 060
- Weight defaults `vector_weight` / `keyword_weight` — unchanged; they still decide the
  *blend*, only the final rescale is new

### Steps

1. **`hybrid_merge`**: after computing weighted sums, divide every `final_score` by the
   largest one. Guard the empty and all-zero cases. This is the fix for consequence 1:
   the best hit reaches `1.0` whichever signal produced it, so a keyword-only match is no
   longer capped at `keyword_weight`.

2. **sqlite keyword-only branch**: divide the negated BM25 values by the largest before
   building `ScoredResult`s, instead of passing the raw magnitude through as
   `final_score`.

3. **sqlite LIKE fallback**: replace the hardcoded `1.0` with
   `matched_keywords / total_keywords`, then normalise by the best in the set. A
   substring scan has no ranking of its own, and claiming a perfect score for it is what
   made the weakest path outrank the strongest.

4. **markdown**: keep `matched / total` as the raw signal, then normalise by the best.

5. **postgres**: keep the `key`-weighted-2 / `content`-weighted-1 SQL expression as the
   raw signal, then normalise by the best in the returned rows.

6. **lucid**: `1.0 - rank·0.05` already gives `1.0` at rank 0. Assert it in a test rather
   than changing it, and drop the `0.1` floor only if it turns out to break monotonicity
   past rank 18 — check before touching.

7. **`traits.rs`**: state the contract above on `MemoryEntry::score`, and note on
   `recall` that implementations must satisfy it.

### A note on what this does not fix

Normalising to the best hit means a result set whose best hit is poor still yields a
`1.0`. That is inherent to relative scoring and is why the contract says "not comparable
across queries". Absolute relevance would need a calibrated model, which nothing here
has. Do not paper over it with a magic constant.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
```

Never a bare `cargo test` — the workspace suite writes ~27G.

## Test plan

One shared property, asserted per backend: **the top hit of any non-empty recall scores
`1.0`, and every score lies in `[0, 1]`.**

1. `sqlite_keyword_only_top_hit_scores_one` — no embedder, several matching rows.
   Verified to fail today: raw BM25 magnitudes are not `1.0`.
2. `sqlite_hybrid_keyword_only_match_survives_threshold` — with a stub embedder whose
   vectors miss the target row, a strong keyword match must still score above `0.4`.
   This is consequence 1; it fails today at exactly `keyword_weight`.
3. `sqlite_like_fallback_does_not_outrank_fts` — a query that reaches the fallback must
   not produce a score above a genuine FTS hit for a comparable query.
4. `markdown_scores_are_normalised`
5. `postgres_scores_are_normalised` — behind the `memory-postgres` feature; skip when the
   feature is off rather than faking a connection.
6. `lucid_top_ranked_entry_scores_one`
7. `hybrid_merge_rescales_to_best` in `vector.rs` — direct unit test of step 1.

Each must be checked against pre-change behaviour. A test asserting `score <= 1.0` alone
would pass today for four of the six producers and prove nothing.

## Escape hatches

- If normalising postgres requires a second query or a window function that changes its
  cost profile, STOP and report — normalising in Rust over the returned rows is the
  intended approach, and anything heavier deserves its own decision.
- If any consumer outside `src/memory/` compares `score` against an absolute constant
  other than `min_relevance_score`, STOP and report; that would make the old scale a
  contract with a second owner.

## Maintenance note

The contract lives on the trait. Any new backend must normalise before returning, and any
new retrieval path inside an existing backend must join the same rescale — the LIKE
fallback is exactly the case that was missed last time. When 073 adds `memory reindex`,
scores are unaffected: it changes which vectors exist, not how they are ranked.

## Rollback

Behavioural only, no schema or config change. `git revert` restores the previous scales.
Stored rows are untouched — `score` is computed per query and never persisted.
