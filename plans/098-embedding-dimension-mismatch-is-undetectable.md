# Plan 098: Detect an embedding-dimension mismatch instead of overwriting the evidence

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/store/sqlite/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

Changing `KB_EMBEDDING_DIM` on an existing Knowledge Base corrupts it silently,
and the code destroys the one piece of evidence that would reveal it.

The vector table's dimension is fixed when it is created —
`src/kb/store/sqlite/schema.rs:144-146`:

```sql
        CREATE VIRTUAL TABLE IF NOT EXISTS chunk_vec USING vec0(
            embedding float[{dim}] distance_metric=cosine
        );
```

`IF NOT EXISTS` means a later open with a different `{dim}` is a **no-op** — the
table keeps the old width. But two lines later, `migrate()` overwrites the
recorded dimension unconditionally (`schema.rs:197-200`):

```rust
    conn.execute(
        "INSERT OR REPLACE INTO kb_meta(key, value) VALUES('embedding_dim', ?1)",
        rusqlite::params![embedding_dim.to_string()],
    )?;
```

`migrate` runs on every `SqliteStore::open` (`sqlite/mod.rs:54`), and
`open_store` (`axi/cli.rs:765-769`) runs on **every** `rantaiclaw kb …`
invocation. So a single `kb list` after changing the env var rewrites the
metadata to the new value and the mismatch becomes unprovable.

A reader for the recorded value exists — `current_embedding_dim`
(`schema.rs:205`) — with **zero callers** anywhere in `src/` or `tests/`.

The up-front guard in `store_chunks_impl` does not help either. It compares
against the configured dimension, not the table's (`chunks.rs:45-53`):

```rust
        for (i, emb) in embeddings.iter().enumerate() {
            if emb.len() != self.embedding_dim {
```

`self.embedding_dim` is the value passed to `open`. A 1536-wide vector against a
config of 1536 passes the guard, then hits a `float[4096]` column and surfaces
as a raw sqlite-vec error — despite the module doc claiming "The dimension
contract is enforced **before** any INSERT runs" (`chunks.rs:4-6`).

Finally, the documented remedy does not exist. `schema.rs:138-140` says
"changing KB_EMBEDDING_DIM requires migration via the bulk_re_embed path", but
`bulk_re_embed` writes into the same fixed-width table. And `SCHEMA_VERSION = 2`
(`schema.rs:23`) carries a doc comment promising "a corresponding migration
branch below" — there is none; `migrate()` is pure `CREATE TABLE IF NOT EXISTS`.

## Current state (verified at 2ca7e59)

- No migration machinery for the KB database at all
- `current_embedding_dim` — defined, never called
- `kb_meta.embedding_dim` — overwritten on every open
- No test covers config-dim vs table-dim (`store_sqlite_test.rs:325
  dimension_mismatch_errors_loudly` tests vector length vs configured dim,
  which is a different thing)

## Scope

**In scope**: detect the mismatch and refuse with an actionable error; stop
destroying the evidence.

**Out of scope**: building an actual dimension migration (rebuilding `chunk_vec`
and re-embedding into it). That is a larger feature; this plan makes the
failure loud and recoverable-by-hand, which is the prerequisite.

## Git workflow

```bash
git switch -c fix/kb-embedding-dim-guard
```

## Steps

### Step 1: Read before writing

In `migrate()`, read the recorded dimension **before** the write and compare:

```rust
    // Read first: the write below would otherwise destroy the only evidence a
    // mismatch existed. `CREATE VIRTUAL TABLE IF NOT EXISTS` is a no-op on an
    // existing db, so the vec0 column keeps its original width regardless of
    // what the caller configured.
    if let Some(existing) = current_embedding_dim(conn)? {
        if existing != embedding_dim {
            return Err(KbError::Config(format!(
                "KB embedding dimension mismatch: this database was created with \
                 {existing}, but KB_EMBEDDING_DIM is {embedding_dim}. The vector \
                 table's width is fixed at creation. Either set \
                 KB_EMBEDDING_DIM={existing}, or start a new database (move \
                 kb.db aside and re-ingest) — there is no in-place dimension \
                 migration."
            )));
        }
    }
```

Place it after the `execute_batch` (so a fresh database gets its tables) but
before the `kb_meta` writes.

**Verify**: `cargo build --features kb`.

### Step 2: Make the store guard compare against the table

In `SqliteStore::open` (`sqlite/mod.rs:38-65`), `migrate` now guarantees the
configured dimension equals the recorded one, so `self.embedding_dim` becomes
trustworthy. Update the doc comment on `chunks.rs:4-6` to say *why* the
up-front check is now sound, referencing the guard — otherwise the next reader
finds the same misleading claim.

### Step 3: Tests

Two, in `store_sqlite_test.rs`:

1. Open a temp db with dim 4, drop the handle, reopen the same path with dim 8.
   Assert `SqliteStore::open` returns `KbError::Config` whose message contains
   both numbers.
2. Control: open with dim 4, reopen with dim 4. Assert it succeeds. Without
   this, a mistake that rejects every reopen would pass test 1.

**Verify**: test 1 fails when Step 1 is reverted.

### Step 4: Correct the two misleading comments

- `schema.rs:138-140` — say plainly that there is no in-place dimension
  migration and that changing the dimension requires a new database.
- `schema.rs:22-23` — `SCHEMA_VERSION` promises a migration branch that does
  not exist. Either say the value is informational today, or note that adding
  one is required before any change to an existing table.

Leaving a comment that points at a non-existent remedy is how this defect
survived.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb store_sqlite_test
cargo test --features kb --test kb
```

End-to-end:

```bash
cargo build --release --features kb
export KB_DB_PATH=/tmp/kbplan98.db
KB_EMBEDDING_DIM=4 ./target/release/rantaiclaw kb list
KB_EMBEDDING_DIM=8 ./target/release/rantaiclaw kb list
# expect a clear config error naming 4 and 8 — NOT a silent success
sqlite3 $KB_DB_PATH "select value from kb_meta where key='embedding_dim'"
# expect 4 — the original value must survive the failed open
```

That last assertion is the point of the plan: the evidence is still there.

## Done criteria

- A dimension change is refused with a message that names both values and the
  way out.
- `kb_meta.embedding_dim` is never overwritten with a conflicting value.
- Both tests present, and the mismatch test is red without the guard.

## STOP conditions

- An existing deployment has a `kb.db` whose recorded dimension already
  disagrees with its `chunk_vec` width (someone changed the env var before this
  guard existed). The guard will refuse to open it. That is correct, but say so
  in the release notes with the recovery step, or operators will read it as a
  regression.
