# Plan 093: GraphRAG must see LLM-extracted entities (and the test that hides it must be rewritten)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/store/sqlite/intelligence.rs src/kb/intelligence/ tests/kb/intelligence_test.rs`
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

GraphRAG exists to expand retrieval through the entity graph the LLM extractor
builds. It cannot reach a single LLM-extracted entity.

`graph_expand_chunks` joins mentions to chunks on the chunk index
(`src/kb/store/sqlite/intelligence.rs:507-508`):

```sql
                 JOIN entity_mention m
                       ON m.document_id = c.document_id AND m.chunk_index = c.chunk_index
```

But LLM mentions are always stored with a NULL chunk index
(`src/kb/intelligence/mod.rs:56-63`):

```rust
        mentions.push(EntityMention {
            id: new_id(),
            entity_id: id.clone(),
            document_id: document_id.to_string(),
            chunk_index: None,          // <-- aggregated for the whole document
            context: None,
            source: ExtractSource::Llm,
        });
```

In SQL, `NULL = <integer>` is NULL, never true. So the join drops every LLM
mention. Only pattern entities — emails and URLs, which get `Some(idx)` at
`mod.rs:85` — can ever surface a chunk.

The entity graph itself is fine: seeds match (`intelligence.rs:452-454` selects
from `entity`), and one-hop expansion walks `entity_relation`. It is only the
final chunk join that silently discards the result. `KB_GRAPHRAG_ENABLED=true`
therefore buys almost nothing.

## Why CI never caught it

`tests/kb/intelligence_test.rs:320-352` seeds all three mentions like this:

```rust
        .add_mention(&EntityMention {
            id: "m1".into(),
            entity_id: alice.clone(),
            document_id: "d_graphrag".into(),
            chunk_index: Some(0),          // <-- production never writes this
            context: None,
            source: ExtractSource::Llm,    // <-- with an LLM source
        })
```

`ExtractSource::Llm` combined with `chunk_index: Some(_)` is a row shape the
real extractor cannot produce. The only end-to-end test of this SQL uses
hand-built rows that do not exist in production. Fixing the query without
fixing the test leaves the same blind spot for the next change.

## Current state (verified at 2ca7e59)

- Chunk-level mentions: pattern only — `intelligence/mod.rs:69-92`
- Document-level mentions: LLM — `intelligence/mod.rs:44-66`
- The join: `store/sqlite/intelligence.rs:507-508`
- `graph_expand_chunks` correctly filters soft-deleted docs at `:510`

## Design decision

Store LLM mentions **per chunk** rather than once per document.

The extractor already runs per chunk — `CombinedLlmExtractor::extract` loops
`for &chunk in chunks` (`intelligence/extract/llm.rs:137`) — but flattens
everything into one `Extracted` before returning, losing which chunk produced
what. Widening the extractor's return to carry the chunk index is the honest
fix: it makes the mention rows match reality and it makes GraphRAG precise
(a chunk is returned because it actually mentions the entity).

The alternative — relaxing the SQL to `(m.chunk_index IS NULL OR m.chunk_index
= c.chunk_index)` — would make every chunk of a document match every
document-level entity, so any seed match drags in the whole document. That
defeats the point of chunk-level retrieval. Do not take it.

## Scope

**In scope**: per-chunk LLM mentions, the orchestrator that builds them, and
the test rewrite.

**Out of scope**: relation wiring (plan 094), entity counts (095), graph degree
(096).

## Git workflow

```bash
git switch -c fix/graphrag-llm-entity-mentions
```

## Steps

### Step 1: Carry the chunk index out of the extractor

Widen `Extracted` (`intelligence/extract/mod.rs:9-12`) so entities carry the
index of the chunk they came from:

```rust
pub struct Extracted {
    /// `(chunk_index, name, type, confidence)`.
    pub entities: Vec<(usize, String, EntityType, f32)>,
    pub relations: Vec<(String, String, RelationType, f32)>,
}
```

Relations stay document-level: the LLM emits them per chunk but they are wired
by entity name across the whole document, and plan 094 owns that path.

Update `CombinedLlmExtractor::extract` (`extract/llm.rs:134-218`) to enumerate
its chunk loop and stamp the index.

`extract/pattern.rs` is **not** affected: `extract_pattern_entities` returns
`Vec<(String, EntityType)>` directly and never constructs an `Extracted`
(`intelligence/mod.rs:71` calls it per chunk, outside the trait). Verified —
`Extracted` appears only in `extract/mod.rs:9,16` and `extract/llm.rs`.

**Verify**: `cargo build --features kb`.

### Step 2: Emit one mention per (entity, chunk)

In `extract_document_intelligence` (`intelligence/mod.rs:44-66`), replace the
single `chunk_index: None` mention with one mention per chunk the entity
appeared in. Keep entity dedup by `canonical_key` unchanged — only the mention
rows multiply.

Keep `entity_id_by_name` keyed as it is today; plan 094 changes that key.

**Verify**: after ingest with `KB_INTELLIGENCE_ENABLED=true`,
`sqlite3 kb.db "select source, chunk_index, count(*) from entity_mention group by 1,2"`
shows no `llm | NULL` rows.

### Step 3: Rewrite the test to use the production path

Replace the hand-seeded mentions in
`intelligence_test.rs:320-352` with a call to `extract_document_intelligence`
driven by a stub `EntityRelationExtractor` (the file already defines stubs at
`:200` and `:765` — reuse that pattern). The test must exercise the same code
that ingest uses, so the row shapes are whatever production actually writes.

Keep both existing assertions (seed chunk found, neighbour-only chunk found)
and both must still pass.

**Verify**: revert Step 2 and confirm the rewritten test goes RED. A test that
passes against both the old and new mention shape is not testing this.

### Step 4: Guard the invariant directly

Add a small test asserting no LLM mention is written with a NULL chunk index
after `extract_document_intelligence` runs. That is the invariant the SQL join
depends on, stated once, where a future refactor will trip over it.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb intelligence_test
cargo test --features kb --test kb retrieve_test
```

End-to-end:

```bash
cargo build --release --features kb
export KB_DB_PATH=/tmp/kbplan93.db KB_INTELLIGENCE_ENABLED=true KB_GRAPHRAG_ENABLED=true
./target/release/rantaiclaw kb ingest ./docs/reference/kb.md
sqlite3 $KB_DB_PATH "select source, count(*) filter (where chunk_index is null) from entity_mention group by 1"
# expect: no llm rows with a null chunk_index
./target/release/rantaiclaw kb search "<an entity name from the doc>" --top 5
```

## Done criteria

- No LLM mention carries a NULL chunk index.
- The GraphRAG test drives the production orchestrator, not hand-built rows.
- Reverting the fix turns that test red.

## STOP conditions

- Widening `Extracted` cascades beyond `llm.rs`, `pattern.rs` and
  `intelligence/mod.rs` — stop and report the extra consumers.
- Mention row counts explode on a large document (one per entity per chunk):
  measure before/after on a real document; if the growth is unacceptable,
  stop and raise it rather than silently capping.
