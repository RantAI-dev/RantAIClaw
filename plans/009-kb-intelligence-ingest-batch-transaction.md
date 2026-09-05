# Plan 009: Batch KB intelligence-ingestion writes into one transaction

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/kb/intelligence/ src/kb/store/sqlite/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.
>
> **REVISED after cold review**: the earlier draft's "simplest port" (pass
> pre-built structs, insert as-is, no RETURNING) is WRONG and would silently
> orphan mentions/relations. `upsert_entity` returns the *surviving* (first-seen)
> row id for a `canonical_key`, and entities are GLOBAL across documents, so a
> pre-assigned `new_id()` won't match an existing row — mentions/relations built
> against it become dangling. The batched method MUST resolve surviving ids
> inside the transaction and remap. Corrected below.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

Ingesting a document's extracted intelligence issues `2N + 2M + R` separate
blocking SQLite round-trips — one per entity, per mention, per relation — each
its own `spawn_blocking` + mutex acquisition + a single auto-committed `INSERT`
(and `upsert_entity` does a second `SELECT` round-trip inside the same lock). On
the default rollback journal each auto-commit is an fsync, so entity-heavy
documents make KB build time scale with entity count instead of document count.
`store_chunks` already does the right thing (one transaction, prepared statements
reused); this brings intelligence ingestion to the same pattern — while
preserving the cross-document entity-merge semantics that make the surviving-id
resolution load-bearing.

## Current state (verified at 4d35107)

- `src/kb/intelligence/mod.rs` — the ingest loop:
  - `:37` — `store.delete_document_intelligence(document_id)` runs BEFORE
    extraction (this is the re-ingest idempotency; keep it valid).
  - `:53` `store.upsert_entity(&entity).await?` → returns the surviving id →
    used at `:57` for the mention and stored in `entity_id_by_name`.
  - `:54-63` `store.add_mention(...)`; `:80/:81` the per-chunk pattern-entity
    upsert+mention (nested loop); `:100-110` `store.add_relation(...)`.
  - `n_ent` increments once per extraction iteration (`:65`, `:92`) — i.e. it
    equals the number of MENTIONS produced, NOT the count of distinct entities.
  - Returns `IntelligenceSummary { entities: n_ent, relations: n_rel }` (`:115-118`).

- `src/kb/store/sqlite/intelligence.rs` — each method its own `spawn_blocking` +
  `blocking_lock`:
  - `upsert_entity` (`:47-80`): `INSERT … ON CONFLICT(canonical_key) DO UPDATE SET confidence = max(...)`
    (`:58-70`) then `SELECT id … WHERE canonical_key` (`:71-75`) — **returns the
    first-seen surviving row id**, which may differ from `e.id`. The trait doc
    says so explicitly (`src/kb/store/mod.rs:186-188`).
  - `add_mention` (`:82-103`), `add_relation` (`:105-128`) — each a single INSERT.
  - `delete_document_intelligence` (`:315-342`).
  - IDs are Rust-generated uuids (`new_id()` = uuid v4, `mod.rs:22`;
    `id: String`, `types.rs:184/195/206`; schema `id TEXT PRIMARY KEY`,
    `schema.rs:171/176/180`) — NOT autoincrement. **But that is not the point** —
    the point is the surviving-id return value above.

- Entities are GLOBAL across documents (`resolve.rs:1-2`; test asserts
  `graph.nodes[0].doc_count == 2`, `tests/kb/intelligence_test.rs:177`). So a
  `canonical_key` collision (another document's entity, or an intra-document
  LLM-vs-pattern collision) is normal and must resolve to the existing id.

- **The pattern to mirror** — `store_chunks_impl` (a `pub(crate) async fn` on
  `SqliteStore`, `src/kb/store/sqlite/chunks.rs:29-102`): one `spawn_blocking`
  (`:65`), `blocking_lock` (`:66`), `tx` (`:67`), `prepare` once (`:69-76`), loop
  (`:78-95`), `commit` (`:97`). The `IntelligenceStore` impl is written inline in
  `intelligence.rs` (no `_impl` delegation), so `store_intelligence` goes directly
  in `impl IntelligenceStore for SqliteStore`.

- **Two impls of the store trait**: `SqliteStore` (`intelligence.rs:46`) AND a
  test mock `FakeIntel` (`tests/kb/retrieve_test.rs:376`). A new trait method with
  no default impl breaks the test target until `FakeIntel` implements it too.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint (kb) | `cargo clippy --features kb --all-targets -- -D warnings` | exit 0 |
| KB tests | `cargo test --features kb kb::` | all pass, incl. new |
| Intelligence tests | `cargo test --features kb intelligence` | pass |

## Scope

**In scope**:
- `src/kb/store/sqlite/intelligence.rs` — add a batched `store_intelligence(...)`.
- The store trait (find it: `grep -rn "fn upsert_entity\|trait IntelligenceStore" src/kb/store/`)
  — add the method (no default impl, OR a default that loops the existing methods
  so `FakeIntel` needn't change; decide in Step 1).
- `tests/kb/retrieve_test.rs` — implement `store_intelligence` on `FakeIntel`
  (a stub matching its sibling methods at `:377-397`) IF the trait method has no
  default impl.
- `src/kb/intelligence/mod.rs` — call the batched method.
- Tests under the KB test tree.

**Out of scope** (do NOT touch):
- Extraction logic; `chunks.rs`; the `ON CONFLICT` confidence-max semantics
  (preserve exactly); `IntelligenceSummary`'s shape and its per-iteration counts.

## Git workflow

- Branch: `advisor/009-kb-intelligence-ingest-batch-transaction`
- Commit per logical unit (store method + trait, then FakeIntel + caller).
  Messages e.g. `perf(kb): batch entity/mention/relation ingest into one transaction`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Add a batched `store_intelligence` that resolves + remaps ids in-transaction

Add `store_intelligence(&self, document_id: &str, entities: &[Entity], mentions: &[EntityMention], relations: &[Relation]) -> KbResult<()>`
to the trait + `SqliteStore` impl. The caller still builds the structs (with
pre-assigned `new_id()`s and the name→provisional-id wiring), but the STORE must
reconcile provisional ids against surviving ids:

Inside ONE `spawn_blocking` + `blocking_lock` + `Transaction`:
1. Prepare the entity upsert once. For each entity, run
   `INSERT … ON CONFLICT(canonical_key) DO UPDATE SET confidence = max(...) RETURNING id`
   (rusqlite 0.37 bundled SQLite supports `RETURNING`; if you prefer, keep the
   existing `SELECT id WHERE canonical_key` after the upsert). Build a map
   `provided_id -> surviving_id` (`HashMap<String, String>`), keyed by the
   entity's provisional `id`.
2. Prepare the mention insert once. For each mention, look up
   `surviving_id = map[&mention.entity_id]` (fall back to `mention.entity_id` if
   somehow absent) and insert with the surviving id.
3. Prepare the relation insert once. For each relation, remap BOTH
   `source_entity_id` and `target_entity_id` through the map before inserting.
4. `tx.commit()`.

This preserves the exact cross-document merge behavior the per-item path had
(surviving-id resolution) while collapsing `2N+2M+R` round-trips into one
transaction.

Decide the trait-default question: EITHER give the trait method a default impl
that calls the existing `upsert_entity`/`add_mention`/`add_relation` in sequence
(so `FakeIntel` needs no change, but then remap must happen in the default too —
messy), OR make it a required method and add a `FakeIntel` stub (Step 1b).
**Prefer required method + FakeIntel stub** — the whole point is the sqlite impl
does the transaction; a default that loops the old methods wouldn't batch.

**Verify**: `cargo build --features kb 2>&1 | tail -5` → compiles.

### Step 1b: Implement `store_intelligence` on the test mock

In `tests/kb/retrieve_test.rs`, add `store_intelligence` to `FakeIntel` mirroring
its sibling stubs (`:377-397`) — e.g. record the inputs or `unimplemented!()` if
the sibling methods do (read them and match). Without this, the test target won't
compile under `--all-targets`.

**Verify**: `cargo build --features kb --tests 2>&1 | tail -5` → compiles.

### Step 2: Rewire the caller to build slices then call once (preserve counts)

In `src/kb/intelligence/mod.rs`, build `Vec<Entity>`, `Vec<EntityMention>`,
`Vec<Relation>` from the existing loops, then make ONE
`store.store_intelligence(document_id, &entities, &mentions, &relations).await?`
call. **Preserve `n_ent` as a per-iteration count** (increment once per extraction
iteration as today — it equals `mentions.len()`, NOT `entities.len()`); do NOT
switch to `entities.len()` or a deduped count, or the summary silently changes.
Keep `n_rel` counting as today. Keep the `entity_id_by_name` map for wiring
relation names (it still maps name → provisional id; the store remaps provisional
→ surviving, so relations resolve correctly).

Keep `delete_document_intelligence(document_id)` at `mod.rs:37` as-is (it handles
re-ingest idempotency and remains valid). Do NOT also move a delete into the
transaction unless you ALSO remove line 37 (avoid a double-delete) — the simplest
correct choice is to leave `:37` and not add a delete inside `store_intelligence`.

**Verify**: `grep -n "store.upsert_entity\|store.add_mention\|store.add_relation" src/kb/intelligence/mod.rs`
→ no matches; `cargo build --features kb 2>&1 | tail -5` → compiles.

## Test plan

- Tests under the KB test tree (`tests/kb/` or inline `#[cfg(feature="kb")]`
  `#[cfg(test)]`), using a `tempfile` or in-memory sqlite store:
  1. `store_intelligence_persists_all`: build entities + mentions + relations,
     call `store_intelligence`, query counts, assert all rows landed and the
     `IntelligenceSummary` counts match the per-iteration semantics.
  2. `store_intelligence_cross_document_merge`: ingest entity "Acme" for doc A,
     then ingest a DIFFERENT doc B whose extracted entity has the same
     `canonical_key` but a fresh `new_id()`; assert the mention/relation for doc B
     resolves to the SURVIVING (doc-A) entity id — i.e. `entity_mention` joins to
     one `entity` row, `doc_count == 2`, NO orphaned mention. **This is the
     regression the remap fixes; it must fail without Step 1's remap.**
  3. `store_intelligence_reingest_idempotent`: ingest doc A twice; assert no
     duplicate rows (delete-before + ON CONFLICT hold).
  - Model after `tests/kb/intelligence_test.rs` (the `doc_count == 2` test at
    `:177` is a template).
- Verification: `cargo test --features kb intelligence` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --features kb --all-targets -- -D warnings` exits 0 (FakeIntel implements the new method)
- [ ] `cargo test --features kb kb::` passes; the cross-document-merge test exists and passes
- [ ] `grep -n "store.add_relation\|store.add_mention\|store.upsert_entity" src/kb/intelligence/mod.rs` returns no matches (single batched call)
- [ ] `IntelligenceSummary` counts are unchanged for a given input (per-iteration semantics preserved)
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The store methods / `store_chunks_impl` / the surviving-id `SELECT` don't match
  the excerpts (drift).
- `RETURNING id` is not supported by the bundled rusqlite (a query error) — fall
  back to the existing `SELECT id WHERE canonical_key` inside the transaction; if
  neither works, report.
- `FakeIntel`'s sibling methods do something other than record/`unimplemented!()`
  such that a stub would change test behavior — report before stubbing.

## Maintenance notes

- The surviving-id remap is the load-bearing correctness invariant — a reviewer
  must confirm mentions and BOTH relation endpoints are remapped, or cross-
  document entity merges silently lose edges.
- The per-item `upsert_entity`/`add_mention`/`add_relation` have no non-test
  production callers besides this loop (verified) — keep them for tests; just stop
  using them in the hot path.
- If a WAL journal is later enabled for the KB db, the per-commit fsync cost drops
  and this becomes cheaper still — the transaction batching remains correct.
