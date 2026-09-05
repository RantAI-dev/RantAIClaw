# 060 — Recall path hardening: scope, escaping, embedding failure

- **Findings:** #7, #11, #25 (memory deepscan, wave 2)
- **Written against:** main after 059
- **Risk tier:** medium (`src/memory/**`)
- **Effort:** M
- **Depends on:** 059 (all three live inside `recall` / `fts5_search`, which 059 just changed)
- **Blocks:** 073 (`memory reindex` relies on NULL provenance surviving an embedder outage)

Three defects in the same two functions. Separate PRs would each rewrite the other's lines.

## Problem A — scoped recall is global-then-filter (#7)

`recall(query, limit, Some(sid))` runs `fts5_search` with **no session predicate**, takes the
global top `limit * 2`, and only then drops rows whose `session_id` does not match.

So a conversation's own memories are only findable if they happen to rank in the global
top-2N. On a busy database they do not, and a scoped recall returns nothing while matching
rows sit in the table. `vector_search` already takes the filter (`sqlite.rs`); the keyword
path and the LIKE fallback do not.

This is what makes the layered-memory read hollow: the conversation tier can come back
empty for reasons that have nothing to do with relevance.

## Problem B — one quote character disables FTS (#11)

`fts5_search` wraps every whitespace token in double quotes. A token that already contains
`"` yields `""hi""`, which is not valid FTS5 syntax. The statement errors, the error is
swallowed by `unwrap_or_default()`, and recall silently drops to the LIKE fallback — the
weakest path — with nothing logged.

## Problem C — an embedding outage fails reads *and* writes (#25)

`store` and `recall` both call `get_or_compute_embedding` with `?`. One 429 from the
embedding provider therefore fails the whole write, and the whole read.

Callers make it invisible: auto-save uses `let _ = mem.store(...)`, so memories are dropped
silently; `build_memory_context` swallows the `Err`, so context comes back empty. A
transient provider problem reads as "the agent has no memory".

## Change

### Files in scope

- `src/memory/sqlite.rs` — `recall`, `fts5_search`, `store`, the LIKE fallback

### Files explicitly out of scope

- `vector.rs` and the score contract — settled in 059; the rescale point does not move
- `recall_layered` in `memory/mod.rs` — scope *composition* is 065
- Other backends — postgres already filters in SQL, markdown has no sessions
- `reindex()` — 073

### Steps

1. **Scope in SQL, not after the fact.** `fts5_search` takes `session_id: Option<&str>` and
   adds `AND m.session_id = ?` to the join it already performs against `memories`. Same for
   the LIKE fallback query. Keep the post-filter as a cheap assertion — it should now never
   drop anything — rather than deleting it outright.

2. **Escape quotes instead of producing invalid syntax.** Inside an FTS5 string literal a
   `"` is written `""`. Escape each token that way before wrapping, and drop tokens that are
   empty once escaped.

3. **Stop swallowing FTS errors.** `unwrap_or_default()` hides a real failure as "no
   results". Log at `warn` when the statement errors, then fall through as today. A no-match
   is already `Ok(empty)`, so this only surfaces genuine failures.

4. **Degrade on embedding failure rather than failing the operation.**
   - `store`: on error, log `warn` once and write the row with no embedding. The row stays
     keyword-searchable, and its provenance columns stay `NULL` — which is exactly the state
     073's `reindex` looks for.
   - `recall`: on error, log `warn` and continue keyword-only.

   This is a deliberate, documented departure from fail-fast (CLAUDE.md §3.5). Memory is an
   auxiliary capability: refusing to store a user's message because a third-party embedding
   endpoint is rate-limiting is a worse outcome than storing it without a vector. §3.5 asks
   that intentional fallback be documented rather than silent, which is what the `warn` and
   the doc comment are for.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
```

Never a bare `cargo test` — the workspace suite writes ~27G.

## Test plan

1. `scoped_recall_finds_rows_outside_the_global_top_n` — write many rows in session A that
   outrank one row in session B, then recall scoped to B. The B row must come back. Verified
   to fail before step 1: the global top-2N is all A.
2. `scoped_recall_still_excludes_other_sessions` — the filter must not have become a no-op.
3. `fts_query_with_quote_character_still_uses_fts` — a query containing `"` must return the
   FTS hit, not fall through. Assert on a row the LIKE fallback could not have matched
   (whole-token-only), so the test cannot pass via the fallback.
4. `store_survives_embedding_provider_failure` — a stub embedder that always errors; the row
   must still be stored, be keyword-recallable, and carry `NULL` provenance.
5. `recall_survives_embedding_provider_failure` — same embedder; keyword results still come
   back rather than an `Err`.

Each checked against pre-change behaviour. Test 3 in particular must assert something the
fallback cannot satisfy, or it passes either way.

## Escape hatches

- If adding the session predicate to the FTS join measurably changes the query plan for the
  unscoped case (the common one), STOP and report — the unscoped path must not regress to
  pay for the scoped one.
- If a caller depends on `store` returning `Err` when embedding fails, STOP and report;
  degrading would then be a contract change rather than a fix.

## Maintenance note

After this, `recall`'s three retrieval paths agree on scope, and an embedder outage is a
logged degradation instead of an outage of memory itself. 073 clears the `NULL` provenance
those degraded writes leave behind — the two plans meet there deliberately.

## Rollback

Behavioural only, no schema or config change. `git revert` is complete. Rows written while
an embedder was down remain valid and keyword-searchable either way.
