# 056 — Snapshot hydration writes a schema `init_schema` cannot open

- **Finding:** #1 (memory deepscan, wave 0)
- **Written against:** `9781696`
- **Risk tier:** medium (`src/memory/**`, startup path)
- **Effort:** S
- **Depends on:** nothing
- **Blocks:** 058 (schema migration — must not land on top of a divergent DDL)

## Problem

`snapshot::hydrate_from_snapshot` re-declares the memory schema instead of reusing
`SqliteMemory::init_schema`. The two declarations have drifted. Snapshot's
`embedding_cache` has no `accessed_at` column; `SqliteMemory::init_schema` then runs

```sql
CREATE INDEX IF NOT EXISTS idx_cache_accessed ON embedding_cache(accessed_at);
```

against that table. `CREATE TABLE IF NOT EXISTS` is a no-op on the existing table, so the
index creation hits a column that does not exist, `execute_batch` fails, and
`SqliteMemory::with_embedder` returns `Err`. `create_memory` propagates it, so the agent
cannot start with memory at all.

Order of events in `create_memory` (`src/memory/mod.rs:221-239` then `:299-305`):
hydration runs *first*, backend construction second — so hydration poisons the database
that construction then opens.

### Reproduced

```
hydrate schema     → init_schema FAILED: OperationalError no such column: accessed_at
fresh db (control) → init_schema: OK
```

The control matters: the batch itself is fine. Only the hydrated database breaks it.

### Trigger conditions

`auto_hydrate` defaults to `true` (`schema.rs:2022`), so any workspace with a
`MEMORY_SNAPSHOT.md` and a missing/empty `brain.db` hits this. Producing that snapshot
requires `snapshot_enabled` (default `false`), so the trigger is narrow — but the failure
is total.

## Root cause

Two declarations of one schema. Patching snapshot's DDL to add `accessed_at` fixes today's
symptom and leaves the divergence in place for the next column. Fix the duplication.

A second, latent divergence confirms this: snapshot's DDL omits the FTS5 sync triggers
(`memories_ai` / `memories_ad` / `memories_au`), which is why `hydrate_from_snapshot`
hand-inserts into `memories_fts`. Once the real schema is used, those triggers exist and
the manual insert becomes a duplicate row in the FTS index.

## Change

### Files in scope

- `src/memory/sqlite.rs` — widen `init_schema` visibility
- `src/memory/snapshot.rs` — call it; drop the local DDL and the manual FTS insert

### Files explicitly out of scope

- `src/memory/mod.rs` — hydration ordering is correct; do not reorder
- Anything under `src/memory/` other than the two above
- The `embedding_cache` schema itself — 058 owns schema changes

### Steps

1. In `src/memory/sqlite.rs`, change `fn init_schema` to `pub(crate) fn init_schema`.
   Do not change its body.

2. In `src/memory/snapshot.rs::hydrate_from_snapshot`, replace the `conn.execute_batch(
   "CREATE TABLE IF NOT EXISTS memories ... embedding_cache ...")` block with a call to
   `crate::memory::sqlite::SqliteMemory::init_schema(&conn)?`.

3. In the same function, delete the manual `INSERT INTO memories_fts(key, content)`
   that follows a successful `memories` insert. The `memories_ai` trigger now does it.
   Keep the `hydrated += 1` accounting on the same branch.

4. Keep `export_snapshot` untouched — it only reads.

## Verification

Must fail before step 2, pass after:

```bash
cargo test --lib memory::snapshot
cargo test --lib memory::sqlite
```

Full gate:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
```

Do **not** run a bare `cargo test` — the workspace suite writes ~27G.

## Test plan

Add to `src/memory/snapshot.rs` tests, following the existing `TempDir` pattern in that
module:

1. `hydrated_database_opens_with_sqlite_memory` — write a `MEMORY_SNAPSHOT.md`, call
   `hydrate_from_snapshot`, then `SqliteMemory::new(workspace)`. Must be `Ok`. This is the
   regression test; it fails on `main` today.

2. `hydrated_entries_are_recallable` — after the above, `recall()` must return the
   hydrated entry. Guards against the FTS index being left empty once the manual insert
   is removed.

3. `hydrate_does_not_double_index_fts` — hydrate one entry, then assert `recall` returns
   exactly one row for a term in it. Guards the opposite failure: trigger *and* manual
   insert both firing.

Test 3 is the one that proves step 3 was necessary; without it the duplicate-insert
regression is invisible.

## Escape hatches

- If `init_schema` turns out to need `&mut Connection` or otherwise cannot be called from
  snapshot's context, STOP and report — do not copy the DDL back.
- If making `init_schema` `pub(crate)` trips a visibility lint that suggests restructuring
  the module, STOP. A module reshuffle is out of scope for a wave-0 hotfix.

## Maintenance note

After this lands there is exactly one memory DDL. Any future column must be added in
`SqliteMemory::init_schema` only. 058 adds the embedding-provenance columns and the UTC
timestamp change on top of this; it must not reintroduce a second declaration.

## Rollback

Single commit, two files, no schema change to existing databases (the DDL is
`IF NOT EXISTS` throughout). `git revert` is safe and complete.
