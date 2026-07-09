# KB Retrieval Tuning Notes

Captured during Phase 12 of the Rust KB port. Documents the
final tuning state that crosses the `recall ≥ 0.85` parity gate,
plus the rationale for each knob so future regressions are
diagnosable.

## Acceptance result

| Metric           | Value         | Threshold |
| ---------------- | ------------- | --------- |
| `hit@8` recall   | **0.867**     | ≥ 0.85    |
| Passing queries  | 26 / 30       | -         |
| Corpus size      | 161 docs      | -         |
| Embeddings sent  | 161 + 30 query | -        |

Run on commit `74d1008` / branch `feat/kb-rust-port`. See
`docs/reference/kb-bench.md` for latency baselines.

## Final tuning state

The parity test uses the **default** `KbConfig::from_env()` settings,
which are themselves tuned to match the TypeScript baseline:

| Setting                          | Value                       | Why                                                      |
| -------------------------------- | --------------------------- | -------------------------------------------------------- |
| `KB_EMBEDDING_MODEL`             | `qwen/qwen3-embedding-8b`   | Same model the TS retriever uses; 4096-dim.              |
| `KB_EMBEDDING_DIM`               | `4096`                      | Matches model output.                                    |
| `KB_DEFAULT_MAX_CHUNKS`          | `8`                         | hit@8 is the parity metric.                              |
| `KB_HYBRID_BM25_ENABLED`         | `true`                      | RRF over vector + BM25 boosts recall on lexical queries. |
| `KB_RERANK_ENABLED`              | `false` (test override)     | Pure retrieval baseline; reranker layered on separately. |
| `KB_CONTEXTUAL_RETRIEVAL_ENABLED`| `false`                     | Not needed at 0.867 recall; would add LLM cost per doc.  |
| `KB_QUERY_EXPANSION_ENABLED`     | `false`                     | Not needed at 0.867 recall; would add LLM cost per query.|
| Smart-chunker (`SmartChunkOptions::default`) | -               | Default 1200/200 char target/overlap, structure-aware.   |
| RRF `k`                          | `60`                        | Cormack/Clarke 2009 standard.                            |
| Min similarity                   | `0.30`                      | Hard floor; mirrors TS `retriever.ts`.                   |

The parity test in `tests/kb/parity_test.rs` constructs
`Retriever::new(cfg, store, embedder)` without `with_reranker`, so the
rerank stage is bypassed even when `KB_RERANK_ENABLED=true` in the env.
This isolates the "pure hybrid retrieval" recall before any rerank
boost — adding the reranker on top should only ever improve recall.

## Miss analysis (4 of 30)

All four misses are **false negatives** caused by title-prefix
mismatches between `expectedDocs` and actual document titles in the
fixture, not by retrieval failure:

| Query topic              | Expected (fixture) | Actually retrieved (top-K)         |
| ------------------------ | ------------------ | ---------------------------------- |
| PSAK 201 (lookup-en)     | `PSAK 201`         | `PSAK 201 Penyajian Laporan Keuangan` |
| PSAK 201 (lookup-id)     | `PSAK 201`         | `PSAK 201 Penyajian Laporan Keuangan` |
| ISAK 119                 | `ISAK 119`         | `ISAK 119 Pengharian Liabilitas Keuangan Dengan Instrumen Ekuitas` |
| Kerangka Konsept         | `Kerangka Konsept` | `Kerangka Konsept Pelaporan Keuangan` |

In every case the matching long-form title is present in the top-8
sources but the curator's `expectedDocs` uses a short prefix
(`"PSAK 201"` instead of the full title). This is a fixture-curation
issue, not a retrieval issue — the retriever found the right
document.

If the parity threshold is raised in the future, the lowest-effort
fix is to update the fixture to use full document titles in
`expectedDocs`. The Rust retriever already correctly ranks the
target document in the top-K for every query.

## Knobs to try if recall regresses

Apply roughly in this order; each step costs more (LLM calls or
latency) than the previous one.

1. **Increase `KB_DEFAULT_MAX_CHUNKS` from 8 to 10 or 12.** Cheap;
   only widens the candidate window. Likely closes any pure title-prefix
   miss that's currently ranked at position 9-12.
2. **Switch parity-test threshold to substring match instead of exact
   title match.** Aligns the test with how the TS evaluator counts hits
   — see `scripts/eval-rag/run.ts` which uses `.toLowerCase().includes()`
   for the recall metric.
3. **Enable `KB_RERANK_ENABLED=true` and attach an `LlmReranker` via
   `Retriever::with_reranker`.** Adds ~500ms per query but typically
   moves recall +5-10pts. Use OpenRouter `openai/gpt-4.1-nano` as the
   reranker model to keep cost low.
4. **Enable `KB_QUERY_EXPANSION_ENABLED=true`.** Generates 3
   paraphrases per query, embeds all 4, unions the results by max
   similarity. Costs 1 LLM call + N embedding calls per query. Best
   when queries are short or under-specified.
5. **Enable `KB_CONTEXTUAL_RETRIEVAL_ENABLED=true` during ingest.**
   Adds an LLM-generated context prefix to each chunk before
   embedding. One-time cost at ingest, no runtime cost. Helps when
   the corpus has many similar-titled documents.
6. **Tune the smart chunker (`SmartChunkOptions::default`).**
   For very short documents (titles only), increase
   `min_chunk_size` to avoid splitting a 200-char title into
   sub-chunks that confuse vector ranking.

## How to re-run the parity test

```bash
# From the rantaiclaw worktree root
set -a && source ../../../../.env && set +a  # provides OPENROUTER_API_KEY + KB_*
cargo test --features kb --release --test kb \
  -- --ignored rag_golden_parity --nocapture
```

Expected runtime: ~4-5 minutes (ingest ~3 min, retrieval ~1 min) for
161 documents with default `KB_EMBED_CONCURRENCY=4`. Raise concurrency
or use a TEI sidecar to speed up the ingest pass.

## What this test does NOT cover

- Reranker behavior — Phase 8 has dedicated tests.
- Query expansion / contextual retrieval / standalone query — Phase 7
  has dedicated tests.
- `enumerate`, `followup`, and `oos` query kinds in the fixture.
- TS-vs-Rust head-to-head comparison. The TS retriever runs against a
  live Postgres+pgvector corpus that we don't replicate locally;
  comparison is documented separately in `docs/reference/kb-bench.md`.
