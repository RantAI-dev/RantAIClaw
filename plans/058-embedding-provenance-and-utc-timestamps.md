# 058 — Embedding provenance + canonical UTC timestamps

- **Findings:** #26, #27 (memory deepscan, wave 1)
- **Written against:** `105c5ab`
- **Risk tier:** medium (`src/memory/**`, schema migration)
- **Effort:** M
- **Depends on:** 056 (one DDL declaration — this must not land on a divergent schema)
- **Blocks:** 059 (score contract), 073 (`memory reindex`)

Both findings change the shape of a stored row, so they share one migration. Split into
two plans they would be two sequential migrations rewriting the same table.

## Problem A — no embedding provenance (#26)

Nothing records which model or dimensionality produced a stored embedding.
`cosine_similarity` returns `0.0` when vector lengths differ (`vector.rs:5-7`) — silently,
not as an error. So changing `embedding_model`, or setting `embedding_dimensions` to a
value the model does not actually emit, makes every vector comparison score zero. Recall
falls back to keyword-only and the operator sees no warning anywhere.

There is a second, sharper edge in the same root cause. The embedding cache is keyed on
`SHA256(text)` alone (`sqlite.rs::content_hash`). The model is not part of the key, so
after switching models the cache returns the **previous model's vector** for the same
text — wrong dimensionality, wrong space, served as a hit.

With `reindex()` currently unreachable (finding #21, plan 073) there is no recovery path,
so this state is permanent once entered.

## Problem B — timestamps compared as local-offset strings (#27)

`sqlite.rs::store` writes `Local::now().to_rfc3339()`. `hygiene::prune_conversation_rows`
builds its cutoff the same way and compares with SQL `<` — a lexicographic comparison of
strings that may carry different UTC offsets.

Same-offset machines are fine. Mixed offsets are not, and this repo ships a Dockerfile:

```
row written in a UTC container : 2026-08-05T04:00:00+00:00
cutoff computed on a +07:00 host: 2026-08-05T10:00:00+07:00   (same instant)
lexicographic: "04..." < "10..."  → row deleted
```

Deletion is the unsafe direction: conversation memories are pruned up to the offset
difference early. The reverse pairing merely under-prunes.

## Change

### Files in scope

- `src/memory/sqlite.rs` — schema, provenance write + read, cache key, UTC writes
- `src/memory/snapshot.rs` — UTC writes for hydrated rows
- `src/memory/hygiene.rs` — UTC cutoff

### Files explicitly out of scope

- `hygiene`'s file-archival cutoffs (`:136`, `:180`, `:225`, `:263`) — those compare
  date-only filename prefixes against a local date, which is what an operator reading
  `2026-08-05.md` expects. Do not convert them.
- `snapshot.rs:66` — the human-facing `**Last exported:**` header. Local time is correct
  for a human reading the file.
- `vector.rs` — scoring semantics belong to 059.
- `reindex()` — 073 wires it up; do not enable it here.

### Steps

1. **Schema** (`init_schema`): add `embedding_model TEXT` and `embedding_dims INTEGER`
   to `memories`, using the same "check `sqlite_master`, then `ALTER TABLE`" idiom the
   existing `session_id` migration uses. Both nullable — pre-existing rows have unknown
   provenance and must stay readable.

2. **Migration marker**: use `PRAGMA user_version` (currently unused across the repo).
   Version `0` means "pre-058". On seeing `0`, run step 3, then set `1`.

3. **UTC backfill**: rewrite `created_at` / `updated_at` on every existing row from
   whatever offset they carry into UTC. Parse with `DateTime::parse_from_rfc3339`, which
   handles any offset correctly; on a parse failure leave the row untouched rather than
   guessing. Runs once, guarded by the marker.

4. **Write UTC**: `Utc::now().to_rfc3339()` in `sqlite.rs::store`,
   `get_or_compute_embedding`, and `snapshot.rs`'s hydrate path.

5. **Cutoff in UTC**: `hygiene::prune_conversation_rows` builds its cutoff with
   `Utc::now()`. After step 3 every row is UTC, so the string comparison is sound again.

6. **Cache key**: `content_hash` takes the model name and dimensionality alongside the
   text. A model switch then misses rather than serving a foreign vector.

7. **Write provenance**: `store` records the embedder's `name()` and `dimensions()`
   whenever it writes an embedding; `NULL` when no embedding was produced.

8. **Consume provenance**: `vector_search` skips rows whose `embedding_dims` differs from
   the live embedder, and logs once per call when it skipped any — that log is the
   operator's signal to run the reindex that 073 adds. Rows with `NULL` provenance are
   pre-058 and are matched on vector length, as today.

Step 8 is what turns this from a schema change into a fix. Without it the columns are
inert.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
```

Never a bare `cargo test` — the workspace suite writes ~27G.

## Test plan

In `src/memory/sqlite.rs` tests:

1. `legacy_local_offset_timestamps_migrate_to_utc` — insert a row carrying `+07:00`
   through a raw connection, reopen via `SqliteMemory`, assert the stored timestamp is
   UTC and denotes the same instant.
2. `migration_runs_once` — reopen twice; the second open must not rewrite (assert
   `user_version` is `1` and timestamps are unchanged).
3. `unparseable_timestamp_is_left_alone` — a garbage `updated_at` survives migration
   rather than being zeroed.
4. `embedding_cache_key_separates_models` — the same text under two embedder identities
   must not share a cache entry.
5. `vector_search_skips_foreign_dimensioned_rows` — store under a 4-dim stub embedder,
   query with an 8-dim one, assert the mismatched row is not returned.

In `src/memory/hygiene.rs` tests:

6. `prune_does_not_delete_rows_newer_than_cutoff_across_offsets` — write a row whose
   local-offset rendering sorts *below* the cutoff string but whose instant is *newer*.
   Must survive. This is the data-loss regression; verified to fail against a
   `Local::now()` cutoff.

Each test must be checked against the pre-change behaviour. A migration test that passes
either way proves nothing.

## Escape hatches

- If `PRAGMA user_version` turns out to be claimed by a dependency (rusqlite migrations,
  a bundled extension), STOP and switch to a `schema_meta` table — do not co-opt it.
- If the UTC backfill would need to rewrite more rows than fit comfortably in one
  transaction on a realistic database, STOP and report; batching changes the rollback
  story and deserves its own decision.

## Maintenance note

`user_version` is now the memory schema's migration counter. Any later schema change
increments it and adds its own guarded step. Provenance columns are nullable by design:
`NULL` means "written before 058", and 073's reindex is what clears them.

## Rollback

`git revert` restores the code. The migration is not reversed by that: rows stay UTC and
the two columns remain. Both are forward-compatible — the pre-058 code reads UTC
timestamps as opaque strings and ignores unknown columns — so a revert leaves a working
database, only without provenance filtering.
