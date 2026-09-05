# Plan 155: make `min_relevance_score` mean something — stop guaranteeing the top hit passes

> **Executor instructions**: One PR, one concern: the relevance threshold in
> memory-context injection. This plan is a **design decision plus a bounded
> implementation** — read "The problem is scale, not the filter" before
> touching code; the naive fix (compare raw scores to the threshold) is wrong
> and is explicitly ruled out below. Execute AFTER plans 153/154 are merged,
> then re-check "Why this matters" against the shipped behaviour — if 153+154
> already reduce this to cosmetic for the operator, say so in the PR and scale
> the change down accordingly. If anything under "STOP conditions" occurs,
> stop and report. When done, add this plan's row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat d0089a4..HEAD -- src/memory/context.rs src/memory/vector.rs src/memory/sqlite.rs src/memory/postgres.rs src/memory/lucid.rs src/tools/memory_recall.rs`
> All line numbers below are from `d0089a4`. If this diff is non-empty,
> re-verify each cited line before editing.

## Status

- **DONE 2026-08-17** — PR #558 (merged e4b939b), released **v0.22.0-alpha**; keyword signal pivoted from saturated BM25 to query coverage after the A/B measured BM25 ≈ 1e-6 on the live store

- **Priority**: P2 — after 153/154 the injectable pool is curated shared
  facts only, so a false-positive injection is low-harm; but the threshold is
  still structurally vacuous for the top hit, and "hello" should inject
  nothing rather than the least-irrelevant fact
- **Effort**: M
- **Risk**: MEDIUM (changes which memories surface for every backend; needs
  A/B evidence on a live store, not just unit tests)
- **Depends on**: plans 153, 154 (merge first; they change what this filter
  sees)
- **Category**: bugfix (memory ranking)
- **Planned at**: `d0089a4`, 2026-08-17

## Why this matters

`build_memory_context` filters on `score >= min_relevance_score`
(`src/memory/context.rs:158-162`, default 0.4). But backends normalize scores
**relative to the best hit** before that filter runs
(`normalize_entry_scores`, `src/memory/vector.rs:16-34`, called from sqlite /
postgres / lucid recall and again from `context.rs:150`): the best hit is
always rescaled to exactly 1.0. Consequence: the top-ranked entry passes the
threshold **by construction**, however weakly it matched — the filter can
only ever trim the tail, never say "nothing here is relevant". Both live
repros (2026-08-16/17, see plan 153) rode this: a casual greeting always
found *some* best match, and that match was always injected.

Every surveyed runtime uses an absolute floor (OpenClaw `minScore` 0.35,
ZeroClaw 0.4 applied to its raw hybrid score).

## The problem is scale, not the filter

The naive fix — apply `min_relevance_score` to the **raw** score — does not
work, because raw scores are not on a common scale:

- vector path: cosine similarity, bounded 0–1 (`vector.rs:37`), absolute —
  usable as-is;
- keyword path (sqlite FTS5): BM25, unbounded, corpus-dependent
  (`src/memory/sqlite.rs:406-428`; negated so higher = better) — a raw test
  fixture legitimately produces 4.0 (`vector.rs:506-516`);
- LIKE fallback: already a bounded 0–1 query-coverage score
  (`sqlite.rs:898-900`, test at `sqlite.rs:2226-2256`).

Relative normalization exists precisely to paper over this — and in doing so
it destroys the threshold's meaning. The fix is to make the **blended score
absolutely bounded at the source**, then threshold on it.

### Chosen approach

1. Bound the keyword component before blending: map BM25 `s` (already
   negated, ≥ 0) through the saturating transform `s / (s + K)` (pick `K` so
   that a solid single-term match lands ~0.5–0.7; derive it from the existing
   FTS tests' fixtures and record the derivation in a code comment). This is
   monotone (ranking order unchanged) and bounded 0–1.
2. Blend as today (`vector_weight` / `keyword_weight`,
   `sqlite.rs:725-776`) — with both components in 0–1 the blended
   `final_score` is absolute and bounded.
3. **Stop re-normalizing relative-to-best** on the paths that feed the
   threshold: drop the `normalize_entry_scores` calls in the backends' recall
   and in `context.rs:150` (the echo-removal re-rank becomes unnecessary —
   removing an entry no longer changes anyone else's score; delete the
   re-rank, keep the echo removal). Keep the function only if a display
   surface still wants a relative percentage — check
   `src/tools/memory_recall.rs` (`recall_renders_the_score_as_a_real_percentage`
   test) and render whatever keeps that display honest.
4. Postgres (`src/memory/postgres.rs`) and lucid (`src/memory/lucid.rs`)
   recall paths get the same treatment — enumerate their score sources first;
   if one is already bounded, only remove its relative normalization.

`min_relevance_score` keeps its config default (0.4) and its meaning becomes
real: below-floor sets inject **nothing**. Say in the PR/CHANGELOG that
recall selectivity changes (schema version bump is NOT needed — no config
default changes — but the behaviour note is).

### Tests (write first, watch them fail)

- The headline invariant, in `context.rs` tests:
  `a_weak_best_hit_is_not_injected` — single entry whose raw blended score is
  below 0.4 → empty block. (Today this is impossible to express because
  normalization forces the top hit to 1.0 — that impossibility IS the bug.)
- Control: a genuinely strong hit (cosine ≥ threshold) still injects.
- BM25 transform: unit test the saturation mapping (monotone, bounded,
  ordering preserved against the existing `fts5_bm25_ranking` fixture at
  `sqlite.rs:1296`).
- Existing tests that encode relative normalization
  (`normalize_entry_scores_rescales_to_best`, the `context.rs` re-rank test
  `a_self_echo_does_not_bury_the_facts_beneath_it`) will need updating — for
  each one, state in the PR what behaviour replaced the one it pinned. The
  echo test's *scenario* must still pass: facts must survive echo removal
  (they now survive trivially, since no renormalization buries them).
- **Mutation check**: re-add a `normalize_entry_scores` call before the
  threshold — `a_weak_best_hit_is_not_injected` must go red.

### A/B evidence (required, not optional)

On a copy of the operator's live `brain.db` (sandbox
`RANTAICLAW_CONFIG_DIR`): run the same 5 queries (a greeting, two real
questions about stored facts, two off-topic questions) before and after.
Record what was injected in each case in the PR description. Success shape:
off-topic queries inject nothing; the real questions still surface their
facts. If the real questions lose their facts, the transform constant `K` or
the default threshold is mis-tuned — fix before merge, do not ship and tune
later.

### Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
cargo test --lib tools::memory_recall
```

## STOP conditions

- Embeddings are disabled in the default config (keyword-only recall) and the
  BM25 transform alone cannot separate relevant from irrelevant on the A/B
  queries — that means the floor is untunable for keyword-only stores; report
  with the A/B numbers instead of shipping a threshold that filters
  everything or nothing.
- Postgres/lucid score sources turn out not to be boundable without
  re-ranking semantics changes beyond this plan's scope — ship sqlite +
  shared-builder first, file a follow-up for the stragglers, and say so.

## Rollback

Single revert restores relative normalization; no stored data changes (scores
are computed at read time).
