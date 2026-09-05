# Plan 101: Group-filtered vector search must not lose recall to the KNN window

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/store/sqlite/chunks.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: M (design first)
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

> **This plan starts with a design step.** Unlike the others in this batch, the
> fix shape is not settled. Do Step 0 and report before writing the
> implementation.

## Why this matters

Searching inside one knowledge base can return nothing while that knowledge
base plainly contains matching documents.

`search_by_vector_impl` asks the vector index for a fixed window of nearest
neighbours and applies the group/category filter **afterwards**, in Rust —
`src/kb/store/sqlite/chunks.rs:334-380`:

```rust
            // Over-fetch from vec0 then post-filter in Rust. vec0's MATCH
            // operator does not accept arbitrary WHERE clauses on joined
            // tables, so we pull a larger KNN window and filter afterwards.
            // Heuristic factor of 4 keeps the result set bounded under
            // typical document/category filters.
            let knn_limit = (limit_i.saturating_mul(4)).max(limit_i).max(8);
```

With a corpus of 10,000 chunks, a knowledge base holding 50 of them, and
`limit = 8`, the query pulls the 32 globally-nearest chunks and then keeps only
those in the group. If none of the top 32 belong to it — the normal case for a
small group in a large corpus — the search returns **zero results**.

Two things make it worse:

- Soft-deleted chunks stay in `chunk_vec` (only the join filters them,
  `chunks.rs:365`), so they consume window slots and are then discarded.
- Query expansion runs `search_by_vector` once per paraphrase
  (`retrieve/mod.rs:381-385`), each with the same narrow window, so the union
  does not recover the loss.

This is the most likely cause of a "the KB has the answer but search finds
nothing" report, and it gets worse as a corpus grows — exactly backwards.

## Current state (verified at 2ca7e59)

- Window: `chunks.rs:339`
- Filter resolution: `resolve_allowed_documents` (`chunks.rs:437-505`) already
  computes the allowed document-id set **before** the vector query runs
- vec0 query: `chunks.rs:341-347`
- Callers that pass `group_ids`: `RetrieveOptions.group_ids` →
  `SearchFilter.group_ids` (`retrieve/mod.rs:144-149`); the console always
  scopes to a group when searching inside a KB
- No test covers recall under a selective filter

## Step 0 — design spike (do this first, then report)

Evaluate at least these three, on the real crate version of `sqlite-vec` in
`Cargo.toml`:

**(a) Push the filter into the vec0 query.** Later sqlite-vec releases support
metadata columns and a `rowid IN (...)` constraint on a KNN query; restricting
the scan to the allowed rowids makes recall exact rather than merely better.

**Check the version first — this repo pins `sqlite-vec = "0.1"`
(`Cargo.toml:221`), which predates that support.** So (a) most likely requires a
dependency upgrade, which is its own decision (vector-store behaviour, bundled C
version, and the fact that `chunk_vec`'s on-disk format is created by whatever
version is linked). Do not upgrade inside this plan; report and let it be
decided.

**(b) Iterative widening.** Keep the post-filter, but loop: if the filtered
result count is below `limit` and the window did not exhaust the index, re-query
with a larger `k` (e.g. ×4 each round, capped). Bounded, simple, no dependency
on vec0 features — but does N queries in the bad case.

**(c) Scale the window by selectivity.** `resolve_allowed_documents` already
knows the allowed document count; the store can also cheaply count total live
documents. Size `knn_limit` as `limit * (total / allowed)` with a ceiling. One
query, approximate, and the ceiling still loses recall for a very small group in
a very large corpus.

Report which is available and viable. Recommendation if (a) is supported: take
it. Otherwise (b), because it is the only one that is correct rather than
merely better.

## Scope

**In scope**: recall for filtered vector search.

**Out of scope**: soft-deleted chunks occupying the index — that is a
consequence of soft delete being the default and belongs in its own plan.
Mention it in the PR body.

## Git workflow

```bash
git switch -c fix/group-filtered-search-recall
```

## Steps (after the spike is agreed)

### Step 1: Implement the chosen approach

Keep `resolve_allowed_documents` as the single source of the allowed set — do
not duplicate the filter logic.

### Step 2: Bound it

Whatever the approach, it must have an explicit ceiling and must not silently
truncate. If a cap is hit, log at debug with the counts so the behaviour is
observable.

### Step 3: The test that proves recall

Seed 200 chunks across 20 documents. Put 2 documents in a group. Craft the
query vector so the group's chunks are ranked ~150th globally. Assert a
group-scoped search with `limit = 5` returns them.

Then the control: the same search **unfiltered** must still return the globally
nearest chunks. Without the control, a fix that simply widens the window to the
whole index would pass.

**Verify**: the recall test is red at `2ca7e59`.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb store_sqlite_test
cargo test --features kb --test kb retrieve_test
```

Measure before/after latency on the 200-chunk fixture and record both numbers
in the PR. A recall fix that makes every search slow is a trade, not a win, and
the reviewer needs the figure.

## Done criteria

- A group-scoped search finds in-scope chunks that rank far outside the global
  top-K.
- Unfiltered search behaviour and latency are unchanged.
- The chosen approach and its bound are documented in the function's doc
  comment.

## STOP conditions

- The spike shows (a) is available but requires a `sqlite-vec` upgrade: stop
  and report. A dependency bump on the vector store is its own decision.
- The recall test cannot be made deterministic (embedding-ordering flakiness):
  stop rather than weakening it — a flaky recall test is worse than none.
