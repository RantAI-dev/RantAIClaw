# Plan 100: Exclude soft-deleted documents from KB counts

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/store/sqlite/groups.rs src/kb/store/sqlite/drift.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

Deleting a document from the web console is a **soft** delete —
`delete_doc` defaults `hard` to false (`api.rs:875`) and the console never
sends `?hard=true` (`claw-ui api.ts:356-358`). Most read paths honour that.
Two counting paths do not, so the numbers stop matching the lists beside them.

**Group document count.** `list_groups_impl` (`groups.rs:74-80`):

```sql
                        (SELECT COUNT(*) FROM document_group dg WHERE dg.group_id = g.id)
                            AS document_count
```

No join to `document`, no `deleted_at` filter. Meanwhile
`list_group_documents_impl` (`groups.rs:250-255`) does filter. So a knowledge
base card shows "5 docs" and opening it lists 4 — permanently, until a hard
delete. The console makes this visible on one screen: `kb-panel.tsx:656` uses
the filtered list length in the detail view while the card list uses
`group.document_count`.

**Drift count.** `count_by_embedding_model_impl` (`store/sqlite/drift.rs:19-26`)
groups over `chunk` with no join. But `list_chunks_for_re_embed`
(`chunks.rs:217-222`) *does* skip soft-deleted parents. So `kb drift` reports N
stale chunks, `kb re-embed` processes fewer than N, and the next `kb drift`
still reports a non-zero remainder — forever. The operator cannot reach
`in_sync`.

## Deliberate behaviour — do not "fix" this

The knowledge graph **intentionally** retains a soft-deleted document's
entities. `tests/kb/intelligence_test.rs:531-535` pins it:

```rust
    assert_eq!(
        store.graph(None, 100).await.unwrap().nodes.len(),
        1,
        "soft delete must preserve the document's intelligence"
    );
```

Leave `graph()` alone. This plan touches only the two counts above.

## Current state (verified at 2ca7e59)

| Filters soft-deleted | Does not |
|---|---|
| `search_by_vector`, `bm25_search`, `list_group_documents`, `chunk_count`, `chunk_counts`, `list_chunks_for_re_embed`, `graph_expand_chunks`, `get_document`, `list_documents` | `list_groups.document_count`, `count_by_embedding_model` |

`store_sqlite_test.rs:402 group_lifecycle_…` asserts `document_count == 1` for
a live document only — it does not pin the soft-deleted case, so this is free
to fix.

## Scope

**In scope**: the two counting queries and their tests.

**Out of scope**: `graph()` (deliberate, pinned), and whether soft delete
should be the console default at all.

## Git workflow

```bash
git switch -c fix/counts-exclude-soft-deleted
```

## Steps

### Step 1: Group document count

```sql
                        (SELECT COUNT(*) FROM document_group dg
                          JOIN document d ON d.id = dg.document_id
                         WHERE dg.group_id = g.id AND d.deleted_at IS NULL)
                            AS document_count
```

This also makes the count immune to the orphan rows plan 099 prevents — a
membership row pointing at a nonexistent document no longer counts, because the
join drops it.

**Verify**: `cargo test --features kb --test kb store_sqlite_test` —
`group_lifecycle_…` must still pass unchanged.

### Step 2: Drift count

```sql
                "SELECT c.embedding_model, COUNT(*) AS n
                 FROM chunk c
                 JOIN document d ON d.id = c.document_id
                 WHERE d.deleted_at IS NULL
                 GROUP BY c.embedding_model",
```

Now `check_drift`'s total matches what `bulk_re_embed` will actually walk.

**Verify**: `cargo test --features kb --test kb maintenance_test` — the four
drift tests use live documents and should pass unchanged.

### Step 3: Tests that pin the invariant

`store_sqlite_test.rs`: create a group, attach two documents, soft-delete one,
assert `document_count == 1` **and** `list_group_documents().len() == 1`. The
pair is the point — the bug was the two disagreeing.

`maintenance_test.rs`: seed chunks for two documents with a stale model,
soft-delete one, assert `check_drift().stale_chunk_count` equals what a
subsequent `bulk_re_embed` reports as examined. Again a pair, not a single
number.

**Verify**: both are red before Steps 1-2.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb store_sqlite_test
cargo test --features kb --test kb maintenance_test
```

End-to-end — the loop that never terminates today:

```bash
cargo build --release --features kb
export KB_DB_PATH=/tmp/kbplan100.db
./target/release/rantaiclaw kb ingest ./README.md
./target/release/rantaiclaw kb ingest ./CONTRIBUTING.md
DOC=$(./target/release/rantaiclaw kb list --json | python3 -c 'import json,sys;print(json.load(sys.stdin)[0]["id"]["0"])')
./target/release/rantaiclaw kb delete "$DOC"           # soft
sqlite3 $KB_DB_PATH "update chunk set embedding_model='old/model'"
./target/release/rantaiclaw kb drift                    # note stale count
./target/release/rantaiclaw kb re-embed --include-current
./target/release/rantaiclaw kb drift                    # MUST be in_sync now
```

## Done criteria

- Card count and detail list agree after a soft delete.
- `kb drift` reaches `in_sync` after one `re-embed`.
- Both paired tests are red without the fix.

## STOP conditions

- `graph()` counts change as a side effect — they must not; revert and narrow
  the edit.
